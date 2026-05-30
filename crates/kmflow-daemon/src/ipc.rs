use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tracing::info;

#[derive(Debug, Serialize, Deserialize)]
pub enum IpcRequest {
    Status,
    Pair { addr: String },
    Layout { peer: String, edge: String },
    Stop,
    SetupFirewall,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum IpcResponse {
    Ok {
        message: String,
    },
    Error {
        message: String,
    },
    Status {
        state: String,
        peers: Vec<PeerStatus>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerStatus {
    pub hostname: String,
    pub addr: String,
    pub state: String,
    pub rtt_ms: f64,
    pub screen: String,
}

pub fn socket_path() -> PathBuf {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(runtime_dir).join("kmflow.sock")
}

pub struct IpcServer {
    listener: UnixListener,
}

impl IpcServer {
    pub async fn bind() -> Result<Self> {
        let path = socket_path();
        // Remove stale socket
        let _ = tokio::fs::remove_file(&path).await;
        let listener = UnixListener::bind(&path).context("bind IPC socket")?;
        info!(?path, "IPC server listening");
        Ok(Self { listener })
    }

    pub async fn accept(&self) -> Result<IpcConnection> {
        let (stream, _) = self.listener.accept().await.context("accept IPC")?;
        Ok(IpcConnection { stream })
    }
}

pub struct IpcConnection {
    stream: UnixStream,
}

impl IpcConnection {
    pub async fn read_request(&mut self) -> Result<IpcRequest> {
        let mut reader = BufReader::new(&mut self.stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.context("read IPC")?;
        serde_json::from_str(&line).context("parse IPC request")
    }

    pub async fn write_response(&mut self, response: &IpcResponse) -> Result<()> {
        let mut data = serde_json::to_string(response)?;
        data.push('\n');
        self.stream
            .write_all(data.as_bytes())
            .await
            .context("write IPC")?;
        Ok(())
    }
}

pub struct IpcClient;

impl IpcClient {
    pub async fn send(request: &IpcRequest) -> Result<IpcResponse> {
        let path = socket_path();
        let mut stream = UnixStream::connect(&path)
            .await
            .context("connect to daemon (is it running?)")?;

        let mut data = serde_json::to_string(request)?;
        data.push('\n');
        stream.write_all(data.as_bytes()).await?;

        let mut reader = BufReader::new(&mut stream);
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        serde_json::from_str(&line).context("parse IPC response")
    }
}
