#!/bin/bash
# Lance un client Wayland réel dans MaiLand (phase W4) avec l'environnement d'une session
# graphique, puis le tue au bout de `secondes` s'il tourne encore :
#   run -s /bin/bash /opt/maios/apprun.sh <secondes> <programme> [arguments...]
# La racine ext4 est en lecture seule : tout ce que les clients écrivent (configuration,
# caches) va dans /tmp (tmpfs).
#   run -s /bin/bash /opt/maios/apprun.sh --strace errors:firefox <secondes> <programme> ...
# trace en plus les appels système du programme (`/proc/maios/strace`, « mode:motif »).
exec 2>&1
strace=
if [ "$1" = "--strace" ]; then
  strace=${2/:/ }
  shift 2
  echo "$strace" > /proc/maios/strace
fi
t=${1:-20}
shift
export HOME=/tmp/home XDG_CONFIG_HOME=/tmp/home/.config XDG_CACHE_HOME=/tmp/home/.cache
export XDG_DATA_HOME=/tmp/home/.local/share XDG_STATE_HOME=/tmp/home/.local/state
export XDG_RUNTIME_DIR=/run/user/0 WAYLAND_DISPLAY=wayland-0 XDG_SESSION_TYPE=wayland
export PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin LANG=C.UTF-8 TERM=xterm-256color
export SHELL=/bin/bash USER=root LOGNAME=root
# Pas de GPU : rendu logiciel partout, tampons `wl_shm`.
export GDK_BACKEND=wayland GSK_RENDERER=cairo GTK_A11Y=none NO_AT_BRIDGE=1
export QT_QPA_PLATFORM=wayland QT_QUICK_BACKEND=software QT_WAYLAND_DISABLE_WINDOWDECORATION=1
export SDL_VIDEO_DRIVER=wayland SDL_AUDIO_DRIVER=dummy SDL_RENDER_DRIVER=software
export MOZ_ENABLE_WAYLAND=1 MOZ_DISABLE_CONTENT_SANDBOX=1 MOZ_DISABLE_RDD_SANDBOX=1
export MOZ_DISABLE_SOCKET_PROCESS_SANDBOX=1 MOZ_DISABLE_GMP_SANDBOX=1 MOZ_CRASHREPORTER_DISABLE=1
export LIBGL_ALWAYS_SOFTWARE=1
mkdir -p "$XDG_CONFIG_HOME" "$XDG_CACHE_HOME" "$XDG_DATA_HOME" "$XDG_STATE_HOME"
echo "apprun : debut (pid $$) $*"
"$@" &
pid=$!
(sleep "$t"; kill -9 "$pid" 2> /dev/null && echo "apprun : $1 tue apres $t s") &
w=$!
wait "$pid"
code=$?
kill "$w" 2> /dev/null
[ -n "$strace" ] && echo off > /proc/maios/strace
echo "apprun : FIN $1 code $code"
