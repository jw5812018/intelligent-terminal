use agent_client_protocol as acp;
use acp::Agent as _;
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::app::{AppEvent, PermOption, PlanEntry, PlanEntryStatus};
use crate::shell::{ShellManager, TerminalConfig};

/// Shared state accessible from the Client trait impl.
struct ClientState {
    event_tx: mpsc::UnboundedSender<AppEvent>,
    shell_mgr: Arc<ShellManager>,
}

/// Our Client trait implementation — handles incoming agent requests and notifications.
struct WtaClient {
    state: Arc<ClientState>,
}

#[async_trait::async_trait(?Send)]
impl acp::Client for WtaClient {
    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        let description = args
            .tool_call
            .fields
            .title
            .clone()
            .unwrap_or_else(|| "Permission requested".to_string());

        let options: Vec<PermOption> = args
            .options
            .iter()
            .map(|o| PermOption {
                id: o.option_id.to_string(),
                name: o.name.clone(),
                kind: format!("{:?}", o.kind),
            })
            .collect();

        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();

        let _ = self.state.event_tx.send(AppEvent::PermissionRequest {
            description,
            options,
            responder: resp_tx,
        });

        // Wait for user to choose
        match resp_rx.await {
            Ok(option_id) => Ok(acp::RequestPermissionResponse::new(
                acp::RequestPermissionOutcome::Selected(
                    acp::SelectedPermissionOutcome::new(option_id),
                ),
            )),
            Err(_) => Ok(acp::RequestPermissionResponse::new(
                acp::RequestPermissionOutcome::Cancelled,
            )),
        }
    }

    async fn session_notification(
        &self,
        args: acp::SessionNotification,
    ) -> acp::Result<()> {
        match args.update {
            acp::SessionUpdate::AgentMessageChunk(chunk) => {
                if let acp::ContentBlock::Text(text_content) = chunk.content {
                    let _ = self
                        .state
                        .event_tx
                        .send(AppEvent::AgentMessageChunk(text_content.text));
                }
            }
            acp::SessionUpdate::ToolCall(tool_call) => {
                let _ = self.state.event_tx.send(AppEvent::ToolCall {
                    id: tool_call.tool_call_id.to_string(),
                    title: tool_call.title.clone(),
                    status: format!("{:?}", tool_call.status),
                });
            }
            acp::SessionUpdate::ToolCallUpdate(update) => {
                if let Some(status) = &update.fields.status {
                    let _ = self.state.event_tx.send(AppEvent::ToolCallUpdate {
                        id: update.tool_call_id.to_string(),
                        status: format!("{:?}", status),
                    });
                }
            }
            acp::SessionUpdate::Plan(plan) => {
                let entries = plan
                    .entries
                    .iter()
                    .map(|e| PlanEntry {
                        content: e.content.clone(),
                        status: match e.status {
                            acp::PlanEntryStatus::Completed => PlanEntryStatus::Completed,
                            acp::PlanEntryStatus::InProgress => PlanEntryStatus::InProgress,
                            _ => PlanEntryStatus::Pending,
                        },
                    })
                    .collect();
                let _ = self.state.event_tx.send(AppEvent::Plan(entries));
            }
            _ => {} // Ignore other update types for now
        }
        Ok(())
    }

    async fn create_terminal(
        &self,
        args: acp::CreateTerminalRequest,
    ) -> acp::Result<acp::CreateTerminalResponse> {
        let env: Vec<(String, String)> = args
            .env
            .iter()
            .map(|e| (e.name.clone(), e.value.clone()))
            .collect();
        let cwd = args.cwd.as_ref().map(|p| p.to_string_lossy().to_string());

        let config = TerminalConfig {
            command: args.command.clone(),
            args: args.args.clone(),
            cwd,
            env,
        };

        match self.state.shell_mgr.create_terminal(config).await {
            Ok(id) => {
                // Show tool-call-like feedback
                let _ = self.state.event_tx.send(AppEvent::ToolCall {
                    id: id.clone(),
                    title: format!("{} {}", args.command, args.args.join(" ")),
                    status: "running".to_string(),
                });
                Ok(acp::CreateTerminalResponse::new(id))
            }
            Err(e) => Err(acp::Error::internal_error().data(e.to_string())),
        }
    }

    async fn terminal_output(
        &self,
        args: acp::TerminalOutputRequest,
    ) -> acp::Result<acp::TerminalOutputResponse> {
        match self
            .state
            .shell_mgr
            .get_output(&args.terminal_id.to_string())
        {
            Ok(output) => {
                let mut resp = acp::TerminalOutputResponse::new(output.data, false);
                if let Some(code) = output.exit_status {
                    resp = resp.exit_status(acp::TerminalExitStatus::new().exit_code(code));
                }
                Ok(resp)
            }
            Err(e) => Err(acp::Error::internal_error().data(e.to_string())),
        }
    }

    async fn wait_for_terminal_exit(
        &self,
        args: acp::WaitForTerminalExitRequest,
    ) -> acp::Result<acp::WaitForTerminalExitResponse> {
        let tid = args.terminal_id.to_string();

        match self.state.shell_mgr.wait_for_exit(&tid).await {
            Ok(code) => {
                // Update tool call status
                let _ = self.state.event_tx.send(AppEvent::ToolCallUpdate {
                    id: tid,
                    status: format!("exited ({})", code),
                });
                Ok(acp::WaitForTerminalExitResponse::new(
                    acp::TerminalExitStatus::new().exit_code(code),
                ))
            }
            Err(e) => Err(acp::Error::internal_error().data(e.to_string())),
        }
    }

    async fn release_terminal(
        &self,
        args: acp::ReleaseTerminalRequest,
    ) -> acp::Result<acp::ReleaseTerminalResponse> {
        let _ = self
            .state
            .shell_mgr
            .release(&args.terminal_id.to_string());
        Ok(acp::ReleaseTerminalResponse::new())
    }

    async fn kill_terminal(
        &self,
        args: acp::KillTerminalRequest,
    ) -> acp::Result<acp::KillTerminalResponse> {
        let _ = self.state.shell_mgr.kill(&args.terminal_id.to_string());
        Ok(acp::KillTerminalResponse::new())
    }
}

/// Top-level ACP client task: spawn agent, handshake, prompt loop.
pub async fn run_acp_client(
    agent_cmd: String,
    initial_prompt: Option<String>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
    mut prompt_rx: mpsc::UnboundedReceiver<String>,
    shell_mgr: Arc<ShellManager>,
) {
    if let Err(e) =
        run_inner(agent_cmd, initial_prompt, event_tx.clone(), &mut prompt_rx, shell_mgr).await
    {
        let _ = event_tx.send(AppEvent::AgentError(format!("{:#}", e)));
    }
}

async fn run_inner(
    agent_cmd: String,
    initial_prompt: Option<String>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
    prompt_rx: &mut mpsc::UnboundedReceiver<String>,
    shell_mgr: Arc<ShellManager>,
) -> Result<()> {
    // Parse agent command into program + args
    let parts: Vec<&str> = agent_cmd.split_whitespace().collect();
    let program = parts
        .first()
        .ok_or_else(|| anyhow::anyhow!("empty agent command"))?;
    let args = &parts[1..];

    // Spawn agent subprocess
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn agent '{}': {}", agent_cmd, e))?;

    let outgoing = child.stdin.take().unwrap().compat_write();
    let incoming = child.stdout.take().unwrap().compat();

    let state = Arc::new(ClientState {
        event_tx: event_tx.clone(),
        shell_mgr,
    });

    let client = WtaClient {
        state: state.clone(),
    };

    let (conn, handle_io) = acp::ClientSideConnection::new(client, outgoing, incoming, |fut| {
        tokio::task::spawn_local(fut);
    });

    tokio::task::spawn_local(async move {
        if let Err(e) = handle_io.await {
            eprintln!("ACP I/O error: {:#}", e);
        }
    });

    // Initialize
    conn.initialize(
        acp::InitializeRequest::new(acp::ProtocolVersion::V1)
            .client_capabilities(acp::ClientCapabilities::new().terminal(true))
            .client_info(
                acp::Implementation::new("wta", env!("CARGO_PKG_VERSION"))
                    .title("Windows Terminal Agent"),
            ),
    )
    .await
    .map_err(|e| anyhow::anyhow!("initialize failed: {}", e))?;

    // Create session
    let cwd = std::env::current_dir().unwrap_or_default();
    let session = conn
        .new_session(acp::NewSessionRequest::new(cwd))
        .await
        .map_err(|e| anyhow::anyhow!("new_session failed: {}", e))?;

    let session_id = session.session_id.clone();

    // Notify app of connection
    let agent_name = program.to_string();
    let _ = event_tx.send(AppEvent::AgentConnected {
        name: agent_name,
        session_id: session_id.to_string(),
    });

    // Send initial prompt if provided
    if let Some(prompt_text) = initial_prompt {
        let _ = event_tx.send(AppEvent::AgentMessageChunk(String::new())); // trigger streaming state
        let result = conn
            .prompt(acp::PromptRequest::new(
                session_id.clone(),
                vec![prompt_text.into()],
            ))
            .await;
        let _ = event_tx.send(AppEvent::AgentMessageEnd);
        if let Err(e) = result {
            let _ = event_tx.send(AppEvent::AgentError(format!("prompt error: {}", e)));
        }
    }

    // Prompt loop: wait for user input, send to agent
    while let Some(text) = prompt_rx.recv().await {
        let result = conn
            .prompt(acp::PromptRequest::new(
                session_id.clone(),
                vec![text.into()],
            ))
            .await;
        let _ = event_tx.send(AppEvent::AgentMessageEnd);
        if let Err(e) = result {
            let _ = event_tx.send(AppEvent::AgentError(format!("prompt error: {}", e)));
        }
    }

    Ok(())
}
