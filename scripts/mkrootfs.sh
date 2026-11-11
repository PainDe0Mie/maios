#!/usr/bin/env bash
# Image ext4 d'une racine Linux pour MaiOS (MaiSS), faite depuis la Debian de WSL, sans
# téléchargement ni droits administrateur :
#   * les fichiers des paquets choisis (`dpkg -L`) et toutes les bibliothèques dont leurs ELF
#     ont besoin (`ldd`), avec leurs chaînes de liens symboliques ;
#   * les paquets téléchargés par `fetch_debs.sh` (Python 3 et sa suite de tests), absents de
#     WSL, extraits tels quels (`dpkg-deb -x`, sans script d'installation) ;
#   * la disposition « usr fusionné » de Debian (`/bin -> usr/bin`, `/lib64 -> usr/lib64`…) ;
#   * quelques fichiers de `/etc` (utilisateur root, `PATH`, invite) ;
#   * `/opt/maios/ext4test.sh` et les empreintes SHA-256 de tous les fichiers de `/usr`, pour
#     vérifier dans MaiOS que chaque octet lu est le bon.
# `fakeroot` fait de root le propriétaire de tout, `mke2fs -d` remplit le volume, `e2fsck -fD`
# construit les index htree des gros dossiers.
#
#   wsl bash scripts/mkrootfs.sh               # -> build/rootfs.ext4
#   python scripts/maios_bench.py --kvm --tests ext4
set -euo pipefail

if [ "$(id -u)" != 0 ]; then
  exec fakeroot -- bash "$0" "$@"
fi

ROOT_DIR=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$ROOT_DIR/build/rootfs.ext4}
PKGS=${PKGS:-"bash coreutils grep sed findutils tar gzip xz-utils bzip2 diffutils mawk perl-base procps \
  debianutils ncurses-base ncurses-bin libc-bin hostname util-linux less file sensible-utils base-files \
  libpython3.13-minimal libpython3.13-stdlib media-types tzdata ca-certificates dash netbase iproute2 \
  binutils binutils-x86-64-linux-gnu gcc gcc-14 gcc-14-x86-64-linux-gnu cpp cpp-14 cpp-14-x86-64-linux-gnu \
  libgcc-14-dev libc6-dev linux-libc-dev libcrypt-dev xkb-data librsvg2-common"}
DEBS_DIR=${DEBS_DIR:-$HOME/maios_pkgs/debs}
STAGE=$(mktemp -d /tmp/maios-rootfs.XXXXXX)
trap 'rm -rf "$STAGE"' EXIT

# Copie `f` dans l'étage à son chemin réel, en recopiant chaque lien de la chaîne.
copy() {
  local f=$1 d g t
  for _ in $(seq 1 40); do
    [ -e "$f" ] || [ -L "$f" ] || return 0
    d=$(realpath "$(dirname "$f")")
    g="$d/$(basename "$f")"
    mkdir -p "$STAGE$d"
    if [ -L "$g" ]; then
      cp -a "$g" "$STAGE$d/"
      t=$(readlink "$g")
      case $t in /*) f=$t ;; *) f="$d/$t" ;; esac
    elif [ -d "$g" ]; then
      mkdir -p "$STAGE$g"
      return 0
    else
      [ -e "$STAGE$g" ] || cp -a "$g" "$STAGE$d/"
      return 0
    fi
  done
}

# Dépendances des paquets téléchargés déjà installées dans WSL (liste de `fetch_debs.sh`).
if [ -f "$DEBS_DIR/INSTALLED" ]; then PKGS="$PKGS $(sort -u "$DEBS_DIR/INSTALLED" | tr '\n' ' ')"; fi
echo "paquets :"
for p in $PKGS; do
  if ! dpkg-query -W -f='${Status}' "$p" 2> /dev/null | grep -q "install ok installed"; then
    echo "  $p : absent, ignore"
    continue
  fi
  n=0
  while IFS= read -r f; do
    case $f in
      /usr/share/doc/* | /usr/share/man/* | /usr/share/info/* | /usr/share/locale/* | /usr/share/lintian/*) continue ;;
    esac
    copy "$f"
    n=$((n + 1))
  done < <(dpkg -L "$p")
  echo "  $p : $n chemins"
done

# Paquets téléchargés : leurs fichiers, sans la documentation.
if [ -f "$DEBS_DIR/LIST" ]; then
  echo "paquets telecharges ($DEBS_DIR) :"
  for deb in "$DEBS_DIR"/*.deb; do
    dpkg-deb --fsys-tarfile "$deb" | tar -x -C "$STAGE" --no-same-owner \
      --exclude='./usr/share/doc/*' --exclude='./usr/share/man/*' --exclude='./usr/share/lintian/*'
    echo "  $(dpkg-deb -f "$deb" Package) $(dpkg-deb -f "$deb" Version)"
  done
  for d in bin sbin lib lib64; do
    if [ -d "$STAGE/$d" ] && [ ! -L "$STAGE/$d" ]; then
      echo "  $d/ hors de /usr : fusionne" && mkdir -p "$STAGE/usr/$d" && cp -a "$STAGE/$d/." "$STAGE/usr/$d/" && rm -rf "${STAGE:?}/$d"
    fi
  done
else
  echo "aucun paquet telecharge ($DEBS_DIR/LIST absent : wsl bash scripts/fetch_debs.sh)"
fi

# Liens de `update-alternatives` dont la cible est dans l'image
# (`/usr/bin/awk -> /etc/alternatives/awk -> /usr/bin/mawk`).
for a in /etc/alternatives/*; do
  t=$(readlink -f "$a") || continue
  [ -e "$STAGE$t" ] || continue
  link=$(update-alternatives --query "$(basename "$a")" 2> /dev/null | awk '/^Link:/ { print $2 }' || true)
  copy "$a"
  if [ -n "$link" ] && [ -L "$link" ]; then copy "$link"; fi
done

# Clients Wayland de test de MaiLand (`userland/tests/wayland/build.sh`), avec leurs
# bibliothèques (libwayland-client, libxkbcommon, libffi) ajoutées par l'étape suivante.
mkdir -p "$STAGE/opt/maios"
for wl in wlhello wlinput wltime wlsub wlsec; do
  if [ -x "$ROOT_DIR/build/wayland/$wl" ]; then
    install -m 755 "$ROOT_DIR/build/wayland/$wl" "$STAGE/opt/maios/$wl"
    echo "wayland : /opt/maios/$wl"
  else
    echo "wayland : build/wayland/$wl absent (wsl bash userland/tests/wayland/build.sh)"
  fi
done

# Bibliothèques de tous les ELF (dépendances complètes données par `ldd`), cherchées d'abord dans
# l'étage (bibliothèques des paquets téléchargés) puis dans WSL. `ldd` échoue sur un ELF i386 (pas
# de chargeur 32 bits dans WSL) ou statique : ignoré, les bibliothèques i386 viennent toutes des
# paquets `:i386` de `fetch_i386.sh`.
echo "bibliotheques :"
before=$(find "$STAGE" -type f | wc -l)
LIBS_STAGE="$STAGE/usr/lib/x86_64-linux-gnu:$STAGE/usr/lib"
while IFS= read -r f; do
  { LD_LIBRARY_PATH=$LIBS_STAGE ldd "$f" 2> /dev/null || true; } | awk '/=> \// { print $3 } /^\t\/[^ ]+ \(/ { print $1 }' | while read -r lib; do
    case $lib in "$STAGE"/*) continue ;; esac
    copy "$lib"
  done
done < <(find "$STAGE" -type f \( -perm -u+x -o -name '*.so*' \) -exec sh -c 'head -c 4 "$1" | grep -q "ELF" && echo "$1"' _ {} \;)
echo "  $(($(find "$STAGE" -type f | wc -l) - before)) fichiers ajoutes"

# Modules Python précompilés (`py3compile` de l'installation Debian, qui n'a pas tourné) : sans
# `__pycache__`, chaque lancement recompilerait tout ce qu'il importe. Le Python de l'étage
# tourne ici dans WSL ; `-s`/`-p` donnent aux .pyc les chemins qu'ils auront dans MaiOS, et
# `mke2fs -d` garde les dates des sources que les .pyc vérifient.
for py in "$STAGE"/usr/bin/python3.[0-9]*; do
  [ -x "$py" ] || continue
  v=$(basename "$py")
  t0=$(date +%s)
  LD_LIBRARY_PATH=$LIBS_STAGE "$py" -I -m compileall -q -j0 -s "$STAGE" -p / "$STAGE/usr/lib/$v" > /dev/null 2>&1 || true
  echo "  $v : $(find "$STAGE/usr/lib/$v" -name '*.pyc' | wc -l) modules precompiles en $(($(date +%s) - t0)) s"
done

# Caches que les scripts d'installation de Debian auraient produits (absents de `dpkg -L`) :
# schémas GSettings, chargeurs d'images de gdk-pixbuf, base MIME, polices de fontconfig (sans
# cache, chaque client graphique relirait toutes les polices au démarrage).
SCHEMAS="$STAGE/usr/share/glib-2.0/schemas"
if [ -d "$SCHEMAS" ] && [ -x /usr/lib/x86_64-linux-gnu/glib-2.0/glib-compile-schemas ]; then
  /usr/lib/x86_64-linux-gnu/glib-2.0/glib-compile-schemas "$SCHEMAS" && echo "caches : schemas GSettings"
fi
PIXBUF=/usr/lib/x86_64-linux-gnu/gdk-pixbuf-2.0/2.10.0
if [ -d "$STAGE$PIXBUF/loaders" ] && [ -f "$PIXBUF/loaders.cache" ]; then
  cp "$PIXBUF/loaders.cache" "$STAGE$PIXBUF/" && echo "caches : chargeurs de gdk-pixbuf"
fi
if [ -d "$STAGE/usr/share/mime/packages" ]; then
  update-mime-database "$STAGE/usr/share/mime" > /dev/null 2>&1 && echo "caches : base MIME"
fi
if [ -d "$STAGE/etc/fonts" ]; then
  mkdir -p "$STAGE/var/cache/fontconfig"
  fc-cache -s -f -y "$STAGE" > /dev/null 2>&1 && echo "caches : polices ($(ls "$STAGE/var/cache/fontconfig" | wc -l) fichiers)"
fi
for t in "$STAGE"/usr/share/icons/*/; do
  [ -f "$t/index.theme" ] && gtk4-update-icon-cache -q -f -t "$t" 2> /dev/null || true
done

# Clients graphiques réels (phase W4 de MaiLand), lancés par `apprun.sh`.
install -m 755 "$ROOT_DIR/scripts/rootfs/apprun.sh" "$STAGE/opt/maios/apprun.sh"
install -m 755 "$ROOT_DIR/scripts/rootfs/nettest.sh" "$STAGE/opt/maios/nettest.sh"
# Banc S1 du plan 07 : Xwayland et clients X11.
install -m 755 "$ROOT_DIR/scripts/rootfs/xtest.sh" "$STAGE/opt/maios/xtest.sh"
# Banc S3 : bubblewrap réel (root puis utilisateur 1000).
install -m 755 "$ROOT_DIR/scripts/rootfs/bwraptest.sh" "$STAGE/opt/maios/bwraptest.sh"

# Disposition « usr fusionné ».
for d in bin sbin lib lib64; do
  mkdir -p "$STAGE/usr/$d"
  [ -e "$STAGE/$d" ] || ln -s "usr/$d" "$STAGE/$d"
done
mkdir -p "$STAGE"/{etc,root,home,tmp,var/tmp,var/log,dev,proc,sys,run,mnt,opt/maios}
chmod 1777 "$STAGE/tmp" "$STAGE/var/tmp"
chmod 700 "$STAGE/root"

cat > "$STAGE/etc/passwd" << 'EOF'
root:x:0:0:root:/root:/bin/bash
maios:x:1000:1000:MaiOS:/home/maios:/bin/bash
nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin
EOF
cat > "$STAGE/etc/group" << 'EOF'
root:x:0:
maios:x:1000:
nogroup:x:65534:
EOF
echo maios > "$STAGE/etc/hostname"
# D-Bus, GLib et Firefox lisent l'identifiant de la machine.
echo 6d61696f736d61696f736d61696f7300 > "$STAGE/etc/machine-id"
printf '127.0.0.1\tlocalhost\n127.0.1.1\tmaios\n::1\tlocalhost\n' > "$STAGE/etc/hosts"
printf 'passwd: files\ngroup: files\nhosts: files dns\n' > "$STAGE/etc/nsswitch.conf"
echo "nameserver 10.0.2.3" > "$STAGE/etc/resolv.conf"
cat > "$STAGE/etc/profile" << 'EOF'
export PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin
export HOME=/root TERM=linux LANG=C.UTF-8
PS1='\u@\h:\w\$ '
EOF
cp "$STAGE/etc/profile" "$STAGE/etc/bash.bashrc"

# Vérification dans MaiOS.
(cd "$STAGE" && find usr -type f -print0 | sort -z | xargs -0 sha256sum | sed 's|  usr/|  /usr/|') > "$STAGE/opt/maios/sha256.txt"
cat > "$STAGE/opt/maios/ext4test.sh" << 'EOF'
#!/bin/bash
# Vérifications de MaiSS (ext4) lancées par `maios_bench.py --tests ext4`.
exec 2>&1
ok=0; ko=0
echo "ext4test : debut (pid $$)"
check() { if eval "$2" > /dev/null 2>&1; then echo "[ok] $1"; ok=$((ok + 1)); else echo "[ECHEC] $1"; ko=$((ko + 1)); fi; }
echo "ext4test : bash $BASH_VERSION"
. /etc/os-release && echo "ext4test : $PRETTY_NAME"
check "lien /bin -> usr/bin" '[ "$(readlink /bin)" = usr/bin ]'
check "chaine de liens de ld.so" '[ "$(readlink -f /lib64/ld-linux-x86-64.so.2)" = /usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2 ]'
check "proprietaire et droits" '[ "$(stat -c %u:%g:%a /usr/bin/bash)" = 0:0:755 ]'
check "dossier /root en 700" '[ "$(stat -c %a /root)" = 700 ]'
check "programmes dans /usr/bin" '[ "$(ls /usr/bin | wc -l)" -gt 100 ]'
check "recherche dans PATH" 'command -v sha256sum'
check "perl" '[ "$(perl -e "print 6*7")" = 42 ]'
check "awk" '[ "$(echo 3 4 | awk "{print \$1*\$2}")" = 12 ]'
check "sed et grep" '[ "$(echo maios | sed s/mai/MAI/ | grep -c MAIos)" = 1 ]'
check "find" '[ "$(find /usr/lib -name "libc.so.6" | wc -l)" -ge 1 ]'
check "ecriture refusee (EROFS)" '! touch /usr/bin/x 2> /dev/null'
n=$(wc -l < /opt/maios/sha256.txt)
t0=$(date +%s%N)
if sha256sum --quiet -c /opt/maios/sha256.txt; then echo "[ok] $n empreintes SHA-256"; ok=$((ok + 1)); else echo "[ECHEC] empreintes SHA-256"; ko=$((ko + 1)); fi
t1=$(date +%s%N)
kb=$(du -sk /usr | cut -f1)
echo "ext4test : debit $((kb * 1000000 / ((t1 - t0) / 1000 + 1) / 1024)) Mio/s ($((kb / 1024)) Mio en $(((t1 - t0) / 1000000)) ms)"
# Mesures séparées : un gros fichier seul, le calcul seul, beaucoup de petits fichiers,
# le parcours des métadonnées.
mesure() { local s e; s=$(date +%s%N); eval "$2" > /dev/null 2>&1; e=$(date +%s%N); echo "ext4test : temps $1 $(((e - s) / 1000000)) ms"; }
big=$(find /usr -type f -size +1M -printf '%s %p
' | sort -n | tail -1 | cut -d' ' -f2)
echo "ext4test : gros fichier $big ($(stat -c %s "$big") o)"
mesure gros_cat "cat $big"
mesure gros_cat_2 "cat $big"
mesure sha_zero_48M "head -c 50000000 /dev/zero | sha256sum"
mesure find_usr "find /usr"
mesure stat_usr "find /usr -type f -exec true {} +"
mesure cat_usr "find /usr -type f -exec cat {} +"
mesure sha_usr "find /usr -type f -exec sha256sum {} +"
echo "ext4test : FIN $ok ok, $ko echec(s)"
EOF
chmod 755 "$STAGE/opt/maios/ext4test.sh"
install -m 755 "$ROOT_DIR/scripts/rootfs/pytest.sh" "$STAGE/opt/maios/pytest.sh"

chown -R 0:0 "$STAGE"
# Utilisateur ordinaire (Steam ne tourne jamais en root).
mkdir -p "$STAGE/home/maios"
chown 1000:1000 "$STAGE/home/maios"
chmod 750 "$STAGE/home/maios"
mb=$(du -sm "$STAGE" | cut -f1)
size=$((mb * 13 / 10 + 48))
mkdir -p "$(dirname "$OUT")"
rm -f "$OUT"
mke2fs -q -F -t ext4 -b 4096 -L maios-linux -d "$STAGE" "$OUT" "${size}M"
e2fsck -fyD "$OUT" > /dev/null 2>&1 || [ $? -le 2 ]
echo "image : $OUT (${size} Mio, $(find "$STAGE" | wc -l) entrees, $(wc -l < "$STAGE/opt/maios/sha256.txt") fichiers dans /usr)"
