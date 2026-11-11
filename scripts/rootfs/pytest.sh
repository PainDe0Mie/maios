#!/bin/bash
# Vérifications de Python 3 dans MaiOS, lancées par `maios_bench.py --tests python` :
# démarrage, modules d'extension C (dlopen), fils, sous-processus, sockets, asyncio, puis une
# sélection de la suite de régression de CPython (`python3 -m test`), un module à la fois.
#   run -s /bin/bash /opt/maios/pytest.sh [test_os test_signal ...]
exec 2>&1
ok=0; ko=0
echo "pytest : debut (pid $$)"
check() {
  local out
  if out=$(eval "$2" 2>&1); then echo "[ok] $1"; ok=$((ok + 1)); else
    echo "[ECHEC] $1"; ko=$((ko + 1)); printf '%s
' "$out" | tail -15 | sed 's/^/  | /'
  fi
}
# Horloge sans sous-processus (`date` serait lui-même un fork + exec mesuré).
us() { local t=${EPOCHREALTIME/[.,]/}; echo $((10#$t)); }
ms() { echo $(( $(us) / 1000 )); }
py() { python3 -I -c "$1"; }
# Mémoire libre et disponible (Mio) dans $MEM, lue sans sous-processus (pas de `$(...)`).
mem() { local k v u f=0 a=0; while read -r k v u; do case $k in MemFree:) f=$v;; MemAvailable:) a=$v;; esac; done < /proc/meminfo; MEM="libre $((f / 1024)) Mio, dispo $((a / 1024)) Mio"; }

echo "pytest : $(python3 --version)"
t0=$(ms); python3 -I -c pass; t1=$(ms)
echo "pytest : temps demarrage $((t1 - t0)) ms"
t0=$(ms); python3 -I -S -c pass; t1=$(ms)
echo "pytest : temps demarrage sans site $((t1 - t0)) ms"
t0=$(us); /bin/true; t1=$(us)
echo "pytest : temps /bin/true $(( (t1 - t0) / 1000 )) ms"
t0=$(us); for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do /bin/true; done; t1=$(us)
echo "pytest : fork + exec + exit de /bin/true : $(( (t1 - t0) / 20 )) us en moyenne (20 lancements)"
t0=$(us); for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do ( : ); done; t1=$(us)
echo "pytest : fork + exit d'un sous-shell : $(( (t1 - t0) / 20 )) us en moyenne"
# Où passe le temps d'un lancement : appels système et fautes de page (noyau MaiOS).
if [ -e /proc/maios/syscalls ]; then
  echo reset > /proc/maios/syscalls; echo on > /proc/maios/syscalls
  for i in 1 2 3 4 5 6 7 8 9 10; do /bin/true; done
  echo off > /proc/maios/syscalls
  head -16 /proc/maios/syscalls | sed 's/^/pytest : sys /'
fi
echo "pytest : modules lus depuis __pycache__ : $(python3 -I -v -c pass 2>&1 | grep -c "code object from '/usr/lib/python3.13/__pycache__") ; recompiles : $(python3 -I -v -c pass 2>&1 | grep -c 'could not create\|bytecode is stale\|code object from .*\.py.$')"
python3 -I -X importtime -c pass 2>&1 | sort -t'|' -k2 -n | tail -4 | sed 's/^/pytest : importtime /'
check "python3 demarre" 'py pass'
check "modules C (dlopen)" 'py "import _ssl, _sqlite3, _ctypes, _decimal, _hashlib, zlib, _lzma, _bz2, _json, _asyncio, select, mmap, _socket, _posixsubprocess, readline"'
check "calcul" '[ "$(py "print(sum(i * i for i in range(10**6)))")" = 333332833333500000 ]'
check "fils (threading)" 'py "
import threading
n = [0]; l = threading.Lock()
def f():
    for _ in range(20000):
        with l: n[0] += 1
ts = [threading.Thread(target=f) for _ in range(8)]
[t.start() for t in ts]; [t.join() for t in ts]
assert n[0] == 160000, n"'
check "sous-processus" '[ "$(py "import subprocess; print(subprocess.run([\"echo\", \"maios\"], capture_output=True, text=True).stdout.strip())")" = maios ]'
check "socketpair" 'py "
import socket
a, b = socket.socketpair(); a.sendall(b\"x\" * 100000); a.close()
d = b\"\"
while (c := b.recv(65536)): d += c
assert len(d) == 100000"'
check "asyncio" 'py "
import asyncio
async def m():
    r = await asyncio.gather(*(asyncio.sleep(0.01, i) for i in range(50)))
    assert r == list(range(50))
asyncio.run(m())"'
check "sqlite3" 'py "
import sqlite3
c = sqlite3.connect(\":memory:\"); c.execute(\"create table t(x)\")
c.executemany(\"insert into t values (?)\", ((i,) for i in range(1000)))
assert c.execute(\"select sum(x) from t\").fetchone()[0] == 499500"'
check "ctypes" 'py "import ctypes, os; assert ctypes.CDLL(None).getpid() == os.getpid()"'
check "hashlib (OpenSSL)" '[ "$(py "import hashlib; print(hashlib.sha256(b\"maios\").hexdigest()[:16])")" = "$(printf maios | sha256sum | cut -c1-16)" ]'
check "netlink (ip link, ip addr, ip route)" 'ip -o link show lo | grep -q LOOPBACK && ip -o addr show lo | grep -q "127.0.0.1/8" && ip route show table local | grep -q "^local 127.0.0.0/8"'
check "netlink (if_nameindex, getaddrinfo AI_ADDRCONFIG)" 'py "
import socket
n = socket.if_nameindex(); assert (1, \"lo\") in n, n
assert socket.if_nametoindex(\"lo\") == 1 and socket.if_indextoname(1) == \"lo\"
r = socket.getaddrinfo(\"localhost\", 80, socket.AF_INET, socket.SOCK_STREAM, 0, socket.AI_ADDRCONFIG)
assert r and r[0][4][0] == \"127.0.0.1\", r"'
check "IPv6 : donnees annexes (RFC 3542)" 'py "
import socket, array
a = socket.socket(socket.AF_INET6, socket.SOCK_DGRAM); a.bind((\"::1\", 0))
a.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_RECVHOPLIMIT, 1)
b = socket.socket(socket.AF_INET6, socket.SOCK_DGRAM)
b.sendmsg([b\"x\"], [(socket.IPPROTO_IPV6, socket.IPV6_HOPLIMIT, array.array(\"i\", [7]))], 0, a.getsockname())
m, anc, fl, _ = a.recvmsg(10, 1024)
assert anc == [(socket.IPPROTO_IPV6, socket.IPV6_HOPLIMIT, array.array(\"i\", [7]).tobytes())], anc"'
check "gcc (compile et lance un programme C)" 'printf "#include <stdio.h>\nint main(void) { puts(\"maios\"); return 0; }\n" > /tmp/h.c && gcc -O2 -o /tmp/h /tmp/h.c && [ "$(/tmp/h)" = maios ]'
check "multiprocessing (fork)" 'py "
import multiprocessing as mp
mp.set_start_method(\"fork\")
with mp.Pool(4) as p: assert p.map(abs, range(-50, 0)) == list(range(50, 0, -1))"'

# Suite de régression de CPython, un module par processus (un crash n'emporte pas les autres).
tests=${*:-"test_errno test_stat test_fileio test_fcntl test_os test_posix test_select test_selectors \
  test_epoll test_poll test_mmap test_signal test_thread test_threading test_subprocess test_socket \
  test_time test_tempfile test_shutil test_glob test_pwd test_grp test_resource test_pty \
  test_json test_re test_zlib test_hashlib test_sqlite3 test_ctypes"}
export TMPDIR=/tmp
cd /tmp || exit 1
df -k /tmp | sed 's/^/pytest : df /'
for t in $tests; do
  s=$(ms)
  out=$(python3 -m test -v --timeout 300 "$t" 2>&1); rc=$?
  e=$(ms); mem
  res=$(printf '%s\n' "$out" | grep -E '^(Total tests)' | tr '\n' ' ')
  if [ $rc = 0 ]; then
    echo "[ok] $t ($((e - s)) ms) $res [$MEM]"; ok=$((ok + 1))
  else
    echo "[ECHEC] $t rc=$rc ($((e - s)) ms) $res [$MEM]"; ko=$((ko + 1))
    # Sous-tests en échec, puis la dernière ligne de chaque trace (le type d'erreur et son message).
    printf '%s\n' "$out" | grep -E ' \.\.\. (FAIL|ERROR)$|^(FAIL|ERROR): ' | grep -E '^(FAIL|ERROR): ' | sort -u | head -150 | sed "s/^/  $t | /"
    printf '%s\n' "$out" | grep -E '^[A-Za-z_.]*(Error|Exception|Failure)[A-Za-z]*: |^Fatal Python error' | sort | uniq -c | sort -rn | head -25 | sed "s/^/  $t > /"
    # Les 8 lignes qui précèdent chacune des 3 premières exceptions (où elle est levée).
    printf '%s\n' "$out" | awk '{ b[n % 8] = $0; n++ }
      /^[A-Za-z_.]*(Error|Exception)[A-Za-z]*: / && c < 3 { c++; for (i = n - 8; i < n; i++) if (i >= 0) print b[i % 8]; print "--" }' | sed "s/^/  $t ~ /"
    # Place dans /tmp (ENOSPC) : volume, puis ce qui l'occupe.
    if printf '%s\n' "$out" | grep -q 'No space left'; then
      df -k /tmp /dev/shm 2>&1 | sed "s/^/  $t df /"
      du -sk /tmp/* 2>/dev/null | sort -rn | head -5 | sed "s/^/  $t du /"
    fi
    # Tué par un signal (plantage, délai) : la fin de la sortie dit quel test tournait.
    if [ $rc -ge 128 ]; then
      # Pas de rapport final : les verdicts au fil de l'eau (`... ERROR`), puis la fin.
      printf '%s\n' "$out" | grep -E ' \.\.\. (FAIL|ERROR)$' | head -60 | sed "s/^/  $t | /"
      printf '%s\n' "$out" | tail -25 | sed "s/^/  $t # /"
    fi
  fi
done
echo "pytest : FIN $ok ok, $ko echec(s)"
