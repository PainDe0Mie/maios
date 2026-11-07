//! Pool de frames pré-zéroées (MaiMM M1), inspiré de la *zeroed page list* de Windows.
//!
//! Une page fault anonyme, une nouvelle table de pages ou un tampon remis à une application
//! doivent être remplis de zéros. Les zéroer à la demande coûte environ 1 µs par page, sur le
//! chemin critique. Ici, les cœurs **inactifs** zéroent des frames à l'avance (voir
//! `memory::refill_zeroed_frames`, appelé par la tâche idle) et les déposent dans ce pool. Le
//! consommateur n'a plus qu'à les prendre.
//!
//! Le pool ne connaît pas les adresses virtuelles et ne zéroe donc rien lui-même : il ne fait que
//! conserver des numéros de frames dont le contenu est garanti nul.
//!
//! Structure : un tableau fixe de cases atomiques contenant `numéro de frame + 1`, ou 0 si la case
//! est vide. Prendre une frame coûte un `swap(0)`, en déposer une un CAS `0 -> n`. Aucun verrou :
//! le pool est utilisable depuis n'importe quel contexte, interruptions comprises.

use core::mem;
use core::sync::atomic::{AtomicUsize, Ordering::{Acquire, Relaxed, Release}};

use crate::{general_frames, stats, AllocatedFrames, MemoryRegionType};

/// Capacité du pool : 1024 frames, soit 4 Mio.
pub const ZEROED_POOL_CAPACITY: usize = 1024;

#[allow(clippy::declare_interior_mutable_const)]
const EMPTY: AtomicUsize = AtomicUsize::new(0);
static SLOTS: [AtomicUsize; ZEROED_POOL_CAPACITY] = [EMPTY; ZEROED_POOL_CAPACITY];
static COUNT: AtomicUsize = AtomicUsize::new(0);
static TAKE_HINT: AtomicUsize = AtomicUsize::new(0);
static PUT_HINT: AtomicUsize = AtomicUsize::new(0);

/// Alloue une frame dont le contenu est **garanti nul**, si le pool en contient une.
///
/// Renvoie `None` quand le pool est vide. L'appelant alloue alors une frame normale et la zéroe
/// lui-même. Chaque succès ou échec est comptabilisé dans `stats().events`.
pub fn allocate_zeroed_frame() -> Option<AllocatedFrames> {
    let frame = take();
    stats::record_zeroed(frame.is_some());
    frame
}

/// Nombre de frames actuellement dans le pool.
pub fn zeroed_pool_len() -> usize {
    COUNT.load(Relaxed)
}

/// Nombre de cases libres dans le pool.
pub fn zeroed_pool_deficit() -> usize {
    ZEROED_POOL_CAPACITY.saturating_sub(COUNT.load(Relaxed))
}

/// Dépose dans le pool des frames **dont le contenu vient d'être mis à zéro et qui ne sont plus
/// mappées nulle part**. Celles qui ne rentrent pas sont simplement libérées.
pub fn push_zeroed_frames(frames: AllocatedFrames) {
    if frames.typ() != MemoryRegionType::Free || frames.size_in_frames() == 0 {
        return; // les frames réservées (MMIO…) n'ont rien à faire ici : `drop` les rend
    }
    let start = frames.start().number();
    let n = frames.size_in_frames();
    // Le pool devient propriétaire des frames : il conserve leurs numéros, pas l'objet.
    mem::forget(frames);
    let mut stored = 0;
    while stored < n && put(start + stored) {
        stored += 1;
    }
    if stored < n {
        drop(general_frames(start + stored, n - stored).into_allocated_frames());
    }
}

/// Prend une frame du pool (sans comptabilité).
pub(crate) fn take() -> Option<AllocatedFrames> {
    if COUNT.load(Relaxed) == 0 {
        return None;
    }
    let start = TAKE_HINT.load(Relaxed);
    for i in 0..ZEROED_POOL_CAPACITY {
        let idx = (start + i) % ZEROED_POOL_CAPACITY;
        if SLOTS[idx].load(Relaxed) == 0 {
            continue;
        }
        // Acquire : on voit les zéros écrits avant le dépôt (Release dans `put`).
        let got = SLOTS[idx].swap(0, Acquire);
        if got != 0 {
            COUNT.fetch_sub(1, Relaxed);
            TAKE_HINT.store(idx + 1, Relaxed);
            return Some(general_frames(got - 1, 1).into_allocated_frames());
        }
    }
    None
}

fn put(frame: usize) -> bool {
    let start = PUT_HINT.load(Relaxed);
    for i in 0..ZEROED_POOL_CAPACITY {
        let idx = (start + i) % ZEROED_POOL_CAPACITY;
        if SLOTS[idx].load(Relaxed) != 0 {
            continue;
        }
        if SLOTS[idx].compare_exchange(0, frame + 1, Release, Relaxed).is_ok() {
            COUNT.fetch_add(1, Relaxed);
            PUT_HINT.store(idx + 1, Relaxed);
            return true;
        }
    }
    false
}

/// Vide le pool vers l'allocateur (pression mémoire). Renvoie le nombre de frames rendues.
pub(crate) fn drain() -> usize {
    let mut n = 0;
    while let Some(af) = take() {
        drop(af);
        n += 1;
    }
    n
}
