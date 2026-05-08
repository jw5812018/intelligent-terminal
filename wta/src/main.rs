mod agent_registry;
mod agent_sessions;
mod agent_hooks_installer;
mod history_loader;
mod app;
mod coordinator;
mod event;
mod logging;
mod osc52;
mod preflight;
mod protocol;
mod runtime_paths;
mod shared_host;
mod shell;
mod theme;
mod ui;
mod ui_trace;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    style::Print,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;
use serde_json::json;
use std::io;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use shell::wt_channel::{PipeChannel, WtChannel};
use shell::ShellManager;

// ─── CLI Definition ─────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "wta",
    about = "Windows Terminal Agent — ACP TUI client / tmux-like CLI"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Initial prompt to send to the agent (ACP mode only)
    #[arg(value_name = "PROMPT")]
    prompt: Option<String>,

    /// Agent CLI command (e.g. "copilot --acp --stdio")
    #[arg(long, default_value = agent_registry::DEFAULT_ACP_COMMAND)]
    agent: String,

    /// Delegate agent CLI command (e.g. "codex")
    #[arg(long)]
    delegate_agent: Option<String>,

    /// Model override for the delegate agent
    #[arg(long)]
    delegate_model: Option<String>,

    /// Disable auto-fix on command failure
    #[arg(long)]
    no_autofix: bool,

    // Legacy flags (hidden, backward compat)
    #[arg(long, hide = true)]
    info: bool,
    #[arg(long, hide = true)]
    test_pipe: bool,

    /// Output raw JSON instead of human-readable format
    #[arg(long, global = true)]
    json: bool,

    /// Windows Terminal pipe name (overrides VT discovery and WT_PIPE_NAME env var)
    #[arg(long, global = true)]
    pipe_name: Option<String>,

    /// Windows Terminal auth token (overrides WT_MCP_TOKEN env var, use with --pipe-name)
    #[arg(long, global = true)]
    pipe_token: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Show Windows Terminal protocol connection info
    Info,

    /// Test pipe connection to Windows Terminal
    TestPipe,

    /// (Re-)install the wt-agent-hooks bridge into Claude Code and Copilot CLI.
    ///
    /// Runs the same idempotent install routine that fires automatically on
    /// every wta startup. Useful as a repair tool when a CLI's settings.json
    /// has lost the `hooks` block or the plugin registration entry, or when
    /// upgrading from an older wta build that wrote a stale path.
    ///
    /// Exits 0 even if a CLI is not installed — only writes for present
    /// `~/.claude/` and `~/.copilot/` directories.
    ///
    /// **Hidden alias** for `wta hooks install`. Kept for backward
    /// compatibility with the Settings UI's "Install hooks" button which
    /// shells out to `wta install-hooks`. Prefer `wta hooks install` in
    /// new code.
    #[command(hide = true)]
    InstallHooks,

    /// Inspect, install, or remove the wt-agent-hooks bridge.
    ///
    /// Source of truth for both the Settings UI ("Install hooks" button)
    /// and `Verify-AgentHooks.ps1`. `wta hooks status --json` returns a
    /// per-CLI install report; `wta hooks uninstall` reverses what
    /// `wta hooks install` produces.
    Hooks {
        #[command(subcommand)]
        action: HooksAction,
    },

    /// List all Windows Terminal windows
    #[command(alias = "lsw")]
    ListWindows,

    /// List tabs in a window
    #[command(alias = "lst")]
    ListTabs {
        /// Window ID (defaults to first window)
        #[arg(short = 'w', long)]
        window_id: Option<String>,
    },

    /// List panes in a tab
    #[command(alias = "lsp")]
    ListPanes {
        /// Tab ID (defaults to active tab)
        #[arg(short = 't', long)]
        tab_id: Option<String>,

        /// Window ID (used with tab_id)
        #[arg(short = 'w', long)]
        window_id: Option<String>,
    },

    /// Create a new tab
    #[command(alias = "neww")]
    NewTab {
        /// Command to run in the new tab
        #[arg(short = 'c', long)]
        command: Option<String>,

        /// Working directory
        #[arg(short = 'd', long)]
        cwd: Option<String>,

        /// Tab title
        #[arg(short = 'n', long)]
        title: Option<String>,
    },

    /// Split the current pane
    #[command(alias = "splitw")]
    SplitPane {
        /// Target pane ID
        #[arg(short = 't', long)]
        target: Option<String>,

        /// Split horizontally (panes side by side)
        // Note: no short alias — `-h` is reserved by clap's auto-generated
        // `--help` flag and would panic clap's debug_asserts at startup.
        #[arg(long)]
        horizontal: bool,

        /// Split vertically (panes stacked)
        #[arg(short = 'v', long)]
        vertical: bool,

        /// Size as fraction (0.0-1.0)
        #[arg(short = 's', long)]
        size: Option<f64>,

        /// Command to run in the new pane
        #[arg(short = 'c', long)]
        command: Option<String>,
    },

    /// Send keys to a pane (like tmux send-keys)
    #[command(alias = "send")]
    SendKeys {
        /// Target pane ID (defaults to active pane)
        #[arg(short = 't', long)]
        target: Option<String>,

        /// Keys to send (supports Enter, Space, C-c, Escape, Tab, BSpace, C-{letter})
        #[arg(required = true, trailing_var_arg = true)]
        keys: Vec<String>,
    },

    /// Capture pane output (like tmux capture-pane -p)
    #[command(alias = "capturep")]
    CapturePane {
        /// Target pane ID (defaults to active pane)
        #[arg(short = 't', long)]
        target: Option<String>,

        /// Maximum lines to capture
        #[arg(short = 'l', long)]
        max_lines: Option<u32>,

        /// Only return the most recent completed shell prompt
        /// (command + output). Requires OSC 133 shell integration.
        #[arg(long)]
        last_prompt: bool,
    },

    /// Close/kill a pane
    #[command(alias = "killp")]
    KillPane {
        /// Target pane ID (defaults to active pane)
        #[arg(short = 't', long)]
        target: Option<String>,
    },

    /// Show the currently active pane
    ActivePane,

    /// Show process status of a pane
    PaneStatus {
        /// Target pane ID (defaults to active pane)
        #[arg(short = 't', long)]
        target: Option<String>,
    },

    /// Wait for a pane's process to exit (poll get_process_status)
    WaitFor {
        /// Target pane ID
        #[arg(short = 't', long)]
        target: String,

        /// Poll interval in milliseconds
        #[arg(long, default_value = "500")]
        interval: u64,

        /// Timeout in seconds (0 = wait forever)
        #[arg(long, default_value = "0")]
        timeout: u64,
    },

    /// Discover and print the Windows Terminal pipe name and token
    PipeId,

    /// Print shell commands to set WT_PIPE_NAME/WT_MCP_TOKEN environment variables
    #[command(alias = "setenv")]
    SetEnv {
        /// Shell syntax: bash (default), powershell, cmd
        #[arg(short = 's', long, default_value = "bash")]
        shell: String,
    },

    /// Listen for events from Windows Terminal (VT sequences, connection state changes)
    #[command(alias = "mon")]
    Listen {
        /// Filter by pane ID (show events from all panes if omitted)
        #[arg(short = 't', long)]
        target: Option<String>,
    },

    /// Diagnostic: dump historical sessions discovered by the on-disk loader.
    #[command(name = "debug-history")]
    DebugHistory,

    /// Diagnostic: drive the live UI by publishing a synthetic sequence
    /// of agent events through wtcli (`send-event` / `publish`) so an
    /// open Agent pane (F2) shows real state transitions over time.
    /// Run from any Windows Terminal pane (WT_COM_CLSID must be set).
    #[command(name = "debug-live")]
    DebugLive {
        /// Milliseconds to sleep between transitions.
        #[arg(long, default_value_t = 1500)]
        interval_ms: u64,

        /// CLI source label to use for the synthetic session ("copilot",
        /// "claude", or "gemini").
        #[arg(long, default_value = "copilot")]
        cli_source: String,
    },
    /// Pre-warm the shared host (called by Windows Terminal on startup)
    #[command(name = "ensure-host")]
    EnsureHost {
        /// Agent CLI command
        #[arg(long)]
        agent: Option<String>,

        /// Delegate agent CLI command
        #[arg(long)]
        delegate_agent: Option<String>,

        /// Model override for the delegate agent
        #[arg(long)]
        delegate_model: Option<String>,
    },

    /// Attach to a running shared host (lightweight agent pane TUI)
    #[command(name = "attach")]
    Attach {
        /// Override the host pipe name (auto-derived from WT pipe if omitted)
        #[arg(long)]
        host_pipe: Option<String>,

        /// Initial prompt to submit on attach
        #[arg(value_name = "PROMPT")]
        prompt: Option<String>,
    },

    /// Delegate a prompt to a new tab with a configured agent (fire-and-forget)
    Delegate {
        /// The prompt to send to the delegate agent
        #[arg(value_name = "PROMPT")]
        prompt: String,

        /// Agent CLI command (used to derive delegate agent commandline)
        #[arg(long, default_value = agent_registry::DEFAULT_ACP_COMMAND)]
        agent: String,

        /// Delegate agent CLI command (e.g. "codex")
        #[arg(long)]
        delegate_agent: Option<String>,

        /// Model override for the delegate agent
        #[arg(long)]
        delegate_model: Option<String>,

        /// Working directory for the delegate agent tab
        #[arg(long)]
        cwd: Option<String>,
    },

    /// Show a quick-pick dialog in Windows Terminal and print the user's selection
    QuickPick {
        /// Choices to present (1 or more, all positional args)
        #[arg(required = true)]
        choices: Vec<String>,

        /// Title/question shown above the choices
        #[arg(long, default_value = "Select an option")]
        title: String,

        /// Allow freeform text input in addition to choices
        #[arg(long)]
        free_input: bool,
    },
}

/// Subcommands for `wta hooks`. See `agent_hooks_installer` for what
/// each action does.
#[derive(Subcommand, Debug)]
enum HooksAction {
    /// (Re-)install the wt-agent-hooks bridge for every supported CLI.
    Install,

    /// Print per-CLI install state. Returns JSON with `--json` (suitable
    /// for the Settings UI / Verify-AgentHooks.ps1), or a human-readable
    /// table by default.
    Status,

    /// Uninstall the bridge for one or all CLIs. Best-effort: missing
    /// CLIs are skipped at info level. With `--json` returns a structured
    /// per-CLI result report.
    Uninstall {
        /// Which CLI(s) to uninstall for. Default: `all`.
        #[arg(long, value_enum, default_value_t = HooksCliFilter::All)]
        cli: HooksCliFilter,
    },
}

/// `--cli` filter for `wta hooks uninstall`.
#[derive(Copy, Clone, Debug, clap::ValueEnum)]
enum HooksCliFilter {
    All,
    Copilot,
    Claude,
    Gemini,
}

impl HooksCliFilter {
    fn into_scope(self) -> agent_hooks_installer::CliScope {
        use agent_hooks_installer::{CliKind, CliScope};
        match self {
            HooksCliFilter::All => CliScope::All,
            HooksCliFilter::Copilot => CliScope::One(CliKind::Copilot),
            HooksCliFilter::Claude => CliScope::One(CliKind::Claude),
            HooksCliFilter::Gemini => CliScope::One(CliKind::Gemini),
        }
    }
}

// ─── Entry Point ────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Extract global pipe overrides for all code paths
    let pipe_override = PipeOverride {
        pipe_name: cli.pipe_name.clone(),
        pipe_token: cli.pipe_token.clone(),
    };

    // Legacy flags first (backward compat)
    if cli.test_pipe {
        return run_test_pipe(&pipe_override).await;
    }
    if cli.info {
        return run_info_mode(&pipe_override).await;
    }
    let json_mode = cli.json;

    match cli.command {
        // Subcommand aliases for legacy modes
        Some(Command::Info) => run_info_mode(&pipe_override).await,
        Some(Command::TestPipe) => run_test_pipe(&pipe_override).await,

        // Run the hooks installer manually. Useful as a repair tool: the
        // same routine fires automatically on every wta startup, but if a
        // CLI's settings.json has drifted (e.g. user removed our entry by
        // hand, or upgraded from an older wta) running this subcommand
        // resets it without launching the full agent pane.
        Some(Command::InstallHooks) => run_hooks_install(),

        // New `wta hooks <action>` group. Source of truth for the
        // Settings UI / Verify-AgentHooks.ps1.
        Some(Command::Hooks { action }) => match action {
            HooksAction::Install => run_hooks_install(),
            HooksAction::Status => run_hooks_status(json_mode),
            HooksAction::Uninstall { cli } => run_hooks_uninstall(cli, json_mode),
        },

        // ── List commands ──
        Some(Command::ListWindows) => {
            let result = wt_call(&pipe_override, "list_windows", json!({})).await?;
            print_output(&result, json_mode, format_windows_human);
            Ok(())
        }
        Some(Command::ListTabs { window_id }) => {
            let channel = connect_channel(&pipe_override).await?;
            let wid = match window_id {
                Some(id) => id,
                None => get_first_window_id(&channel).await?,
            };
            let result = channel
                .request("list_tabs", json!({ "window_id": wid }))
                .await?;
            print_output(&result, json_mode, format_tabs_human);
            Ok(())
        }
        Some(Command::ListPanes {
            tab_id,
            window_id,
        }) => {
            let channel = connect_channel(&pipe_override).await?;
            let tid = match tab_id {
                Some(id) => id,
                None => {
                    let wid = match window_id {
                        Some(id) => id,
                        None => get_first_window_id(&channel).await?,
                    };
                    get_first_tab_id(&channel, &wid).await?
                }
            };
            let result = channel
                .request("list_panes", json!({ "tab_id": tid }))
                .await?;
            print_output(&result, json_mode, format_panes_human);
            Ok(())
        }

        // ── Create/split ──
        Some(Command::NewTab {
            command,
            cwd,
            title,
        }) => {
            let mut params = json!({});
            if let Some(c) = command {
                params["command"] = json!(c);
            }
            if let Some(d) = cwd {
                params["cwd"] = json!(d);
            }
            if let Some(t) = title {
                params["title"] = json!(t);
            }
            let result = wt_call(&pipe_override, "create_tab", params).await?;
            print_output(&result, json_mode, format_created_tab);
            Ok(())
        }
        Some(Command::SplitPane {
            target,
            horizontal,
            vertical,
            size,
            command,
        }) => {
            let channel = connect_channel(&pipe_override).await?;
            let pane_id = resolve_pane_id(&channel, &target).await?;
            let split_dir = if horizontal {
                "horizontal"
            } else if vertical {
                "vertical"
            } else {
                "automatic"
            };
            let mut params = json!({
                "pane_id": pane_id,
                "direction": split_dir,
            });
            if let Some(s) = size {
                params["size"] = json!(s);
            }
            if let Some(c) = command {
                params["command"] = json!(c);
            }
            let result = channel.request("split_pane", params).await?;
            print_output(&result, json_mode, format_created_pane);
            Ok(())
        }

        // ── Send keys ──
        Some(Command::SendKeys { target, keys }) => {
            let channel = connect_channel(&pipe_override).await?;
            let pane_id = resolve_pane_id(&channel, &target).await?;
            let text = translate_keys(&keys);
            channel
                .request("send_input", json!({ "pane_id": pane_id, "text": text }))
                .await?;
            Ok(())
        }

        // ── Capture pane ──
        Some(Command::CapturePane { target, max_lines, last_prompt }) => {
            let channel = connect_channel(&pipe_override).await?;
            let pane_id = resolve_pane_id(&channel, &target).await?;
            let mut params = json!({ "pane_id": pane_id });
            if let Some(n) = max_lines {
                params["max_lines"] = json!(n);
            }
            if last_prompt {
                params["source"] = json!("last_prompt");
            }
            let result = channel.request("read_pane_output", params).await?;
            if json_mode {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else if let Some(output) = result.get("content").and_then(|v| v.as_str()) {
                print!("{}", output);
            }
            Ok(())
        }

        // ── Kill pane ──
        Some(Command::KillPane { target }) => {
            let channel = connect_channel(&pipe_override).await?;
            let pane_id = resolve_pane_id(&channel, &target).await?;
            channel
                .request("close_pane", json!({ "pane_id": pane_id }))
                .await?;
            if !json_mode {
                println!("Pane {} closed.", pane_id);
            }
            Ok(())
        }

        // ── Active pane ──
        Some(Command::ActivePane) => {
            let result = wt_call(&pipe_override, "get_active_pane", json!({})).await?;
            print_output(&result, json_mode, format_active_pane);
            Ok(())
        }

        // ── Pane status ──
        Some(Command::PaneStatus { target }) => {
            let channel = connect_channel(&pipe_override).await?;
            let pane_id = resolve_pane_id(&channel, &target).await?;
            let result = channel
                .request("get_process_status", json!({ "pane_id": pane_id }))
                .await?;
            print_output(&result, json_mode, format_pane_status);
            Ok(())
        }

        // ── Wait for ──
        Some(Command::WaitFor {
            target,
            interval,
            timeout,
        }) => {
            let channel = connect_channel(&pipe_override).await?;
            let start = std::time::Instant::now();
            loop {
                let result = channel
                    .request(
                        "get_process_status",
                        json!({ "pane_id": target }),
                    )
                    .await?;

                let is_running = result
                    .get("state")
                    .and_then(|v| v.as_str())
                    .map(|s| s == "running")
                    .unwrap_or(false);

                if !is_running {
                    print_output(&result, json_mode, format_pane_status);
                    return Ok(());
                }

                if timeout > 0 && start.elapsed().as_secs() >= timeout {
                    bail!("Timeout after {}s waiting for pane {} to exit", timeout, target);
                }

                tokio::time::sleep(std::time::Duration::from_millis(interval)).await;
            }
        }

        // ── Pipe discovery ──
        Some(Command::PipeId) => {
            run_pipe_id(&pipe_override, json_mode)
        }

        // ── Set environment variables ──
        Some(Command::SetEnv { shell }) => {
            run_set_env(&pipe_override, &shell)
        }

        // ── Ensure host — no-op (centralized architecture: no separate host process) ──
        Some(Command::EnsureHost { .. }) => Ok(()),

        // ── Attach — no-op (centralized architecture: use default TUI mode instead) ──
        Some(Command::Attach { .. }) => Ok(()),

        // ── Delegate prompt to new tab agent ──
        Some(Command::Delegate {
            prompt,
            agent,
            delegate_agent,
            delegate_model,
            cwd,
        }) => {
            run_delegate(&pipe_override, &prompt, &agent, delegate_agent.as_deref(), delegate_model.as_deref(), cwd.as_deref()).await
        }

        // ── Quick pick ──
        Some(Command::QuickPick {
            title,
            choices,
            free_input,
        }) => {
            let channel = connect_channel(&pipe_override).await?;
            let choices_json: Vec<serde_json::Value> =
                choices.iter().map(|c| serde_json::Value::String(c.clone())).collect();
            let result = channel
                .request(
                    "quick_pick",
                    json!({
                        "title": title,
                        "choices": choices_json,
                        "allow_free_input": free_input,
                    }),
                )
                .await?;
            let cancelled = result.get("cancelled").and_then(|v| v.as_bool()).unwrap_or(false);
            if cancelled {
                std::process::exit(1);
            }
            if let Some(selected) = result.get("selected").and_then(|v| v.as_str()) {
                println!("{}", selected);
            }
            Ok(())
        }

        // ── Listen for events ──
        Some(Command::Listen { target }) => {
            run_listen(&pipe_override, target.as_deref()).await
        }

        // ── Diagnostic: dump historical sessions discovered on disk ──
        Some(Command::DebugHistory) => {
            let sessions = history_loader::load_all();
            println!("Loaded {} historical session(s):", sessions.len());
            for s in &sessions {
                let key_short = if s.key.len() > 36 { &s.key[..36] } else { &s.key };
                println!(
                    "  [{:>10?}] {:<36} cwd={:<40} | {}",
                    s.cli_source,
                    key_short,
                    s.cwd.display().to_string(),
                    s.title.chars().take(60).collect::<String>(),
                );
            }
            Ok(())
        }

        // ── Diagnostic: drive live UI by publishing synthetic agent events ──
        Some(Command::DebugLive { interval_ms, cli_source }) => {
            run_debug_live(interval_ms, &cli_source);
            Ok(())
        }

        // ── No subcommand = ACP TUI mode (default) ──
        None => run_default_tui(cli, pipe_override).await,
    }
}

// ─── Pipe override (CLI --pipe-name / --pipe-token) ─────────────────────────

#[derive(Debug, Clone)]
struct PipeOverride {
    pipe_name: Option<String>,
    pipe_token: Option<String>,
}

/// Resolve pipe connection info. Priority: CLI args > VT discovery > env vars.
fn resolve_pipe_info(po: &PipeOverride) -> Option<shell::wt_channel::ConnectionInfo> {
    use shell::wt_channel::{ConnectionInfo, DiscoverySource, discover_connection_info};

    // 1. CLI override — highest priority
    if let Some(ref name) = po.pipe_name {
        return Some(ConnectionInfo {
            pipe_name: name.clone(),
            token: po.pipe_token.clone().unwrap_or_default(),
            source: DiscoverySource::EnvVar, // reuse; semantically "explicit"
        });
    }

    // 2. VT discovery + env var fallback
    discover_connection_info()
}

// ─── Helper: connect to WT pipe (no debug channel, no ShellManager) ─────────

async fn connect_channel(po: &PipeOverride) -> Result<PipeChannel> {
    if let Some(info) = resolve_pipe_info(po) {
        return PipeChannel::connect_with(&info.pipe_name, &info.token).await;
    }
    bail!("Cannot find Windows Terminal pipe. Use --pipe-name or set WT_PIPE_NAME.");
}

/// Single-shot: connect + call + return JSON
async fn wt_call(po: &PipeOverride, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
    let channel = connect_channel(po).await?;
    channel.request(method, params).await
}

/// Resolve -t target: Some(id) → use it, None → get_active_pane fallback
async fn resolve_pane_id(channel: &PipeChannel, target: &Option<String>) -> Result<String> {
    match target {
        Some(id) => Ok(id.clone()),
        None => {
            let result = channel.request("get_active_pane", json!({})).await?;
            let pane_id = result
                .get("pane_id")
                .and_then(|v| match v {
                    serde_json::Value::String(s) => Some(s.clone()),
                    serde_json::Value::Number(n) => Some(n.to_string()),
                    _ => None,
                })
                .ok_or_else(|| anyhow::anyhow!("No active pane found. Use -t to specify a pane ID."))?;
            Ok(pane_id)
        }
    }
}

/// Get the first window ID from list_windows.
async fn get_first_window_id(channel: &PipeChannel) -> Result<String> {
    let result = channel.request("list_windows", json!({})).await?;
    result
        .get("windows")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|w| w.get("window_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("No windows found"))
}

/// Get the first tab ID from a window.
async fn get_first_tab_id(channel: &PipeChannel, window_id: &str) -> Result<String> {
    let result = channel
        .request("list_tabs", json!({ "window_id": window_id }))
        .await?;
    result
        .get("tabs")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|t| match t.get("tab_id") {
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            Some(serde_json::Value::Number(n)) => Some(n.to_string()),
            _ => None,
        })
        .ok_or_else(|| anyhow::anyhow!("No tabs found in window {}", window_id))
}

/// Translate tmux key names to actual characters.
///
/// Handles: Enter, Space, Escape, Tab, BSpace, C-c, C-d, C-{letter}
/// Bare strings are passed through as-is (so "echo hello" Enter becomes "echo hello\r").
fn translate_keys(keys: &[String]) -> String {
    let mut out = String::new();
    for key in keys {
        match key.as_str() {
            "Enter" | "CR" => out.push('\r'),
            "Space" => out.push(' '),
            "Escape" | "Esc" => out.push('\x1b'),
            "Tab" => out.push('\t'),
            "BSpace" | "Backspace" => out.push('\x08'),
            "C-c" => out.push('\x03'),
            "C-d" => out.push('\x04'),
            "C-z" => out.push('\x1a'),
            "C-l" => out.push('\x0c'),
            "C-a" => out.push('\x01'),
            "C-e" => out.push('\x05'),
            "C-k" => out.push('\x0b'),
            "C-u" => out.push('\x15'),
            "C-w" => out.push('\x17'),
            other => {
                // Generic C-{letter} pattern
                if other.len() == 3
                    && other.starts_with("C-")
                    && other.as_bytes()[2].is_ascii_alphabetic()
                {
                    let letter = other.as_bytes()[2].to_ascii_lowercase();
                    out.push((letter & 0x1f) as char);
                } else {
                    out.push_str(other);
                }
            }
        }
    }
    out
}

// ─── Output helpers ─────────────────────────────────────────────────────────

fn print_output(val: &serde_json::Value, json_mode: bool, formatter: fn(&serde_json::Value)) {
    if json_mode {
        println!(
            "{}",
            serde_json::to_string_pretty(val).unwrap_or_else(|_| val.to_string())
        );
    } else {
        formatter(val);
    }
}

// ─── `wta hooks <action>` handlers ──────────────────────────────────────────

fn run_hooks_install() -> Result<()> {
    // Initialize logging so the install attempt is observable in
    // %LOCALAPPDATA%\IntelligentTerminal\logs\wta-install-hooks.log.
    // Without this, all tracing calls inside the installer (idempotency
    // hits, marketplace-add failures, etc.) go to /dev/null and nobody
    // can diagnose what happened.
    let _guard = logging::init("install-hooks");
    agent_hooks_installer::ensure_installed();
    println!(
        "wt-agent-hooks install attempted (idempotent). \
         Run `wta hooks status` to inspect the result. \
         Trace log: %LOCALAPPDATA%\\IntelligentTerminal\\logs\\wta-install-hooks.log"
    );
    Ok(())
}

fn run_hooks_status(json_mode: bool) -> Result<()> {
    let report = agent_hooks_installer::status();
    if json_mode {
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .unwrap_or_else(|_| serde_json::to_string(&report).unwrap_or_default())
        );
    } else {
        format_hooks_status_human(&report);
    }
    Ok(())
}

fn run_hooks_uninstall(cli: HooksCliFilter, json_mode: bool) -> Result<()> {
    let report = agent_hooks_installer::uninstall(cli.into_scope());
    if json_mode {
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .unwrap_or_else(|_| serde_json::to_string(&report).unwrap_or_default())
        );
    } else {
        format_hooks_uninstall_human(&report);
    }
    Ok(())
}

fn format_hooks_status_human(r: &agent_hooks_installer::StatusReport) {
    println!(
        "bundle source: {}{}",
        r.bundle_source.kind,
        r.bundle_source
            .path
            .as_deref()
            .map(|p| format!(" ({})", p))
            .unwrap_or_default(),
    );
    println!();
    for c in &r.clis {
        let summary = if !c.binary_on_path {
            "✗ CLI not on PATH".to_string()
        } else if c.plugin_installed && c.plugin_enabled && c.marketplace_path_valid {
            "✓ installed".to_string()
        } else if c.plugin_installed && !c.marketplace_path_valid {
            // #25: silently-broken state — plugin reports installed but
            // the registered marketplace path is gone.
            "⚠ marketplace path stale".to_string()
        } else if c.plugin_installed {
            "⚠ installed but disabled".to_string()
        } else {
            "✗ not installed".to_string()
        };
        let detail = format!(
            "marketplace={}, path_valid={}, plugin={}, enabled={}{}",
            yn(c.marketplace_registered),
            yn(c.marketplace_path_valid),
            yn(c.plugin_installed),
            yn(c.plugin_enabled),
            c.detection_fallback
                .map(|m| format!(", detection={}", m))
                .unwrap_or_default(),
        );
        println!("  {:<10} {:<28}  ({})", c.name, summary, detail);
        if let Some(p) = c.marketplace_path.as_deref() {
            println!("    path: {}", p);
        }
    }
}

fn format_hooks_uninstall_human(r: &agent_hooks_installer::UninstallReport) {
    for c in &r.clis {
        let summary = if !c.attempted {
            "skipped (CLI not on PATH)".to_string()
        } else {
            let plugin = c
                .plugin_uninstalled
                .map(|b| if b { "ok" } else { "failed" })
                .unwrap_or("-");
            let mkt = c
                .marketplace_removed
                .map(|b| if b { "ok" } else { "failed" })
                .unwrap_or("-");
            format!(
                "plugin={} marketplace={} staging={}",
                plugin,
                mkt,
                if c.staging_dir_removed { "ok" } else { "failed" },
            )
        };
        println!("  {:<10} {}", c.name, summary);
        for m in &c.messages {
            println!("    · {}", m);
        }
    }
}

fn yn(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}


// ─── debug-live: drive the real UI via wtcli ────────────────────────────────
//
// Pure-state-machine validation lives in unit tests (see agent_sessions.rs).
// This subcommand publishes a representative sequence of agent events
// through the same channels real hooks/connection lifecycle use, so an
// open Agent pane (F2) exhibits real state-color transitions over time:
//   Idle → Working → Attention → Error → Ended.
//
// Requirements:
//   * Run from a Windows Terminal pane (WT_COM_CLSID is set there).
//   * Have at least one Agent pane open in WT (the wta TUI in attach mode
//     is what consumes the events and renders the agents list).
fn run_debug_live(interval_ms: u64, cli_source: &str) {
    use std::thread::sleep;
    use std::time::Duration;

    if std::env::var_os("WT_COM_CLSID").is_none() {
        eprintln!("WT_COM_CLSID is not set in this environment.");
        eprintln!("Run `wta debug-live` from a Windows Terminal pane (any profile).");
        eprintln!("Open an Agent pane (F2) first so the wta TUI is listening for events.");
        return;
    }

    // Synthesize stable identifiers for this run. Using a non-zero pane
    // session id ensures the agent pane's wta won't filter the events out
    // as "from our own pane".
    let pid_lo = (std::process::id() & 0xffff_ffff) as u32;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let pane = format!(
        "{:08x}-debu-9b7a-0000-{:012x}",
        pid_lo,
        now_ms & 0xffff_ffff_ffff
    );
    let asid = format!("debug-live-{:08x}", pid_lo);
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| String::from("?"));

    println!("debug-live: publishing synthetic agent events.");
    println!("  cli_source      : {}", cli_source);
    println!("  pane_session_id : {}", pane);
    println!("  agent_session_id: {}", asid);
    println!("  interval        : {} ms", interval_ms);
    println!();
    println!("Open the Agent pane (F2) in WT to watch the row change state.");
    println!();

    // Sequence: target_status, transition_label, action.
    // For Error we publish a `connection_state: failed` raw event because no
    // hook event maps to ConnectionFailed (real Error originates from the
    // connection lifecycle in C++).
    let payload_started = serde_json::json!({ "cwd": cwd });
    let payload_tool_start = serde_json::json!({ "tool_name": "shell.run" });
    let payload_notify = serde_json::json!({ "message": "approve write to C:\\foo?" });
    let payload_stopped = serde_json::json!({ "reason": "demo finished" });

    let agent_steps: &[(&str, &str, &serde_json::Value)] = &[
        ("agent.session.started",  "Idle",      &payload_started),
        ("agent.tool.starting",    "Working",   &payload_tool_start),
        ("agent.notification",     "Attention", &payload_notify),
        // ToolCompleted does *not* clear Attention by design (Attention
        // requires explicit user resolution). Included to verify that.
        ("agent.tool.completed",   "Attention (still — needs user resolve)", &serde_json::Value::Null),
    ];

    for (idx, (event, target, payload)) in agent_steps.iter().enumerate() {
        println!("[{}/{}] send-event {} -> {}", idx + 1, agent_steps.len() + 2, event, target);
        let payload_obj: serde_json::Value = (*payload).clone();
        let wrapper = serde_json::json!({
            "cli_source":       cli_source,
            "agent_session_id": asid,
            "payload":          payload_obj,
        });
        let wrapper_str = serde_json::to_string(&wrapper).unwrap_or_else(|_| "{}".into());
        if let Err(e) = run_wtcli(&["send-event", "-e", event, "-p", &pane, &wrapper_str]) {
            eprintln!("  wtcli send-event failed: {}", e);
            return;
        }
        sleep(Duration::from_millis(interval_ms));
    }

    println!("[{}/{}] publish connection_state:failed -> Error", agent_steps.len() + 1, agent_steps.len() + 2);
    let conn_failed = serde_json::json!({
        "type":   "event",
        "method": "connection_state",
        "params": {
            "session_id": pane,
            "state":      "failed",
            "reason":     "simulated API 503",
        },
    });
    if let Err(e) = run_wtcli(&["publish", &conn_failed.to_string()]) {
        eprintln!("  wtcli publish failed: {}", e);
        return;
    }
    sleep(Duration::from_millis(interval_ms));

    println!("[{}/{}] send-event agent.session.stopped -> Ended", agent_steps.len() + 2, agent_steps.len() + 2);
    let stopped_wrapper = serde_json::json!({
        "cli_source":       cli_source,
        "agent_session_id": asid,
        "payload":          payload_stopped,
    });
    if let Err(e) = run_wtcli(&["send-event", "-e", "agent.session.stopped", "-p", &pane,
                                 &stopped_wrapper.to_string()]) {
        eprintln!("  wtcli send-event failed: {}", e);
        return;
    }

    println!();
    println!("Done. The synthetic session ({}-{}) should now show as Ended.", &asid[..asid.len().min(12)], cli_source);
    println!("Re-run with `--interval-ms <N>` to slow down or speed up.");
}

/// Resolve and run a wtcli subcommand, inheriting stderr so users see
/// COM errors. Searches a few candidate locations because `debug-live`
/// is most useful when run from the deployed package, but users may also
/// invoke a dev build directly.
fn run_wtcli(args: &[&str]) -> std::io::Result<()> {
    use std::process::{Command, Stdio};

    let exe = locate_wtcli().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "wtcli.exe not found. `debug-live` must run from the deployed \
             Windows Terminal package (where wta.exe and wtcli.exe sit \
             together). Open Windows Terminal from Start Menu, then run \
             `wta debug-live` in any pane. Running `wta\\target\\debug\\wta.exe` \
             directly will not work because dev builds lack package identity \
             and cannot reach the COM server.",
        )
    })?;

    let status = Command::new(&exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()?;

    if !status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("wtcli exited with status {}", status),
        ));
    }
    Ok(())
}

/// Hunt for `wtcli.exe` in plausible locations in priority order:
///   1. Co-located with the running `wta.exe` (this is the package layout)
///   2. Sibling MSBuild output dirs when running a cargo dev build, e.g.
///      `<repo>/wta/target/debug/wta.exe` ->
///      `<repo>/src/cascadia/CascadiaPackage/bin/x64/{Debug,Release}/AppX/wtcli.exe`
///   3. Less specific MSBuild layouts (`<root>/bin/x64/{Debug,Release}`)
///   4. PATH
fn locate_wtcli() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;

    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()));

    // 1. Co-located.
    if let Some(d) = exe_dir.as_ref() {
        let p = d.join("wtcli.exe");
        if p.exists() { return Some(p); }
    }

    // Walk up looking for a repo root, then probe known build-output layouts.
    if let Some(d) = exe_dir.as_ref() {
        let mut cur: Option<&std::path::Path> = Some(d.as_path());
        for _ in 0..6 {
            if let Some(c) = cur {
                for cfg in &["Debug", "Release"] {
                    // 2. Cascadia package AppX layout (real wtcli.exe lives here).
                    let appx: PathBuf = c
                        .join("src").join("cascadia").join("CascadiaPackage")
                        .join("bin").join("x64").join(cfg).join("AppX").join("wtcli.exe");
                    if appx.exists() {
                        return Some(appx);
                    }

                    // 3. Generic MSBuild output (older layouts / fallback).
                    let generic: PathBuf = c.join("bin").join("x64").join(cfg).join("wtcli.exe");
                    if generic.exists() {
                        return Some(generic);
                    }
                }
                cur = c.parent();
            }
        }
    }

    // 4. PATH lookup.
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let p = dir.join("wtcli.exe");
            if p.exists() { return Some(p); }
        }
    }

    None
}

fn format_windows_human(val: &serde_json::Value) {
    if let Some(windows) = val.get("windows").and_then(|v| v.as_array()) {
        if windows.is_empty() {
            println!("No windows found.");
            return;
        }
        println!("{:<12} {:<30} {}", "WINDOW_ID", "TITLE", "FOCUSED");
        for w in windows {
            let id = json_str_or_num(w, "window_id");
            let title = w
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("-");
            let focused = w
                .get("is_focused")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            println!(
                "{:<12} {:<30} {}",
                id,
                title,
                if focused { "*" } else { "" }
            );
        }
    } else {
        println!("{}", serde_json::to_string_pretty(val).unwrap_or_default());
    }
}

fn format_tabs_human(val: &serde_json::Value) {
    if let Some(tabs) = val.get("tabs").and_then(|v| v.as_array()) {
        if tabs.is_empty() {
            println!("No tabs found.");
            return;
        }
        println!("{:<10} {:<30} {}", "TAB_ID", "TITLE", "FOCUSED");
        for t in tabs {
            let id = json_str_or_num(t, "tab_id");
            let title = t.get("title").and_then(|v| v.as_str()).unwrap_or("-");
            let focused = t
                .get("is_active")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            println!(
                "{:<10} {:<30} {}",
                id,
                title,
                if focused { "*" } else { "" }
            );
        }
    } else {
        println!("{}", serde_json::to_string_pretty(val).unwrap_or_default());
    }
}

fn format_panes_human(val: &serde_json::Value) {
    if let Some(panes) = val.get("panes").and_then(|v| v.as_array()) {
        if panes.is_empty() {
            println!("No panes found.");
            return;
        }
        println!(
            "{:<10} {:<8} {:<8} {:<10} {}",
            "PANE_ID", "PID", "ACTIVE", "ROWS", "COLS"
        );
        for p in panes {
            let id = json_str_or_num(p, "pane_id");
            let pid = p
                .get("pid")
                .and_then(|v| v.as_u64())
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".to_string());
            let active = p
                .get("is_active")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let size = p.get("size");
            let rows = size
                .and_then(|s| s.get("rows"))
                .and_then(|v| v.as_u64())
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".to_string());
            let cols = size
                .and_then(|s| s.get("columns"))
                .and_then(|v| v.as_u64())
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".to_string());
            println!(
                "{:<10} {:<8} {:<8} {:<10} {}",
                id,
                pid,
                if active { "*" } else { "" },
                rows,
                cols
            );
        }
    } else {
        println!("{}", serde_json::to_string_pretty(val).unwrap_or_default());
    }
}

fn format_active_pane(val: &serde_json::Value) {
    let id = json_str_or_num(val, "pane_id");
    let tab = json_str_or_num(val, "tab_id");
    let win = json_str_or_num(val, "window_id");
    println!("Active pane: {} (tab: {}, window: {})", id, tab, win);
}

fn format_pane_status(val: &serde_json::Value) {
    let state = val
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let running = state == "running";
    let exit_code = val
        .get("exit_code")
        .and_then(|v| v.as_i64())
        .map(|n| n.to_string())
        .unwrap_or_else(|| "-".to_string());
    let pid = val
        .get("pid")
        .and_then(|v| v.as_u64())
        .map(|n| n.to_string())
        .unwrap_or_else(|| "-".to_string());
    if running {
        println!("Running (PID: {})", pid);
    } else {
        println!("Exited (code: {}, PID: {})", exit_code, pid);
    }
}

fn format_created_tab(val: &serde_json::Value) {
    let tab_id = json_str_or_num(val, "tab_id");
    let pane_id = json_str_or_num(val, "pane_id");
    println!("Created tab {} (pane {})", tab_id, pane_id);
}

fn format_created_pane(val: &serde_json::Value) {
    let pane_id = json_str_or_num(val, "pane_id");
    println!("Created pane {}", pane_id);
}

/// Extract a field that may be string or number from JSON.
fn json_str_or_num(val: &serde_json::Value, key: &str) -> String {
    match val.get(key) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        _ => "-".to_string(),
    }
}

// ─── pipe-id / set-env commands ─────────────────────────────────────────────

fn run_pipe_id(po: &PipeOverride, json_mode: bool) -> Result<()> {
    match resolve_pipe_info(po) {
        Some(info) => {
            if json_mode {
                let val = json!({
                    "pipe_name": info.pipe_name,
                    "token_set": !info.token.is_empty(),
                    "source": format!("{:?}", info.source),
                });
                println!("{}", serde_json::to_string_pretty(&val)?);
            } else {
                println!("{}", info.pipe_name);
            }
            Ok(())
        }
        None => {
            bail!("Cannot discover pipe. Use --pipe-name or set WT_PIPE_NAME, or run inside Windows Terminal.");
        }
    }
}

fn run_set_env(po: &PipeOverride, shell_type: &str) -> Result<()> {
    let info = resolve_pipe_info(po).ok_or_else(|| {
        anyhow::anyhow!("Cannot discover pipe. Use --pipe-name or set WT_PIPE_NAME, or run inside Windows Terminal.")
    })?;

    match shell_type {
        "bash" | "sh" | "zsh" => {
            println!("export WT_PIPE_NAME='{}'", info.pipe_name);
            if !info.token.is_empty() {
                println!("export WT_MCP_TOKEN='{}'", info.token);
            }
            eprintln!("# Run: eval \"$(wta set-env)\"");
        }
        "powershell" | "pwsh" | "ps" => {
            println!("$env:WT_PIPE_NAME = '{}'", info.pipe_name);
            if !info.token.is_empty() {
                println!("$env:WT_MCP_TOKEN = '{}'", info.token);
            }
            eprintln!("# Run: wta set-env -s powershell | Invoke-Expression");
        }
        "cmd" => {
            println!("set WT_PIPE_NAME={}", info.pipe_name);
            if !info.token.is_empty() {
                println!("set WT_MCP_TOKEN={}", info.token);
            }
            eprintln!("REM Run in a for /f loop or copy-paste");
        }
        "fish" => {
            println!("set -gx WT_PIPE_NAME '{}'", info.pipe_name);
            if !info.token.is_empty() {
                println!("set -gx WT_MCP_TOKEN '{}'", info.token);
            }
            eprintln!("# Run: wta set-env -s fish | source");
        }
        other => {
            bail!("Unknown shell type '{}'. Use: bash, powershell, cmd, fish", other);
        }
    }

    Ok(())
}

// ─── Listen mode ────────────────────────────────────────────────────────────

async fn run_listen(po: &PipeOverride, pane_filter: Option<&str>) -> Result<()> {
    let channel = connect_channel(po).await?;
    let arc_channel = std::sync::Arc::new(channel);

    // Subscribe to events and start the background reader.
    let mut event_rx = arc_channel.subscribe_events();
    arc_channel.start_reader().await;

    // Send any request to trigger lazy page event registration on the server.
    let _ = arc_channel.request("get_capabilities", json!({})).await;

    eprintln!("Connected. Listening for events... (Ctrl+C to stop)");
    if let Some(pane) = pane_filter {
        eprintln!("Filtering: session_id={}", pane);
    }

    while let Some(msg) = event_rx.recv().await {
        // Only print events, skip responses.
        if msg.get("type").and_then(|v| v.as_str()) != Some("event") {
            continue;
        }

        // Optional session_id filter.
        if let Some(filter) = pane_filter {
            let session_id = msg
                .get("params")
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str());
            if session_id != Some(filter) {
                continue;
            }
        }

        // Re-serialize to guarantee compact single-line JSON (safe for jq piping).
        println!("{}", serde_json::to_string(&msg).unwrap_or_default());
    }

    eprintln!("Event stream closed.");
    Ok(())
}

// ─── Delegate prompt to new tab agent ────────────────────────────────────────

async fn run_delegate(
    po: &PipeOverride,
    prompt: &str,
    agent_cmd: &str,
    delegate_agent_cmd: Option<&str>,
    delegate_model: Option<&str>,
    cwd: Option<&str>,
) -> Result<()> {
    let _guard = logging::init("delegate");
    tracing::info!(prompt, agent = agent_cmd, cwd, "run_delegate started");

    let (debug_tx, _) = tokio::sync::mpsc::unbounded_channel::<app::DebugMessage>();
    let channel = match connect_to_wt_pipe(po, debug_tx).await {
        Ok(ch) => { tracing::info!("pipe connected"); ch }
        Err(e) => { tracing::warn!(error = %e, "pipe FAILED"); return Err(e); }
    };
    let shell_mgr = ShellManager::new()
        .with_wt_channel(Arc::new(channel) as Arc<dyn shell::wt_channel::WtChannel>);

    match delegate_with_context(&shell_mgr, prompt, agent_cmd, delegate_agent_cmd, delegate_model, cwd).await {
        Ok(()) => { tracing::info!("delegate OK"); Ok(()) }
        Err(e) => { tracing::warn!(error = %e, "delegate FAILED"); Err(e) }
    }
}

/// Shared delegation logic: enrich the prompt with the active pane's recent
/// output (when available), build the delegate-agent commandline, and create a
/// new tab to launch it. WT's GetActivePane already resolves the agent pane to
/// the user's working pane, so a single query is enough.
async fn delegate_with_context(
    shell_mgr: &ShellManager,
    prompt: &str,
    agent_cmd: &str,
    delegate_agent_cmd: Option<&str>,
    delegate_model: Option<&str>,
    cwd: Option<&str>,
) -> Result<()> {
    let active = shell_mgr.wt_get_active_pane().await.ok();
    let active_pane_id = active
        .as_ref()
        .and_then(|v| v.get("pane_id"))
        .and_then(|v| match v {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            _ => None,
        });

    let pane_context = if let Some(ref pane_id) = active_pane_id {
        match shell_mgr.wt_read_pane_output(pane_id, Some(30)).await {
            Ok(value) => value
                .get("content")
                .and_then(|c| c.as_str())
                .map(|s| s.to_string()),
            Err(_) => None,
        }
    } else {
        None
    };

    let full_prompt = match (pane_context, active_pane_id) {
        (Some(context), Some(pane_id)) => format!(
            "{}\n\n## Terminal Context (pane {})\n```\n{}\n```",
            prompt, pane_id, context
        ),
        _ => prompt.to_string(),
    };

    let delegate_agents = crate::coordinator::default_delegate_agent_runtimes(
        delegate_agent_cmd,
        Some(agent_cmd),
        delegate_model,
    );
    let runtime = delegate_agents
        .first()
        .ok_or_else(|| anyhow::anyhow!("no delegate agent configured"))?;

    let commandline = crate::coordinator::build_delegate_commandline(runtime, &full_prompt)?;

    tracing::debug!(commandline, cwd, "delegate_with_context: launching");

    shell_mgr
        .wt_create_tab(Some(&commandline), cwd, None)
        .await?;

    Ok(())
}

// ─── Ensure host (background mode — no-op in centralized architecture) ──────

async fn run_ensure_host(
    po: &PipeOverride,
    agent_cmd: String,
    delegate_agent_cmd: Option<String>,
    delegate_model: Option<String>,
) -> Result<()> {
    let _guard = logging::init("ensure-host");
    tracing::info!("=== ensure-host starting ===");

    let local_set = tokio::task::LocalSet::new();
    local_set
        .run_until(async move {
            // Connect to the Windows Terminal protocol pipe.
            // Must be inside LocalSet because start_reader() uses spawn_local.
            let (debug_tx, _debug_rx) =
                tokio::sync::mpsc::unbounded_channel::<app::DebugMessage>();
            let mut shell_mgr = ShellManager::new();
            let wt_event_rx = match connect_to_wt_pipe(po, debug_tx).await {
                Ok(channel) => {
                    tracing::info!("Connected to WT pipe");
                    let event_rx = channel.subscribe_events();
                    let arc_channel = Arc::new(channel);
                    shell_mgr = shell_mgr.with_wt_channel(
                        arc_channel.clone() as Arc<dyn shell::wt_channel::WtChannel>,
                    );

                    tracing::info!("Starting pipe reader...");
                    arc_channel.start_reader().await;
                    tracing::info!("Pipe reader started, fetching capabilities...");
                    match arc_channel
                        .request("get_capabilities", serde_json::json!({}))
                        .await
                    {
                        Ok(v) => tracing::info!(result = %v, "get_capabilities OK"),
                        Err(e) => tracing::warn!(error = %e, "get_capabilities FAILED"),
                    }
                    Some(event_rx)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "No WT pipe");
                    None
                }
            };
            let shell_mgr = Arc::new(shell_mgr);

            // Compute shared host pipe name.  Use resolve_pipe_info so the
            // hash matches what attach clients compute (they also use it).
            let wt_info = resolve_pipe_info(&po);
            let host_pipe_name = shared_host::pipe_name_for(
                wt_info.as_ref(),
                Some(agent_cmd.as_str()),
                delegate_agent_cmd.as_deref(),
            );
            tracing::info!(pipe = %host_pipe_name, "Host pipe");

            // Autofix command channel. The host-side WT event listener pushes
            // Trigger on actionable failures and Execute when the user clicks
            // the bottom-bar icon / presses Ctrl+. without an attach TUI
            // running. run_host_server routes them into host_command_tx so
            // the existing state machine handles both cases.
            let (autofix_cmd_tx, autofix_cmd_rx) = tokio::sync::mpsc::unbounded_channel::<shared_host::HostAutofixCommand>();

            // Spawn background listener for WT events.
            if let Some(mut wt_rx) = wt_event_rx {
                let sm = Arc::clone(&shell_mgr);
                let agent_for_recs = agent_cmd.clone();
                let delegate_for_recs = delegate_agent_cmd.clone();
                let delegate_model_for_recs = delegate_model.clone();
                let host_autofix_tx = autofix_cmd_tx.clone();
                tokio::task::spawn_local(async move {
                    // Create a recommendation executor for delegation.
                    let (rec_tx, rec_rx) =
                        tokio::sync::mpsc::unbounded_channel();
                    let (evt_tx, _evt_rx) = tokio::sync::mpsc::unbounded_channel();
                    let delegate_agents =
                        crate::coordinator::default_delegate_agent_runtimes(
                            delegate_for_recs.as_deref(),
                            Some(agent_for_recs.as_str()),
                            delegate_model_for_recs.as_deref(),
                        );
                    let delegate_agent_id = delegate_agents
                        .first()
                        .map(|r| r.id.clone())
                        .unwrap_or_else(|| agent_registry::KNOWN_AGENTS[0].id.to_string());
                    tokio::spawn(crate::coordinator::run_recommendation_executor(
                        rec_rx,
                        evt_tx,
                        sm,
                        delegate_agents,
                    ));

                    while let Some(event_json) = wt_rx.recv().await {
                        let method = event_json
                            .get("method")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let session_id = event_json
                            .get("params")
                            .and_then(|p| p.get("session_id"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let params = event_json
                            .get("params")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);

                        if method == "agent_prompt" {
                            let prompt = params
                                .get("prompt")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            if !prompt.is_empty() {
                                tracing::info!(prompt = %prompt, "agent_prompt received");
                                let choice = crate::coordinator::RecommendationChoice {
                                    choice: 1,
                                    title: "Delegate to tab agent".into(),
                                    rationale: String::new(),
                                    actions: vec![
                                        crate::coordinator::RecommendedAction::OpenAndSend {
                                            target: crate::coordinator::OpenTarget::Tab,
                                            parent: None,
                                            input: prompt,
                                            agent: Some(delegate_agent_id.clone()),
                                            cwd: None,
                                            title: None,
                                            direction: None,
                                        },
                                    ],
                                };
                                let _ = rec_tx.send(crate::coordinator::ChoiceExecution {
                                    choice,
                                    insert_only: false,
                                });
                            }
                            continue;
                        }

                        // User pressed Ctrl+. or clicked the bottom-bar
                        // autofix icon — execute the armed recommendation.
                        if method == "autofix_execute" {
                            tracing::info!(session_id = %session_id, "host autofix_execute");
                            let _ = host_autofix_tx.send(
                                shared_host::HostAutofixCommand::Execute {
                                    pane_id: session_id.clone(),
                                },
                            );
                            continue;
                        }

                        // Classify any other event — if actionable (OSC 133;D
                        // non-zero exit, connection failures, etc.), fire the
                        // host-side autofix pipeline so the bottom bar goes
                        // from Idle → Pending → Armed even when no attach
                        // TUI (agent pane) is running.
                        let note = crate::app::classify_wt_event(&method, &session_id, &params);
                        if note.severity == crate::app::WtEventSeverity::Actionable
                            && method != "agent_prompt"
                        {
                            tracing::info!(session_id = %session_id, summary = %note.summary, "host autofix trigger");
                            let _ = host_autofix_tx.send(
                                shared_host::HostAutofixCommand::Trigger {
                                    pane_id: session_id.clone(),
                                    summary: note.summary.clone(),
                                },
                            );
                        }
                        // A successful command in an armed pane means the error was
                        // resolved without the fix. Dismiss the autofix state.
                        //
                        // For Suggested state specifically, ANY new prompt activity
                        // (osc:133;A — shell prompt rendered) in any pane also
                        // dismisses, because the user is moving on from the prior
                        // failure. The host's ClearAutofixForPane handler is the
                        // single arbitration point: it clears Suggested regardless
                        // of pane_id match (suggestions are global UI state),
                        // while keeping Armed/Pending strictly same-pane.
                        if method == "vt_sequence"
                            && note.severity == crate::app::WtEventSeverity::Informational
                        {
                            if let Some(seq) = params.get("sequence").and_then(|v| v.as_str()) {
                                let is_exit_zero = seq.strip_prefix("osc:133;")
                                    .and_then(|rest| rest.strip_prefix("D;"))
                                    .and_then(|code| code.trim().parse::<i32>().ok())
                                    .map(|c| c == 0)
                                    .unwrap_or(false);
                                let is_prompt_start = seq == "osc:133;A";
                                if is_exit_zero || is_prompt_start {
                                    tracing::info!(
                                        session_id = %session_id,
                                        seq = %seq,
                                        "host autofix clear on success/prompt-start"
                                    );
                                    let _ = host_autofix_tx.send(
                                        shared_host::HostAutofixCommand::ClearOnSuccess {
                                            pane_id: session_id.clone(),
                                        },
                                    );
                                }
                            }
                        }
                    }
                });
            }

            // Start the shared host server (ACP client + host service).
            tracing::info!("Starting shared host server...");
            match shared_host::run_host_server(
                host_pipe_name,
                agent_cmd,
                delegate_agent_cmd,
                shell_mgr,
                true, // wt_connected
                Some(autofix_cmd_rx),
            )
            .await
            {
                Ok(()) => tracing::info!("Shared host server exited normally"),
                Err(e) => tracing::warn!(error = %e, "Shared host server FAILED"),
            }
            Ok(())
        })
        .await
}

// ─── Attach TUI mode (lightweight pane client) ─────────────────────────────

async fn run_attach_tui(
    po: PipeOverride,
    agent: String,
    delegate_agent: Option<String>,
    _delegate_model: Option<String>,
    no_autofix: bool,
    host_pipe_override: Option<String>,
    initial_prompt: Option<String>,
) -> Result<()> {
    let _guard = logging::init("attach");
    tracing::info!("=== run_attach_tui started ===");

    let (debug_tx, debug_rx) = tokio::sync::mpsc::unbounded_channel::<app::DebugMessage>();

    // Connect to WT pipe (for pane identity discovery and event forwarding).
    let mut shell_mgr = ShellManager::new();
    let mut wt_event_rx = None;
    let mut wt_pipe_channel: Option<Arc<PipeChannel>> = None;
    let wt_connected = match connect_to_wt_pipe(&po, debug_tx.clone()).await {
        Ok(channel) => {
            tracing::info!("Connected to WT pipe OK");
            wt_event_rx = Some(channel.subscribe_events());
            let arc_channel = Arc::new(channel);
            wt_pipe_channel = Some(Arc::clone(&arc_channel));
            shell_mgr =
                shell_mgr.with_wt_channel(arc_channel as Arc<dyn shell::wt_channel::WtChannel>);
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, "NO WT pipe");
            false
        }
    };
    let shell_mgr = Arc::new(shell_mgr);

    // Discover our own pane identity.
    let pane_identity = if wt_connected {
        discover_pane_identity(&shell_mgr).await
    } else {
        None
    };
    tracing::info!(pane_identity = ?pane_identity, "pane_identity");

    // Compute the shared host pipe name (must match ensure-host).
    let host_pipe_name = if let Some(name) = host_pipe_override {
        name
    } else {
        let wt_info = resolve_pipe_info(&po);
        shared_host::pipe_name_for(
            wt_info.as_ref(),
            Some(agent.as_str()),
            delegate_agent.as_deref(),
        )
    };
    tracing::info!(host_pipe_name = %host_pipe_name, "host_pipe_name");

    // Trigger _ensurePageEventsRegistered on the WT server.
    if let Some(ref pipe_ch) = wt_pipe_channel {
        pipe_ch.start_reader().await;
        let _ = pipe_ch
            .request("get_capabilities", serde_json::json!({}))
            .await;
    }

    let autofix_enabled = !no_autofix;
    let log_agent_events = std::env::var("WTA_LOG_AGENT_EVENT")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);

    // ── Preflight: check agent CLI before connecting to shared host ──
    let preflight_result = preflight::check_agent(&agent).await;
    let start_in_setup = !preflight_result.all_passed();

    // Set up the TUI.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    // Set pane background via OSC 11 so the terminal chrome pixels match the chat area.
    execute!(stdout, Print("\x1b]11;#0c0c0c\x07"))?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let local_set = tokio::task::LocalSet::new();
    let result = local_set
        .run_until(async {
            let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
            let (prompt_tx, prompt_rx) = tokio::sync::mpsc::unbounded_channel();
            let (recommendation_tx, recommendation_rx) = tokio::sync::mpsc::unbounded_channel();
            let (permission_tx, permission_rx) = tokio::sync::mpsc::unbounded_channel();
            let (dismiss_autofix_tx, dismiss_autofix_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
            let debug_capture_enabled = Arc::new(AtomicBool::new(false));

            // Crossterm event reader.
            let evt_tx = event_tx.clone();
            tokio::task::spawn_local(event::read_crossterm_events(evt_tx));

            // Debug message forwarder.
            let dbg_event_tx = event_tx.clone();
            let mut debug_rx = debug_rx;
            tokio::task::spawn_local(async move {
                while let Some(msg) = debug_rx.recv().await {
                    let _ = dbg_event_tx.send(app::AppEvent::DebugPipeMessage(msg));
                }
            });

            // WT event reader (forwards push events to TUI).
            if let Some(mut wt_rx) = wt_event_rx {
                let wt_event_tx = event_tx.clone();
                tokio::task::spawn_local(async move {
                    while let Some(event_json) = wt_rx.recv().await {
                        let method = event_json
                            .get("method")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        // Pane GUIDs arrive in two cases on the wire:
                        //   - hook bridge events  → lowercase (WT_SESSION env var)
                        //   - WT-native events    → uppercase (canonical Windows GUID)
                        // Normalise here so downstream code (registry,
                        // is_agent_pane lookup, osc:133;A demote, etc.) all
                        // compare in the same case.
                        let session_id = event_json
                            .get("params")
                            .and_then(|p| p.get("session_id"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_ascii_lowercase();
                        let params = event_json
                            .get("params")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);
                        let _ = wt_event_tx.send(app::AppEvent::WtEvent {
                            method,
                            session_id,
                            params,
                        });
                    }
                });
            }

            // Build PaneContext from discovered pane identity.
            let pane_context = shared_host::PaneContext {
                session_id: pane_identity.as_ref().map(|(p, _, _)| p.clone()),
                tab_id: pane_identity.as_ref().map(|(_, t, _)| t.clone()),
                window_id: pane_identity.as_ref().map(|(_, _, w)| w.clone()),
                cwd: std::env::var("WTA_SOURCE_CWD").ok().filter(|s| !s.is_empty()),
                source_session_id: std::env::var("WTA_SOURCE_SESSION_ID")
                    .ok()
                    .filter(|s| !s.is_empty()),
            };

            // Spawn the attach client (replaces run_acp_client in shared mode).
            // In attach mode, all prompts/recommendations/permissions are forwarded
            // to the shared host — no local ACP client or recommendation executor.
            if !start_in_setup {
                let attach_event_tx = event_tx.clone();
                tokio::task::spawn_local(shared_host::run_attach_client(
                    host_pipe_name.clone(),
                    attach_event_tx,
                    prompt_rx,
                    recommendation_rx,
                    permission_rx,
                    dismiss_autofix_rx,
                    pane_context.clone(),
                    initial_prompt.clone(),
                    debug_capture_enabled.clone(),
                ));
            }

            let (_ui_event_tx, ui_event_rx) = tokio::sync::mpsc::unbounded_channel();

            let mut app_state = app::App::new(
                prompt_tx,
                recommendation_tx,
                permission_tx,
                debug_capture_enabled.clone(),
                wt_connected,
                autofix_enabled,
                log_agent_events,
            );
            app_state.set_app_event_tx(event_tx.clone());
            let _ = dismiss_autofix_tx; // was used for shared_mode dismiss; no longer needed

            // If preflight failed, enter Setup mode with static guidance
            // (no retry — user must close and reopen the agent pane).
            if start_in_setup {
                let _ = event_tx.send(app::AppEvent::PreflightComplete(preflight_result.clone()));
            }

            if let Some((pane_id, tab_id, window_id)) = pane_identity {
                app_state.pane_session_id = Some(pane_id);
                app_state.tab_id = Some(tab_id);
                app_state.window_id = Some(window_id);
            }
            app_state.source_session_id =
                std::env::var("WTA_SOURCE_SESSION_ID").ok().filter(|s| !s.is_empty());
            app_state.source_cwd =
                std::env::var("WTA_SOURCE_CWD").ok().filter(|s| !s.is_empty());

            app_state.run(&mut terminal, event_rx, ui_event_rx).await
        })
        .await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), Print("\x1b]111\x07"), DisableMouseCapture, LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if let Err(e) = result {
        eprintln!("Error: {e:?}");
        std::process::exit(1);
    }
    Ok(())
}

// ─── Default ACP TUI mode ───────────────────────────────────────────────────

async fn run_default_tui(cli: Cli, po: PipeOverride) -> Result<()> {
    let _guard = logging::init("main");
    tracing::info!("=== run_default_tui started ===");

    // Debug channel for TUI debug panel (pipe traffic viewer)
    let (debug_tx, debug_rx) = tokio::sync::mpsc::unbounded_channel::<app::DebugMessage>();

    // Try to connect to the Windows Terminal pipe.
    let mut shell_mgr = ShellManager::new();
    let mut wt_event_rx = None;
    let mut wt_pipe_channel: Option<Arc<PipeChannel>> = None;
    let wt_connected = match connect_to_wt_pipe(&po, debug_tx.clone()).await {
        Ok(channel) => {
            tracing::info!("Connected to WT pipe OK — subscribing to events");
            // Subscribe to push events before wrapping in Arc.
            wt_event_rx = Some(channel.subscribe_events());
            let arc_channel = Arc::new(channel);
            wt_pipe_channel = Some(Arc::clone(&arc_channel));
            shell_mgr = shell_mgr.with_wt_channel(arc_channel as Arc<dyn shell::wt_channel::WtChannel>);
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, "NO WT pipe");
            false
        }
    };
    let shell_mgr = Arc::new(shell_mgr);

    // Try to discover our own pane identity by PID matching
    let pane_identity = if wt_connected {
        discover_pane_identity(&shell_mgr).await
    } else {
        None
    };

    run_acp_tui_mode(cli, shell_mgr, wt_connected, debug_rx, pane_identity, wt_event_rx, wt_pipe_channel).await
}

// ─── Existing functions (preserved) ─────────────────────────────────────────

/// Discover our own pane identity by matching our PID against WT's pane list.
async fn discover_pane_identity(shell_mgr: &ShellManager) -> Option<(String, String, String)> {
    let our_pid = std::process::id();

    let windows = shell_mgr.wt_list_windows().await.ok()?;
    let windows_arr = windows.get("windows")?.as_array()?;

    for win in windows_arr {
        let window_id = win.get("window_id")?.as_str()?;
        let tabs = shell_mgr.wt_list_tabs(window_id).await.ok()?;
        let tabs_arr = tabs.get("tabs")?.as_array()?;

        for tab in tabs_arr {
            let tab_id_str = match tab.get("tab_id") {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(serde_json::Value::Number(n)) => n.to_string(),
                _ => continue,
            };
            let panes = shell_mgr.wt_list_panes(&tab_id_str).await.ok()?;
            let panes_arr = panes.get("panes")?.as_array()?;

            for pane in panes_arr {
                if let Some(pid) = pane.get("pid").and_then(|v| v.as_u64()) {
                    if pid == our_pid as u64 {
                        let pane_id = match pane.get("pane_id") {
                            Some(serde_json::Value::String(s)) => s.clone(),
                            Some(serde_json::Value::Number(n)) => n.to_string(),
                            _ => continue,
                        };
                        return Some((pane_id, tab_id_str.clone(), window_id.to_string()));
                    }
                }
            }
        }
    }
    None
}

async fn run_acp_tui_mode(
    cli: Cli,
    shell_mgr: Arc<ShellManager>,
    wt_connected: bool,
    debug_rx: tokio::sync::mpsc::UnboundedReceiver<app::DebugMessage>,
    pane_identity: Option<(String, String, String)>,
    wt_event_rx: Option<tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>>,
    wt_pipe_channel: Option<Arc<PipeChannel>>,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    execute!(stdout, Print("\x1b]11;#0c0c0c\x07"))?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result =
        run_acp_app(&mut terminal, cli, shell_mgr, wt_connected, debug_rx, pane_identity, wt_event_rx, wt_pipe_channel).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), Print("\x1b]111\x07"), DisableMouseCapture, LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if let Err(e) = result {
        eprintln!("Error: {e:?}");
        std::process::exit(1);
    }
    Ok(())
}

async fn run_test_pipe(po: &PipeOverride) -> Result<()> {
    use shell::wt_channel::WtChannel;

    println!("Connecting to Windows Terminal pipe...");
    let channel = connect_channel(po).await?;
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

/// Try to connect to the WT pipe using CLI override, VT discovery, or env var fallback.
async fn connect_to_wt_pipe(
    po: &PipeOverride,
    debug_tx: tokio::sync::mpsc::UnboundedSender<app::DebugMessage>,
) -> Result<shell::wt_channel::PipeChannel> {
    use shell::wt_channel::PipeChannel;

    if let Some(info) = resolve_pipe_info(po) {
        eprintln!(
            "[wta] Discovered pipe via {:?}: {}",
            info.source, info.pipe_name
        );
        let channel = PipeChannel::connect_with(&info.pipe_name, &info.token).await?;
        return Ok(channel.with_debug_sender(debug_tx));
    }

    bail!("Cannot find Windows Terminal pipe. Use --pipe-name or set WT_PIPE_NAME.");
}

/// Show Windows Terminal protocol connection info and pane identity.
async fn run_info_mode(po: &PipeOverride) -> Result<()> {
    use shell::wt_channel::{DiscoverySource, WtChannel};

    println!("Windows Terminal Protocol Info");
    println!("========================================");

    let info = match resolve_pipe_info(po) {
        Some(info) => info,
        None => {
            println!("  Status: Not running inside Windows Terminal");
            println!("  (No VT response, WT_PIPE_NAME not set, no --pipe-name)");
            return Ok(());
        }
    };

    let source_str = match info.source {
        DiscoverySource::VtOsc => "VT OSC discovery",
        DiscoverySource::EnvVar => "WT_PIPE_NAME env var",
        DiscoverySource::ComClsid => "WT_COM_CLSID env var",
    };
    let token_display = if info.token.is_empty() {
        "(dev bypass)"
    } else {
        "(set)"
    };

    println!("  Pipe:   {}", info.pipe_name);
    println!("  Token:  {}", token_display);
    println!("  Source: {}", source_str);
    println!();

    let channel = match PipeChannel::connect_with(&info.pipe_name, &info.token).await {
        Ok(ch) => ch,
        Err(e) => {
            println!("  Connection failed: {}", e);
            return Ok(());
        }
    };

    let our_pid = std::process::id();
    let mut pane_info: Option<(String, String, String)> = None;
    let mut total_windows = 0u32;
    let mut total_tabs = 0u32;
    let mut total_panes = 0u32;

    if let Ok(windows) = channel.request("list_windows", serde_json::json!({})).await {
        if let Some(windows_arr) = windows.get("windows").and_then(|v| v.as_array()) {
            total_windows = windows_arr.len() as u32;

            for win in windows_arr {
                let window_id = match win.get("window_id").and_then(|v| v.as_str()) {
                    Some(id) => id,
                    None => continue,
                };

                if let Ok(tabs) = channel
                    .request("list_tabs", serde_json::json!({ "window_id": window_id }))
                    .await
                {
                    if let Some(tabs_arr) = tabs.get("tabs").and_then(|v| v.as_array()) {
                        total_tabs += tabs_arr.len() as u32;

                        for tab in tabs_arr {
                            let tab_id_str = match tab.get("tab_id") {
                                Some(serde_json::Value::String(s)) => s.clone(),
                                Some(serde_json::Value::Number(n)) => n.to_string(),
                                _ => continue,
                            };

                            if let Ok(panes) = channel
                                .request(
                                    "list_panes",
                                    serde_json::json!({ "tab_id": tab_id_str }),
                                )
                                .await
                            {
                                if let Some(panes_arr) =
                                    panes.get("panes").and_then(|v| v.as_array())
                                {
                                    total_panes += panes_arr.len() as u32;

                                    for pane in panes_arr {
                                        if let Some(pid) =
                                            pane.get("pid").and_then(|v| v.as_u64())
                                        {
                                            if pid == our_pid as u64 {
                                                let pane_id = match pane.get("pane_id") {
                                                    Some(serde_json::Value::String(s)) => {
                                                        s.clone()
                                                    }
                                                    Some(serde_json::Value::Number(n)) => {
                                                        n.to_string()
                                                    }
                                                    _ => "?".to_string(),
                                                };
                                                pane_info = Some((
                                                    pane_id,
                                                    tab_id_str.clone(),
                                                    window_id.to_string(),
                                                ));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    if let Some((pane_id, tab_id, window_id)) = pane_info {
        println!("Current Pane (PID {}):", our_pid);
        println!("  Window ID: {}", window_id);
        println!("  Tab ID:    {}", tab_id);
        println!("  Pane ID:   {}", pane_id);
    } else {
        println!("Current Pane (PID {}): not found in WT pane list", our_pid);
    }

    println!();
    println!("Summary:");
    println!(
        "  Windows: {}, Tabs: {}, Panes: {}",
        total_windows, total_tabs, total_panes
    );

    Ok(())
}

async fn run_acp_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    cli: Cli,
    shell_mgr: Arc<ShellManager>,
    wt_connected: bool,
    mut debug_rx: tokio::sync::mpsc::UnboundedReceiver<app::DebugMessage>,
    pane_identity: Option<(String, String, String)>,
    wt_event_rx: Option<tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>>,
    wt_pipe_channel: Option<Arc<PipeChannel>>,
) -> Result<()> {
    let agent_cmd = cli.agent.clone();

    let local_set = tokio::task::LocalSet::new();
    local_set
        .run_until(async move {
            let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
            let (prompt_tx, prompt_rx) = tokio::sync::mpsc::unbounded_channel();

            let evt_tx = event_tx.clone();
            tokio::task::spawn_local(event::read_crossterm_events(evt_tx));

            let dbg_event_tx = event_tx.clone();
            tokio::task::spawn_local(async move {
                while let Some(msg) = debug_rx.recv().await {
                    let _ = dbg_event_tx.send(app::AppEvent::DebugPipeMessage(msg));
                }
            });

            // Start the background pipe reader and trigger lazy event registration.
            // start_reader() splits the pipe and must complete before any requests.
            // get_capabilities triggers _ensurePageEventsRegistered() on the WT server.
            if let Some(ref pipe_ch) = wt_pipe_channel {
                tracing::info!("start_reader: starting...");
                pipe_ch.start_reader().await;
                tracing::info!("start_reader: done, sending get_capabilities...");
                match pipe_ch.request("get_capabilities", serde_json::json!({})).await {
                    Ok(v) => tracing::info!(result = %v, "get_capabilities OK"),
                    Err(e) => tracing::warn!(error = %e, "get_capabilities FAILED"),
                }
            } else {
                tracing::warn!("no wt_pipe_channel — events won't work");
            }

            // Background WT event reader: forwards push events from the pipe to the TUI.
            if let Some(mut wt_rx) = wt_event_rx {
                tracing::info!("wt_event_rx: starting background reader task");
                let wt_event_tx = event_tx.clone();
                tokio::task::spawn_local(async move {
                    while let Some(event_json) = wt_rx.recv().await {
                        tracing::debug!(event = %event_json, "wt_event_rx: received event");
                        let method = event_json
                            .get("method")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        // Pane GUIDs arrive in two cases on the wire:
                        //   - hook bridge events  → lowercase (WT_SESSION env var)
                        //   - WT-native events    → uppercase (canonical Windows GUID)
                        // Normalise here so downstream code (registry,
                        // is_agent_pane lookup, osc:133;A demote, etc.) all
                        // compare in the same case.
                        let session_id = event_json
                            .get("params")
                            .and_then(|p| p.get("session_id"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_ascii_lowercase();
                        let params = event_json
                            .get("params")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);
                        let _ = wt_event_tx.send(app::AppEvent::WtEvent {
                            method,
                            session_id,
                            params,
                        });
                    }
                });
            }

            // ── Preflight: check agent CLI availability before launching ──
            let preflight_result = preflight::check_agent(&agent_cmd).await;
            let start_in_setup = !preflight_result.all_passed();

            let shell_mgr_for_recs = Arc::clone(&shell_mgr);

            // Onboarding install request channel: the App fires this when the user
            // chooses "Install via winget" from the setup wizard.
            let (install_req_tx, mut install_req_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

            // Single task owns prompt_rx and decides when to spawn the ACP client:
            // either now (preflight passed) or later (after a successful install).
            let spawn_event_tx = event_tx.clone();
            let spawn_shell_mgr = Arc::clone(&shell_mgr);
            let spawn_agent_cmd = agent_cmd.clone();
            tokio::task::spawn_local(async move {
                let mut prompt_rx_opt = Some(prompt_rx);
                let mut spawned = false;

                if !start_in_setup {
                    if let Some(rx) = prompt_rx_opt.take() {
                        tokio::task::spawn_local(protocol::acp::client::run_acp_client(
                            spawn_agent_cmd.clone(),
                            spawn_event_tx.clone(),
                            rx,
                            Arc::clone(&spawn_shell_mgr),
                            wt_connected,
                        ));
                        spawned = true;
                    }
                }

                while let Some(()) = install_req_rx.recv().await {
                    let _ = spawn_event_tx.send(app::AppEvent::InstallStarted);
                    let progress_tx = spawn_event_tx.clone();
                    let result = preflight::winget_install_copilot(move |line| {
                        let _ = progress_tx.send(app::AppEvent::InstallProgress(line));
                    })
                    .await;
                    let _ = spawn_event_tx
                        .send(app::AppEvent::InstallComplete(result.clone()));

                    if result.is_ok() && !spawned {
                        // PATH was refreshed inside winget_install_copilot; rerun preflight.
                        let new_pf = preflight::check_agent(&spawn_agent_cmd).await;
                        let all_passed = new_pf.all_passed();
                        let _ = spawn_event_tx
                            .send(app::AppEvent::PreflightComplete(new_pf));

                        if all_passed {
                            if let Some(rx) = prompt_rx_opt.take() {
                                tokio::task::spawn_local(protocol::acp::client::run_acp_client(
                                    spawn_agent_cmd.clone(),
                                    spawn_event_tx.clone(),
                                    rx,
                                    Arc::clone(&spawn_shell_mgr),
                                    wt_connected,
                                ));
                                spawned = true;
                            }
                        }
                    }
                }
            });

            let (recommendation_tx, recommendation_rx) = tokio::sync::mpsc::unbounded_channel();
            let (permission_tx, _permission_rx) = tokio::sync::mpsc::unbounded_channel();
            let debug_capture_enabled = Arc::new(AtomicBool::new(false));
            let (_ui_event_tx, ui_event_rx) = tokio::sync::mpsc::unbounded_channel();

            // Spawn the recommendation executor so selected choices actually run.
            let rec_event_tx = event_tx.clone();
            let delegate_agents = crate::coordinator::default_delegate_agent_runtimes(
                cli.delegate_agent.as_deref(),
                Some(cli.agent.as_str()),
                cli.delegate_model.as_deref(),
            );
            tokio::spawn(crate::coordinator::run_recommendation_executor(
                recommendation_rx,
                rec_event_tx,
                shell_mgr_for_recs,
                delegate_agents,
            ));

            let autofix_enabled = !cli.no_autofix;
            let log_agent_events = std::env::var("WTA_LOG_AGENT_EVENT")
                .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
                .unwrap_or(false);
            let mut app_state = app::App::new(prompt_tx, recommendation_tx, permission_tx, debug_capture_enabled, wt_connected, autofix_enabled, log_agent_events);
            app_state.set_install_request_tx(install_req_tx);
            app_state.set_app_event_tx(event_tx.clone());

            // If preflight failed, start in Setup mode
            if start_in_setup {
                let _ = event_tx.send(app::AppEvent::PreflightComplete(preflight_result));
            }

            if let Some((pane_id, tab_id, window_id)) = pane_identity {
                app_state.pane_session_id = Some(pane_id);
                app_state.tab_id = Some(tab_id);
                app_state.window_id = Some(window_id);
            }
            // Read source pane context from env vars set by WT when creating the agent pane.
            app_state.source_session_id = std::env::var("WTA_SOURCE_SESSION_ID").ok().filter(|s| !s.is_empty());
            app_state.source_cwd = std::env::var("WTA_SOURCE_CWD").ok().filter(|s| !s.is_empty());

            // If a prompt was passed via CLI arg (e.g., from command palette creating
            // a new agent pane), delegate it to a new tab agent on startup.
            if let Some(ref initial_prompt) = cli.prompt {
                if !initial_prompt.is_empty() {
                    app_state.delegate_to_tab_agent(initial_prompt);
                }
            }

            app_state.run(terminal, event_rx, ui_event_rx).await
        })
        .await
}
