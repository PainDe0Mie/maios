//! Tables de pages de la moitié basse, manipulées par la physmap.

use kernel_config::memory::PAGE_SIZE;
use memory::paging::address_space::alloc_table_frame;
use memory::paging::kernel_half::frame_ptr;
use crate::Prot;

pub const PTE_PRESENT: u64 = 1 << 0;
pub const PTE_WRITABLE: u64 = 1 << 1;
const PTE_USER: u64 = 1 << 2;
const PTE_ACCESSED: u64 = 1 << 5;
const PTE_DIRTY: u64 = 1 << 6;
const PTE_HUGE: u64 = 1 << 7;
const PTE_NX: u64 = 1 << 63;
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
/// Entrée intermédiaire : les droits effectifs sont ceux de la feuille.
const TABLE_FLAGS: u64 = PTE_PRESENT | PTE_WRITABLE | PTE_USER;

#[inline]
pub fn pte_addr(pte: u64) -> usize { (pte & ADDR_MASK) as usize }

#[inline]
fn idx(va: usize, level: u32) -> usize { (va >> (12 + 9 * (level - 1))) & 0x1FF }

/// Entrée feuille pour `paddr` avec les droits `prot` (A et D posés d'avance : pas
/// d'assistance matérielle à la première écriture).
pub fn leaf(paddr: usize, prot: Prot) -> u64 {
    let mut e = paddr as u64 & ADDR_MASK;
    if prot.read() { e |= PTE_PRESENT | PTE_USER | PTE_ACCESSED; }
    if prot.write() { e |= PTE_WRITABLE | PTE_DIRTY; }
    if !prot.exec() { e |= PTE_NX; }
    e
}

/// Même frame, nouveaux droits.
pub fn with_prot(pte: u64, prot: Prot) -> u64 {
    leaf(pte_addr(pte), prot)
}

/// Entrée feuille de `va`, si toutes les tables existent.
pub unsafe fn lookup(p4_pfn: usize, va: usize) -> Option<u64> {
    let mut table: *mut u64 = frame_ptr(p4_pfn);
    for level in (2..=4).rev() {
        let e = *table.add(idx(va, level));
        if e & PTE_PRESENT == 0 || e & PTE_HUGE != 0 { return None; }
        table = frame_ptr(pte_addr(e) / PAGE_SIZE);
    }
    Some(*table.add(idx(va, 1)))
}

/// Pointeur sur l'entrée feuille de `va`, en créant les tables manquantes.
pub unsafe fn walk_create(p4_pfn: usize, va: usize) -> Option<*mut u64> {
    let mut table: *mut u64 = frame_ptr(p4_pfn);
    for level in (2..=4).rev() {
        let slot = table.add(idx(va, level));
        if *slot & PTE_PRESENT == 0 {
            let pfn = alloc_table_frame()?;
            *slot = (pfn * PAGE_SIZE) as u64 | TABLE_FLAGS;
        }
        table = frame_ptr(pte_addr(*slot) / PAGE_SIZE);
    }
    Some(table.add(idx(va, 1)))
}

/// Appelle `f(va, entrée)` pour chaque feuille présente de `[start, end)`, en sautant les
/// tables absentes.
pub unsafe fn for_each_present(p4_pfn: usize, start: usize, end: usize, mut f: impl FnMut(usize, &mut u64)) {
    let mut va = start & !(PAGE_SIZE - 1);
    while va < end {
        let mut table: *mut u64 = frame_ptr(p4_pfn);
        let mut skipped = false;
        for level in (2..=4).rev() {
            let e = *table.add(idx(va, level));
            if e & PTE_PRESENT == 0 || e & PTE_HUGE != 0 {
                let span = 1usize << (12 + 9 * (level - 1));
                va = (va & !(span - 1)) + span;
                skipped = true;
                break;
            }
            table = frame_ptr(pte_addr(e) / PAGE_SIZE);
        }
        if skipped { continue; }
        let pte = &mut *table.add(idx(va, 1));
        if *pte & PTE_PRESENT != 0 {
            f(va, pte);
        }
        va += PAGE_SIZE;
    }
}
