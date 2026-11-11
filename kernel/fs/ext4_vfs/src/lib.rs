//! # Volumes ext4 dans le VFS (MaiSS)
//!
//! Relie le cœur [`ext4`] (sans dépendance, testé sur l'hôte) au noyau :
//!
//! * [`Disk`] lit l'appareil par la couche bloc (`blk`, files par cœur, sans verrou global) :
//!   les données vont directement dans le tampon de l'appelant (DMA), seules les métadonnées
//!   passent par le [`cache::MetaCache`].
//! * [`Volume`] garde l'**identité** des inodes : un même inode est toujours le même nœud
//!   (`Arc`) tant que quelqu'un le tient, ce qu'exigent le cache de pages des projections
//!   (`maiproc::pagecache`, indexé par le nœud) et la règle de `ld.so` qui compare
//!   `(st_dev, st_ino)` pour ne pas charger deux fois une bibliothèque. Les inodes récents sont
//!   gardés en vie (comme le cache d'inodes inutilisés de Linux) et les recherches de noms,
//!   positives comme négatives, sont mises en cache (le *dcache* : `ld.so` sonde beaucoup de
//!   chemins absents).
//! * [`mount_all`] monte chaque volume ext4 trouvé (disque entier ou partition MBR/GPT) sous
//!   `/mnt/<nom>`, et le premier qui ressemble à une racine Linux (`/usr`, `/etc`) prête ses
//!   dossiers de premier niveau à la racine du VFS (`/usr`, `/bin -> usr/bin`, `/lib64`…),
//!   avant les fichiers livrés avec MaiOS, qui ne comblent alors que ce qui manque.
//!
//! Le volume est en lecture seule : les écritures rendent « système de fichiers en lecture
//! seule » (`EROFS`).

#![no_std]

extern crate alloc;

pub mod cache;

use alloc::{
    format,
    string::String,
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

use cache::{Gen2, MetaCache};
use fs_node::{DirEntry, DirRef, Directory, File, FileOrDir, FsNode, Metadata, NodeKind, Timespec, WeakDirRef};
use io::{ByteReader, ByteWriter, IoError, KnownLength};
use log::{info, warn};
use memory::MappedPages;
use spin::Mutex;

pub use ext4::Error as Ext4Error;

const READ_ONLY: &str = "systeme de fichiers en lecture seule";

/// Blocs de métadonnées en cache par volume (32 Mio avec des blocs de 4 Kio).
const META_CACHE_BLOCKS: usize = 8192;
/// Inodes gardés en vie après leur dernier usage.
const KEEP_INODES: usize = 8192;
/// Noms en cache (recherches positives et négatives).
const DCACHE_NAMES: usize = 32768;

// --- Appareil ----------------------------------------------------------------------------------

/// Statistiques d'entrées-sorties d'un volume.
#[derive(Default)]
pub struct DiskStats {
    pub reads: AtomicU64,
    pub read_bytes: AtomicU64,
    pub meta_hits: AtomicU64,
    pub meta_misses: AtomicU64,
}

/// Région d'un appareil `blk` qui porte un volume.
pub struct Disk {
    blk: Arc<blk::BlockDevice>,
    /// Début de la partition, en octets.
    start: u64,
    /// Taille de la partition, en octets.
    len: u64,
    cache: MetaCache,
    pub stats: DiskStats,
}

impl Disk {
    pub fn new(blk: Arc<blk::BlockDevice>, start: u64, len: u64) -> Disk {
        Disk { blk, start, len, cache: MetaCache::new(META_CACHE_BLOCKS), stats: DiskStats::default() }
    }

    pub fn device_name(&self) -> &str {
        &self.blk.name
    }
}

impl ext4::BlockDev for Disk {
    fn read(&self, offset: u64, buf: &mut [u8]) -> ext4::Result<()> {
        if offset + buf.len() as u64 > self.len {
            return Err(Ext4Error::Corrupt("lecture au-dela de la partition"));
        }
        self.stats.reads.fetch_add(1, Relaxed);
        self.stats.read_bytes.fetch_add(buf.len() as u64, Relaxed);
        let dbs = self.blk.block_size() as u64;
        let abs = self.start + offset;
        if abs % dbs == 0 && buf.len() as u64 % dbs == 0 {
            return self.blk.read(abs / dbs, buf).map_err(|_| Ext4Error::Io);
        }
        // Secteurs de l'appareil plus grands que 512 octets : on lit la plage couvrante.
        let s = abs / dbs * dbs;
        let e = (abs + buf.len() as u64).div_ceil(dbs) * dbs;
        let mut tmp = vec![0u8; (e - s) as usize];
        self.blk.read(s / dbs, &mut tmp).map_err(|_| Ext4Error::Io)?;
        let o = (abs - s) as usize;
        buf.copy_from_slice(&tmp[o..o + buf.len()]);
        Ok(())
    }

    fn meta(&self, offset: u64, len: usize) -> ext4::Result<Arc<[u8]>> {
        if let Some(b) = self.cache.get(offset) {
            if b.len() == len {
                self.stats.meta_hits.fetch_add(1, Relaxed);
                return Ok(b);
            }
        }
        self.stats.meta_misses.fetch_add(1, Relaxed);
        let mut v = vec![0u8; len];
        self.read(offset, &mut v)?;
        let b: Arc<[u8]> = Arc::from(v.into_boxed_slice());
        self.cache.insert(offset, b.clone());
        Ok(b)
    }
}

// --- Volume ------------------------------------------------------------------------------------

enum WeakNode {
    File(Weak<Mutex<Ext4File>>),
    Dir(Weak<Mutex<Ext4Dir>>),
}

impl WeakNode {
    fn upgrade(&self) -> Option<FileOrDir> {
        match self {
            WeakNode::File(w) => w.upgrade().map(|f| FileOrDir::File(f)),
            WeakNode::Dir(w) => w.upgrade().map(|d| FileOrDir::Dir(d)),
        }
    }
}

struct Nodes {
    /// Identité : inode → nœud vivant.
    live: alloc::collections::BTreeMap<u32, WeakNode>,
    /// Maintien en vie des inodes récents.
    keep: Gen2<u32, FileOrDir>,
}

/// Volume ext4 monté.
pub struct Volume {
    pub fs: ext4::Fs<Arc<Disk>>,
    pub name: String,
    /// `st_dev` des fichiers du volume.
    pub dev_id: u64,
    nodes: Mutex<Nodes>,
    /// (dossier, nom) → (inode, type) ; `None` : le nom n'existe pas.
    dcache: Mutex<Gen2<(u32, String), Option<(u32, ext4::FileKind)>>>,
    pub dcache_hits: AtomicU64,
    pub dcache_misses: AtomicU64,
}

static NEXT_DEV: AtomicU64 = AtomicU64::new(0);

fn ts(t: ext4::Time) -> Timespec {
    Timespec { sec: t.sec, nsec: t.nsec }
}

fn kind(k: ext4::FileKind) -> NodeKind {
    match k {
        ext4::FileKind::Dir => NodeKind::Dir,
        ext4::FileKind::Symlink => NodeKind::Symlink,
        ext4::FileKind::CharDev => NodeKind::CharDev,
        ext4::FileKind::BlockDev => NodeKind::BlockDev,
        ext4::FileKind::Fifo => NodeKind::Fifo,
        ext4::FileKind::Socket => NodeKind::Socket,
        ext4::FileKind::Regular | ext4::FileKind::Unknown => NodeKind::File,
    }
}

impl Volume {
    /// Monte le volume porté par `disk`.
    pub fn mount(disk: Disk, name: String) -> Result<Arc<Volume>, Ext4Error> {
        let fs = ext4::Fs::mount(Arc::new(disk))?;
        // Majeur 259 (`blkext`, celui des NVMe sous Linux), un mineur par volume.
        let minor = NEXT_DEV.fetch_add(1, Relaxed);
        Ok(Arc::new(Volume {
            fs,
            name,
            dev_id: (259 << 8) | minor,
            nodes: Mutex::new(Nodes { live: alloc::collections::BTreeMap::new(), keep: Gen2::new(KEEP_INODES) }),
            dcache: Mutex::new(Gen2::new(DCACHE_NAMES)),
            dcache_hits: AtomicU64::new(0),
            dcache_misses: AtomicU64::new(0),
        }))
    }

    pub fn disk(&self) -> &Disk {
        self.fs.device()
    }

    fn metadata_of(&self, i: &ext4::Inode) -> Metadata {
        Metadata {
            kind: kind(i.kind()),
            perm: i.perm(),
            uid: i.uid,
            gid: i.gid,
            ino: i.ino as u64,
            dev: self.dev_id,
            rdev: i.rdev(),
            nlink: i.nlink as u32,
            size: i.size,
            blocks: i.blocks,
            blksize: self.fs.block_size() as u32,
            atime: ts(i.atime),
            mtime: ts(i.mtime),
            ctime: ts(i.ctime),
            btime: i.crtime.map(ts),
        }
    }

    /// Recherche d'un nom dans un dossier, par le cache des noms.
    fn lookup(&self, dir: &ext4::Inode, name: &str) -> Option<(u32, ext4::FileKind)> {
        let key = (dir.ino, String::from(name));
        if let Some(hit) = self.dcache.lock().get(&key) {
            self.dcache_hits.fetch_add(1, Relaxed);
            return hit;
        }
        self.dcache_misses.fetch_add(1, Relaxed);
        let found = match self.fs.lookup(dir, &fs_node::rawname::encode(name)) {
            Ok(r) => r.map(|e| (e.ino, e.kind)),
            Err(e) => {
                warn!("ext4 {} : recherche de {:?} dans l'inode {} : {}", self.name, name, dir.ino, e.as_str());
                return None; // Pas mis en cache : une erreur d'E/S peut être passagère.
            }
        };
        self.dcache.lock().insert(key, found);
        found
    }

    /// Cible du lien `ino`, lue depuis l'inode (sans le nœud ni son verrou).
    fn link_target(&self, ino: u32) -> Option<String> {
        let inode = self.fs.inode(ino).ok()?;
        if inode.kind() != ext4::FileKind::Symlink {
            return None;
        }
        match self.fs.read_link(&inode) {
            Ok(t) => Some(fs_node::rawname::decode(&t)),
            Err(e) => {
                warn!("ext4 {} : lien {} illisible : {}", self.name, ino, e.as_str());
                None
            }
        }
    }

    /// Nœud de l'inode `ino` (le même tant qu'il est vivant).
    fn node(self: &Arc<Self>, ino: u32, name: &str, parent: WeakDirRef) -> Option<FileOrDir> {
        {
            let mut n = self.nodes.lock();
            if let Some(node) = n.live.get(&ino).and_then(|w| w.upgrade()) {
                n.keep.get(&ino);
                return Some(node);
            }
        }
        // Lecture de l'inode hors de tout verrou.
        let inode = match self.fs.inode(ino) {
            Ok(i) => i,
            Err(e) => {
                warn!("ext4 {} : inode {} ({:?}) illisible : {}", self.name, ino, name, e.as_str());
                return None;
            }
        };
        let (node, weak) = self.make_node(inode, String::from(name), parent);
        let mut n = self.nodes.lock();
        // Un autre cœur a pu le créer entre-temps : on garde le premier.
        if let Some(existing) = n.live.get(&ino).and_then(|w| w.upgrade()) {
            return Some(existing);
        }
        if n.live.len() > 4 * KEEP_INODES {
            n.live.retain(|_, w| w.upgrade().is_some());
        }
        n.live.insert(ino, weak);
        n.keep.insert(ino, node.clone());
        Some(node)
    }

    /// Nouveau nœud, et sa référence faible typée pour le registre d'identité.
    fn make_node(self: &Arc<Self>, inode: ext4::Inode, name: String, parent: WeakDirRef) -> (FileOrDir, WeakNode) {
        if inode.is_dir() {
            let d = Arc::new_cyclic(|me: &Weak<Mutex<Ext4Dir>>| {
                Mutex::new(Ext4Dir { vol: self.clone(), inode, name, parent, me: me.clone() })
            });
            let w = WeakNode::Dir(Arc::downgrade(&d));
            (FileOrDir::Dir(d), w)
        } else {
            let f = Arc::new(Mutex::new(Ext4File { vol: self.clone(), inode, name, parent }));
            let w = WeakNode::File(Arc::downgrade(&f));
            (FileOrDir::File(f), w)
        }
    }

    /// Dossier racine du volume, inscrit sous `parent` avec le nom `name`.
    pub fn root_dir(self: &Arc<Self>, name: &str, parent: WeakDirRef) -> Result<DirRef, Ext4Error> {
        let inode = self.fs.root()?;
        let ino = inode.ino;
        let (node, weak) = self.make_node(inode, String::from(name), parent);
        let mut n = self.nodes.lock();
        n.live.insert(ino, weak);
        n.keep.insert(ino, node.clone());
        match node {
            FileOrDir::Dir(d) => Ok(d),
            FileOrDir::File(_) => Err(Ext4Error::NotDir),
        }
    }

    /// Ligne de statistiques (pour `ext4info` et le journal).
    pub fn describe(&self) -> String {
        let s = &self.disk().stats;
        let st = self.fs.statfs();
        format!(
            "{} ({}) : blocs de {} o, {} Mio, {} Mio libres, {} inodes ; lectures {} ({} Mio), metadonnees {} succes / {} defauts ({} en cache), noms {} / {}",
            self.name,
            self.disk().device_name(),
            st.block_size,
            (st.blocks * st.block_size as u64) >> 20,
            (st.free_blocks * st.block_size as u64) >> 20,
            st.inodes,
            s.reads.load(Relaxed),
            s.read_bytes.load(Relaxed) >> 20,
            s.meta_hits.load(Relaxed),
            s.meta_misses.load(Relaxed),
            self.disk().cache.len(),
            self.dcache_hits.load(Relaxed),
            self.dcache_misses.load(Relaxed),
        )
    }
}

// --- Nœuds -------------------------------------------------------------------------------------

pub struct Ext4File {
    vol: Arc<Volume>,
    inode: ext4::Inode,
    name: String,
    parent: WeakDirRef,
}

pub struct Ext4Dir {
    vol: Arc<Volume>,
    inode: ext4::Inode,
    name: String,
    parent: WeakDirRef,
    me: Weak<Mutex<Ext4Dir>>,
}

fn io_err(e: Ext4Error) -> IoError {
    IoError::Other(e.as_str())
}

impl FsNode for Ext4File {
    fn get_name(&self) -> String {
        self.name.clone()
    }
    fn get_parent_dir(&self) -> Option<DirRef> {
        self.parent.upgrade()
    }
    fn set_parent_dir(&mut self, new_parent: WeakDirRef) {
        self.parent = new_parent;
    }
    fn metadata(&self) -> Option<Metadata> {
        Some(self.vol.metadata_of(&self.inode))
    }
    fn read_only(&self) -> bool {
        true
    }
}

impl ByteReader for Ext4File {
    fn read_at(&mut self, buffer: &mut [u8], offset: usize) -> Result<usize, IoError> {
        if self.inode.kind() != ext4::FileKind::Regular {
            return Err(IoError::InvalidInput);
        }
        self.vol.fs.read(&self.inode, offset as u64, buffer).map_err(io_err)
    }
}

impl ByteWriter for Ext4File {
    fn write_at(&mut self, _buffer: &[u8], _offset: usize) -> Result<usize, IoError> {
        Err(IoError::Other(READ_ONLY))
    }
    fn flush(&mut self) -> Result<(), IoError> {
        Ok(())
    }
}

impl KnownLength for Ext4File {
    fn len(&self) -> usize {
        self.inode.size as usize
    }
}

impl File for Ext4File {
    fn as_mapping(&self) -> Result<&MappedPages, &'static str> {
        Err("ext4 : pas de projection directe (passer par le cache de pages)")
    }

    fn set_len(&mut self, _len: usize) -> Result<(), &'static str> {
        Err(READ_ONLY)
    }

    fn read_link(&self) -> Option<String> {
        if self.inode.kind() != ext4::FileKind::Symlink {
            return None;
        }
        match self.vol.fs.read_link(&self.inode) {
            Ok(t) => Some(fs_node::rawname::decode(&t)),
            Err(e) => {
                warn!("ext4 {} : lien {} illisible : {}", self.vol.name, self.inode.ino, e.as_str());
                None
            }
        }
    }
}

impl FsNode for Ext4Dir {
    fn get_name(&self) -> String {
        self.name.clone()
    }
    fn get_parent_dir(&self) -> Option<DirRef> {
        self.parent.upgrade()
    }
    fn set_parent_dir(&mut self, new_parent: WeakDirRef) {
        self.parent = new_parent;
    }
    fn metadata(&self) -> Option<Metadata> {
        Some(self.vol.metadata_of(&self.inode))
    }
    fn read_only(&self) -> bool {
        true
    }
}

impl Directory for Ext4Dir {
    fn get(&self, name: &str) -> Option<FileOrDir> {
        match name {
            "" | "." => return self.me.upgrade().map(|d| FileOrDir::Dir(d)),
            ".." => return self.parent.upgrade().map(FileOrDir::Dir),
            _ => {}
        }
        let (ino, _) = self.vol.lookup(&self.inode, name)?;
        let me: DirRef = self.me.upgrade()?;
        self.vol.node(ino, name, Arc::downgrade(&me))
    }

    fn get_with_link(&self, name: &str) -> Option<(FileOrDir, Option<String>)> {
        if matches!(name, "" | "." | "..") {
            return self.get(name).map(|n| (n, None));
        }
        let (ino, k) = self.vol.lookup(&self.inode, name)?;
        let me: DirRef = self.me.upgrade()?;
        let node = self.vol.node(ino, name, Arc::downgrade(&me))?;
        // Le type de l'entrée suffit ; sans lui (`filetype` absent), celui de l'inode.
        let maybe_link = k == ext4::FileKind::Symlink || (k == ext4::FileKind::Unknown && node.is_file());
        let link = if maybe_link { self.vol.link_target(ino) } else { None };
        Some((node, link))
    }

    fn insert(&mut self, _node: FileOrDir) -> Result<Option<FileOrDir>, &'static str> {
        Err(READ_ONLY)
    }

    fn remove(&mut self, _node: &FileOrDir) -> Option<FileOrDir> {
        None
    }

    fn list(&self) -> Vec<String> {
        let mut out = Vec::new();
        let r = self.vol.fs.read_dir(&self.inode, |e| {
            if e.name != b"." && e.name != b".." {
                out.push(fs_node::rawname::decode(&e.name));
            }
            true
        });
        if let Err(e) = r {
            warn!("ext4 {} : dossier {} illisible : {}", self.vol.name, self.inode.ino, e.as_str());
        }
        out
    }

    fn entries(&self) -> Option<Vec<DirEntry>> {
        let mut out = Vec::new();
        self.vol
            .fs
            .read_dir(&self.inode, |e| {
                if e.name != b"." && e.name != b".." {
                    out.push(DirEntry {
                        name: fs_node::rawname::decode(&e.name),
                        ino: e.ino as u64,
                        kind: if e.kind == ext4::FileKind::Unknown { None } else { Some(kind(e.kind)) },
                    });
                }
                true
            })
            .ok()?;
        Some(out)
    }
}

// --- Montage -----------------------------------------------------------------------------------

static VOLUMES: Mutex<Vec<Arc<Volume>>> = Mutex::new(Vec::new());

/// Volumes montés.
pub fn volumes() -> Vec<Arc<Volume>> {
    VOLUMES.lock().clone()
}

/// Dossiers d'une racine Linux qui ne sont **pas** prêtés à la racine du VFS : ceux que MaiOS
/// fournit lui-même (en mémoire, inscriptibles, ou produits à la volée).
const NOT_BOUND: &[&str] = &["dev", "proc", "sys", "tmp", "run", "mnt", "media", "lost+found"];

fn try_mount(disk: Disk, name: String) -> Option<Arc<Volume>> {
    match Volume::mount(disk, name.clone()) {
        Ok(v) => Some(v),
        Err(Ext4Error::Corrupt("pas un volume ext2/3/4")) => None,
        Err(e) => {
            warn!("ext4 : {} : montage impossible : {}", name, e.as_str());
            None
        }
    }
}

/// Monte tous les volumes ext4 des appareils `blk`. Renvoie le nombre de volumes montés.
pub fn mount_all() -> usize {
    let mut found = Vec::new();
    for dev in blk::devices() {
        let bs = dev.block_size() as u64;
        let size = dev.capacity * bs;
        if let Some(v) = try_mount(Disk::new(dev.clone(), 0, size), dev.name.clone()) {
            found.push(v);
            continue;
        }
        let storage = blk::BlkStorage::new(dev.clone());
        for p in partition_table::detect_partitions(&storage) {
            let start = p.start_lba * bs;
            let len = p.size_sectors * bs;
            let name = format!("{}p{}", dev.name, p.index + 1);
            if let Some(v) = try_mount(Disk::new(dev.clone(), start, len), name) {
                found.push(v);
            }
        }
    }
    if found.is_empty() {
        return 0;
    }
    let root = root::get_root().clone();
    let mnt = match root.lock().get("mnt") {
        Some(FileOrDir::Dir(d)) => Some(d),
        _ => None,
    };
    let mnt = match mnt {
        Some(d) => d,
        None => match vfs_node::VFSDirectory::create(String::from("mnt"), &root) {
            Ok(d) => d,
            Err(e) => {
                warn!("ext4 : /mnt impossible a creer : {}", e);
                return 0;
            }
        },
    };
    let mut linux_root_bound = false;
    let mut mounted = 0;
    for vol in found {
        let label = String::from_utf8_lossy(vol.fs.sb.label()).into_owned();
        let point = if label.is_empty() || label.contains('/') { vol.name.clone() } else { label };
        let dir = match vol.root_dir(&point, Arc::downgrade(&mnt)) {
            Ok(d) => d,
            Err(e) => {
                warn!("ext4 : {} : racine illisible : {}", vol.name, e.as_str());
                continue;
            }
        };
        if let Err(e) = mnt.lock().insert(FileOrDir::Dir(dir.clone())) {
            warn!("ext4 : /mnt/{} : {}", point, e);
            continue;
        }
        warn!(
            "MaiSS : ext4 {} monte sur /mnt/{} ({} blocs rejoues depuis le journal)",
            vol.name,
            point,
            vol.fs.journal_replayed_blocks()
        );
        if !linux_root_bound && looks_like_linux_root(&dir) {
            let n = bind_into_root(&dir);
            warn!("MaiSS : racine Linux /mnt/{} : {} dossiers pretes a /", point, n);
            linux_root_bound = true;
        }
        VOLUMES.lock().push(vol);
        mounted += 1;
    }
    mounted
}

fn looks_like_linux_root(dir: &DirRef) -> bool {
    let d = dir.lock();
    matches!(d.get("usr"), Some(FileOrDir::Dir(_))) && matches!(d.get("etc"), Some(FileOrDir::Dir(_)))
}

/// Inscrit à la racine du VFS les entrées de premier niveau de `dir` qui n'y sont pas encore
/// (même nœud, sans copie, comme un montage par liaison).
fn bind_into_root(dir: &DirRef) -> usize {
    let names = dir.lock().list();
    let root = root::get_root().clone();
    let mut n = 0;
    for name in names {
        if NOT_BOUND.contains(&name.as_str()) {
            continue;
        }
        if root.lock().get(&name).is_some() {
            info!("ext4 : /{} existe deja, non prete", name);
            continue;
        }
        let Some(node) = dir.lock().get(&name) else { continue };
        if root.lock().insert(node).is_ok() {
            n += 1;
        }
    }
    n
}

/// Résumé de tous les volumes montés.
pub fn describe_all() -> Vec<String> {
    volumes().iter().map(|v| v.describe()).collect()
}
