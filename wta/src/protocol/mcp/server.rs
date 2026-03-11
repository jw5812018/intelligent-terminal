use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerInfo};
use rmcp::schemars;
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use serde::Deserialize;
use std::sync::Arc;

use crate::shell::{ShellManager, TerminalConfig};

#[derive(Clone)]
pub struct WtaMcpServer {
    shell_mgr: Arc<ShellManager>,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RunCommandParams {
    /// The command to run
    pub command: String,
    /// Arguments to pass to the command
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory (optional)
    pub cwd: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CreateTerminalParams {
    /// The command to run in the terminal
    pub command: String,
    /// Arguments to pass to the command
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory (optional)
    pub cwd: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TerminalIdParams {
    /// The terminal ID
    pub terminal_id: String,
}

impl WtaMcpServer {
    pub fn new(shell_mgr: Arc<ShellManager>) -> Self {
        Self {
            shell_mgr,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl WtaMcpServer {
    /// Run a command to completion and return its output.
    #[tool(description = "Run a command and return its output")]
    async fn run_command(&self, Parameters(params): Parameters<RunCommandParams>) -> String {
        let config = TerminalConfig {
            command: params.command,
            args: params.args,
            cwd: params.cwd,
            env: vec![],
        };

        let id = match self.shell_mgr.create_terminal(config).await {
            Ok(id) => id,
            Err(e) => return format!("Error creating terminal: {}", e),
        };

        let exit_code = match self.shell_mgr.wait_for_exit(&id).await {
            Ok(code) => code,
            Err(e) => return format!("Error waiting for exit: {}", e),
        };

        let output = match self.shell_mgr.get_output(&id) {
            Ok(o) => o.data,
            Err(e) => return format!("Error getting output: {}", e),
        };

        let _ = self.shell_mgr.release(&id);

        format!("Exit code: {}\n\n{}", exit_code, output)
    }

    /// Create a persistent terminal session and return its ID.
    #[tool(description = "Create a persistent terminal session")]
    async fn create_terminal(
        &self,
        Parameters(params): Parameters<CreateTerminalParams>,
    ) -> String {
        let config = TerminalConfig {
            command: params.command,
            args: params.args,
            cwd: params.cwd,
            env: vec![],
        };

        match self.shell_mgr.create_terminal(config).await {
            Ok(id) => format!("Terminal created: {}", id),
            Err(e) => format!("Error creating terminal: {}", e),
        }
    }

    /// Get buffered output from a terminal session.
    #[tool(description = "Get output from a terminal session")]
    async fn get_terminal_output(
        &self,
        Parameters(params): Parameters<TerminalIdParams>,
    ) -> String {
        match self.shell_mgr.get_output(&params.terminal_id) {
            Ok(output) => {
                let status = match output.exit_status {
                    Some(code) => format!(" (exited: {})", code),
                    None => " (running)".to_string(),
                };
                format!("[{}{}]\n{}", params.terminal_id, status, output.data)
            }
            Err(e) => format!("Error: {}", e),
        }
    }

    /// Wait for a terminal to exit and return the exit code.
    #[tool(description = "Wait for a terminal to exit")]
    async fn wait_for_terminal(
        &self,
        Parameters(params): Parameters<TerminalIdParams>,
    ) -> String {
        match self.shell_mgr.wait_for_exit(&params.terminal_id).await {
            Ok(code) => format!("Terminal {} exited with code {}", params.terminal_id, code),
            Err(e) => format!("Error: {}", e),
        }
    }

    /// Kill a terminal session.
    #[tool(description = "Kill a terminal session")]
    async fn kill_terminal(
        &self,
        Parameters(params): Parameters<TerminalIdParams>,
    ) -> String {
        match self.shell_mgr.kill(&params.terminal_id) {
            Ok(()) => format!("Terminal {} killed", params.terminal_id),
            Err(e) => format!("Error: {}", e),
        }
    }
}

#[tool_handler]
impl ServerHandler for WtaMcpServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.server_info = Implementation::from_build_env();
        info.instructions = Some(
            "WTA (Windows Terminal Agent) — provides shell integration tools for running commands and managing terminal sessions.".into(),
        );
        info
    }
}

/// Run WTA as a headless MCP server over stdio.
pub async fn run_mcp_server(shell_mgr: Arc<ShellManager>) -> anyhow::Result<()> {
    let server = WtaMcpServer::new(shell_mgr);

    let service = server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| anyhow::anyhow!("MCP server error: {:?}", e))?;

    service
        .waiting()
        .await
        .map_err(|e| anyhow::anyhow!("MCP server error: {:?}", e))?;

    Ok(())
}
