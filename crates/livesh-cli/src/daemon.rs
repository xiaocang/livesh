use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
    },
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, bail};
use livesh_core::{
    config::Config,
    gc,
    limits::{BoundedBytes, EventRing},
    metadata::{StateMetadata, write_metadata},
    paths::{RuntimePaths, ensure_runtime_dirs},
    terminal_model::TerminalModel,
};
use livesh_protocol::{
    AttachId, ClientHello, ClientMsg, ErrorCode, PROTOCOL_VERSION, ServerHello, ServerMsg, ShellId,
    ShellInfo, ShellStatus,
};
use nix::{
    fcntl, libc,
    sys::signal::{Signal, kill},
    unistd::{Pid, Uid},
};
use parking_lot::Mutex;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::mpsc::{self, UnboundedSender},
    time,
};
use uuid::Uuid;

use crate::framing;

pub async fn run() -> anyhow::Result<()> {
    let paths = RuntimePaths::resolve();
    ensure_runtime_dirs(&paths)?;
    let lock = daemon_lock(&paths)?;
    let _lock = lock;

    if paths.socket.exists() {
        let _ = fs::remove_file(&paths.socket);
    }

    let config = Config::load()?;
    if config.cleanup_lost_on_startup {
        gc::startup_gc(&paths)?;
    }

    let daemon_id = format!("daemon_{}", Uuid::new_v4().simple());
    write_daemon_json(&paths, &daemon_id).ok();
    let registry = Arc::new(ShellRegistry::new(paths.clone(), config, daemon_id.clone()));
    spawn_periodic_gc(registry.clone());

    let listener = UnixListener::bind(&paths.socket)
        .with_context(|| format!("bind {}", paths.socket.display()))?;
    let _ = fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600));

    loop {
        let (stream, _) = listener.accept().await?;
        let registry = registry.clone();
        let daemon_id = daemon_id.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_client(stream, registry, daemon_id).await {
                eprintln!("liveshd client error: {err:#}");
            }
        });
    }
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

fn spawn_periodic_gc(registry: Arc<ShellRegistry>) {
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(300));
        loop {
            interval.tick().await;
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
    if msg.contains("not found") {
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
}

impl ShellRegistry {
    fn new(paths: RuntimePaths, config: Config, daemon_id: String) -> Self {
        Self {
            states: Mutex::new(HashMap::new()),
            paths,
            config,
            daemon_id,
        }
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
            cmd.env(key, value);
        }

        let child = pair.slave.spawn_command(cmd)?;
        let pid = child.process_id().map(|pid| pid as i32);
        drop(pair.slave);
        let master = pair.master;
        let reader = master.try_clone_reader()?;
        let writer = master.take_writer()?;
        let state = Arc::new(ShellState::new(
            id.clone(),
            name.clone(),
            cwd,
            shell_path,
            master,
            writer,
            pid,
            &self.config,
        ));

        self.states
            .lock()
            .insert(id.as_str().to_string(), state.clone());
        self.persist_metadata(&state)?;
        self.spawn_reader(state.clone(), reader);
        self.spawn_waiter(id.clone(), state.clone(), child);

        Ok((
            id.clone(),
            name,
            vec!["livesh".to_string(), "--open".to_string(), id.to_string()],
        ))
    }

    fn spawn_reader(self: &Arc<Self>, state: Arc<ShellState>, mut reader: Box<dyn Read + Send>) {
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

    fn spawn_waiter(
        self: &Arc<Self>,
        id: ShellId,
        _state: Arc<ShellState>,
        mut child: Box<dyn Child + Send + Sync>,
    ) {
        let registry = self.clone();
        std::thread::spawn(move || {
            let exit_code = child.wait().ok().map(|status| status.exit_code() as i32);
            registry.cleanup_shell(&id, exit_code);
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
        self.prune_storage_caps();
        let live: Vec<String> = self.states.lock().keys().cloned().collect();
        remove_orphans(&self.paths.states, &live, "json");
        remove_orphans(&self.paths.snapshots, &live, "screen");
        remove_orphans(&self.paths.scrollback, &live, "ring");
        remove_orphans(&self.paths.events, &live, "ring");
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
        let metadata = StateMetadata {
            schema: 1,
            id: state.id.clone(),
            name: state.name.lock().clone(),
            status: *state.status.lock(),
            cwd: state.cwd.display().to_string(),
            shell_path: state.shell_path.display().to_string(),
            daemon_id: self.daemon_id.clone(),
            created_at_ms: state.created_at_ms as u128,
            last_active_at_ms: state.last_active_at_ms.load(Ordering::Relaxed) as u128,
        };
        write_metadata(&self.paths.metadata(&state.id), &metadata)
    }

    fn persist_replay_files(&self, state: &ShellState) -> anyhow::Result<()> {
        write_private_file(&self.paths.snapshot(&state.id), &state.raw_snapshot())?;
        write_private_file(&self.paths.scrollback(&state.id), &state.scrollback_bytes())?;
        write_private_file(&self.paths.events(&state.id), &state.event_bytes())?;
        Ok(())
    }
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
    cwd: PathBuf,
    created_at_ms: u64,
    last_active_at_ms: AtomicU64,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    pid: Option<i32>,
    terminal: Mutex<TerminalModel>,
    scrollback: Mutex<BoundedBytes>,
    events: Mutex<EventRing>,
    attach: Mutex<Option<Attach>>,
    seq: AtomicU64,
}

impl ShellState {
    #[allow(clippy::too_many_arguments)]
    fn new(
        id: ShellId,
        name: String,
        cwd: PathBuf,
        shell_path: PathBuf,
        master: Box<dyn MasterPty + Send>,
        writer: Box<dyn Write + Send>,
        pid: Option<i32>,
        config: &Config,
    ) -> Self {
        let now = now_ms();
        Self {
            id,
            name: Mutex::new(name),
            status: Mutex::new(ShellStatus::Running),
            shell_path,
            cwd,
            created_at_ms: now,
            last_active_at_ms: AtomicU64::new(now),
            master: Mutex::new(Some(master)),
            writer: Mutex::new(Some(writer)),
            pid,
            terminal: Mutex::new(TerminalModel::new(config.limits.snapshot_bytes_per_shell)),
            scrollback: Mutex::new(BoundedBytes::new(config.limits.scrollback_bytes_per_shell)),
            events: Mutex::new(EventRing::new(config.limits.event_ring_bytes_per_shell)),
            attach: Mutex::new(None),
            seq: AtomicU64::new(0),
        }
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
        let mut writer = self.writer.lock();
        let Some(writer) = writer.as_mut() else {
            bail!("shell writer is closed");
        };
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    fn resize(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        let master = self.master.lock();
        let Some(master) = master.as_ref() else {
            bail!("shell pty is closed");
        };
        master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
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
        *self.writer.lock() = None;
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
            cwd: self.cwd.clone(),
            shell_path: self.shell_path.clone(),
            created_at_ms: self.created_at_ms as u128,
            last_active_at_ms: self.last_active_at_ms.load(Ordering::Relaxed) as u128,
            attached: self.attach.lock().is_some(),
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
