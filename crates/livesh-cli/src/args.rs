use std::{ffi::OsString, path::PathBuf};

use anyhow::{Context, bail};
use livesh_protocol::ShellId;

#[derive(Debug, Clone)]
pub enum LiveshMode {
    New {
        name: Option<String>,
        state_json_fd: Option<i32>,
        cwd: Option<PathBuf>,
    },
    Open {
        id: ShellId,
    },
    Real,
    Help,
    Version,
}

#[derive(Debug, Clone)]
pub enum LiveshctlMode {
    List { json: bool },
    Rename { id: ShellId, name: String },
    Kill { id: ShellId },
    Gc,
    Status,
    Help,
    Version,
}

pub fn parse_livesh<I>(args: I) -> anyhow::Result<LiveshMode>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let _program = args.next();
    let mut name = None;
    let mut state_json_fd = None;
    let mut cwd: Option<PathBuf> = None;

    while let Some(arg) = args.next() {
        let arg = arg.to_string_lossy();
        match arg.as_ref() {
            "--help" | "-h" => return Ok(LiveshMode::Help),
            "--version" | "-V" => return Ok(LiveshMode::Version),
            "--real" => return Ok(LiveshMode::Real),
            "--open" => {
                let id = args.next().context("--open requires a shell id")?;
                let id = ShellId::new(id.to_string_lossy().to_string())?;
                return Ok(LiveshMode::Open { id });
            }
            "--name" => {
                let value = args.next().context("--name requires a value")?;
                name = Some(value.to_string_lossy().to_string());
            }
            "--state-json-fd" => {
                let value = args
                    .next()
                    .context("--state-json-fd requires a file descriptor")?;
                let fd = value
                    .to_string_lossy()
                    .parse::<i32>()
                    .context("invalid --state-json-fd value")?;
                state_json_fd = Some(fd);
            }
            "--cwd" => {
                let value = args.next().context("--cwd requires a directory path")?;
                cwd = Some(PathBuf::from(value));
            }
            unknown => bail!("unknown livesh argument: {unknown}"),
        }
    }

    Ok(LiveshMode::New {
        name,
        state_json_fd,
        cwd,
    })
}

pub fn parse_liveshctl<I>(args: I) -> anyhow::Result<LiveshctlMode>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let _program = args.next();
    let Some(cmd) = args.next() else {
        return Ok(LiveshctlMode::Help);
    };
    let cmd = cmd.to_string_lossy();

    match cmd.as_ref() {
        "--help" | "-h" | "help" => Ok(LiveshctlMode::Help),
        "--version" | "-V" | "version" => Ok(LiveshctlMode::Version),
        "list" => {
            let json = args
                .next()
                .map(|arg| arg.to_string_lossy() == "--json")
                .unwrap_or(false);
            Ok(LiveshctlMode::List { json })
        }
        "rename" => {
            let id = args.next().context("rename requires a shell id")?;
            let name = args.next().context("rename requires a new name")?;
            Ok(LiveshctlMode::Rename {
                id: ShellId::new(id.to_string_lossy().to_string())?,
                name: name.to_string_lossy().to_string(),
            })
        }
        "kill" => {
            let id = args.next().context("kill requires a shell id")?;
            Ok(LiveshctlMode::Kill {
                id: ShellId::new(id.to_string_lossy().to_string())?,
            })
        }
        "gc" => Ok(LiveshctlMode::Gc),
        "status" => Ok(LiveshctlMode::Status),
        unknown => bail!("unknown liveshctl command: {unknown}"),
    }
}

pub fn livesh_help() -> &'static str {
    "usage: livesh [--name <name>] [--cwd <dir>] [--state-json-fd <fd>]\n       livesh --open <sh_id>\n       livesh --real\n"
}

pub fn liveshctl_help() -> &'static str {
    "usage: liveshctl list [--json]\n       liveshctl rename <sh_id> <name>\n       liveshctl kill <sh_id>\n       liveshctl gc\n       liveshctl status\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_new_livesh_options() {
        let mode = parse_livesh(os(&[
            "livesh",
            "--name",
            "dev",
            "--cwd",
            "/repo",
            "--state-json-fd",
            "3",
        ]))
        .unwrap();
        match mode {
            LiveshMode::New {
                name,
                state_json_fd,
                cwd,
            } => {
                assert_eq!(name.as_deref(), Some("dev"));
                assert_eq!(state_json_fd, Some(3));
                assert_eq!(cwd.as_deref(), Some(std::path::Path::new("/repo")));
            }
            other => panic!("unexpected mode: {other:?}"),
        }
    }

    #[test]
    fn parses_open_id() {
        let mode = parse_livesh(os(&["livesh", "--open", "sh_abc"])).unwrap();
        match mode {
            LiveshMode::Open { id } => assert_eq!(id.as_str(), "sh_abc"),
            other => panic!("unexpected mode: {other:?}"),
        }
    }

    #[test]
    fn parses_liveshctl_json_list() {
        let mode = parse_liveshctl(os(&["liveshctl", "list", "--json"])).unwrap();
        match mode {
            LiveshctlMode::List { json } => assert!(json),
            other => panic!("unexpected mode: {other:?}"),
        }
    }
}
