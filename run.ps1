<#
.SYNOPSIS
    Build MaiOS dans WSL (optionnel) puis lance l'ISO dans QEMU (PC moderne).

.DESCRIPTION
    Sans option, le script demande d'abord s'il faut rebuilder.
    Repondre Entree (ou "n") lance directement l'ISO existante.

    La VM est configuree comme un PC recent : machine q35, acceleration
    materielle (WHPX sous Windows, KVM sous WSL, sinon TCG multi-thread),
    CPU "host"/"max" (AVX, rdtscp, etc.), 8 Go de RAM, carte reseau Intel,
    et UEFI en option.

.PARAMETER Build
    Build puis lance, sans poser la question.

.PARAMETER BuildOnly
    Build uniquement, ne lance pas QEMU.

.PARAMETER RunOnly
    Lance directement l'ISO existante, sans poser la question.

.PARAMETER Debug
    Build en mode debug au lieu de release.

.PARAMETER QemuPath
    Chemin explicite vers qemu-system-x86_64.exe (sinon detection automatique).
    La variable d'environnement QEMU_BIN est aussi prise en compte.

.PARAMETER Ram
    Memoire de la VM (defaut 8G). Ex : -Ram 16G

.PARAMETER Cpus
    Nombre de coeurs de la VM (defaut 4). Ex : -Cpus 8

.PARAMETER Uefi
    Demarre en UEFI (OVMF) au lieu du BIOS legacy. L'ISO doit etre bootable en UEFI.

.PARAMETER NoAccel
    Desactive l'acceleration materielle (TCG pur, plus lent mais plus compatible).

.EXAMPLE
    .\run.ps1             # Demande s'il faut builder, puis lance
    .\run.ps1 -RunOnly    # Lance directement l'ISO
    .\run.ps1 -Build      # Build + lance, sans question
    .\run.ps1 -BuildOnly  # Build uniquement
    .\run.ps1 -RunOnly -Uefi -Ram 16G -Cpus 8
#>
param(
    [switch]$Build,
    [switch]$BuildOnly,
    [switch]$RunOnly,
    [switch]$Debug,
    [string]$QemuPath,
    [string]$Ram  = "8G",
    [int]$Cpus    = 4,
    [switch]$Uefi,
    [switch]$NoAccel
)

$ErrorActionPreference = "Stop"

# ── Chemins ──────────────────────────────────────────────────────────────────
$ProjectDir   = $PSScriptRoot
$IsoPath      = Join-Path $ProjectDir "build\MaiOS.iso"
$DiskImage    = Join-Path $ProjectDir "fat32.img"

# ── Affichage ────────────────────────────────────────────────────────────────
function Write-Step($msg)  { Write-Host "`n>> $msg" -ForegroundColor Cyan }
function Write-OK($msg)    { Write-Host "   $msg"   -ForegroundColor Green }
function Write-Warn($msg)  { Write-Host "   $msg"   -ForegroundColor Yellow }
function Write-Err($msg)   { Write-Host "   $msg"   -ForegroundColor Red }

function ConvertTo-WslPath($winPath) {
    return (wsl wslpath -u ($winPath -replace '\\','/')).Trim()
}

# ── Detection de QEMU ────────────────────────────────────────────────────────
# Retourne @{ Kind = "windows"|"wsl"; Path = ... } ou $null.
function Find-Qemu {
    $candidates = @(
        $QemuPath,
        $env:QEMU_BIN,
        (Get-Command qemu-system-x86_64.exe -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty Source),
        "$env:ProgramFiles\qemu\qemu-system-x86_64.exe",
        "${env:ProgramFiles(x86)}\qemu\qemu-system-x86_64.exe",
        "$env:USERPROFILE\scoop\apps\qemu\current\qemu-system-x86_64.exe",
        "C:\msys64\ucrt64\bin\qemu-system-x86_64.exe",
        "C:\msys64\mingw64\bin\qemu-system-x86_64.exe",
        "C:\qemu\qemu-system-x86_64.exe"
    )
    foreach ($c in $candidates) {
        if ($c -and (Test-Path $c)) { return @{ Kind = "windows"; Path = $c } }
    }

    # Repli : QEMU installe dans WSL (affichage via WSLg)
    $wslQemu = (wsl bash -lc 'command -v qemu-system-x86_64 2>/dev/null') | Select-Object -First 1
    if ($wslQemu) { return @{ Kind = "wsl"; Path = $wslQemu.Trim() } }

    return $null
}

function Show-QemuInstallHelp {
    Write-Err "QEMU introuvable (ni sous Windows, ni dans WSL)."
    Write-Host ""
    Write-Host "   Installer QEMU pour Windows (recommande) :" -ForegroundColor Gray
    Write-Host "       winget install --id SoftwareFreedomConservancy.QEMU" -ForegroundColor White
    Write-Host "   puis rouvrir le terminal (il s'installe dans C:\Program Files\qemu)." -ForegroundColor Gray
    Write-Host ""
    Write-Host "   Ou dans WSL :" -ForegroundColor Gray
    Write-Host "       wsl sudo apt install -y qemu-system-x86" -ForegroundColor White
    Write-Host ""
    Write-Host "   Ou indiquer un chemin : .\run.ps1 -QemuPath 'D:\...\qemu-system-x86_64.exe'" -ForegroundColor Gray
}

# ── Detection de l'acceleration materielle ───────────────────────────────────
# Retourne "whpx", "kvm" ou "tcg".
function Find-Accel($q) {
    if ($NoAccel) { return "tcg" }

    if ($q.Kind -eq "wsl") {
        wsl bash -lc '[ -r /dev/kvm ] && [ -w /dev/kvm ]' | Out-Null
        if ($LASTEXITCODE -eq 0) { return "kvm" }
        return "tcg"
    }

    # Windows : on teste WHPX en lancant une VM vide (necessite la
    # fonctionnalite "Plateforme d'hyperviseur Windows" activee).
    try {
        $p = Start-Process -FilePath $q.Path `
            -ArgumentList @("-machine","none","-accel","whpx","-display","none","-S") `
            -PassThru -WindowStyle Hidden
        if ($p.WaitForExit(2500)) {
            if ($p.ExitCode -eq 0) { return "whpx" }
            return "tcg"
        }
        $p.Kill()      # toujours en vie apres 2,5 s : l'accel fonctionne
        return "whpx"
    } catch {
        return "tcg"
    }
}

# ── Detection du firmware UEFI (OVMF) ────────────────────────────────────────
# Retourne le chemin (Windows ou WSL selon $q.Kind) ou $null.
function Find-Ovmf($q) {
    if ($q.Kind -eq "windows") {
        $dir = Split-Path $q.Path -Parent
        foreach ($rel in @("share\edk2-x86_64-code.fd", "..\share\qemu\edk2-x86_64-code.fd")) {
            $f = Join-Path $dir $rel
            if (Test-Path $f) { return (Resolve-Path $f).Path }
        }
        return $null
    }
    foreach ($f in @("/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_CODE.fd", "/usr/share/ovmf/OVMF.fd", "/usr/share/qemu/OVMF.fd")) {
        wsl bash -lc "[ -f '$f' ]" | Out-Null
        if ($LASTEXITCODE -eq 0) { return $f }
    }
    return $null
}

# ── Faut-il builder ? ────────────────────────────────────────────────────────
if ($BuildOnly -or $Build) {
    $doBuild = $true
} elseif ($RunOnly) {
    $doBuild = $false
} else {
    $isoInfo = if (Test-Path $IsoPath) {
        $iso = Get-Item $IsoPath
        "ISO existante : $([math]::Round($iso.Length / 1MB, 1)) Mo (compilée le $($iso.LastWriteTime.ToString('dd/MM/yyyy à HH:mm')))"
    } else {
        "Aucune ISO existante dans build\MaiOS.iso"
    }
    Write-Host ""
    Write-Host "   $isoInfo" -ForegroundColor DarkGray
    try {
        $answer = Read-Host "   Voulez-vous compiler MaiOS avant de lancer ? [o/N] (Entrée = lancer directement l'ISO)"
    } catch {
        $answer = ""   # session non interactive : on lance sans builder
    }
    $doBuild = $answer -match '^\s*(o|oui|y|yes)\s*$'
}

if (-not $doBuild -and -not (Test-Path $IsoPath)) {
    Write-Warn "Aucune ISO dans build\ : build obligatoire."
    $doBuild = $true
}

# Detecter QEMU tot pour prevenir avant un long build.
$qemu = $null
if (-not $BuildOnly) {
    $qemu = Find-Qemu
    if (-not $qemu) {
        Show-QemuInstallHelp
        if (-not $doBuild) { exit 1 }
        Write-Warn "Le build va quand meme se faire, mais QEMU ne pourra pas etre lance."
    }
}

# ── Build (dans WSL) ─────────────────────────────────────────────────────────
if ($doBuild) {
    Write-Step "Build de MaiOS dans WSL..."

    $WslProject = ConvertTo-WslPath $ProjectDir
    $buildMode  = if ($Debug) { "debug" } else { "release" }

    # bash -lc : shell de connexion, pour que ~/.cargo/bin soit dans le PATH.
    wsl bash -lc "command -v nasm >/dev/null 2>&1 || { echo '  Installation de nasm...'; sudo apt-get install -y nasm >/dev/null 2>&1; }"

    $buildCmd = "cd '$WslProject' && make iso BUILD_MODE=$buildMode 2>&1"
    Write-Host "   Commande : make iso BUILD_MODE=$buildMode" -ForegroundColor DarkGray

    $output = wsl bash -lc $buildCmd
    $exitCode = $LASTEXITCODE

    # Affiche les 30 dernieres lignes du build
    $lines = $output -split "`n"
    if ($lines.Count -gt 30) {
        Write-Host "   ... ($($lines.Count - 30) lignes masquees)" -ForegroundColor DarkGray
    }
    $lines | Select-Object -Last 30 | ForEach-Object { Write-Host "   $_" -ForegroundColor DarkGray }

    if ($exitCode -ne 0) {
        Write-Err "Echec du build (code $exitCode)"
        exit 1
    }
    Write-OK "Build OK"
}

# ── Lancement de QEMU ────────────────────────────────────────────────────────
if ($BuildOnly) { exit 0 }

if (-not (Test-Path $IsoPath)) {
    Write-Err "ISO introuvable : $IsoPath"
    exit 1
}
if (-not $qemu) { exit 1 }

Write-Step "Lancement de QEMU..."

$useWsl = $qemu.Kind -eq "wsl"
$isoArg  = if ($useWsl) { ConvertTo-WslPath $IsoPath } else { $IsoPath }

# Acceleration + modele de CPU
$accel = Find-Accel $qemu
switch ($accel) {
    "whpx" { $accelArg = "whpx,kernel-irqchip=off"; $cpuModel = "max"  }
    "kvm"  { $accelArg = "kvm";                     $cpuModel = "host" }
    default { $accelArg = "tcg,thread=multi";       $cpuModel = "max"  }
}
if ($accel -eq "tcg" -and -not $NoAccel) {
    Write-Warn "Pas d'acceleration materielle (WHPX/KVM indisponible) : la VM sera lente."
    if (-not $useWsl) {
        Write-Warn "Windows : activer 'Plateforme d'hyperviseur Windows' dans les fonctionnalites facultatives."
    }
}

$qemuArgs = @(
    "-name", "MaiOS"
    "-machine", "q35,hpet=on"
    "-accel", $accelArg
    # CPU recent : rdtscp, AVX, AVX2, BMI, etc. (le qemu64 par defaut est un CPU de 2005).
    "-cpu", $cpuModel
    "-smp", "$Cpus"
    "-m", $Ram
    "-cdrom", $isoArg
    "-boot", "d"
    # Horloge RTC a l'heure locale (sinon UTC) pour l'horloge du bureau.
    "-rtc", "base=localtime"
    "-no-reboot"
    "-no-shutdown"
    "-serial", "stdio"
    "-serial", "null"
    # Carte graphique avec 64 Mo de VRAM (assez pour du 4K 32 bits).
    "-vga", "std"
    "-global", "VGA.vgamem_mb=64"
    "-s"
    # Audio Intel HDA
    "-device", "intel-hda,id=hda0,msi=off"
    "-device", "hda-output,bus=hda0.0"
    # Reseau : carte Intel e1000 en NAT (acces internet depuis la VM)
    "-nic", "user,model=e1000"
)

# UEFI (OVMF) en option
if ($Uefi) {
    $ovmf = Find-Ovmf $qemu
    if ($ovmf) {
        $qemuArgs += @("-drive", "if=pflash,format=raw,readonly=on,file=$ovmf")
        Write-OK "Firmware : UEFI (OVMF)"
    } else {
        Write-Warn "OVMF introuvable : demarrage en BIOS legacy."
        Write-Warn "Dans WSL : sudo apt install -y ovmf"
    }
}

# Image disque si elle existe (q35 = controleur AHCI/SATA moderne)
if (Test-Path $DiskImage) {
    $diskArg = if ($useWsl) { ConvertTo-WslPath $DiskImage } else { $DiskImage }
    $qemuArgs += @("-drive", "format=raw,file=$diskArg,if=ide")
    Write-OK "Disque : fat32.img attache"
}

$isoSize = [math]::Round((Get-Item $IsoPath).Length / 1MB, 1)
Write-OK "QEMU : $($qemu.Path) ($($qemu.Kind))"
Write-OK "ISO  : $IsoPath ($isoSize Mo)"
Write-OK "Accel: $accel | CPU : $cpuModel"
Write-OK "RAM  : $Ram | CPUs : $Cpus | Audio : HDA | Reseau : e1000 | Clavier : PS/2"
Write-Host ""

if ($useWsl) {
    $quoted = ($qemuArgs | ForEach-Object { "'" + ($_ -replace "'", "'\''") + "'" }) -join ' '
    wsl bash -lc "$($qemu.Path) $quoted"
} else {
    & $qemu.Path @qemuArgs
}