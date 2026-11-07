//! MaiProc : moitié haute partagée par tous les espaces d'adresses, et *physmap*.
//!
//! Avant de créer le premier processus, [`seal`] :
//! 1. pré-alloue les tables P3 de toutes les entrées P4 du noyau qui peuvent servir
//!    (allocations dynamiques, physmap, tas, image) : à partir de là, les entrées P4 de la moitié
//!    haute ne changent plus, et toute modification du noyau aux niveaux inférieurs est visible
//!    depuis chaque espace d'adresses, qui n'en copie que les entrées P4 ;
//! 2. projette toute la RAM générale à [`PHYSMAP_START`] en pages de 2 Mio (lecture/écriture,
//!    non exécutable, globale) : le noyau peut alors lire ou écrire n'importe quelle frame
//!    (tables de pages des processus, pages à zéroer ou à copier) sans mappage temporaire.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use kernel_config::memory::{
    ENTRIES_PER_PAGE_TABLE, KERNEL_DYNAMIC_P4_COUNT, KERNEL_DYNAMIC_P4_START,
    KERNEL_HALF_FIRST_P4_INDEX, KERNEL_HEAP_P4_INDEX, KERNEL_TEXT_P4_INDEX, PAGE_SIZE,
    PHYSMAP_MAX_BYTES, PHYSMAP_P4_COUNT, PHYSMAP_P4_INDEX, PHYSMAP_START, RECURSIVE_P4_INDEX,
    UPCOMING_PAGE_TABLE_RECURSIVE_P4_INDEX,
};
use pte_flags::PteFlagsArch;
use super::Mapper;

const HALF: usize = ENTRIES_PER_PAGE_TABLE / 2;
const TWO_MIB: usize = 2 * 1024 * 1024;

/// Bits matériels d'une entrée x86_64.
const PTE_PRESENT: u64 = 1 << 0;
const PTE_WRITABLE: u64 = 1 << 1;
const PTE_HUGE: u64 = 1 << 7;
const PTE_GLOBAL: u64 = 1 << 8;
const PTE_NX: u64 = 1 << 63;

static SEALED: AtomicBool = AtomicBool::new(false);
/// Fin (exclue) de la RAM projetée par la physmap, en octets physiques.
static PHYSMAP_END: AtomicUsize = AtomicUsize::new(0);
/// Frame du P4 du noyau (pour les tâches noyau et le diagnostic).
static KERNEL_P4: AtomicUsize = AtomicUsize::new(0);
/// Copie des entrées P4 256..512 du noyau, écrite une fois avant `SEALED`.
static mut KERNEL_P4_TEMPLATE: [u64; HALF] = [0; HALF];

/// Vrai une fois la moitié haute scellée et la physmap prête.
#[inline]
pub fn is_sealed() -> bool {
    SEALED.load(Ordering::Acquire)
}

/// Adresse virtuelle (physmap) de l'adresse physique `paddr`.
#[inline(always)]
pub fn phys_to_virt(paddr: usize) -> usize {
    debug_assert!(paddr < PHYSMAP_END.load(Ordering::Relaxed), "physmap : {paddr:#x} hors RAM projetée");
    PHYSMAP_START + paddr
}

/// Pointeur (physmap) sur la frame `pfn`.
#[inline(always)]
pub fn frame_ptr<T>(pfn: usize) -> *mut T {
    phys_to_virt(pfn * PAGE_SIZE) as *mut T
}

/// Fin (exclue) de la RAM projetée.
pub fn physmap_end() -> usize {
    PHYSMAP_END.load(Ordering::Relaxed)
}

/// Frame physique du P4 du noyau.
pub fn kernel_p4_frame() -> usize {
    KERNEL_P4.load(Ordering::Relaxed)
}

/// Entrée `index` (≥ 256) du P4 modèle du noyau.
#[inline]
pub(crate) fn kernel_p4_entry(index: usize) -> u64 {
    debug_assert!(index >= HALF && is_sealed());
    unsafe { KERNEL_P4_TEMPLATE[index - HALF] }
}

/// Entrées P4 de la moitié haute qui doivent être partagées par tous les espaces.
fn shared_p4_indices() -> impl Iterator<Item = usize> {
    let dynamic = KERNEL_DYNAMIC_P4_START..KERNEL_DYNAMIC_P4_START + KERNEL_DYNAMIC_P4_COUNT;
    let physmap = PHYSMAP_P4_INDEX..PHYSMAP_P4_INDEX + PHYSMAP_P4_COUNT;
    dynamic.chain(physmap).chain([KERNEL_HEAP_P4_INDEX, KERNEL_TEXT_P4_INDEX])
}

/// Scelle la moitié haute et construit la physmap. `ram` donne les plages de RAM générale en
/// numéros de frame `[début, fin)`. À appeler une fois, sur le BSP, avec le P4 du noyau actif.
pub fn seal(ram: &[(usize, usize)]) -> Result<(), &'static str> {
    if is_sealed() {
        return Ok(());
    }
    let table_flags = PteFlagsArch::new().valid(true).writable(true).executable(true);
    let mut mapper = Mapper::from_current();
    KERNEL_P4.store(mapper.target_p4.number(), Ordering::Relaxed);

    // 1. Tables P3 de toutes les entrées partagées.
    let mut created = 0;
    for index in shared_p4_indices() {
        if mapper.p4()[index].is_unused() {
            created += 1;
        }
        let _ = mapper.p4_mut().next_table_create(index, table_flags);
    }

    // 2. Physmap : pages de 2 Mio couvrant chaque plage de RAM (arrondie vers l'extérieur).
    let mut end_bytes = 0usize;
    let mut huge_pages = 0usize;
    for &(start_pfn, end_pfn) in ram {
        let start = (start_pfn * PAGE_SIZE) & !(TWO_MIB - 1);
        let end = ((end_pfn * PAGE_SIZE) + TWO_MIB - 1) & !(TWO_MIB - 1);
        if end > PHYSMAP_MAX_BYTES {
            log::error!("physmap : RAM au-delà de {} Gio ignorée", PHYSMAP_MAX_BYTES >> 30);
        }
        let mut pa = start;
        while pa < end.min(PHYSMAP_MAX_BYTES) {
            let va = PHYSMAP_START + pa;
            let p4i = (va >> 39) & 0x1FF;
            let p3i = (va >> 30) & 0x1FF;
            let p2i = (va >> 21) & 0x1FF;
            let p3 = mapper.p4_mut().next_table_create(p4i, table_flags);
            let p2 = p3.next_table_create(p3i, table_flags);
            let entry = &mut p2[p2i] as *mut _ as *mut u64;
            // SAFETY : entrée P2 de la physmap, réservée à cet usage.
            unsafe {
                if *entry & PTE_PRESENT == 0 {
                    *entry = pa as u64 | PTE_PRESENT | PTE_WRITABLE | PTE_HUGE | PTE_GLOBAL | PTE_NX;
                    huge_pages += 1;
                }
            }
            pa += TWO_MIB;
        }
        end_bytes = end_bytes.max(end.min(PHYSMAP_MAX_BYTES));
    }
    PHYSMAP_END.store(end_bytes, Ordering::Relaxed);

    // 3. Modèle des entrées P4 partagées.
    let mut low_present = 0;
    for i in 0..ENTRIES_PER_PAGE_TABLE {
        let raw = mapper.p4()[i].value();
        if i < KERNEL_HALF_FIRST_P4_INDEX {
            if raw & PTE_PRESENT != 0 {
                low_present += 1;
            }
            continue;
        }
        let keep = i != RECURSIVE_P4_INDEX && i != UPCOMING_PAGE_TABLE_RECURSIVE_P4_INDEX;
        // SAFETY : écrit une seule fois, avant la publication par `SEALED`.
        unsafe { KERNEL_P4_TEMPLATE[i - HALF] = if keep { raw } else { 0 }; }
    }
    SEALED.store(true, Ordering::Release);
    log::warn!(
        "MaiProc : moitie haute scellee ({} tables P3 creees), physmap {} Mio en {} pages de 2 Mio, \
         {} entrees P4 basses encore utilisees par le noyau",
        created, end_bytes >> 20, huge_pages, low_present,
    );
    Ok(())
}
