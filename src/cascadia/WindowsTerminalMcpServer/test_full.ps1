param([string]$PipeName, [string]$Token)

try {
    # Extract just the pipe name from \\.\pipe\XXX
    $justPipeName = $PipeName
    $prefix = '\\.\pipe\'
    if ($PipeName.StartsWith($prefix)) {
        $justPipeName = $PipeName.Substring($prefix.Length)
    }

    Write-Host "Raw PipeName: $PipeName"
    Write-Host "Connecting to pipe: $justPipeName"
    $c = New-Object System.IO.Pipes.NamedPipeClientStream('.', $justPipeName, 'InOut')
    $c.Connect(3000)
    Write-Host 'Connected!'

    $writer = New-Object System.IO.StreamWriter($c)
    $writer.AutoFlush = $true
    $reader = New-Object System.IO.StreamReader($c)

    # Authenticate
    $authMsg = "{`"type`":`"request`",`"id`":`"1`",`"method`":`"authenticate`",`"params`":{`"token`":`"$Token`"}}"
    Write-Host "Sending auth..."
    $writer.WriteLine($authMsg)

    $task = $reader.ReadLineAsync()
    if ($task.Wait(3000)) {
        Write-Host "Auth response: $($task.Result)"
    } else {
        Write-Host 'Auth timeout'; return
    }

    # List windows
    $listMsg = '{"type":"request","id":"2","method":"list_windows"}'
    Write-Host "`nSending list_windows..."
    $writer.WriteLine($listMsg)

    $task2 = $reader.ReadLineAsync()
    if ($task2.Wait(5000)) {
        Write-Host "list_windows response: $($task2.Result)"
    } else {
        Write-Host 'list_windows timeout'
    }

    $c.Dispose()
} catch {
    Write-Host "Error: $_"
}
