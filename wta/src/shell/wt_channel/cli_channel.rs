use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context};
use tokio::sync::mpsc;

use crate::app::DebugMessage;
use super::WtChannel;

/// Extract a JSON value as a string, handling both String and Number types.
/// Protocol IDs may arrive as either strings or numbers depending on the caller.
fn json_id_as_str(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Resolve the full path to `wtcli.exe` at startup.
fn resolve_wtcli_path() -> String {
    // 1. Explicit override via environment variable.
    if let Ok(p) = std::env::var("WT_WTCLI_PATH") {
        if std::path::Path::new(&p).exists() {
            return p;
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        // 2. Sibling of current exe (installed scenario: wta.exe and wtcli.exe co-located).
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("wtcli.exe");
            if sibling.exists() {
                return sibling.to_string_lossy().to_string();
            }
        }

        // 3. Walk up from exe to repo root, check bin/x64/{Debug,Release}/wtcli/wtcli.exe (dev builds).
        //    The project output directory (wtcli/) contains the .winmd needed for MBM marshaling.
        let mut cursor = exe.parent().map(|p| p.to_path_buf());
        while let Some(dir) = cursor {
            for sub in &["bin/x64/Debug/wtcli/wtcli.exe", "bin/x64/Release/wtcli/wtcli.exe"] {
                let candidate = dir.join(sub);
                if candidate.exists() {
                    return candidate.to_string_lossy().to_string();
                }
            }
            let parent = dir.parent().map(|p| p.to_path_buf());
            if parent.as_deref() == Some(dir.as_path()) {
                break;
            }
            cursor = parent;
        }
    }

    // 4. Fall back to PATH search.
    "wtcli".to_string()
}

/// Channel that invokes `wtcli.exe` for protocol operations.
/// Replaces the old PipeChannel (named-pipe transport).
pub struct CliChannel {
    available: AtomicBool,
    debug_tx: Option<mpsc::UnboundedSender<DebugMessage>>,
    event_tx: std::sync::Mutex<Option<mpsc::UnboundedSender<serde_json::Value>>>,
    wtcli_path: String,
}

impl CliChannel {
    pub async fn connect() -> anyhow::Result<Self> {
        // WT_COM_CLSID must be set — wtcli reads it from the environment.
        if std::env::var("WT_COM_CLSID").is_err() && std::env::var("WT_PIPE_NAME").is_err() {
            bail!("Neither WT_COM_CLSID nor WT_PIPE_NAME set. Must run inside a Windows Terminal pane.");
        }

        Ok(Self {
            available: AtomicBool::new(true),
            debug_tx: None,
            event_tx: std::sync::Mutex::new(None),
            wtcli_path: resolve_wtcli_path(),
        })
    }

    pub async fn connect_with(pipe_name: &str, _token: &str) -> anyhow::Result<Self> {
        // For backward compat: pipe_name may be a COM CLSID or an actual pipe name.
        // Either way, wtcli handles it via its own environment.
        if pipe_name.is_empty() {
            bail!("Empty connection identifier");
        }

        Ok(Self {
            available: AtomicBool::new(true),
            debug_tx: None,
            event_tx: std::sync::Mutex::new(None),
            wtcli_path: resolve_wtcli_path(),
        })
    }

    pub fn with_debug_sender(mut self, tx: mpsc::UnboundedSender<DebugMessage>) -> Self {
        self.debug_tx = Some(tx);
        self
    }

    pub fn subscribe_events(&self) -> mpsc::UnboundedReceiver<serde_json::Value> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.event_tx.lock().unwrap() = Some(tx);
        rx
    }

    /// Start background event listener (wraps `wtcli listen --json`).
    pub async fn start_reader(self: &std::sync::Arc<Self>) {
        let wtcli = self.wtcli_path.clone();
        let weak = std::sync::Arc::downgrade(self);
        tokio::spawn(async move {
            let Ok(mut child) = tokio::process::Command::new(&wtcli)
                .args(["--json", "listen"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
            else {
                return;
            };

            let stdout = child.stdout.take().unwrap();
            let mut reader = tokio::io::BufReader::new(stdout);
            let mut line = String::new();

            loop {
                line.clear();
                use tokio::io::AsyncBufReadExt;
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        let Some(this) = weak.upgrade() else { break };
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                            let tx = this.event_tx.lock().unwrap();
                            if let Some(tx) = tx.as_ref() {
                                let _ = tx.send(val);
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }

    /// Run a wtcli subcommand and return the parsed JSON output.
    async fn run_wtcli(&self, args: &[&str]) -> anyhow::Result<serde_json::Value> {
        let output = tokio::process::Command::new(&self.wtcli_path)
            .arg("--json")
            .args(args)
            .output()
            .await
            .context("Failed to run wtcli")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("wtcli failed: {}", stderr.trim());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let trimmed = stdout.trim();
        if trimmed.is_empty() {
            return Ok(serde_json::Value::Null);
        }
        let val: serde_json::Value = serde_json::from_str(trimmed)
            .context("Failed to parse wtcli JSON output")?;
        Ok(val)
    }
}

#[async_trait::async_trait]
impl WtChannel for CliChannel {
    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        // Map protocol method names to wtcli subcommands + args.
        match method {
            "list_windows" => self.run_wtcli(&["list-windows"]).await,
            "list_tabs" => {
                let mut args = vec!["list-tabs"];
                let wid = params.get("window_id").and_then(json_id_as_str).unwrap_or_default();
                if !wid.is_empty() {
                    args.extend(["-w", &wid]);
                }
                self.run_wtcli(&args).await
            }
            "list_panes" => {
                let mut args = vec!["list-panes"];
                let wid = params.get("window_id").and_then(json_id_as_str).unwrap_or_default();
                let tid = params.get("tab_id").and_then(json_id_as_str).unwrap_or_default();
                if !wid.is_empty() {
                    args.extend(["-w", &wid]);
                }
                if !tid.is_empty() {
                    args.extend(["-t", &tid]);
                }
                self.run_wtcli(&args).await
            }
            "get_active_pane" => self.run_wtcli(&["active-pane"]).await,
            "read_pane_output" => {
                let pane_id = params.get("pane_id").and_then(json_id_as_str).unwrap_or_default();
                let max_lines = params.get("max_lines").and_then(|v| v.as_i64()).unwrap_or(200);
                let source = params.get("source").and_then(|v| v.as_str()).unwrap_or("");
                let lines_owned = max_lines.to_string();
                let mut args = vec!["capture-pane"];
                if !pane_id.is_empty() {
                    args.extend(["-t", &pane_id]);
                }
                if source == "last_prompt" {
                    args.push("--last-prompt");
                } else {
                    args.extend(["-l", &lines_owned]);
                }
                self.run_wtcli(&args).await
            }
            "get_process_status" => {
                let pane_id = params.get("pane_id").and_then(json_id_as_str).unwrap_or_default();
                let mut args = vec!["pane-status"];
                if !pane_id.is_empty() {
                    args.extend(["-t", &pane_id]);
                }
                self.run_wtcli(&args).await
            }
            "create_tab" => {
                let mut args = vec!["new-tab"];
                let cmd = params.get("commandline").and_then(|v| v.as_str()).unwrap_or("");
                let title = params.get("title").and_then(|v| v.as_str()).unwrap_or("");
                let cwd = params.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
                let cmd_owned;
                let title_owned;
                let cwd_owned;
                if !cmd.is_empty() {
                    cmd_owned = cmd.to_string();
                    args.extend(["-c", &cmd_owned]);
                }
                if !title.is_empty() {
                    title_owned = title.to_string();
                    args.extend(["-n", &title_owned]);
                }
                if !cwd.is_empty() {
                    cwd_owned = cwd.to_string();
                    args.extend(["-d", &cwd_owned]);
                }
                self.run_wtcli(&args).await
            }
            "split_pane" => {
                let pane_id = params.get("pane_id").and_then(json_id_as_str).unwrap_or_default();
                let cmd = params.get("commandline").and_then(|v| v.as_str()).unwrap_or("");
                let dir = params.get("direction").and_then(|v| v.as_str()).unwrap_or("");
                let cmd_owned;
                let mut args = vec!["split-pane"];
                if !pane_id.is_empty() {
                    args.extend(["-t", &pane_id]);
                }
                if dir == "horizontal" || dir == "down" || dir == "up" {
                    args.push("-H");
                } else {
                    args.push("-v");
                }
                if !cmd.is_empty() {
                    cmd_owned = cmd.to_string();
                    args.extend(["-c", &cmd_owned]);
                }
                self.run_wtcli(&args).await
            }
            "close_pane" => {
                let pane_id = params.get("pane_id").and_then(json_id_as_str).unwrap_or_default();
                self.run_wtcli(&["kill-pane", "-t", &pane_id]).await
            }
            "send_input" => {
                let pane_id = params.get("pane_id").and_then(json_id_as_str).unwrap_or_default();
                let text = params.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let text_owned = text.to_string();
                let mut args = vec!["send-keys"];
                if !pane_id.is_empty() {
                    args.extend(["-t", &pane_id]);
                }
                args.push(&text_owned);
                self.run_wtcli(&args).await
            }
            "get_capabilities" => self.run_wtcli(&["info"]).await,
            "quick_pick" => {
                let title = params.get("title").and_then(|v| v.as_str()).unwrap_or("");
                let title_owned = title.to_string();
                let mut args = vec!["quick-pick"];
                if !title_owned.is_empty() {
                    args.extend(["--title", &title_owned]);
                }
                if let Some(choices) = params.get("choices").and_then(|v| v.as_array()) {
                    for c in choices {
                        if let Some(s) = c.as_str() {
                            args.push(s);
                        }
                    }
                }
                self.run_wtcli(&args).await
            }
            other => bail!("Unsupported method: {}", other),
        }
    }

    fn is_available(&self) -> bool {
        self.available.load(Ordering::Relaxed)
    }
}
