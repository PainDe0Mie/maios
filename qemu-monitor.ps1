<#
.SYNOPSIS
    Client interactif pour le monitor QEMU (remplace telnet).

.DESCRIPTION
    Se connecte au monitor QEMU sur TCP localhost:55555.
    Utilise les sockets .NET natifs — pas besoin de telnet.

.EXAMPLE
    .\qemu-monitor.ps1           # Se connecte et ouvre une session interactive
    .\qemu-monitor.ps1 -Cmd info status   # Envoie une commande et quitte
    .\qemu-monitor.ps1 -Cmd quit          # Arrête QEMU proprement

.PARAMETER Port
    Port TCP du monitor QEMU (défaut: 55555).

.PARAMETER Cmd
    Commande unique à envoyer puis quitter (mode non-interactif).
#>
param(
    [int]$Port = 55555,
    [string]$Cmd = ""
)

$Host_ = "127.0.0.1"
$Timeout = 3000  # ms

function Connect-QemuMonitor {
    param([int]$port)
    try {
        $client = [System.Net.Sockets.TcpClient]::new()
        $result = $client.BeginConnect($Host_, $port, $null, $null)
        $ok = $result.AsyncWaitHandle.WaitOne($Timeout, $true)
        if (-not $ok -or -not $client.Connected) {
            $client.Close()
            return $null
        }
        $client.EndConnect($result)
        return $client
    } catch {
        return $null
    }
}

function Read-Available {
    param($stream, [int]$waitMs = 200)
    $buf = [byte[]]::new(4096)
    $sb  = [System.Text.StringBuilder]::new()
    $deadline = [DateTime]::Now.AddMilliseconds($waitMs)
    while ([DateTime]::Now -lt $deadline) {
        if ($stream.DataAvailable) {
            $n = $stream.Read($buf, 0, $buf.Length)
            if ($n -gt 0) {
                $sb.Append([System.Text.Encoding]::UTF8.GetString($buf, 0, $n)) | Out-Null
                $deadline = [DateTime]::Now.AddMilliseconds(80)  # reset après données
            }
        } else {
            Start-Sleep -Milliseconds 20
        }
    }
    return $sb.ToString()
}

# ── Connexion ────────────────────────────────────────────────────────────────
$client = Connect-QemuMonitor -port $Port
if ($null -eq $client) {
    Write-Host "[ERREUR] Impossible de se connecter au monitor QEMU sur localhost:$Port" -ForegroundColor Red
    Write-Host "         QEMU est-il en cours d'exécution ? (lance build-and-run.ps1 d'abord)" -ForegroundColor Yellow
    exit 1
}

$stream = $client.GetStream()
$stream.ReadTimeout  = 500
$stream.WriteTimeout = 500

# Lire le banner QEMU
$banner = Read-Available $stream -waitMs 500
if ($banner) {
    Write-Host $banner.TrimEnd() -ForegroundColor DarkGreen
}

# ── Mode commande unique ou multiples (séparées par ;) ────────────────────────
if ($Cmd -ne "") {
    $commands = $Cmd -split ';'
    foreach ($c in $commands) {
        $c = $c.Trim()
        if ($c -eq "") { continue }
        $bytes = [System.Text.Encoding]::UTF8.GetBytes("$c`n")
        $stream.Write($bytes, 0, $bytes.Length)
        $response = Read-Available $stream -waitMs 400
        Write-Host $response.TrimEnd()
    }
    $client.Close()
    exit 0
}

# ── Mode interactif ───────────────────────────────────────────────────────────
Write-Host ""
Write-Host "=== QEMU Monitor (localhost:$Port) ===" -ForegroundColor Cyan
Write-Host "Commandes utiles : info status | info registers | info mem | quit | system_reset" -ForegroundColor DarkGray
Write-Host "Tapez 'exit' ou Ctrl+C pour quitter le monitor (sans arrêter QEMU)." -ForegroundColor DarkGray
Write-Host ""

try {
    while ($true) {
        Write-Host "(qemu) " -NoNewline -ForegroundColor Yellow
        $line = Read-Host

        if ($null -eq $line -or $line -eq "exit" -or $line -eq "q") {
            break
        }

        if ($line -eq "") { continue }

        $bytes = [System.Text.Encoding]::UTF8.GetBytes("$line`n")
        try {
            $stream.Write($bytes, 0, $bytes.Length)
        } catch {
            Write-Host "[ERREUR] Connexion perdue." -ForegroundColor Red
            break
        }

        $response = Read-Available $stream -waitMs 300
        if ($response) {
            # Supprimer le "(qemu) " du monitor QEMU (on affiche le nôtre)
            $clean = $response -replace '\(qemu\)\s*$', '' -replace '\(qemu\)\s*\n', ''
            Write-Host $clean.TrimEnd()
        }
    }
} finally {
    $client.Close()
    Write-Host ""
    Write-Host "Monitor déconnecté." -ForegroundColor DarkGray
}
