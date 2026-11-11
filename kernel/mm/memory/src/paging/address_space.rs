//! MaiProc : espace d'adresses d'un processus (racine P4) et gestion des PCID.
//!
//! Un [`AddressSpace`] possède son P4 : la moitié basse (entrées 0..256) appartient au processus,
//! la moitié haute est copiée du modèle scellé du noyau ([`super::kernel_half`]), sauf l'entrée
//! récursive (510), qui pointe sur ce P4, et l'entrée « à venir » (508), laissée vide.
//!
//! ## PCID
//! Schéma de Linux (A. Lutomirski, 4.14) : chaque cœur a [`NR_DYN_ASIDS`] PCID dynamiques
//! (1..=6 ; le PCID 0 est celui du noyau au démarrage). Un emplacement retient l'identifiant de
//! l'espace qu'il sert et la génération TLB vue au dernier chargement. Toute modification des
//! mappages utilisateur d'un espace incrémente sa génération ([`AddressSpace::bump_tlb_gen`]) :
//! un cœur qui recharge l'espace avec une génération en retard vide ce seul PCID ; sinon CR3 est
//! écrit avec le bit 63 (aucun vidage). Les pages du noyau sont globales et ne sont jamais vidées.
//!
//! Les tâches noyau laissent l'espace courant chargé (TLB « paresseuse ») : elles n'utilisent que
//! la moitié haute, identique partout. La référence gardée par le cœur ([`CpuAsids::loaded`])
//! empêche de libérer les tables d'un espace encore chargé.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use kernel_config::memory::{
    ENTRIES_PER_PAGE_TABLE, KERNEL_HALF_FIRST_P4_INDEX, PAGE_SIZE, RECURSIVE_P4_INDEX,
};
use super::kernel_half::{self, frame_ptr, kernel_p4_entry};

/// Nombre de PCID dynamiques par cœur.
pub const NR_DYN_ASIDS: usize = 6;
/// Nombre maximal de cœurs (indexés par identifiant APIC).
const MAX_CPUS: usize = 512;

const PTE_PRESENT: u64 = 1 << 0;
const PTE_WRITABLE: u64 = 1 << 1;
const PTE_HUGE: u64 = 1 << 7;
const PTE_ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
const CR3_NOFLUSH: u64 = 1 << 63;

static NEXT_SPACE_ID: AtomicU64 = AtomicU64::new(1);
static PCID_ENABLED: AtomicBool = AtomicBool::new(false);

/// Espace d'adresses d'un processus.
pub struct AddressSpace {
    /// Identifiant unique, jamais réutilisé (sert d'étiquette dans les emplacements PCID).
    id: u64,
    /// Frame du P4.
    p4_pfn: usize,
    /// Génération TLB : incrémentée à chaque retrait ou réduction de droits d'un mappage utilisateur.
    tlb_gen: AtomicU64,
    /// Cœurs qui exécutent en ce moment un fil de cet espace (cible des invalidations par IPI).
    /// Un cœur passé à une tâche noyau garde l'espace chargé (TLB paresseuse) mais sort du
    /// masque : il videra son PCID au prochain fil de l'espace si la génération a avancé.
    active: [AtomicU64; MAX_CPUS / 64],
}

impl AddressSpace {
    /// Crée un espace vide (moitié basse sans mappage). La moitié haute doit être scellée.
    pub fn new() -> Result<Arc<AddressSpace>, &'static str> {
        if !kernel_half::is_sealed() {
            return Err("AddressSpace::new() : moitie haute du noyau non scellee");
        }
        let p4_pfn = alloc_table_frame().ok_or("AddressSpace::new() : plus de memoire")?;
        let p4: *mut u64 = frame_ptr(p4_pfn);
        for i in KERNEL_HALF_FIRST_P4_INDEX..ENTRIES_PER_PAGE_TABLE {
            let value = if i == RECURSIVE_P4_INDEX {
                (p4_pfn * PAGE_SIZE) as u64 | PTE_PRESENT | PTE_WRITABLE
            } else {
                kernel_p4_entry(i)
            };
            // SAFETY : la frame vient d'être allouée et n'est vue que par nous.
            unsafe { p4.add(i).write(value) };
        }
        Ok(Arc::new(AddressSpace {
            id: NEXT_SPACE_ID.fetch_add(1, Ordering::Relaxed),
            p4_pfn,
            tlb_gen: AtomicU64::new(1),
            active: [NO_CPU; MAX_CPUS / 64],
        }))
    }

    /// Identifiant unique de l'espace.
    pub fn id(&self) -> u64 { self.id }

    /// Frame physique du P4.
    pub fn p4_pfn(&self) -> usize { self.p4_pfn }

    /// Pointeur (physmap) sur les 512 entrées du P4.
    pub fn p4_table(&self) -> *mut u64 { frame_ptr(self.p4_pfn) }

    /// Génération TLB courante.
    pub fn tlb_gen(&self) -> u64 { self.tlb_gen.load(Ordering::Acquire) }

    /// Signale qu'une entrée de la moitié basse a été retirée ou restreinte : les cœurs qui ont cet
    /// espace en cache (autre que le cœur courant, qui doit invalider lui-même avec `invlpg`)
    /// videront leur PCID au prochain chargement.
    pub fn bump_tlb_gen(&self) -> u64 {
        self.tlb_gen.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Appelle `f(cœur)` pour chaque cœur (autre que `except`) qui exécute un fil de cet espace.
    pub fn for_each_active_cpu(&self, except: usize, mut f: impl FnMut(usize)) {
        for (w, word) in self.active.iter().enumerate() {
            let mut bits = word.load(Ordering::SeqCst);
            while bits != 0 {
                let b = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let cpu = w * 64 + b;
                if cpu != except {
                    f(cpu);
                }
            }
        }
    }

    fn set_active(&self, cpu: usize, on: bool) {
        let (w, bit) = (cpu / 64, 1u64 << (cpu % 64));
        if on {
            self.active[w].fetch_or(bit, Ordering::SeqCst);
        } else {
            self.active[w].fetch_and(!bit, Ordering::SeqCst);
        }
    }

    /// Invalide, sur le cœur courant et si cet espace y est chargé, les pages `pages` (ou tout
    /// le PCID si `full`).
    pub fn flush_local(&self, pages: &[usize], full: bool) {
        flush_loaded(self.id, pages, full);
    }

    /// Vrai si cet espace est celui chargé sur le cœur courant.
    pub fn is_current(&self) -> bool {
        let cpu = cpu_slot();
        // SAFETY : état propre à ce cœur, lu interruptions éventuellement actives (lecture d'un u64).
        unsafe { CPU_ASIDS[cpu].loaded_id == self.id }
    }
}

impl Drop for AddressSpace {
    /// Libère les tables de pages de la moitié basse. Les pages feuilles doivent déjà avoir été
    /// rendues par le propriétaire (le gestionnaire de mémoire du processus) ; celles qui restent
    /// sont signalées et perdues plutôt que libérées à l'aveugle.
    fn drop(&mut self) {
        let mut leaked = 0usize;
        unsafe {
            let p4: *mut u64 = frame_ptr(self.p4_pfn);
            for i in 0..KERNEL_HALF_FIRST_P4_INDEX {
                let e4 = *p4.add(i);
                if e4 & PTE_PRESENT == 0 { continue; }
                let p3_pfn = ((e4 & PTE_ADDR_MASK) as usize) / PAGE_SIZE;
                let p3: *mut u64 = frame_ptr(p3_pfn);
                for j in 0..ENTRIES_PER_PAGE_TABLE {
                    let e3 = *p3.add(j);
                    if e3 & PTE_PRESENT == 0 { continue; }
                    if e3 & PTE_HUGE != 0 { leaked += 1; continue; }
                    let p2_pfn = ((e3 & PTE_ADDR_MASK) as usize) / PAGE_SIZE;
                    let p2: *mut u64 = frame_ptr(p2_pfn);
                    for k in 0..ENTRIES_PER_PAGE_TABLE {
                        let e2 = *p2.add(k);
                        if e2 & PTE_PRESENT == 0 { continue; }
                        if e2 & PTE_HUGE != 0 { leaked += 1; continue; }
                        let p1_pfn = ((e2 & PTE_ADDR_MASK) as usize) / PAGE_SIZE;
                        let p1: *const u64 = frame_ptr(p1_pfn);
                        leaked += (0..ENTRIES_PER_PAGE_TABLE).filter(|&l| *p1.add(l) & PTE_PRESENT != 0).count();
                        frame_allocator::free_raw_frames(p1_pfn, 1);
                    }
                    frame_allocator::free_raw_frames(p2_pfn, 1);
                }
                frame_allocator::free_raw_frames(p3_pfn, 1);
            }
            frame_allocator::free_raw_frames(self.p4_pfn, 1);
        }
        if leaked != 0 {
            log::error!("AddressSpace {} : {} pages encore mappees a la destruction (perdues)", self.id, leaked);
        }
    }
}

/// Alloue une frame de table de pages mise à zéro.
pub fn alloc_table_frame() -> Option<usize> {
    let (pfn, zeroed) = frame_allocator::alloc_raw_frame_prefer_zeroed()?;
    if !zeroed {
        // SAFETY : frame neuve, accessible par la physmap.
        unsafe { core::ptr::write_bytes(frame_ptr::<u8>(pfn), 0, PAGE_SIZE) };
    }
    Some(pfn)
}

// ---------------------------------------------------------------------------------------------
// PCID par cœur
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct AsidSlot {
    space_id: u64,
    tlb_gen: u64,
}

struct CpuAsids {
    slots: [AsidSlot; NR_DYN_ASIDS],
    next_victim: usize,
    /// Espace chargé dans CR3 (0 : P4 du noyau).
    loaded_id: u64,
    /// Référence qui garde l'espace chargé en vie.
    loaded: Option<Arc<AddressSpace>>,
}

#[allow(clippy::declare_interior_mutable_const)]
const NO_CPU: AtomicU64 = AtomicU64::new(0);

const EMPTY_CPU: CpuAsids = CpuAsids {
    slots: [AsidSlot { space_id: 0, tlb_gen: 0 }; NR_DYN_ASIDS],
    next_victim: 0,
    loaded_id: 0,
    loaded: None,
};

/// État PCID de chaque cœur, indexé par identifiant APIC ; seul le cœur concerné y touche,
/// interruptions coupées.
static mut CPU_ASIDS: [CpuAsids; MAX_CPUS] = [EMPTY_CPU; MAX_CPUS];

/// Identifiant du cœur courant (TSC_AUX = identifiant APIC, posé à l'initialisation du cœur).
#[inline(always)]
pub fn cpu_slot() -> usize {
    let mut aux = 0u32;
    // SAFETY : lecture de TSC/TSC_AUX, sans effet de bord.
    unsafe { core::arch::x86_64::__rdtscp(&mut aux) };
    aux as usize % MAX_CPUS
}

/// Active PCID sur le cœur courant si le processeur le gère (CPUID.1:ECX[17]). À appeler une
/// fois par cœur, P4 du noyau chargé avec le PCID 0. Renvoie `true` si PCID est actif.
pub fn enable_pcid_on_this_cpu() -> bool {
    // SAFETY : CPUID est toujours disponible en mode long.
    let has_pcid = unsafe { core::arch::x86_64::__cpuid(1).ecx & (1 << 17) != 0 };
    if !has_pcid {
        return false;
    }
    unsafe {
        let cr3: u64;
        core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags));
        if cr3 & 0xFFF != 0 {
            // CR4.PCIDE exige un PCID nul au moment où on l'active.
            core::arch::asm!("mov cr3, {}", in(reg) cr3 & !0xFFF, options(nostack, preserves_flags));
        }
        let mut cr4: u64;
        core::arch::asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
        cr4 |= 1 << 17;
        core::arch::asm!("mov cr4, {}", in(reg) cr4, options(nostack, preserves_flags));
    }
    PCID_ENABLED.store(true, Ordering::Relaxed);
    true
}

/// Vrai si PCID est actif.
pub fn pcid_enabled() -> bool {
    PCID_ENABLED.load(Ordering::Relaxed)
}

/// Charge `space` sur le cœur courant (interruptions coupées obligatoires).
///
/// Ne fait rien si l'espace est déjà chargé et à jour. Gère la réutilisation des PCID et le
/// vidage sélectif selon la génération TLB.
pub fn switch_to(space: &Arc<AddressSpace>) {
    let cpu = cpu_slot();
    // SAFETY : état propre à ce cœur ; l'appelant a coupé les interruptions.
    let state = unsafe { &mut CPU_ASIDS[cpu] };
    if state.loaded_id != space.id {
        if let Some(old) = &state.loaded {
            old.set_active(cpu, false);
        }
    }
    // Entrée dans le masque **avant** de lire la génération : un cœur qui modifie l'espace
    // incrémente la génération puis lit le masque (ordre total SeqCst). Soit il voit ce cœur et
    // lui envoie une IPI, soit ce cœur voit la nouvelle génération et vide son PCID.
    space.set_active(cpu, true);
    let gen = space.tlb_gen();
    let p4 = (space.p4_pfn * PAGE_SIZE) as u64;

    if !pcid_enabled() {
        if state.loaded_id != space.id {
            state.loaded_id = space.id;
            state.loaded = Some(space.clone());
            write_cr3(p4);
        } else if state.slots[0].tlb_gen < gen {
            write_cr3(p4);
        }
        state.slots[0] = AsidSlot { space_id: space.id, tlb_gen: gen };
        return;
    }

    // Emplacement déjà attribué à cet espace ?
    let found = state.slots.iter().position(|s| s.space_id == space.id);
    let (slot, need_flush) = match found {
        Some(i) => (i, state.slots[i].tlb_gen < gen),
        None => {
            let i = state.next_victim;
            state.next_victim = (i + 1) % NR_DYN_ASIDS;
            (i, true)
        }
    };
    if state.loaded_id == space.id && !need_flush {
        return;
    }
    state.slots[slot] = AsidSlot { space_id: space.id, tlb_gen: gen };
    let pcid = (slot + 1) as u64;
    let value = p4 | pcid | if need_flush { 0 } else { CR3_NOFLUSH };
    if state.loaded_id != space.id {
        state.loaded_id = space.id;
        state.loaded = Some(space.clone());
    }
    write_cr3(value);
}

/// Le cœur passe à une tâche noyau : l'espace chargé le reste (TLB paresseuse), mais ce cœur
/// n'exécute plus de fil de cet espace et sort de son masque d'invalidation (interruptions
/// coupées).
pub fn enter_kernel_task() {
    let cpu = cpu_slot();
    // SAFETY : état propre à ce cœur ; l'appelant a coupé les interruptions.
    let state = unsafe { &mut CPU_ASIDS[cpu] };
    if let Some(s) = &state.loaded {
        s.set_active(cpu, false);
    }
}

/// Invalidation demandée par un autre cœur (IPI) ou locale : sans effet si l'espace `space_id`
/// n'est pas chargé ici.
pub fn flush_loaded(space_id: u64, pages: &[usize], full: bool) {
    let cpu = cpu_slot();
    // SAFETY : état propre à ce cœur ; lecture d'un identifiant (u64 aligné).
    let state = unsafe { &mut CPU_ASIDS[cpu] };
    if state.loaded_id != space_id || space_id == 0 {
        return;
    }
    if full {
        // Génération lue avant le vidage : tout ce qui a été modifié jusque-là est couvert.
        let gen = state.loaded.as_ref().map_or(0, |s| s.tlb_gen());
        let cr3: u64;
        // SAFETY : relecture de CR3 (P4 et PCID courants).
        unsafe { core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags)) };
        // Écrire CR3 sans le bit 63 vide les entrées non globales du PCID courant.
        write_cr3(cr3 & !CR3_NOFLUSH);
        if let Some(s) = state.slots.iter_mut().find(|s| s.space_id == space_id) {
            s.tlb_gen = s.tlb_gen.max(gen);
        }
    } else {
        for &va in pages {
            flush_user_page_local(va);
        }
    }
}

/// Invalide `vaddr` (moitié basse) dans l'espace courant du cœur courant.
#[inline]
pub fn flush_user_page_local(vaddr: usize) {
    // SAFETY : `invlpg` n'a pas d'autre effet que d'invalider une entrée de TLB.
    unsafe { core::arch::asm!("invlpg [{}]", in(reg) vaddr, options(nostack, preserves_flags)) };
}

#[inline(always)]
fn write_cr3(value: u64) {
    // SAFETY : `value` désigne un P4 valide dont la moitié haute est celle du noyau.
    unsafe { core::arch::asm!("mov cr3, {}", in(reg) value, options(nostack, preserves_flags)) };
}
