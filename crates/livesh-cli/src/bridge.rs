use std::io::Write;

use anyhow::bail;
use livesh_protocol::{AttachId, ClientMsg, ServerMsg, ShellId};
use tokio::{
    io::{AsyncReadExt, stdin},
    signal::unix::{SignalKind, signal},
    task::JoinHandle,
};

use crate::{client::Client, raw_mode::RawModeGuard, tty};

pub async fn open_and_bridge(client: Client, id: ShellId) -> anyhow::Result<i32> {
    let size = tty::current_size();
    let snapshot = client.open_shell(id, size.cols, size.rows, true).await?;
    bridge_snapshot(client, snapshot.attach_id, snapshot.screen_bytes).await
}

pub async fn bridge_snapshot(
    client: Client,
    attach_id: AttachId,
    screen_bytes: Vec<u8>,
) -> anyhow::Result<i32> {
    let raw_guard = if tty::stdin_stdout_are_tty() {
        Some(RawModeGuard::enter()?)
    } else {
        None
    };

    {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(&screen_bytes)?;
        stdout.flush()?;
    }

    let input_task = spawn_input_task(client.clone(), attach_id.clone());
    let resize_task = spawn_resize_task(client.clone(), attach_id.clone())?;

    let result = output_loop(client.clone(), attach_id.clone()).await;
    let _ = client.send(&ClientMsg::Detach { attach_id }).await;
    input_task.abort();
    resize_task.abort();
    drop(raw_guard);
    result
}

fn spawn_input_task(client: Client, attach_id: AttachId) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut input = stdin();
        let mut buf = [0_u8; 8192];
        while let Ok(n) = input.read(&mut buf).await {
            if n == 0 {
                break;
            }
            if client
                .send(&ClientMsg::Input {
                    attach_id: attach_id.clone(),
                    bytes: buf[..n].to_vec(),
                })
                .await
                .is_err()
            {
                break;
            }
        }
    })
}

fn spawn_resize_task(client: Client, attach_id: AttachId) -> anyhow::Result<JoinHandle<()>> {
    let mut signal = signal(SignalKind::window_change())?;
    Ok(tokio::spawn(async move {
        while signal.recv().await.is_some() {
            let size = tty::current_size();
            if client
                .send(&ClientMsg::Resize {
                    attach_id: attach_id.clone(),
                    cols: size.cols,
                    rows: size.rows,
                })
                .await
                .is_err()
            {
                break;
            }
        }
    }))
}

async fn output_loop(client: Client, attach_id: AttachId) -> anyhow::Result<i32> {
    loop {
        match client.recv().await? {
            ServerMsg::Output {
                attach_id: msg_attach,
                bytes,
                ..
            } if msg_attach == attach_id => {
                let mut stdout = std::io::stdout().lock();
                stdout.write_all(&bytes)?;
                stdout.flush()?;
            }
            ServerMsg::Exited {
                attach_id: msg_attach,
                exit_code,
                ..
            } if msg_attach.as_ref().is_none_or(|id| id == &attach_id) => {
                return Ok(exit_code.unwrap_or(0));
            }
            ServerMsg::DetachedByAnotherClient { attach_id: old } if old == attach_id => {
                return Ok(0);
            }
            ServerMsg::CwdChanged {
                attach_id: msg_attach,
                cwd,
            } if msg_attach == attach_id => {
                let _ = std::env::set_current_dir(&cwd);
            }
            ServerMsg::Error { code, message } => {
                bail!("{code:?}: {message}");
            }
            _ => {}
        }
    }
}
