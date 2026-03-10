// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

using System.ComponentModel;
using System.Text.Json.Nodes;
using ModelContextProtocol.Server;

namespace Microsoft.Terminal.Mcp;

/// <summary>
/// MCP tool definitions that map to Windows Terminal protocol methods.
/// Each tool sends the corresponding protocol request over the named pipe
/// and returns the result.
/// </summary>
[McpServerToolType]
public sealed class TerminalTools
{
    private readonly TerminalPipeClient _client;

    public TerminalTools(TerminalPipeClient client)
    {
        _client = client;
    }

    // ========================================================================
    // Query Tools
    // ========================================================================

    [McpServerTool(Name = "get_active_pane"), Description("Returns the currently focused pane — the pane the user was last working in. Use this to understand the user's current context, read their recent output, or help with errors they encountered.")]
    public async Task<string> GetActivePane()
    {
        var result = await _client.SendRequestAsync("get_active_pane");
        return result?.ToJsonString() ?? "{}";
    }

    [McpServerTool(Name = "list_windows"), Description("Lists all open Windows Terminal windows.")]
    public async Task<string> ListWindows()
    {
        var result = await _client.SendRequestAsync("list_windows");
        return result?.ToJsonString() ?? "{}";
    }

    [McpServerTool(Name = "list_tabs"), Description("Lists all open tabs, optionally filtered to a single window.")]
    public async Task<string> ListTabs(
        [Description("Filter to tabs in this window. Omit to list all tabs.")] string? window_id = null)
    {
        var parameters = new JsonObject();
        if (!string.IsNullOrEmpty(window_id))
            parameters["window_id"] = window_id;

        var result = await _client.SendRequestAsync("list_tabs", parameters);
        return result?.ToJsonString() ?? "{}";
    }

    [McpServerTool(Name = "list_panes"), Description("Lists panes, optionally filtered to a single tab or window.")]
    public async Task<string> ListPanes(
        [Description("Filter to panes in this tab.")] string? tab_id = null,
        [Description("Filter to panes in this window.")] string? window_id = null)
    {
        var parameters = new JsonObject();
        if (!string.IsNullOrEmpty(tab_id))
            parameters["tab_id"] = tab_id;
        if (!string.IsNullOrEmpty(window_id))
            parameters["window_id"] = window_id;

        var result = await _client.SendRequestAsync("list_panes", parameters);
        return result?.ToJsonString() ?? "{}";
    }

    [McpServerTool(Name = "read_pane_output"), Description("Reads the scrollback or visible screen content of a pane.")]
    public async Task<string> ReadPaneOutput(
        [Description("Target pane ID.")] string pane_id,
        [Description("'scrollback' (default) or 'screen' (visible viewport only).")] string? source = null,
        [Description("Maximum number of lines to return. Default: 200.")] int? max_lines = null)
    {
        var parameters = new JsonObject
        {
            ["pane_id"] = pane_id
        };
        if (!string.IsNullOrEmpty(source))
            parameters["source"] = source;
        if (max_lines.HasValue)
            parameters["max_lines"] = max_lines.Value;

        var result = await _client.SendRequestAsync("read_pane_output", parameters);
        return result?.ToJsonString() ?? "{}";
    }

    [McpServerTool(Name = "get_process_status"), Description("Checks whether a pane's process is still running or has exited.")]
    public async Task<string> GetProcessStatus(
        [Description("Target pane ID.")] string pane_id)
    {
        var parameters = new JsonObject
        {
            ["pane_id"] = pane_id
        };

        var result = await _client.SendRequestAsync("get_process_status", parameters);
        return result?.ToJsonString() ?? "{}";
    }

    [McpServerTool(Name = "get_session_variable"), Description("Reads a session variable from a pane.")]
    public async Task<string> GetSessionVariable(
        [Description("Target pane ID.")] string pane_id,
        [Description("Variable name.")] string name)
    {
        var parameters = new JsonObject
        {
            ["pane_id"] = pane_id,
            ["name"] = name
        };

        var result = await _client.SendRequestAsync("get_session_variable", parameters);
        return result?.ToJsonString() ?? "{}";
    }

    [McpServerTool(Name = "get_settings"), Description("Returns the full contents of the user's settings.json file.")]
    public async Task<string> GetSettings()
    {
        var result = await _client.SendRequestAsync("get_settings");
        return result?.ToJsonString() ?? "{}";
    }

    // ========================================================================
    // Mutation Tools
    // ========================================================================

    [McpServerTool(Name = "create_tab"), Description("Opens a new tab in a window.")]
    public async Task<string> CreateTab(
        [Description("Window to create the tab in. Uses the most recently focused window if omitted.")] string? window_id = null,
        [Description("Profile name or GUID. Uses default profile if omitted.")] string? profile = null,
        [Description("Command to run. Uses the profile's default if omitted.")] string? commandline = null,
        [Description("Initial tab/pane title.")] string? title = null,
        [Description("If true, prevents the process from overriding the title.")] bool? suppress_application_title = null,
        [Description("If true, injects MCP credentials (WT_MCP_TOKEN, WT_PIPE_NAME) into the new pane's environment so an AI CLI launched there can use terminal control tools. Only set this when spawning a delegate AI CLI instance.")] bool? inject_mcp_credentials = null,
        [Description("If true (default), the tab is created without stealing focus from the current tab.")] bool? background = null)
    {
        var parameters = new JsonObject();
        if (!string.IsNullOrEmpty(window_id))
            parameters["window_id"] = window_id;
        if (!string.IsNullOrEmpty(profile))
            parameters["profile"] = profile;
        if (!string.IsNullOrEmpty(commandline))
            parameters["commandline"] = commandline;
        if (!string.IsNullOrEmpty(title))
            parameters["title"] = title;
        if (suppress_application_title.HasValue)
            parameters["suppress_application_title"] = suppress_application_title.Value;
        if (inject_mcp_credentials.HasValue)
            parameters["inject_mcp_credentials"] = inject_mcp_credentials.Value;
        parameters["background"] = background ?? true;

        var result = await _client.SendRequestAsync("create_tab", parameters);
        return result?.ToJsonString() ?? "{}";
    }

    [McpServerTool(Name = "split_pane"), Description("Splits an existing pane.")]
    public async Task<string> SplitPane(
        [Description("Pane to split.")] string pane_id,
        [Description("Direction: 'right', 'left', 'down', or 'up'.")] string direction,
        [Description("Profile for the new pane.")] string? profile = null,
        [Description("Command to run in the new pane.")] string? commandline = null,
        [Description("Fraction of the split (0.0-1.0). Default: 0.5.")] double? size = null,
        [Description("If true, injects MCP credentials into the new pane's environment so an AI CLI launched there can use terminal control tools. Only set this when spawning a delegate AI CLI instance.")] bool? inject_mcp_credentials = null,
        [Description("If true (default), the split happens without stealing focus from the current pane.")] bool? background = null)
    {
        var parameters = new JsonObject
        {
            ["pane_id"] = pane_id,
            ["direction"] = direction
        };
        if (!string.IsNullOrEmpty(profile))
            parameters["profile"] = profile;
        if (!string.IsNullOrEmpty(commandline))
            parameters["commandline"] = commandline;
        if (size.HasValue)
            parameters["size"] = size.Value;
        if (inject_mcp_credentials.HasValue)
            parameters["inject_mcp_credentials"] = inject_mcp_credentials.Value;
        parameters["background"] = background ?? true;

        var result = await _client.SendRequestAsync("split_pane", parameters);
        return result?.ToJsonString() ?? "{}";
    }

    [McpServerTool(Name = "close_pane"), Description("Closes a pane. If it is the last pane in a tab, the tab is also closed.")]
    public async Task<string> ClosePane(
        [Description("Pane to close.")] string pane_id)
    {
        var parameters = new JsonObject
        {
            ["pane_id"] = pane_id
        };

        var result = await _client.SendRequestAsync("close_pane", parameters);
        return result?.ToJsonString() ?? "{}";
    }

    [McpServerTool(Name = "send_input"), Description("Sends text input to a pane as if the user typed it.")]
    public async Task<string> SendInput(
        [Description("Target pane ID.")] string pane_id,
        [Description("Text to send. Use \\n for Enter.")] string text)
    {
        var parameters = new JsonObject
        {
            ["pane_id"] = pane_id,
            ["text"] = text
        };

        var result = await _client.SendRequestAsync("send_input", parameters);
        return result?.ToJsonString() ?? "{}";
    }

    [McpServerTool(Name = "set_session_variable"), Description("Sets a session variable on a pane.")]
    public async Task<string> SetSessionVariable(
        [Description("Target pane ID.")] string pane_id,
        [Description("Variable name.")] string name,
        [Description("Variable value. Use null to delete.")] string? value = null)
    {
        var parameters = new JsonObject
        {
            ["pane_id"] = pane_id,
            ["name"] = name
        };

        if (value != null)
            parameters["value"] = value;
        else
            parameters["value"] = null;

        var result = await _client.SendRequestAsync("set_session_variable", parameters);
        return result?.ToJsonString() ?? "{}";
    }

    [McpServerTool(Name = "set_settings"), Description("Replaces the settings.json content. Creates a backup before overwriting.")]
    public async Task<string> SetSettings(
        [Description("The full replacement contents for settings.json.")] string settings)
    {
        var parameters = new JsonObject
        {
            ["settings"] = settings
        };

        var result = await _client.SendRequestAsync("set_settings", parameters);
        return result?.ToJsonString() ?? "{}";
    }
}
