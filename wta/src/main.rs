mod app;
mod event;
mod protocol;
mod shell;
mod theme;
mod ui;

use anyhow::Result;
use clap::Parser;
use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;
use std::io;
use std::sync::Arc;

use shell::ShellManager;

#[derive(Parser, Debug)]
#[command(name = "wta", about = "Windows Terminal Agent — ACP TUI client / MCP tool server")]
struct Cli {
    /// Initial prompt to send to the agent (ACP mode only)
    #[arg(value_name = "PROMPT")]
    prompt: Option<String>,

    /// Agent CLI command (e.g. "copilot --acp --stdio")
    #[arg(long, default_value = "copilot --acp --stdio")]
    agent: String,

    /// Run as MCP server (headless, no TUI)
    #[arg(long, group = "mode")]
    mcp: bool,

    /// Run as ACP client with TUI (default)
    #[arg(long, group = "mode")]
    acp: bool,

    /// Test pipe connection to Windows Terminal (connect, authenticate, list_windows)
    #[arg(long, group = "mode")]
    test_pipe: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let shell_mgr = Arc::new(ShellManager::new());

    if cli.test_pipe {
        return run_test_pipe().await;
    } else if cli.mcp {
        // Headless MCP server mode — no TUI
        protocol::mcp::server::run_mcp_server(shell_mgr).await
    } else {
        // ACP TUI client mode (default)
        run_acp_tui_mode(cli, shell_mgr).await
    }
}

async fn run_acp_tui_mode(cli: Cli, shell_mgr: Arc<ShellManager>) -> Result<()> {
    // Init terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Run the app
    let result = run_acp_app(&mut terminal, cli, shell_mgr).await;

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if let Err(e) = result {
        eprintln!("Error: {e:?}");
        std::process::exit(1);
    }
    Ok(())
}

async fn run_test_pipe() -> Result<()> {
    use shell::wt_channel::{PipeChannel, WtChannel};

    println!("Connecting to Windows Terminal pipe...");
    let channel: PipeChannel = PipeChannel::connect().await?;
    println!("Connected and authenticated!\n");

    let result: serde_json::Value = channel
        .request("list_windows", serde_json::json!({}))
        .await?;
    println!("list_windows:");
    println!("{}\n", serde_json::to_string_pretty(&result)?);

    let result: serde_json::Value = channel
        .request("get_capabilities", serde_json::json!({}))
        .await?;
    println!("get_capabilities:");
    println!("{}", serde_json::to_string_pretty(&result)?);

    Ok(())
}

async fn run_acp_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    cli: Cli,
    shell_mgr: Arc<ShellManager>,
) -> Result<()> {
    let local_set = tokio::task::LocalSet::new();
    local_set
        .run_until(async move {
            let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
            let (prompt_tx, prompt_rx) = tokio::sync::mpsc::unbounded_channel();

            // Start crossterm event reader
            let evt_tx = event_tx.clone();
            tokio::task::spawn_local(event::read_crossterm_events(evt_tx));

            // Start ACP client
            let acp_event_tx = event_tx.clone();
            tokio::task::spawn_local(protocol::acp::client::run_acp_client(
                cli.agent.clone(),
                cli.prompt.clone(),
                acp_event_tx,
                prompt_rx,
                shell_mgr,
            ));

            // Run main event loop
            let mut app_state = app::App::new(prompt_tx);
            app_state.run(terminal, event_rx).await
        })
        .await
}
