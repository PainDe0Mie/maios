<#
.SYNOPSIS
    Build MaiOS in WSL and launch in QEMU on Windows.

.PARAMETER BuildOnly
    Only build, don't launch QEMU.

.PARAMETER RunOnly
    Skip build, launch QEMU with existing ISO.

.PARAMETER Debug
    Build in debug mode instead of release.

.EXAMPLE
    .\run.ps1          # Build + Run
    .\run.ps1 -RunOnly # Just launch QEMU
    .\run.ps1 -BuildOnly # Just build
#>
param(
    [switch]$BuildOnly,
    [switch]$RunOnly,
    [switch]$Debug
)

$ErrorActionPreference = "Stop"

# ── Paths ────────────────────────────────────────────────────────────────────
$ProjectDir   = $PSScriptRoot
$WslProject   = wsl wslpath -u ($ProjectDir -replace '\\','/')
$QemuBin      = "C:\Program Files\qemu\qemu-system-x86_64.exe"
$IsoPath      = Join-Path $ProjectDir "build\MaiOS.iso"
$DiskImage    = Join-Path $ProjectDir "fat32.img"

# ── Colors ───────────────────────────────────────────────────────────────────
function Write-Step($msg)  { Write-Host "`n>> $msg" -ForegroundColor Cyan }
function Write-OK($msg)    { Write-Host "   $msg"   -ForegroundColor Green }
function Write-Err($msg)   { Write-Host "   $msg"   -ForegroundColor Red }

# ── Build (in WSL) ───────────────────────────────────────────────────────────
if (-not $RunOnly) {
    Write-Step "Building MaiOS in WSL..."

    $buildMode = if ($Debug) { "debug" } else { "release" }

    # Ensure nasm is installed in WSL
    wsl bash -c "command -v nasm >/dev/null 2>&1 || { echo '  Installing nasm...'; sudo apt-get install -y nasm >/dev/null 2>&1; }"

    $buildCmd = "cd '$WslProject' && make iso BUILD_MODE=$buildMode 2>&1"
    Write-Host "   Running: make iso BUILD_MODE=$buildMode" -ForegroundColor DarkGray

    $output = wsl bash -c $buildCmd
    $exitCode = $LASTEXITCODE

    # Show last 30 lines of build output
    $lines = $output -split "`n"
    if ($lines.Count -gt 30) {
        Write-Host "   ... ($($lines.Count - 30) lines omitted)" -ForegroundColor DarkGray
    }
    $lines | Select-Object -Last 30 | ForEach-Object { Write-Host "   $_" -ForegroundColor DarkGray }

    if ($exitCode -ne 0) {
        Write-Err "Build failed (exit code $exitCode)"
        exit 1
    }
    Write-OK "Build OK"
}

# ── Launch QEMU ──────────────────────────────────────────────────────────────
if (-not $BuildOnly) {
    if (-not (Test-Path $IsoPath)) {
        Write-Err "ISO not found: $IsoPath"
        Write-Err "Run without -RunOnly to build first."
        exit 1
    }

    Write-Step "Launching QEMU..."

    $qemuArgs = @(
        "-cdrom", $IsoPath
        "-boot", "d"
        "-m", "512M"
        "-smp", "4"
        "-no-reboot"
        "-no-shutdown"
        "-serial", "mon:stdio"
        "-serial", "pty"
        "-s"
    )

    # Add disk image if it exists
    if (Test-Path $DiskImage) {
        $qemuArgs += @("-drive", "format=raw,file=$DiskImage,if=ide")
        Write-OK "Disk: fat32.img attached"
    }

    $isoSize = [math]::Round((Get-Item $IsoPath).Length / 1MB, 1)
    Write-OK "ISO: $IsoPath ($isoSize MB)"
    Write-OK "RAM: 512M | CPUs: 4"
    Write-Host ""

    & $QemuBin @qemuArgs
}
