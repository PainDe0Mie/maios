# create_disk.ps1 — Crée une image disque FAT32 pour MaiOS (Windows)
#
# Usage : powershell -ExecutionPolicy Bypass -File tools\create_disk.ps1
#
# Crée fat32.img (64 MB) formaté en FAT32 à la racine du repo.

param(
    [int]$SizeMB = 64,
    [string]$OutputFile = "fat32.img"
)

$ErrorActionPreference = "Stop"

# Aller à la racine du repo (parent de tools/)
$repoRoot = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$imgPath = Join-Path $repoRoot $OutputFile

if (Test-Path $imgPath) {
    Write-Host "[OK] $OutputFile existe deja ($([math]::Round((Get-Item $imgPath).Length / 1MB)) MB)" -ForegroundColor Green
    Write-Host "     Supprimez-le manuellement si vous voulez le recreer."
    exit 0
}

Write-Host "Creation de $OutputFile ($SizeMB MB)..." -ForegroundColor Cyan

# 1. Creer le fichier vide
$sizeBytes = $SizeMB * 1MB
$fs = [System.IO.File]::Create($imgPath)
$fs.SetLength($sizeBytes)
$fs.Close()
Write-Host "  Fichier cree : $imgPath"

# 2. Formater en FAT32 via diskpart + VHD mount
# On utilise diskpart pour attacher le fichier comme VHD, le partitionner et formater

$diskpartScript = @"
create vdisk file="$imgPath" maximum=$SizeMB type=fixed
select vdisk file="$imgPath"
attach vdisk
create partition primary
format fs=fat32 quick label="MAIOS"
assign letter=Z
detach vdisk
"@

$tempScript = [System.IO.Path]::GetTempFileName()
$diskpartScript | Out-File -FilePath $tempScript -Encoding ASCII

Write-Host "  Formatage FAT32 via diskpart..." -ForegroundColor Cyan
$result = Start-Process -FilePath "diskpart" -ArgumentList "/s $tempScript" -Wait -PassThru -Verb RunAs -WindowStyle Hidden

Remove-Item $tempScript -Force

if ($result.ExitCode -eq 0) {
    Write-Host "[OK] $OutputFile cree et formate en FAT32 ($SizeMB MB)" -ForegroundColor Green
} else {
    Write-Host "[ERREUR] diskpart a echoue (code $($result.ExitCode))" -ForegroundColor Red
    Write-Host "         Essayez via WSL : dd if=/dev/zero of=fat32.img bs=1M count=64 && mkfs.fat -F 32 fat32.img"
    exit 1
}
