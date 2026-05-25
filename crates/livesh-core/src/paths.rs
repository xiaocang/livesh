use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use livesh_protocol::ShellId;
use nix::unistd::Uid;

#[derive(Debug, Clone)]
pub struct RuntimePaths {
    pub base: PathBuf,
    pub socket: PathBuf,
    pub lock: PathBuf,
    pub daemon_json: PathBuf,
    pub states: PathBuf,
    pub snapshots: PathBuf,
    pub scrollback: PathBuf,
    pub events: PathBuf,
    pub tmp: PathBuf,
    pub log: PathBuf,
}

impl RuntimePaths {
    pub fn resolve() -> Self {
        let base = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) => PathBuf::from(dir).join("livesh"),
            None => PathBuf::from("/tmp").join(format!("livesh-{}", Uid::current().as_raw())),
        };

        Self {
            socket: base.join("liveshd.sock"),
            lock: base.join("liveshd.lock"),
            daemon_json: base.join("daemon.json"),
            states: base.join("states"),
            snapshots: base.join("snapshots"),
            scrollback: base.join("scrollback"),
            events: base.join("events"),
            tmp: base.join("tmp"),
            log: base.join("liveshd.log"),
            base,
        }
    }

    pub fn metadata(&self, id: &ShellId) -> PathBuf {
        self.states.join(format!("{}.json", id.as_str()))
    }

    pub fn snapshot(&self, id: &ShellId) -> PathBuf {
        self.snapshots.join(format!("{}.screen", id.as_str()))
    }

    pub fn scrollback(&self, id: &ShellId) -> PathBuf {
        self.scrollback.join(format!("{}.ring", id.as_str()))
    }

    pub fn events(&self, id: &ShellId) -> PathBuf {
        self.events.join(format!("{}.ring", id.as_str()))
    }
}

pub fn ensure_runtime_dirs(paths: &RuntimePaths) -> anyhow::Result<()> {
    ensure_private_dir(&paths.base)?;
    ensure_private_dir(&paths.states)?;
    ensure_private_dir(&paths.snapshots)?;
    ensure_private_dir(&paths.scrollback)?;
    ensure_private_dir(&paths.events)?;
    ensure_private_dir(&paths.tmp)?;
    Ok(())
}

pub fn ensure_private_dir(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        let meta = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
        if !meta.is_dir() {
            bail!("{} exists but is not a directory", path.display());
        }
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            bail!(
                "{} must not be accessible by group or other users; mode is {:o}",
                path.display(),
                mode
            );
        }
        return Ok(());
    }

    fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod 0700 {}", path.display()))?;
    Ok(())
}
