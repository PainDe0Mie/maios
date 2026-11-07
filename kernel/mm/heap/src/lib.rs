//! Tas global du noyau MaiOS.
//!
//! * Au démarrage, avant que la mémoire virtuelle ne soit prête, un tas d'amorçage
//!   (`block_allocator`) sert les allocations dans la zone `KERNEL_HEAP_START..+INITIAL_SIZE`.
//! * Ensuite, [`enable_kalloc`] bascule sur **kalloc** (MaiMM M2, crate `kalloc`) : tas par cœur
//!   sans verrou, segments de 2 Mio, libérations distantes sans verrou, mémoire rendue au système.
//!
//! Cette crate est la colle entre kalloc (cœur pur, sans dépendance) et le noyau :
//! * elle coupe les interruptions autour de chaque appel et fournit l'identifiant du cœur ;
//! * elle mappe les segments (frames contiguës de LLFree) dans une arène virtuelle de 32 Gio
//!   réservée une fois pour toutes, et les démappe quand kalloc les rend ;
//! * elle sert directement par mappage les objets de plus de 1 Mio (ou alignés à plus de 4 Kio) ;
//!   de 128 Kio à 1 Mio, kalloc les sert en pages larges dans ses segments.
//!
//! Les opérations qui touchent aux tables de pages (rendre un segment) ne sont jamais faites dans
//! le chemin de libération (qui peut tourner sous un verrou de table de pages, ou dans une
//! interruption) : elles sont différées à la tâche inactive ([`idle_maintenance`]), qui garde
//! aussi une petite réserve de segments mappés d'avance.

#![no_std]

extern crate alloc;

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::*};

use block_allocator::FixedSizeBlockAllocator;
use irq_safety::hold_interrupts;
use kalloc::{Alloc, Free, Kalloc, SegmentHandle, MAX_CPUS, OS_WORDS, SEGMENT_SIZE};
use kernel_config::memory::{KERNEL_HEAP_INITIAL_SIZE, KERNEL_HEAP_MAX_SIZE, KERNEL_HEAP_START, PAGE_SIZE};
use memory::{AllocatedPages, MappedPages, PteFlags, VirtualAddress};
use sync_irq::IrqSafeMutex;

#[global_allocator]
pub static GLOBAL_ALLOCATOR: Heap = Heap::empty();

/// Les pages du tas sont inscriptibles et non exécutables.
pub const HEAP_FLAGS: PteFlags = PteFlags::from_bits_truncate(
    PteFlags::new().bits()
    | PteFlags::VALID.bits()
    | PteFlags::WRITABLE.bits()
);

/// Fin du tas d'amorçage : sert à router les libérations de ses objets.
const INITIAL_HEAP_END_ADDR: usize = KERNEL_HEAP_START + KERNEL_HEAP_INITIAL_SIZE;

/// Arène virtuelle des segments : la moitié haute de l'entrée P4 du tas, réservée d'un bloc à
/// l'activation (comme la zone `vmalloc` de Linux ou les arènes de mimalloc). Loin du tas
/// d'amorçage et des replis de l'allocateur de pages, qui posent piles et gros mappages dans les
/// régions désignées quand la région générale manque de place.
const ARENA_START: usize = KERNEL_HEAP_START + KERNEL_HEAP_MAX_SIZE / 2;
const ARENA_SIZE: usize = 32 << 30;
const SEGMENT_PAGES: usize = SEGMENT_SIZE / PAGE_SIZE;
const _: () = assert!(ARENA_START % SEGMENT_SIZE == 0);
const _: () = assert!(ARENA_START + ARENA_SIZE <= KERNEL_HEAP_START + KERNEL_HEAP_MAX_SIZE);
const _: () = assert!(core::mem::size_of::<MappedPages>() <= OS_WORDS * core::mem::size_of::<usize>());
const _: () = assert!(core::mem::size_of::<MappedPages>() <= PAGE_SIZE);

/// Segments gardés mappés d'avance dans la réserve (le chemin d'allocation ne mappe presque jamais).
const LOW_WATERMARK: usize = 2;
/// La tâche inactive fait la maintenance d'un cœur une fois sur N passages (~128 ms à 500 Hz).
const MAINTENANCE_PERIOD: u32 = 64;

static KALLOC: Kalloc = Kalloc::new();
static ENABLED: AtomicBool = AtomicBool::new(false);
static TRIM_PENDING: AtomicBool = AtomicBool::new(false);
static SEGMENTS_MAPPED: AtomicUsize = AtomicUsize::new(0);
static LARGE_COUNT: AtomicUsize = AtomicUsize::new(0);
static LARGE_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Reste de l'arène, jamais encore découpé (on prend les segments par le début).
static ARENA: IrqSafeMutex<AllocatedPages> = IrqSafeMutex::new(AllocatedPages::empty());

/// Plages de segments rendus, gardées pour être remappées (sans allocation : pile fixe).
struct FreeRanges {
    slots: [AllocatedPages; 256],
    len: usize,
}

#[allow(clippy::declare_interior_mutable_const)]
const NO_RANGE: AllocatedPages = AllocatedPages::empty();
static FREE_RANGES: IrqSafeMutex<FreeRanges> = IrqSafeMutex::new(FreeRanges { slots: [NO_RANGE; 256], len: 0 });

/// Plage virtuelle de 2 Mio pour un nouveau segment.
fn take_range() -> Option<AllocatedPages> {
    {
        let mut fr = FREE_RANGES.lock();
        if fr.len > 0 {
            fr.len -= 1;
            let i = fr.len;
            return Some(core::mem::replace(&mut fr.slots[i], AllocatedPages::empty()));
        }
    }
    let mut arena = ARENA.lock();
    if arena.size_in_pages() < SEGMENT_PAGES {
        return None;
    }
    let at = *arena.start() + SEGMENT_PAGES;
    let whole = core::mem::replace(&mut *arena, AllocatedPages::empty());
    match whole.split(at) {
        Ok((seg, rest)) => {
            *arena = rest;
            Some(seg)
        }
        Err(whole) => {
            *arena = whole;
            None
        }
    }
}

/// Remet une plage dans la pile ; si elle est pleine, la plage revient à l'allocateur de pages.
fn give_range(pages: AllocatedPages) {
    let mut fr = FREE_RANGES.lock();
    if fr.len < fr.slots.len() {
        let i = fr.len;
        fr.slots[i] = pages;
        fr.len += 1;
    } else {
        drop(fr);
        drop(pages);
    }
}

#[allow(clippy::declare_interior_mutable_const)]
const ZERO_U32: AtomicU32 = AtomicU32::new(0);
static IDLE_PASSES: [AtomicU32; MAX_CPUS] = [ZERO_U32; MAX_CPUS];

/// Initializes the single heap, which is the first heap used by the system.
pub fn init_single_heap(start_virt_addr: usize, size_in_bytes: usize) {
    unsafe { GLOBAL_ALLOCATOR.initial_allocator.lock().init(start_virt_addr, size_in_bytes); }
}

/// Bascule le tas global sur kalloc. À appeler une fois les tables de pages, LLFree, les cœurs
/// secondaires et le TLB shootdown prêts.
pub fn enable_kalloc(nr_cpus: usize) -> Result<(), &'static str> {
    let seed = {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: lecture du TSC.
        let t = unsafe { core::arch::x86_64::_rdtsc() };
        #[cfg(not(target_arch = "x86_64"))]
        let t = 0x2545_F491_4F6C_DD1Du64;
        t ^ (&nr_cpus as *const usize as u64).rotate_left(29)
    };
    KALLOC.init(seed);
    KALLOC.set_pool_cap(nr_cpus.max(2));
    let arena = memory::allocate_pages_by_bytes_at(VirtualAddress::new_canonical(ARENA_START), ARENA_SIZE)
        .map_err(|_| "kalloc : impossible de reserver l'arene virtuelle des segments")?;
    *ARENA.lock() = arena;
    // Un segment par cœur (son tas) plus la réserve : mappés pendant que le tas d'amorçage sert
    // encore les allocations internes du mappage.
    for _ in 0..nr_cpus + LOW_WATERMARK {
        if !commit_segment() {
            return Err("kalloc : impossible de mapper les segments initiaux");
        }
    }
    ENABLED.store(true, Release);
    Ok(())
}

/// Vrai une fois kalloc actif.
pub fn kalloc_enabled() -> bool {
    ENABLED.load(Relaxed)
}

#[inline(always)]
fn cpu_index() -> usize {
    let c = cpu::current_cpu().value() as usize;
    debug_assert!(c < MAX_CPUS);
    c
}

// ---------------------------------------------------------------------------
// Segments
// ---------------------------------------------------------------------------

/// Mappe un segment de 2 Mio et le donne à kalloc.
///
/// La plage vient de l'arène réservée : aucun appel à l'allocateur de pages, donc aucune
/// allocation interne ni collision possible. Les frames sont contiguës (LLFree), ce qui
/// permettra à M4 de promouvoir le segment en page de 2 Mio sans copie.
fn commit_segment() -> bool {
    let Some(pages) = take_range() else {
        log::error!("kalloc : arene virtuelle epuisee");
        return false;
    };
    let va = pages.start_address().value();
    let Some(frames) = memory::allocate_frames(SEGMENT_PAGES) else {
        give_range(pages);
        log::error!("kalloc : plus de frames physiques pour un segment");
        return false;
    };
    let Some(mmi) = memory::get_kernel_mmi_ref() else { return false };
    let mapped = mmi.lock().page_table.map_allocated_pages_to(pages, frames, HEAP_FLAGS);
    let mp = match mapped {
        Ok(mp) => mp,
        Err(e) => {
            log::error!("kalloc : echec du mappage d'un segment : {e}");
            return false;
        }
    };
    let mut os = [0usize; OS_WORDS];
    // SAFETY: `os` est assez grand (assertion plus haut) ; le `MappedPages` y vit jusqu'à ce que
    // kalloc rende le segment (`release_segment`) ; la plage, alignée sur 2 Mio, est à l'usage
    // exclusif de kalloc.
    unsafe {
        ptr::write(os.as_mut_ptr() as *mut MappedPages, mp);
        let _irq = hold_interrupts();
        KALLOC.add_segment(va, os);
    }
    SEGMENTS_MAPPED.fetch_add(1, Relaxed);
    true
}

/// Démappe un segment rendu par kalloc : frames rendues à LLFree, plage gardée pour plus tard.
unsafe fn release_segment(h: SegmentHandle) {
    let mp: MappedPages = ptr::read(h.os.as_ptr() as *const MappedPages);
    let Some(mmi) = memory::get_kernel_mmi_ref() else { return };
    let parts = {
        let mut g = mmi.lock();
        mp.unmap_into_parts(&mut g.page_table)
    };
    match parts {
        Ok((pages, frames)) => {
            drop(frames);
            give_range(pages);
        }
        Err(mp) => drop(mp),
    }
    SEGMENTS_MAPPED.fetch_sub(1, Relaxed);
}

/// Maintenance de la tâche inactive du cœur courant : libérations distantes en attente, pages
/// et segments vides rendus, réserve de segments mappés d'avance. Renvoie `true` si elle a
/// travaillé (la tâche inactive revérifie alors l'ordonnanceur avant de dormir).
pub fn idle_maintenance() -> bool {
    if !ENABLED.load(Relaxed) {
        return false;
    }
    let cpu = cpu_index();
    let pass = IDLE_PASSES[cpu].fetch_add(1, Relaxed);
    let mut work = false;
    if pass % MAINTENANCE_PERIOD == 0 {
        // SAFETY: interruptions coupées sur ce cœur.
        let trim = unsafe {
            let _irq = hold_interrupts();
            KALLOC.collect(cpu)
        };
        if trim {
            TRIM_PENDING.store(true, Relaxed);
        }
    }
    if TRIM_PENDING.swap(false, AcqRel) {
        loop {
            // SAFETY: le segment rendu n'est plus accédé par kalloc.
            let h = unsafe {
                let _irq = hold_interrupts();
                KALLOC.trim()
            };
            match h {
                Some(h) => unsafe { release_segment(h) },
                None => break,
            }
            work = true;
        }
    }
    // Le verrou de la réserve est aussi pris par les allocations en interruption : toujours
    // interruptions coupées autour des appels qui le prennent.
    let pool_len = || {
        let _irq = hold_interrupts();
        KALLOC.pool_len()
    };
    while pool_len() < LOW_WATERMARK {
        if !commit_segment() {
            break;
        }
        work = true;
    }
    work
}

// ---------------------------------------------------------------------------
// Gros objets : mappage direct, `MappedPages` rangé dans la page qui précède l'objet
// ---------------------------------------------------------------------------

unsafe fn large_alloc(layout: Layout) -> *mut u8 {
    let align = layout.align().max(PAGE_SIZE);
    let total = layout.size() + PAGE_SIZE + (align - PAGE_SIZE);
    let mp = match memory::create_mapping(total, HEAP_FLAGS) {
        Ok(mp) => mp,
        Err(_) => return ptr::null_mut(),
    };
    let base = mp.start_address().value();
    let block = (base + PAGE_SIZE + align - 1) & !(align - 1);
    LARGE_BYTES.fetch_add(mp.size_in_bytes(), Relaxed);
    LARGE_COUNT.fetch_add(1, Relaxed);
    ptr::write((block - PAGE_SIZE) as *mut MappedPages, mp);
    block as *mut u8
}

unsafe fn large_capacity(p: *mut u8) -> usize {
    let mp = &*((p as usize - PAGE_SIZE) as *const MappedPages);
    mp.start_address().value() + mp.size_in_bytes() - p as usize
}

unsafe fn large_free(p: *mut u8) {
    let mp: MappedPages = ptr::read((p as usize - PAGE_SIZE) as *const MappedPages);
    LARGE_BYTES.fetch_sub(mp.size_in_bytes(), Relaxed);
    LARGE_COUNT.fetch_sub(1, Relaxed);
    drop(mp);
}

// ---------------------------------------------------------------------------
// Allocateur global
// ---------------------------------------------------------------------------

#[inline(always)]
unsafe fn kalloc_alloc(layout: Layout) -> *mut u8 {
    loop {
        let r = {
            let _irq = hold_interrupts();
            KALLOC.alloc(cpu_index(), layout.size(), layout.align())
        };
        match r {
            Alloc::Ok(p) => return p,
            Alloc::Large => return large_alloc(layout),
            Alloc::NeedSegment => {
                if !commit_segment() {
                    return ptr::null_mut();
                }
            }
        }
    }
}

#[inline(always)]
unsafe fn kalloc_free(p: *mut u8, layout: Layout) {
    let r = {
        let _irq = hold_interrupts();
        KALLOC.free(cpu_index(), p, layout.size(), layout.align())
    };
    match r {
        Free::Done => {}
        Free::Trim => TRIM_PENDING.store(true, Relaxed),
        Free::Large => large_free(p),
    }
}

#[inline(always)]
fn in_initial_heap(p: *mut u8) -> bool {
    let a = p as usize;
    (KERNEL_HEAP_START..INITIAL_HEAP_END_ADDR).contains(&a)
}

/// Le tas global : tas d'amorçage, puis kalloc.
pub struct Heap {
    initial_allocator: IrqSafeMutex<FixedSizeBlockAllocator>,
}

impl Heap {
    /// Returns a heap in which only an empty initial allocator has been created.
    pub const fn empty() -> Heap {
        Heap { initial_allocator: IrqSafeMutex::new(FixedSizeBlockAllocator::new()) }
    }
}

unsafe impl GlobalAlloc for Heap {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ENABLED.load(Relaxed) {
            kalloc_alloc(layout)
        } else {
            self.initial_allocator.lock().allocate(layout)
        }
    }

    #[inline]
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        if in_initial_heap(p) {
            self.initial_allocator.lock().deallocate(p, layout);
        } else {
            kalloc_free(p, layout);
        }
    }

    /// Redimensionnement sur place quand le bloc suffit et n'est pas plus de deux fois trop grand
    /// (règle de mimalloc) : un `Vec` qui grandit par petits pas ne copie plus à chaque fois.
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ENABLED.load(Relaxed) && !in_initial_heap(p) {
            let align = layout.align();
            let old_small = kalloc::in_segments(layout.size(), align);
            let new_small = kalloc::in_segments(new_size, align);
            if old_small && new_small {
                let bs = KALLOC.block_size(p);
                if new_size <= bs && new_size >= bs / 2 {
                    return p;
                }
            } else if !old_small && !new_small {
                let cap = large_capacity(p);
                if new_size <= cap && new_size >= cap / 2 {
                    return p;
                }
            }
        }
        let new_layout = Layout::from_size_align_unchecked(new_size, layout.align());
        let q = self.alloc(new_layout);
        if !q.is_null() {
            ptr::copy_nonoverlapping(p, q, layout.size().min(new_size));
            self.dealloc(p, layout);
        }
        q
    }
}

// ---------------------------------------------------------------------------
// Statistiques
// ---------------------------------------------------------------------------

/// État du tas, pour `free -v` et les bancs de mesure.
#[derive(Debug, Clone, Copy, Default)]
pub struct HeapStats {
    pub enabled: bool,
    pub kalloc: kalloc::Stats,
    /// Segments de 2 Mio mappés (tas des cœurs + réserve).
    pub segments_mapped: usize,
    /// Gros objets mappés directement.
    pub large_count: usize,
    pub large_bytes: usize,
}

pub fn stats() -> HeapStats {
    let kalloc = {
        let _irq = hold_interrupts();
        KALLOC.stats()
    };
    HeapStats {
        enabled: ENABLED.load(Relaxed),
        kalloc,
        segments_mapped: SEGMENTS_MAPPED.load(Relaxed),
        large_count: LARGE_COUNT.load(Relaxed),
        large_bytes: LARGE_BYTES.load(Relaxed),
    }
}

/// Compteurs du tas d'un seul cœur.
pub fn cpu_stats(cpu: usize) -> Option<kalloc::Stats> {
    KALLOC.heap_stats(cpu)
}
