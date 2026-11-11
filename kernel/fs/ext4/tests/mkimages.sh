#!/usr/bin/env bash
# Images de test du cœur ext4 (MaiSS), sans droits administrateur : `mke2fs -d` remplit le
# volume depuis une arborescence, `e2fsck -fD` construit les index htree, `debugfs` écrit des
# transactions dans le journal.
#
#   bash kernel/fs/ext4/tests/mkimages.sh /tmp/ext4img
#   EXT4_IMAGES=/tmp/ext4img cargo test --release
set -euo pipefail
OUT=${1:?dossier de sortie}
rm -rf "$OUT"
mkdir -p "$OUT"
SRC="$OUT/src"
mkdir -p "$SRC"

# --- Arborescence source ------------------------------------------------------------------
cd "$SRC"
: > empty
printf 'x' > one
head -c 59 /dev/urandom > s59
head -c 60 /dev/urandom > s60
head -c 61 /dev/urandom > s61
head -c 4095 /dev/urandom > b4095
head -c 4096 /dev/urandom > b4096
head -c 4097 /dev/urandom > b4097
head -c 1048576 /dev/urandom > m1
head -c 5000000 /dev/urandom > m5
# Fichier creux à nombreux îlots : un extent par îlot, d'où un arbre de profondeur 1.
: > sparse
for i in $(seq 0 699); do
  printf 'ilot-%04d' "$i" | dd of=sparse bs=1 seek=$((i * 12288 + (i % 7) * 100)) conv=notrunc status=none
done
truncate -s $((700 * 12288 + 50000)) sparse
# Trou au début, données au milieu, trou à la fin.
truncate -s 3000000 holes
head -c 70000 /dev/urandom | dd of=holes bs=1 seek=1500000 conv=notrunc status=none
ln -s one short_link
ln -s "$(printf 'a%.0s' $(seq 1 59))" link59
ln -s "$(printf 'b%.0s' $(seq 1 60))" link60
ln -s "$(printf 'c/%.0s' $(seq 1 100))end" long_link
ln -s ../dir/sub/deep.txt rel_link
ln m1 hard_m1
mkdir -p dir/sub dir/empty
echo "profond" > dir/sub/deep.txt
chmod 600 dir/sub/deep.txt
cp one setuid && chmod 4755 setuid
mkfifo fifo
mkdir big
for i in $(seq 1 3000); do : > "big/fichier_$i"; done
for i in $(seq 1 40); do : > "big/accentué_é_$i"; done
: > "big/$(printf 'n%.0s' $(seq 1 255))"
echo "été" > "été.txt"
# Assez d'entrées pour un index htree à deux niveaux avec des blocs de 1 Kio.
mkdir huge
(cd huge && seq -f "entree_au_nom_assez_long_pour_remplir_les_feuilles_%06g" 1 12000 | xargs touch)
cd - > /dev/null

common() { # nom options...
  local name=$1; shift
  mke2fs -q -F -L "$name" -d "$SRC" "$@" "$OUT/$name.img" 64M
}
# e2fsck rend 1 quand il a optimisé quelque chose : ce n'est pas une erreur.
index() { e2fsck -fyD "$OUT/$1.img" > /dev/null 2>&1 || [ $? -le 2 ]; }

common ext4 -t ext4 -b 4096;                                                      index ext4
common ext4_1k -t ext4 -b 1024;                                             index ext4_1k
common ext4_inline -t ext4 -b 4096 -O inline_data;                                  index ext4_inline
# Version de hachage et signe : posés dans le superbloc, puis `e2fsck -fD` reconstruit les index.
ssv() { debugfs -w -R "ssv $2 $3" "$OUT/$1.img" > /dev/null 2>&1; }
common ext4_nocsum -t ext4 -O ^metadata_csum,uninit_bg; ssv ext4_nocsum def_hash_version tea;  index ext4_nocsum
common ext4_legacy -t ext4; ssv ext4_legacy def_hash_version legacy; ssv ext4_legacy flags 2;  index ext4_legacy
common ext4_metabg -t ext4 -O meta_bg,^resize_inode;                        index ext4_metabg
common ext4_32 -t ext4 -b 4096 -O ^64bit;                                           index ext4_32
common ext3 -t ext3;                                                        index ext3
common ext2 -t ext2 -O ^dir_index

# --- Journal à rejouer --------------------------------------------------------------------
# Une transaction validée remplace le premier bloc de `one` par 4096 octets de « J ».
cp "$OUT/ext4.img" "$OUT/ext4_journal.img"
BLK=$(debugfs -R "bmap /one 0" "$OUT/ext4_journal.img" 2> /dev/null)
head -c 4096 /dev/zero | tr '\0' 'J' > "$OUT/jdata"
debugfs -w -f - "$OUT/ext4_journal.img" > /dev/null 2>&1 <<EOF
jo
jw -b $BLK $OUT/jdata
jc
EOF
echo "$BLK" > "$OUT/journal_block"

ls -la "$OUT"
