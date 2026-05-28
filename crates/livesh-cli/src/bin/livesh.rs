use std::{
    env,
    io::{self, BufRead, Write},
    os::unix::process::CommandExt,
    path::PathBuf,
    process::Command,
};

use anyhow::Context;
use livesh_cli::{
    args::{LiveshMode, livesh_help, parse_livesh},
    bridge,
    client::{Client, CreatedShell, ServerError},
    exit_code_for_error, state_json, tty,
};
use livesh_core::shell_resolve;
use livesh_protocol::{ClientKind, ClientMsg, ErrorCode};

const FD_LIMIT_TIERS_MS: &[u64] = &[
    3 * 24 * 60 * 60 * 1000, // 3 days
    24 * 60 * 60 * 1000,     // 1 day
];

const REEXEC_GUARD_ENV: &str = "LIVESH_INTERNAL_NO_REEXEC";

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
            cwd,
        } => run_managed_shell(name, state_json_fd, cwd, true).await,
        LiveshMode::Upgrade { name, cwd } => run_managed_shell(name, None, cwd, false).await,
    }
}

async fn run_managed_shell(
    name: Option<String>,
    state_json_fd: Option<i32>,
    cwd: Option<PathBuf>,
    allow_fallback: bool,
) -> anyhow::Result<i32> {
    let force_live = env::var_os("LIVESH_FORCE_LIVE").is_some();
    if allow_fallback && !force_live && !tty::stdin_stdout_are_tty() {
        return shell_resolve::exec_real_shell().map(|()| 0);
    }

    let client = match Client::connect_or_spawn(ClientKind::Livesh).await {
        Ok(client) => client,
        Err(err) => return fail_or_fallback(allow_fallback, err, "daemon unavailable"),
    };
    let _ = client.expect_ok(ClientMsg::RunGc).await;
    let config = livesh_core::config::Config::load()?;
    let shell_path = shell_resolve::resolve_real_shell_with_config(Some(&config.real_shell))?;
    let cwd = match cwd {
        Some(path) => path,
        None => env::current_dir().context("current directory")?,
    };
    let env = shell_resolve::filtered_current_env();
    let size = tty::current_size();
    let created = match create_shell_with_fd_recovery(
        &client,
        name,
        cwd,
        shell_path,
        env,
        size.cols,
        size.rows,
    )
    .await
    {
        Ok(created) => created,
        Err(err) => return fail_or_fallback(allow_fallback, err, "shell creation failed"),
    };

    if let Some(fd) = state_json_fd {
        state_json::write(fd, &created.id, &created.name, &created.restore_argv)?;
    }

    if env::var_os(REEXEC_GUARD_ENV).is_none() {
        drop(client);
        let exe = env::current_exe().context("resolve current_exe for reexec")?;
        let id = created.id.to_string();
        let err = Command::new(&exe)
            .arg("--open")
            .arg(&id)
            .env(REEXEC_GUARD_ENV, "1")
            .exec();
        let err = anyhow::Error::from(err)
            .context(format!("reexec {} --open {}", exe.display(), id));
        return fail_or_fallback(allow_fallback, err, "reexec failed");
    }

    bridge::open_and_bridge(client, created.id).await
}

fn fail_or_fallback(
    allow_fallback: bool,
    err: anyhow::Error,
    reason: &str,
) -> anyhow::Result<i32> {
    if !allow_fallback {
        return Err(err);
    }
    eprintln!("livesh: {reason} ({err:#}); falling back to real shell");
    shell_resolve::exec_real_shell().map(|()| 0)
}

/// Try create_shell; if the daemon reports FdLimit, kill idle detached shells
/// in tiers (3 days, 1 day, then prompt for all) and retry between tiers.
async fn create_shell_with_fd_recovery(
    client: &Client,
    name: Option<String>,
    cwd: PathBuf,
    shell_path: PathBuf,
    env: Vec<(String, String)>,
    cols: u16,
    rows: u16,
) -> anyhow::Result<CreatedShell> {
    match client
        .create_shell(
            name.clone(),
            cwd.clone(),
            shell_path.clone(),
            env.clone(),
            cols,
            rows,
        )
        .await
    {
        Ok(created) => return Ok(created),
        Err(err) if is_fd_limit(&err) => {}
        Err(err) => return Err(err),
    }

    for older_than_ms in FD_LIMIT_TIERS_MS.iter().copied() {
        let prompt = format!(
            "livesh: fd limit reached. Kill detached shells idle for >{}? [y/N] ",
            humanize_ms(older_than_ms)
        );
        if !prompt_yes_no(&prompt)? {
            anyhow::bail!("fd limit reached and user declined to kill detached sessions");
        }
        client
            .expect_ok(ClientMsg::KillIdleDetached { older_than_ms })
            .await?;
        match client
            .create_shell(
                name.clone(),
                cwd.clone(),
                shell_path.clone(),
                env.clone(),
                cols,
                rows,
            )
            .await
        {
            Ok(created) => return Ok(created),
            Err(err) if is_fd_limit(&err) => continue,
            Err(err) => return Err(err),
        }
    }

    if !prompt_yes_no("livesh: still no fd. Kill ALL detached sessions? [y/N] ")? {
        anyhow::bail!("fd limit reached and user declined to kill detached sessions");
    }
    client
        .expect_ok(ClientMsg::KillIdleDetached { older_than_ms: 0 })
        .await?;
    client.create_shell(name, cwd, shell_path, env, cols, rows).await
}

fn is_fd_limit(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ServerError>()
        .is_some_and(|e| e.code == ErrorCode::FdLimit)
}

fn prompt_yes_no(prompt: &str) -> anyhow::Result<bool> {
    eprint!("{prompt}");
    io::stderr().flush().ok();
    let stdin = io::stdin();
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn humanize_ms(ms: u64) -> String {
    let secs = ms / 1000;
    if secs >= 24 * 60 * 60 {
        format!("{}d", secs / (24 * 60 * 60))
    } else if secs >= 3600 {
        format!("{}h", secs / 3600)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{}s", secs)
    }
}
