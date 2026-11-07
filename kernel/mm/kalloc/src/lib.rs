//! kalloc : l'allocateur d'objets du noyau MaiOS (MaiMM, phase M2).
//!
//! Remplace `multiple_heaps` + `slabmalloc` (un tas par cœur sous verrou, pages de 8 Kio, mémoire
//! jamais rendue). Mécanismes repris des travaux publiés :
//!
//! * **Segments de 2 Mio alignés** découpés en tranches de 64 Kio ; l'en-tête du segment porte
//!   les descripteurs de pages, retrouvés par simple masque d'adresse (mimalloc, Leijen et al.,
//!   « Mimalloc: Free List Sharding in Action », MSR-TR-2019-18). Les frames de chaque segment
//!   sont contiguës et alignées sur 2 Mio : M4 pourra les promouvoir en page huge sans copie.
//! * **Listes libres par page** (*free list sharding*) : `free` pour l'allocation, `local_free`
//!   pour les libérations du cœur propriétaire, `thread_free` (atomique) pour celles des autres
//!   cœurs. Le chemin rapide d'allocation et de libération locale ne contient aucune opération
//!   atomique, comme les *sheaves* de Linux 6.18 ou les magazines de Bonwick (USENIX '01).
//! * **Libérations distantes sans verrou**, groupées : un CAS par bloc, récupérées en une seule
//!   opération par le propriétaire (même objectif que les files de messages de snmalloc, Liétar
//!   et al., ISMM '19). Pour une page pleine, le protocole de libération différée de mimalloc
//!   (`USE_DELAYED` / `DELAYED_FREEING`) prévient le propriétaire.
//! * **Extension paresseuse** des pages par 4 Kio et **retour des pages et segments vides** :
//!   la mémoire libérée revient à LLFree.
//! * **Durcissement** (mode sécurisé de mimalloc) : liens chiffrés par des clés par page,
//!   contrôle de portée de chaque lien décodé, détection des doubles libérations, témoin de
//!   segment contre les libérations de pointeurs étrangers.
//!
//! Cette crate est le cœur pur : sans dépendance, testée sur l'hôte (`src/tests.rs`). La partie
//! noyau (crate `heap`) coupe les interruptions autour de chaque appel, fournit l'identifiant du
//! cœur, mappe les segments et sert directement les objets de plus de 128 Kio.

#![no_std]
#![allow(clippy::missing_safety_doc)]

#[cfg(test)]
extern crate std;

pub mod class;
mod heap;
mod sync;
#[cfg(test)]
mod tests;

use core::ptr::null_mut;
use core::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering::*};

use heap::{Heap, Segment};
use sync::SpinLock;

pub const SEGMENT_SHIFT: usize = 21;
/// Taille (et alignement) d'un segment : 2 Mio.
pub const SEGMENT_SIZE: usize = 1 << SEGMENT_SHIFT;
pub const SEGMENT_MASK: usize = SEGMENT_SIZE - 1;
pub const SLICE_SHIFT: usize = 16;
/// Une tranche : 64 Kio.
pub const SLICE_SIZE: usize = 1 << SLICE_SHIFT;
pub const SLICES: usize = SEGMENT_SIZE / SLICE_SIZE;
/// En-tête de segment (descripteurs de pages + tas du cœur pour le segment « maison »).
pub const HEADER_SIZE: usize = 8192;
/// Nombre maximal de cœurs (identifiants 0..MAX_CPUS).
pub const MAX_CPUS: usize = 256;
/// Mots réservés au système dans chaque en-tête de segment (son mappage, par exemple).
pub const OS_WORDS: usize = 8;
/// Alignement maximal servi par les classes ; au-delà, mappage direct.
pub const MAX_ALIGN: usize = 4096;
pub use class::MAX_SMALL;
/// Plus grand objet servi dans les segments (pages larges) ; au-delà, mappage direct.
pub const MAX_LARGE: usize = 1 << 20;

/// Vrai si un objet (`size`, `align`) est servi dans les segments de kalloc (classes ou pages
/// larges) ; sinon il revient au système (mappage direct).
#[inline(always)]
pub fn in_segments(size: usize, align: usize) -> bool {
    size <= MAX_LARGE && align <= MAX_ALIGN
}

/// Résultat d'une allocation.
#[derive(Debug, PartialEq, Eq)]
pub enum Alloc {
    Ok(*mut u8),
    /// Il faut fournir un segment ([`Kalloc::add_segment`]) puis réessayer.
    NeedSegment,
    /// Objet trop grand ou trop aligné : à servir par mappage direct.
    Large,
}

/// Résultat d'une libération.
#[derive(Debug, PartialEq, Eq)]
pub enum Free {
    Done,
    /// Fait, et la réserve globale dépasse son plafond : appeler [`Kalloc::trim`].
    Trim,
    /// Objet servi par mappage direct.
    Large,
}

/// Segment rendu au système.
#[derive(Debug, Clone, Copy)]
pub struct SegmentHandle {
    pub base: usize,
    pub os: [usize; OS_WORDS],
}

/// Statistiques cumulées de tous les tas.
#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    pub allocs: u64,
    pub frees: u64,
    pub remote_frees: u64,
    pub slow_allocs: u64,
    pub delayed_frees: u64,
    pub pages_fresh: u64,
    pub pages_retired: u64,
    pub segments_taken: u64,
    pub segments_released: u64,
    /// Segments engagés (mappés) par kalloc, réserve comprise.
    pub segments: usize,
    /// Segments dans la réserve globale.
    pub pool: usize,
    pub heaps: usize,
}

struct Pool {
    head: *mut Segment,
    count: usize,
}

// SAFETY: la réserve n'est accessible que sous son verrou.
unsafe impl Send for Pool {}

pub struct Kalloc {
    heaps: [AtomicPtr<Heap>; MAX_CPUS],
    pool: SpinLock<Pool>,
    pool_cap: AtomicUsize,
    cookie: AtomicUsize,
    seed: AtomicU64,
    segments: AtomicUsize,
}

// SAFETY: tout état partagé est atomique ou sous verrou ; le reste appartient à un cœur.
unsafe impl Sync for Kalloc {}
unsafe impl Send for Kalloc {}

#[allow(clippy::declare_interior_mutable_const)]
const NULL_HEAP: AtomicPtr<Heap> = AtomicPtr::new(null_mut());

/// splitmix64 (Steele et al., OOPSLA '14) : dérivation des clés.
#[inline]
fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl Default for Kalloc {
    fn default() -> Self {
        Self::new()
    }
}

impl Kalloc {
    pub const fn new() -> Self {
        Kalloc {
            heaps: [NULL_HEAP; MAX_CPUS],
            pool: SpinLock::new(Pool { head: null_mut(), count: 0 }),
            pool_cap: AtomicUsize::new(8),
            cookie: AtomicUsize::new(0x5A5A_A5A5_5A5A_A5A5),
            seed: AtomicU64::new(0x2545_F491_4F6C_DD1D),
            segments: AtomicUsize::new(0),
        }
    }

    /// Fixe la graine aléatoire (clés de chiffrement, témoin). À appeler avant le premier segment.
    pub fn init(&self, seed: u64) {
        let s = mix(seed);
        self.seed.store(s, Relaxed);
        self.cookie.store((mix(s) as usize) | 1, Relaxed);
    }

    /// Nombre de segments vides gardés dans la réserve globale avant d'en rendre au système.
    pub fn set_pool_cap(&self, n: usize) {
        self.pool_cap.store(n, Relaxed);
    }

    #[inline(always)]
    pub(crate) fn cookie(&self) -> usize {
        self.cookie.load(Relaxed)
    }

    pub(crate) fn page_keys(&self, page: usize) -> [usize; 2] {
        let a = mix(self.seed.load(Relaxed) ^ page as u64);
        let b = mix(a);
        [a as usize, (b as usize) | 1]
    }

    /// Alloue `size` octets alignés sur `align` sur le cœur `cpu`.
    ///
    /// # Safety
    /// Interruptions coupées sur le cœur `cpu` (personne d'autre n'utilise son tas pendant
    /// l'appel) ; `size` > 0, `align` puissance de deux.
    #[inline(always)]
    pub unsafe fn alloc(&self, cpu: usize, size: usize, align: usize) -> Alloc {
        let r = match class::lookup(size, align) {
            Some(c) => heap::alloc(self, cpu, c),
            None if in_segments(size, align) => heap::alloc_large(self, cpu, size),
            None => return Alloc::Large,
        };
        match r {
            Ok(p) => Alloc::Ok(p),
            Err(()) => Alloc::NeedSegment,
        }
    }

    /// Libère `ptr`, alloué avec (`size`, `align`), depuis le cœur `cpu`.
    ///
    /// # Safety
    /// Mêmes conditions que [`Kalloc::alloc`] ; `ptr` vient de cet allocateur avec cette taille.
    #[inline(always)]
    pub unsafe fn free(&self, cpu: usize, ptr: *mut u8, size: usize, align: usize) -> Free {
        if !in_segments(size, align) {
            return Free::Large;
        }
        if heap::free(self, cpu, ptr as usize) {
            Free::Trim
        } else {
            Free::Done
        }
    }

    /// Taille réellement disponible dans le bloc de `ptr` (objet servi dans les segments).
    ///
    /// # Safety
    /// `ptr` est un bloc vivant de cet allocateur.
    pub unsafe fn block_size(&self, ptr: *mut u8) -> usize {
        heap::block_size(ptr as usize)
    }

    /// Donne un segment neuf à l'allocateur.
    ///
    /// # Safety
    /// `base` est aligné sur [`SEGMENT_SIZE`], et les `SEGMENT_SIZE` octets sont lisibles et
    /// inscriptibles, à l'usage exclusif de kalloc jusqu'à ce que [`Kalloc::trim`] les rende.
    pub unsafe fn add_segment(&self, base: usize, os: [usize; OS_WORDS]) {
        debug_assert_eq!(base & SEGMENT_MASK, 0);
        let s = heap::segment_init(self, base, os);
        self.segments.fetch_add(1, Relaxed);
        let mut pool = self.pool.lock();
        *heap::segment_next(s) = pool.head;
        pool.head = s;
        pool.count += 1;
    }

    /// Rend au système un segment de la réserve si elle dépasse son plafond.
    ///
    /// # Safety
    /// Le segment rendu ne doit plus être accédé par kalloc : l'appelant le démappe.
    pub unsafe fn trim(&self) -> Option<SegmentHandle> {
        let mut pool = self.pool.lock();
        if pool.count <= self.pool_cap.load(Relaxed) || pool.head.is_null() {
            return None;
        }
        let s = pool.head;
        pool.head = *heap::segment_next(s);
        pool.count -= 1;
        drop(pool);
        self.segments.fetch_sub(1, Relaxed);
        Some(SegmentHandle { base: s as usize, os: heap::segment_os(s) })
    }

    /// Maintenance du tas du cœur `cpu` (à appeler depuis sa tâche inactive) : libérations
    /// distantes en attente, pages et segments vides rendus. Vrai si [`Kalloc::trim`] a du
    /// travail.
    ///
    /// # Safety
    /// Interruptions coupées sur le cœur `cpu`.
    pub unsafe fn collect(&self, cpu: usize) -> bool {
        let h = self.heaps[cpu].load(Relaxed);
        if h.is_null() {
            return false;
        }
        heap::collect(self, h)
    }

    pub(crate) unsafe fn pool_take(&self) -> *mut Segment {
        let mut pool = self.pool.lock();
        let s = pool.head;
        if !s.is_null() {
            pool.head = *heap::segment_next(s);
            pool.count -= 1;
            drop(pool);
            heap::segment_reset(s);
        }
        s
    }

    /// Remet un segment vide dans la réserve. Vrai si elle dépasse son plafond.
    pub(crate) unsafe fn pool_put(&self, s: *mut Segment) -> bool {
        heap::segment_reset(s);
        let mut pool = self.pool.lock();
        *heap::segment_next(s) = pool.head;
        pool.head = s;
        pool.count += 1;
        pool.count > self.pool_cap.load(Relaxed)
    }

    /// Segments vides disponibles dans la réserve globale.
    pub fn pool_len(&self) -> usize {
        self.pool.lock().count
    }

    /// Statistiques (lectures relâchées : instantané approximatif).
    pub fn stats(&self) -> Stats {
        let mut st = Stats {
            segments: self.segments.load(Relaxed),
            pool: self.pool.lock().count,
            ..Stats::default()
        };
        for cpu in 0..MAX_CPUS {
            self.add_heap_counters(cpu, &mut st);
        }
        st
    }

    /// Compteurs du seul tas du cœur `cpu` (`None` s'il n'a jamais alloué).
    pub fn heap_stats(&self, cpu: usize) -> Option<Stats> {
        let mut st = Stats::default();
        if self.add_heap_counters(cpu, &mut st) {
            Some(st)
        } else {
            None
        }
    }

    fn add_heap_counters(&self, cpu: usize, st: &mut Stats) -> bool {
        let h = match self.heaps.get(cpu) {
            Some(h) => h.load(Acquire),
            None => return false,
        };
        if h.is_null() {
            return false;
        }
        st.heaps += 1;
        // SAFETY: un tas n'est jamais détruit ; compteurs atomiques.
        let c = unsafe { heap::heap_counters(h) };
        st.allocs += c.allocs.load(Relaxed);
        st.frees += c.frees.load(Relaxed);
        st.remote_frees += c.remote_frees.load(Relaxed);
        st.slow_allocs += c.slow_allocs.load(Relaxed);
        st.delayed_frees += c.delayed_frees.load(Relaxed);
        st.pages_fresh += c.pages_fresh.load(Relaxed);
        st.pages_retired += c.pages_retired.load(Relaxed);
        st.segments_taken += c.segments_taken.load(Relaxed);
        st.segments_released += c.segments_released.load(Relaxed);
        true
    }
}
