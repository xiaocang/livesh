use std::{
    io::Write,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::protocol::{AttachId, ClientKind, ClientMsg, ErrorCode, ServerMsg, ShellId};
use crate::shell_resolve;
use parking_lot::Mutex;
use tokio::{
    io::{AsyncReadExt, stdin},
    signal::unix::{SignalKind, signal},
    sync::watch,
    task::JoinHandle,
    time::{sleep, timeout},
};

use crate::{
    client::{Client, ServerError},
    raw_mode::RawModeGuard,
    tty,
};

// After the daemon connection drops — almost always a `liveshctl
// upgrade-daemon` hot-upgrade — keep retrying the re-attach for a few seconds.
// The new daemon adopts our shell under the same id, so re-opening it resumes
// the session in place instead of killing the client.
const RECONNECT_ATTEMPTS: u32 = 50;
const RECONNECT_DELAY: Duration = Duration::from_millis(100);
const RECONNECT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const STALE_KEY_RELEASE_SUPPRESSION: Duration = Duration::from_millis(500);
const PENDING_CSI_TIMEOUT: Duration = Duration::from_millis(20);
const MAX_OUTPUT_RESET_PREFIX: usize = 64;

/// The connection the input/resize tasks currently forward to. Swapped out on
/// reconnect so those long-lived tasks follow the session to the new daemon
/// without being torn down (which would drop buffered keystrokes).
#[derive(Clone)]
struct Target {
    client: Client,
    attach_id: AttachId,
}

/// Why the output loop returned. Only `Disconnected` is recoverable.
enum Outcome {
    Exited(i32),
    Disconnected,
    Failed(anyhow::Error),
}

/// How the bridge finished, decided after the reconnect loop gives up.
enum BridgeEnd {
    Exit(i32),
    Error(anyhow::Error),
    /// The daemon connection dropped and never came back (no liveshd to
    /// re-attach to). Fall back to a real shell so the user keeps a working
    /// terminal instead of being dropped with an error.
    DaemonGone(anyhow::Error),
}

pub async fn open_and_bridge(client: Client, id: ShellId) -> anyhow::Result<i32> {
    let size = tty::current_size();
    let snapshot = client
        .open_shell(id.clone(), size.cols, size.rows, true)
        .await?;
    bridge_snapshot(
        client,
        id,
        snapshot.name,
        snapshot.attach_id,
        snapshot.screen_bytes,
    )
    .await
}

pub async fn bridge_snapshot(
    client: Client,
    id: ShellId,
    name: String,
    attach_id: AttachId,
    screen_bytes: Vec<u8>,
) -> anyhow::Result<i32> {
    // Title the process for `ps`/`w`. The head (name + short id) is fixed; the
    // cwd is filled in by `output_loop` as `CwdChanged` arrives, so the title
    // tracks where the shell actually is.
    let default_name = crate::config::Config::load()
        .map(|c| c.default_name)
        .unwrap_or_else(|_| "shell".to_string());
    let title_head = title_head(&name, &id, &default_name);
    crate::proctitle::set_title(&title_head);
    let raw_guard = if tty::stdin_stdout_are_tty() {
        Some(RawModeGuard::enter()?)
    } else {
        None
    };

    paint(&screen_bytes)?;

    let mut current = Target { client, attach_id };
    let (target_tx, target_rx) = watch::channel(current.clone());
    let input_filter = Arc::new(Mutex::new(BridgeInputFilter::new()));

    let input_task = spawn_input_task(target_rx.clone(), input_filter.clone());
    let resize_task = match spawn_resize_task(target_rx) {
        Ok(task) => task,
        Err(err) => {
            input_task.abort();
            drop(raw_guard);
            return Err(err);
        }
    };

    let end = loop {
        match output_loop(
            current.client.clone(),
            current.attach_id.clone(),
            &title_head,
            input_filter.clone(),
        )
        .await
        {
            Outcome::Exited(code) => break BridgeEnd::Exit(code),
            Outcome::Failed(err) => break BridgeEnd::Error(err),
            Outcome::Disconnected => match reconnect(&id).await {
                Ok((client, attach_id, screen_bytes)) => {
                    current = Target { client, attach_id };
                    // Point the long-lived input/resize tasks at the new
                    // daemon before repainting the adopted screen.
                    let _ = target_tx.send(current.clone());
                    if let Err(err) = paint(&screen_bytes) {
                        break BridgeEnd::Error(err);
                    }
                    continue;
                }
                Err(err) => break BridgeEnd::DaemonGone(err),
            },
        }
    };

    // Best-effort detach on whatever connection we last held; a no-op if it is
    // already dead.
    let _ = current
        .client
        .send(&ClientMsg::Detach {
            attach_id: current.attach_id.clone(),
        })
        .await;
    input_task.abort();
    resize_task.abort();
    // Restore the terminal before returning or replacing the process so the
    // real shell (or the caller) sees a sane tty.
    drop(raw_guard);

    match end {
        BridgeEnd::Exit(code) => Ok(code),
        BridgeEnd::Error(err) => Err(err),
        BridgeEnd::DaemonGone(err) => {
            eprintln!(
                "livesh: lost liveshd and could not reconnect ({err:#}); \
                 dropping to a real shell"
            );
            shell_resolve::exec_real_shell().map(|()| 0)
        }
    }
}

/// Longest shell name shown in the title; longer names are truncated with `...`.
const MAX_TITLE_NAME: usize = 20;

/// Fixed part of the `ps`/`w` process title, e.g. `livesh [api] (sh_4cceeab1)`.
/// The id is shortened to `sh_` plus the first 8 hex chars — enough to match
/// `liveshctl ls` without bloating the line. The `[name]` segment is dropped
/// when the shell still carries the default name (it adds no information), and
/// long names are truncated. `output_loop` appends the live cwd.
fn title_head(name: &str, id: &ShellId, default_name: &str) -> String {
    let short_id: String = id.as_str().chars().take("sh_".len() + 8).collect();
    if name.is_empty() || name == default_name {
        format!("livesh ({short_id})")
    } else {
        format!("livesh [{}] ({short_id})", truncate(name, MAX_TITLE_NAME))
    }
}

/// Truncate to at most `max` chars, marking elision with a trailing `...`.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(3)).collect();
    format!("{head}...")
}

/// Render a cwd for the process title, collapsing `$HOME` to `~`.
fn display_cwd(cwd: &std::path::Path) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home = std::path::Path::new(&home);
        if let Ok(rest) = cwd.strip_prefix(home) {
            if rest.as_os_str().is_empty() {
                return "~".to_string();
            }
            return format!("~/{}", rest.display());
        }
    }
    cwd.display().to_string()
}

fn paint(bytes: &[u8]) -> anyhow::Result<()> {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(bytes)?;
    stdout.flush()?;
    Ok(())
}

fn spawn_input_task(
    target_rx: watch::Receiver<Target>,
    input_filter: Arc<Mutex<BridgeInputFilter>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut input = stdin();
        let mut buf = [0_u8; 8192];
        loop {
            let read_result = {
                let wait = input_filter.lock().pending_timeout(Instant::now());
                match wait {
                    Some(wait) if wait.is_zero() => {
                        let bytes = input_filter.lock().flush_pending();
                        if !bytes.is_empty() {
                            send_input(&target_rx, bytes).await;
                        }
                        continue;
                    }
                    Some(wait) => match timeout(wait, input.read(&mut buf)).await {
                        Ok(result) => result,
                        Err(_) => {
                            let bytes = input_filter.lock().flush_pending();
                            if !bytes.is_empty() {
                                send_input(&target_rx, bytes).await;
                            }
                            continue;
                        }
                    },
                    None => input.read(&mut buf).await,
                }
            };
            let n = match read_result {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let bytes = {
                let mut input_filter = input_filter.lock();
                input_filter.filter_input(&buf[..n], Instant::now())
            };
            if bytes.is_empty() {
                continue;
            }
            send_input(&target_rx, bytes).await;
        }
    })
}

async fn send_input(target_rx: &watch::Receiver<Target>, bytes: Vec<u8>) {
    let target = target_rx.borrow().clone();
    // A send error means the daemon is mid-upgrade; the output loop will
    // reconnect and repoint us. Drop these bytes rather than tear down the
    // session, then keep reading stdin.
    let _ = target
        .client
        .send(&ClientMsg::Input {
            attach_id: target.attach_id.clone(),
            bytes,
        })
        .await;
}

fn spawn_resize_task(target_rx: watch::Receiver<Target>) -> anyhow::Result<JoinHandle<()>> {
    let mut signal = signal(SignalKind::window_change())?;
    Ok(tokio::spawn(async move {
        while signal.recv().await.is_some() {
            let size = tty::current_size();
            let target = target_rx.borrow().clone();
            let _ = target
                .client
                .send(&ClientMsg::Resize {
                    attach_id: target.attach_id.clone(),
                    cols: size.cols,
                    rows: size.rows,
                })
                .await;
        }
    }))
}

async fn output_loop(
    client: Client,
    attach_id: AttachId,
    title_head: &str,
    input_filter: Arc<Mutex<BridgeInputFilter>>,
) -> Outcome {
    loop {
        let msg = match client.recv().await {
            Ok(msg) => msg,
            // A recv error means the daemon closed the connection — almost
            // always a hot-upgrade. Ask the bridge to reconnect.
            Err(_) => return Outcome::Disconnected,
        };
        match msg {
            ServerMsg::Output {
                attach_id: msg_attach,
                bytes,
                ..
            } if msg_attach == attach_id => {
                input_filter.lock().observe_output(&bytes, Instant::now());
                if let Err(err) = paint(&bytes) {
                    return Outcome::Failed(err);
                }
            }
            ServerMsg::Exited {
                attach_id: msg_attach,
                exit_code,
                ..
            } if msg_attach.as_ref().is_none_or(|id| id == &attach_id) => {
                return Outcome::Exited(exit_code.unwrap_or(0));
            }
            ServerMsg::DetachedByAnotherClient { attach_id: old } if old == attach_id => {
                return Outcome::Exited(0);
            }
            ServerMsg::CwdChanged {
                attach_id: msg_attach,
                cwd,
            } if msg_attach == attach_id => {
                let _ = std::env::set_current_dir(&cwd);
                // Keep the `ps`/`w` title pointed at the shell's current dir.
                crate::proctitle::set_title(&format!("{title_head} {}", display_cwd(&cwd)));
            }
            ServerMsg::Error { code, message } => {
                return Outcome::Failed(ServerError { code, message }.into());
            }
            _ => {}
        }
    }
}

#[derive(Default)]
struct BridgeInputFilter {
    stale_release_until: Option<Instant>,
    pending: Vec<u8>,
    pending_until: Option<Instant>,
    output_pending: Vec<u8>,
}

impl BridgeInputFilter {
    fn new() -> Self {
        Self::default()
    }

    fn observe_output(&mut self, bytes: &[u8], now: Instant) {
        let input = self.take_output_pending_then(bytes);
        if output_resets_kitty_keyboard_protocol(&input) {
            self.stale_release_until = Some(now + STALE_KEY_RELEASE_SUPPRESSION);
        }
        self.output_pending = trailing_keyboard_reset_prefix(&input);
    }

    fn filter_input(&mut self, bytes: &[u8], now: Instant) -> Vec<u8> {
        let mut out = if self.pending_expired(now) {
            self.flush_pending()
        } else {
            Vec::new()
        };

        if !self.should_filter(now) {
            out.extend_from_slice(&self.take_pending_then(bytes));
            return out;
        }

        let input = self.take_pending_then(bytes);
        out.reserve(input.len());
        let mut i = 0;
        while i < input.len() {
            if input[i] == 0x1b {
                match classify_csi_u(&input[i..]) {
                    CsiU::Release(len) => {
                        i += len;
                        continue;
                    }
                    CsiU::Other(len) => {
                        out.extend_from_slice(&input[i..i + len]);
                        i += len;
                        continue;
                    }
                    CsiU::Incomplete => {
                        self.set_pending(&input[i..], now);
                        break;
                    }
                    CsiU::NotCsiU => {}
                }
            }
            out.push(input[i]);
            i += 1;
        }
        out
    }

    fn pending_timeout(&self, now: Instant) -> Option<Duration> {
        if self.pending.is_empty() {
            return None;
        }
        Some(
            self.pending_until
                .unwrap_or(now)
                .saturating_duration_since(now),
        )
    }

    fn flush_pending(&mut self) -> Vec<u8> {
        self.pending_until = None;
        std::mem::take(&mut self.pending)
    }

    fn should_filter(&mut self, now: Instant) -> bool {
        match self.stale_release_until {
            Some(until) if now <= until => true,
            Some(_) => {
                self.stale_release_until = None;
                false
            }
            None => false,
        }
    }

    fn pending_expired(&self, now: Instant) -> bool {
        !self.pending.is_empty() && self.pending_until.is_none_or(|until| now >= until)
    }

    fn set_pending(&mut self, bytes: &[u8], now: Instant) {
        self.pending.clear();
        self.pending.extend_from_slice(bytes);
        self.pending_until = Some(now + PENDING_CSI_TIMEOUT);
    }

    fn take_pending_then(&mut self, bytes: &[u8]) -> Vec<u8> {
        if self.pending.is_empty() {
            return bytes.to_vec();
        }
        let mut input = Vec::with_capacity(self.pending.len() + bytes.len());
        input.extend_from_slice(&self.pending);
        input.extend_from_slice(bytes);
        self.pending.clear();
        self.pending_until = None;
        input
    }

    fn take_output_pending_then(&mut self, bytes: &[u8]) -> Vec<u8> {
        if self.output_pending.is_empty() {
            return bytes.to_vec();
        }
        let mut input = Vec::with_capacity(self.output_pending.len() + bytes.len());
        input.extend_from_slice(&self.output_pending);
        input.extend_from_slice(bytes);
        self.output_pending.clear();
        input
    }
}

/// Kitty's keyboard protocol reports key releases as `CSI ... :3u` when an
/// application asks for release events. When that application restores the
/// keyboard mode on exit, a release already queued by the terminal can arrive
/// at the shell prompt instead. After observing the restore, drop only those
/// release reports for a short window.
fn output_resets_kitty_keyboard_protocol(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b'[' {
            match bytes[i + 2] {
                b'<' => {
                    let mut j = i + 3;
                    while j < bytes.len() && bytes[j].is_ascii_digit() {
                        j += 1;
                    }
                    if j < bytes.len() && bytes[j] == b'u' {
                        return true;
                    }
                }
                b'=' => {
                    if direct_keyboard_protocol_reset(&bytes[i + 3..]) {
                        return true;
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    false
}

fn trailing_keyboard_reset_prefix(bytes: &[u8]) -> Vec<u8> {
    let start = bytes.len().saturating_sub(MAX_OUTPUT_RESET_PREFIX);
    for i in start..bytes.len() {
        let suffix = &bytes[i..];
        if is_keyboard_reset_prefix(suffix) {
            return suffix.to_vec();
        }
    }
    Vec::new()
}

fn is_keyboard_reset_prefix(bytes: &[u8]) -> bool {
    match bytes {
        [0x1b] | [0x1b, b'['] => true,
        [0x1b, b'[', b'<', rest @ ..] => rest.iter().all(u8::is_ascii_digit),
        [0x1b, b'[', b'=', rest @ ..] => rest.iter().all(|b| matches!(*b, b'0'..=b'9' | b';')),
        _ => false,
    }
}

fn direct_keyboard_protocol_reset(bytes: &[u8]) -> bool {
    let Some(end) = bytes.iter().position(|b| *b == b'u') else {
        return false;
    };
    if bytes[..end]
        .iter()
        .any(|b| !matches!(*b, b'0'..=b'9' | b';'))
    {
        return false;
    }

    let mut params = bytes[..end].split(|b| *b == b';');
    let flags = params.next().and_then(parse_ascii_u32).unwrap_or_default();
    let mode = params.next().and_then(parse_ascii_u32).unwrap_or(1);
    match mode {
        1 => flags & (KEYBOARD_REPORT_EVENTS | KEYBOARD_REPORT_ALL_KEYS) == 0,
        3 => flags & (KEYBOARD_REPORT_EVENTS | KEYBOARD_REPORT_ALL_KEYS) != 0,
        _ => false,
    }
}

const KEYBOARD_REPORT_EVENTS: u32 = 0b10;
const KEYBOARD_REPORT_ALL_KEYS: u32 = 0b1000;

fn parse_ascii_u32(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

enum CsiU {
    Release(usize),
    Other(usize),
    Incomplete,
    NotCsiU,
}

fn classify_csi_u(bytes: &[u8]) -> CsiU {
    if bytes.is_empty() || bytes[0] != 0x1b {
        return CsiU::NotCsiU;
    }
    if bytes.len() == 1 {
        return CsiU::Incomplete;
    }
    if bytes[1] != b'[' {
        return CsiU::NotCsiU;
    }

    let mut i = 2;
    while i < bytes.len() {
        match bytes[i] {
            b'u' => {
                let params = &bytes[2..i];
                let len = i + 1;
                return if is_kitty_release_event(params) {
                    CsiU::Release(len)
                } else {
                    CsiU::Other(len)
                };
            }
            b'0'..=b'9' | b';' | b':' => i += 1,
            _ => return CsiU::NotCsiU,
        }
    }
    CsiU::Incomplete
}

fn is_kitty_release_event(params: &[u8]) -> bool {
    let mut fields = params.split(|b| *b == b';');
    let _key = fields.next();
    let Some(modifiers) = fields.next() else {
        return false;
    };
    let mut subfields = modifiers.split(|b| *b == b':');
    let _modifier = subfields.next();
    matches!(subfields.next(), Some(b"3"))
}

/// Reconnect to the (possibly just-upgraded) daemon and re-attach to `id`.
/// Retries while the new daemon comes up. Gives up immediately if the shell is
/// genuinely gone (a fresh daemon that never adopted it), since retrying can't
/// recover that.
async fn reconnect(id: &ShellId) -> anyhow::Result<(Client, AttachId, Vec<u8>)> {
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..RECONNECT_ATTEMPTS {
        if attempt > 0 {
            sleep(RECONNECT_DELAY).await;
        }
        match timeout(RECONNECT_HANDSHAKE_TIMEOUT, try_reattach(id)).await {
            Ok(Ok(reattach)) => return Ok(reattach),
            Ok(Err(err)) if is_not_found(&err) => return Err(err),
            Ok(Err(err)) => last_err = Some(err),
            Err(_) => last_err = Some(anyhow::anyhow!("re-attach handshake timed out")),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("daemon did not come back after upgrade")))
}

async fn try_reattach(id: &ShellId) -> anyhow::Result<(Client, AttachId, Vec<u8>)> {
    let client = Client::connect(ClientKind::Livesh).await?;
    let size = tty::current_size();
    let snapshot = client
        .open_shell(id.clone(), size.cols, size.rows, true)
        .await?;
    Ok((client, snapshot.attach_id, snapshot.screen_bytes))
}

fn is_not_found(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ServerError>()
        .is_some_and(|e| e.code == ErrorCode::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_filter_drops_stale_kitty_release_events() {
        let mut filter = BridgeInputFilter::new();
        let now = Instant::now();
        filter.observe_output(b"\x1b[<u", now);

        let out = filter.filter_input(b"a\x1b[100;1:3u\x1b[102;1:3ub", now);

        assert_eq!(out, b"ab");
    }

    #[test]
    fn input_filter_keeps_non_release_csi_u_events() {
        let mut filter = BridgeInputFilter::new();
        let now = Instant::now();
        filter.observe_output(b"\x1b[<1u", now);

        let out = filter.filter_input(
            b"\x1b[100;1:1u\x1b[100;1:2u\x1b[100;1u",
            now + Duration::from_millis(1),
        );

        assert_eq!(out, b"\x1b[100;1:1u\x1b[100;1:2u\x1b[100;1u");
    }

    #[test]
    fn input_filter_keeps_release_events_after_window_expires() {
        let mut filter = BridgeInputFilter::new();
        let now = Instant::now();
        filter.observe_output(b"\x1b[<8u", now);

        let out = filter.filter_input(
            b"\x1b[100;1:3u",
            now + STALE_KEY_RELEASE_SUPPRESSION + Duration::from_millis(1),
        );

        assert_eq!(out, b"\x1b[100;1:3u");
    }

    #[test]
    fn input_filter_handles_split_release_event() {
        let mut filter = BridgeInputFilter::new();
        let now = Instant::now();
        filter.observe_output(b"\x1b[<u", now);

        let first = filter.filter_input(b"hello \x1b[10", now);
        let second = filter.filter_input(b"0;1:3u world", now);

        assert_eq!(first, b"hello ");
        assert_eq!(second, b" world");
    }

    #[test]
    fn input_filter_flushes_pending_release_after_timeout() {
        let mut filter = BridgeInputFilter::new();
        let now = Instant::now();
        filter.observe_output(b"\x1b[<u", now);

        let first = filter.filter_input(b"\x1b[10", now);
        let second = filter.filter_input(
            b"0;1:3u!",
            now + STALE_KEY_RELEASE_SUPPRESSION + Duration::from_millis(1),
        );

        assert_eq!(first, b"");
        assert_eq!(second, b"\x1b[100;1:3u!");
    }

    #[test]
    fn input_filter_flushes_bare_escape_after_timeout() {
        let mut filter = BridgeInputFilter::new();
        let now = Instant::now();
        filter.observe_output(b"\x1b[<u", now);

        let first = filter.filter_input(b"\x1b", now);
        let second =
            filter.filter_input(b"x", now + PENDING_CSI_TIMEOUT + Duration::from_millis(1));

        assert_eq!(first, b"");
        assert_eq!(second, b"\x1bx");
    }

    #[test]
    fn input_filter_ignores_text_without_csi_prefix() {
        let mut filter = BridgeInputFilter::new();
        let now = Instant::now();
        filter.observe_output(b"\x1b[<u", now);

        let out = filter.filter_input(b"02;1:3u00;1:3u", now);

        assert_eq!(out, b"02;1:3u00;1:3u");
    }

    #[test]
    fn output_detection_covers_pop_and_direct_resets() {
        assert!(output_resets_kitty_keyboard_protocol(b"\x1b[<u"));
        assert!(output_resets_kitty_keyboard_protocol(b"\x1b[<8u"));
        assert!(output_resets_kitty_keyboard_protocol(b"\x1b[=0u"));
        assert!(output_resets_kitty_keyboard_protocol(b"\x1b[=10;3u"));
        assert!(!output_resets_kitty_keyboard_protocol(b"\x1b[=10;2u"));
        assert!(!output_resets_kitty_keyboard_protocol(b"\x1b[100;1:3u"));
    }

    #[test]
    fn output_detection_handles_split_keyboard_reset() {
        let mut filter = BridgeInputFilter::new();
        let now = Instant::now();
        filter.observe_output(b"\x1b[", now);
        filter.observe_output(b"<u", now);

        let out = filter.filter_input(b"\x1b[100;1:3u", now);

        assert_eq!(out, b"");
    }

    #[test]
    fn output_detection_handles_split_direct_reset() {
        let mut filter = BridgeInputFilter::new();
        let now = Instant::now();
        filter.observe_output(b"\x1b[=1", now);
        filter.observe_output(b"0;3u", now);

        let out = filter.filter_input(b"\x1b[100;1:3u", now);

        assert_eq!(out, b"");
    }
}
