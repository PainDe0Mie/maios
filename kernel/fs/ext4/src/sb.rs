//! Superbloc (`struct ext4_super_block`, 1 Kio à l'octet 1024 du volume).

use crate::{crc32c, le16, le32, Error, Result};

pub const MAGIC: u16 = 0xEF53;

pub const COMPAT_HAS_JOURNAL: u32 = 0x4;
pub const COMPAT_DIR_INDEX: u32 = 0x20;

pub const INCOMPAT_COMPRESSION: u32 = 0x1;
pub const INCOMPAT_FILETYPE: u32 = 0x2;
pub const INCOMPAT_RECOVER: u32 = 0x4;
pub const INCOMPAT_JOURNAL_DEV: u32 = 0x8;
pub const INCOMPAT_META_BG: u32 = 0x10;
pub const INCOMPAT_EXTENTS: u32 = 0x40;
pub const INCOMPAT_64BIT: u32 = 0x80;
pub const INCOMPAT_MMP: u32 = 0x100;
pub const INCOMPAT_FLEX_BG: u32 = 0x200;
pub const INCOMPAT_EA_INODE: u32 = 0x400;
pub const INCOMPAT_DIRDATA: u32 = 0x1000;
pub const INCOMPAT_CSUM_SEED: u32 = 0x2000;
pub const INCOMPAT_LARGEDIR: u32 = 0x4000;
pub const INCOMPAT_INLINE_DATA: u32 = 0x8000;
pub const INCOMPAT_ENCRYPT: u32 = 0x10000;
pub const INCOMPAT_CASEFOLD: u32 = 0x20000;

/// Fonctions incompatibles que ce lecteur sait lire.
pub const INCOMPAT_SUPPORTED: u32 = INCOMPAT_FILETYPE
    | INCOMPAT_RECOVER
    | INCOMPAT_META_BG
    | INCOMPAT_EXTENTS
    | INCOMPAT_64BIT
    | INCOMPAT_MMP
    | INCOMPAT_FLEX_BG
    | INCOMPAT_EA_INODE
    | INCOMPAT_CSUM_SEED
    | INCOMPAT_LARGEDIR
    | INCOMPAT_INLINE_DATA
    // Seuls les inodes chiffrés ou insensibles à la casse sont refusés, pas le volume.
    | INCOMPAT_ENCRYPT
    | INCOMPAT_CASEFOLD;

pub const RO_COMPAT_SPARSE_SUPER: u32 = 0x1;
pub const RO_COMPAT_HUGE_FILE: u32 = 0x8;
pub const RO_COMPAT_GDT_CSUM: u32 = 0x10;
pub const RO_COMPAT_METADATA_CSUM: u32 = 0x400;

/// Drapeaux de `s_flags` : hachage des noms de dossiers signé ou non.
pub const FLAG_UNSIGNED_HASH: u32 = 0x2;

#[derive(Debug, Clone)]
pub struct Superblock {
    pub inodes_count: u32,
    pub blocks_count: u64,
    pub r_blocks_count: u64,
    pub free_blocks_count: u64,
    pub free_inodes_count: u32,
    pub first_data_block: u32,
    pub block_size: u32,
    pub blocks_per_group: u32,
    pub inodes_per_group: u32,
    pub state: u16,
    pub rev_level: u32,
    pub inode_size: u16,
    pub feature_compat: u32,
    pub feature_incompat: u32,
    pub feature_ro_compat: u32,
    pub uuid: [u8; 16],
    pub volume_name: [u8; 16],
    pub journal_inum: u32,
    pub hash_seed: [u32; 4],
    pub def_hash_version: u8,
    pub desc_size: u16,
    pub first_meta_bg: u32,
    pub min_extra_isize: u16,
    pub flags: u32,
    pub checksum_seed: u32,
    pub mkfs_time: u32,
}

impl Superblock {
    pub fn parse(b: &[u8]) -> Result<Superblock> {
        if b.len() < 1024 || le16(b, 0x38) != MAGIC {
            return Err(Error::Corrupt("pas un volume ext2/3/4"));
        }
        let incompat = le32(b, 0x60);
        let ro_compat = le32(b, 0x64);
        let unsupported = incompat & !INCOMPAT_SUPPORTED;
        if unsupported & INCOMPAT_COMPRESSION != 0 {
            return Err(Error::Unsupported("ext4 : compression"));
        }
        if unsupported & INCOMPAT_JOURNAL_DEV != 0 {
            return Err(Error::Unsupported("ext4 : volume de journal externe"));
        }
        if unsupported & INCOMPAT_DIRDATA != 0 {
            return Err(Error::Unsupported("ext4 : dirdata"));
        }
        if unsupported != 0 {
            return Err(Error::Unsupported("ext4 : fonction incompatible inconnue"));
        }
        if ro_compat & RO_COMPAT_METADATA_CSUM != 0 {
            if b[0x175] != 1 {
                return Err(Error::Unsupported("ext4 : type de somme de controle inconnu"));
            }
            let c = crc32c(!0, &b[..0x3FC]);
            if c != le32(b, 0x3FC) {
                return Err(Error::Checksum("superbloc"));
            }
        }
        let log_bs = le32(b, 0x18);
        if log_bs > 6 {
            return Err(Error::Corrupt("taille de bloc invalide"));
        }
        let block_size = 1024u32 << log_bs;
        let is64 = incompat & INCOMPAT_64BIT != 0;
        let hi = |o: usize| if is64 { (le32(b, o) as u64) << 32 } else { 0 };
        let rev_level = le32(b, 0x4C);
        let inode_size = if rev_level == 0 { 128 } else { le16(b, 0x58) };
        if inode_size < 128 || !inode_size.is_power_of_two() || inode_size as u32 > block_size {
            return Err(Error::Corrupt("taille d'inode invalide"));
        }
        let desc_size = if is64 { le16(b, 0xFE).max(64) } else { 32 };
        if !desc_size.is_power_of_two() || desc_size > 1024 {
            return Err(Error::Corrupt("taille des descripteurs de groupes invalide"));
        }
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&b[0x68..0x78]);
        let mut volume_name = [0u8; 16];
        volume_name.copy_from_slice(&b[0x78..0x88]);
        let sb = Superblock {
            inodes_count: le32(b, 0x0),
            blocks_count: le32(b, 0x4) as u64 | hi(0x150),
            r_blocks_count: le32(b, 0x8) as u64 | hi(0x154),
            free_blocks_count: le32(b, 0xC) as u64 | hi(0x158),
            free_inodes_count: le32(b, 0x10),
            first_data_block: le32(b, 0x14),
            block_size,
            blocks_per_group: le32(b, 0x20),
            inodes_per_group: le32(b, 0x28),
            state: le16(b, 0x3A),
            rev_level,
            inode_size,
            feature_compat: le32(b, 0x5C),
            feature_incompat: incompat,
            feature_ro_compat: ro_compat,
            uuid,
            volume_name,
            journal_inum: le32(b, 0xE0),
            hash_seed: [le32(b, 0xEC), le32(b, 0xF0), le32(b, 0xF4), le32(b, 0xF8)],
            def_hash_version: b[0xFC],
            desc_size,
            first_meta_bg: le32(b, 0x104),
            min_extra_isize: le16(b, 0x15C),
            flags: le32(b, 0x160),
            checksum_seed: le32(b, 0x270),
            mkfs_time: le32(b, 0x108),
        };
        if sb.blocks_per_group == 0 || sb.inodes_per_group == 0 || sb.blocks_per_group > 8 * block_size {
            return Err(Error::Corrupt("geometrie des groupes invalide"));
        }
        if (sb.first_data_block as u64) >= sb.blocks_count {
            return Err(Error::Corrupt("premier bloc de donnees invalide"));
        }
        let ng = sb.group_count();
        if ng == 0 || (sb.inodes_count as u64) > ng * sb.inodes_per_group as u64 {
            return Err(Error::Corrupt("nombre d'inodes incoherent"));
        }
        Ok(sb)
    }

    pub fn has_compat(&self, f: u32) -> bool {
        self.feature_compat & f != 0
    }
    pub fn has_incompat(&self, f: u32) -> bool {
        self.feature_incompat & f != 0
    }
    pub fn has_ro_compat(&self, f: u32) -> bool {
        self.feature_ro_compat & f != 0
    }

    /// Les descripteurs de groupes portent une somme (`metadata_csum` ou `gdt_csum`).
    pub fn has_group_csums(&self) -> bool {
        self.has_ro_compat(RO_COMPAT_METADATA_CSUM) || self.has_ro_compat(RO_COMPAT_GDT_CSUM)
    }

    pub fn group_count(&self) -> u64 {
        let data = self.blocks_count - self.first_data_block as u64;
        data.div_ceil(self.blocks_per_group as u64)
    }

    pub fn group_first_block(&self, g: u64) -> u64 {
        self.first_data_block as u64 + g * self.blocks_per_group as u64
    }

    /// Le groupe porte une copie du superbloc (`sparse_super` : groupes 0, 1 et puissances de
    /// 3, 5 et 7).
    pub fn group_has_super(&self, g: u64) -> bool {
        if !self.has_ro_compat(RO_COMPAT_SPARSE_SUPER) || g <= 1 {
            return true;
        }
        if g % 2 == 0 {
            return false;
        }
        for base in [3u64, 5, 7] {
            let mut p = base;
            while p < g {
                p *= base;
            }
            if p == g {
                return true;
            }
        }
        false
    }

    /// Nom du volume (jusqu'au premier octet nul).
    pub fn label(&self) -> &[u8] {
        let n = self.volume_name.iter().position(|&c| c == 0).unwrap_or(16);
        &self.volume_name[..n]
    }
}
