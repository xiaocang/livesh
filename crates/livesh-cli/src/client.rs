use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, bail};
use livesh_core::paths::RuntimePaths;
use livesh_protocol::{
    AttachId, ClientHello, ClientKind, ClientMsg, ErrorCode, PROTOCOL_VERSION, ServerHello,
    ServerMsg, ShellId, ShellInfo, ShellStatus,
};
use tokio::{
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::Mutex,
};

use crate::{daemon_spawn, framing};

#[derive(Debug)]
pub struct ServerError {
    pub code: ErrorCode,
    pub message: String,
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for ServerError {}

#[derive(Clone)]
pub struct Client {
    reader: Arc<Mutex<OwnedReadHalf>>,
    writer: Arc<Mutex<OwnedWriteHalf>>,
    pub daemon_id: String,
}

#[derive(Debug, Clone)]
pub struct CreatedShell {
    pub id: ShellId,
    pub name: String,
    pub restore_argv: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub id: ShellId,
    pub attach_id: AttachId,
    pub seq: u64,
    pub name: String,
    pub status: ShellStatus,
    pub screen_bytes: Vec<u8>,
}

impl Client {
    pub async fn connect(kind: ClientKind) -> anyhow::Result<Self> {
        let paths = RuntimePaths::resolve();
        let stream = UnixStream::connect(&paths.socket)
            .await
            .with_context(|| format!("connect {}", paths.socket.display()))?;
        Self::from_stream(stream, kind).await
    }

    pub async fn connect_or_spawn(kind: ClientKind) -> anyhow::Result<Self> {
        daemon_spawn::connect_or_spawn(kind).await
    }

    pub async fn from_stream(mut stream: UnixStream, kind: ClientKind) -> anyhow::Result<Self> {
        let hello = ClientHello {
            protocol: PROTOCOL_VERSION,
            client_kind: kind,
        };
        framing::write_frame(&mut stream, &hello).await?;
        let server: ServerHello = framing::read_frame(&mut stream).await?;
        if server.protocol != PROTOCOL_VERSION {
            bail!("protocol mismatch: daemon uses {}", server.protocol);
        }
        let daemon_id = server.daemon_id;
        let (reader, writer) = stream.into_split();
        Ok(Self {
            reader: Arc::new(Mutex::new(reader)),
            writer: Arc::new(Mutex::new(writer)),
            daemon_id,
        })
    }

    pub async fn send(&self, msg: &ClientMsg) -> anyhow::Result<()> {
        let mut writer = self.writer.lock().await;
        framing::write_frame(&mut *writer, msg).await
    }

    pub async fn recv(&self) -> anyhow::Result<ServerMsg> {
        let mut reader = self.reader.lock().await;
        framing::read_frame(&mut *reader).await
    }

    pub async fn ping(&self) -> anyhow::Result<()> {
        self.send(&ClientMsg::Ping).await?;
        match self.recv().await? {
            ServerMsg::Pong => Ok(()),
            ServerMsg::Error { code, message } => Err(ServerError { code, message }.into()),
            other => bail!("unexpected daemon response to ping: {other:?}"),
        }
    }

    pub async fn create_shell(
        &self,
        name: Option<String>,
        cwd: PathBuf,
        shell_path: PathBuf,
        env: Vec<(String, String)>,
        cols: u16,
        rows: u16,
    ) -> anyhow::Result<CreatedShell> {
        self.send(&ClientMsg::CreateShell {
            name,
            cwd,
            shell_path,
            env,
            cols,
            rows,
        })
        .await?;

        match self.recv().await? {
            ServerMsg::Created {
                id,
                name,
                restore_argv,
            } => Ok(CreatedShell {
                id,
                name,
                restore_argv,
            }),
            ServerMsg::Error { code, message } => Err(ServerError { code, message }.into()),
            other => bail!("unexpected daemon response to create: {other:?}"),
        }
    }

    pub async fn open_shell(
        &self,
        id: ShellId,
        cols: u16,
        rows: u16,
        steal: bool,
    ) -> anyhow::Result<Snapshot> {
        self.send(&ClientMsg::OpenShell {
            id,
            cols,
            rows,
            steal,
        })
        .await?;

        match self.recv().await? {
            ServerMsg::Snapshot {
                id,
                attach_id,
                seq,
                name,
                status,
                screen_bytes,
            } => Ok(Snapshot {
                id,
                attach_id,
                seq,
                name,
                status,
                screen_bytes,
            }),
            ServerMsg::Error { code, message } => Err(ServerError { code, message }.into()),
            other => bail!("unexpected daemon response to open: {other:?}"),
        }
    }

    pub async fn list_shells(&self) -> anyhow::Result<Vec<ShellInfo>> {
        self.send(&ClientMsg::ListShells).await?;
        match self.recv().await? {
            ServerMsg::ShellList { shells } => Ok(shells),
            ServerMsg::Error { code, message } => Err(ServerError { code, message }.into()),
            other => bail!("unexpected daemon response to list: {other:?}"),
        }
    }

    pub async fn expect_ok(&self, msg: ClientMsg) -> anyhow::Result<()> {
        self.send(&msg).await?;
        match self.recv().await? {
            ServerMsg::Ok => Ok(()),
            ServerMsg::Error { code, message } => Err(ServerError { code, message }.into()),
            other => bail!("unexpected daemon response: {other:?}"),
        }
    }
}
