//! Attributs étendus (`fs/ext4/xattr.h`) : dans l'inode (après `i_extra_isize`), dans un bloc
//! (`i_file_acl`), ou valeur dans un inode à part (`ea_inode`). Les données en ligne
//! (`inline_data`) sont l'attribut `system.data`.

use alloc::vec::Vec;

use crate::{le16, le32, BlockDev, Error, Fs, Inode, Result};

const XATTR_MAGIC: u32 = 0xEA02_0000;
const INDEX_SYSTEM: u8 = 7;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Xattr {
    /// Nom complet, préfixe compris (`user.mime_type`, `security.capability`…).
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

fn prefix(index: u8) -> Option<&'static [u8]> {
    Some(match index {
        0 => b"",
        1 => b"user.",
        2 => b"system.posix_acl_access",
        3 => b"system.posix_acl_default",
        4 => b"trusted.",
        6 => b"security.",
        7 => b"system.",
        8 => b"system.richacl",
        _ => return None,
    })
}

struct RawEntry<'a> {
    index: u8,
    name: &'a [u8],
    value_offs: usize,
    value_inum: u32,
    value_size: usize,
}

/// Parcourt les entrées de `entries` ; `f` reçoit chacune.
fn walk<'a>(entries: &'a [u8], mut f: impl FnMut(RawEntry<'a>) -> Result<bool>) -> Result<()> {
    let mut off = 0usize;
    while off + 4 <= entries.len() && le32(entries, off) != 0 {
        if off + 16 > entries.len() {
            return Err(Error::Corrupt("attribut etendu tronque"));
        }
        let name_len = entries[off] as usize;
        let end = off + 16 + name_len;
        if end > entries.len() {
            return Err(Error::Corrupt("nom d'attribut etendu tronque"));
        }
        let e = RawEntry {
            index: entries[off + 1],
            name: &entries[off + 16..end],
            value_offs: le16(entries, off + 2) as usize,
            value_inum: le32(entries, off + 4),
            value_size: le32(entries, off + 8) as usize,
        };
        if !f(e)? {
            return Ok(());
        }
        off = (end + 3) & !3;
    }
    Ok(())
}

impl<D: BlockDev> Fs<D> {
    fn xattr_value(&self, base: &[u8], e: &RawEntry) -> Result<Vec<u8>> {
        if e.value_inum != 0 {
            let vi = self.inode(e.value_inum)?;
            if vi.flags & crate::inode::EXT4_EA_INODE_FL == 0 {
                return Err(Error::Corrupt("inode de valeur d'attribut invalide"));
            }
            let mut v = self.read_all(&vi, 64 << 20)?;
            v.truncate(e.value_size);
            return Ok(v);
        }
        let end = e.value_offs.checked_add(e.value_size).ok_or(Error::Corrupt("attribut"))?;
        if end > base.len() {
            return Err(Error::Corrupt("valeur d'attribut etendu hors limites"));
        }
        Ok(base[e.value_offs..end].to_vec())
    }

    /// Visite les attributs de l'inode puis ceux de son bloc.
    fn each_xattr(&self, ino: &Inode, mut f: impl FnMut(u8, &[u8], Vec<u8>) -> bool) -> Result<()> {
        let area = ino.inline_xattr_area();
        let mut go_on = true;
        if area.len() >= 4 && le32(area, 0) == XATTR_MAGIC {
            let entries = &area[4..];
            walk(entries, |e| {
                let v = self.xattr_value(entries, &e)?;
                go_on = f(e.index, e.name, v);
                Ok(go_on)
            })?;
        }
        if !go_on || ino.file_acl == 0 {
            return Ok(());
        }
        let b = self.meta_block(ino.file_acl)?;
        if le32(&b, 0) != XATTR_MAGIC || le32(&b, 8) != 1 {
            return Err(Error::Corrupt("bloc d'attributs etendus invalide"));
        }
        if self.has_metadata_csum() {
            let mut tmp = b.to_vec();
            tmp[0x10..0x14].fill(0);
            let c = crate::crc32c(crate::crc32c(self.csum_seed, &ino.file_acl.to_le_bytes()), &tmp);
            if c != le32(&b, 0x10) {
                return Err(Error::Checksum("bloc d'attributs etendus"));
            }
        }
        walk(&b[32..], |e| {
            let v = self.xattr_value(&b, &e)?;
            Ok(f(e.index, e.name, v))
        })
    }

    /// Tous les attributs étendus de l'inode.
    pub fn xattrs(&self, ino: &Inode) -> Result<Vec<Xattr>> {
        let mut out = Vec::new();
        self.each_xattr(ino, |idx, name, value| {
            if let Some(p) = prefix(idx) {
                let mut full = p.to_vec();
                full.extend_from_slice(name);
                out.push(Xattr { name: full, value });
            }
            true
        })?;
        Ok(out)
    }

    /// Attribut `name` (nom complet).
    pub fn xattr(&self, ino: &Inode, name: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut found = None;
        self.each_xattr(ino, |idx, n, value| {
            if let Some(p) = prefix(idx) {
                if name.len() == p.len() + n.len() && name.starts_with(p) && &name[p.len()..] == n {
                    found = Some(value);
                    return false;
                }
            }
            true
        })?;
        Ok(found)
    }

    /// Valeur de `system.data` (suite des données en ligne).
    pub(crate) fn inline_tail(&self, ino: &Inode) -> Result<Vec<u8>> {
        let area = ino.inline_xattr_area();
        let mut out = Vec::new();
        if area.len() >= 4 && le32(area, 0) == XATTR_MAGIC {
            let entries = &area[4..];
            walk(entries, |e| {
                if e.index == INDEX_SYSTEM && e.name == b"data" {
                    out = self.xattr_value(entries, &e)?;
                    return Ok(false);
                }
                Ok(true)
            })?;
        }
        Ok(out)
    }

    /// Données en ligne complètes : `i_block` (60 octets) puis `system.data`.
    pub(crate) fn inline_data(&self, ino: &Inode) -> Result<Vec<u8>> {
        let mut v = ino.i_block().to_vec();
        v.extend_from_slice(&self.inline_tail(ino)?);
        v.truncate(ino.size as usize);
        Ok(v)
    }
}
