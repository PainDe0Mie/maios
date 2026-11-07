#!/bin/bash
# ISO -> libc (tlibc) -> Doom -> extra_files -> ISO, dans cet ordre pour que les empreintes des
# symboles du noyau vues par Doom correspondent à celles de l'ISO finale.
# Usage (WSL) : bash scripts/rebuild_all.sh
set -e
ROOT=$(cd "$(dirname "$0")/.." && pwd)
LOGS=$(mktemp -d)
cd "$ROOT"
make iso BUILD_MODE=release > "$LOGS/iso1.log" 2>&1 || { grep -B2 -A12 -E "^error|Error" "$LOGS/iso1.log" | head -80; exit 1; }
echo "ISO 1 OK"
cd "$ROOT/userland/libc"
sh build.sh > "$LOGS/tlibc.log" 2>&1 || { tail -40 "$LOGS/tlibc.log"; exit 1; }
echo "libc OK"
cd "$ROOT/userland/ports/doom"
rm -f doom.elf
make WAD_SOURCE="$ROOT/assets/DOOM1.WAD" 2>&1 | grep -E "error|undefined|successfully" | sort | uniq -c
cd "$ROOT"
strip --strip-debug -o extra_files/doom.elf.new userland/ports/doom/doom.elf
mv -f extra_files/doom.elf.new extra_files/doom.elf
cp -f assets/DOOM1.WAD extra_files/DOOM1.WAD
make iso BUILD_MODE=release > "$LOGS/iso2.log" 2>&1 || { grep -B2 -A12 -E "^error|Error" "$LOGS/iso2.log" | head -80; exit 1; }
echo "ISO 2 OK"
echo -n "hash doom : "; nm userland/ports/doom/doom.elf | grep -o "_ZN13maios_syscall8dispatch[A-Za-z0-9_]*" | sort -u
echo -n "hash noyau: "; nm build/isofiles/modules/*maios_syscall-* | grep -o "_ZN13maios_syscall8dispatch[A-Za-z0-9_]*" | sort -u
ls -la build/MaiOS.iso extra_files/doom.elf
rm -rf "$LOGS"
