param([int]$TermPid)

try {
    $pipeName = "WindowsTerminal-$TermPid"
    Write-Host "Connecting to $pipeName..."
    $c = New-Object System.IO.Pipes.NamedPipeClientStream('.', $pipeName, 'InOut')
    $c.Connect(2000)
    Write-Host 'Connected!'

    $writer = New-Object System.IO.StreamWriter($c)
    $writer.AutoFlush = $true
    $reader = New-Object System.IO.StreamReader($c)

    # Send authenticate request with dummy token
    $msg = '{"type":"request","id":"1","method":"authenticate","params":{"token":"test"}}'
    Write-Host "Sending: $msg"
    $writer.WriteLine($msg)

    # Read response
    $task = $reader.ReadLineAsync()
    if ($task.Wait(3000)) {
        Write-Host "Response: $($task.Result)"
    } else {
        Write-Host 'Timeout reading response'
    }

    $c.Dispose()
} catch {
    Write-Host "Error: $_"
}
