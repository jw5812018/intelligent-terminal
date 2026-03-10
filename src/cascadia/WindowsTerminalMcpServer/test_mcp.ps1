# Test the MCP server by sending it stdio MCP protocol messages
param([string]$PipeName, [string]$Token)

$env:WT_PIPE_NAME = $PipeName
$env:WT_MCP_TOKEN = $Token

$exePath = "$PSScriptRoot\bin\Debug\net9.0\win-x64\WindowsTerminalMcpServer.exe"
Write-Host "Starting MCP server with WT_PIPE_NAME=$PipeName"

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $exePath
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false

$proc = [System.Diagnostics.Process]::Start($psi)

Start-Sleep -Milliseconds 500

# Read any stderr
if ($proc.StandardError.Peek() -ge 0) {
    Write-Host "STDERR: $($proc.StandardError.ReadToEnd())"
}

# Send MCP initialize
$initMsg = '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}'
Write-Host "`nSending MCP initialize..."
$proc.StandardInput.WriteLine($initMsg)
$proc.StandardInput.Flush()

Start-Sleep -Milliseconds 1000

# Read response
$response = $proc.StandardOutput.ReadLine()
Write-Host "Response: $response"

# Send initialized notification
$notif = '{"jsonrpc":"2.0","method":"notifications/initialized"}'
$proc.StandardInput.WriteLine($notif)
$proc.StandardInput.Flush()

Start-Sleep -Milliseconds 500

# Call list_windows tool
$toolCall = '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_windows","arguments":{}}}'
Write-Host "`nSending tools/call list_windows..."
$proc.StandardInput.WriteLine($toolCall)
$proc.StandardInput.Flush()

Start-Sleep -Milliseconds 3000

# Read response
if (!$proc.StandardOutput.EndOfStream) {
    $toolResponse = $proc.StandardOutput.ReadLine()
    Write-Host "Tool response: $toolResponse"
}

# Check stderr
$stderr = $proc.StandardError.ReadToEnd()
if ($stderr) {
    Write-Host "`nSTDERR output:`n$stderr"
}

$proc.Kill()
