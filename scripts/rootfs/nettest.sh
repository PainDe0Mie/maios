#!/bin/bash
# Accès à Internet depuis des programmes Linux réels (réseau NAT de QEMU : passerelle 10.0.2.2,
# DNS 10.0.2.3) : résolution de noms (glibc, `getent`), HTTP et HTTPS (Python, `urllib` + `ssl`).
#   run -s /bin/bash /opt/maios/nettest.sh [hote]
exec 2>&1
host=${1:-example.com}
ok=0
ko=0
check() {
  local name=$1
  shift
  local out
  if out=$(timeout 30 "$@" 2>&1); then
    echo "[ok] $name : $(echo "$out" | head -1)"
    ok=$((ok + 1))
  else
    echo "[ECHEC] $name (code $?) : $(echo "$out" | tail -3 | tr '\n' ' ')"
    ko=$((ko + 1))
  fi
}
echo "nettest : debut (pid $$)"
echo "nettest : resolv.conf : $(grep -v '^#' /etc/resolv.conf | tr '\n' ' ')"
check "getent hosts $host" getent hosts "$host"
check "getent ahostsv4 $host" getent ahostsv4 "$host"
check "python getaddrinfo" python3 -c "import socket; print(socket.getaddrinfo('$host', 443, socket.AF_INET, socket.SOCK_STREAM)[0][4])"
check "python tcp 80" python3 -c "import socket; s = socket.create_connection(('$host', 80), 10); print('connecte', s.getpeername())"
check "python http" python3 -c "import urllib.request as u; r = u.urlopen('http://$host/', timeout=15); print(r.status, len(r.read()), 'octets')"
check "python https" python3 -c "import urllib.request as u; r = u.urlopen('https://$host/', timeout=15); print(r.status, len(r.read()), 'octets')"
echo "nettest : FIN $ok ok, $ko echec"
