#!/bin/bash
# Banc S1 du plan 07 (Steam) : Xwayland dans MaiLand et clients X11 de Debian.
#   run -s /bin/bash /opt/maios/xtest.sh
# Xwayland « rootful » (une fenêtre MaiLand pour tout l'écran X), extensions, GLX logiciel,
# entrées (pilotées par le harnais au marqueur « xtest : pret entrees »), fin propre du serveur.
exec 2>&1
export HOME=/tmp/home XDG_CONFIG_HOME=/tmp/home/.config XDG_CACHE_HOME=/tmp/home/.cache
export XDG_RUNTIME_DIR=/run/user/0 WAYLAND_DISPLAY=wayland-0
export PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin LANG=C.UTF-8 LIBGL_ALWAYS_SOFTWARE=1
mkdir -p "$XDG_CONFIG_HOME" "$XDG_CACHE_HOME"
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
    [ -n "$3" ] && head -20 "$3" | sed 's/^/  | /'
  fi
}
echo "xtest : debut (pid $$)"
t=/tmp/xtest
rm -rf "$t"
mkdir -p "$t"

# -displayfd : Xwayland écrit le numéro d'écran une fois prêt à accepter des clients.
start=$(date +%s%N)
Xwayland -geometry 800x600 -shm -displayfd 3 3> "$t/dpy" > "$t/xwayland.log" 2>&1 &
xw=$!
for _ in $(seq 1 240); do
  [ -s "$t/dpy" ] && break
  kill -0 "$xw" 2> /dev/null || break
  sleep 0.25
done
check "Xwayland pret (-displayfd)" '[ -s "$t/dpy" ]'
if [ ! -s "$t/dpy" ]; then
  sed 's/^/xwayland | /' "$t/xwayland.log"
  echo "xtest : FIN $ok ok, $((ko + 1)) echec"
  exit 1
fi
echo "xtest : demarrage de Xwayland $((($(date +%s%N) - start) / 1000000)) ms"
export DISPLAY=:$(head -1 "$t/dpy")
check "socket X /tmp/.X11-unix/X${DISPLAY#:}" '[ -S "/tmp/.X11-unix/X${DISPLAY#:}" ]'

xdpyinfo > "$t/xdpy" 2>&1
check "xdpyinfo" 'grep -q "^name of display:" "$t/xdpy"' "$t/xdpy"
check "ecran 800x600" 'grep -q "dimensions: *800x600 pixels" "$t/xdpy"'
check "profondeur 24" 'grep -q "depth of root window: *24 planes" "$t/xdpy"'
for ext in BIG-REQUESTS Composite DAMAGE GLX MIT-SHM Present RANDR RENDER SHAPE SYNC XFIXES XInputExtension XKEYBOARD XTEST; do
  check "extension $ext" 'grep -q "^ *$ext\$" "$t/xdpy"'
done

glxinfo -B > "$t/glx" 2>&1
check "GLX : llvmpipe" 'grep -q "OpenGL renderer string: llvmpipe" "$t/glx"' "$t/glx"
check "GLX : rendu direct" 'grep -q "direct rendering: Yes" "$t/glx"'

xprop -root _XKB_RULES_NAMES > "$t/xkb" 2>&1
check "xprop : regles XKB de la racine" 'grep -q "^_XKB_RULES_NAMES(STRING) = \"evdev\"" "$t/xkb"' "$t/xkb"
xwininfo -root > "$t/root" 2>&1
check "xwininfo : racine 800x600" 'grep -q "Width: 800" "$t/root" && grep -q "Height: 600" "$t/root"' "$t/root"

# Entrées : xev occupe tout l'écran X (pas de gestionnaire de fenêtres : elle reste en 0,0).
# Sans gestionnaire, le focus X suit le pointeur (PointerRoot).
xev -geometry 800x600+0+0 -event keyboard -event button -event mouse > "$t/xev" 2>&1 &
xe=$!
sleep 4
echo "xtest : pret entrees"
for _ in $(seq 1 40); do
  grep -q "keycode 38 (keysym" "$t/xev" && break
  sleep 0.25
done
sleep 1
kill "$xe" 2> /dev/null
wait "$xe" 2> /dev/null
check "xev : mouvement du pointeur" 'grep -q "^MotionNotify" "$t/xev"' "$t/xev"
check "xev : bouton 1 enfonce" 'grep -A3 "^ButtonPress" "$t/xev" | grep -q "button 1,"'
check "xev : bouton 1 relache" 'grep -A3 "^ButtonRelease" "$t/xev" | grep -q "button 1,"'
# Touches physiques Q puis Maj+A de QEMU (codes X 24 et 38) : le symbole dépend de la disposition
# (us ou fr) que MaiLand donne à Xwayland, d'où la majuscule vérifiée par l'état et la casse.
check "xev : touche Q (code 24)" 'grep -q "keycode 24 (keysym 0x[0-9a-f]*, [a-z])" "$t/xev"'
check "xev : Maj+A (code 38, majuscule)" 'grep -q "state 0x1, keycode 38 (keysym 0x[0-9a-f]*, [A-Z])" "$t/xev"'

# GLX : quelques secondes de glxgears (images par seconde dans le journal).
timeout -s INT 7 glxgears > "$t/gears" 2>&1
fps=$(awk '/FPS/ { v = $(NF - 1) } END { print v }' "$t/gears")
check "glxgears : images" '[ -n "$fps" ]'
echo "xtest : glxgears ${fps:-0} i/s"

kill -TERM "$xw"
for _ in $(seq 1 40); do
  kill -0 "$xw" 2> /dev/null || break
  sleep 0.25
done
check "Xwayland termine sur SIGTERM" '! kill -0 "$xw" 2> /dev/null'
wait "$xw" 2> /dev/null
check "socket X retiree" '[ ! -e "/tmp/.X11-unix/X${DISPLAY#:}" ]'
grep -E "\(EE\)|Fatal" "$t/xwayland.log" | sed 's/^/xwayland | /'
echo "xtest : FIN $ok ok, $ko echec"
