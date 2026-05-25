use std::env;

use anyhow::Context;
use livesh_cli::{
    args::{LiveshMode, livesh_help, parse_livesh},
    bridge,
    client::Client,
    exit_code_for_error, state_json, tty,
};
use livesh_core::shell_resolve;
use livesh_protocol::ClientKind;

#[tokio::main]
async fn main() {
    let code = match run().await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("livesh: {err:#}");
            exit_code_for_error(&err)
        }
    };
    std::process::exit(code);
}

async fn run() -> anyhow::Result<i32> {
    match parse_livesh(env::args_os())? {
        LiveshMode::Help => {
            print!("{}", livesh_help());
            Ok(0)
        }
        LiveshMode::Version => {
            println!("livesh {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        LiveshMode::Real => shell_resolve::exec_real_shell().map(|()| 0),
        LiveshMode::Open { id } => {
            let client = Client::connect_or_spawn(ClientKind::Livesh).await?;
            bridge::open_and_bridge(client, id).await
        }
        LiveshMode::New {
            name,
            state_json_fd,
        } => {
            let force_live = env::var_os("LIVESH_FORCE_LIVE").is_some();
            if !force_live && !tty::stdin_stdout_are_tty() {
                return shell_resolve::exec_real_shell().map(|()| 0);
            }

            let client = Client::connect_or_spawn(ClientKind::Livesh).await?;
            let config = livesh_core::config::Config::load()?;
            let shell_path =
                shell_resolve::resolve_real_shell_with_config(Some(&config.real_shell))?;
            let cwd = env::current_dir().context("current directory")?;
            let env = shell_resolve::filtered_current_env();
            let size = tty::current_size();
            let created = client
                .create_shell(name, cwd, shell_path, env, size.cols, size.rows)
                .await?;

            if let Some(fd) = state_json_fd {
                state_json::write(fd, &created.id, &created.name, &created.restore_argv)?;
            }

            bridge::open_and_bridge(client, created.id).await
        }
    }
}
