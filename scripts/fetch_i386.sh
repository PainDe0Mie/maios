#!/usr/bin/env bash
# Plan 07 (Steam), S2 : paquets i386 de Debian, sans root ni `dpkg --add-architecture`.
# Une configuration apt privée (`APT::Architectures` amd64 + i386, listes et cache dans
# ~/maios_pkgs/apt-i386) télécharge les index signés du dépôt de WSL (signatures vérifiées par
# apt avec le trousseau de Debian), puis :
#   * bibliothèques i386 d'exécution (`libc6:i386`…) -> ~/maios_pkgs/debs (extraites dans
#     l'image par `mkrootfs.sh`, comme les autres paquets téléchargés) ;
#   * multilib pour `gcc -m32` (`libc6-dev-i386`, `lib32gcc-14-dev`…) -> extraites dans
#     ~/maios_pkgs/multilib (utilisées par `userland/tests/linux_abi/build.sh`), et la glibc i386
#     extraite dans ~/maios_pkgs/i386 (sysroot des bancs).
#
#   wsl bash scripts/fetch_i386.sh
set -euo pipefail

RUNTIME=${RUNTIME:-"libc6:i386 libgcc-s1:i386"}
GCCV=$(gcc -dumpversion | cut -d. -f1)
MULTILIB=${MULTILIB:-"libc6-dev-i386 libc6-i386 lib32gcc-${GCCV}-dev lib32gcc-s1"}
P=$HOME/maios_pkgs
A=$P/apt-i386
DEST=${DEBS_DIR:-$P/debs}
mkdir -p "$A/lists/partial" "$A/cache/archives/partial" "$A/multilib-debs" "$DEST"
: > "$A/status"
APT=(-o Dir::State::Lists="$A/lists" -o Dir::Cache="$A/cache" -o Dir::State::status="$A/status"
  -o APT::Architectures::=amd64 -o APT::Architectures::=i386 -o Debug::NoLocking=1)

apt-get "${APT[@]}" update > /dev/null
du -sh "$A/lists" | sed 's/^/index : /'

(cd "$DEST" && rm -f libc6_*_i386.deb libgcc-s1_*_i386.deb && apt-get "${APT[@]}" download $RUNTIME > /dev/null)
for p in $RUNTIME; do echo "${p}" >> "$DEST/LIST"; done
sort -u -o "$DEST/LIST" "$DEST/LIST"

(cd "$A/multilib-debs" && rm -f ./*.deb && apt-get "${APT[@]}" download $MULTILIB > /dev/null)

# Arborescences extraites (sans scripts d'installation).
rm -rf "$P/multilib" "$P/i386"
mkdir -p "$P/multilib" "$P/i386"
for deb in "$A"/multilib-debs/*.deb; do dpkg-deb -x "$deb" "$P/multilib"; done
# Le script `libc.so` désigne `/lib32/libc.so.6` et `/lib/ld-linux.so.2` (racine fusionnée) :
# liens dans l'arborescence, que `ld --sysroot` préfixe.
ln -s usr/lib32 "$P/multilib/lib32"
mkdir -p "$P/multilib/lib"
ln -s ../usr/lib32/ld-linux.so.2 "$P/multilib/lib/ld-linux.so.2"
for p in $RUNTIME; do
  n=${p%%:*}
  dpkg-deb -x "$(ls "$DEST/${n}_"*_i386.deb)" "$P/i386"
done
ls -l "$DEST"/*_i386.deb "$A"/multilib-debs/*.deb | awk '{ s += $5; print $5, $9 } END { print s, "octets au total" }'
