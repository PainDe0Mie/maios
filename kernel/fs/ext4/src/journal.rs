//! Journal JBD2 (`include/linux/jbd2.h`, `fs/jbd2/recovery.c`), rejoué **en mémoire**.
//!
//! Un volume marqué `needs_recovery` a, dans son journal, des transactions validées pas encore
//! recopiées à leur place. Linux les réécrit sur le disque au montage ; un montage en lecture
//! seule ne doit rien écrire. On fait donc les trois passes de `jbd2_journal_recover`
//! (balayage, révocations, rejeu) en ne retenant que l'emplacement, dans le journal, de la
//! version la plus récente de chaque bloc : les lectures de ce bloc sont ensuite servies depuis
//! le journal ([`crate::Fs::meta_block`]).
//!
//! Formats : en-têtes gros-boutistes, étiquettes v1 (32 ou 64 bits), v2 et v3 (`csum_v3`),
//! blocs « échappés » (dont les 4 premiers octets étaient le nombre magique), révocations 32 ou
//! 64 bits. Une transaction sans bloc de validation, ou dont la somme est fausse (v2/v3), met
//! fin au journal, comme dans Linux.

use alloc::{collections::BTreeMap, vec::Vec};

use crate::{be32, crc32c, BlockDev, Error, Fs, Result};

pub const JBD2_MAGIC: u32 = 0xC03B_3998;

const BT_DESCRIPTOR: u32 = 1;
const BT_COMMIT: u32 = 2;
const BT_SB_V1: u32 = 3;
const BT_SB_V2: u32 = 4;
const BT_REVOKE: u32 = 5;

const FEAT_INCOMPAT_REVOKE: u32 = 0x1;
const FEAT_INCOMPAT_64BIT: u32 = 0x2;
const FEAT_INCOMPAT_CSUM_V2: u32 = 0x8;
const FEAT_INCOMPAT_CSUM_V3: u32 = 0x10;

const TAG_ESCAPE: u32 = 1;
const TAG_SAME_UUID: u32 = 2;
const TAG_LAST: u32 = 8;

/// Le numéro de séquence `a` est après `b` (arithmétique modulo 2³²).
fn tid_gt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

struct Journal<'f, D: BlockDev> {
    fs: &'f Fs<D>,
    ino: crate::Inode,
    first: u32,
    maxlen: u32,
    incompat: u32,
    csum_seed: u32,
}

impl<'f, D: BlockDev> Journal<'f, D> {
    /// Bloc physique du bloc `j` du journal.
    fn phys(&self, j: u32) -> Result<u64> {
        self.fs
            .map(&self.ino, j as u64)?
            .phys
            .ok_or(Error::Corrupt("trou dans le journal"))
    }

    fn block(&self, j: u32) -> Result<(u64, alloc::sync::Arc<[u8]>)> {
        let p = self.phys(j)?;
        Ok((p, self.fs.raw_meta_block(p)?))
    }

    fn next(&self, j: u32) -> u32 {
        if j + 1 >= self.maxlen { self.first } else { j + 1 }
    }

    fn csum(&self) -> bool {
        self.incompat & (FEAT_INCOMPAT_CSUM_V2 | FEAT_INCOMPAT_CSUM_V3) != 0
    }

    fn tag_bytes(&self) -> usize {
        if self.incompat & FEAT_INCOMPAT_CSUM_V3 != 0 {
            return 16;
        }
        let mut sz = 12;
        if self.incompat & FEAT_INCOMPAT_CSUM_V2 != 0 {
            sz += 2;
        }
        if self.incompat & FEAT_INCOMPAT_64BIT != 0 { sz } else { sz - 4 }
    }

    /// Somme d'un bloc de validation (v2/v3) : CRC32C du bloc, somme à zéro.
    fn commit_ok(&self, b: &[u8]) -> bool {
        if !self.csum() {
            return true;
        }
        // struct commit_header : 12 octets d'en-tête, h_chksum_type, h_chksum_size, 2 octets,
        // h_chksum[8] (à l'octet 16 ; seul le premier mot sert en v2/v3).
        let mut tmp = b.to_vec();
        tmp[16..20].fill(0);
        crc32c(self.csum_seed, &tmp) == be32(b, 16)
    }
}

/// Une écriture journalisée : bloc du volume, emplacement dans le journal, échappé.
struct Write {
    fs_block: u64,
    jphys: u64,
    escaped: bool,
}

pub(crate) fn replay<D: BlockDev>(fs: &Fs<D>) -> Result<BTreeMap<u64, (u64, bool)>> {
    let ino = fs.inode(fs.sb.journal_inum)?;
    let mut j = Journal { fs, ino, first: 0, maxlen: 0, incompat: 0, csum_seed: 0 };
    let (_, sb) = j.block(0)?;
    if be32(&sb, 0) != JBD2_MAGIC {
        return Err(Error::Corrupt("superbloc du journal"));
    }
    let bt = be32(&sb, 4);
    if bt != BT_SB_V1 && bt != BT_SB_V2 {
        return Err(Error::Corrupt("superbloc du journal"));
    }
    if be32(&sb, 0xC) as usize != fs.bs {
        return Err(Error::Unsupported("journal de taille de bloc differente"));
    }
    j.maxlen = be32(&sb, 0x10);
    j.first = be32(&sb, 0x14);
    let mut seq = be32(&sb, 0x18);
    let start = be32(&sb, 0x1C);
    if bt == BT_SB_V2 {
        j.incompat = be32(&sb, 0x28);
    }
    if j.first == 0 || j.first >= j.maxlen || (j.maxlen as u64) > fs.nblocks(&j.ino) {
        return Err(Error::Corrupt("geometrie du journal"));
    }
    if j.csum() {
        j.csum_seed = crc32c(!0, &sb[0x30..0x40]);
    }
    if start == 0 {
        return Ok(BTreeMap::new()); // Journal vide : rien à rejouer.
    }

    // Passe 1 : balayage des transactions validées, révocations comprises.
    let mut committed: Vec<(u32, Write)> = Vec::new();
    let mut revoked: BTreeMap<u64, u32> = BTreeMap::new();
    let mut pending: Vec<Write> = Vec::new();
    let mut pending_revokes: Vec<u64> = Vec::new();
    let mut pos = start;
    let tag_bytes = j.tag_bytes();
    let tail = if j.csum() { 4 } else { 0 };
    // Garde-fou : jamais plus de tours que de blocs dans le journal.
    let mut budget = j.maxlen as usize * 2;
    loop {
        if budget == 0 {
            break;
        }
        budget -= 1;
        let (_, b) = j.block(pos)?;
        if be32(&b, 0) != JBD2_MAGIC || be32(&b, 8) != seq {
            break;
        }
        match be32(&b, 4) {
            BT_DESCRIPTOR => {
                let mut off = 12usize;
                let mut data = j.next(pos);
                while off + tag_bytes <= fs.bs - tail {
                    let blocknr_lo = be32(&b, off) as u64;
                    let (flags, hi) = if j.incompat & FEAT_INCOMPAT_CSUM_V3 != 0 {
                        (be32(&b, off + 4), be32(&b, off + 8) as u64)
                    } else {
                        let flags = u16::from_be_bytes([b[off + 6], b[off + 7]]) as u32;
                        let hi = if j.incompat & FEAT_INCOMPAT_64BIT != 0 { be32(&b, off + 8) as u64 } else { 0 };
                        (flags, hi)
                    };
                    let fs_block = blocknr_lo | (hi << 32);
                    pending.push(Write { fs_block, jphys: j.phys(data)?, escaped: flags & TAG_ESCAPE != 0 });
                    data = j.next(data);
                    off += tag_bytes;
                    if flags & TAG_SAME_UUID == 0 {
                        off += 16;
                    }
                    if flags & TAG_LAST != 0 {
                        break;
                    }
                }
                // Le bloc suivant est celui qui suit les données de ce descripteur.
                pos = data;
                continue;
            }
            BT_COMMIT => {
                if !j.commit_ok(&b) {
                    break;
                }
                for w in pending.drain(..) {
                    committed.push((seq, w));
                }
                for r in pending_revokes.drain(..) {
                    let e = revoked.entry(r).or_insert(seq);
                    if tid_gt(seq, *e) {
                        *e = seq;
                    }
                }
                seq = seq.wrapping_add(1);
            }
            BT_REVOKE => {
                if j.incompat & FEAT_INCOMPAT_REVOKE == 0 {
                    return Err(Error::Corrupt("revocation sans la fonction revoke"));
                }
                let count = be32(&b, 12) as usize;
                let rsz = if j.incompat & FEAT_INCOMPAT_64BIT != 0 { 8 } else { 4 };
                let mut off = 16usize;
                while off + rsz <= count.min(fs.bs) {
                    let blk = if rsz == 8 {
                        (be32(&b, off) as u64) << 32 | be32(&b, off + 4) as u64
                    } else {
                        be32(&b, off) as u64
                    };
                    pending_revokes.push(blk);
                    off += rsz;
                }
            }
            _ => break,
        }
        pos = j.next(pos);
    }

    // Passes 2 et 3 : la version la plus récente de chaque bloc non révoqué. Une révocation de
    // séquence R annule les écritures des transactions T <= R.
    let mut overlay = BTreeMap::new();
    for (t, w) in committed {
        if let Some(&r) = revoked.get(&w.fs_block) {
            if !tid_gt(t, r) {
                continue;
            }
        }
        if w.fs_block < fs.sb.blocks_count {
            overlay.insert(w.fs_block, (w.jphys, w.escaped));
        }
    }
    Ok(overlay)
}
