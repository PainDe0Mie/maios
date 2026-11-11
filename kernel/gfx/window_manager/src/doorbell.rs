//! Sonnette du compositeur : une tâche unique dort jusqu'à ce qu'on sonne ou qu'une échéance
//! arrive.
//!
//! * [`Doorbell::ring`] est sûre en interruption (clavier, souris) : un échange atomique, puis
//!   au plus un `unblock()`. Elle ne débloque que sur le passage « pas sonné → sonné », si bien
//!   qu'une rafale d'événements ne coûte qu'un réveil.
//! * [`Doorbell::wait`] suit la règle de MaiSched : **se bloquer, puis revérifier**. Une
//!   sonnerie arrivée entre les deux trouve la tâche déjà bloquée et la réveille ; MaiSched ne
//!   la retire de sa file qu'au `schedule()`, qui la voit redevenue prête.
//! * L'échéance passe par `sleep::wake_at`. On n'arme un nouveau minuteur que si aucun n'est
//!   déjà prévu plus tôt (ou si l'ancien est échu) : les réveils anticipés par la sonnette ne
//!   remplissent pas la liste des minuteurs. Un minuteur périmé ne fait qu'un réveil de trop,
//!   que la boucle du compositeur absorbe (elle revérifie tout à chaque tour).

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use spin::Once;
use task::TaskRef;
use time::{Instant, Monotonic};

pub struct Doorbell {
    rung: AtomicBool,
    task: Once<TaskRef>,
    /// Échéance du minuteur armé (`ticks` d'un `Instant`), `u64::MAX` si aucun.
    armed: AtomicU64,
}

impl Default for Doorbell {
    fn default() -> Self {
        Self::new()
    }
}

impl Doorbell {
    pub const fn new() -> Self {
        Self { rung: AtomicBool::new(true), task: Once::new(), armed: AtomicU64::new(u64::MAX) }
    }

    /// Attache la sonnette à la tâche courante (une seule fois).
    pub fn bind_current(&self) {
        if let Some(t) = task::get_my_current_task() {
            self.task.call_once(|| t);
        }
    }

    /// Sonne : réveille la tâche si elle ne l'était pas déjà.
    #[inline]
    pub fn ring(&self) {
        if !self.rung.swap(true, Ordering::AcqRel) {
            if let Some(t) = self.task.get() {
                let _ = t.unblock();
            }
        }
    }

    /// Dort jusqu'à une sonnerie ou jusqu'à `deadline`. Peut revenir plus tôt (réveil de trop) :
    /// l'appelant revérifie toujours son état.
    ///
    /// La sonnerie est consommée au réveil : tout ce qui a été publié avant `ring()` est visible
    /// ensuite (`AcqRel`).
    pub fn wait(&self, deadline: Instant) {
        if self.rung.swap(false, Ordering::AcqRel) {
            return;
        }
        let t = match self.task.get() {
            Some(t) => t,
            None => return,
        };
        if t.block().is_err() {
            return;
        }
        if self.rung.swap(false, Ordering::AcqRel) {
            let _ = t.unblock();
            return;
        }
        let now = time::now::<Monotonic>();
        if deadline <= now {
            let _ = t.unblock();
            return;
        }
        let armed = self.armed.load(Ordering::Relaxed);
        if armed == u64::MAX || armed <= now.ticks() || deadline.ticks() < armed {
            self.armed.store(deadline.ticks(), Ordering::Relaxed);
            sleep::wake_at(t.clone(), deadline);
        }
        task::schedule();
        self.rung.store(false, Ordering::Release);
    }
}
