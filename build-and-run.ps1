<#
.SYNOPSIS
    Build MaiOS and launch in QEMU - with auto-setup

.PARAMETER RunOnly
    Skip build, launch QEMU with existing ISO.

.PARAMETER BuildOnly
    Only build, don't launch QEMU.

.EXAMPLE
    .\build-and-run.ps1          # Build + Run (default)
    .\build-and-run.ps1 -RunOnly # Just launch QEMU
    .\build-and-run.ps1 -BuildOnly # Just build
#>
param(
    [switch]$RunOnly,
    [switch]$BuildOnly
)

$ErrorActionPreference = "Stop"

# ── Colors ───────────────────────────────────────────────────────────────────
function Write-Step($msg) { Write-Host "`n>> $msg" -ForegroundColor Cyan }
function Write-OK($msg) { Write-Host "   [OK] $msg"   -ForegroundColor Green }
function Write-Warn($msg) { Write-Host "   [!] $msg"   -ForegroundColor Yellow }
function Write-Err($msg) { Write-Host "   [ERROR] $msg"   -ForegroundColor Red }
function Write-Info($msg) { Write-Host "   ... $msg"   -ForegroundColor DarkGray }

# ── Paths ────────────────────────────────────────────────────────────────────
$ProjectDir = Split-Path $PSScriptRoot -Leaf
$WorkDir = $PSScriptRoot
$QemuBin = "C:\Program Files\qemu\qemu-system-x86_64.exe"
$IsoPath = Join-Path $WorkDir "build\MaiOS.iso"
$DiskImage = Join-Path $WorkDir "fat32.img"
$NasmPath = "C:\Program Files\NASM"

Write-Host "========================================" -ForegroundColor Cyan
Write-Host "     MaiOS - Build & Run Launcher       " -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Cyan

# ── Pre-flight checks ────────────────────────────────────────────────────────
Write-Step "Checking dependencies..."

if (-not (Test-Path $NasmPath)) {
    Write-Err "NASM not found at: $NasmPath"
    Write-Info "Download from: https://www.nasm.us/"
    exit 1
}
Write-OK "NASM: $NasmPath"

if (-not (Test-Path $QemuBin)) {
    Write-Err "QEMU not found at: $QemuBin"
    Write-Info "Download from: https://www.qemu.org/"
    exit 1
}
Write-OK "QEMU: $QemuBin"

# Check WSL
$wslCheck = wsl --list --verbose 2>&1
if ($LASTEXITCODE -ne 0) {
    Write-Err "WSL not available"
    Write-Info "Install WSL: wsl --install"
    exit 1
}
Write-OK "WSL: Available"

# ── Build (in WSL) ───────────────────────────────────────────────────────────
if (-not $RunOnly) {
    Write-Step "Building MaiOS..."

    $wslProjectPath = wsl wslpath -u ($WorkDir -replace '\\', '/')

    Write-Info "Running: make iso"
    Write-Info ""

    wsl bash -lc "cd '$wslProjectPath' && make iso"
    $exitCode = $LASTEXITCODE

    if ($exitCode -ne 0) {
        Write-Host ""
        Write-Err "Build failed (exit code $exitCode)"
        exit 1
    }

    Write-OK "Build successful"
}

# ── Launch QEMU ──────────────────────────────────────────────────────────────
if (-not $BuildOnly) {
    if (-not (Test-Path $IsoPath)) {
        Write-Err "ISO not found: $IsoPath"
        Write-Warn "Run without -RunOnly to build first"
        exit 1
    }

    Write-Step "Launching QEMU..."

    $isoSize = [math]::Round((Get-Item $IsoPath).Length / 1MB, 1)
    Write-Info "ISO: $IsoPath ($isoSize MB)"
    Write-Info "RAM: 4 GB"
    Write-Info "CPUs: 4 (SMP)"
    Write-Info "Machine: Q35"
    Write-Info "Boot: CD-ROM"

    if (Test-Path $DiskImage) {
        $diskSize = [math]::Round((Get-Item $DiskImage).Length / 1MB, 1)
        Write-Info "Disk: fat32.img ($diskSize MB)"
    }

    Write-Host ""
    Write-Host "Starting QEMU..." -ForegroundColor Green
    Write-Host "Serial output below (this window)." -ForegroundColor DarkGray
    Write-Host "Monitor QEMU : ouvre un autre PowerShell et lance .\qemu-monitor.ps1" -ForegroundColor DarkGray
    Write-Host "Quitter QEMU : .\qemu-monitor.ps1 -Cmd quit" -ForegroundColor DarkGray
    Write-Host ""

    $qemuArgs = @(
        "-M", "q35"
        "-cdrom", $IsoPath
        "-boot", "d"
        "-cpu", "Broadwell"
        "-m", "4G"
        "-smp", "4"
        "-no-reboot"
        "-no-shutdown"
        "-serial", "stdio"
        "-serial", "null"
        "-monitor", "telnet:localhost:55555,server,nowait"
        "-net", "none"
        "-display", "sdl,gl=on"
        "-s"
        "-device", "intel-hda,id=hda0,msi=off"
        "-device", "hda-output,bus=hda0.0"
        # "-device", "virtio-gpu-pci"
    )

    if (Test-Path $DiskImage) {
        $qemuArgs += @("-drive", "format=raw,file=$DiskImage,if=ide")
    }

    & $QemuBin @qemuArgs
}

Write-Host ""
Write-OK "Build and run completed"
