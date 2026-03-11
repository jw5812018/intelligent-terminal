use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncReadExt;
use tokio::process::Child;
use tokio::process::Command;

/// Configuration for creating a new terminal.
pub struct TerminalConfig {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
}

/// Output from a managed terminal.
pub struct TerminalOutput {
    pub data: String,
    pub exit_status: Option<u32>,
}

/// A managed terminal subprocess.
struct ManagedTerminal {
    child: Mutex<Child>,
    output: Arc<Mutex<String>>,
    exited: Arc<Mutex<Option<u32>>>,
}

/// Protocol-agnostic shell integration layer.
/// Manages terminal subprocesses — shared between ACP and MCP modes.
pub struct ShellManager {
    terminals: Mutex<HashMap<String, ManagedTerminal>>,
    next_id: Mutex<u64>,
}

impl ShellManager {
    pub fn new() -> Self {
        Self {
            terminals: Mutex::new(HashMap::new()),
            next_id: Mutex::new(1),
        }
    }

    /// Spawn a new managed terminal, return its ID.
    pub async fn create_terminal(&self, config: TerminalConfig) -> anyhow::Result<String> {
        let id = {
            let mut next = self.next_id.lock().unwrap();
            let id = format!("term_{}", *next);
            *next += 1;
            id
        };

        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        if let Some(ref dir) = config.cwd {
            cmd.current_dir(dir);
        }
        for (k, v) in &config.env {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn()?;

        let output = Arc::new(Mutex::new(String::new()));
        let exited = Arc::new(Mutex::new(None));

        // Spawn stdout capture task
        let out_buf = output.clone();
        if let Some(mut stdout) = child.stdout.take() {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                loop {
                    match stdout.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            if let Ok(s) = std::str::from_utf8(&buf[..n]) {
                                if let Ok(mut out) = out_buf.lock() {
                                    out.push_str(s);
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        // Spawn stderr capture task (into same buffer)
        let out_buf2 = output.clone();
        if let Some(mut stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                loop {
                    match stderr.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            if let Ok(s) = std::str::from_utf8(&buf[..n]) {
                                if let Ok(mut out) = out_buf2.lock() {
                                    out.push_str(s);
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        self.terminals.lock().unwrap().insert(
            id.clone(),
            ManagedTerminal {
                child: Mutex::new(child),
                output,
                exited,
            },
        );

        Ok(id)
    }

    /// Get buffered output and optionally the exit status.
    pub fn get_output(&self, terminal_id: &str) -> anyhow::Result<TerminalOutput> {
        let terminals = self.terminals.lock().unwrap();
        let term = terminals
            .get(terminal_id)
            .ok_or_else(|| anyhow::anyhow!("unknown terminal: {}", terminal_id))?;

        let data = {
            let mut buf = term.output.lock().unwrap();
            let s = buf.clone();
            buf.clear();
            s
        };
        let exit_status = *term.exited.lock().unwrap();
        Ok(TerminalOutput { data, exit_status })
    }

    /// Wait for a terminal to exit, return exit code.
    pub async fn wait_for_exit(&self, terminal_id: &str) -> anyhow::Result<u32> {
        // Verify the terminal exists before entering the poll loop.
        {
            let terminals = self.terminals.lock().unwrap();
            if !terminals.contains_key(terminal_id) {
                return Err(anyhow::anyhow!("unknown terminal: {}", terminal_id));
            }
        }

        // Can't hold Mutex across await, so poll with try_wait instead.
        loop {
            {
                let terminals = self.terminals.lock().unwrap();
                if let Some(term) = terminals.get(terminal_id) {
                    let mut child = term.child.lock().unwrap();
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            let code = status.code().unwrap_or(1) as u32;
                            *term.exited.lock().unwrap() = Some(code);
                            return Ok(code);
                        }
                        Ok(None) => {} // still running
                        Err(e) => return Err(e.into()),
                    }
                } else {
                    return Err(anyhow::anyhow!("unknown terminal: {}", terminal_id));
                }
            }
            // Yield, then try again
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// Kill a terminal's process.
    pub fn kill(&self, terminal_id: &str) -> anyhow::Result<()> {
        let terminals = self.terminals.lock().unwrap();
        let term = terminals
            .get(terminal_id)
            .ok_or_else(|| anyhow::anyhow!("unknown terminal: {}", terminal_id))?;
        let mut child = term.child.lock().unwrap();
        let _ = child.start_kill();
        Ok(())
    }

    /// Release (kill + remove) a terminal.
    pub fn release(&self, terminal_id: &str) -> anyhow::Result<()> {
        let mut terminals = self.terminals.lock().unwrap();
        if let Some(term) = terminals.remove(terminal_id) {
            let mut child = term.child.lock().unwrap();
            let _ = child.start_kill();
        }
        Ok(())
    }
}
