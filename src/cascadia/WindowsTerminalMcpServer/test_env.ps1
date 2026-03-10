# Diagnostic: log what env vars the MCP server child process receives
$logFile = "$env:TEMP\wt-mcp-env-debug.txt"
"=== MCP Server Env Debug ===" | Out-File $logFile
"Time: $(Get-Date)" | Out-File $logFile -Append
"WT_MCP_TOKEN: $(if ($env:WT_MCP_TOKEN) { 'SET (length ' + $env:WT_MCP_TOKEN.Length + ')' } else { 'NOT SET' })" | Out-File $logFile -Append
"WT_PIPE_NAME: $($env:WT_PIPE_NAME ?? 'NOT SET')" | Out-File $logFile -Append
"WT_MCP_CONFIG: $($env:WT_MCP_CONFIG ?? 'NOT SET')" | Out-File $logFile -Append

# Now run the actual MCP server
& "C:\Users\pabhojwa\source\repos\terminal\src\cascadia\WindowsTerminalMcpServer\bin\Debug\net9.0\win-x64\WindowsTerminalMcpServer.exe"
