//! tmpfs de MaiOS (MaiSS) : système de fichiers en mémoire au sens POSIX, comme le `shmem` de
//! Linux, monté sur `/tmp` et `/dev/shm`.
//!
//! * **Fichiers creux.** Le contenu d'un fichier est une table `page → frame physique`
//!   (`BTreeMap`, ordonnée : la recherche des zones de données est directe). Une page jamais
//!   écrite n'occupe rien et se lit nulle : écrire un octet à 2 Gio coûte une page, pas 2 Gio.
//!   Les frames sont lues et écrites par la physmap (toute la RAM projetée) : aucune
//!   projection, aucune table de pages, aucune invalidation de TLB par accès. Une page neuve
//!   vient de préférence du pool pré-zéroé de MaiMM.
//! * **Quota.** Comme le `size=` de tmpfs : chaque volume a une limite en pages (par défaut
//!   la moitié de la RAM, la valeur de Linux) et aucune page n'est prise quand la mémoire libre
//!   passe sous la réserve du noyau. Au-delà, l'écriture échoue (`ENOSPC`), le noyau ne manque
//!   jamais de mémoire à cause de `/tmp`.
//! * **Inodes.** Numéro, type, droits, propriétaire, nombre de liens, dates à la nanoseconde
//!   (`atime` en *relatime*, `btime`), comme un vrai système de fichiers. Plusieurs nœuds du
//!   VFS peuvent partager un inode (liens durs) : le nombre de liens suit les nœuds rattachés à
//!   un dossier, et les données vivent tant qu'un nœud (dossier ou descripteur ouvert) tient
//!   l'inode — un fichier supprimé mais ouvert reste lisible, comme sous Unix.
//! * **Liens symboliques** : la cible est connue du dossier dès l'insertion, la résolution des
//!   chemins ne verrouille jamais l'enfant (règle du VFS).
//!
//! Horloge : le noyau qui monte le volume fournit la date courante par [`set_clock`].

#![no_std]

extern crate alloc;

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use fs_node::{
    DirEntry, DirRef, Directory, File, FileOrDir, FileRef, FsNode, MetaChange, Metadata, NodeKind, TimeSet,
    Timespec, WeakDirRef,
};
use io::{ByteReader, ByteWriter, IoError, KnownLength};
use memory::MappedPages;
use spin::Mutex;

const PAGE: usize = 4096;
const PAGE_SHIFT: u32 = 12;

/// Erreur d'écriture quand le volume est plein ou la mémoire trop basse (`ENOSPC`).
pub const NO_SPACE: &str = "tmpfs : plus de place";
const NOT_A_DIR_ENTRY: &str = "tmpfs : nom invalide";
const EXISTS: &str = "tmpfs : le nom existe deja";

/// Frames libres du système en dessous desquelles tmpfs ne prend plus de page : la réserve
/// système de MaiMM (16 Mio : tables de pages, tas noyau), la même que celle du pool pré-zéroé.
/// Linux n'arrête tmpfs qu'au filigrane `min_free_kbytes` (~3 Mio pour 512 Mio) ; l'ancienne
/// réserve de 64 Mio (12 % de la RAM) faisait échouer en `ENOSPC` des écritures de quelques
/// octets dès qu'un processus tenait ~100 Mio (compresseur xz de test_shutil).
const SYSTEM_RESERVE_FRAMES: usize = memory::RESERVE_FRAMES;

// --- Horloge -----------------------------------------------------------------------------------

static CLOCK: AtomicUsize = AtomicUsize::new(0);

/// Fournit la date courante (temps réel) aux volumes tmpfs. Sans horloge, les dates sont nulles.
pub fn set_clock(now: fn() -> Timespec) {
    CLOCK.store(now as usize, Ordering::Release);
}

fn now() -> Timespec {
    match CLOCK.load(Ordering::Acquire) {
        0 => Timespec::default(),
        // SAFETY : seule `set_clock` écrit ici, avec un `fn() -> Timespec`.
        f => unsafe { core::mem::transmute::<usize, fn() -> Timespec>(f)() },
    }
}

// --- Volume ------------------------------------------------------------------------------------

/// Un volume tmpfs (le *superbloc*) : identité, numéros d'inode, quota.
pub struct Volume {
    name: String,
    dev: u64,
    next_ino: AtomicU64,
    pages: AtomicUsize,
    limit_pages: usize,
    inodes: AtomicUsize,
}

static NEXT_MINOR: AtomicU32 = AtomicU32::new(1);

impl Volume {
    fn new(name: &str, limit_bytes: Option<usize>) -> Arc<Volume> {
        let total = frame_allocator::stats().total_frames;
        let limit_pages = limit_bytes.map_or(total / 2, |b| (b + PAGE - 1) / PAGE);
        // Périphérique anonyme (majeur 0), comme les tmpfs de Linux.
        let minor = NEXT_MINOR.fetch_add(1, Ordering::Relaxed);
        Arc::new(Volume {
            name: name.to_string(),
            dev: minor as u64 & 0xff | ((minor as u64 & !0xff) << 12),
            next_ino: AtomicU64::new(2),
            pages: AtomicUsize::new(0),
            limit_pages,
            inodes: AtomicUsize::new(0),
        })
    }

    /// Réserve une page du quota.
    fn charge(&self) -> bool {
        let free = frame_allocator::free_general_frames();
        if free.map_or(false, |f| f < SYSTEM_RESERVE_FRAMES) {
            log::warn!("tmpfs {} : reserve systeme atteinte ({:?} frames libres)", self.name, free);
            return false;
        }
        let ok = self
            .pages
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |p| if p < self.limit_pages { Some(p + 1) } else { None })
            .is_ok();
        if !ok {
            log::warn!(
                "tmpfs {} : quota atteint ({} pages sur {}, {} inodes, {:?} frames libres)",
                self.name,
                self.pages.load(Ordering::Relaxed),
                self.limit_pages,
                self.inodes.load(Ordering::Relaxed),
                free
            );
        }
        ok
    }

    fn uncharge(&self, n: usize) {
        self.pages.fetch_sub(n, Ordering::AcqRel);
    }

    /// (pages occupées, limite en pages, inodes) pour `statfs`.
    pub fn usage(&self) -> (usize, usize, usize) {
        (self.pages.load(Ordering::Relaxed), self.limit_pages, self.inodes.load(Ordering::Relaxed))
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

// --- Inodes ------------------------------------------------------------------------------------

struct Attr {
    perm: u16,
    uid: u32,
    gid: u32,
    nlink: u32,
    atime: Timespec,
    mtime: Timespec,
    ctime: Timespec,
    btime: Timespec,
}

/// Contenu d'un fichier régulier : taille et pages présentes.
struct Data {
    size: u64,
    pages: BTreeMap<u64, usize>,
}

enum Body {
    File(Mutex<Data>),
    Symlink(String),
    Dir,
    /// Nœud spécial de `mknod` (tube nommé, socket, périphérique) : sans contenu.
    Special { kind: NodeKind, rdev: (u32, u32) },
}

struct Inode {
    ino: u64,
    vol: Arc<Volume>,
    attr: Mutex<Attr>,
    body: Body,
}

impl Inode {
    fn new(vol: &Arc<Volume>, perm: u16, body: Body) -> Arc<Inode> {
        let t = now();
        vol.inodes.fetch_add(1, Ordering::Relaxed);
        Arc::new(Inode {
            ino: vol.next_ino.fetch_add(1, Ordering::Relaxed),
            vol: vol.clone(),
            attr: Mutex::new(Attr { perm, uid: 0, gid: 0, nlink: 0, atime: t, mtime: t, ctime: t, btime: t }),
            body,
        })
    }

    fn kind(&self) -> NodeKind {
        match self.body {
            Body::File(_) => NodeKind::File,
            Body::Symlink(_) => NodeKind::Symlink,
            Body::Dir => NodeKind::Dir,
            Body::Special { kind, .. } => kind,
        }
    }

    fn metadata(&self, size: u64, pages: u64, nlink_override: Option<u32>) -> Metadata {
        let a = self.attr.lock();
        Metadata {
            kind: self.kind(),
            perm: a.perm,
            uid: a.uid,
            gid: a.gid,
            ino: self.ino,
            dev: self.vol.dev,
            rdev: match self.body {
                Body::Special { rdev, .. } => rdev,
                _ => (0, 0),
            },
            nlink: nlink_override.unwrap_or(a.nlink),
            size,
            blocks: pages * (PAGE as u64 / 512),
            blksize: PAGE as u32,
            atime: a.atime,
            mtime: a.mtime,
            ctime: a.ctime,
            btime: Some(a.btime),
        }
    }

    fn set_metadata(&self, change: MetaChange) {
        let mut a = self.attr.lock();
        let t = now();
        match change {
            MetaChange::Perm(p) => a.perm = p & 0o7777,
            MetaChange::Owner { uid, gid } => {
                if let Some(u) = uid {
                    a.uid = u;
                }
                if let Some(g) = gid {
                    a.gid = g;
                }
            }
            MetaChange::Times { atime, mtime } => {
                let pick = |s: TimeSet, old: Timespec| match s {
                    TimeSet::Omit => old,
                    TimeSet::Now => t,
                    TimeSet::At(v) => v,
                };
                a.atime = pick(atime, a.atime);
                a.mtime = pick(mtime, a.mtime);
            }
        }
        a.ctime = t;
    }

    /// Contenu modifié : `mtime` et `ctime`.
    fn touch_modified(&self) {
        let t = now();
        let mut a = self.attr.lock();
        a.mtime = t;
        a.ctime = t;
    }

    /// Lecture : `atime` en *relatime* (seulement s'il précède la dernière modification, ou
    /// date de plus d'un jour), ce qui évite une écriture d'attributs par lecture.
    fn touch_accessed(&self) {
        let mut a = self.attr.lock();
        if a.atime.sec <= a.mtime.sec || a.atime.sec <= a.ctime.sec {
            a.atime = now();
            return;
        }
        let t = now();
        if t.sec - a.atime.sec >= 86400 {
            a.atime = t;
        }
    }

    fn link_delta(&self, delta: i32) {
        let mut a = self.attr.lock();
        a.nlink = (a.nlink as i64 + delta as i64).max(0) as u32;
        a.ctime = now();
    }
}

impl Drop for Inode {
    fn drop(&mut self) {
        if let Body::File(data) = &mut self.body {
            let data = data.get_mut();
            let n = data.pages.len();
            for (_, &pfn) in data.pages.iter() {
                // SAFETY : la frame n'appartient qu'à cet inode, qui disparaît.
                unsafe { frame_allocator::free_raw_frames(pfn, 1) };
            }
            data.pages.clear();
            self.vol.uncharge(n);
        }
        self.vol.inodes.fetch_sub(1, Ordering::Relaxed);
    }
}

#[inline]
fn frame_bytes(pfn: usize) -> *mut u8 {
    memory::paging::kernel_half::frame_ptr::<u8>(pfn)
}

impl Data {
    /// Copie `buf.len()` octets depuis `off` (zéros dans les trous) ; l'appelant borne à la taille.
    fn read(&self, buf: &mut [u8], off: u64) {
        let mut done = 0;
        while done < buf.len() {
            let pos = off + done as u64;
            let page = pos >> PAGE_SHIFT;
            let in_page = (pos as usize) & (PAGE - 1);
            let n = (PAGE - in_page).min(buf.len() - done);
            match self.pages.get(&page) {
                // SAFETY : frame possédée par l'inode, projetée par la physmap ; `in_page + n <= PAGE`.
                Some(&pfn) => unsafe {
                    core::ptr::copy_nonoverlapping(frame_bytes(pfn).add(in_page), buf[done..].as_mut_ptr(), n)
                },
                None => buf[done..done + n].fill(0),
            }
            done += n;
        }
    }

    /// Écrit `buf` à `off`, en prenant les pages manquantes ; renvoie les octets écrits (moins
    /// que demandé si le quota est atteint, `Err` si rien n'a pu être écrit).
    fn write(&mut self, vol: &Volume, buf: &[u8], off: u64) -> Result<usize, &'static str> {
        let mut done = 0;
        while done < buf.len() {
            let pos = off + done as u64;
            let page = pos >> PAGE_SHIFT;
            let in_page = (pos as usize) & (PAGE - 1);
            let n = (PAGE - in_page).min(buf.len() - done);
            let pfn = match self.pages.get(&page) {
                Some(&pfn) => pfn,
                None => match Self::new_page(vol, n == PAGE) {
                    Some(pfn) => {
                        self.pages.insert(page, pfn);
                        pfn
                    }
                    None => break,
                },
            };
            // SAFETY : comme dans `read`.
            unsafe { core::ptr::copy_nonoverlapping(buf[done..].as_ptr(), frame_bytes(pfn).add(in_page), n) };
            done += n;
        }
        if done == 0 && !buf.is_empty() {
            return Err(NO_SPACE);
        }
        self.size = self.size.max(off + done as u64);
        Ok(done)
    }

    /// Page neuve, nulle sauf si elle va être entièrement écrite (`full`).
    fn new_page(vol: &Volume, full: bool) -> Option<usize> {
        if !vol.charge() {
            return None;
        }
        match frame_allocator::alloc_raw_frame_prefer_zeroed() {
            Some((pfn, zeroed)) => {
                if !zeroed && !full {
                    // SAFETY : frame neuve, à nous seuls.
                    unsafe { core::ptr::write_bytes(frame_bytes(pfn), 0, PAGE) };
                }
                Some(pfn)
            }
            None => {
                vol.uncharge(1);
                None
            }
        }
    }

    /// `truncate` : rend les pages au-delà de `len` et met à zéro la fin de la dernière.
    fn truncate(&mut self, vol: &Volume, len: u64) {
        if len < self.size {
            let first_gone = (len + PAGE as u64 - 1) >> PAGE_SHIFT;
            let gone: Vec<u64> = self.pages.range(first_gone..).map(|(&p, _)| p).collect();
            for p in &gone {
                if let Some(pfn) = self.pages.remove(p) {
                    // SAFETY : la frame n'est plus référencée par la table.
                    unsafe { frame_allocator::free_raw_frames(pfn, 1) };
                }
            }
            vol.uncharge(gone.len());
            let tail = (len as usize) & (PAGE - 1);
            if tail != 0 {
                if let Some(&pfn) = self.pages.get(&(len >> PAGE_SHIFT)) {
                    // SAFETY : comme dans `read`.
                    unsafe { core::ptr::write_bytes(frame_bytes(pfn).add(tail), 0, PAGE - tail) };
                }
            }
        }
        self.size = len;
    }

    /// Premier octet de données (`SEEK_DATA`) ou de trou (`SEEK_HOLE`) à partir de `off`.
    fn seek(&self, off: u64, data: bool) -> Option<u64> {
        if off >= self.size {
            return None;
        }
        let mut page = off >> PAGE_SHIFT;
        if data {
            let (&p, _) = self.pages.range(page..).next()?;
            let at = (p << PAGE_SHIFT).max(off);
            return if at < self.size { Some(at) } else { None };
        }
        while self.pages.contains_key(&page) {
            page += 1;
        }
        Some((page << PAGE_SHIFT).max(off).min(self.size))
    }
}

// --- Fichiers et liens symboliques ---------------------------------------------------------------

/// Un nom de fichier (ou de lien symbolique) dans un dossier tmpfs ; plusieurs `TmpFile`
/// partagent un inode quand le fichier a plusieurs liens durs.
pub struct TmpFile {
    name: String,
    parent: WeakDirRef,
    inode: Arc<Inode>,
}

impl TmpFile {
    fn new_ref(name: &str, parent: WeakDirRef, inode: Arc<Inode>) -> FileRef {
        if parent.upgrade().is_some() {
            inode.link_delta(1);
        }
        Arc::new(Mutex::new(TmpFile { name: name.to_string(), parent, inode })) as FileRef
    }

    fn data(&self) -> Option<&Mutex<Data>> {
        match &self.inode.body {
            Body::File(d) => Some(d),
            _ => None,
        }
    }

    /// `SEEK_DATA` (`data`) ou `SEEK_HOLE` depuis `off` ; `None` : au-delà de la fin (`ENXIO`).
    pub fn seek_data(&self, off: u64, data: bool) -> Option<u64> {
        self.data()?.lock().seek(off, data)
    }
}

impl FsNode for TmpFile {
    fn get_name(&self) -> String {
        self.name.clone()
    }

    fn get_parent_dir(&self) -> Option<DirRef> {
        self.parent.upgrade()
    }

    /// Le nombre de liens suit les rattachements : un nœud retiré de son dossier (`remove`,
    /// qui efface son parent) en perd un, un nœud rattaché en gagne un.
    fn set_parent_dir(&mut self, new_parent: WeakDirRef) {
        let was = self.parent.upgrade().is_some();
        let is = new_parent.upgrade().is_some();
        self.parent = new_parent;
        match (was, is) {
            (true, false) => self.inode.link_delta(-1),
            (false, true) => self.inode.link_delta(1),
            _ => {}
        }
    }

    fn set_name(&mut self, name: String) -> Result<(), &'static str> {
        self.name = name;
        self.inode.attr.lock().ctime = now();
        Ok(())
    }

    fn is_movable(&self) -> bool {
        true
    }

    fn metadata(&self) -> Option<Metadata> {
        Some(match &self.inode.body {
            Body::File(d) => {
                let d = d.lock();
                self.inode.metadata(d.size, d.pages.len() as u64, None)
            }
            Body::Symlink(t) => self.inode.metadata(t.len() as u64, 0, None),
            Body::Dir | Body::Special { .. } => self.inode.metadata(0, 0, None),
        })
    }

    fn set_metadata(&mut self, change: MetaChange) -> Result<(), &'static str> {
        self.inode.set_metadata(change);
        Ok(())
    }
}

impl ByteReader for TmpFile {
    fn read_at(&mut self, buffer: &mut [u8], offset: usize) -> Result<usize, IoError> {
        let Some(data) = self.data() else { return Err(IoError::InvalidInput) };
        let n = {
            let d = data.lock();
            if offset as u64 >= d.size {
                return Ok(0);
            }
            let n = buffer.len().min((d.size - offset as u64) as usize);
            d.read(&mut buffer[..n], offset as u64);
            n
        };
        self.inode.touch_accessed();
        Ok(n)
    }
}

impl ByteWriter for TmpFile {
    fn write_at(&mut self, buffer: &[u8], offset: usize) -> Result<usize, IoError> {
        let Some(data) = self.data() else { return Err(IoError::InvalidInput) };
        let n = data.lock().write(&self.inode.vol, buffer, offset as u64)?;
        self.inode.touch_modified();
        Ok(n)
    }

    fn flush(&mut self) -> Result<(), IoError> {
        Ok(())
    }
}

impl KnownLength for TmpFile {
    fn len(&self) -> usize {
        match &self.inode.body {
            Body::File(d) => d.lock().size as usize,
            Body::Symlink(t) => t.len(),
            Body::Dir | Body::Special { .. } => 0,
        }
    }
}

impl File for TmpFile {
    fn as_mapping(&self) -> Result<&MappedPages, &'static str> {
        Err("tmpfs : fichier sans projection contigue (lire par read_at)")
    }

    fn set_len(&mut self, len: usize) -> Result<(), &'static str> {
        let Some(data) = self.data() else { return Err("tmpfs : pas un fichier regulier") };
        data.lock().truncate(&self.inode.vol, len as u64);
        self.inode.touch_modified();
        Ok(())
    }

    fn read_link(&self) -> Option<String> {
        match &self.inode.body {
            Body::Symlink(t) => Some(t.clone()),
            _ => None,
        }
    }

    fn new_link(&self, name: String, parent: WeakDirRef) -> Option<FileRef> {
        Some(TmpFile::new_ref(&name, parent, self.inode.clone()))
    }
}

// --- Dossiers ----------------------------------------------------------------------------------

struct Entry {
    node: FileOrDir,
    ino: u64,
    kind: NodeKind,
    link: Option<String>,
}

pub struct TmpDir {
    name: String,
    parent: WeakDirRef,
    me: WeakDirRef,
    inode: Arc<Inode>,
    entries: BTreeMap<String, Entry>,
    subdirs: u32,
}

impl TmpDir {
    fn new_ref(name: &str, parent: WeakDirRef, vol: &Arc<Volume>, perm: u16) -> DirRef {
        let inode = Inode::new(vol, perm, Body::Dir);
        Arc::new_cyclic(|me: &Weak<Mutex<TmpDir>>| {
            Mutex::new(TmpDir {
                name: name.to_string(),
                parent,
                me: me.clone() as WeakDirRef,
                inode,
                entries: BTreeMap::new(),
                subdirs: 0,
            })
        }) as DirRef
    }

    fn check_new(&self, name: &str) -> Result<(), &'static str> {
        if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.len() > 255 {
            return Err(NOT_A_DIR_ENTRY);
        }
        if self.entries.contains_key(name) {
            return Err(EXISTS);
        }
        Ok(())
    }

    fn add(&mut self, name: &str, node: FileOrDir, ino: u64, kind: NodeKind, link: Option<String>) {
        if kind == NodeKind::Dir {
            self.subdirs += 1;
        }
        self.entries.insert(name.to_string(), Entry { node, ino, kind, link });
        self.inode.touch_modified();
    }
}

impl FsNode for TmpDir {
    fn get_name(&self) -> String {
        self.name.clone()
    }

    fn get_parent_dir(&self) -> Option<DirRef> {
        self.parent.upgrade()
    }

    fn set_parent_dir(&mut self, new_parent: WeakDirRef) {
        self.parent = new_parent;
    }

    fn set_name(&mut self, name: String) -> Result<(), &'static str> {
        self.name = name;
        self.inode.attr.lock().ctime = now();
        Ok(())
    }

    fn is_movable(&self) -> bool {
        true
    }

    fn metadata(&self) -> Option<Metadata> {
        // Un dossier : `.`, son nom dans le parent et le `..` de chaque sous-dossier.
        let size = (self.entries.len() as u64 + 2) * 20;
        Some(self.inode.metadata(size, 0, Some(2 + self.subdirs)))
    }

    fn set_metadata(&mut self, change: MetaChange) -> Result<(), &'static str> {
        self.inode.set_metadata(change);
        Ok(())
    }
}

impl Directory for TmpDir {
    fn get(&self, name: &str) -> Option<FileOrDir> {
        self.entries.get(name).map(|e| e.node.clone())
    }

    fn get_with_link(&self, name: &str) -> Option<(FileOrDir, Option<String>)> {
        self.entries.get(name).map(|e| (e.node.clone(), e.link.clone()))
    }

    fn insert(&mut self, node: FileOrDir) -> Result<Option<FileOrDir>, &'static str> {
        let name = node.get_name();
        if name.is_empty() || name.contains('/') {
            return Err(NOT_A_DIR_ENTRY);
        }
        // Le verrou de l'enfant est pris ici (permis par le contrat d'`insert`).
        let (ino, kind) = node.metadata().map_or((0, node.kind()), |m| (m.ino, m.kind));
        let link = node.read_link();
        let old = self.entries.remove(&name).map(|mut e| {
            if e.kind == NodeKind::Dir {
                self.subdirs -= 1;
            }
            e.node.set_parent_dir(Weak::<Mutex<TmpDir>>::new() as WeakDirRef);
            e.node
        });
        self.add(&name, node, ino, kind, link);
        Ok(old)
    }

    fn remove(&mut self, node: &FileOrDir) -> Option<FileOrDir> {
        let name = node.get_name();
        let same = match (self.entries.get(&name).map(|e| &e.node), node) {
            (Some(FileOrDir::File(a)), FileOrDir::File(b)) => Arc::ptr_eq(a, b),
            (Some(FileOrDir::Dir(a)), FileOrDir::Dir(b)) => fs_node::same_dir(a, b),
            _ => false,
        };
        if !same {
            return None;
        }
        let mut e = self.entries.remove(&name)?;
        if e.kind == NodeKind::Dir {
            self.subdirs -= 1;
        }
        self.inode.touch_modified();
        e.node.set_parent_dir(Weak::<Mutex<TmpDir>>::new() as WeakDirRef);
        Some(e.node)
    }

    fn list(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    fn entries(&self) -> Option<Vec<DirEntry>> {
        Some(self.entries.iter().map(|(n, e)| DirEntry { name: n.clone(), ino: e.ino, kind: Some(e.kind) }).collect())
    }

    fn create_file(&mut self, name: &str, perm: u16) -> Option<Result<FileRef, &'static str>> {
        Some((|| {
            self.check_new(name)?;
            let inode = Inode::new(&self.inode.vol, perm & 0o7777, Body::File(Mutex::new(Data { size: 0, pages: BTreeMap::new() })));
            let ino = inode.ino;
            let f = TmpFile::new_ref(name, self.me.clone(), inode);
            self.add(name, FileOrDir::File(f.clone()), ino, NodeKind::File, None);
            Ok(f)
        })())
    }

    fn create_dir(&mut self, name: &str, perm: u16) -> Option<Result<DirRef, &'static str>> {
        Some((|| {
            self.check_new(name)?;
            let d = TmpDir::new_ref(name, self.me.clone(), &self.inode.vol, perm & 0o7777);
            let ino = d.lock().metadata().map_or(0, |m| m.ino);
            self.add(name, FileOrDir::Dir(d.clone()), ino, NodeKind::Dir, None);
            Ok(d)
        })())
    }

    fn create_symlink(&mut self, name: &str, target: &str) -> Option<Result<FileRef, &'static str>> {
        Some((|| {
            self.check_new(name)?;
            if target.is_empty() || target.len() >= PAGE {
                return Err(NOT_A_DIR_ENTRY);
            }
            let inode = Inode::new(&self.inode.vol, 0o777, Body::Symlink(target.to_string()));
            let ino = inode.ino;
            let f = TmpFile::new_ref(name, self.me.clone(), inode);
            self.add(name, FileOrDir::File(f.clone()), ino, NodeKind::Symlink, Some(target.to_string()));
            Ok(f)
        })())
    }

    fn create_special(&mut self, name: &str, kind: NodeKind, perm: u16, rdev: (u32, u32)) -> Option<Result<FileRef, &'static str>> {
        if !matches!(kind, NodeKind::Fifo | NodeKind::Socket | NodeKind::CharDev | NodeKind::BlockDev) {
            return None;
        }
        Some((|| {
            self.check_new(name)?;
            let inode = Inode::new(&self.inode.vol, perm & 0o7777, Body::Special { kind, rdev });
            let ino = inode.ino;
            let f = TmpFile::new_ref(name, self.me.clone(), inode);
            self.add(name, FileOrDir::File(f.clone()), ino, kind, None);
            Ok(f)
        })())
    }
}

// --- Montage -----------------------------------------------------------------------------------

/// Monte un volume tmpfs neuf en `parent/name` (droits de la racine `perm`, limite
/// `limit_bytes` ou la moitié de la RAM). Renvoie la racine du volume.
pub fn mount(name: &str, parent: &DirRef, perm: u16, limit_bytes: Option<usize>) -> Result<DirRef, &'static str> {
    let vol = Volume::new(name, limit_bytes);
    let root = TmpDir::new_ref(name, Arc::downgrade(parent), &vol, perm);
    parent.lock().insert(FileOrDir::Dir(root.clone()))?;
    Ok(root)
}

/// Volume tmpfs neuf inséré nulle part dans le VFS global (`mount -t tmpfs` d'un espace de
/// montage de MaiProc) : il ne vit que par son montage et ceux qui y sont.
pub fn new_instance(name: &str, perm: u16, limit_bytes: Option<usize>) -> DirRef {
    let vol = Volume::new(name, limit_bytes);
    TmpDir::new_ref(name, Weak::<Mutex<TmpDir>>::new() as WeakDirRef, &vol, perm)
}

/// Volume interne des fichiers anonymes (`memfd_create`), monté nulle part.
static ANON: spin::Once<DirRef> = spin::Once::new();
static ANON_SEQ: AtomicU64 = AtomicU64::new(0);

/// Fichier tmpfs anonyme (`memfd_create`) : créé puis aussitôt détaché de son dossier, il ne vit
/// que par ses descripteurs (nombre de liens nul), comme un fichier supprimé encore ouvert.
pub fn anon_file(perm: u16) -> Result<FileRef, &'static str> {
    let root = ANON.call_once(|| {
        let vol = Volume::new("memfd", None);
        TmpDir::new_ref("memfd", Weak::<Mutex<TmpDir>>::new() as WeakDirRef, &vol, 0o700)
    });
    let name = alloc::format!("memfd-{}", ANON_SEQ.fetch_add(1, Ordering::Relaxed));
    let made = root.lock().create_file(&name, perm);
    let file = made.unwrap_or(Err(NO_SPACE))?;
    let node = FileOrDir::File(file.clone());
    root.lock().remove(&node);
    Ok(file)
}
