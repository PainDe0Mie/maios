//! MaiMM M0 : compteurs de l'allocateur de frames et identification du CPU courant.
//!
//! Les compteurs sont **par CPU**, chacun sur sa propre ligne de cache. Un compteur global
//! partagé serait un point de contention qui annulerait le bénéfice d'un allocateur sans verrou.
//! Les incréments sont `Relaxed` : la somme lue par [`crate::stats`] est cohérente au repos et
//! approximative pendant les allocations concurrentes.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};

/// Nombre d'emplacements par CPU (l'index CPU est pris modulo cette valeur).
pub(crate) const CPU_SLOTS: usize = 64;

#[repr(align(64))]
pub(crate) struct CpuCounters {
    allocs: AtomicU64,
    alloc_frames: AtomicU64,
    frees: AtomicU64,
    free_frames: AtomicU64,
    failures: AtomicU64,
    zeroed_hits: AtomicU64,
    zeroed_misses: AtomicU64,
}

impl CpuCounters {
    const fn new() -> Self {
        CpuCounters {
            allocs: AtomicU64::new(0),
            alloc_frames: AtomicU64::new(0),
            frees: AtomicU64::new(0),
            free_frames: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            zeroed_hits: AtomicU64::new(0),
            zeroed_misses: AtomicU64::new(0),
        }
    }
}

#[allow(clippy::declare_interior_mutable_const)]
const COUNTERS_INIT: CpuCounters = CpuCounters::new();
static COUNTERS: [CpuCounters; CPU_SLOTS] = [COUNTERS_INIT; CPU_SLOTS];

/// Fonction optionnelle fournie par l'architecture pour obtenir l'index du CPU courant.
static CPU_ID_FN: AtomicUsize = AtomicUsize::new(0);

/// Remplace la façon dont l'allocateur identifie le CPU courant.
///
/// Sur x86_64, ce n'est pas nécessaire : `rdtscp` renvoie `IA32_TSC_AUX`, que le noyau
/// initialise avec l'APIC ID de chaque cœur. Il est disponible dès le premier instant et coûte
/// beaucoup moins cher qu'un `rdmsr`, qui provoque une sortie de VM sous KVM.
pub fn set_cpu_id_fn(f: fn() -> usize) {
    CPU_ID_FN.store(f as usize, Relaxed);
}

/// Index du CPU courant, utilisé pour choisir l'arbre LLFree réservé et le compteur à
/// incrémenter. Une valeur fausse (tâche migrée) n'affecte que la localité, jamais la correction.
#[inline]
pub fn current_cpu() -> usize {
    let f = CPU_ID_FN.load(Relaxed);
    if f != 0 {
        // SAFETY: seule `set_cpu_id_fn` écrit ici, avec un vrai `fn() -> usize`.
        let f: fn() -> usize = unsafe { core::mem::transmute(f) };
        return f();
    }
    #[cfg(target_arch = "x86_64")]
    {
        let mut aux = 0u32;
        // SAFETY: `rdtscp` n'a aucun effet de bord ; il est disponible sur tous les CPU x86_64
        // ciblés par MaiOS (Broadwell et plus récents).
        unsafe { core::arch::x86_64::__rdtscp(&mut aux) };
        aux as usize
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        0
    }
}

#[inline]
fn slot() -> &'static CpuCounters {
    &COUNTERS[current_cpu() % CPU_SLOTS]
}

#[inline]
pub(crate) fn record_alloc(frames: usize) {
    let s = slot();
    s.allocs.fetch_add(1, Relaxed);
    s.alloc_frames.fetch_add(frames as u64, Relaxed);
}

#[inline]
pub(crate) fn record_free(frames: usize) {
    let s = slot();
    s.frees.fetch_add(1, Relaxed);
    s.free_frames.fetch_add(frames as u64, Relaxed);
}

#[inline]
pub(crate) fn record_failure() {
    slot().failures.fetch_add(1, Relaxed);
}

#[inline]
pub(crate) fn record_zeroed(hit: bool) {
    let s = slot();
    if hit {
        s.zeroed_hits.fetch_add(1, Relaxed);
    } else {
        s.zeroed_misses.fetch_add(1, Relaxed);
    }
}

/// Totaux des compteurs d'événements, sommés sur tous les CPU.
#[derive(Debug, Default, Clone, Copy)]
pub struct EventCounters {
    /// Nombre d'appels d'allocation réussis.
    pub allocations: u64,
    /// Nombre total de frames allouées par ces appels.
    pub allocated_frames: u64,
    /// Nombre de libérations, c'est-à-dire de `Frames` rendues à l'allocateur.
    pub deallocations: u64,
    /// Nombre total de frames libérées.
    pub freed_frames: u64,
    /// Allocations refusées (mémoire épuisée ou adresse indisponible).
    pub failures: u64,
    /// Frames pré-zéroées servies par le pool.
    pub zeroed_hits: u64,
    /// Demandes de frame zéroée qui ont dû zéroer à la volée (pool vide).
    pub zeroed_misses: u64,
}

pub(crate) fn sum_counters() -> EventCounters {
    let mut e = EventCounters::default();
    for c in COUNTERS.iter() {
        e.allocations += c.allocs.load(Relaxed);
        e.allocated_frames += c.alloc_frames.load(Relaxed);
        e.deallocations += c.frees.load(Relaxed);
        e.freed_frames += c.free_frames.load(Relaxed);
        e.failures += c.failures.load(Relaxed);
        e.zeroed_hits += c.zeroed_hits.load(Relaxed);
        e.zeroed_misses += c.zeroed_misses.load(Relaxed);
    }
    e
}
