//! Tables de pages de la moitié basse, manipulées par la physmap.
//!
//! ## Tables P1 partagées (On-Demand-Fork)
//! `fork` ne copie pas les tables du dernier niveau : le parent et l'enfant pointent sur les mêmes
//! P1, et l'entrée P2 de chacun perd le bit W, ce qui rend toutes les pages en dessous en lecture
//! seule pour le matériel (K. Zhao, S. Gong, P. Fonseca, *On-Demand-Fork*, EuroSys 2021 ; repris
//! dans la série Linux « COW PTE table »). Une table partagée est **copiée à la première
//! modification** ([`walk_create`], [`for_each_present_mut`]) : les entrées des pages privées
//! perdent alors le bit W des deux côtés (copie à l'écriture au niveau des pages) et chaque page
//! gagne une référence ([`crate::refs`]). Une entrée P2 sans W désigne donc toujours une table
//! partagée (le noyau n'en crée pas d'autre).

use kernel_config::memory::PAGE_SIZE;
use memory::paging::address_space::alloc_table_frame;
use memory::paging::kernel_half::frame_ptr;
use crate::{refs, tlb::Flush, Prot};

pub const PTE_PRESENT: u64 = 1 << 0;
pub const PTE_WRITABLE: u64 = 1 << 1;
const PTE_USER: u64 = 1 << 2;
const PTE_ACCESSED: u64 = 1 << 5;
const PTE_DIRTY: u64 = 1 << 6;
const PTE_HUGE: u64 = 1 << 7;
/// Bit logiciel : page d'une projection partagée (`MAP_SHARED`), jamais copiée à l'écriture.
pub const PTE_SHARED_MAP: u64 = 1 << 9;
const PTE_NX: u64 = 1 << 63;
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
/// Entrée intermédiaire : les droits effectifs sont ceux de la feuille.
const TABLE_FLAGS: u64 = PTE_PRESENT | PTE_WRITABLE | PTE_USER;
const ENTRIES: usize = 512;
/// Étendue d'une table P1 (2 Mio).
pub const P1_SPAN: usize = PAGE_SIZE * ENTRIES;

#[inline]
pub fn pte_addr(pte: u64) -> usize { (pte & ADDR_MASK) as usize }

#[inline]
pub fn pte_pfn(pte: u64) -> usize { pte_addr(pte) / PAGE_SIZE }

#[inline]
fn idx(va: usize, level: u32) -> usize { (va >> (12 + 9 * (level - 1))) & 0x1FF }

/// Entrée feuille pour `paddr` avec les droits `prot` (A et D posés d'avance : pas
/// d'assistance matérielle à la première écriture). `shared` : projection `MAP_SHARED`.
pub fn leaf(paddr: usize, prot: Prot, shared: bool) -> u64 {
    let mut e = paddr as u64 & ADDR_MASK;
    if prot.read() { e |= PTE_PRESENT | PTE_USER | PTE_ACCESSED; }
    if prot.write() { e |= PTE_WRITABLE | PTE_DIRTY; }
    if !prot.exec() { e |= PTE_NX; }
    if shared { e |= PTE_SHARED_MAP; }
    e
}

/// Même frame, nouveaux droits ; une page privée encore partagée (copie à l'écriture en attente)
/// reste en lecture seule.
pub fn protect(pte: u64, prot: Prot) -> u64 {
    let shared_map = pte & PTE_SHARED_MAP != 0;
    let mut p = prot;
    if p.write() && !shared_map && refs::is_shared(pte_pfn(pte)) {
        p = Prot(p.0 & !Prot::WRITE);
    }
    leaf(pte_addr(pte), p, shared_map)
}

/// Entrée feuille **effective** de `va` (le bit W est retiré si la table P1 est partagée), si
/// toutes les tables existent.
///
/// # Safety
/// `p4_pfn` est le P4 d'un espace dont l'appelant tient le verrou.
pub unsafe fn lookup(p4_pfn: usize, va: usize) -> Option<u64> {
    let mut table: *mut u64 = frame_ptr(p4_pfn);
    let mut writable = true;
    for level in (2..=4).rev() {
        let e = *table.add(idx(va, level));
        if e & PTE_PRESENT == 0 || e & PTE_HUGE != 0 { return None; }
        writable &= e & PTE_WRITABLE != 0;
        table = frame_ptr(pte_pfn(e));
    }
    let leaf = *table.add(idx(va, 1));
    Some(if writable { leaf } else { leaf & !PTE_WRITABLE })
}

/// Rend privée la table P1 désignée par l'entrée P2 `slot` (sans W), qui couvre `va` : copie si
/// elle a d'autres propriétaires, sinon simple reprise. `None` : mémoire insuffisante.
///
/// L'ancienne table n'est rendue qu'après invalidation (`fl`) : les caches de structures de
/// pagination des autres cœurs qui exécutent cet espace peuvent encore la désigner, et son
/// propriétaire restant va la modifier.
unsafe fn unshare_p1(slot: *mut u64, va: usize, fl: &mut Flush) -> Option<()> {
    let old_pfn = pte_pfn(*slot);
    if !refs::is_shared(old_pfn) {
        *slot |= PTE_WRITABLE;
        fl.upgrade(va);
        return Some(());
    }
    let new_pfn = alloc_table_frame()?;
    let old: *mut u64 = frame_ptr(old_pfn);
    let new: *mut u64 = frame_ptr(new_pfn);
    for i in 0..ENTRIES {
        let mut e = *old.add(i);
        if e & PTE_PRESENT != 0 {
            refs::get(pte_pfn(e));
            if e & PTE_SHARED_MAP == 0 && e & PTE_WRITABLE != 0 {
                // Page privée : copie à l'écriture des deux côtés. Les autres propriétaires de
                // l'ancienne table la voient déjà en lecture seule (leur P2 est sans W).
                e &= !PTE_WRITABLE;
                *old.add(i) = e;
            }
        }
        *new.add(i) = e;
    }
    *slot = (new_pfn * PAGE_SIZE) as u64 | TABLE_FLAGS;
    fl.page(va & !(P1_SPAN - 1));
    fl.put_table(old_pfn);
    Some(())
}

/// Pointeur sur l'entrée feuille de `va`, en créant les tables manquantes et en rendant privée
/// une table P1 partagée (l'appelant va la modifier).
///
/// # Safety
/// `p4_pfn` est le P4 d'un espace dont l'appelant tient le verrou.
pub unsafe fn walk_create(p4_pfn: usize, va: usize, fl: &mut Flush) -> Option<*mut u64> {
    let mut table: *mut u64 = frame_ptr(p4_pfn);
    for level in (2..=4).rev() {
        let slot = table.add(idx(va, level));
        if *slot & PTE_PRESENT == 0 {
            let pfn = alloc_table_frame()?;
            *slot = (pfn * PAGE_SIZE) as u64 | TABLE_FLAGS;
        } else if level == 2 && *slot & PTE_WRITABLE == 0 {
            unshare_p1(slot, va, fl)?;
        }
        table = frame_ptr(pte_pfn(*slot));
    }
    Some(table.add(idx(va, 1)))
}

/// Ce que [`for_each_present_mut`] fait d'une table P1 entièrement couverte par la plage.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WholeTable {
    /// La parcourir comme les autres.
    Visit,
    /// Retrait (`munmap`) : une table partagée perd simplement une référence (ses pages ne
    /// sont pas touchées), une table privée est vidée par `f` puis libérée.
    Release,
}

/// Appelle `f(fl, va, &mut entrée)` pour chaque feuille présente de `[start, end)`, après avoir
/// rendu privées les tables P1 partagées concernées. `released(n)` est appelé pour chaque table
/// partagée lâchée en bloc (`n` pages présentes). `None` : mémoire insuffisante (le parcours
/// s'arrête).
///
/// # Safety
/// `p4_pfn` est le P4 d'un espace dont l'appelant tient le verrou.
pub unsafe fn for_each_present_mut(
    p4_pfn: usize,
    start: usize,
    end: usize,
    whole: WholeTable,
    fl: &mut Flush,
    mut released: impl FnMut(usize),
    mut f: impl FnMut(&mut Flush, usize, &mut u64),
) -> Option<()> {
    let mut va = start & !(PAGE_SIZE - 1);
    while va < end {
        let mut table: *mut u64 = frame_ptr(p4_pfn);
        let mut skipped = false;
        for level in (2..=4).rev() {
            let slot = table.add(idx(va, level));
            let e = *slot;
            if e & PTE_PRESENT == 0 || e & PTE_HUGE != 0 {
                let span = 1usize << (12 + 9 * (level - 1));
                va = (va & !(span - 1)) + span;
                skipped = true;
                break;
            }
            if level == 2 {
                let base = va & !(P1_SPAN - 1);
                let covers = whole == WholeTable::Release && va == base && base + P1_SPAN <= end;
                if covers {
                    let p1_pfn = pte_pfn(e);
                    if e & PTE_WRITABLE == 0 && refs::is_shared(p1_pfn) {
                        // Table partagée : ses pages restent à ses autres propriétaires.
                        let p1: *const u64 = frame_ptr(p1_pfn);
                        released((0..ENTRIES).filter(|&i| *p1.add(i) & PTE_PRESENT != 0).count());
                    } else {
                        let p1: *mut u64 = frame_ptr(p1_pfn);
                        for i in 0..ENTRIES {
                            let pte = &mut *p1.add(i);
                            if *pte & PTE_PRESENT != 0 {
                                f(fl, base + i * PAGE_SIZE, pte);
                            }
                        }
                    }
                    *slot = 0;
                    fl.page(base);
                    fl.put_table(p1_pfn);
                    va = base + P1_SPAN;
                    skipped = true;
                    break;
                }
                if e & PTE_WRITABLE == 0 {
                    unshare_p1(slot, va, fl)?;
                }
            }
            table = frame_ptr(pte_pfn(*slot));
        }
        if skipped { continue; }
        let pte = &mut *table.add(idx(va, 1));
        if *pte & PTE_PRESENT != 0 {
            f(fl, va, pte);
        }
        va += PAGE_SIZE;
    }
    Some(())
}

/// `fork` : construit dans `dst` (vide) des tables P4/P3/P2 neuves qui pointent sur les **mêmes**
/// tables P1 que `src`, en lecture seule des deux côtés. Renvoie le nombre de tables partagées.
/// `None` : mémoire insuffisante (`dst` reste cohérent et se libère normalement).
///
/// # Safety
/// L'appelant tient le verrou de `src` ; `dst` n'est vu par personne d'autre.
pub unsafe fn fork_tables(src_p4: usize, dst_p4: usize) -> Option<usize> {
    let mut shared = 0;
    let s4: *mut u64 = frame_ptr(src_p4);
    let d4: *mut u64 = frame_ptr(dst_p4);
    for i in 0..256 {
        let e4 = *s4.add(i);
        if e4 & PTE_PRESENT == 0 { continue; }
        let d3_pfn = alloc_table_frame()?;
        *d4.add(i) = (d3_pfn * PAGE_SIZE) as u64 | TABLE_FLAGS;
        let s3: *mut u64 = frame_ptr(pte_pfn(e4));
        let d3: *mut u64 = frame_ptr(d3_pfn);
        for j in 0..ENTRIES {
            let e3 = *s3.add(j);
            if e3 & PTE_PRESENT == 0 || e3 & PTE_HUGE != 0 { continue; }
            let d2_pfn = alloc_table_frame()?;
            *d3.add(j) = (d2_pfn * PAGE_SIZE) as u64 | TABLE_FLAGS;
            let s2: *mut u64 = frame_ptr(pte_pfn(e3));
            let d2: *mut u64 = frame_ptr(d2_pfn);
            for k in 0..ENTRIES {
                let e2 = *s2.add(k);
                if e2 & PTE_PRESENT == 0 || e2 & PTE_HUGE != 0 { continue; }
                refs::get(pte_pfn(e2));
                let ro = e2 & !PTE_WRITABLE;
                *s2.add(k) = ro;
                *d2.add(k) = ro;
                shared += 1;
            }
        }
    }
    Some(shared)
}

/// Destruction d'un espace : chaque table P1 perd la référence de cet espace (ses pages avec,
/// si c'était la dernière) et son entrée P2 est effacée. Les P2/P3/P4 restent à libérer par
/// l'`AddressSpace`.
///
/// # Safety
/// Plus personne n'utilise les mappages utilisateur de cet espace.
pub unsafe fn release_all(p4_pfn: usize) {
    let p4: *mut u64 = frame_ptr(p4_pfn);
    for i in 0..256 {
        let e4 = *p4.add(i);
        if e4 & PTE_PRESENT == 0 { continue; }
        let p3: *mut u64 = frame_ptr(pte_pfn(e4));
        for j in 0..ENTRIES {
            let e3 = *p3.add(j);
            if e3 & PTE_PRESENT == 0 || e3 & PTE_HUGE != 0 { continue; }
            let p2: *mut u64 = frame_ptr(pte_pfn(e3));
            for k in 0..ENTRIES {
                let e2 = *p2.add(k);
                if e2 & PTE_PRESENT == 0 || e2 & PTE_HUGE != 0 { continue; }
                *p2.add(k) = 0;
                refs::put_table(pte_pfn(e2));
            }
        }
    }
}
