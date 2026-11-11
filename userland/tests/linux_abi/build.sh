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
# Jalon M2 : processus, signaux, tubes, fils (pthread de musl).
$CC $CFLAGS -static -pthread -o "$OUT/proctest" "$HERE/proctest.c"
# Jalon M3 : vDSO, epoll, eventfd/timerfd/signalfd, pidfd, sockets Unix, /proc ; deux fois :
# musl statique, et glibc dynamique (chargée par /lib64/ld-linux-x86-64.so.2).
$CC $CFLAGS -static -o "$OUT/m3test" "$HERE/m3test.c"
gcc $CFLAGS -o "$OUT/m3test_glibc" "$HERE/m3test.c"
# Suite de M3 : sockets TCP/UDP (IPv4, IPv6, double pile) et io_uring.
$CC $CFLAGS -static -o "$OUT/m3net" "$HERE/m3net.c"
gcc $CFLAGS -o "$OUT/m3net_glibc" "$HERE/m3net.c"
# inotify, minuteurs POSIX, priorités, renameat2, FIONREAD UDP (clients réels : GLib, Firefox).
$CC $CFLAGS -static -o "$OUT/m4test" "$HERE/m4test.c"
gcc $CFLAGS -o "$OUT/m4test_glibc" "$HERE/m4test.c"
# Jalon S3 du plan 07 (Steam) : capacités, espaces de noms, montages, pivot_root.
$CC $CFLAGS -static -o "$OUT/nstest" "$HERE/nstest.c"
# Jalon S2 du plan 07 (Steam) : ABI i386, programme 32 bits autonome (sans libc, `int 0x80`).
gcc -m32 -msse2 -O2 -Wall -ffreestanding -fno-pic -no-pie -fno-stack-protector -fno-asynchronous-unwind-tables     -nostdlib -static -o "$OUT/ia32test" "$HERE/ia32test.c"
# Les mêmes bancs que M3/M4 en i386 contre la glibc i386 (/lib/ld-linux.so.2), si les paquets
# de `scripts/fetch_i386.sh` sont là : multilib (en-têtes `stubs-32.h`, crt*.o, libgcc 32 bits)
# extraite dans ~/maios_pkgs/multilib, liée par `ld --sysroot` (chemins absolus de `libc.so`).
ML="$PKGS/multilib"
I386="$PKGS/i386/usr/lib/i386-linux-gnu"
I386_BINS=()
if [ -f "$ML/usr/lib32/crt1.o" ] && [ -f "$I386/libc.so.6" ]; then
    GV=$(gcc -dumpversion | cut -d. -f1)
    CC32="gcc -m32 -B$ML/usr/lib32/ -B$ML/usr/lib/gcc/x86_64-linux-gnu/$GV/32/ -L$ML/usr/lib32
        -L$ML/usr/lib/gcc/x86_64-linux-gnu/$GV/32 -idirafter /usr/include/x86_64-linux-gnu
        -idirafter $ML/usr/include/x86_64-linux-gnu -Wl,--sysroot=$ML"
    for t in m3test m3net m4test; do
        $CC32 $CFLAGS -o "$OUT/${t}_i386" "$HERE/$t.c"
        I386_BINS+=("$OUT/${t}_i386")
    done
else
    echo "glibc i386 absente (wsl bash scripts/fetch_i386.sh) : pas de m3test_i386, m3net_i386, m4test_i386"
    rm -f "$OUT"/m3test_i386 "$OUT"/m3net_i386 "$OUT"/m4test_i386
fi
strip "$OUT/maitest" "$OUT/maitest_pie" "$OUT/proctest" "$OUT/m3test" "$OUT/m3net" "$OUT/m4test" "$OUT/ia32test"
# Sysroot Linux (visible à la racine de MaiOS : /lib64, /lib, /usr…) : chargeur dynamique et
# bibliothèques de la glibc du système, plus des programmes Debian dynamiques (glibc/).
SR="$ROOT/extra_files/sysroot"
rm -rf "$SR"
mkdir -p "$OUT/glibc"
for bin in /usr/bin/ls /usr/bin/dash /usr/bin/cat; do cp -L "$bin" "$OUT/glibc/"; done
for bin in "$OUT/m3test_glibc" "$OUT/m3net_glibc" "$OUT/m4test_glibc" "$OUT"/glibc/*; do
    for lib in $(ldd "$bin" | grep -o '/[^ ]*'); do
        mkdir -p "$SR$(dirname "$lib")"
        cp -L "$lib" "$SR$lib"
    done
done
# Glibc i386 (pas de `ldd` 32 bits dans WSL) : chargeur et bibliothèques de `libc6:i386` et
# `libgcc-s1:i386`, sans les modules `gconv`.
if [ ${#I386_BINS[@]} -gt 0 ]; then
    mkdir -p "$SR/lib/i386-linux-gnu"
    cp -L "$PKGS/i386/usr/lib/ld-linux.so.2" "$SR/lib/ld-linux.so.2"
    for lib in "$I386"/*.so*; do cp -L "$lib" "$SR/lib/i386-linux-gnu/"; done
fi
# Résolution de noms : /etc/hosts d'abord (getaddrinfo de musl et de la glibc), puis le DNS de
# QEMU (réseau utilisateur, 10.0.2.3).
mkdir -p "$SR/etc"
cat > "$SR/etc/hosts" <<'HOSTS'
127.0.0.1	localhost
::1	localhost ip6-localhost ip6-loopback
127.0.1.1	maios
HOSTS
cat > "$SR/etc/nsswitch.conf" <<'NSS'
hosts: files dns
networks: files
services: files
protocols: files
NSS
echo "nameserver 10.0.2.3" > "$SR/etc/resolv.conf"
du -sh "$SR"
cp "$HERE/m2.sh" "$OUT/m2.sh"
cp "$SYS/usr/bin/busybox" "$OUT/busybox"
ls -la "$OUT"
readelf -h "$OUT/maitest" | grep -E "Type|Entry"
readelf -h "$OUT/maitest_pie" | grep -E "Type|Entry"
readelf -l "$OUT/busybox" | grep -E "interpreter|Elf file type" || true
