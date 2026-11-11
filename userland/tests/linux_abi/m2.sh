# Jalon M2 de MaiProc : le shell de BusyBox (ash) sur MaiOS.
# Lancé par : run -s /extra_files/linux/busybox sh /extra_files/linux/m2.sh
# Chaque contrôle écrit [ok] ou [ECHEC] ; la dernière ligne est « sh : termine ».
BB=/extra_files/linux/busybox
ok() { echo "[ok] $1"; }
ko() { echo "[ECHEC] $1"; }
echo "sh : debut (pid $$)"

x=$(echo sous-shell)
[ "$x" = "sous-shell" ] && ok "substitution de commande" || ko "substitution de commande : '$x'"

n=$($BB ls /extra_files/linux | $BB wc -l)
[ "$n" -ge 3 ] && ok "tube ls | wc -l : $n" || ko "tube ls | wc -l : '$n'"

u=$($BB echo bonjour | $BB tr a-z A-Z | $BB tr B b)
[ "$u" = "bONJOUR" ] && ok "tube a trois etages : $u" || ko "tube a trois etages : '$u'"

last=$($BB seq 1 5000 | $BB tail -n 1)
[ "$last" = "5000" ] && ok "seq 5000 | tail : $last" || ko "seq 5000 | tail : '$last'"

echo ligne1 > /m2test.txt
echo ligne2 >> /m2test.txt
c=$($BB wc -l < /m2test.txt)
[ "$c" -eq 2 ] && ok "redirections > >> < : $c lignes" || ko "redirections : '$c'"
$BB rm -f /m2test.txt

( exit 3 )
[ $? -eq 3 ] && ok "sous-shell : code 3" || ko "sous-shell : code"

$BB false
[ $? -eq 1 ] && ok "code de sortie d'une commande externe" || ko "code de sortie externe"

trap 'echo "[ok] trap USR1"' USR1
kill -USR1 $$

$BB sleep 1 &
p=$!
wait $p
[ $? -eq 0 ] && ok "tache de fond + wait ($p)" || ko "tache de fond + wait"

$BB sleep 5 &
p=$!
kill $p
wait $p
s=$?
[ $s -eq 143 ] && ok "kill d'une tache de fond : statut $s" || ko "kill d'une tache de fond : statut $s"

i=0
while [ $i -lt 20 ]; do
    $BB true
    i=$((i + 1))
done
[ $i -eq 20 ] && ok "20 fork + exec en boucle" || ko "boucle fork + exec"

if $BB grep -q nexistepas /extra_files/linux/m2.sh.absent 2>/dev/null; then ko "grep sur fichier absent"; else ok "grep sur fichier absent : echec attendu"; fi

v=$(echo "a b c" | { read p1 p2 p3; echo "$p3$p2$p1"; })
[ "$v" = "cba" ] && ok "read dans un groupe en tube : $v" || ko "read en tube : '$v'"

echo "sh : termine"
