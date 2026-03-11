use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{bail, Context};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::ClientOptions;
use tokio::sync::Mutex;

use super::types::{WireRequest, WireResponse};
use super::WtChannel;

/// Named-pipe channel to the Windows Terminal protocol server.
///
/// Connects to `\\.\pipe\WindowsTerminal-<PID>` using env vars
/// `WT_PIPE_NAME` and `WT_MCP_TOKEN`. Protocol is line-delimited JSON
/// with serial request-response (one at a time).
pub struct PipeChannel {
    pipe: Mutex<tokio::net::windows::named_pipe::NamedPipeClient>,
    next_id: AtomicU64,
    available: AtomicBool,
}

impl PipeChannel {
    /// Connect to the WT protocol server and authenticate.
    ///
    /// Reads `WT_PIPE_NAME` and `WT_MCP_TOKEN` from environment variables.
    pub async fn connect() -> anyhow::Result<Self> {
        let pipe_name = std::env::var("WT_PIPE_NAME")
            .context("WT_PIPE_NAME not set. Must run inside a Windows Terminal pane with protocol access.")?;
        let token = std::env::var("WT_MCP_TOKEN")
            .context("WT_MCP_TOKEN not set. Must run inside a Windows Terminal pane with protocol access.")?;

        let pipe = ClientOptions::new()
            .open(&pipe_name)
            .context(format!("Failed to connect to pipe: {}", pipe_name))?;

        let channel = Self {
            pipe: Mutex::new(pipe),
            next_id: AtomicU64::new(1),
            available: AtomicBool::new(false),
        };

        // Authenticate
        let result = channel
            .request(
                "authenticate",
                serde_json::json!({ "token": token }),
            )
            .await
            .context("Authentication failed")?;

        let authenticated = result
            .get("authenticated")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if !authenticated {
            bail!("Authentication rejected by Windows Terminal");
        }

        channel.available.store(true, Ordering::Relaxed);
        Ok(channel)
    }
}

#[async_trait::async_trait]
impl WtChannel for PipeChannel {
    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();

        let wire_req = WireRequest {
            msg_type: "request",
            id,
            method,
            params,
        };

        let mut json = serde_json::to_string(&wire_req)?;
        json.push('\n');

        let mut pipe = self.pipe.lock().await;

        // Write request
        pipe.write_all(json.as_bytes()).await?;

        // Read response line (byte-by-byte until \n)
        let mut buf = Vec::with_capacity(4096);
        loop {
            let byte = pipe.read_u8().await?;
            if byte == b'\n' {
                break;
            }
            buf.push(byte);
        }

        let resp: WireResponse = serde_json::from_slice(&buf)
            .context("Failed to parse response from Windows Terminal")?;

        if let Some(err) = resp.error {
            bail!("WT protocol error [{}]: {}", err.code, err.message);
        }

        Ok(resp.result.unwrap_or(serde_json::Value::Null))
    }

    fn is_available(&self) -> bool {
        self.available.load(Ordering::Relaxed)
    }
}
