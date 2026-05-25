use std::path::PathBuf;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

pub const PROTOCOL_VERSION: u16 = 2;
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ShellId(String);

impl ShellId {
    pub fn new(id: impl Into<String>) -> Result<Self, ProtocolError> {
        let id = id.into();
        if is_valid_shell_id(&id) {
            Ok(Self(id))
        } else {
            Err(ProtocolError::InvalidShellId(id))
        }
    }

    pub fn new_unchecked(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ShellId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AttachId(String);

impl AttachId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AttachId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShellStatus {
    Running,
    Exited,
    Lost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    Usage,
    NotFound,
    DaemonUnavailable,
    Internal,
    RuntimeDir,
    TemporaryFailure,
    TooManyShells,
    ProtocolMismatch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientHello {
    pub protocol: u16,
    pub client_kind: ClientKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientKind {
    Livesh,
    Liveshctl,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerHello {
    pub protocol: u16,
    pub daemon_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMsg {
    Ping,
    CreateShell {
        name: Option<String>,
        cwd: PathBuf,
        shell_path: PathBuf,
        env: Vec<(String, String)>,
        cols: u16,
        rows: u16,
    },
    OpenShell {
        id: ShellId,
        cols: u16,
        rows: u16,
        steal: bool,
    },
    Input {
        attach_id: AttachId,
        bytes: Vec<u8>,
    },
    Resize {
        attach_id: AttachId,
        cols: u16,
        rows: u16,
    },
    Detach {
        attach_id: AttachId,
    },
    ListShells,
    RenameShell {
        id: ShellId,
        name: String,
    },
    KillShell {
        id: ShellId,
    },
    RunGc,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMsg {
    Pong,
    Created {
        id: ShellId,
        name: String,
        restore_argv: Vec<String>,
    },
    Snapshot {
        id: ShellId,
        attach_id: AttachId,
        seq: u64,
        name: String,
        status: ShellStatus,
        screen_bytes: Vec<u8>,
    },
    Output {
        attach_id: AttachId,
        seq: u64,
        bytes: Vec<u8>,
    },
    Exited {
        id: ShellId,
        attach_id: Option<AttachId>,
        exit_code: Option<i32>,
    },
    DetachedByAnotherClient {
        attach_id: AttachId,
    },
    CwdChanged {
        attach_id: AttachId,
        cwd: PathBuf,
    },
    ShellList {
        shells: Vec<ShellInfo>,
    },
    Ok,
    Error {
        code: ErrorCode,
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellInfo {
    pub id: ShellId,
    pub name: String,
    pub status: ShellStatus,
    pub cwd: PathBuf,
    pub shell_path: PathBuf,
    pub created_at_ms: u128,
    pub last_active_at_ms: u128,
    pub attached: bool,
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("invalid shell id: {0}")]
    InvalidShellId(String),
    #[error("frame length {0} exceeds maximum {MAX_FRAME_LEN}")]
    FrameTooLarge(usize),
    #[error("decode failed: {0}")]
    Decode(#[from] bincode::Error),
}

pub fn is_valid_shell_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("sh_") else {
        return false;
    };

    !rest.is_empty()
        && rest.len() <= 64
        && rest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub fn encode_frame<T: Serialize>(value: &T) -> anyhow::Result<Vec<u8>> {
    let payload = bincode::serialize(value)?;
    if payload.len() > MAX_FRAME_LEN {
        anyhow::bail!("encoded frame exceeds maximum frame length");
    }

    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

pub fn decode_payload<T: DeserializeOwned>(payload: &[u8]) -> Result<T, ProtocolError> {
    if payload.len() > MAX_FRAME_LEN {
        return Err(ProtocolError::FrameTooLarge(payload.len()));
    }

    Ok(bincode::deserialize(payload)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_shell_ids() {
        assert!(is_valid_shell_id("sh_abc"));
        assert!(is_valid_shell_id("sh_2e7b9e9b-aaaa"));
        assert!(!is_valid_shell_id("abc"));
        assert!(!is_valid_shell_id("sh_../x"));
        assert!(!is_valid_shell_id("sh_"));
    }

    #[test]
    fn round_trips_frames() {
        let msg = ClientMsg::Ping;
        let frame = encode_frame(&msg).unwrap();
        let decoded: ClientMsg = decode_payload(&frame[4..]).unwrap();
        assert!(matches!(decoded, ClientMsg::Ping));
    }
}
