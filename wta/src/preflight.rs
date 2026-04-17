// ─── Preflight Checks ────────────────────────────────────────────────────────
//
// Pre-flight validation run before launching the ACP agent.
// Checks CLI presence on PATH and authentication status, producing
// structured results that the setup wizard can display.

use crate::agent_registry::{self, AcpAuthFlow, AgentProfile};

/// Status of a single preflight check.
#[derive(Debug, Clone, PartialEq)]
pub enum CheckStatus {
    /// Check is in progress.
    Checking,
    /// Check passed successfully.
    Passed,
    /// Check failed with a reason.
    Failed(String),
    /// Check was skipped (prerequisite not met).
    Skipped,
}

/// Result of all preflight checks for an agent.
#[derive(Debug, Clone)]
pub struct PreflightResult {
    pub agent_id: String,
    pub display_name: String,
    pub cli_status: CheckStatus,
    pub cli_path: Option<String>,
    pub auth_status: CheckStatus,
    pub install_hint: String,
    pub install_url: String,
    pub auth_hint: String,
}

impl PreflightResult {
    /// Returns true if all required checks passed.
    pub fn all_passed(&self) -> bool {
        self.cli_status == CheckStatus::Passed
            && matches!(
                self.auth_status,
                CheckStatus::Passed | CheckStatus::Skipped
            )
    }
}

/// Extract the agent id (bare name) from an agent command string.
/// e.g. "copilot --acp --stdio" → "copilot"
pub fn extract_agent_id(agent_cmd: &str) -> &str {
    agent_cmd.split_whitespace().next().unwrap_or(agent_cmd)
}

/// Run all preflight checks for the given agent command.
pub async fn check_agent(agent_cmd: &str) -> PreflightResult {
    let agent_id = extract_agent_id(agent_cmd);
    let profile = agent_registry::lookup_profile(agent_id);

    let mut result = PreflightResult {
        agent_id: agent_id.to_string(),
        display_name: profile.display_name.to_string(),
        cli_status: CheckStatus::Checking,
        cli_path: None,
        auth_status: CheckStatus::Skipped,
        install_hint: profile.install_hint.to_string(),
        install_url: profile.install_url.to_string(),
        auth_hint: profile.auth_hint.to_string(),
    };

    // 1. Check if CLI is on PATH
    let resolved = agent_registry::resolve_bare_agent_name(agent_id);
    match find_on_path(&resolved, profile) {
        Some(path) => {
            result.cli_status = CheckStatus::Passed;
            result.cli_path = Some(path);
        }
        None => {
            result.cli_status = CheckStatus::Failed("Not found on PATH".to_string());
            // Skip auth check if CLI isn't even installed
            result.auth_status = CheckStatus::Skipped;
            return result;
        }
    }

    // 2. Check authentication (only for agents with external auth)
    if profile.acp_auth_flow == AcpAuthFlow::External
        && !profile.auth_check_command.is_empty()
    {
        result.auth_status = check_auth(profile.auth_check_command).await;
    } else if profile.acp_auth_flow == AcpAuthFlow::InProtocol {
        // In-protocol auth is handled during connection, mark as skipped
        result.auth_status = CheckStatus::Skipped;
    } else {
        result.auth_status = CheckStatus::Skipped;
    }

    result
}

/// Find the agent executable on PATH and return its full path.
fn find_on_path(resolved_name: &str, profile: &AgentProfile) -> Option<String> {
    let path_var = std::env::var("PATH").ok()?;

    // Try the resolved name first (e.g. "copilot.exe")
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(resolved_name);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().to_string());
        }
    }

    // Try each extension from the profile
    let base = resolved_name
        .strip_suffix(".exe")
        .or_else(|| resolved_name.strip_suffix(".cmd"))
        .unwrap_or(resolved_name);

    for ext in profile.exe_search_order {
        let name = format!("{}{}", base, ext);
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(&name);
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().to_string());
            }
        }
    }

    None
}

/// Check authentication by running the auth check command.
/// Returns Passed if exit code is 0, Failed otherwise.
async fn check_auth(auth_check_command: &str) -> CheckStatus {
    let parts: Vec<&str> = auth_check_command.split_whitespace().collect();
    let (program, args) = match parts.split_first() {
        Some((prog, args)) => (*prog, args),
        None => return CheckStatus::Skipped,
    };

    match tokio::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => match tokio::time::timeout(
            std::time::Duration::from_secs(10),
            child.wait_with_output(),
        )
        .await
        {
            Ok(Ok(output)) => {
                if output.status.success() {
                    CheckStatus::Passed
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let reason = if stderr.trim().is_empty() {
                        "Not authenticated".to_string()
                    } else {
                        // Take first non-empty line from stderr as the reason
                        stderr
                            .lines()
                            .find(|l| !l.trim().is_empty())
                            .unwrap_or("Not authenticated")
                            .trim()
                            .to_string()
                    };
                    CheckStatus::Failed(reason)
                }
            }
            Ok(Err(e)) => CheckStatus::Failed(format!("Auth check failed: {}", e)),
            Err(_) => CheckStatus::Failed("Auth check timed out".to_string()),
        },
        Err(_) => {
            // Can't run auth check command — probably CLI not fully functional
            CheckStatus::Failed("Could not run auth check".to_string())
        }
    }
}
