$client = [System.Net.Sockets.TcpClient]::new("127.0.0.1", 55555)
$s = $client.GetStream()
Start-Sleep -Milliseconds 300
$b = [byte[]]::new(8192)
while ($s.DataAvailable) { $s.Read($b, 0, $b.Length) | Out-Null }

foreach ($cpu in 0..3) {
    $cmds = @("cpu $cpu", "info registers")
    foreach ($c in $cmds) {
        $x = [System.Text.Encoding]::UTF8.GetBytes("$c`n")
        $s.Write($x, 0, $x.Length)
        Start-Sleep -Milliseconds 200
    }
    $r = ""
    while ($s.DataAvailable) {
        $n = $s.Read($b, 0, $b.Length)
        $r += [System.Text.Encoding]::UTF8.GetString($b, 0, $n)
    }
    Write-Host "=== CPU $cpu ==="
    foreach ($line in ($r -split "`n")) {
        if ($line -match "(RIP|RBX|RSP|RFL|RAX)=") {
            Write-Host $line.Trim()
        }
    }
    Write-Host ""
}
$client.Close()
