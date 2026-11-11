#!/usr/bin/env bash
# Construit les clients Wayland de test de MaiLand (glibc dynamique, libwayland-client de
# Debian, comme les vraies applications), sans droits administrateur :
#   * en-têtes et `wayland-scanner` : `libwayland-dev` et `libwayland-bin` téléchargés
#     (`apt-get download`, signatures vérifiées) et extraits dans ~/maios_pkgs/wldev ;
#   * code de liaison de xdg-shell généré depuis le XML que MaiLand utilise
#     (`kernel/gfx/wl_proto/protocols/xdg-shell.xml`) ;
#   * en-têtes de libxkbcommon (`libxkbcommon-dev`, même méthode, ~/maios_pkgs/xkb) : `wlinput`
#     compile la carte clavier reçue du serveur avec la bibliothèque des vraies applications ;
#   * `wltime` (phase W3) : presentation-time, fifo, commit-timing, viewporter,
#     fractional-scale, single-pixel-buffer ;
#   * `wlsub` (phase W4) : sous-surfaces, xdg-activation ;
#   * `wlsec` (phase W4) : security-context-v1, sélection primaire ;
#   * édition de liens avec `libwayland-client.so.0` et `libxkbcommon.so.0` installées dans WSL.
# `mkrootfs.sh` copie ensuite `build/wayland/wl*` dans `/opt/maios` avec leurs bibliothèques.
#
#   wsl bash userland/tests/wayland/build.sh
#   wsl bash scripts/mkrootfs.sh
#   python scripts/maios_bench.py --kvm --tests wayland
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "$0")/../../.." && pwd)
PK=${WL_DEV_DIR:-$HOME/maios_pkgs/wldev}
OUT=$ROOT_DIR/build/wayland
mkdir -p "$PK" "$OUT"

(
  cd "$PK"
  for p in libwayland-dev libwayland-bin; do
    ls "${p}"_*.deb > /dev/null 2>&1 || apt-get download "$p" > /dev/null
  done
  if [ ! -x x/usr/bin/wayland-scanner ]; then
    rm -rf x && mkdir x
    for d in *.deb; do dpkg-deb -x "$d" x; done
  fi
)

XK=${XKB_DEV_DIR:-$HOME/maios_pkgs/xkb}
mkdir -p "$XK"
(
  cd "$XK"
  ls libxkbcommon-dev_*.deb > /dev/null 2>&1 || apt-get download libxkbcommon-dev > /dev/null
  if [ ! -f root/usr/include/xkbcommon/xkbcommon.h ]; then
    for d in ./*.deb; do dpkg-deb -x "$d" root; done
  fi
)

SCANNER="$PK/x/usr/bin/wayland-scanner"
PROTOS=$ROOT_DIR/kernel/gfx/wl_proto/protocols
GEN=()
GEN3=()
W3=(presentation-time fifo-v1 commit-timing-v1 viewporter fractional-scale-v1 single-pixel-buffer-v1)
for p in xdg-shell xdg-decoration-unstable-v1 cursor-shape-v1 xdg-activation-v1 primary-selection-unstable-v1 \
  security-context-v1 "${W3[@]}"; do
  "$SCANNER" client-header "$PROTOS/$p.xml" "$OUT/$p-client-protocol.h"
  "$SCANNER" private-code "$PROTOS/$p.xml" "$OUT/$p-protocol.c"
  case " ${W3[*]} " in
    *" $p "*) GEN3+=("$OUT/$p-protocol.c") ;;
    *) GEN+=("$OUT/$p-protocol.c") ;;
  esac
done

CFLAGS=(-O2 -Wall -Wextra -Werror -I"$PK/x/usr/include" -I"$XK/root/usr/include" -I"$OUT")
gcc "${CFLAGS[@]}" -o "$OUT/wlhello" "$ROOT_DIR/userland/tests/wayland/wlhello.c" "$OUT/xdg-shell-protocol.c" \
  "$OUT/xdg-decoration-unstable-v1-protocol.c" \
  -l:libwayland-client.so.0
gcc "${CFLAGS[@]}" -o "$OUT/wlinput" "$ROOT_DIR/userland/tests/wayland/wlinput.c" "${GEN[@]}" \
  -l:libwayland-client.so.0 -l:libxkbcommon.so.0
gcc "${CFLAGS[@]}" -o "$OUT/wltime" "$ROOT_DIR/userland/tests/wayland/wltime.c" "$OUT/xdg-shell-protocol.c" "${GEN3[@]}" \
  "$OUT/xdg-decoration-unstable-v1-protocol.c"   -l:libwayland-client.so.0
gcc "${CFLAGS[@]}" -o "$OUT/wlsub" "$ROOT_DIR/userland/tests/wayland/wlsub.c" "$OUT/xdg-shell-protocol.c" \
  "$OUT/xdg-decoration-unstable-v1-protocol.c" \
  "$OUT/presentation-time-protocol.c" "$OUT/xdg-activation-v1-protocol.c" -l:libwayland-client.so.0
gcc "${CFLAGS[@]}" -o "$OUT/wlsec" "$ROOT_DIR/userland/tests/wayland/wlsec.c"   "$OUT/primary-selection-unstable-v1-protocol.c" "$OUT/security-context-v1-protocol.c" -l:libwayland-client.so.0
for c in wlhello wlinput wltime wlsub wlsec; do
  echo "client : $OUT/$c ($(stat -c %s "$OUT/$c") octets)"
done
