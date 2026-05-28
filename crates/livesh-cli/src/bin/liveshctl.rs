use std::env;

use livesh_cli::{
    args::{LiveshctlMode, liveshctl_help, parse_liveshctl},
    client::Client,
    exit_code_for_error,
};
use livesh_protocol::{ClientKind, ClientMsg, ShellInfo, ShellStatus};

#[tokio::main]
async fn main() {
    let code = match run().await {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("liveshctl: {err:#}");
            exit_code_for_error(&err)
        }
    };
    std::process::exit(code);
}

async fn run() -> anyhow::Result<()> {
    match parse_liveshctl(env::args_os())? {
        LiveshctlMode::Help => {
            print!("{}", liveshctl_help());
            Ok(())
        }
        LiveshctlMode::Version => {
            println!("liveshctl {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        LiveshctlMode::List { json } => {
            let client = Client::connect_or_spawn(ClientKind::Liveshctl).await?;
            let shells = client.list_shells().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&shells)?);
            } else {
                print_shells(&shells);
            }
            Ok(())
        }
        LiveshctlMode::Rename { id, name } => {
            let client = Client::connect_or_spawn(ClientKind::Liveshctl).await?;
            client.expect_ok(ClientMsg::RenameShell { id, name }).await
        }
        LiveshctlMode::Kill { id } => {
            let client = Client::connect_or_spawn(ClientKind::Liveshctl).await?;
            client.expect_ok(ClientMsg::KillShell { id }).await
        }
        LiveshctlMode::Gc => {
            let client = Client::connect_or_spawn(ClientKind::Liveshctl).await?;
            client.expect_ok(ClientMsg::RunGc).await
        }
        LiveshctlMode::Status => {
            let client = Client::connect_or_spawn(ClientKind::Liveshctl).await?;
            client.ping().await?;
            println!("liveshd ok ({})", client.daemon_id);
            Ok(())
        }
        LiveshctlMode::UpgradeDaemon { binary } => {
            let client = Client::connect_or_spawn(ClientKind::Liveshctl).await?;
            client
                .expect_ok(ClientMsg::UpgradeDaemon {
                    binary: binary.clone(),
                })
                .await?;
            // The daemon execs in place, so the socket flaps briefly.
            // Wait for it to come back and confirm via ping.
            for _ in 0..200 {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                if let Ok(client) = Client::connect_or_spawn(ClientKind::Liveshctl).await {
                    if client.ping().await.is_ok() {
                        println!("liveshd upgraded ({})", client.daemon_id);
                        return Ok(());
                    }
                }
            }
            anyhow::bail!("liveshd did not come back after upgrade");
        }
    }
}

fn print_shells(shells: &[ShellInfo]) {
    if shells.is_empty() {
        return;
    }

    for shell in shells {
        let status = match shell.status {
            ShellStatus::Running => "running",
            ShellStatus::Exited => "exited",
            ShellStatus::Lost => "lost",
        };
        let attached = if shell.attached {
            "attached"
        } else {
            "detached"
        };
        println!(
            "{}\t{}\t{}\t{}\t{}",
            shell.id,
            shell.name,
            status,
            attached,
            shell.cwd.display()
        );
    }
}
