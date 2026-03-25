#!/bin/bash
# create_disk.sh — Crée une image disque FAT32 pour MaiOS
#
# Usage : ./tools/create_disk.sh [taille_en_MB]
#
# Depuis Windows (WSL) :
#   wsl bash tools/create_disk.sh

set -e

SIZE_MB=${1:-64}
IMG="fat32.img"

cd "$(dirname "$0")/.."

if [ -f "$IMG" ]; then
    echo "[OK] $IMG existe déjà ($(du -h "$IMG" | cut -f1))"
    echo "     Supprimez-le manuellement pour le recréer."
    exit 0
fi

echo "Création de $IMG (${SIZE_MB} MB)..."
dd if=/dev/zero of="$IMG" bs=1M count="$SIZE_MB" status=progress
mkfs.fat -F 32 -n MAIOS "$IMG"
echo "[OK] $IMG créé et formaté en FAT32"
