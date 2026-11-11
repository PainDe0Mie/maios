//! Répertoires : blocs linéaires (`ext4_dir_entry_2`), index htree et données en ligne.
//!
//! La recherche d'un nom passe par l'index htree quand le dossier en a un : hachage du nom,
//! dichotomie dans chaque niveau d'index, lecture d'une seule feuille (et des suivantes tant
//! que leur hachage de départ marque une collision). Si l'index est incohérent ou d'une
//! version inconnue, la recherche retombe sur un parcours linéaire, toujours exact.

use alloc::{sync::Arc, vec::Vec};

use crate::{
    hash::{self, dx_hash},
    le16, le32, sb, BlockDev, Error, FileKind, Fs, Inode, Result,
};

/// Entrée de dossier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub ino: u32,
    pub kind: FileKind,
    pub name: Vec<u8>,
}

/// Fin de bloc des sommes de contrôle (`ext4_dir_entry_tail`).
const TAIL_FT: u8 = 0xDE;
/// Niveaux d'index au plus (`ext4_dir_htree_level` : 2, ou 3 avec `largedir`).
const MAX_HTREE_LEVELS: u8 = 2;

fn rec_len(raw: u16, bs: usize) -> usize {
    // Blocs de 64 Kio : 65535 et 0 signifient « jusqu'à la fin du bloc ».
    if bs >= 65536 && (raw == 65535 || raw == 0) {
        bs
    } else {
        raw as usize
    }
}

/// Parcourt les entrées d'une zone linéaire (`start..end`). `f` renvoie `false` pour s'arrêter ;
/// le résultat dit si le parcours a été arrêté.
fn parse_entries(b: &[u8], start: usize, end: usize, filetype: bool, mut f: impl FnMut(u32, u8, &[u8]) -> bool) -> Result<bool> {
    let mut off = start;
    while off + 8 <= end {
        let ino = le32(b, off);
        let rl = rec_len(le16(b, off + 4), b.len());
        let name_len = b[off + 6] as usize;
        let ft = b[off + 7];
        if rl < 8 || rl % 4 != 0 || off + rl > end || 8 + name_len > rl {
            return Err(Error::Corrupt("entree de dossier invalide"));
        }
        if ino != 0 && !(name_len == 0 && ft == TAIL_FT) {
            let ft = if filetype { ft } else { 0 };
            if !f(ino, ft, &b[off + 8..off + 8 + name_len]) {
                return Ok(true);
            }
        }
        off += rl;
    }
    Ok(false)
}

impl<D: BlockDev> Fs<D> {
    fn has_filetype(&self) -> bool {
        self.sb.has_incompat(sb::INCOMPAT_FILETYPE)
    }

    /// Bloc `lblk` d'un dossier, somme de contrôle de feuille vérifiée s'il en a une.
    fn dir_block(&self, dir: &Inode, lblk: u64) -> Result<Option<Arc<[u8]>>> {
        let Some(b) = self.file_meta_block(dir, lblk)? else { return Ok(None) };
        if self.has_metadata_csum() {
            let t = self.bs - 12;
            if le32(&b, t) == 0 && le16(&b, t + 4) == 12 && b[t + 6] == 0 && b[t + 7] == TAIL_FT {
                if crate::crc32c(dir.csum_seed, &b[..t]) != le32(&b, t + 8) {
                    return Err(Error::Checksum("bloc de dossier"));
                }
            }
        }
        Ok(Some(b))
    }

    /// Visite toutes les entrées (`.` et `..` compris). `f` renvoie `false` pour s'arrêter.
    pub fn read_dir(&self, dir: &Inode, mut f: impl FnMut(&DirEntry) -> bool) -> Result<()> {
        if !dir.is_dir() {
            return Err(Error::NotDir);
        }
        if dir.is_encrypted() {
            return Err(Error::Unsupported("ext4 : dossier chiffre"));
        }
        let ft = self.has_filetype();
        let mut emit = |ino: u32, t: u8, name: &[u8]| {
            let kind = FileKind::from_dirent(t);
            f(&DirEntry { ino, kind, name: name.to_vec() })
        };
        if dir.has_inline_data() {
            return self.inline_dir(dir, &mut emit).map(|_| ());
        }
        for lblk in 0..self.nblocks(dir) {
            if let Some(b) = self.dir_block(dir, lblk)? {
                if parse_entries(&b, 0, b.len(), ft, &mut emit)? {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Dossier en ligne : `i_block` commence par le numéro du parent, suivi d'entrées ;
    /// `system.data` en porte d'autres. `.` et `..` n'y sont pas écrits : on les fabrique.
    fn inline_dir(&self, dir: &Inode, emit: &mut impl FnMut(u32, u8, &[u8]) -> bool) -> Result<bool> {
        let ib = dir.i_block();
        let ft = self.has_filetype();
        if !emit(dir.ino, 2, b".") || !emit(le32(ib, 0), 2, b"..") {
            return Ok(true);
        }
        let mut first = [0u8; 56];
        first.copy_from_slice(&ib[4..60]);
        if parse_entries(&first, 0, 56, ft, &mut *emit)? {
            return Ok(true);
        }
        let tail = self.inline_tail(dir)?;
        if !tail.is_empty() && parse_entries(&tail, 0, tail.len(), ft, &mut *emit)? {
            return Ok(true);
        }
        Ok(false)
    }

    /// Cherche `name` dans le dossier `dir`.
    pub fn lookup(&self, dir: &Inode, name: &[u8]) -> Result<Option<DirEntry>> {
        if !dir.is_dir() {
            return Err(Error::NotDir);
        }
        if name.is_empty() || name.len() > 255 {
            return Ok(None);
        }
        if dir.flags & crate::inode::EXT4_INDEX_FL != 0
            && self.sb.has_compat(sb::COMPAT_DIR_INDEX)
            && !dir.is_casefolded()
            && !dir.is_encrypted()
            && !dir.has_inline_data()
        {
            // Index incohérent ou d'un format inconnu : on retombe sur le parcours linéaire.
            if let Ok(r) = self.htree_lookup(dir, name) {
                return Ok(r);
            }
        }
        let mut found = None;
        self.read_dir(dir, |e| {
            if e.name == name {
                found = Some(e.clone());
                return false;
            }
            true
        })?;
        Ok(found)
    }

    fn scan_leaf(&self, dir: &Inode, lblk: u64, name: &[u8]) -> Result<Option<DirEntry>> {
        let Some(b) = self.dir_block(dir, lblk)? else { return Ok(None) };
        let mut found = None;
        parse_entries(&b, 0, b.len(), self.has_filetype(), |ino, t, n| {
            if n == name {
                found = Some(DirEntry { ino, kind: FileKind::from_dirent(t), name: n.to_vec() });
                return false;
            }
            true
        })?;
        Ok(found)
    }

    pub(crate) fn htree_lookup(&self, dir: &Inode, name: &[u8]) -> Result<Option<DirEntry>> {
        let root = self.dir_block(dir, 0)?.ok_or(Error::Corrupt("racine htree absente"))?;
        // dx_root : « . » (12 octets), « .. » (bloc entier), puis dx_root_info à l'octet 24.
        if le32(&root, 24) != 0 {
            return Err(Error::Corrupt("dx_root_info"));
        }
        let mut version = root[28];
        let info_len = root[29] as usize;
        let levels = root[30];
        if info_len != 8 || levels >= MAX_HTREE_LEVELS + (self.sb.has_incompat(sb::INCOMPAT_LARGEDIR) as u8) {
            return Err(Error::Corrupt("dx_root_info"));
        }
        if version <= hash::DX_HASH_TEA && self.sb.flags & sb::FLAG_UNSIGNED_HASH != 0 {
            version += 3;
        }
        let h = dx_hash(name, version, &self.sb.hash_seed).ok_or(Error::Unsupported("hachage htree"))?;

        // Pile des niveaux : (bloc du nœud, début des entrées, nombre, entrée choisie).
        let mut frames: Vec<(Arc<[u8]>, usize, usize, usize)> = Vec::new();
        let mut node = root;
        let mut entries_off = 24 + info_len;
        for level in 0..=levels {
            let (count, at) = self.dx_pick(&node, entries_off, h)?;
            frames.push((node.clone(), entries_off, count, at));
            let blk = le32(&node, entries_off + 8 * at + 4) as u64 & 0x0FFF_FFFF;
            if level == levels {
                break;
            }
            node = self.dir_block(dir, blk)?.ok_or(Error::Corrupt("noeud htree absent"))?;
            // dx_node : une fausse entrée de 8 octets (inode 0, bloc entier), puis les entrées.
            if le32(&node, 0) != 0 || rec_len(le16(&node, 4), self.bs) != self.bs {
                return Err(Error::Corrupt("dx_node"));
            }
            entries_off = 8;
        }

        loop {
            let (ref n, off, _, at) = *frames.last().unwrap();
            let leaf = le32(n, off + 8 * at + 4) as u64 & 0x0FFF_FFFF;
            if let Some(e) = self.scan_leaf(dir, leaf, name)? {
                return Ok(Some(e));
            }
            // Suite des collisions (`ext4_htree_next_block`) : on avance d'une entrée au niveau
            // le plus bas qui en a encore, et on continue seulement si le hachage de départ du
            // bloc suivant est le nôtre.
            let mut depth = frames.len();
            loop {
                if depth == 0 {
                    return Ok(None);
                }
                let f = &mut frames[depth - 1];
                f.3 += 1;
                if f.3 < f.2 {
                    break;
                }
                depth -= 1;
            }
            let (ref n, off, _, at) = frames[depth - 1];
            let bhash = le32(n, off + 8 * at);
            if bhash & !1 != h {
                return Ok(None);
            }
            // Redescendre jusqu'à une feuille par les premières entrées.
            frames.truncate(depth);
            while frames.len() < levels as usize + 1 {
                let (ref n, off, _, at) = *frames.last().unwrap();
                let blk = le32(n, off + 8 * at + 4) as u64 & 0x0FFF_FFFF;
                let child = self.dir_block(dir, blk)?.ok_or(Error::Corrupt("noeud htree absent"))?;
                let count = le16(&child, 8 + 2) as usize;
                if count == 0 {
                    return Err(Error::Corrupt("noeud htree vide"));
                }
                frames.push((child, 8, count, 0));
            }
        }
    }

    /// Dichotomie dans les entrées d'un nœud : (nombre d'entrées, entrée qui couvre `h`).
    fn dx_pick(&self, node: &[u8], off: usize, h: u32) -> Result<(usize, usize)> {
        let limit = le16(node, off) as usize;
        let count = le16(node, off + 2) as usize;
        if count == 0 || count > limit || off + 8 * limit > node.len() {
            return Err(Error::Corrupt("dx_countlimit"));
        }
        // L'entrée 0 n'a pas de hachage (elle porte count/limit) : elle couvre tout ce qui est
        // sous le hachage de l'entrée 1.
        let (mut lo, mut hi) = (1usize, count);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if le32(node, off + 8 * mid) > h {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        Ok((count, lo - 1))
    }
}
