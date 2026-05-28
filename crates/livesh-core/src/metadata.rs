use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

use anyhow::Context;
use livesh_protocol::{ShellId, ShellStatus};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateMetadata {
    pub schema: u16,
    pub id: ShellId,
    pub name: String,
    pub status: ShellStatus,
    pub cwd: String,
    pub shell_path: String,
    pub daemon_id: String,
    pub created_at_ms: u128,
    pub last_active_at_ms: u128,
}

pub fn read_metadata(path: &Path) -> anyhow::Result<StateMetadata> {
    let bytes = fs::read(path).with_context(|| format!("read metadata {}", path.display()))?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn write_metadata(path: &Path, metadata: &StateMetadata) -> anyhow::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(metadata)?;
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("open metadata temp file {}", tmp.display()))?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all().ok();
    fs::rename(&tmp, path).with_context(|| format!("rename metadata {}", path.display()))?;
    Ok(())
}
