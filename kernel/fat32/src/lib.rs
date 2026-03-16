//! Driver FAT32 pour mai_os — corrections :
//! - `DirEntry83` renommé en pub pour les méthodes pub de `Fat32Volume`
//! - `Weak::new()` typé explicitement pour éviter le E0277
//! - Imports inutilisés supprimés

#![no_std]
#![allow(dead_code)]

extern crate alloc;
#[macro_use] extern crate log;

use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use spin::Mutex;
use storage_device::StorageDeviceRef;
use fs_node::{DirRef, FileOrDir, WeakDirRef};

// ────────────────────────────────────────────────────────────────────────────
// Constantes FAT32
// ────────────────────────────────────────────────────────────────────────────

const SECTOR_SIZE: usize = 512;
const FAT32_EOC:   u32   = 0x0FFF_FFF8;
const FAT32_FREE:  u32   = 0x0000_0000;
const FAT32_BAD:   u32   = 0x0FFF_FFF7;

const ATTR_READ_ONLY: u8 = 0x01;
const ATTR_HIDDEN:    u8 = 0x02;
const ATTR_SYSTEM:    u8 = 0x04;
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_ARCHIVE:   u8 = 0x20;
const ATTR_LFN:       u8 = 0x0F;

// ────────────────────────────────────────────────────────────────────────────
// Helpers lecture little-endian
// ────────────────────────────────────────────────────────────────────────────

#[inline] fn ru16(buf: &[u8], off: usize) -> u16 { u16::from_le_bytes([buf[off], buf[off+1]]) }
#[inline] fn ru32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off+1], buf[off+2], buf[off+3]])
}

// ────────────────────────────────────────────────────────────────────────────
// BPB
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Fat32Params {
    bytes_per_sector:    u16,
    sectors_per_cluster: u8,
    reserved_sectors:    u16,
    num_fats:            u8,
    total_sectors:       u32,
    sectors_per_fat:     u32,
    root_cluster:        u32,
    fat_start_sector:    u32,
    data_start_sector:   u32,
    bytes_per_cluster:   usize,
}

impl Fat32Params {
    fn parse(boot: &[u8]) -> Option<Self> {
        if boot.len() < 512 { return None; }
        if boot[510] != 0x55 || boot[511] != 0xAA { return None; }
        let bytes_per_sector    = ru16(boot, 11);
        let sectors_per_cluster = boot[13];
        let reserved_sectors    = ru16(boot, 14);
        let num_fats            = boot[16];
        let root_entry_count    = ru16(boot, 17);
        let total_sectors_16    = ru16(boot, 19) as u32;
        let sectors_per_fat_16  = ru16(boot, 22) as u32;
        let total_sectors_32    = ru32(boot, 32);
        let sectors_per_fat_32  = ru32(boot, 36);
        let root_cluster        = ru32(boot, 44);
        let sectors_per_fat = if sectors_per_fat_16 == 0 { sectors_per_fat_32 } else { return None; };
        let total_sectors   = if total_sectors_16 == 0 { total_sectors_32 } else { total_sectors_16 };
        if root_entry_count != 0 { return None; }
        let fat_start  = reserved_sectors as u32;
        let data_start = fat_start + num_fats as u32 * sectors_per_fat;
        Some(Fat32Params {
            bytes_per_sector,
            sectors_per_cluster,
            reserved_sectors,
            num_fats,
            total_sectors,
            sectors_per_fat,
            root_cluster,
            fat_start_sector:  fat_start,
            data_start_sector: data_start,
            bytes_per_cluster: bytes_per_sector as usize * sectors_per_cluster as usize,
        })
    }

    #[inline]
    fn cluster_to_sector(&self, cluster: u32) -> u32 {
        self.data_start_sector + (cluster - 2) * self.sectors_per_cluster as u32
    }
}

// ────────────────────────────────────────────────────────────────────────────
// DirEntry83 — pub pour que les méthodes pub de Fat32Volume compilent
// ────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct DirEntry83 {
    name:       [u8; 11],
    pub attr:       u8,
    cluster_hi: u16,
    cluster_lo: u16,
    pub file_size:  u32,
}

impl DirEntry83 {
    fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < 32 { return None; }
        let attr = buf[11];
        if attr == ATTR_LFN   { return None; }
        if buf[0] == 0xE5     { return None; }
        if buf[0] == 0x00     { return None; }
        let mut name = [0u8; 11];
        name.copy_from_slice(&buf[0..11]);
        Some(DirEntry83 { name, attr, cluster_hi: ru16(buf,20), cluster_lo: ru16(buf,26), file_size: ru32(buf,28) })
    }

    pub fn is_dir(&self)       -> bool { self.attr & ATTR_DIRECTORY != 0 }
    fn is_volume_id(&self) -> bool { self.attr & ATTR_VOLUME_ID != 0 }

    pub fn cluster(&self) -> u32 {
        ((self.cluster_hi as u32) << 16) | self.cluster_lo as u32
    }

    pub fn short_name(&self) -> String {
        let base_s: String = self.name[..8].iter().take_while(|&&c| c != b' ').map(|&c| c as char).collect();
        let ext_s:  String = self.name[8..].iter().take_while(|&&c| c != b' ').map(|&c| c as char).collect();
        if ext_s.is_empty() { base_s } else { alloc::format!("{}.{}", base_s, ext_s) }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// LFN Collector
// ────────────────────────────────────────────────────────────────────────────

struct LfnCollector { parts: Vec<(u8, [u16; 13])> }

impl LfnCollector {
    fn new()            -> Self { LfnCollector { parts: Vec::new() } }
    fn clear(&mut self) { self.parts.clear(); }

    fn push(&mut self, buf: &[u8]) {
        if buf.len() < 32 || buf[11] != ATTR_LFN { return; }
        let order = buf[0] & 0x3F;
        let mut chars = [0u16; 13];
        let offsets = [1,3,5,7,9, 14,16,18,20,22,24, 28,30];
        for (i, &off) in offsets.iter().enumerate() { chars[i] = ru16(buf, off); }
        self.parts.push((order, chars));
    }

    fn build(&mut self) -> Option<String> {
        if self.parts.is_empty() { return None; }
        self.parts.sort_by_key(|(o, _)| *o);
        let mut s = String::new();
        for (_, chars) in &self.parts {
            for &c in chars {
                if c == 0 || c == 0xFFFF { break; }
                s.push(if c < 0x80 { c as u8 as char } else { '?' });
            }
        }
        self.parts.clear();
        if s.is_empty() { None } else { Some(s) }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Fat32Volume
// ────────────────────────────────────────────────────────────────────────────

pub struct Fat32Volume {
    device: StorageDeviceRef,
    params: Fat32Params,
}

impl Fat32Volume {
    pub fn mount(device: StorageDeviceRef) -> Result<Arc<Mutex<Self>>, &'static str> {
        let mut boot = [0u8; SECTOR_SIZE];
        device.lock().read_blocks(&mut boot, 0).map_err(|_| "FAT32: cannot read boot sector")?;
        let params = Fat32Params::parse(&boot).ok_or("FAT32: not a valid FAT32 volume")?;
        info!("FAT32: root_cluster={}, data_start={}", params.root_cluster, params.data_start_sector);
        Ok(Arc::new(Mutex::new(Fat32Volume { device, params })))
    }

    fn read_sector(&self, sector: u32, buf: &mut [u8]) -> Result<(), &'static str> {
        self.device.lock().read_blocks(buf, sector as usize).map(|_| ()).map_err(|_| "FAT32: read error")
    }

    fn read_cluster(&self, cluster: u32, buf: &mut Vec<u8>) -> Result<(), &'static str> {
        let bpc = self.params.bytes_per_cluster;
        let spc = self.params.sectors_per_cluster as usize;
        let bps = self.params.bytes_per_sector as usize;
        buf.resize(bpc, 0);
        let first = self.params.cluster_to_sector(cluster);
        for i in 0..spc {
            let mut sec = [0u8; SECTOR_SIZE];
            self.read_sector(first + i as u32, &mut sec)?;
            buf[i*bps..(i+1)*bps].copy_from_slice(&sec[..bps]);
        }
        Ok(())
    }

    fn fat_entry(&self, cluster: u32) -> Result<u32, &'static str> {
        let off = (cluster * 4) as usize;
        let sec = self.params.fat_start_sector as usize + off / SECTOR_SIZE;
        let pos = off % SECTOR_SIZE;
        let mut buf = [0u8; SECTOR_SIZE];
        self.read_sector(sec as u32, &mut buf)?;
        Ok(ru32(&buf, pos) & 0x0FFF_FFFF)
    }

    fn cluster_chain(&self, first: u32) -> Result<Vec<u32>, &'static str> {
        let mut chain = Vec::new();
        let mut cur = first;
        loop {
            if cur < 2 || cur >= FAT32_BAD { break; }
            chain.push(cur);
            if chain.len() > 1_000_000 { return Err("FAT32: chain loop"); }
            let next = self.fat_entry(cur)?;
            if next >= FAT32_EOC { break; }
            cur = next;
        }
        Ok(chain)
    }

    fn iter_dir<F>(&self, dir_cluster: u32, mut callback: F) -> Result<(), &'static str>
    where F: FnMut(String, &DirEntry83) {
        let chain = self.cluster_chain(dir_cluster)?;
        let mut lfn = LfnCollector::new();
        for cluster in chain {
            let mut buf = Vec::new();
            self.read_cluster(cluster, &mut buf)?;
            for chunk in buf.chunks_exact(32) {
                if chunk[0] == 0x00 { return Ok(()); }
                if chunk[0] == 0xE5 { lfn.clear(); continue; }
                if chunk[11] == ATTR_LFN { lfn.push(chunk); continue; }
                if let Some(entry) = DirEntry83::parse(chunk) {
                    if entry.is_volume_id() { lfn.clear(); continue; }
                    let name = lfn.build().unwrap_or_else(|| entry.short_name());
                    if name != "." && name != ".." { callback(name, &entry); }
                } else { lfn.clear(); }
            }
        }
        Ok(())
    }

    pub fn read_file(&self, first_cluster: u32, size: usize) -> Result<Vec<u8>, &'static str> {
        let chain = self.cluster_chain(first_cluster)?;
        let bpc   = self.params.bytes_per_cluster;
        let mut out = Vec::with_capacity(size);
        for cluster in chain {
            if out.len() >= size { break; }
            let mut buf = Vec::new();
            self.read_cluster(cluster, &mut buf)?;
            let to_copy = (size - out.len()).min(bpc);
            out.extend_from_slice(&buf[..to_copy]);
        }
        out.truncate(size);
        Ok(out)
    }

    pub fn find_entry(&self, dir_cluster: u32, name: &str) -> Result<Option<DirEntry83>, &'static str> {
        let name_up = name.to_uppercase();
        let mut found = None;
        self.iter_dir(dir_cluster, |entry_name, entry| {
            if entry_name.to_uppercase() == name_up { found = Some(entry.clone()); }
        })?;
        Ok(found)
    }

    pub fn resolve_path(&self, path: &str) -> Result<Option<DirEntry83>, &'static str> {
        let mut cur = self.params.root_cluster;
        for seg in path.trim_matches('/').split('/') {
            if seg.is_empty() { continue; }
            match self.find_entry(cur, seg)? {
                None        => return Ok(None),
                Some(entry) => {
                    if entry.is_dir() { cur = entry.cluster(); }
                    else              { return Ok(Some(entry)); }
                }
            }
        }
        Ok(Some(DirEntry83 {
            name:       *b".          ",
            attr:       ATTR_DIRECTORY,
            cluster_hi: (cur >> 16) as u16,
            cluster_lo:  cur as u16,
            file_size:  0,
        }))
    }

    pub fn list_dir(&self, path: &str) -> Result<Vec<(String, bool)>, &'static str> {
        let start = if path == "/" || path.is_empty() {
            self.params.root_cluster
        } else {
            match self.resolve_path(path)? {
                Some(e) if e.is_dir() => e.cluster(),
                _ => return Err("FAT32: path not found"),
            }
        };
        let mut entries = Vec::new();
        self.iter_dir(start, |name, entry| { entries.push((name, entry.is_dir())); })?;
        Ok(entries)
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Intégration VFS
// ────────────────────────────────────────────────────────────────────────────

use io::{ByteReader, ByteWriter, IoError, KnownLength};
use memory::MappedPages;

pub struct Fat32File {
    name:          String,
    volume:        Arc<Mutex<Fat32Volume>>,
    first_cluster: u32,
    size:          usize,
    parent:        WeakDirRef,
}

impl Fat32File {
    pub fn new(name: String, volume: Arc<Mutex<Fat32Volume>>,
               first_cluster: u32, size: usize, parent: WeakDirRef) -> fs_node::FileRef {
        Arc::new(Mutex::new(Fat32File { name, volume, first_cluster, size, parent }))
    }
}

impl ByteReader for Fat32File {
    fn read_at(&mut self, buffer: &mut [u8], offset: usize) -> Result<usize, IoError> {
        if offset >= self.size { return Ok(0); }
        let data = self.volume.lock().read_file(self.first_cluster, self.size)
            .map_err(|_| IoError::InvalidInput)?;
        let end = (offset + buffer.len()).min(data.len());
        let n   = end - offset;
        buffer[..n].copy_from_slice(&data[offset..end]);
        Ok(n)
    }
}
impl ByteWriter for Fat32File {
    fn write_at(&mut self, _buf: &[u8], _off: usize) -> Result<usize, IoError> {
        Err(IoError::from("FAT32: read-only"))
    }
    fn flush(&mut self) -> Result<(), IoError> { Ok(()) }
}
impl KnownLength for Fat32File {
    fn len(&self) -> usize { self.size }
}
impl fs_node::File for Fat32File {
    fn as_mapping(&self) -> Result<&MappedPages, &'static str> {
        Err("FAT32: as_mapping not supported")
    }
}
impl fs_node::FsNode for Fat32File {
    fn get_name(&self) -> String { self.name.clone() }
    fn get_parent_dir(&self) -> Option<DirRef> { self.parent.upgrade() }
    fn set_parent_dir(&mut self, p: WeakDirRef) { self.parent = p; }
}

// ── Fat32Directory ────────────────────────────────────────────────────────────

pub struct Fat32Directory {
    name:    String,
    volume:  Arc<Mutex<Fat32Volume>>,
    cluster: u32,
    parent:  WeakDirRef,
}

impl Fat32Directory {
    pub fn new(name: String, volume: Arc<Mutex<Fat32Volume>>,
               cluster: u32, parent: WeakDirRef) -> DirRef {
        Arc::new(Mutex::new(Fat32Directory { name, volume, cluster, parent }))
    }
}

impl fs_node::FsNode for Fat32Directory {
    fn get_name(&self) -> String { self.name.clone() }
    fn get_parent_dir(&self) -> Option<DirRef> { self.parent.upgrade() }
    fn set_parent_dir(&mut self, p: WeakDirRef) { self.parent = p; }
}

impl fs_node::Directory for Fat32Directory {
    fn get(&self, name: &str) -> Option<FileOrDir> {
        let vol   = self.volume.lock();
        let entry = vol.find_entry(self.cluster, name).ok()??;
        // Weak vers soi-même : on utilise le type concret pour satisfaire Sized
        let self_weak: WeakDirRef =
            Weak::<Mutex<Fat32Directory>>::new(); // sera corrigé lors de l'insertion VFS
        if entry.is_dir() {
            Some(FileOrDir::Dir(Fat32Directory::new(
                name.to_string(), self.volume.clone(), entry.cluster(), self_weak,
            )))
        } else {
            Some(FileOrDir::File(Fat32File::new(
                name.to_string(), self.volume.clone(),
                entry.cluster(), entry.file_size as usize, self_weak,
            )))
        }
    }

    fn list(&self) -> Vec<String> {
        self.volume.lock()
            .list_dir("")
            .map(|v| v.into_iter().map(|(n, _)| n).collect())
            .unwrap_or_default()
    }

    fn insert(&mut self, _node: FileOrDir) -> Result<Option<FileOrDir>, &'static str> {
        Err("FAT32: read-only")
    }

    fn remove(&mut self, _node: &FileOrDir) -> Option<FileOrDir> { None }
}

// ────────────────────────────────────────────────────────────────────────────
// Point d'entrée
// ────────────────────────────────────────────────────────────────────────────

pub fn mount_and_get_root(device: StorageDeviceRef, name: &str) -> Result<DirRef, &'static str> {
    let volume       = Fat32Volume::mount(device)?;
    let root_cluster = volume.lock().params.root_cluster;
    Ok(Fat32Directory::new(
        name.to_string(),
        volume,
        root_cluster,
        Weak::<Mutex<Fat32Directory>>::new(), // parent mis à jour dans mount_disk_in_vfs
    ))
}