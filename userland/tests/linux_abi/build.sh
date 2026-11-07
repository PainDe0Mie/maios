#!/bin/bash
# Binaires Linux de test pour MaiProc (processus en mode utilisateur), à lancer dans WSL :
#   bash userland/tests/linux_abi/build.sh
# musl et BusyBox viennent des paquets Debian, téléchargés (sans sudo) et extraits dans
# ~/maios_pkgs ; les binaires vont dans extra_files/linux/ (visibles sous /extra_files/linux).
set -e
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
PKGS="$HOME/maios_pkgs"
SYS="$PKGS/root"
OUT="$ROOT/extra_files/linux"
mkdir -p "$PKGS" "$OUT"
if [ ! -f "$SYS/usr/lib/x86_64-linux-musl/libc.a" ] || [ ! -f "$SYS/usr/bin/busybox" ]; then
    (cd "$PKGS" && apt-get download musl musl-dev musl-tools busybox-static)
    mkdir -p "$SYS"
    for f in "$PKGS"/*.deb; do dpkg-deb -x "$f" "$SYS"; done
fi
# Spécification musl-gcc recalée sur l'arborescence extraite.
SPECS="$PKGS/musl-gcc.specs"
sed "s|/usr/lib/x86_64-linux-musl|$SYS/usr/lib/x86_64-linux-musl|g; s|/usr/include/x86_64-linux-musl|$SYS/usr/include/x86_64-linux-musl|g" \
    "$SYS/usr/lib/x86_64-linux-musl/musl-gcc.specs" > "$SPECS"
CC="gcc -specs $SPECS"
CFLAGS="-O2 -Wall -fno-stack-protector"
$CC $CFLAGS -static -o "$OUT/maitest" "$HERE/maitest.c"
$CC $CFLAGS -static-pie -fPIE -o "$OUT/maitest_pie" "$HERE/maitest.c"
strip "$OUT/maitest" "$OUT/maitest_pie"
cp "$SYS/usr/bin/busybox" "$OUT/busybox"
ls -la "$OUT"
readelf -h "$OUT/maitest" | grep -E "Type|Entry"
readelf -h "$OUT/maitest_pie" | grep -E "Type|Entry"
readelf -l "$OUT/busybox" | grep -E "interpreter|Elf file type" || true
