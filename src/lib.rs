pub mod protocol;

pub mod config;
pub mod gc;
pub mod limits;
pub mod metadata;
pub mod paths;
pub mod proctitle;
pub mod shell_cwd;
pub mod shell_resolve;
pub mod terminal_model;

pub mod args;
pub mod bridge;
pub mod client;
pub mod daemon;
pub mod daemon_spawn;
pub mod framing;
pub mod pty_master;
pub mod raw_mode;
pub mod state_json;
pub mod takeover;
pub mod tty;

pub const EXIT_USAGE: i32 = 64;
pub const EXIT_NOT_FOUND: i32 = 66;
pub const EXIT_DAEMON_UNAVAILABLE: i32 = 69;
pub const EXIT_INTERNAL: i32 = 70;
pub const EXIT_RUNTIME_DIR: i32 = 73;
pub const EXIT_TEMPORARY_FAILURE: i32 = 75;

pub fn exit_code_for_error(err: &anyhow::Error) -> i32 {
    if let Some(server) = err.downcast_ref::<client::ServerError>() {
        return match server.code {
            protocol::ErrorCode::Usage => EXIT_USAGE,
            protocol::ErrorCode::NotFound => EXIT_NOT_FOUND,
            protocol::ErrorCode::DaemonUnavailable => EXIT_DAEMON_UNAVAILABLE,
            protocol::ErrorCode::RuntimeDir => EXIT_RUNTIME_DIR,
            protocol::ErrorCode::TemporaryFailure => EXIT_TEMPORARY_FAILURE,
            _ => EXIT_INTERNAL,
        };
    }

    EXIT_INTERNAL
}
