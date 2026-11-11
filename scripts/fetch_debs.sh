#!/usr/bin/env bash
# Télécharge (sans les installer) les paquets Debian que `mkrootfs.sh` ajoute à l'image et qui
# ne sont pas installés dans la Debian de WSL : leurs dépendances complètes, moins celles déjà
# présentes (copiées depuis WSL par `mkrootfs.sh`). `apt-get download` vérifie les signatures
# du dépôt. Aucun droit administrateur : les listes d'apt doivent déjà être à jour
# (`sudo apt-get update` sinon).
#
#   wsl bash scripts/fetch_debs.sh            # -> ~/maios_pkgs/debs/*.deb
#   EXTRA_PKGS="python3 libpython3.13-testsuite" wsl bash scripts/fetch_debs.sh
set -euo pipefail

# Clients Wayland réels de la phase W4 de MaiLand : terminal foot, démos de GTK 4, programmes de
# test de SDL 3, exemples et greffon Wayland de Qt 6, Firefox ESR. Plan 07 (Steam), S1 : Xwayland et
# clients X11 de test (xeyes, xterm, xev, glxgears).
EXTRA_PKGS=${EXTRA_PKGS:-"python3 python3-venv libpython3.13-testsuite wayland-utils   foot gtk-4-examples libsdl3-tests qt6-wayland qt6-base-examples firefox-esr   xwayland x11-apps x11-utils x11-xkb-utils xterm mesa-utils xauth   bubblewrap"}
# Choisis par apt pour satisfaire `debconf | debconf-2.0` alors que debconf est installé, ou
# inutiles dans l'image.
SKIP=${SKIP:-"cdebconf libdebian-installer4 libtextwrap1"}
DEST=${DEBS_DIR:-$HOME/maios_pkgs/debs}
mkdir -p "$DEST"
cd "$DEST"

installed() { dpkg-query -W -f='${Status}' "$1" 2> /dev/null | grep -q "install ok installed"; }

# Fermeture des dépendances (les paquets virtuels `<...>` et les alternatives non choisies
# sont ignorés : on ne garde que les noms qui ont une version candidate).
mapfile -t all < <(apt-cache depends --recurse --no-recommends --no-suggests --no-conflicts \
  --no-breaks --no-replaces --no-enhances $EXTRA_PKGS | grep '^[a-z0-9]' | sort -u)
need=()
have=()
for p in "${all[@]}"; do
  if installed "$p"; then have+=("$p"); continue; fi
  case " $SKIP " in *" $p "*) continue ;; esac
  [ -n "$(apt-cache policy "$p" | awk '/Candidate:/ { if ($2 != "(none)") print $2 }')" ] || continue
  need+=("$p")
done
echo "paquets a telecharger (${#need[@]}) : ${need[*]}"
# Les anciennes versions sont retirées pour qu'un seul .deb par paquet reste.
for p in "${need[@]}"; do
  v=$(apt-cache policy "$p" | awk '/Candidate:/ { print $2 }')
  ls "${p}_"*.deb > /dev/null 2>&1 && ! ls "${p}_${v//:/%3a}_"*.deb > /dev/null 2>&1 && rm -f "${p}_"*.deb
done
rm -f cdebconf_*.deb libdebian-installer4_*.deb libtextwrap1_*.deb
apt-get download "${need[@]}" > /dev/null
printf '%s\n' "${need[@]}" > "$DEST/LIST"
# Dépendances déjà installées dans WSL : `mkrootfs.sh` en copie tous les fichiers (données
# chargées sans `ldd` : pilotes Mesa, polices, configuration de fontconfig, schémas GSettings…).
printf '%s\n' "${have[@]}" > "$DEST/INSTALLED"
du -ch "$DEST"/*.deb | tail -1
