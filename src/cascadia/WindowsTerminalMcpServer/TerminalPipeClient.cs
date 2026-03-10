// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

using System.IO.Pipes;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;

namespace Microsoft.Terminal.Mcp;

/// <summary>
/// Client for the Windows Terminal protocol pipe.
/// Handles connection, authentication, and request/response messaging.
/// Connects lazily on first SendRequestAsync call.
/// </summary>
public sealed class TerminalPipeClient : IDisposable
{
    private NamedPipeClientStream? _pipe;
    private StreamReader? _reader;
    private StreamWriter? _writer;
    private readonly SemaphoreSlim _connectLock = new(1, 1);
    private readonly SemaphoreSlim _writeLock = new(1, 1);
    private int _nextRequestId;
    private bool _connected;

    public bool IsConnected => _connected && _pipe?.IsConnected == true;

    /// <summary>
    /// Ensures the pipe is connected and authenticated. Safe to call multiple times.
    /// </summary>
    private async Task EnsureConnectedAsync(CancellationToken cancellationToken = default)
    {
        if (_connected && _pipe?.IsConnected == true)
        {
            return;
        }

        await _connectLock.WaitAsync(cancellationToken);
        try
        {
            if (_connected && _pipe?.IsConnected == true)
            {
                return;
            }

            var pipeName = Environment.GetEnvironmentVariable("WT_PIPE_NAME");
            var token = Environment.GetEnvironmentVariable("WT_MCP_TOKEN");

            if (string.IsNullOrEmpty(pipeName))
            {
                throw new InvalidOperationException(
                    "WT_PIPE_NAME environment variable is not set. " +
                    "This MCP server must be launched from within a Windows Terminal pane " +
                    "that has protocol access.");
            }

            if (string.IsNullOrEmpty(token))
            {
                throw new InvalidOperationException(
                    "WT_MCP_TOKEN environment variable is not set. " +
                    "This MCP server must be launched from within a Windows Terminal pane " +
                    "that has protocol access.");
            }

            // Extract just the pipe name from the full path.
            // Handle both backslash (\\.\pipe\X) and forward slash (//./pipe/X) forms.
            var serverName = ".";
            var justPipeName = pipeName;
            if (pipeName.StartsWith(@"\\.\pipe\", StringComparison.OrdinalIgnoreCase))
            {
                justPipeName = pipeName[@"\\.\pipe\".Length..];
            }
            else if (pipeName.StartsWith("//./pipe/", StringComparison.OrdinalIgnoreCase))
            {
                justPipeName = pipeName["//./pipe/".Length..];
            }

            _pipe = new NamedPipeClientStream(serverName, justPipeName, PipeDirection.InOut, PipeOptions.Asynchronous);
            await _pipe.ConnectAsync(5000, cancellationToken);

            _reader = new StreamReader(_pipe, Encoding.UTF8);
            _writer = new StreamWriter(_pipe, Encoding.UTF8) { AutoFlush = true };

            // Authenticate immediately after connecting.
            var authResult = await SendRequestInternalAsync("authenticate", new JsonObject
            {
                ["token"] = token
            }, cancellationToken);

            var authenticated = authResult?["authenticated"]?.GetValue<bool>() == true;
            if (!authenticated)
            {
                _pipe.Dispose();
                _pipe = null;
                _reader = null;
                _writer = null;
                throw new InvalidOperationException("Authentication with Windows Terminal failed. Invalid token.");
            }

            _connected = true;
        }
        finally
        {
            _connectLock.Release();
        }
    }

    /// <summary>
    /// Sends a protocol request and waits for the response.
    /// Connects lazily on first call.
    /// </summary>
    public async Task<JsonNode?> SendRequestAsync(string method, JsonObject? parameters = null, CancellationToken cancellationToken = default)
    {
        await EnsureConnectedAsync(cancellationToken);
        return await SendRequestInternalAsync(method, parameters, cancellationToken);
    }

    /// <summary>
    /// Internal send that does not trigger connection (used during auth handshake).
    /// </summary>
    private async Task<JsonNode?> SendRequestInternalAsync(string method, JsonObject? parameters = null, CancellationToken cancellationToken = default)
    {
        if (_writer == null || _reader == null)
        {
            throw new InvalidOperationException("Not connected to Windows Terminal.");
        }

        var requestId = Interlocked.Increment(ref _nextRequestId).ToString();

        var request = new JsonObject
        {
            ["type"] = "request",
            ["id"] = requestId,
            ["method"] = method
        };

        if (parameters != null)
        {
            request["params"] = parameters;
        }

        var requestJson = request.ToJsonString(new JsonSerializerOptions
        {
            WriteIndented = false
        });

        await _writeLock.WaitAsync(cancellationToken);
        try
        {
            await _writer.WriteLineAsync(requestJson);
        }
        finally
        {
            _writeLock.Release();
        }

        var responseLine = await _reader.ReadLineAsync(cancellationToken);
        if (string.IsNullOrEmpty(responseLine))
        {
            throw new InvalidOperationException("Connection closed by Windows Terminal.");
        }

        var response = JsonNode.Parse(responseLine);
        if (response == null)
        {
            throw new InvalidOperationException("Failed to parse response from Windows Terminal.");
        }

        var error = response["error"];
        if (error != null && error.GetValueKind() != JsonValueKind.Null)
        {
            var code = error["code"]?.GetValue<string>() ?? "unknown";
            var message = error["message"]?.GetValue<string>() ?? "Unknown error";
            throw new TerminalProtocolException(code, message);
        }

        return response["result"];
    }

    public void Dispose()
    {
        _writer?.Dispose();
        _reader?.Dispose();
        _pipe?.Dispose();
        _connectLock.Dispose();
        _writeLock.Dispose();
    }
}

/// <summary>
/// Exception thrown when a terminal protocol request returns an error.
/// </summary>
public class TerminalProtocolException : Exception
{
    public string Code { get; }

    public TerminalProtocolException(string code, string message)
        : base(message)
    {
        Code = code;
    }
}
