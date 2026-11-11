#!/usr/bin/env bash
# Régénère les cartes XKB embarquées par MaiLand (wl_keyboard.keymap), à lancer dans WSL :
#   wsl bash kernel/gfx/mailand/keymaps/gen.sh
# `xkbcli compile-keymap` (libxkbcommon-tools, extrait sans root) compile les règles evdev de
# xkb-data (MIT) en une carte complète au format texte v1, celle que libxkbcommon des clients
# relit avec xkb_keymap_new_from_string. Une carte par disposition de MaiOS.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
pkgs="$HOME/maios_pkgs/xkb"
mkdir -p "$pkgs"
if [ ! -x "$pkgs/root/usr/bin/xkbcli" ]; then
    (cd "$pkgs" && apt-get download libxkbcommon-tools libxkbcommon-dev libxkbcommon0 >/dev/null)
    for d in "$pkgs"/*.deb; do dpkg-deb -x "$d" "$pkgs/root"; done
fi
tool="$(find "$pkgs/root" -name 'xkbcli-compile-keymap' | head -n1)"
[ -n "$tool" ] || { echo "xkbcli-compile-keymap introuvable" >&2; exit 1; }
export LD_LIBRARY_PATH="$pkgs/root/usr/lib/x86_64-linux-gnu"
for layout in us fr; do
    "$tool" --rules evdev --model pc105 --layout "$layout" > "$here/$layout.xkb"
    echo "$layout.xkb : $(wc -c < "$here/$layout.xkb") octets"
done
