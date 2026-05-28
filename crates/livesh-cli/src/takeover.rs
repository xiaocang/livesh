//! Cross-execv state transfer between an old liveshd and the new binary
//! it execs in-place. The old daemon clears FD_CLOEXEC on the fds we
//! want to keep, encodes a manifest into an env var, then execvs. The
//! new daemon parses the env var and adopts the fds.

use std::{
    os::unix::io::RawFd,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

/// Env var name carrying the takeover manifest (JSON).
pub const TAKEOVER_ENV: &str = "LIVESH_TAKEOVER";

/// The structure handed across execv. Listener fd plus one entry per
/// shell whose PTY master we want the new daemon to adopt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u16,
    pub listener_fd: RawFd,
    pub runtime_dir: PathBuf,
    pub daemon_id: String,
    pub shells: Vec<ShellHandoff>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellHandoff {
    pub id: String,
    pub master_fd: RawFd,
}

pub fn encode(manifest: &Manifest) -> String {
    // Manifest is small and never contains user data, so unwrap is fine.
    serde_json::to_string(manifest).expect("manifest serialization")
}

pub fn decode(env_value: &str) -> anyhow::Result<Manifest> {
    Ok(serde_json::from_str(env_value)?)
}

pub fn read_env() -> Option<anyhow::Result<Manifest>> {
    std::env::var(TAKEOVER_ENV).ok().map(|v| decode(&v))
}

pub fn clear_env() {
    // Safe in main(): no other threads have started yet.
    unsafe { std::env::remove_var(TAKEOVER_ENV) };
}

/// Check if the given path looks like the same liveshd binary the old
/// daemon was running. Used only to log a warning, never to block.
pub fn binary_matches(a: &Path, b: &Path) -> bool {
    a.canonicalize().ok() == b.canonicalize().ok()
}
