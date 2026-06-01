use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        io::{AsRawFd, FromRawFd, RawFd},
    },
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, bail};
use crate::{
    config::Config,
    gc,
    limits::{BoundedBytes, EventRing},
    metadata::{StateMetadata, read_metadata, write_metadata},
    paths::{RuntimePaths, ensure_runtime_dirs},
    shell_cwd,
    terminal_model::TerminalModel,
};
use crate::protocol::{
    AttachId, ClientHello, ClientMsg, ErrorCode, PROTOCOL_VERSION, ServerHello, ServerMsg, ShellId,
    ShellInfo, ShellStatus,
};
use nix::{
    fcntl, libc,
    sys::signal::{Signal, kill},
    unistd::{Pid, Uid},
};
use parking_lot::Mutex;
use portable_pty::{Child, CommandBuilder, PtySize, native_pty_system};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::mpsc::{self, UnboundedSender},
    time,
};
use uuid::Uuid;

use crate::{
    framing,
    pty_master::{OwnedPtyMaster, OwnedReader, clear_cloexec},
    takeover,
};

pub async fn run(strip_prefix_env: Vec<String>) -> anyhow::Result<()> {
    raise_nofile_limit(NOFILE_TARGET);

    let paths = RuntimePaths::resolve();
    ensure_runtime_dirs(&paths)?;
    let _lock = daemon_lock(&paths)?;

    let takeover_result = takeover::read_env();
    let takeover_manifest = match takeover_result {
        Some(Ok(m)) => Some(m),
        Some(Err(err)) => {
            eprintln!("liveshd: ignoring malformed LIVESH_TAKEOVER: {err}");
            None
        }
        None => None,
    };
    takeover::clear_env();

    let config = Config::load()?;
    if takeover_manifest.is_none() && config.cleanup_lost_on_startup {
        gc::startup_gc(&paths)?;
    }

    let daemon_id = takeover_manifest
        .as_ref()
        .map(|m| m.daemon_id.clone())
        .unwrap_or_else(|| format!("daemon_{}", Uuid::new_v4().simple()));
    write_daemon_json(&paths, &daemon_id).ok();

    let (upgrade_tx, mut upgrade_rx) = mpsc::unbounded_channel::<UpgradeRequest>();
    let registry = Arc::new(ShellRegistry::new(
        paths.clone(),
        config,
        daemon_id.clone(),
        strip_prefix_env,
        upgrade_tx,
    ));
    spawn_periodic_gc(registry.clone());

    let listener = match &takeover_manifest {
        Some(manifest) => adopt_listener(manifest.listener_fd)?,
        None => {
            if paths.socket.exists() {
                let _ = fs::remove_file(&paths.socket);
            }
            let l = UnixListener::bind(&paths.socket)
                .with_context(|| format!("bind {}", paths.socket.display()))?;
            let _ = fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600));
            l
        }
    };

    if let Some(manifest) = takeover_manifest {
        registry.adopt_shells(&manifest)?;
    }

    loop {
        tokio::select! {
            biased;
            request = upgrade_rx.recv() => {
                let Some(request) = request else { return Ok(()); };
                return registry.perform_upgrade(listener, request).await;
            }
            accept = listener.accept() => {
                let (stream, _) = accept?;
                let registry = registry.clone();
                let daemon_id = daemon_id.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle_client(stream, registry, daemon_id).await {
                        eprintln!("liveshd client error: {err:#}");
                    }
                });
            }
        }
    }
}

fn adopt_listener(fd: RawFd) -> anyhow::Result<UnixListener> {
    let std_listener =
        unsafe { std::os::unix::net::UnixListener::from_raw_fd(fd) };
    std_listener
        .set_nonblocking(true)
        .context("set adopted listener non-blocking")?;
    UnixListener::from_std(std_listener).context("wrap adopted listener into tokio")
}

#[allow(deprecated)]
fn daemon_lock(paths: &RuntimePaths) -> anyhow::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&paths.lock)
        .with_context(|| format!("open {}", paths.lock.display()))?;
    fcntl::flock(file.as_raw_fd(), fcntl::FlockArg::LockExclusiveNonblock)
        .context("another liveshd is already running")?;
    Ok(file)
}

fn write_daemon_json(paths: &RuntimePaths, daemon_id: &str) -> anyhow::Result<()> {
    let bytes = serde_json::json!({
        "schema": 1,
        "daemon_id": daemon_id,
        "pid": std::process::id(),
        "started_at_ms": now_ms(),
    });
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&paths.daemon_json)?;
    file.write_all(serde_json::to_string_pretty(&bytes)?.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

const NOFILE_TARGET: libc::rlim_t = 8192;

fn raise_nofile_limit(target: libc::rlim_t) {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return;
    }
    if limit.rlim_cur >= target {
        return;
    }
    let desired = libc::rlimit {
        rlim_cur: target,
        rlim_max: limit.rlim_max.max(target),
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &desired) } == 0 {
        return;
    }
    // Setting above the hard cap (e.g. macOS kern.maxfilesperproc) failed; fall back to
    // whatever the kernel will allow up to rlim_max.
    let fallback = libc::rlimit {
        rlim_cur: limit.rlim_max,
        rlim_max: limit.rlim_max,
    };
    unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &fallback) };
}

fn spawn_periodic_gc(registry: Arc<ShellRegistry>) {
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(300));
        loop {
            interval.tick().await;
            registry.cleanup_dead_shells();
            registry.prune_storage_caps();
        }
    });
}

async fn handle_client(
    mut stream: UnixStream,
    registry: Arc<ShellRegistry>,
    daemon_id: String,
) -> anyhow::Result<()> {
    verify_peer_uid(&stream)?;

    let hello: ClientHello = framing::read_frame(&mut stream).await?;
    if hello.protocol != PROTOCOL_VERSION {
        let response = ServerHello {
            protocol: PROTOCOL_VERSION,
            daemon_id,
        };
        framing::write_frame(&mut stream, &response).await?;
        bail!("client protocol mismatch: {}", hello.protocol);
    }

    framing::write_frame(
        &mut stream,
        &ServerHello {
            protocol: PROTOCOL_VERSION,
            daemon_id,
        },
    )
    .await?;

    let (mut reader, mut writer) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMsg>();
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if framing::write_frame(&mut writer, &msg).await.is_err() {
                break;
            }
        }
    });

    let mut attached = Vec::<AttachId>::new();
    loop {
        let msg = match framing::read_frame::<ClientMsg, _>(&mut reader).await {
            Ok(msg) => msg,
            Err(_) => break,
        };

        if let Some(attach_id) = handle_msg(registry.clone(), tx.clone(), msg) {
            attached.push(attach_id);
        }
    }

    for attach_id in attached {
        registry.detach(&attach_id);
    }
    drop(tx);
    writer_task.abort();
    Ok(())
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
fn verify_peer_uid(stream: &UnixStream) -> anyhow::Result<()> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("get peer uid");
    }
    if uid != Uid::current().as_raw() {
        bail!("socket peer uid {uid} does not match current uid");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn verify_peer_uid(stream: &UnixStream) -> anyhow::Result<()> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("get peer credentials");
    }
    if cred.uid != Uid::current().as_raw() {
        bail!("socket peer uid {} does not match current uid", cred.uid);
    }
    Ok(())
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
    target_os = "linux"
)))]
fn verify_peer_uid(_stream: &UnixStream) -> anyhow::Result<()> {
    Ok(())
}

fn handle_msg(
    registry: Arc<ShellRegistry>,
    tx: UnboundedSender<ServerMsg>,
    msg: ClientMsg,
) -> Option<AttachId> {
    match msg {
        ClientMsg::Ping => {
            send(&tx, ServerMsg::Pong);
            None
        }
        ClientMsg::CreateShell {
            name,
            cwd,
            shell_path,
            env,
            cols,
            rows,
        } => {
            match registry.create_shell(name, cwd, shell_path, env, cols, rows) {
                Ok((id, name, restore_argv)) => send(
                    &tx,
                    ServerMsg::Created {
                        id,
                        name,
                        restore_argv,
                    },
                ),
                Err(err) => send_error(&tx, classify_error(&err), err.to_string()),
            }
            None
        }
        ClientMsg::OpenShell {
            id,
            cols,
            rows,
            steal,
        } => match registry.open_shell(id, cols, rows, steal, tx.clone()) {
            Ok(attach_id) => Some(attach_id),
            Err(err) => {
                send_error(&tx, classify_error(&err), err.to_string());
                None
            }
        },
        ClientMsg::Input { attach_id, bytes } => {
            if let Err(err) = registry.write_input(&attach_id, &bytes) {
                send_error(&tx, classify_error(&err), err.to_string());
            }
            None
        }
        ClientMsg::Resize {
            attach_id,
            cols,
            rows,
        } => {
            if let Err(err) = registry.resize(&attach_id, cols, rows) {
                send_error(&tx, classify_error(&err), err.to_string());
            }
            None
        }
        ClientMsg::Detach { attach_id } => {
            registry.detach(&attach_id);
            None
        }
        ClientMsg::ListShells => {
            send(
                &tx,
                ServerMsg::ShellList {
                    shells: registry.list(),
                },
            );
            None
        }
        ClientMsg::RenameShell { id, name } => {
            match registry.rename(&id, name) {
                Ok(()) => send(&tx, ServerMsg::Ok),
                Err(err) => send_error(&tx, classify_error(&err), err.to_string()),
            }
            None
        }
        ClientMsg::KillShell { id } => {
            match registry.kill_shell(&id) {
                Ok(()) => send(&tx, ServerMsg::Ok),
                Err(err) => send_error(&tx, classify_error(&err), err.to_string()),
            }
            None
        }
        ClientMsg::RunGc => {
            registry.run_gc();
            send(&tx, ServerMsg::Ok);
            None
        }
        ClientMsg::KillIdleDetached { older_than_ms } => {
            registry.kill_idle_detached(older_than_ms);
            send(&tx, ServerMsg::Ok);
            None
        }
        ClientMsg::UpgradeDaemon { binary } => {
            match registry.request_upgrade(binary) {
                Ok(()) => send(&tx, ServerMsg::Ok),
                Err(err) => send_error(&tx, ErrorCode::Internal, err.to_string()),
            }
            None
        }
    }
}

fn send(tx: &UnboundedSender<ServerMsg>, msg: ServerMsg) {
    let _ = tx.send(msg);
}

fn send_error(tx: &UnboundedSender<ServerMsg>, code: ErrorCode, message: String) {
    send(tx, ServerMsg::Error { code, message });
}

fn classify_error(err: &anyhow::Error) -> ErrorCode {
    let msg = err.to_string();
    if msg.contains("dup of fd") || msg.contains("Too many open files") || msg.contains("EMFILE") {
        ErrorCode::FdLimit
    } else if msg.contains("not found") {
        ErrorCode::NotFound
    } else if msg.contains("too many") {
        ErrorCode::TooManyShells
    } else {
        ErrorCode::Internal
    }
}

struct ShellRegistry {
    states: Mutex<HashMap<String, Arc<ShellState>>>,
    paths: RuntimePaths,
    config: Config,
    daemon_id: String,
    strip_prefix_env: Vec<String>,
    upgrade_tx: UnboundedSender<UpgradeRequest>,
}

#[derive(Debug, Clone)]
struct UpgradeRequest {
    binary: Option<PathBuf>,
}

impl ShellRegistry {
    fn new(
        paths: RuntimePaths,
        config: Config,
        daemon_id: String,
        strip_prefix_env: Vec<String>,
        upgrade_tx: UnboundedSender<UpgradeRequest>,
    ) -> Self {
        Self {
            states: Mutex::new(HashMap::new()),
            paths,
            config,
            daemon_id,
            strip_prefix_env,
            upgrade_tx,
        }
    }

    fn request_upgrade(&self, binary: Option<PathBuf>) -> anyhow::Result<()> {
        self.upgrade_tx
            .send(UpgradeRequest { binary })
            .map_err(|_| anyhow::anyhow!("upgrade channel closed"))?;
        Ok(())
    }

    fn create_shell(
        self: &Arc<Self>,
        name: Option<String>,
        cwd: PathBuf,
        shell_path: PathBuf,
        env: Vec<(String, String)>,
        cols: u16,
        rows: u16,
    ) -> anyhow::Result<(ShellId, String, Vec<String>)> {
        if self.states.lock().len() >= self.config.limits.max_shells {
            bail!("too many live shells; use liveshctl list and liveshctl kill");
        }

        let id = ShellId::new_unchecked(format!("sh_{}", Uuid::new_v4().simple()));
        let name = name.unwrap_or_else(|| self.config.default_name.clone());
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(&shell_path);
        cmd.cwd(&cwd);
        for (key, value) in env {
            if matches_strip_prefix(&key, &self.strip_prefix_env) {
                continue;
            }
            cmd.env(key, value);
        }
        if !self.strip_prefix_env.is_empty() {
            let to_remove: Vec<String> = cmd
                .iter_full_env_as_str()
                .filter_map(|(key, _)| {
                    matches_strip_prefix(key, &self.strip_prefix_env).then(|| key.to_string())
                })
                .collect();
            for key in to_remove {
                cmd.env_remove(&key);
            }
        }
        cmd.env("LIVESH_SHELL_ID", id.as_str());

        let child = pair.slave.spawn_command(cmd)?;
        let pid = child.process_id().map(|pid| pid as i32);
        drop(pair.slave);
        let master = OwnedPtyMaster::from_portable(pair.master)?;
        let reader = master.clone_reader()?;
        let state = Arc::new(ShellState::new(
            id.clone(),
            name.clone(),
            cwd,
            shell_path,
            master,
            pid,
            cols,
            rows,
            &self.config,
        ));

        self.states
            .lock()
            .insert(id.as_str().to_string(), state.clone());
        self.persist_metadata(&state)?;
        self.spawn_reader(state.clone(), reader);
        self.spawn_child_waiter(id.clone(), child);
        self.spawn_cwd_poller(state.clone());

        Ok((
            id.clone(),
            name,
            vec!["livesh".to_string(), "--open".to_string(), id.to_string()],
        ))
    }

    fn spawn_reader(self: &Arc<Self>, state: Arc<ShellState>, mut reader: OwnedReader) {
        let registry = self.clone();
        std::thread::spawn(move || {
            let mut buf = [0_u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        state.record_output(&buf[..n]);
                        registry.persist_replay_files(&state).ok();
                    }
                    Err(_) => break,
                }
            }
        });
    }

    fn spawn_child_waiter(
        self: &Arc<Self>,
        id: ShellId,
        mut child: Box<dyn Child + Send + Sync>,
    ) {
        let registry = self.clone();
        std::thread::spawn(move || {
            let exit_code = child.wait().ok().map(|status| status.exit_code() as i32);
            registry.cleanup_shell(&id, exit_code);
        });
    }

    /// Adopted-from-takeover counterpart of `spawn_child_waiter`. The new
    /// daemon inherited the child PID but not a `portable_pty::Child`,
    /// so we reap via `waitpid` directly.
    fn spawn_pid_waiter(self: &Arc<Self>, id: ShellId, pid: i32) {
        let registry = self.clone();
        std::thread::spawn(move || {
            loop {
                let mut status: libc::c_int = 0;
                let rc = unsafe { libc::waitpid(pid, &mut status, 0) };
                if rc == pid {
                    let exit_code = if libc::WIFEXITED(status) {
                        Some(libc::WEXITSTATUS(status))
                    } else if libc::WIFSIGNALED(status) {
                        Some(-libc::WTERMSIG(status))
                    } else {
                        None
                    };
                    registry.cleanup_shell(&id, exit_code);
                    return;
                }
                if rc < 0 {
                    let err = std::io::Error::last_os_error();
                    if matches!(err.raw_os_error(), Some(libc::EINTR)) {
                        continue;
                    }
                    // ECHILD / EINVAL / etc. — give up, treat as gone.
                    registry.cleanup_shell(&id, None);
                    return;
                }
            }
        });
    }

    fn spawn_cwd_poller(self: &Arc<Self>, state: Arc<ShellState>) {
        let Some(pid) = state.pid else {
            return;
        };
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(500));
                if !matches!(*state.status.lock(), ShellStatus::Running) {
                    break;
                }
                let Some(new_cwd) = shell_cwd::read_cwd(pid) else {
                    continue;
                };
                let changed = {
                    let mut cur = state.cwd.lock();
                    if *cur == new_cwd {
                        false
                    } else {
                        *cur = new_cwd.clone();
                        true
                    }
                };
                if !changed {
                    continue;
                }
                let attach = state.attach.lock();
                if let Some(attach) = attach.as_ref() {
                    let _ = attach.tx.send(ServerMsg::CwdChanged {
                        attach_id: attach.id.clone(),
                        cwd: new_cwd,
                    });
                }
            }
        });
    }

    fn open_shell(
        &self,
        id: ShellId,
        cols: u16,
        rows: u16,
        steal: bool,
        tx: UnboundedSender<ServerMsg>,
    ) -> anyhow::Result<AttachId> {
        let state = self
            .states
            .lock()
            .get(id.as_str())
            .cloned()
            .with_context(|| format!("shell state not found: {id}"))?;
        state.attach(cols, rows, steal, tx)
    }

    fn write_input(&self, attach_id: &AttachId, bytes: &[u8]) -> anyhow::Result<()> {
        let state = self
            .state_for_attach(attach_id)
            .with_context(|| format!("attach not found: {attach_id}"))?;
        state.write_input(bytes)
    }

    fn resize(&self, attach_id: &AttachId, cols: u16, rows: u16) -> anyhow::Result<()> {
        let state = self
            .state_for_attach(attach_id)
            .with_context(|| format!("attach not found: {attach_id}"))?;
        state.resize(cols, rows)
    }

    fn detach(&self, attach_id: &AttachId) {
        if let Some(state) = self.state_for_attach(attach_id) {
            state.detach(attach_id);
        }
    }

    fn rename(&self, id: &ShellId, name: String) -> anyhow::Result<()> {
        let state = self
            .states
            .lock()
            .get(id.as_str())
            .cloned()
            .with_context(|| format!("shell state not found: {id}"))?;
        *state.name.lock() = name;
        self.persist_metadata(&state)?;
        Ok(())
    }

    fn kill_shell(&self, id: &ShellId) -> anyhow::Result<()> {
        let state = self
            .states
            .lock()
            .get(id.as_str())
            .cloned()
            .with_context(|| format!("shell state not found: {id}"))?;
        if let Some(pid) = state.pid {
            let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
        }
        self.cleanup_shell(id, None);
        Ok(())
    }

    fn list(&self) -> Vec<ShellInfo> {
        let mut shells: Vec<_> = self
            .states
            .lock()
            .values()
            .map(|state| state.info())
            .collect();
        shells.sort_by_key(|info| info.created_at_ms);
        shells
    }

    fn run_gc(&self) {
        self.cleanup_dead_shells();
        self.prune_storage_caps();
        let live: Vec<String> = self.states.lock().keys().cloned().collect();
        remove_orphans(&self.paths.states, &live, "json");
        remove_orphans(&self.paths.snapshots, &live, "screen");
        remove_orphans(&self.paths.scrollback, &live, "ring");
        remove_orphans(&self.paths.events, &live, "ring");
    }

    /// Kill every detached shell whose last activity is at least
    /// `older_than_ms` milliseconds in the past, then GC their files.
    fn kill_idle_detached(&self, older_than_ms: u64) -> usize {
        let cutoff = now_ms().saturating_sub(older_than_ms);
        let victims: Vec<(ShellId, Option<i32>)> = {
            let states = self.states.lock();
            states
                .iter()
                .filter_map(|(id, state)| {
                    if state.is_attached() {
                        return None;
                    }
                    if state.last_active_at_ms.load(Ordering::Relaxed) > cutoff {
                        return None;
                    }
                    Some((ShellId::new_unchecked(id.clone()), state.pid))
                })
                .collect()
        };
        for (id, pid) in &victims {
            if let Some(pid) = pid {
                let _ = kill(Pid::from_raw(*pid), Signal::SIGTERM);
            }
            self.cleanup_shell(id, None);
        }
        victims.len()
    }

    /// Sweep the registry for shells whose child process has gone away
    /// without the waiter noticing (e.g. SIGKILL, double-fork, daemon races)
    /// and free their handles so fds get released.
    fn cleanup_dead_shells(&self) -> usize {
        let dead: Vec<ShellId> = {
            let states = self.states.lock();
            states
                .iter()
                .filter_map(|(id, state)| {
                    let pid = state.pid?;
                    if kill(Pid::from_raw(pid), None).is_ok() {
                        return None;
                    }
                    Some(ShellId::new_unchecked(id.clone()))
                })
                .collect()
        };
        for id in &dead {
            self.cleanup_shell(id, None);
        }
        dead.len()
    }

    fn prune_storage_caps(&self) {
        let size = gc::runtime_size(&self.paths);
        if size <= self.config.limits.global_runtime_bytes {
            return;
        }

        for state in self.states.lock().values() {
            if !state.is_attached() {
                state.clear_replay_buffers();
                self.persist_replay_files(state).ok();
            }
        }
    }

    fn cleanup_shell(&self, id: &ShellId, exit_code: Option<i32>) {
        let state = self.states.lock().remove(id.as_str());
        if let Some(state) = state {
            state.mark_exited(exit_code);
            state.close_handles();
            gc::remove_shell_files(&self.paths, id);
            let _ = fs::remove_file(pid_hint_path(&self.paths, id));
        }
    }

    fn state_for_attach(&self, attach_id: &AttachId) -> Option<Arc<ShellState>> {
        self.states
            .lock()
            .values()
            .find(|state| state.has_attach(attach_id))
            .cloned()
    }

    fn persist_metadata(&self, state: &ShellState) -> anyhow::Result<()> {
        let (rows, cols) = *state.size.lock();
        let metadata = StateMetadata {
            schema: 1,
            id: state.id.clone(),
            name: state.name.lock().clone(),
            status: *state.status.lock(),
            cwd: state.cwd.lock().display().to_string(),
            shell_path: state.shell_path.display().to_string(),
            daemon_id: self.daemon_id.clone(),
            created_at_ms: state.created_at_ms as u128,
            last_active_at_ms: state.last_active_at_ms.load(Ordering::Relaxed) as u128,
            rows,
            cols,
        };
        write_metadata(&self.paths.metadata(&state.id), &metadata)
    }

    fn persist_replay_files(&self, state: &ShellState) -> anyhow::Result<()> {
        write_private_file(&self.paths.snapshot(&state.id), &state.raw_snapshot())?;
        write_private_file(&self.paths.scrollback(&state.id), &state.scrollback_bytes())?;
        write_private_file(&self.paths.events(&state.id), &state.event_bytes())?;
        Ok(())
    }

    /// Rebuild the registry after the previous daemon execv'd into us.
    /// Master fds were left open with CLOEXEC cleared and are listed in
    /// the takeover manifest; per-shell replay state lives on disk.
    fn adopt_shells(self: &Arc<Self>, manifest: &takeover::Manifest) -> anyhow::Result<()> {
        for handoff in &manifest.shells {
            if let Err(err) = self.adopt_one(handoff) {
                eprintln!(
                    "liveshd: dropping shell {} during takeover: {err:#}",
                    handoff.id
                );
                // Best-effort: close the orphan fd so we don't leak.
                unsafe { libc::close(handoff.master_fd) };
                gc::remove_shell_files(&self.paths, &ShellId::new_unchecked(handoff.id.clone()));
            }
        }
        Ok(())
    }

    fn adopt_one(self: &Arc<Self>, handoff: &takeover::ShellHandoff) -> anyhow::Result<()> {
        let id = ShellId::new_unchecked(handoff.id.clone());
        let metadata_path = self.paths.metadata(&id);
        let metadata: StateMetadata = read_metadata(&metadata_path)
            .with_context(|| format!("read metadata for {id}"))?;
        let master = unsafe { OwnedPtyMaster::from_raw_fd(handoff.master_fd) };
        let pid = (metadata.created_at_ms != 0)
            .then_some(read_pid_hint(&self.paths, &id))
            .flatten();
        let reader = master.clone_reader().context("dup adopted master")?;

        let state = Arc::new(ShellState::adopt(
            id.clone(),
            metadata,
            master,
            pid,
            &self.paths,
            &self.config,
        )?);
        self.states.lock().insert(id.as_str().to_string(), state.clone());
        // Refresh metadata with the new daemon id so peers can tell who owns the shell now.
        self.persist_metadata(&state).ok();
        self.spawn_reader(state.clone(), reader);
        if let Some(pid) = pid {
            self.spawn_pid_waiter(id.clone(), pid);
        }
        self.spawn_cwd_poller(state);
        Ok(())
    }

    /// Persist final state and `execv` the new binary in place. Listener
    /// fd and master fds get FD_CLOEXEC cleared so they survive across
    /// the exec; everything else dies and the new daemon reconstructs.
    async fn perform_upgrade(
        self: &Arc<Self>,
        listener: UnixListener,
        request: UpgradeRequest,
    ) -> anyhow::Result<()> {
        // 1. Flush every shell's replay state to disk under the locks. The
        //    reader threads might still write a bit more after this, which
        //    we accept as a small data-loss window across the execv.
        let states: Vec<Arc<ShellState>> = self.states.lock().values().cloned().collect();
        for state in &states {
            self.persist_metadata(state).ok();
            self.persist_replay_files(state).ok();
        }

        // 2. Build the takeover manifest. Master fds first get CLOEXEC
        //    cleared; otherwise execv would close them.
        let mut shells = Vec::with_capacity(states.len());
        for state in &states {
            let guard = state.master.lock();
            let Some(master) = guard.as_ref() else { continue; };
            if let Err(err) = master.clear_cloexec() {
                eprintln!(
                    "liveshd: skipping shell {} during upgrade (clear_cloexec: {err})",
                    state.id
                );
                continue;
            }
            if let Some(pid) = state.pid {
                write_pid_hint(&self.paths, &state.id, pid).ok();
            }
            shells.push(takeover::ShellHandoff {
                id: state.id.as_str().to_string(),
                master_fd: master.as_raw_fd(),
            });
        }

        // 3. Listener fd: convert tokio listener to std + clear CLOEXEC so
        //    the new daemon can adopt the same bound socket.
        let std_listener = listener.into_std().context("tokio listener into std")?;
        let listener_fd: RawFd = std_listener.as_raw_fd();
        clear_cloexec(listener_fd).context("clear cloexec on listener")?;
        // Forget the std listener so its Drop doesn't close the fd.
        std::mem::forget(std_listener);

        let manifest = takeover::Manifest {
            schema: 1,
            listener_fd,
            runtime_dir: self.paths.base.clone(),
            daemon_id: self.daemon_id.clone(),
            shells,
        };

        // 4. Resolve the new binary path. Default to current_exe() so a
        //    rebuild of the same file works out of the box.
        let binary = match request.binary {
            Some(path) => path,
            None => std::env::current_exe().context("resolve current liveshd binary")?,
        };
        if !binary.exists() {
            anyhow::bail!("upgrade target {} does not exist", binary.display());
        }

        // 5. execv. argv[0] = original program name so /proc-style tools
        //    still see "liveshd".
        let original_argv: Vec<std::ffi::CString> = std::env::args_os()
            .map(|a| std::ffi::CString::new(a.into_encoded_bytes()).unwrap_or_default())
            .collect();
        let argv_cstr = if original_argv.is_empty() {
            vec![std::ffi::CString::new("liveshd").unwrap()]
        } else {
            original_argv
        };
        let argv: Vec<&std::ffi::CStr> = argv_cstr.iter().map(|c| c.as_c_str()).collect();
        let binary_cstr = std::ffi::CString::new(binary.as_os_str().as_encoded_bytes())
            .context("binary path contains NUL")?;

        // 6. Encode manifest into env. Use unsafe set_var because execvp
        //    inherits the environment from the parent process.
        let payload = takeover::encode(&manifest);
        unsafe { std::env::set_var(takeover::TAKEOVER_ENV, &payload) };

        let err = nix::unistd::execv(&binary_cstr, &argv);
        // execv only returns on failure; if we get here the new binary is
        // broken and we still hold all the fds.
        unsafe { std::env::remove_var(takeover::TAKEOVER_ENV) };
        Err(anyhow::anyhow!(
            "execv {} failed: {:?}",
            binary.display(),
            err
        ))
    }
}

fn pid_hint_path(paths: &RuntimePaths, id: &ShellId) -> PathBuf {
    paths.states.join(format!("{}.pid", id.as_str()))
}

fn write_pid_hint(paths: &RuntimePaths, id: &ShellId, pid: i32) -> std::io::Result<()> {
    let path = pid_hint_path(paths, id);
    fs::write(path, pid.to_string())
}

fn read_pid_hint(paths: &RuntimePaths, id: &ShellId) -> Option<i32> {
    let path = pid_hint_path(paths, id);
    let raw = fs::read_to_string(path).ok()?;
    raw.trim().parse().ok()
}

fn write_private_file(path: &PathBuf, bytes: &[u8]) -> anyhow::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

fn remove_orphans(dir: &PathBuf, live: &[String], extension: &str) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if path.extension().and_then(|ext| ext.to_str()) != Some(extension) {
            continue;
        }
        if !live.iter().any(|id| id == stem) {
            let _ = fs::remove_file(path);
        }
    }
}

struct Attach {
    id: AttachId,
    tx: UnboundedSender<ServerMsg>,
}

struct ShellState {
    id: ShellId,
    name: Mutex<String>,
    status: Mutex<ShellStatus>,
    shell_path: PathBuf,
    cwd: Mutex<PathBuf>,
    created_at_ms: u64,
    last_active_at_ms: AtomicU64,
    master: Mutex<Option<OwnedPtyMaster>>,
    pid: Option<i32>,
    terminal: Mutex<TerminalModel>,
    /// Current PTY dimensions as (rows, cols). Kept in sync with the grid and
    /// persisted to metadata so a hot-upgrade can rebuild the grid at the
    /// matching size.
    size: Mutex<(u16, u16)>,
    scrollback: Mutex<BoundedBytes>,
    events: Mutex<EventRing>,
    attach: Mutex<Option<Attach>>,
    seq: AtomicU64,
}

impl ShellState {
    fn new(
        id: ShellId,
        name: String,
        cwd: PathBuf,
        shell_path: PathBuf,
        master: OwnedPtyMaster,
        pid: Option<i32>,
        cols: u16,
        rows: u16,
        config: &Config,
    ) -> Self {
        let now = now_ms();
        Self {
            id,
            name: Mutex::new(name),
            status: Mutex::new(ShellStatus::Running),
            shell_path,
            cwd: Mutex::new(cwd),
            created_at_ms: now,
            last_active_at_ms: AtomicU64::new(now),
            master: Mutex::new(Some(master)),
            pid,
            terminal: Mutex::new(TerminalModel::new(rows, cols)),
            size: Mutex::new((rows, cols)),
            scrollback: Mutex::new(BoundedBytes::new(config.limits.scrollback_bytes_per_shell)),
            events: Mutex::new(EventRing::new(config.limits.event_ring_bytes_per_shell)),
            attach: Mutex::new(None),
            seq: AtomicU64::new(0),
        }
    }

    /// Re-create a ShellState from on-disk metadata + replay files after a
    /// hot-upgrade adopted the PTY master fd. The reader thread will
    /// resume scribbling into the same TerminalModel / scrollback / event
    /// ring as soon as we hand the state back to the registry.
    fn adopt(
        id: ShellId,
        metadata: StateMetadata,
        master: OwnedPtyMaster,
        pid: Option<i32>,
        paths: &RuntimePaths,
        config: &Config,
    ) -> anyhow::Result<Self> {
        // Older metadata predates persisted dimensions (deserializes to 0);
        // fall back to a conventional 80x24 until the next attach resizes us.
        let rows = if metadata.rows == 0 { 24 } else { metadata.rows };
        let cols = if metadata.cols == 0 { 80 } else { metadata.cols };
        let mut terminal = TerminalModel::new(rows, cols);
        if let Ok(snapshot) = fs::read(paths.snapshot(&id)) {
            terminal.process(&snapshot);
        }
        let mut scrollback = BoundedBytes::new(config.limits.scrollback_bytes_per_shell);
        if let Ok(bytes) = fs::read(paths.scrollback(&id)) {
            scrollback.append(&bytes);
        }
        let mut events = EventRing::new(config.limits.event_ring_bytes_per_shell);
        if let Ok(bytes) = fs::read(paths.events(&id)) {
            events.append(0, &bytes);
        }
        let created_at_ms = metadata.created_at_ms.min(u128::from(u64::MAX)) as u64;
        let last_active_ms = metadata.last_active_at_ms.min(u128::from(u64::MAX)) as u64;
        Ok(Self {
            id,
            name: Mutex::new(metadata.name),
            status: Mutex::new(metadata.status),
            shell_path: PathBuf::from(metadata.shell_path),
            cwd: Mutex::new(PathBuf::from(metadata.cwd)),
            created_at_ms,
            last_active_at_ms: AtomicU64::new(last_active_ms),
            master: Mutex::new(Some(master)),
            pid,
            terminal: Mutex::new(terminal),
            size: Mutex::new((rows, cols)),
            scrollback: Mutex::new(scrollback),
            events: Mutex::new(events),
            attach: Mutex::new(None),
            seq: AtomicU64::new(0),
        })
    }

    fn attach(
        &self,
        cols: u16,
        rows: u16,
        steal: bool,
        tx: UnboundedSender<ServerMsg>,
    ) -> anyhow::Result<AttachId> {
        let mut attach = self.attach.lock();
        if let Some(existing) = attach.as_ref() {
            if !steal {
                bail!("shell already has an attached client");
            }
            let _ = existing.tx.send(ServerMsg::DetachedByAnotherClient {
                attach_id: existing.id.clone(),
            });
        }

        self.resize(cols, rows).ok();
        let attach_id = AttachId::new(format!("att_{}", Uuid::new_v4().simple()));
        let seq = self.seq.load(Ordering::SeqCst);
        let screen_bytes = self.terminal.lock().snapshot_bytes();
        let name = self.name.lock().clone();
        let status = *self.status.lock();
        let _ = tx.send(ServerMsg::Snapshot {
            id: self.id.clone(),
            attach_id: attach_id.clone(),
            seq,
            name,
            status,
            screen_bytes,
        });
        let _ = tx.send(ServerMsg::CwdChanged {
            attach_id: attach_id.clone(),
            cwd: self.cwd.lock().clone(),
        });
        *attach = Some(Attach {
            id: attach_id.clone(),
            tx: tx.clone(),
        });

        for (seq, bytes) in self.events.lock().after(seq) {
            let _ = tx.send(ServerMsg::Output {
                attach_id: attach_id.clone(),
                seq,
                bytes,
            });
        }

        Ok(attach_id)
    }

    fn detach(&self, attach_id: &AttachId) {
        let mut attach = self.attach.lock();
        if attach
            .as_ref()
            .is_some_and(|attach| attach.id.as_str() == attach_id.as_str())
        {
            *attach = None;
        }
    }

    fn has_attach(&self, attach_id: &AttachId) -> bool {
        self.attach
            .lock()
            .as_ref()
            .is_some_and(|attach| attach.id.as_str() == attach_id.as_str())
    }

    fn is_attached(&self) -> bool {
        self.attach.lock().is_some()
    }

    fn write_input(&self, bytes: &[u8]) -> anyhow::Result<()> {
        let master = self.master.lock();
        let Some(master) = master.as_ref() else {
            bail!("shell pty is closed");
        };
        master.write_all(bytes)?;
        Ok(())
    }

    fn resize(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        {
            let master = self.master.lock();
            let Some(master) = master.as_ref() else {
                bail!("shell pty is closed");
            };
            master.resize(cols, rows)?;
        }
        self.terminal.lock().set_size(rows, cols);
        *self.size.lock() = (rows, cols);
        Ok(())
    }

    fn record_output(&self, bytes: &[u8]) {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.last_active_at_ms.store(now_ms(), Ordering::Relaxed);
        self.terminal.lock().process(bytes);
        self.scrollback.lock().append(bytes);
        self.events.lock().append(seq, bytes);

        let mut attach = self.attach.lock();
        let should_detach = attach.as_ref().is_some_and(|current| {
            current
                .tx
                .send(ServerMsg::Output {
                    attach_id: current.id.clone(),
                    seq,
                    bytes: bytes.to_vec(),
                })
                .is_err()
        });
        if should_detach {
            *attach = None;
        }
    }

    fn mark_exited(&self, exit_code: Option<i32>) {
        *self.status.lock() = ShellStatus::Exited;
        let attach = self.attach.lock().take();
        if let Some(attach) = attach {
            let _ = attach.tx.send(ServerMsg::Exited {
                id: self.id.clone(),
                attach_id: Some(attach.id),
                exit_code,
            });
        }
    }

    fn close_handles(&self) {
        *self.master.lock() = None;
    }

    fn clear_replay_buffers(&self) {
        self.terminal.lock().clear();
        self.scrollback.lock().clear();
        self.events.lock().clear();
    }

    fn raw_snapshot(&self) -> Vec<u8> {
        self.terminal.lock().raw_snapshot()
    }

    fn scrollback_bytes(&self) -> Vec<u8> {
        self.scrollback.lock().bytes()
    }

    fn event_bytes(&self) -> Vec<u8> {
        self.events.lock().bytes()
    }

    fn info(&self) -> ShellInfo {
        ShellInfo {
            id: self.id.clone(),
            name: self.name.lock().clone(),
            status: *self.status.lock(),
            cwd: self.cwd.lock().clone(),
            shell_path: self.shell_path.clone(),
            created_at_ms: self.created_at_ms as u128,
            last_active_at_ms: self.last_active_at_ms.load(Ordering::Relaxed) as u128,
            attached: self.attach.lock().is_some(),
        }
    }
}

fn matches_strip_prefix(key: &str, prefixes: &[String]) -> bool {
    prefixes
        .iter()
        .any(|prefix| !prefix.is_empty() && key.starts_with(prefix.as_str()))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_only_real_prefix() {
        let prefixes = vec!["CMUX_".to_string(), "FOO_".to_string()];
        assert!(matches_strip_prefix("CMUX_SESSION", &prefixes));
        assert!(matches_strip_prefix("FOO_BAR", &prefixes));
        assert!(!matches_strip_prefix("CMUX", &prefixes));
        assert!(!matches_strip_prefix("MY_CMUX_", &prefixes));
        assert!(!matches_strip_prefix("PATH", &prefixes));
    }

    #[test]
    fn empty_prefix_list_strips_nothing() {
        assert!(!matches_strip_prefix("CMUX_X", &[]));
    }

    #[test]
    fn empty_prefix_string_is_ignored() {
        let prefixes = vec![String::new()];
        assert!(!matches_strip_prefix("CMUX_X", &prefixes));
        assert!(!matches_strip_prefix("", &prefixes));
    }

    #[test]
    fn command_builder_strips_inherited_and_explicit_env() {
        unsafe {
            std::env::set_var("CMUX_FROM_DAEMON_ENV", "leaked");
        }

        let mut cmd = CommandBuilder::new("/bin/sh");
        let prefixes = vec!["CMUX_".to_string()];

        let client_env = vec![
            ("CMUX_FROM_CLIENT".to_string(), "leaked".to_string()),
            ("PATH".to_string(), "/usr/bin".to_string()),
        ];
        for (key, value) in client_env {
            if matches_strip_prefix(&key, &prefixes) {
                continue;
            }
            cmd.env(key, value);
        }
        let to_remove: Vec<String> = cmd
            .iter_full_env_as_str()
            .filter_map(|(key, _)| {
                matches_strip_prefix(key, &prefixes).then(|| key.to_string())
            })
            .collect();
        for key in to_remove {
            cmd.env_remove(&key);
        }

        let env: Vec<(String, String)> = cmd
            .iter_full_env_as_str()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert!(
            !env.iter().any(|(k, _)| k.starts_with("CMUX_")),
            "expected no CMUX_* in env, got: {:?}",
            env.iter().filter(|(k, _)| k.starts_with("CMUX_")).collect::<Vec<_>>()
        );
        assert!(env.iter().any(|(k, v)| k == "PATH" && v == "/usr/bin"));

        unsafe {
            std::env::remove_var("CMUX_FROM_DAEMON_ENV");
        }
    }
}
