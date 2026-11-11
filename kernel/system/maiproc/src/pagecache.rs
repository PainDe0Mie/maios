//! Registre des caches de pages des fichiers du VFS ([`uvm::CachedFile`]).
//!
//! Un fichier projeté (programme, bibliothèque, `mmap`) a un seul cache, retrouvé par l'identité
//! de son nœud. Le registre garde une référence faible sur le nœud : son adresse ne peut pas
//! être réutilisée par un autre fichier tant que l'entrée existe. Une écriture ou une troncature
//! par un processus invalide l'entrée (les projections existantes gardent l'ancien contenu,
//! comme le permet `MAP_PRIVATE`) ; un changement de taille par un autre chemin aussi.
//!
//! Les entrées sans projection sont gardées dans la limite de [`MAX_IDLE_PAGES`], les plus
//! anciennes rendues d'abord (le prochain `execve` de `busybox` ou de la `libc` retrouve ainsi
//! ses pages sans relire le fichier).
//!
//! # Projections partagées (MaiSS)
//! Un fichier projeté en `MAP_SHARED` a aussi un seul [`uvm::ShmObject`] adossé au fichier,
//! retrouvé par la même identité ([`shared`]) : tous les processus voient les mêmes frames. Le
//! registre ne garde qu'une référence faible (l'objet meurt avec sa dernière projection, et se
//! réécrit alors dans le fichier). `write()` recopie dans ses pages ([`after_write`]), `read()`
//! voit ce que les projections ont écrit ([`overlay_read`]), et la tâche `maiss_flush` réécrit
//! les pages sales toutes les [`FLUSH_PERIOD`] (les *flusher threads* de Linux, dont
//! `dirty_expire_centisecs` vaut 30 s ; ici 5 s).

use alloc::{boxed::Box, collections::BTreeMap, sync::{Arc, Weak}};
use fs_node::{File, FileRef};
use spin::Mutex;
use uvm::{CachedFile, FileIo, FileReader, ShmObject};

/// Pages gardées pour des fichiers qu'aucun processus ne projette (64 Mio).
const MAX_IDLE_PAGES: usize = 16 * 1024;

type Node = spin::Mutex<dyn File + Send>;

struct Entry {
    file: Weak<Node>,
    len: usize,
    cache: Arc<CachedFile>,
    last_use: u64,
}

struct Registry {
    entries: BTreeMap<usize, Entry>,
    clock: u64,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry { entries: BTreeMap::new(), clock: 0 });

struct VfsReader(FileRef);

impl FileReader for VfsReader {
    fn len(&self) -> usize {
        self.0.lock().len()
    }

    fn read_at(&self, offset: usize, dst: &mut [u8]) -> usize {
        self.0.lock().read_at(dst, offset).unwrap_or(0)
    }
}

fn key(file: &FileRef) -> usize {
    Arc::as_ptr(file) as *const () as usize
}

struct VfsIo(FileRef);

impl FileIo for VfsIo {
    fn len(&self) -> usize {
        self.0.lock().len()
    }

    fn read_at(&self, offset: usize, dst: &mut [u8]) -> usize {
        self.0.lock().read_at(dst, offset).unwrap_or(0)
    }

    fn write_at(&self, offset: usize, src: &[u8]) -> usize {
        let mut f = self.0.lock();
        let len = f.len();
        if offset >= len {
            return 0;
        }
        let n = src.len().min(len - offset);
        f.write_at(&src[..n], offset).unwrap_or(0)
    }
}

/// Objets partagés des fichiers projetés en `MAP_SHARED`, par identité du nœud.
static SHARED: Mutex<BTreeMap<usize, (Weak<Node>, Weak<ShmObject>)>> = Mutex::new(BTreeMap::new());
static FLUSHER: spin::Once<()> = spin::Once::new();

/// Période du vidage des pages sales des projections partagées.
pub const FLUSH_PERIOD: core::time::Duration = core::time::Duration::from_secs(5);

/// Objet partagé du fichier `file` (créé au besoin) : toutes ses projections `MAP_SHARED`.
pub fn shared(file: &FileRef) -> Arc<ShmObject> {
    let k = key(file);
    let mut m = SHARED.lock();
    if let Some((f, o)) = m.get(&k) {
        if f.strong_count() > 0 {
            if let Some(o) = o.upgrade() {
                return o;
            }
        }
    }
    let obj = ShmObject::for_file(Box::new(VfsIo(file.clone())));
    m.insert(k, (Arc::downgrade(file), Arc::downgrade(&obj)));
    m.retain(|_, (f, o)| f.strong_count() > 0 && o.strong_count() > 0);
    drop(m);
    FLUSHER.call_once(|| {
        if let Err(e) = spawn::new_task_builder(flusher, ()).name(alloc::string::String::from("maiss_flush")).spawn() {
            log::error!("maiproc : tâche de vidage non créée : {}", e);
        }
    });
    obj
}

fn existing(file: &FileRef) -> Option<Arc<ShmObject>> {
    SHARED.lock().get(&key(file)).and_then(|(_, o)| o.upgrade())
}

/// `write()` de `data` à `offset` dans `file` : les projections partagées voient l'écriture.
pub fn after_write(file: &FileRef, offset: usize, data: &[u8]) {
    if let Some(o) = existing(file) {
        o.update_from_write(offset, data);
    }
}

/// `read()` de `dst` à `offset` dans `file` : recouvert de ce que les projections ont écrit.
pub fn overlay_read(file: &FileRef, offset: usize, dst: &mut [u8]) {
    if let Some(o) = existing(file) {
        o.overlay_read(offset, dst);
    }
}

/// `fsync` : réécrit les pages sales des projections partagées de `file`.
pub fn sync_file(file: &FileRef) -> usize {
    existing(file).map_or(0, |o| o.writeback(0, usize::MAX))
}

fn flusher(_: ()) {
    loop {
        let _ = sleep::sleep(FLUSH_PERIOD);
        let objs: alloc::vec::Vec<Arc<ShmObject>> = SHARED.lock().values().filter_map(|(_, o)| o.upgrade()).collect();
        for o in objs {
            o.writeback(0, usize::MAX);
        }
    }
}

/// Cache du fichier `file` (créé au besoin).
pub fn get(file: &FileRef) -> Arc<CachedFile> {
    let len = file.lock().len();
    let k = key(file);
    let mut r = REGISTRY.lock();
    r.clock += 1;
    let now = r.clock;
    if let Some(e) = r.entries.get_mut(&k) {
        if e.len == len && e.file.strong_count() > 0 {
            e.last_use = now;
            return e.cache.clone();
        }
    }
    let cache = CachedFile::new(Box::new(VfsReader(file.clone())));
    r.entries.insert(k, Entry { file: Arc::downgrade(file), len, cache: cache.clone(), last_use: now });
    trim(&mut r);
    cache
}

/// Le contenu de `file` change : son cache n'est plus réutilisé.
pub fn invalidate(file: &FileRef) {
    REGISTRY.lock().entries.remove(&key(file));
}

/// Rend les caches inutilisés les plus anciens au-delà de la limite, et ceux des fichiers
/// disparus.
fn trim(r: &mut Registry) {
    r.entries.retain(|_, e| e.file.strong_count() > 0);
    loop {
        let idle: usize = r.entries.values().filter(|e| Arc::strong_count(&e.cache) == 1).map(|e| e.cache.resident()).sum();
        if idle <= MAX_IDLE_PAGES {
            return;
        }
        let oldest = r.entries.iter().filter(|(_, e)| Arc::strong_count(&e.cache) == 1).min_by_key(|(_, e)| e.last_use).map(|(k, _)| *k);
        match oldest {
            Some(k) => {
                r.entries.remove(&k);
            }
            None => return,
        }
    }
}

/// Plus grand fichier dont les `read()` passent par le cache (au-delà, lecture directe : un
/// fichier lu une fois ne doit pas occuper la mémoire en entier).
pub const MAX_READ_CACHED: usize = MAX_IDLE_PAGES * kernel_config::memory::PAGE_SIZE / 4;

/// Mémoire épuisée : rend tous les caches qu'aucune projection ni lecture ne tient (les pages
/// encore projetées vivent par leurs propres références). Rend le nombre de pages lâchées.
pub fn shrink() -> usize {
    let mut r = REGISTRY.lock();
    let mut freed = 0;
    r.entries.retain(|_, e| {
        let idle = Arc::strong_count(&e.cache) == 1 || e.file.strong_count() == 0;
        if idle {
            freed += e.cache.resident();
        }
        !idle
    });
    freed
}

/// (fichiers en cache, pages résidentes) pour `/proc/meminfo`.
pub fn stats() -> (usize, usize) {
    let r = REGISTRY.lock();
    (r.entries.len(), r.entries.values().map(|e| e.cache.resident()).sum())
}

/// `file` a-t-il des projections partagées (son objet partagé existe encore) ?
pub fn has_shared(file: &FileRef) -> bool {
    existing(file).is_some()
}
