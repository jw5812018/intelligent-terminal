use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, Mutex};

use super::{WtAction, WtChannel, WtRequest, WtResponse};

/// Bidirectional channel to Windows Terminal using OSC 9001 escape sequences.
///
/// Sends requests via stdout as `\x1b]9001;WtaReq;{json}\x07`.
/// Receives responses via stdin as `\x1b]9001;WtaRes;{json}\x07`,
/// routed here through the event reader's `wt_tx` channel.
pub struct VtChannel {
    next_id: AtomicU64,
    /// Pending requests waiting for responses, keyed by request ID.
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<WtResponse>>>>,
    available: AtomicBool,
}

impl VtChannel {
    pub fn new(mut response_rx: mpsc::UnboundedReceiver<WtResponse>) -> Self {
        let pending: Arc<Mutex<HashMap<String, oneshot::Sender<WtResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Background task: route incoming responses to pending oneshot channels.
        let p = pending.clone();
        tokio::spawn(async move {
            while let Some(resp) = response_rx.recv().await {
                if let Some(tx) = p.lock().await.remove(&resp.id) {
                    let _ = tx.send(resp);
                }
            }
        });

        Self {
            next_id: AtomicU64::new(1),
            pending,
            available: AtomicBool::new(true),
        }
    }
}

#[async_trait::async_trait]
impl WtChannel for VtChannel {
    async fn request(&self, action: WtAction) -> anyhow::Result<WtResponse> {
        let id = format!("wta_{}", self.next_id.fetch_add(1, Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);

        // Write OSC 9001 to stdout.
        // Safe: ratatui only writes during terminal.draw(); WtChannel writes between draws.
        // Both use std::io::stdout().lock() which serializes at the OS level.
        let req = WtRequest {
            id: id.clone(),
            action,
        };
        let json = serde_json::to_string(&req)?;
        let osc = format!("\x1b]9001;WtaReq;{}\x07", json);
        {
            let mut out = std::io::stdout().lock();
            out.write_all(osc.as_bytes())?;
            out.flush()?;
        }

        // Wait with timeout.
        match tokio::time::timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(_)) => Err(anyhow::anyhow!("response channel dropped")),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(anyhow::anyhow!("request timed out after 30s"))
            }
        }
    }

    fn is_available(&self) -> bool {
        self.available.load(Relaxed)
    }
}
