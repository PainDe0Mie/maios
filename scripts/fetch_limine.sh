#!/usr/bin/env bash
# Récupère et compile Limine, le chargeur d'amorçage de MaiOS, dans tools/limine/bin.
#
# Version épinglée et archive vérifiée par SHA-256. Compilée dans un dossier temporaire du système
# de fichiers Linux (bien plus rapide que /mnt/... sous WSL), avec les cibles BIOS (CD et disque)
# et UEFI x86_64 (CD et disque). Prérequis : gcc, make, nasm, mtools, curl.
set -euo pipefail

VERSION=12.9.6
SHA256=6a4477636e108bcaaa88d7749f43dc3d3ba09a7d548244f3b1e847d27bc1b9a4
URL="https://github.com/limine-bootloader/limine/releases/download/v${VERSION}/limine-${VERSION}.tar.xz"

ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT="$ROOT/tools/limine/bin"

if [ -x "$OUT/limine" ] && [ "$(cat "$OUT/VERSION" 2>/dev/null)" = "$VERSION" ]; then
    echo "Limine $VERSION deja present dans $OUT"
    exit 0
fi

for tool in gcc make nasm mformat curl; do
    command -v "$tool" > /dev/null || { echo "fetch_limine : '$tool' manque (apt install build-essential nasm mtools curl)"; exit 1; }
done

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

echo "Telechargement de Limine $VERSION..."
curl -fsSL -o "$WORK/limine.tar.xz" "$URL"
echo "$SHA256  $WORK/limine.tar.xz" | sha256sum -c --quiet

tar -xf "$WORK/limine.tar.xz" -C "$WORK"
cd "$WORK/limine-$VERSION"
echo "Compilation de Limine $VERSION (BIOS + UEFI x86_64)..."
# Limine prend clang par défaut ; la chaîne GNU fait aussi l'affaire pour x86.
TOOLCHAIN=()
if ! command -v clang > /dev/null; then
    TOOLCHAIN=(CC_FOR_TARGET=gcc LD_FOR_TARGET=ld OBJCOPY_FOR_TARGET=objcopy
               OBJDUMP_FOR_TARGET=objdump READELF_FOR_TARGET=readelf)
fi
./configure --enable-bios --enable-bios-cd --enable-uefi-x86-64 --enable-uefi-cd "${TOOLCHAIN[@]}" \
    > "$WORK/configure.log" 2>&1 || { tail -20 "$WORK/configure.log"; exit 1; }
make -j"$(nproc)" > "$WORK/make.log" 2>&1 || { tail -30 "$WORK/make.log"; exit 1; }

mkdir -p "$OUT"
cp bin/limine-bios.sys bin/limine-bios-cd.bin bin/limine-uefi-cd.bin bin/BOOTX64.EFI bin/limine "$OUT/"
cp COPYING "$OUT/LICENSE" 2> /dev/null || true
echo "$VERSION" > "$OUT/VERSION"
echo "Limine $VERSION installe dans $OUT"
