#!/bin/bash
# Banc S3 du plan 07 (Steam) : le vrai bubblewrap de Debian (`bwrap`), employé comme le font
# pressure-vessel (conteneur du Steam Runtime) et flatpak.
#   run -s /bin/bash /opt/maios/bwraptest.sh
# D'abord en root (bwrap sans espace utilisateur, sauf demande), puis en utilisateur ordinaire
# (uid 1000, `setpriv`) : bwrap non setuid crée alors un espace utilisateur, comme pour Steam.
exec 2>&1
export PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin LANG=C.UTF-8
ok=0
ko=0
# check <libellé> <condition> [fichier montré en cas d'échec]
check() {
  if eval "$2"; then
    echo "[ok] $1"
    ok=$((ok + 1))
  else
    echo "[ECHEC] $1"
    ko=$((ko + 1))
    [ -n "$3" ] && [ -f "$3" ] && head -20 "$3" | sed 's/^/  | /'
  fi
}
echo "bwraptest : debut (pid $$)"
t=/tmp/bwt
rm -rf "$t"
mkdir -p "$t/rw" "$t/u"
chmod 1777 "$t" "$t/u"
# Aussi pour valider le banc sur Linux : `BWRAP=chemin bash bwraptest.sh` en uid 1000 (sans root,
# la partie root est sautée).
B=${BWRAP:-/usr/bin/bwrap}
o=$t/out

# Racine minimale à la pressure-vessel : /usr en lecture seule, liens « usr fusionné », /etc,
# /proc et /dev neufs, /tmp en tmpfs.
base="--ro-bind /usr /usr --symlink usr/bin /bin --symlink usr/sbin /sbin --symlink usr/lib /lib
  --symlink usr/lib64 /lib64 --ro-bind /etc /etc --proc /proc --dev /dev --tmpfs /tmp --dir /run"
# bw <fichier de sortie> <arguments…> : lance bwrap, sortie dans le fichier, rend son code.
bw() {
  local f=$1
  shift
  "$B" "$@" > "$f" 2>&1
  rc=$?
}

$B --version > "$o" 2>&1
check "bwrap --version" 'grep -q "^bubblewrap 0\." "$o"' "$o"

if [ "$(id -u)" = 0 ]; then
echo "bwraptest : --- root ---"
bw "$o" $base -- /bin/true
check "root : /bin/true dans le bac a sable" '[ $rc -eq 0 ]' "$o"
bw "$o" $base -- /bin/sh -c 'exit 7'
check "root : code de sortie transmis (7)" '[ $rc -eq 7 ]' "$o"
bw "$o" $base -- /bin/sh -c 'touch /usr/x'
check "root : --ro-bind /usr -> EROFS" 'grep -q "Read-only file system" "$o"' "$o"
bw "$o" $base --bind "$t/rw" /mnt -- /bin/sh -c 'echo dedans > /mnt/f'
check "root : --bind en ecriture visible dehors" 'grep -qx dedans "$t/rw/f"' "$o"
bw "$o" $base --bind "$t/rw" /mnt --remount-ro /mnt -- /bin/sh -c 'touch /mnt/g'
check "root : --remount-ro" 'grep -q "Read-only file system" "$o" && [ ! -e "$t/rw/g" ]' "$o"
bw "$o" $base -- /bin/sh -c 'echo x > /tmp/cache; cat /tmp/cache'
check "root : /tmp du bac a sable prive" 'grep -qx x "$o" && [ ! -e /tmp/cache ]' "$o"
bw "$o" $base --unshare-pid -- /bin/sh -c 'echo "pid=$$ init=$(cat /proc/1/comm)"'
check "root : --unshare-pid (pid 2, init bwrap)" 'grep -qx "pid=2 init=bwrap" "$o"' "$o"
bw "$o" $base --unshare-pid --as-pid-1 -- /bin/sh -c 'echo "pid=$$"'
check "root : --as-pid-1" 'grep -qx "pid=1" "$o"' "$o"
bw "$o" $base --unshare-uts --hostname bac -- /bin/hostname
check "root : --unshare-uts --hostname" 'grep -qx bac "$o"' "$o"
dehors=$(readlink /proc/self/ns/ipc)
bw "$o" $base --unshare-ipc -- /bin/readlink /proc/self/ns/ipc
check "root : --unshare-ipc" 'grep -q "^ipc:\[" "$o" && ! grep -qx "$dehors" "$o"' "$o"
bw "$o" $base --setenv FOO bar --unsetenv HOME --chdir /usr -- /bin/sh -c 'echo "$FOO ${HOME-vide} $(pwd)"'
check "root : --setenv --unsetenv --chdir" 'grep -qx "bar vide /usr" "$o"' "$o"
bw "$o" $base --dir /x/y --perms 0700 --dir /p --symlink /usr/bin/true /t -- \
  /bin/sh -c 'test -d /x/y && stat -c %a /p && readlink /t'
check "root : --dir --perms --symlink" 'grep -qx 700 "$o" && grep -qx /usr/bin/true "$o"' "$o"
bw "$o" $base --file 3 /run/salut -- /bin/cat /run/salut 3<<< "bonjour"
check "root : --file (donnees par fd)" 'grep -qx bonjour "$o"' "$o"
bw "$o" $base --ro-bind-data 3 /run/ro -- /bin/sh -c 'cat /run/ro; echo z >> /run/ro' 3<<< "fige"
check "root : --ro-bind-data" 'grep -qx fige "$o" && grep -q "Read-only file system" "$o"' "$o"
bw "$o" $base -- /bin/sh -c 'test -c /dev/null && test -c /dev/zero && test -c /dev/urandom &&
  head -c 4 /dev/urandom | wc -c && test -d /dev/pts && test -e /dev/ptmx && test -d /dev/shm &&
  readlink /dev/fd && test -c /dev/tty'
check "root : --dev (null, zero, urandom, pts, ptmx, shm, fd, tty)" 'grep -qx 4 "$o" && grep -qx /proc/self/fd "$o"' "$o"
# Sortie sur un terminal : bwrap lie le terminal de l'hôte (`ttyname(1)`) sur /dev/console.
"$B" $base --dev /dev -- /bin/sh -c 'test -c /dev/console' 2> "$o"
rc=$?
check "root : --dev, sortie terminal : /dev/console lie" '[ $rc -eq 0 ]' "$o"
bw "$o" $base -- /bin/sh -c 'exec 3<> /dev/ptmx && echo pty'
check "root : --dev : /dev/ptmx ouvrable" 'grep -qx pty "$o"' "$o"
bw "$o" $base --new-session -- /bin/sh -c 'read a b c d e f r < /proc/$$/stat; [ "$a" = "$f" ] && echo session'
check "root : --new-session (setsid)" 'grep -qx session "$o"' "$o"
bw "$o" $base --cap-drop ALL -- /bin/grep CapEff /proc/self/status
check "root : --cap-drop ALL" 'grep -q "CapEff:[[:space:]]*0000000000000000" "$o"' "$o"
bw "$o" $base --unshare-user --uid 1234 --gid 5678 -- /bin/sh -c 'echo "$(id -u) $(id -g)"; cat /proc/self/uid_map'
check "root : --unshare-user --uid --gid" 'grep -qx "1234 5678" "$o" && grep -Eq "^ *1234 +0 +1$" "$o"' "$o"
bw "$o" $base --info-fd 4 -- /bin/true 4> "$t/info"
check "root : --info-fd (child-pid)" 'grep -q "\"child-pid\"" "$t/info"' "$t/info"
bw "$o" $base --json-status-fd 4 -- /bin/sh -c 'exit 3' 4> "$t/json"
check "root : --json-status-fd (exit-code 3)" 'grep -q "\"exit-code\": *3" "$t/json"' "$t/json"

# Réseau : bwrap allume `lo` (rtnetlink, `loopback_setup`) ; aucune carte, pas de route dehors.
bw "$o" $base --unshare-net -- /bin/cat /proc/net/dev
check "root : --unshare-net : lo seul" 'grep -q "^ *lo:" "$o" && ! grep -q "eth" "$o"' "$o"
bw "$o" $base --unshare-net -- /bin/bash -c 'exec 3<> /dev/tcp/127.0.0.1/9'
check "root : --unshare-net : lo allume (connexion refusee)" 'grep -q "Connection refused" "$o"' "$o"
bw "$o" $base --unshare-net -- /bin/bash -c 'exec 3<> /dev/tcp/10.0.2.2/7'
check "root : --unshare-net : dehors injoignable" 'grep -q "Network is unreachable" "$o"' "$o"

fi

echo "bwraptest : --- utilisateur 1000 ---"
U="setpriv --reuid=1000 --regid=1000 --clear-groups --"
[ "$(id -u)" = 1000 ] && U=
u() {
  local f=$1
  shift
  $U "$B" "$@" > "$f" 2>&1
  rc=$?
}
u "$o" $base -- /bin/sh -c 'echo "$(id -u) $(id -g)"; cat /proc/self/uid_map'
# `--dev` monte un devpts, qui exige root dans son espace : bwrap crée un premier espace où il
# est 0 (0 -> 1000), puis un second, imbriqué, où l'appelant redevient 1000 (1000 -> 0).
check "1000 : deux espaces utilisateur (1000 -> 0 -> 1000)" 'grep -qx "1000 1000" "$o" && grep -Eq "^ *1000 +0 +1$" "$o"' "$o"
u "$o" $base --unshare-all -- /bin/sh -c 'echo "pid=$$"; cat /proc/net/dev; hostname'
check "1000 : --unshare-all" 'grep -qx "pid=2" "$o" && grep -q "^ *lo:" "$o"' "$o"
u "$o" $base --unshare-all --share-net -- /bin/cat /proc/net/dev
check "1000 : --share-net garde les cartes" 'grep -q "^ *eth0:" "$o"' "$o"
u "$o" $base --unshare-user --uid 0 --gid 0 -- /bin/sh -c 'id -u; touch /etc/x'
check "1000 : --uid 0 sans pouvoir sur /etc" 'grep -qx 0 "$o" && grep -Eq "Read-only file system|Permission denied" "$o"' "$o"
u "$o" $base --bind "$t/u" /mnt -- /bin/sh -c 'echo moi > /mnt/m'
check "1000 : --bind d'un dossier de l'utilisateur" 'grep -qx moi "$t/u/m" && [ "$(stat -c %u "$t/u/m")" = 1000 ]' "$o"
u "$o" $base --unshare-pid --die-with-parent --new-session -- /bin/sh -c 'echo fin'
check "1000 : --die-with-parent --new-session" 'grep -qx fin "$o"' "$o"
u "$o" $base --dev-bind /dev /dev -- /bin/true
check "1000 : --dev-bind /dev" '[ $rc -eq 0 ]' "$o"
u "$o" $base --cap-add CAP_SYS_ADMIN -- /bin/grep CapEff /proc/self/status
check "1000 : --cap-add CAP_SYS_ADMIN dans l'espace" 'grep -q "CapEff:[[:space:]]*0000000000200000" "$o"' "$o"
# Bac à sable dans le bac à sable : steamwebhelper (Chromium) crée ses propres espaces dans
# le conteneur de pressure-vessel.
u "$o" $base --ro-bind "$B" /run/bwrap --unshare-all -- /run/bwrap $base --unshare-all -- /bin/sh -c 'echo "imbrique pid=$$"'
check "1000 : bwrap imbrique" 'grep -qx "imbrique pid=2" "$o"' "$o"

rm -rf "$t"
echo "bwraptest : FIN $ok ok, $ko echec"
