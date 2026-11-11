//! Inodes (`struct ext4_inode`).

use alloc::boxed::Box;

use crate::{crc, le16, le32, sb, BlockDev, Error, Fs, Result};

pub const EXT4_INDEX_FL: u32 = 0x1000;
pub const EXT4_ENCRYPT_FL: u32 = 0x800;
pub const EXT4_HUGE_FILE_FL: u32 = 0x40000;
pub const EXT4_EXTENTS_FL: u32 = 0x80000;
pub const EXT4_EA_INODE_FL: u32 = 0x200000;
pub const EXT4_INLINE_DATA_FL: u32 = 0x1000_0000;
pub const EXT4_CASEFOLD_FL: u32 = 0x4000_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Regular,
    Dir,
    Symlink,
    CharDev,
    BlockDev,
    Fifo,
    Socket,
    Unknown,
}

impl FileKind {
    pub fn from_mode(mode: u16) -> FileKind {
        match mode & 0xF000 {
            0x8000 => FileKind::Regular,
            0x4000 => FileKind::Dir,
            0xA000 => FileKind::Symlink,
            0x2000 => FileKind::CharDev,
            0x6000 => FileKind::BlockDev,
            0x1000 => FileKind::Fifo,
            0xC000 => FileKind::Socket,
            _ => FileKind::Unknown,
        }
    }

    /// Type d'une entrée de dossier (`file_type` de `ext4_dir_entry_2`).
    pub fn from_dirent(t: u8) -> FileKind {
        match t {
            1 => FileKind::Regular,
            2 => FileKind::Dir,
            3 => FileKind::CharDev,
            4 => FileKind::BlockDev,
            5 => FileKind::Fifo,
            6 => FileKind::Socket,
            7 => FileKind::Symlink,
            _ => FileKind::Unknown,
        }
    }

    /// Valeur `DT_*` de `getdents64`.
    pub fn dt(self) -> u8 {
        match self {
            FileKind::Fifo => 1,
            FileKind::CharDev => 2,
            FileKind::Dir => 4,
            FileKind::BlockDev => 6,
            FileKind::Regular => 8,
            FileKind::Symlink => 10,
            FileKind::Socket => 12,
            FileKind::Unknown => 0,
        }
    }
}

/// Date (secondes depuis 1970, nanosecondes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Time {
    pub sec: i64,
    pub nsec: u32,
}

impl Time {
    /// Champ de 32 bits signé et son champ `_extra` : 2 bits d'époque (bits 32 et 33 des
    /// secondes) et 30 bits de nanosecondes.
    fn decode(base: u32, extra: Option<u32>) -> Time {
        let mut sec = base as i32 as i64;
        let mut nsec = 0;
        if let Some(x) = extra {
            sec += ((x & 3) as i64) << 32;
            nsec = (x >> 2).min(999_999_999);
        }
        Time { sec, nsec }
    }
}

#[derive(Clone)]
pub struct Inode {
    pub ino: u32,
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub nlink: u16,
    pub flags: u32,
    /// Blocs occupés, en secteurs de 512 octets.
    pub blocks: u64,
    pub generation: u32,
    pub file_acl: u64,
    pub atime: Time,
    pub mtime: Time,
    pub ctime: Time,
    pub crtime: Option<Time>,
    /// Graine des sommes des blocs de cet inode (extents, dossiers, attributs).
    pub(crate) csum_seed: u32,
    /// Inode tel qu'il est sur le disque (`i_block`, attributs étendus dans l'inode).
    raw: Box<[u8]>,
    extra_isize: u16,
}

impl core::fmt::Debug for Inode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Inode")
            .field("ino", &self.ino)
            .field("mode", &format_args!("{:o}", self.mode))
            .field("size", &self.size)
            .field("flags", &format_args!("{:#x}", self.flags))
            .finish()
    }
}

impl Inode {
    pub(crate) fn parse<D: BlockDev>(fs: &Fs<D>, ino: u32, b: &[u8]) -> Result<Inode> {
        let isz = b.len();
        let extra_isize = if isz > 128 { le16(b, 0x80) } else { 0 };
        if isz > 128 && (128 + extra_isize as usize > isz || extra_isize % 4 != 0) {
            return Err(Error::Corrupt("i_extra_isize invalide"));
        }
        let generation = le32(b, 0x64);
        let csum_seed = crc::crc32c_u32(crc::crc32c_u32(fs.csum_seed, ino), generation);
        if fs.has_metadata_csum() {
            let has_hi = extra_isize >= 4;
            let mut tmp = b.to_vec();
            tmp[0x7C] = 0;
            tmp[0x7D] = 0;
            if has_hi {
                tmp[0x82] = 0;
                tmp[0x83] = 0;
            }
            let c = crate::crc32c(csum_seed, &tmp);
            let lo = le16(b, 0x7C) as u32;
            let ok = if has_hi {
                c == lo | (le16(b, 0x82) as u32) << 16
            } else {
                c & 0xFFFF == lo
            };
            if !ok {
                return Err(Error::Checksum("inode"));
            }
        }
        let mode = le16(b, 0x0);
        let nlink = le16(b, 0x1A);
        if mode == 0 && nlink == 0 {
            return Err(Error::Corrupt("inode libre reference"));
        }
        let flags = le32(b, 0x20);
        let fits = |off: usize| isz > 128 && 128 + extra_isize as usize >= off + 4;
        let extra = |off: usize| if fits(off) { Some(le32(b, off)) } else { None };
        let mut blocks = le32(b, 0x1C) as u64;
        if fs.sb.has_ro_compat(sb::RO_COMPAT_HUGE_FILE) {
            blocks |= (le16(b, 0x74) as u64) << 32;
            if flags & EXT4_HUGE_FILE_FL != 0 {
                blocks <<= fs.log_bs - 9;
            }
        }
        let size = le32(b, 0x4) as u64 | (le32(b, 0x6C) as u64) << 32;
        Ok(Inode {
            ino,
            mode,
            uid: le16(b, 0x2) as u32 | (le16(b, 0x78) as u32) << 16,
            gid: le16(b, 0x18) as u32 | (le16(b, 0x7A) as u32) << 16,
            size,
            nlink,
            flags,
            blocks,
            generation,
            file_acl: le32(b, 0x68) as u64 | (le16(b, 0x76) as u64) << 32,
            atime: Time::decode(le32(b, 0x8), extra(0x8C)),
            ctime: Time::decode(le32(b, 0xC), extra(0x84)),
            mtime: Time::decode(le32(b, 0x10), extra(0x88)),
            crtime: if fits(0x90) { Some(Time::decode(le32(b, 0x90), extra(0x94))) } else { None },
            csum_seed,
            raw: Box::from(b),
            extra_isize,
        })
    }

    pub fn kind(&self) -> FileKind {
        FileKind::from_mode(self.mode)
    }

    pub fn is_dir(&self) -> bool {
        self.kind() == FileKind::Dir
    }

    /// Bits de droits (`0o7777`).
    pub fn perm(&self) -> u16 {
        self.mode & 0o7777
    }

    pub fn i_block(&self) -> &[u8] {
        &self.raw[0x28..0x28 + 60]
    }

    pub fn has_inline_data(&self) -> bool {
        self.flags & EXT4_INLINE_DATA_FL != 0
    }

    pub fn uses_extents(&self) -> bool {
        self.flags & EXT4_EXTENTS_FL != 0
    }

    pub fn is_encrypted(&self) -> bool {
        self.flags & EXT4_ENCRYPT_FL != 0
    }

    pub fn is_casefolded(&self) -> bool {
        self.flags & EXT4_CASEFOLD_FL != 0
    }

    /// Lien dont la cible tient dans `i_block` (règle de `ext4_inode_is_fast_symlink`).
    pub fn is_fast_symlink<D: BlockDev>(&self, fs: &Fs<D>) -> bool {
        if self.kind() != FileKind::Symlink {
            return false;
        }
        if self.flags & EXT4_EA_INODE_FL != 0 {
            return self.size != 0 && self.size < 60;
        }
        if self.has_inline_data() {
            return false;
        }
        let ea_blocks = if self.file_acl != 0 { (fs.bs >> 9) as u64 } else { 0 };
        self.blocks.saturating_sub(ea_blocks) == 0
    }

    /// Numéros majeur et mineur d'un fichier de périphérique.
    pub fn rdev(&self) -> (u32, u32) {
        let ib = self.i_block();
        let old = le32(ib, 0);
        if old != 0 {
            return ((old >> 8) & 0xFF, old & 0xFF);
        }
        let new = le32(ib, 4);
        ((new & 0xFFF00) >> 8, (new & 0xFF) | ((new >> 12) & 0xFFF00))
    }

    /// Zone des attributs étendus dans l'inode (après `i_extra_isize`), en-tête compris.
    pub(crate) fn inline_xattr_area(&self) -> &[u8] {
        let start = 128 + self.extra_isize as usize;
        if self.raw.len() <= 128 || start + 4 > self.raw.len() {
            return &[];
        }
        &self.raw[start..]
    }
}
