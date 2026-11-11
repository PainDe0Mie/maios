//! Cache de pages d'un fichier, partagé par toutes ses projections (programmes, bibliothèques,
//! `mmap` privés).
//!
//! Sans lui, chaque processus copiait les pages de `libc.so.6` ou de `busybox` dans des frames
//! à lui. Avec lui, une page de fichier n'existe qu'une fois en mémoire : une faute en lecture ou
//! en exécution projette la frame du cache **en lecture seule**, et la copie à l'écriture
//! (décidée par le compteur de références, [`crate::refs`]) ne copie que les pages réellement
//! modifiées (`.data` relogée, GOT). C'est le rôle du *page cache* de Linux pour les projections
//! privées de fichiers.
//!
//! Le cache tient une référence sur chacune de ses frames ; chaque entrée de table qui la projette
//! en tient une autre. Une page n'est lue du fichier qu'à sa première faute (lecture paresseuse) ;
//! les octets au-delà de la fin du fichier sont nuls.
//!
//! # Lecture anticipée
//! Un défaut de page lit d'un coup une **fenêtre** de pages absentes (une seule requête à
//! l'appareil au lieu d'une par page), comme le *ondemand readahead* de Linux
//! (`mm/readahead.c`) : fenêtre initiale de [`RA_INIT`] pages, doublée à chaque défaut qui suit
//! exactement la fenêtre précédente (accès séquentiel), jusqu'à [`RA_MAX`] ; un accès ailleurs la
//! ramène à sa taille initiale. La fenêtre s'arrête à la première page déjà présente et à la fin
//! du fichier.

use alloc::{boxed::Box, collections::BTreeMap, sync::Arc, vec, vec::Vec};
use kernel_config::memory::PAGE_SIZE;
use memory::paging::kernel_half::phys_to_virt;
use spin::Mutex;

use crate::PageSource;

/// Lecture du fichier sous-jacent.
pub trait FileReader: Send + Sync {
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Lit à partir de `offset` ; renvoie le nombre d'octets lus.
    fn read_at(&self, offset: usize, dst: &mut [u8]) -> usize;
}

/// Fenêtre initiale de lecture anticipée (pages).
pub const RA_INIT: usize = 4;
/// Fenêtre maximale (128 Kio, `read_ahead_kb` par défaut de Linux).
pub const RA_MAX: usize = 32;
/// Pages lues au plus par une lecture anticipée demandée (8 Mio).
pub const PREFETCH_MAX: usize = 2048;

struct Pages {
    /// Rang de page → frame.
    map: BTreeMap<usize, usize>,
    /// Page qui suivrait la dernière fenêtre lue, et taille de cette fenêtre.
    ra_next: usize,
    ra_size: usize,
}

/// Pages en cache d'un fichier.
pub struct CachedFile {
    src: Box<dyn FileReader>,
    len: usize,
    pages: Mutex<Pages>,
}

impl CachedFile {
    pub fn new(src: Box<dyn FileReader>) -> Arc<CachedFile> {
        // Les frames du cache sont comptées ([`crate::refs`]) : la table doit exister avant la
        // première page, qui peut précéder le premier `Mm` (en-têtes du premier programme).
        if let Err(e) = crate::init() {
            log::error!("uvm : cache de pages sans compteurs de frames : {}", e);
        }
        let len = src.len();
        Arc::new(CachedFile { src, len, pages: Mutex::new(Pages { map: BTreeMap::new(), ra_next: usize::MAX, ra_size: 0 }) })
    }

    /// Lit la fenêtre qui commence à la page absente `index` ; rend la frame de `index`.
    fn fill(&self, p: &mut Pages, index: usize) -> Option<usize> {
        let window = if index == p.ra_next { (p.ra_size * 2).clamp(RA_INIT, RA_MAX) } else { RA_INIT };
        let last = (self.len - 1) / PAGE_SIZE;
        let mut count = 1;
        while count < window && index + count <= last && !p.map.contains_key(&(index + count)) {
            count += 1;
        }
        let mut pfns: Vec<usize> = Vec::with_capacity(count);
        while pfns.len() < count {
            match frame_allocator::alloc_raw_frames(1) {
                Some(f) => pfns.push(f),
                None => break,
            }
        }
        if pfns.is_empty() {
            return None;
        }
        let count = pfns.len();
        let off = index * PAGE_SIZE;
        let want = (count * PAGE_SIZE).min(self.len - off);
        // SAFETY : frames neuves, à nous seuls, accessibles par la physmap.
        let frame = |pfn: usize| unsafe { core::slice::from_raw_parts_mut(phys_to_virt(pfn * PAGE_SIZE) as *mut u8, PAGE_SIZE) };
        if count == 1 {
            let dst = frame(pfns[0]);
            let got = self.src.read_at(off, &mut dst[..want]);
            dst[got..].fill(0);
        } else {
            // Une seule requête pour toute la fenêtre.
            let mut buf = vec![0u8; want];
            let got = self.src.read_at(off, &mut buf);
            buf[got..].fill(0);
            for (i, &pfn) in pfns.iter().enumerate() {
                let dst = frame(pfn);
                let s = i * PAGE_SIZE;
                let n = PAGE_SIZE.min(want.saturating_sub(s));
                dst[..n].copy_from_slice(&buf[s..s + n]);
                dst[n..].fill(0);
            }
        }
        for (i, &pfn) in pfns.iter().enumerate() {
            p.map.insert(index + i, pfn);
        }
        p.ra_next = index + count;
        p.ra_size = count;
        Some(pfns[0])
    }

    /// Frame de la page `index` (lue du fichier si besoin), avec une référence de plus pour
    /// l'appelant. `None` : au-delà du fichier, ou mémoire insuffisante.
    pub fn page(&self, index: usize) -> Option<usize> {
        if index * PAGE_SIZE >= self.len {
            return None;
        }
        let mut pages = self.pages.lock();
        let pfn = match pages.map.get(&index) {
            Some(&p) => p,
            None => self.fill(&mut pages, index)?,
        };
        crate::refs::get(pfn);
        Some(pfn)
    }

    /// Copie `dst.len()` octets à partir de `offset`, à travers le cache (en-têtes ELF).
    pub fn read(&self, offset: usize, dst: &mut [u8]) -> usize {
        let end = (offset + dst.len()).min(self.len);
        let mut pos = offset;
        while pos < end {
            let index = pos / PAGE_SIZE;
            let Some(pfn) = self.page(index) else { break };
            let in_page = pos % PAGE_SIZE;
            let n = (PAGE_SIZE - in_page).min(end - pos);
            // SAFETY : frame du cache (référence prise par `page`), accessible par la physmap.
            let src = unsafe { core::slice::from_raw_parts((phys_to_virt(pfn * PAGE_SIZE) + in_page) as *const u8, n) };
            dst[pos - offset..pos - offset + n].copy_from_slice(src);
            crate::refs::put_page(pfn);
            pos += n;
        }
        pos.saturating_sub(offset)
    }

    /// Lecture anticipée demandée (`readahead`, `POSIX_FADV_WILLNEED`) : lit dans le cache les
    /// pages absentes de `[offset, offset + len)`, par fenêtres de [`RA_MAX`] pages, au plus
    /// [`PREFETCH_MAX`] pages par appel (comme Linux, qui borne aussi chaque demande). S'arrête
    /// sans erreur quand la mémoire manque. Rend le nombre de pages lues.
    pub fn prefetch(&self, offset: usize, len: usize) -> usize {
        if len == 0 || offset >= self.len {
            return 0;
        }
        let end = offset.saturating_add(len).min(self.len);
        let last = (end - 1) / PAGE_SIZE;
        let mut index = offset / PAGE_SIZE;
        let mut read = 0;
        while index <= last && read < PREFETCH_MAX {
            // Verrou relâché entre deux fenêtres : les fautes des projections passent entre.
            let mut p = self.pages.lock();
            if p.map.contains_key(&index) {
                index += 1;
                continue;
            }
            let before = p.map.len();
            p.ra_next = index;
            p.ra_size = RA_MAX;
            if self.fill(&mut p, index).is_none() {
                break;
            }
            let got = p.map.len() - before;
            read += got;
            index += got.max(1);
        }
        read
    }

    /// Pages présentes.
    pub fn resident(&self) -> usize {
        self.pages.lock().map.len()
    }
}

impl PageSource for CachedFile {
    fn len(&self) -> usize {
        self.len
    }

    fn read_at(&self, offset: usize, dst: &mut [u8]) {
        let n = self.read(offset, dst);
        dst[n..].fill(0);
    }

    fn cached_page(&self, index: usize) -> Option<usize> {
        self.page(index)
    }
}

impl Drop for CachedFile {
    fn drop(&mut self) {
        for (_, pfn) in core::mem::take(&mut self.pages.lock().map) {
            crate::refs::put_page(pfn);
        }
    }
}
