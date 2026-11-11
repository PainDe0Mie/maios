//! Vecteurs de messages (MSI/MSI-X) avec un argument, à la manière des *IRQ domains* de Linux.
//!
//! Une entrée de l'IDT n'a pas d'argument : un pilote à plusieurs files (NVMe, cartes réseau à
//! files multiples) devrait écrire une fonction par file. Ce module garde un jeu de
//! [`MAX_DYNAMIC_VECTORS`] trampolines ; chacun lit son emplacement (fonction + donnée), appelle
//! le pilote avec la donnée (par exemple l'adresse de la file), puis envoie l'EOI. Le vecteur est
//! choisi parmi ceux que personne n'utilise, comme [`register_msi_interrupt`].
//!
//! Un emplacement s'écrit avant la mise en place du vecteur dans l'IDT et s'efface après son
//! retrait : un trampoline appelé sans emplacement (interruption en retard) ne fait que l'EOI.
//!
//! [`register_msi_interrupt`]: super::register_msi_interrupt

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::{InterruptHandler, InterruptStackFrame};

/// Fonction d'un vecteur dynamique : reçoit la donnée donnée à l'enregistrement.
/// Appelée interruptions coupées, sur le cœur visé par le message ; l'EOI suit son retour.
pub type MsiCallback = fn(usize);

/// Nombre de vecteurs dynamiques (64 files de complétion NVMe, ou moins plus d'autres pilotes).
pub const MAX_DYNAMIC_VECTORS: usize = 64;

struct Slot {
    /// Emplacement pris (même si sa fonction n'est pas encore posée).
    busy: AtomicBool,
    /// Fonction (`0` : aucune, l'interruption ne fait que l'EOI).
    func: AtomicUsize,
    data: AtomicUsize,
    vector: AtomicUsize,
}

#[allow(clippy::declare_interior_mutable_const)]
const EMPTY: Slot = Slot { busy: AtomicBool::new(false), func: AtomicUsize::new(0), data: AtomicUsize::new(0), vector: AtomicUsize::new(0) };
static SLOTS: [Slot; MAX_DYNAMIC_VECTORS] = [EMPTY; MAX_DYNAMIC_VECTORS];

#[inline(always)]
fn dispatch(slot: usize) {
    let s = &SLOTS[slot];
    let func = s.func.load(Ordering::Acquire);
    if func != 0 {
        // SAFETY : `func` a été écrit par `register_msi_callback` depuis un `MsiCallback`.
        let f: MsiCallback = unsafe { core::mem::transmute::<usize, MsiCallback>(func) };
        f(s.data.load(Ordering::Relaxed));
    }
    super::eoi(s.vector.load(Ordering::Relaxed) as u8);
}

macro_rules! trampolines {
    ($($n:literal),* $(,)?) => {
        static TRAMPOLINES: [InterruptHandler; MAX_DYNAMIC_VECTORS] = [$(trampoline::<$n>),*];
    };
}

extern "x86-interrupt" fn trampoline<const N: usize>(_sf: InterruptStackFrame) {
    dispatch(N);
}

trampolines!(
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
    16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31,
    32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47,
    48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63,
);

/// Vecteur dynamique enregistré ; à rendre par [`unregister_msi_callback`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsiVector {
    /// Numéro du vecteur dans l'IDT (à écrire dans le message MSI/MSI-X).
    pub vector: u8,
    slot: usize,
}

/// Réserve un vecteur libre de l'IDT qui appellera `func(data)` à chaque message.
pub fn register_msi_callback(func: MsiCallback, data: usize) -> Result<MsiVector, &'static str> {
    let slot = (0..MAX_DYNAMIC_VECTORS)
        .find(|&i| SLOTS[i].busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).is_ok())
        .ok_or("register_msi_callback : plus de vecteur dynamique libre")?;
    let s = &SLOTS[slot];
    s.data.store(data, Ordering::Relaxed);
    match super::register_msi_interrupt(TRAMPOLINES[slot]) {
        Ok(vector) => {
            s.vector.store(vector as usize, Ordering::Relaxed);
            s.func.store(func as usize, Ordering::Release);
            Ok(MsiVector { vector, slot })
        }
        Err(e) => {
            s.busy.store(false, Ordering::Release);
            Err(e)
        }
    }
}

/// Rend un vecteur pris par [`register_msi_callback`] (le périphérique ne doit plus l'émettre).
pub fn unregister_msi_callback(v: MsiVector) -> Result<(), &'static str> {
    super::deregister_interrupt(v.vector, TRAMPOLINES[v.slot])?;
    let s = &SLOTS[v.slot];
    s.func.store(0, Ordering::Release);
    s.data.store(0, Ordering::Relaxed);
    s.busy.store(false, Ordering::Release);
    Ok(())
}
