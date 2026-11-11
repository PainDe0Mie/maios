//! Manages preemption on a per-CPU basis.
//!
//! Supports enabling and disabling preemption for the purpose of 
//! safe task state management, e.g., through preemption-safe locks.

#![no_std]
#![feature(negative_impls, thread_local)]

use core::sync::atomic::{AtomicUsize, Ordering};
use cpu::CpuId;

/// A reference to the preemption counter for the current CPU (in CPU-local storage).
// NOTE: This offset must be kept in sync with `cpu_local::PerCpuField`.
#[cls_macros::cpu_local(cls_dep = false)]
static PREEMPTION_COUNT: u8 = 0;

/// Point de préemption différée, enregistré par l'ordonnanceur (`fn()` stockée en `usize`).
static PREEMPT_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Enregistre la fonction appelée quand la préemption redevient possible sur un cœur (compteur
/// qui repasse à 0) : l'ordonnanceur y effectue une replanification demandée pendant la section
/// non préemptible (Linux : `preempt_enable()` → `preempt_schedule()`).
pub fn set_preempt_hook(f: fn()) {
    PREEMPT_HOOK.store(f as usize, Ordering::Release);
}

/// Prevents preemption (preemptive task switching) from occurring
/// until the returned guard object is dropped.
///
/// Préemption paresseuse : le timer local n'est plus masqué. Un tick qui tombe pendant la section
/// fait sa comptabilité et note `need_resched` ; la replanification a lieu quand le dernier garde
/// est relâché. L'ancien masquage réécrivait le compteur du timer APIC à chaque relâche, ce qui
/// relançait la période : sur un cœur actif, le tick de 2 ms ne tombait presque jamais (66 Hz
/// mesurés au lieu de 500), et chaque `schedule()` coûtait deux écritures APIC (sorties de VM).
pub fn hold_preemption() -> PreemptionGuard {
    hold_preemption_internal()
}

/// Identique à [`hold_preemption()`] (conservée pour compatibilité : le timer n'est plus jamais
/// masqué).
#[doc(hidden)]
pub fn hold_preemption_no_timer_disable() -> PreemptionGuard {
    hold_preemption_internal()
}

#[inline]
fn hold_preemption_internal() -> PreemptionGuard {
    // Préemption coupée D'ABORD, cœur lu ensuite (comme `get_cpu()` de Linux). Dans l'autre ordre,
    // une interruption entre les deux pouvait migrer la tâche : le garde portait l'ancien cœur, et
    // `schedule()` manipulait la file d'un autre cœur (entité courante écrasée → tâche prête jamais
    // choisie ; tâche marquée « en cours » sur le mauvais cœur → panique de `switch_to`).
    let prev_val = PREEMPTION_COUNT.fetch_add(1);
    let cpu_id = cpu::current_cpu();
    if prev_val == u8::MAX {
        // Overflow occurred and the counter value wrapped around, which is a bug.
        panic!("BUG: Overflow occurred in the preemption counter for CPU {}", cpu_id);
    }
    PreemptionGuard {
        cpu_id,
        preemption_was_enabled: prev_val == 0,
    }
}

#[inline]
fn run_preempt_hook() {
    let p = PREEMPT_HOOK.load(Ordering::Acquire);
    if p != 0 {
        // SAFETY : seule une `fn()` est enregistrée.
        let f: fn() = unsafe { core::mem::transmute(p) };
        f();
    }
}


/// A guard type that ensures preemption is disabled as long as it is held.
///
/// Call [`hold_preemption()`] to obtain a `PreemptionGuard`.
///
/// Preemption *may* be re-enabled when this guard is dropped,
/// but not necessarily so, because other previous functions 
/// in the call stack may have already acquired a `PreemptionGuard`.
///
/// This type does not implement `Send` because it is invalid
/// to move it across a "thread" boundary (into a different task).
/// More specifically, it is invalid to move a `PreemptionGuard` across
/// CPUs; this error condition is checked for when dropping it.
#[derive(Debug)]
pub struct PreemptionGuard {
    /// The ID of the CPU on which preemption was held.
    ///
    /// This is mostly used for strict sanity checks to ensure that
    /// a guard isn't created on one CPU and then dropped on a different CPU.
    cpu_id: CpuId,
    /// Whether preemption was enabled when this guard was created.
    preemption_was_enabled: bool,
}
impl !Send for PreemptionGuard { }

impl PreemptionGuard {
    /// Returns whether preemption was originally enabled when this guard was created.
    ///
    /// # Return
    /// * `true`: indicates that the caller function/task holding this guard
    ///    was the one that caused the transition from preemption
    ///    being enabled on this CPU to being disabled.
    /// * `false`: indicates that preemption was already disabled
    ///    and that no transition occurred when the caller function/task
    ///    obtained this guard.
    pub fn preemption_was_enabled(&self) -> bool {
        self.preemption_was_enabled
    }

    /// Returns the ID of the CPU on which this guard was created.
    pub fn cpu_id(&self) -> CpuId {
        self.cpu_id
    }
}

impl Drop for PreemptionGuard {
    fn drop(&mut self) {
        let prev_val = PREEMPTION_COUNT.fetch_sub(1);

        // Le compteur repasse à 0 : la préemption redevient possible ; on honore une
        // replanification demandée pendant la section.
        if prev_val == 1 {
            run_preempt_hook();
        } else if prev_val == 0 {
            // Underflow occurred and the counter value wrapped around, which is a bug.
            let cpu_id = cpu::current_cpu();
            panic!("BUG: Underflow occurred in the preemption counter for CPU {}", cpu_id);
        }
    }
}

/// Returns `true` if preemption is currently enabled on this CPU.
///
/// Note that unless preemption or interrupts are disabled, this value can't be used as a lock
/// indicator or property. It is just a snapshot that offers no guarantee that preemption will
/// continue to be enabled or disabled immediately after returning.
pub fn preemption_enabled() -> bool {
    PREEMPTION_COUNT.load() == 0
}

pub unsafe fn force_enable_preemption() {
    let current = PREEMPTION_COUNT.load();
    if current > 0 {
        // Soustrait la valeur actuelle pour ramener à 0
        PREEMPTION_COUNT.fetch_add(u8::wrapping_neg(current));
    }
}
