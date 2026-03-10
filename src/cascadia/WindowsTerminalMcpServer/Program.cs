// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.Hosting;
using Microsoft.Extensions.Logging;
using Microsoft.Terminal.Mcp;
using ModelContextProtocol.Server;

var builder = Host.CreateApplicationBuilder(args);

// Disable all console logging — stdout is the MCP stdio transport channel
// and any non-JSON output breaks the protocol.
builder.Logging.ClearProviders();
builder.Logging.AddDebug(); // Debug output goes to debugger, not stdout

builder.Services.AddSingleton<TerminalPipeClient>();

builder.Services.AddMcpServer(options =>
{
    options.ServerInfo = new()
    {
        Name = "Windows Terminal MCP Server",
        Version = "1.0.0"
    };
})
.WithStdioServerTransport()
.WithToolsFromAssembly();

await builder.Build().RunAsync();
