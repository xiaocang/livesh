use std::{
    fs::{self, OpenOptions},
    os::unix::{fs::OpenOptionsExt, io::AsRawFd, process::CommandExt},
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, bail};
use livesh_core::paths::{RuntimePaths, ensure_runtime_dirs};
use livesh_protocol::ClientKind;
use nix::{fcntl, unistd};
use tokio::time::sleep;

use crate::client::Client;

#[allow(deprecated)]
pub async fn connect_or_spawn(kind: ClientKind) -> anyhow::Result<Client> {
    if let Ok(client) = Client::connect(kind.clone()).await
        && client.ping().await.is_ok()
    {
        return Ok(client);
    }

    let paths = RuntimePaths::resolve();
    ensure_runtime_dirs(&paths)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&paths.lock)
        .with_context(|| format!("open {}", paths.lock.display()))?;
    fcntl::flock(lock.as_raw_fd(), fcntl::FlockArg::LockExclusive).context("lock daemon spawn")?;

    if let Ok(client) = Client::connect(kind.clone()).await
        && client.ping().await.is_ok()
    {
        return Ok(client);
    }

    if paths.socket.exists() {
        let _ = fs::remove_file(&paths.socket);
    }

    drop(lock);
    spawn_daemon(&paths)?;

    for _ in 0..100 {
        if let Ok(client) = Client::connect(kind.clone()).await
            && client.ping().await.is_ok()
        {
            return Ok(client);
        }
        sleep(Duration::from_millis(25)).await;
    }

    bail!("liveshd did not become ready");
}

fn spawn_daemon(paths: &RuntimePaths) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("current executable")?;
    let daemon = exe
        .parent()
        .map(|dir| dir.join("liveshd"))
        .filter(|path| path.exists())
        .unwrap_or_else(|| exe.with_file_name("liveshd"));

    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&paths.log)
        .with_context(|| format!("open daemon log {}", paths.log.display()))?;
    let err = log.try_clone()?;

    let mut cmd = Command::new(&daemon);
    cmd.env("LIVESH_INTERNAL_DAEMON", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err));

    unsafe {
        cmd.pre_exec(|| {
            unistd::setsid().map_err(std::io::Error::other)?;
            Ok(())
        });
    }

    cmd.spawn()
        .with_context(|| format!("spawn {}", daemon.display()))?;
    Ok(())
}
