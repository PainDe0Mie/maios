//! # ext4 pour MaiOS (MaiSS)
//!
//! Cœur du système de fichiers ext4, sans dépendance et sans `unsafe` hors du CRC matériel,
//! testé sur l'hôte contre des images produites par `mke2fs` (voir `src/tests.rs` et
//! `tests/mkimages.sh`). La partie noyau (`kernel/fs/ext4_vfs`) fournit l'accès au disque
//! ([`BlockDev`]), les caches et le branchement au VFS.
//!
//! Ce qui est lu, au format exact de Linux (`Documentation/filesystems/ext4/`) :
//! * superbloc, descripteurs de groupes 32 ou 64 octets, `meta_bg`, `flex_bg`, `sparse_super` ;
//! * inodes (champs étendus, dates en nanosecondes avec les bits d'époque, `huge_file`) ;
//! * arbres d'extents de toute profondeur, extents non initialisés lus comme des zéros, trous ;
//!   cartes de blocs d'ext2/ext3 (indirections simple, double, triple) ;
//! * répertoires linéaires et indexés (htree : hachages *legacy*, *half-MD4* et *TEA*, signés
//!   ou non, suite des collisions), avec repli sur la lecture linéaire si l'index est abîmé ;
//! * données en ligne (`inline_data`) des fichiers et des répertoires ;
//! * attributs étendus dans l'inode, dans un bloc, ou dans un inode à part (`ea_inode`) ;
//! * liens symboliques rapides et lents, fichiers de périphérique ;
//! * sommes de contrôle `metadata_csum` (CRC32C) de tout ce qui est lu, et `gdt_csum` (CRC16) ;
//! * journal JBD2 (v2 et v3, 64 bits, révocations) : un volume démonté sans propreté est rejoué
//!   **en mémoire** (table des blocs les plus récents du journal), sans écrire sur le disque.
//!
//! Les lectures de données se font par extent entier : une suite de blocs contigus sur le
//! disque devient une seule requête, directement dans le tampon de l'appelant.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod crc;
mod dir;
mod extent;
mod hash;
mod inode;
mod journal;
mod sb;
mod xattr;

#[cfg(test)]
mod tests;

use alloc::{collections::BTreeMap, sync::Arc, vec, vec::Vec};

pub use crc::{crc16, crc32c};
pub use dir::DirEntry;
pub use hash::dx_hash;
pub use inode::{FileKind, Inode, Time};
pub use sb::Superblock;
pub use xattr::Xattr;

/// Numéro d'inode de la racine.
pub const ROOT_INO: u32 = 2;

/// Taille d'un secteur : les lectures demandées au [`BlockDev`] en sont des multiples.
pub const SECTOR: u64 = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Erreur de l'appareil.
    Io,
    /// Structure incohérente.
    Corrupt(&'static str),
    /// Somme de contrôle fausse.
    Checksum(&'static str),
    /// Fonction d'ext4 non prise en charge.
    Unsupported(&'static str),
    NotFound,
    NotDir,
    IsDir,
}

impl Error {
    pub fn as_str(&self) -> &'static str {
        match self {
            Error::Io => "erreur d'entree-sortie",
            Error::Corrupt(s) | Error::Checksum(s) | Error::Unsupported(s) => s,
            Error::NotFound => "introuvable",
            Error::NotDir => "pas un dossier",
            Error::IsDir => "est un dossier",
        }
    }
}

pub type Result<T> = core::result::Result<T, Error>;

/// Accès à l'appareil qui porte le volume.
///
/// `offset` et la longueur des tampons sont toujours des multiples de [`SECTOR`] ; les
/// requêtes peuvent arriver de plusieurs cœurs à la fois.
pub trait BlockDev: Send + Sync {
    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<()>;

    /// Bloc de métadonnées de `len` octets à `offset` (multiple de la taille de bloc). Les
    /// métadonnées (tables d'inodes, extents, dossiers) passent toutes par ici : l'implémentation
    /// du noyau les garde en cache.
    fn meta(&self, offset: u64, len: usize) -> Result<Arc<[u8]>> {
        let mut v = vec![0u8; len];
        self.read(offset, &mut v)?;
        Ok(Arc::from(v.into_boxed_slice()))
    }
}

impl<T: BlockDev + ?Sized> BlockDev for Arc<T> {
    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        (**self).read(offset, buf)
    }
    fn meta(&self, offset: u64, len: usize) -> Result<Arc<[u8]>> {
        (**self).meta(offset, len)
    }
}

// --- Lecture des entiers petit-boutistes ------------------------------------------------------

#[inline]
pub(crate) fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
#[inline]
pub(crate) fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
#[inline]
pub(crate) fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Descripteur de groupe (ce qui sert à la lecture).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Group {
    pub inode_table: u64,
    pub flags: u16,
}

pub(crate) const BG_INODE_UNINIT: u16 = 0x1;

/// Statistiques du volume (`statfs`).
#[derive(Debug, Clone, Copy)]
pub struct StatFs {
    pub block_size: u32,
    pub blocks: u64,
    pub free_blocks: u64,
    pub avail_blocks: u64,
    pub inodes: u64,
    pub free_inodes: u64,
    pub name_max: u32,
}

/// Volume ext4 monté en lecture.
pub struct Fs<D: BlockDev> {
    dev: D,
    pub sb: Superblock,
    groups: Vec<Group>,
    pub(crate) bs: usize,
    pub(crate) log_bs: u32,
    /// Graine des sommes `metadata_csum` (`s_csum_seed` de Linux).
    pub(crate) csum_seed: u32,
    /// Blocs dont la version la plus récente est dans le journal (rejeu en mémoire) :
    /// bloc du volume → (bloc physique dans le journal, bloc « échappé »).
    journal_overlay: BTreeMap<u64, (u64, bool)>,
}

impl<D: BlockDev> Fs<D> {
    /// Monte le volume : superbloc, descripteurs de groupes, puis rejeu du journal s'il le faut.
    pub fn mount(dev: D) -> Result<Fs<D>> {
        let mut raw = vec![0u8; 1024];
        dev.read(1024, &mut raw)?;
        let sb = Superblock::parse(&raw)?;
        let bs = sb.block_size as usize;
        let csum_seed = if sb.has_incompat(sb::INCOMPAT_CSUM_SEED) {
            sb.checksum_seed
        } else {
            crc32c(!0, &sb.uuid)
        };
        let mut fs = Fs {
            dev,
            groups: Vec::new(),
            bs,
            log_bs: sb.block_size.trailing_zeros(),
            csum_seed,
            sb,
            journal_overlay: BTreeMap::new(),
        };
        fs.load_groups()?;
        if fs.sb.has_incompat(sb::INCOMPAT_RECOVER) {
            if fs.sb.has_compat(sb::COMPAT_HAS_JOURNAL) && fs.sb.journal_inum != 0 {
                fs.journal_overlay = journal::replay(&fs)?;
                // Les descripteurs de groupes ont pu changer dans le journal.
                if !fs.journal_overlay.is_empty() {
                    fs.load_groups()?;
                }
            } else {
                return Err(Error::Unsupported("journal externe a rejouer"));
            }
        }
        Ok(fs)
    }

    pub fn device(&self) -> &D {
        &self.dev
    }

    pub fn block_size(&self) -> usize {
        self.bs
    }

    /// Nombre de blocs que le rejeu du journal a remplacés.
    pub fn journal_replayed_blocks(&self) -> usize {
        self.journal_overlay.len()
    }

    pub fn has_metadata_csum(&self) -> bool {
        self.sb.has_ro_compat(sb::RO_COMPAT_METADATA_CSUM)
    }

    pub fn statfs(&self) -> StatFs {
        StatFs {
            block_size: self.bs as u32,
            blocks: self.sb.blocks_count,
            free_blocks: self.sb.free_blocks_count,
            avail_blocks: self.sb.free_blocks_count.saturating_sub(self.sb.r_blocks_count),
            inodes: self.sb.inodes_count as u64,
            free_inodes: self.sb.free_inodes_count as u64,
            name_max: 255,
        }
    }

    // --- Blocs -------------------------------------------------------------------------------

    /// Bloc de métadonnées `blk` (journal rejoué compris).
    pub(crate) fn meta_block(&self, blk: u64) -> Result<Arc<[u8]>> {
        if blk >= self.sb.blocks_count {
            return Err(Error::Corrupt("bloc hors du volume"));
        }
        if let Some(&(jblk, escaped)) = self.journal_overlay.get(&blk) {
            let b = self.dev.meta(jblk << self.log_bs, self.bs)?;
            if escaped {
                let mut v = b.to_vec();
                v[0..4].copy_from_slice(&journal::JBD2_MAGIC.to_be_bytes());
                return Ok(Arc::from(v.into_boxed_slice()));
            }
            return Ok(b);
        }
        self.dev.meta(blk << self.log_bs, self.bs)
    }

    /// Bloc lu sans passer par le journal (le journal lui-même).
    pub(crate) fn raw_meta_block(&self, blk: u64) -> Result<Arc<[u8]>> {
        if blk >= self.sb.blocks_count {
            return Err(Error::Corrupt("bloc hors du volume"));
        }
        self.dev.meta(blk << self.log_bs, self.bs)
    }

    /// Lit `out.len()` octets à l'octet `off` du volume, en évitant toute copie pour la partie
    /// alignée sur les secteurs.
    fn read_bytes(&self, off: u64, out: &mut [u8]) -> Result<()> {
        if out.is_empty() {
            return Ok(());
        }
        let end = off + out.len() as u64;
        let a = (off + SECTOR - 1) & !(SECTOR - 1);
        let b = end & !(SECTOR - 1);
        if a >= b {
            // Tout tient dans un ou deux secteurs partiels.
            let s = off & !(SECTOR - 1);
            let e = (end + SECTOR - 1) & !(SECTOR - 1);
            let mut tmp = vec![0u8; (e - s) as usize];
            self.dev.read(s, &mut tmp)?;
            let o = (off - s) as usize;
            out.copy_from_slice(&tmp[o..o + out.len()]);
            return Ok(());
        }
        if off < a {
            let mut tmp = [0u8; SECTOR as usize];
            self.dev.read(a - SECTOR, &mut tmp)?;
            let n = (a - off) as usize;
            out[..n].copy_from_slice(&tmp[SECTOR as usize - n..]);
        }
        let mid = &mut out[(a - off) as usize..(b - off) as usize];
        self.dev.read(a, mid)?;
        if end > b {
            let mut tmp = [0u8; SECTOR as usize];
            self.dev.read(b, &mut tmp)?;
            let n = (end - b) as usize;
            let o = (b - off) as usize;
            out[o..].copy_from_slice(&tmp[..n]);
        }
        Ok(())
    }

    /// Lit des données à partir du bloc physique `pblk`, décalage `in_blk`, en tenant compte du
    /// journal rejoué bloc par bloc s'il y en a un.
    fn read_phys(&self, pblk: u64, in_blk: usize, out: &mut [u8]) -> Result<()> {
        let nblocks = ((in_blk + out.len() + self.bs - 1) >> self.log_bs) as u64;
        if pblk + nblocks > self.sb.blocks_count {
            return Err(Error::Corrupt("extent hors du volume"));
        }
        if self.journal_overlay.is_empty()
            || self.journal_overlay.range(pblk..pblk + nblocks).next().is_none()
        {
            return self.read_bytes((pblk << self.log_bs) + in_blk as u64, out);
        }
        // Lent mais exact : bloc par bloc, ceux du journal à part.
        let mut done = 0usize;
        let mut blk = pblk;
        let mut skip = in_blk;
        while done < out.len() {
            let n = (self.bs - skip).min(out.len() - done);
            if self.journal_overlay.contains_key(&blk) {
                let b = self.meta_block(blk)?;
                out[done..done + n].copy_from_slice(&b[skip..skip + n]);
            } else {
                self.read_bytes((blk << self.log_bs) + skip as u64, &mut out[done..done + n])?;
            }
            done += n;
            blk += 1;
            skip = 0;
        }
        Ok(())
    }

    // --- Groupes -----------------------------------------------------------------------------

    fn load_groups(&mut self) -> Result<()> {
        let sb = &self.sb;
        let ngroups = sb.group_count() as usize;
        let ds = sb.desc_size as usize;
        let per_block = self.bs / ds;
        let meta_bg = sb.has_incompat(sb::INCOMPAT_META_BG);
        let first_meta = if meta_bg { sb.first_meta_bg as usize } else { usize::MAX };
        let mut groups = Vec::with_capacity(ngroups);
        let mut cur_blk = u64::MAX;
        let mut cur: Option<Arc<[u8]>> = None;
        for g in 0..ngroups {
            let dblk = g / per_block;
            let blk = if dblk < first_meta {
                sb.first_data_block as u64 + 1 + dblk as u64
            } else {
                // `meta_bg` : le bloc de descripteurs du méta-groupe `m` est au début du premier
                // groupe du méta-groupe (après la copie du superbloc s'il y en a une).
                let first_group = (dblk * per_block) as u64;
                sb.group_first_block(first_group) + sb.group_has_super(first_group) as u64
            };
            if blk != cur_blk {
                cur = Some(self.meta_block(blk)?);
                cur_blk = blk;
            }
            let b = cur.as_ref().unwrap();
            let d = &b[(g % per_block) * ds..(g % per_block) * ds + ds];
            self.verify_group_desc(g as u32, d)?;
            let mut inode_table = le32(d, 0x8) as u64;
            if ds >= 64 {
                inode_table |= (le32(d, 0x28) as u64) << 32;
            }
            if inode_table == 0 || inode_table >= sb.blocks_count {
                return Err(Error::Corrupt("table d'inodes hors du volume"));
            }
            groups.push(Group { inode_table, flags: le16(d, 0x12) });
        }
        self.groups = groups;
        Ok(())
    }

    fn verify_group_desc(&self, g: u32, d: &[u8]) -> Result<()> {
        let stored = le16(d, 0x1E);
        if self.has_metadata_csum() {
            let mut tmp = d.to_vec();
            tmp[0x1E] = 0;
            tmp[0x1F] = 0;
            let c = crc32c(crc::crc32c_u32(self.csum_seed, g), &tmp) as u16;
            if c != stored {
                return Err(Error::Checksum("descripteur de groupe"));
            }
        } else if self.sb.has_ro_compat(sb::RO_COMPAT_GDT_CSUM) {
            let mut c = crc16(!0, &self.sb.uuid);
            c = crc16(c, &g.to_le_bytes());
            c = crc16(c, &d[..0x1E]);
            if d.len() > 0x20 {
                c = crc16(c, &d[0x20..]);
            }
            if c != stored {
                return Err(Error::Checksum("descripteur de groupe (crc16)"));
            }
        }
        Ok(())
    }

    pub(crate) fn group(&self, g: usize) -> Result<Group> {
        self.groups.get(g).copied().ok_or(Error::Corrupt("groupe inexistant"))
    }

    // --- Inodes ------------------------------------------------------------------------------

    pub fn root(&self) -> Result<Inode> {
        self.inode(ROOT_INO)
    }

    /// Lit et vérifie l'inode `ino`.
    pub fn inode(&self, ino: u32) -> Result<Inode> {
        if ino == 0 || ino > self.sb.inodes_count {
            return Err(Error::Corrupt("numero d'inode invalide"));
        }
        let idx = (ino - 1) as u64;
        let ipg = self.sb.inodes_per_group as u64;
        let grp = self.group((idx / ipg) as usize)?;
        if grp.flags & BG_INODE_UNINIT != 0 && self.sb.has_group_csums() {
            return Err(Error::Corrupt("inode d'un groupe non initialise"));
        }
        let isz = self.sb.inode_size as u64;
        let byte = (idx % ipg) * isz;
        let blk = grp.inode_table + (byte >> self.log_bs);
        let b = self.meta_block(blk)?;
        let o = (byte & (self.bs as u64 - 1)) as usize;
        Inode::parse(self, ino, &b[o..o + isz as usize])
    }

    // --- Données -----------------------------------------------------------------------------

    /// Lit à partir de `off` ; renvoie le nombre d'octets lus (0 en fin de fichier).
    pub fn read(&self, ino: &Inode, off: u64, buf: &mut [u8]) -> Result<usize> {
        if off >= ino.size || buf.is_empty() {
            return Ok(0);
        }
        let n = ((ino.size - off) as usize).min(buf.len());
        if ino.has_inline_data() {
            let data = self.inline_data(ino)?;
            let start = off as usize;
            let avail = data.len().saturating_sub(start).min(n);
            buf[..avail].copy_from_slice(&data[start..start + avail]);
            buf[avail..n].fill(0);
            return Ok(n);
        }
        let mut done = 0usize;
        while done < n {
            let pos = off + done as u64;
            let lblk = pos >> self.log_bs;
            let in_blk = (pos & (self.bs as u64 - 1)) as usize;
            let run = self.map(ino, lblk)?;
            let run_bytes = (run.len as usize)
                .saturating_mul(self.bs)
                .saturating_sub(in_blk)
                .max(1);
            let take = run_bytes.min(n - done);
            let dst = &mut buf[done..done + take];
            match run.phys {
                Some(p) => self.read_phys(p, in_blk, dst)?,
                None => dst.fill(0),
            }
            done += take;
        }
        Ok(n)
    }

    /// Contenu complet (petits fichiers, cibles de liens, valeurs d'attributs dans un inode).
    pub fn read_all(&self, ino: &Inode, max: usize) -> Result<Vec<u8>> {
        if ino.size > max as u64 {
            return Err(Error::Corrupt("fichier trop grand"));
        }
        let mut v = vec![0u8; ino.size as usize];
        let n = self.read(ino, 0, &mut v)?;
        v.truncate(n);
        Ok(v)
    }

    /// Cible d'un lien symbolique.
    pub fn read_link(&self, ino: &Inode) -> Result<Vec<u8>> {
        if ino.kind() != FileKind::Symlink {
            return Err(Error::Corrupt("pas un lien symbolique"));
        }
        if ino.is_fast_symlink(self) {
            let n = (ino.size as usize).min(60);
            return Ok(ino.i_block()[..n].to_vec());
        }
        self.read_all(ino, 4096)
    }

    /// Bloc logique `lblk` d'un fichier, lu en métadonnée (dossiers, extents).
    pub(crate) fn file_meta_block(&self, ino: &Inode, lblk: u64) -> Result<Option<Arc<[u8]>>> {
        match self.map(ino, lblk)?.phys {
            Some(p) => Ok(Some(self.meta_block(p)?)),
            None => Ok(None),
        }
    }

    /// Nombre de blocs logiques d'un fichier.
    pub(crate) fn nblocks(&self, ino: &Inode) -> u64 {
        (ino.size + self.bs as u64 - 1) >> self.log_bs
    }
}
