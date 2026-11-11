//! Attentes bloquantes des fils : files d'attente, délais, interruption par signal.
//!
//! Règle de MaiSched : une tâche se **bloque avant** de s'inscrire à une source de réveil, puis
//! revérifie sa condition ; un réveil arrivé entre-temps la trouve bloquée et la rend prête, et
//! `schedule()` voit qu'elle l'est redevenue. Toutes les attentes bouclent sur leur condition : un
//! réveil parasite (minuteur périmé, signal déjà traité) ne fait qu'un tour de plus.
//!
//! Les files sont protégées par un verrou qui coupe les interruptions : elles peuvent être
//! réveillées depuis une interruption (minuteurs `alarm`). On n'y touche jamais à la mémoire d'un
//! processus (voir `uvm` : les IPI d'invalidation doivent rester servies).

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};
use sync_irq::IrqSafeMutex;
use task::TaskRef;
use time::Instant;

use crate::Thread;

/// Observateur d'une file : prévenu à chaque réveil, sans être retiré (une entrée d'`epoll`
/// reste inscrite tant qu'elle surveille le fichier, comme les rappels `ep_poll_callback` de
/// Linux). `notify` peut être appelé depuis une interruption : rien de bloquant, aucune mémoire
/// de processus.
pub trait Watcher: Send + Sync {
    fn notify(self: Arc<Self>);
}

struct Inner {
    tasks: Vec<TaskRef>,
    watchers: Vec<Arc<dyn Watcher>>,
}

/// File de tâches qui attendent un événement, et d'observateurs ([`Watcher`]).
pub struct WaitQueue {
    inner: IrqSafeMutex<Inner>,
}

impl WaitQueue {
    pub const fn new() -> WaitQueue {
        WaitQueue { inner: IrqSafeMutex::new(Inner { tasks: Vec::new(), watchers: Vec::new() }) }
    }

    fn add(&self, t: &TaskRef) {
        let mut w = self.inner.lock();
        if !w.tasks.iter().any(|x| x.id == t.id) {
            w.tasks.push(t.clone());
        }
    }

    fn remove(&self, t: &TaskRef) {
        self.inner.lock().tasks.retain(|x| x.id != t.id);
    }

    /// Inscrit un observateur permanent.
    pub fn watch(&self, w: Arc<dyn Watcher>) {
        self.inner.lock().watchers.push(w);
    }

    /// Retire un observateur (comparaison des adresses).
    pub fn unwatch(&self, w: &Arc<dyn Watcher>) {
        let target = Arc::as_ptr(w) as *const u8;
        self.inner.lock().watchers.retain(|x| Arc::as_ptr(x) as *const u8 != target);
    }

    /// Observateurs à prévenir (copie prise sous le verrou, appel hors verrou).
    fn watchers(&self, inner: &Inner) -> Option<Vec<Arc<dyn Watcher>>> {
        let _ = self;
        if inner.watchers.is_empty() { None } else { Some(inner.watchers.clone()) }
    }

    /// Réveille toutes les tâches inscrites et prévient les observateurs.
    pub fn wake_all(&self) {
        let (ws, obs) = {
            let mut i = self.inner.lock();
            let obs = self.watchers(&i);
            (core::mem::take(&mut i.tasks), obs)
        };
        for t in ws {
            let _ = t.unblock();
        }
        for o in obs.into_iter().flatten() {
            o.notify();
        }
    }

    /// Réveille une tâche (la plus ancienne) et prévient les observateurs.
    pub fn wake_one(&self) {
        let (t, obs) = {
            let mut i = self.inner.lock();
            let obs = self.watchers(&i);
            (if i.tasks.is_empty() { None } else { Some(i.tasks.remove(0)) }, obs)
        };
        if let Some(t) = t {
            let _ = t.unblock();
        }
        for o in obs.into_iter().flatten() {
            o.notify();
        }
    }
}

impl Default for WaitQueue {
    fn default() -> Self { Self::new() }
}

/// Fin d'une attente sans que la condition soit remplie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitError {
    /// Un signal est à traiter (ou le groupe de fils se termine).
    Interrupted,
    /// Délai écoulé.
    TimedOut,
}

/// Attend que `check()` renvoie `Some`, en dormant sur `queues`. Avec `thread`, l'attente est
/// interrompue par un signal livrable ; avec `deadline`, par le délai (horloge monotone).
///
/// `check` peut consommer ce qu'il trouve (données d'un tube…) : dès qu'il renvoie `Some`, la
/// valeur est rendue sans autre appel.
pub fn wait_event<T>(
    queues: &[&WaitQueue],
    deadline: Option<Instant>,
    thread: Option<&Thread>,
    mut check: impl FnMut() -> Option<T>,
) -> Result<T, WaitError> {
    let Some(task) = task::get_my_current_task() else {
        // Pas de tâche (démarrage) : attente active.
        loop {
            if let Some(v) = check() {
                return Ok(v);
            }
            if deadline.map_or(false, |d| Instant::now() >= d) {
                return Err(WaitError::TimedOut);
            }
            core::hint::spin_loop();
        }
    };
    let interrupted = |t: Option<&Thread>| t.map_or(false, |t| t.signal_pending());
    let mut armed: Option<Instant> = None;
    loop {
        if let Some(v) = check() {
            return Ok(v);
        }
        if interrupted(thread) {
            return Err(WaitError::Interrupted);
        }
        if let Some(d) = deadline {
            if Instant::now() >= d {
                return Err(WaitError::TimedOut);
            }
        }
        let _ = task.block();
        for q in queues {
            q.add(&task);
        }
        if let Some(d) = deadline {
            if armed != Some(d) {
                sleep::wake_at(task.clone(), d);
                armed = Some(d);
            }
        }
        // Revérification une fois inscrit : un événement arrivé avant l'inscription est vu ici.
        let ready = check();
        if ready.is_some() || interrupted(thread) || deadline.map_or(false, |d| Instant::now() >= d) {
            let _ = task.unblock();
            for q in queues {
                q.remove(&task);
            }
            if let Some(v) = ready {
                return Ok(v);
            }
            continue;
        }
        task::schedule();
        for q in queues {
            q.remove(&task);
        }
        // Un minuteur qui a sonné ne sonnera plus : on le réarme au besoin au tour suivant.
        if deadline.map_or(false, |d| Instant::now() >= d) {
            armed = None;
        }
    }
}

/// Événement à usage unique (fin de `vfork`, fin d'un processus pour un parent natif).
pub struct Completion {
    done: AtomicBool,
    queue: WaitQueue,
}

impl Completion {
    pub fn new() -> Arc<Completion> {
        Arc::new(Completion { done: AtomicBool::new(false), queue: WaitQueue::new() })
    }

    pub fn complete(&self) {
        self.done.store(true, Ordering::Release);
        self.queue.wake_all();
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// Attend la fin ; `thread` : interrompue par un signal (renvoie `false`).
    pub fn wait(&self, thread: Option<&Thread>) -> bool {
        wait_event(&[&self.queue], None, thread, || self.is_done().then_some(())).is_ok()
    }
}
