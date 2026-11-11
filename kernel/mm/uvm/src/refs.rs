//! Compteurs de références des frames des processus.
//!
//! Une frame de la moitié basse (page de données ou table P1) peut appartenir à plusieurs
//! propriétaires après un `fork` : tables P1 partagées (On-Demand-Fork), pages en copie à
//! l'écriture, pages d'un objet de mémoire partagée. Le compteur d'une frame donne le nombre de
//! références **en plus de la première** : 0 (valeur initiale de toute frame) signifie « un seul
//! propriétaire », ce qui évite toute initialisation à l'allocation.
//!
//! La décision de copie à l'écriture repose sur ce compteur (une page n'est réutilisée sur place
//! que si son propriétaire est seul), comme Linux depuis la refonte de D. Hildenbrand
//! (`PageAnonExclusive`, 2022) : c'est le compteur de références, et non le nombre de
//! projections, qui dit si une page peut être écrite sans copie.

use alloc::alloc::{alloc_zeroed, Layout};
use core::sync::atomic::{AtomicU32, Ordering};
use kernel_config::memory::PAGE_SIZE;
use memory::paging::kernel_half::frame_ptr;

static REFS: spin::Once<&'static [AtomicU32]> = spin::Once::new();

/// Alloue la table des compteurs (une entrée par frame de mémoire générale). Idempotent.
pub fn init() -> Result<(), &'static str> {
    let mut err = None;
    REFS.call_once(|| {
        let max = frame_allocator::general_memory_ranges().iter().map(|r| r.1).max().unwrap_or(0);
        let layout = match Layout::array::<AtomicU32>(max.max(1)) {
            Ok(l) => l,
            Err(_) => {
                err = Some("uvm::refs : taille de table invalide");
                return &[];
            }
        };
        // SAFETY : taille non nulle ; une suite de zéros est un `AtomicU32` valide.
        let ptr = unsafe { alloc_zeroed(layout) } as *const AtomicU32;
        if ptr.is_null() {
            err = Some("uvm::refs : memoire insuffisante pour les compteurs de frames");
            return &[];
        }
        // SAFETY : zone fraîche de `max` compteurs, jamais libérée.
        unsafe { core::slice::from_raw_parts(ptr, max) }
    });
    match err {
        Some(e) => Err(e),
        None if REFS.get().map_or(true, |t| t.is_empty()) => Err("uvm::refs : aucune memoire generale"),
        None => Ok(()),
    }
}

#[inline]
fn slot(pfn: usize) -> Option<&'static AtomicU32> {
    REFS.get().and_then(|t| t.get(pfn))
}

/// Ajoute une référence à `pfn` (l'appelant en détient déjà une).
#[inline]
pub fn get(pfn: usize) {
    match slot(pfn) {
        Some(s) => {
            s.fetch_add(1, Ordering::Relaxed);
        }
        None => log::error!("uvm::refs : frame {:#x} hors de la memoire generale", pfn),
    }
}

/// Vrai si `pfn` a plusieurs propriétaires.
#[inline]
pub fn is_shared(pfn: usize) -> bool {
    slot(pfn).map_or(false, |s| s.load(Ordering::Acquire) != 0)
}

/// Retire une référence : `true` si c'était la dernière (l'appelant libère alors la frame).
#[inline]
pub fn dec(pfn: usize) -> bool {
    let Some(s) = slot(pfn) else { return true };
    let mut cur = s.load(Ordering::Acquire);
    loop {
        if cur == 0 {
            return true;
        }
        match s.compare_exchange_weak(cur, cur - 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return false,
            Err(v) => cur = v,
        }
    }
}

/// Rend une référence sur une page de données (libérée si c'était la dernière).
#[inline]
pub fn put_page(pfn: usize) {
    if dec(pfn) {
        // SAFETY : plus aucun propriétaire.
        unsafe { frame_allocator::free_raw_frames(pfn, 1) };
    }
}

/// Rend une référence sur une table P1 : si c'était la dernière, les pages qu'elle projette
/// perdent la référence qu'elle portait, puis la table est libérée.
pub fn put_table(p1_pfn: usize) {
    if !dec(p1_pfn) {
        return;
    }
    let p1: *mut u64 = frame_ptr(p1_pfn);
    for i in 0..PAGE_SIZE / 8 {
        // SAFETY : table devenue privée (dernier propriétaire), accessible par la physmap.
        let e = unsafe { *p1.add(i) };
        if e & crate::pt::PTE_PRESENT != 0 {
            put_page(crate::pt::pte_addr(e) / PAGE_SIZE);
        }
    }
    // SAFETY : plus aucun propriétaire.
    unsafe { frame_allocator::free_raw_frames(p1_pfn, 1) };
}
