# send-event.ps1 — Forward CLI agent hook events to WTA via wtcli.
#
# Two distinct GUIDs flow through this script — DO NOT conflate them:
#
#   1. PANE session id  (= $env:WT_SESSION, set per-pane by ConptyConnection):
#      Identifies the Windows Terminal pane the agent is running in.
#      Used by the COM broadcast as `params.session_id`, and by wta's
#      `route_agent_event_to_registry` / `dispatch_focus_pane` for routing.
#      We pass it explicitly via `wtcli send-event -p <guid>`. Without
#      `-p`, wtcli falls back to `GetActivePane()` which is the *currently
#      focused* pane — that is wrong for hooks, which fire from a
#      backgrounded agent pane while the user looks elsewhere (and in
#      particular, can be the wta agent pane itself, in which case wta
#      drops the event as "from our own pane" and the row stays IDLE).
#
#   2. AGENT session id (Claude/Gemini UUID, Copilot folder name):
#      Identifies the CLI agent's own conversation. Used as the resume
#      identifier (`claude --resume <id>`, `gemini --resume <id>`,
#      copilot's session-state directory) and as the registry key in
#      wta when known. Travels in the wrapped payload as
#      `agent_session_id`.
#
# CLI-source identification:
#   The installer hard-codes which CLI invokes this script via the
#   `-CliSource` parameter (claude / copilot / gemini). That is the
#   ONLY reliable signal — env-var heuristics are unreliable because:
#     * Claude registers as TOP-LEVEL hooks (~/.claude/settings.json),
#       not via the plugin loader, so CLAUDE_PLUGIN_ROOT is unset.
#     * Claude doesn't export CLAUDE_SESSION_ID; the session id is
#       only in stdin JSON.
#     * Copilot CLI inherits Claude's plugin shape, so CLAUDE_PLUGIN_ROOT
#       is set when Copilot loads our plugin — making it indistinguishable
#       from a real Claude run by env vars alone.
#   Without `-CliSource`, Claude hook events were silently mis-tagged
#   as "copilot" (the historical default fallback below) and rows
#   showed the wrong CLI label / icon in F2.
param(
    [string]$EventType = "agent.hook",
    [string]$CliSource = ""
)

# Skip if not running inside Windows Terminal
if (-not $env:WT_COM_CLSID) { exit 0 }

# Locate wtcli.exe. Order:
#   1. PATH (works if the package registers a wtcli AppExecutionAlias).
#   2. $env:WTCLI_PATH override (escape hatch for dev builds / debugging).
#   3. The Windows Terminal package InstallLocation (where the build drops it).
$wtcliPath = (Get-Command wtcli -ErrorAction SilentlyContinue).Source
if (-not $wtcliPath -and $env:WTCLI_PATH -and (Test-Path $env:WTCLI_PATH)) {
    $wtcliPath = $env:WTCLI_PATH
}
if (-not $wtcliPath) {
    try {
        $pkgs = Get-AppxPackage -Name "*Terminal*" -ErrorAction SilentlyContinue
        foreach ($pkg in $pkgs) {
            $candidate = Join-Path $pkg.InstallLocation "wtcli.exe"
            if (Test-Path $candidate) { $wtcliPath = $candidate; break }
        }
    } catch { }
}
if (-not $wtcliPath) { exit 0 }

# Read hook JSON from stdin (may be empty for events that don't carry a
# payload, e.g. some CLIs' AfterTool / SessionEnd. We still want those to
# reach WTA so the state can transition out of Working/Working back to Idle.)
$hookData = [Console]::In.ReadToEnd()
if (-not $hookData) { $hookData = "" }

# Wrap payload and send via ProcessStartInfo to avoid PowerShell argument mangling
try {
    # ConvertFrom-Json on empty/whitespace input throws; treat as no payload.
    $parsed = $null
    if ($hookData.Trim()) {
        try { $parsed = $hookData | ConvertFrom-Json } catch { $parsed = $null }
    }

    # Extract agent_session_id from stdin JSON (Claude/Gemini), env (Copilot), or empty.
    $agentSessionId = ""
    if ($parsed -and ($parsed.PSObject.Properties.Name -contains "session_id")) {
        $agentSessionId = [string]$parsed.session_id
    } elseif ($env:COPILOT_SESSION_ID) {
        $agentSessionId = $env:COPILOT_SESSION_ID
    } elseif ($env:CLAUDE_SESSION_ID) {
        $agentSessionId = $env:CLAUDE_SESSION_ID
    } elseif ($env:GEMINI_SESSION_ID) {
        $agentSessionId = $env:GEMINI_SESSION_ID
    }

    # Detect CLI source — priority order:
    #   1. The `-CliSource` script parameter (set by the installer per-CLI;
    #      most reliable: hard-coded at install time, not affected by
    #      env-var leakage between CLIs that share Claude's plugin shape).
    #   2. WTA_CLI_SOURCE env var (manual override / bash hooks).
    #   3. CLI-specific session-id env vars (only that CLI sets each one).
    #   4. CLI-specific marker env vars.
    #   5. CLAUDE_PLUGIN_ROOT — last resort BEFORE the default. Note that
    #      Copilot also sets this when loading our plugin, so this matches
    #      Claude only when COPILOT_SESSION_ID was already absent above.
    #   6. Default "copilot" — LEGACY fallback; should never be hit when
    #      installer plumbing is correct, but kept so a manual / external
    #      invocation without -CliSource doesn't crash.
    if (-not $CliSource) { $CliSource = $env:WTA_CLI_SOURCE }
    if (-not $CliSource) {
        if     ($env:COPILOT_SESSION_ID) { $CliSource = "copilot" }
        elseif ($env:GEMINI_SESSION_ID)  { $CliSource = "gemini" }
        elseif ($env:CLAUDE_SESSION_ID)  { $CliSource = "claude" }
        elseif ($env:GEMINI_CLI)         { $CliSource = "gemini" }
        elseif ($env:COPILOT_CLI)        { $CliSource = "copilot" }
        elseif ($env:CLAUDE_PLUGIN_ROOT) { $CliSource = "claude" }
        else { $CliSource = "copilot" }
    }
    $cliSource = $CliSource

    $wrapper = @{
        cli_source       = $cliSource
        agent_session_id = $agentSessionId
        payload          = $parsed
    }

    $payload = $wrapper | ConvertTo-Json -Compress -Depth 5

    # CommandLineToArgvW-correct escape for a quoted argument:
    #   * Every backslash run that precedes a `"` (or end of string) is doubled.
    #   * Every `"` is preceded by a single extra backslash.
    # This is required so messages containing Windows paths (e.g. permission
    # prompts: 'Get-Acl -Path "C:\Windows\..."') don't have their JSON truncated
    # by the child process's argv parser.
    $sb = New-Object System.Text.StringBuilder
    $bsRun = 0
    foreach ($ch in $payload.ToCharArray()) {
        if ($ch -eq '\') {
            $bsRun++
        } elseif ($ch -eq '"') {
            [void]$sb.Append([string]'\' * ($bsRun * 2 + 1))
            [void]$sb.Append('"')
            $bsRun = 0
        } else {
            if ($bsRun -gt 0) { [void]$sb.Append([string]'\' * $bsRun); $bsRun = 0 }
            [void]$sb.Append($ch)
        }
    }
    if ($bsRun -gt 0) { [void]$sb.Append([string]'\' * ($bsRun * 2)) }
    $escaped = $sb.ToString()

    # Pin the originating pane explicitly via -p $env:WT_SESSION when
    # available. WT_SESSION is set per-pane by ConptyConnection.cpp and is
    # the same GUID returned by IProtocolServer::GetActivePane().SessionId
    # for that pane. Passing it removes wtcli's fallback to "currently
    # focused pane", which is essential for Notification/Stop hooks that
    # fire while the user is in a *different* pane.
    $paneArg = ""
    if ($env:WT_SESSION) {
        $paneArg = " -p `"$($env:WT_SESSION)`""
    }

    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $wtcliPath
    $psi.Arguments = "send-event$paneArg -e $EventType `"$escaped`""
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardError = $true
    $proc = [System.Diagnostics.Process]::Start($psi)
    $proc.WaitForExit(5000)
} catch {
    # Silently ignore errors — hooks must not block the agent.
}
