//! Correspondance bloc logique → bloc physique : arbres d'extents (ext4) et cartes de blocs à
//! indirections (ext2/ext3).
//!
//! [`Fs::map`] renvoie toujours une *suite* : le bloc physique du bloc demandé et le nombre de
//! blocs contigus qui suivent dans le même extent (ou la longueur du trou). La lecture avance
//! ainsi d'un extent entier par appel, et chaque extent devient une seule requête au disque.

use crate::{le16, le32, BlockDev, Error, Fs, Inode, Result};

const EXTENT_MAGIC: u16 = 0xF30A;
const MAX_DEPTH: u16 = 5;
/// Longueur maximale d'un extent initialisé ; au-delà, l'extent est non initialisé.
const INIT_MAX_LEN: u16 = 32768;

/// Suite de blocs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    /// Bloc physique du premier bloc ; `None` pour un trou ou un extent non initialisé (zéros).
    pub phys: Option<u64>,
    /// Nombre de blocs (au moins 1).
    pub len: u64,
}

struct Header {
    entries: u16,
    max: u16,
    depth: u16,
}

fn header(b: &[u8]) -> Result<Header> {
    if b.len() < 12 || le16(b, 0) != EXTENT_MAGIC {
        return Err(Error::Corrupt("en-tete d'extents invalide"));
    }
    let h = Header { entries: le16(b, 2), max: le16(b, 4), depth: le16(b, 6) };
    if h.entries > h.max || 12 + 12 * h.max as usize > b.len() || h.depth > MAX_DEPTH {
        return Err(Error::Corrupt("noeud d'extents invalide"));
    }
    Ok(h)
}

/// Dernière entrée dont le premier bloc logique est `<= lblk` (recherche dichotomique).
fn search(b: &[u8], n: usize, lblk: u32) -> Option<usize> {
    let (mut lo, mut hi) = (0usize, n);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if le32(b, 12 + 12 * mid) <= lblk {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo.checked_sub(1)
}

impl<D: BlockDev> Fs<D> {
    pub(crate) fn map(&self, ino: &Inode, lblk: u64) -> Result<Run> {
        if ino.uses_extents() {
            self.map_extents(ino, lblk)
        } else {
            self.map_indirect(ino, lblk)
        }
    }

    fn map_extents(&self, ino: &Inode, lblk: u64) -> Result<Run> {
        if lblk > u32::MAX as u64 {
            return Ok(Run { phys: None, len: 1 });
        }
        let l = lblk as u32;
        // Fin du trou si `lblk` tombe après le dernier extent d'une feuille : premier bloc de
        // l'entrée suivante d'un niveau supérieur.
        let mut limit: u64 = 1 << 32;
        let mut node_holder;
        let mut node: &[u8] = ino.i_block();
        let mut expect_depth: Option<u16> = None;
        loop {
            let h = header(node)?;
            if let Some(d) = expect_depth {
                if h.depth != d {
                    return Err(Error::Corrupt("profondeur d'extents incoherente"));
                }
            }
            let n = h.entries as usize;
            if h.depth == 0 {
                let i = search(node, n, l);
                let next = |k: usize| if k < n { le32(node, 12 + 12 * k) as u64 } else { limit };
                return Ok(match i {
                    None => Run { phys: None, len: (next(0).min(limit) - lblk).max(1) },
                    Some(i) => {
                        let e = 12 + 12 * i;
                        let first = le32(node, e) as u64;
                        let raw_len = le16(node, e + 4);
                        let (len, uninit) = if raw_len > INIT_MAX_LEN {
                            ((raw_len - INIT_MAX_LEN) as u64, true)
                        } else {
                            (raw_len as u64, false)
                        };
                        let start = (le16(node, e + 6) as u64) << 32 | le32(node, e + 8) as u64;
                        if lblk < first + len {
                            let off = lblk - first;
                            Run { phys: if uninit { None } else { Some(start + off) }, len: len - off }
                        } else {
                            Run { phys: None, len: (next(i + 1).min(limit) - lblk).max(1) }
                        }
                    }
                });
            }
            let i = match search(node, n, l) {
                Some(i) => i,
                None => {
                    // Avant le premier index : trou jusqu'à lui.
                    let first = if n > 0 { le32(node, 12) as u64 } else { limit };
                    return Ok(Run { phys: None, len: (first.min(limit) - lblk).max(1) });
                }
            };
            if i + 1 < n {
                limit = limit.min(le32(node, 12 + 12 * (i + 1)) as u64);
            }
            let e = 12 + 12 * i;
            let child = le32(node, e + 4) as u64 | (le16(node, e + 8) as u64) << 32;
            let blk = self.meta_block(child)?;
            self.verify_extent_block(ino, &blk)?;
            expect_depth = Some(h.depth - 1);
            node_holder = blk;
            node = &node_holder;
        }
    }

    fn verify_extent_block(&self, ino: &Inode, b: &[u8]) -> Result<()> {
        if !self.has_metadata_csum() {
            return Ok(());
        }
        let h = header(b)?;
        let tail = 12 + 12 * h.max as usize;
        if tail + 4 > b.len() {
            return Err(Error::Corrupt("pas de place pour la somme des extents"));
        }
        if crate::crc32c(ino.csum_seed, &b[..tail]) != le32(b, tail) {
            return Err(Error::Checksum("bloc d'extents"));
        }
        Ok(())
    }

    /// Cartes de blocs d'ext2/ext3 : 12 blocs directs, puis indirection simple, double, triple.
    fn map_indirect(&self, ino: &Inode, lblk: u64) -> Result<Run> {
        let apb = (self.bs / 4) as u64;
        let ib = ino.i_block();
        if lblk < 12 {
            let i = lblk as usize;
            return Ok(run_in(|k| le32(ib, 4 * k), i, 12));
        }
        let mut rel = lblk - 12;
        // (niveaux d'indirection, entrée de `i_block`, blocs couverts par un fils)
        let mut levels = 1u32;
        let mut slot = 12usize;
        let mut span = 1u64;
        loop {
            let cover = span * apb;
            if rel < cover {
                break;
            }
            rel -= cover;
            levels += 1;
            slot += 1;
            span *= apb;
            if slot > 14 {
                return Ok(Run { phys: None, len: 1 });
            }
        }
        let mut blk = le32(ib, 4 * slot) as u64;
        let mut covered = span * apb;
        for _ in 0..levels {
            if blk == 0 {
                // Indirection absente : tout ce qu'elle couvre est un trou.
                return Ok(Run { phys: None, len: (covered - rel).max(1) });
            }
            let b = self.meta_block(blk)?;
            covered /= apb;
            let idx = (rel / covered) as usize;
            if covered == 1 {
                return Ok(run_in(|k| le32(&b, 4 * k), idx, apb as usize));
            }
            rel %= covered;
            blk = le32(&b, 4 * idx) as u64;
        }
        Err(Error::Corrupt("carte de blocs"))
    }
}

/// Suite à partir de l'entrée `i` d'un tableau de `n` numéros de blocs : blocs physiquement
/// contigus, ou zéros consécutifs (trou).
fn run_in(get: impl Fn(usize) -> u32, i: usize, n: usize) -> Run {
    let first = get(i);
    let mut len = 1u64;
    let mut k = i + 1;
    while k < n {
        let v = get(k);
        let contiguous = if first == 0 { v == 0 } else { v as u64 == first as u64 + len };
        if !contiguous {
            break;
        }
        len += 1;
        k += 1;
    }
    Run { phys: if first == 0 { None } else { Some(first as u64) }, len }
}
