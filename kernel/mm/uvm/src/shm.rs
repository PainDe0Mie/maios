//! Objets de mémoire partagée : pages anonymes communes à toutes les projections `MAP_SHARED`
//! qui les désignent (y compris celles héritées par `fork`).
//!
//! L'objet tient une référence sur chacune de ses pages ; chaque entrée de table qui la projette
//! en tient une autre ([`crate::refs`]). Une page est créée (mise à zéro) à la première faute,
//! quel que soit le processus qui la prend : parent et enfant voient donc la même page même si
//! aucun des deux ne l'avait touchée avant le `fork` — ce qu'un simple partage des tables ne
//! garantirait pas.
//!
//! # Projections partagées de fichiers (MaiSS)
//! Un objet peut être adossé à un fichier ([`ShmObject::for_file`]) : une page est lue du
//! fichier à sa première faute, toutes les projections `MAP_SHARED` du fichier (de tous les
//! processus) partagent la même frame, et les pages projetées en écriture sont réécrites dans le
//! fichier (*writeback*) par `msync`, `fsync`, le vidage périodique, et à la disparition de
//! l'objet. Une page projetée en écriture reste « sale » tant qu'elle existe : sans balayage des
//! bits *Dirty* des tables, on ne sait pas si elle a été modifiée depuis le dernier vidage, et
//! on la réécrit donc à chaque fois (bornée par la fin du fichier, jamais d'agrandissement).

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use kernel_config::memory::PAGE_SIZE;
use memory::paging::kernel_half::phys_to_virt;
use spin::Mutex;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Accès au fichier derrière un objet partagé.
pub trait FileIo: Send + Sync {
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Lit à partir de `offset` ; renvoie le nombre d'octets lus.
    fn read_at(&self, offset: usize, dst: &mut [u8]) -> usize;
    /// Écrit à partir de `offset` (sans dépasser la fin du fichier) ; renvoie le nombre d'octets
    /// écrits.
    fn write_at(&self, offset: usize, src: &[u8]) -> usize;
}

/// Page d'un objet : frame, et projetée en écriture (à réécrire dans le fichier).
#[derive(Clone, Copy)]
struct Page {
    pfn: usize,
    dirty: bool,
}

/// Objet de mémoire partagée (équivalent de `shmem` pour l'anonyme partagé).
pub struct ShmObject {
    id: u64,
    /// Rang de page → frame.
    pages: Mutex<BTreeMap<usize, Page>>,
    /// Scellé : jamais projeté en écriture (`mprotect` refuse `PROT_WRITE`), comme le vDSO et
    /// sa page de données (`VM_MAYWRITE` retiré sous Linux).
    sealed: bool,
    /// Fichier derrière l'objet (projection partagée de fichier).
    file: Option<alloc::boxed::Box<dyn FileIo>>,
}

impl ShmObject {
    pub fn new() -> Arc<ShmObject> {
        Arc::new(ShmObject { id: NEXT_ID.fetch_add(1, Ordering::Relaxed), pages: Mutex::new(BTreeMap::new()), sealed: false, file: None })
    }

    /// Objet scellé formé des frames données (l'objet prend la référence de l'appelant sur
    /// chacune).
    pub fn sealed_from_frames(frames: &[usize]) -> Arc<ShmObject> {
        let _ = crate::init();
        let pages = frames.iter().enumerate().map(|(i, &f)| (i, Page { pfn: f, dirty: false })).collect();
        Arc::new(ShmObject { id: NEXT_ID.fetch_add(1, Ordering::Relaxed), pages: Mutex::new(pages), sealed: true, file: None })
    }

    /// Objet formé des frames données, projetable en écriture (anneaux d'io_uring : le noyau
    /// les lit et les écrit par la physmap, le programme par sa projection). L'objet prend la
    /// référence de l'appelant sur chacune.
    pub fn from_frames(frames: &[usize]) -> Arc<ShmObject> {
        let _ = crate::init();
        let pages = frames.iter().enumerate().map(|(i, &f)| (i, Page { pfn: f, dirty: false })).collect();
        Arc::new(ShmObject { id: NEXT_ID.fetch_add(1, Ordering::Relaxed), pages: Mutex::new(pages), sealed: false, file: None })
    }

    pub fn is_sealed(&self) -> bool { self.sealed }

    /// Identifiant unique (clés de futex partagés).
    pub fn id(&self) -> u64 { self.id }

    /// Objet adossé au fichier `file` (projections `MAP_SHARED` d'un fichier).
    pub fn for_file(file: alloc::boxed::Box<dyn FileIo>) -> Arc<ShmObject> {
        let _ = crate::init();
        Arc::new(ShmObject { id: NEXT_ID.fetch_add(1, Ordering::Relaxed), pages: Mutex::new(BTreeMap::new()), sealed: false, file: Some(file) })
    }

    pub fn is_file(&self) -> bool { self.file.is_some() }

    /// Frame de la page `index`, créée si besoin (lue du fichier pour un objet de fichier), avec
    /// une référence de plus pour l'appelant (qui va la projeter). `None` : mémoire insuffisante.
    pub fn get_page(&self, index: usize) -> Option<usize> {
        self.get_page_for(index, false)
    }

    /// Comme [`get_page`](Self::get_page) ; `write` : la page sera projetée en écriture (elle
    /// devient à réécrire dans le fichier).
    pub fn get_page_for(&self, index: usize, write: bool) -> Option<usize> {
        let mut pages = self.pages.lock();
        let pfn = match pages.get_mut(&index) {
            Some(p) => {
                p.dirty |= write && self.file.is_some();
                p.pfn
            }
            // Un objet scellé ne grandit pas (projection agrandie au-delà du vDSO).
            None if self.sealed => return None,
            None => {
                let (pfn, zeroed) = frame_allocator::alloc_raw_frame_prefer_zeroed()?;
                // SAFETY : frame neuve, accessible par la physmap.
                let page = unsafe { core::slice::from_raw_parts_mut(phys_to_virt(pfn * PAGE_SIZE) as *mut u8, PAGE_SIZE) };
                match &self.file {
                    Some(f) => {
                        let off = index * PAGE_SIZE;
                        let want = PAGE_SIZE.min(f.len().saturating_sub(off));
                        let got = if want > 0 { f.read_at(off, &mut page[..want]) } else { 0 };
                        page[got..].fill(0);
                    }
                    None if !zeroed => page.fill(0),
                    None => {}
                }
                pages.insert(index, Page { pfn, dirty: write && self.file.is_some() });
                pfn
            }
        };
        crate::refs::get(pfn);
        Some(pfn)
    }

    /// La page `index` a été rendue inscriptible (montée de droits) : à réécrire.
    pub fn mark_dirty(&self, index: usize) {
        if self.file.is_some() {
            if let Some(p) = self.pages.lock().get_mut(&index) {
                p.dirty = true;
            }
        }
    }

    /// Réécrit dans le fichier les pages projetées en écriture dont le rang est dans
    /// `[first, last]`. Renvoie le nombre d'octets écrits. Sans effet sans fichier.
    pub fn writeback(&self, first: usize, last: usize) -> usize {
        let Some(f) = &self.file else { return 0 };
        // Liste prise sous le verrou, écriture hors du verrou (le fichier peut dormir : FAT32 sur
        // NVMe), chaque frame tenue par une référence pendant l'écriture.
        let dirty: alloc::vec::Vec<(usize, usize)> = {
            let pages = self.pages.lock();
            pages
                .range(first..=last)
                .filter(|(_, p)| p.dirty)
                .map(|(&i, p)| {
                    crate::refs::get(p.pfn);
                    (i, p.pfn)
                })
                .collect()
        };
        let len = f.len();
        let mut n = 0;
        for (i, pfn) in dirty {
            let off = i * PAGE_SIZE;
            if off < len {
                let count = PAGE_SIZE.min(len - off);
                // SAFETY : frame de l'objet, référence tenue.
                let src = unsafe { core::slice::from_raw_parts(phys_to_virt(pfn * PAGE_SIZE) as *const u8, count) };
                n += f.write_at(off, src);
            }
            crate::refs::put_page(pfn);
        }
        n
    }

    /// Recopie dans les pages présentes les octets `data` écrits dans le fichier à `offset` par
    /// `write()` : les projections voient l'écriture (cohérence du cache unifié).
    pub fn update_from_write(&self, offset: usize, data: &[u8]) {
        let pages = self.pages.lock();
        if pages.is_empty() || data.is_empty() {
            return;
        }
        let end = offset + data.len();
        for (&i, p) in pages.range(offset / PAGE_SIZE..=(end - 1) / PAGE_SIZE) {
            let ps = i * PAGE_SIZE;
            let lo = offset.max(ps);
            let hi = end.min(ps + PAGE_SIZE);
            // SAFETY : frame de l'objet, sous son verrou.
            unsafe {
                core::ptr::copy_nonoverlapping(data[lo - offset..].as_ptr(), (phys_to_virt(p.pfn * PAGE_SIZE) + (lo - ps)) as *mut u8, hi - lo);
            }
        }
    }

    /// Recouvre `dst` (lu du fichier à `offset` par `read()`) des octets des pages projetées en
    /// écriture : une lecture voit ce que les projections ont écrit et pas encore réécrit.
    pub fn overlay_read(&self, offset: usize, dst: &mut [u8]) {
        let pages = self.pages.lock();
        if pages.is_empty() || dst.is_empty() {
            return;
        }
        let end = offset + dst.len();
        for (&i, p) in pages.range(offset / PAGE_SIZE..=(end - 1) / PAGE_SIZE) {
            if !p.dirty {
                continue;
            }
            let ps = i * PAGE_SIZE;
            let lo = offset.max(ps);
            let hi = end.min(ps + PAGE_SIZE);
            // SAFETY : comme ci-dessus.
            unsafe {
                core::ptr::copy_nonoverlapping((phys_to_virt(p.pfn * PAGE_SIZE) + (lo - ps)) as *const u8, dst[lo - offset..].as_mut_ptr(), hi - lo);
            }
        }
    }

    /// Lit `dst.len()` octets à partir de `offset` tels que les projections les voient : pages
    /// présentes lues dans leur frame (physmap), autres pages lues du fichier, zéros au-delà
    /// de sa fin (là où une projection prendrait `SIGBUS`). Ne crée aucune page : un lecteur du
    /// noyau (serveur Wayland qui lit un pool `wl_shm`) ne fait pas grossir l'objet.
    pub fn read_at(&self, offset: usize, dst: &mut [u8]) {
        let pages = self.pages.lock();
        let mut done = 0usize;
        while done < dst.len() {
            let pos = offset + done;
            let index = pos / PAGE_SIZE;
            let in_page = pos % PAGE_SIZE;
            let n = (PAGE_SIZE - in_page).min(dst.len() - done);
            let out = &mut dst[done..done + n];
            match pages.get(&index) {
                // SAFETY : frame de l'objet, tenue par lui tant que son verrou est pris.
                Some(p) => unsafe {
                    core::ptr::copy_nonoverlapping((phys_to_virt(p.pfn * PAGE_SIZE) + in_page) as *const u8, out.as_mut_ptr(), n);
                },
                None => {
                    let got = match &self.file {
                        Some(f) if pos < f.len() => f.read_at(pos, &mut out[..n.min(f.len() - pos)]),
                        _ => 0,
                    };
                    out[got..].fill(0);
                }
            }
            done += n;
        }
    }

    /// Pages présentes.
    pub fn resident(&self) -> usize { self.pages.lock().len() }
}

impl Drop for ShmObject {
    fn drop(&mut self) {
        // Dernière projection disparue : ce qu'ont écrit les projections va au fichier.
        self.writeback(0, usize::MAX);
        for (_, p) in core::mem::take(&mut *self.pages.lock()) {
            crate::refs::put_page(p.pfn);
        }
    }
}
