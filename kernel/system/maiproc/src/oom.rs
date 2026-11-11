//! Tueur de mémoire épuisée (OOM killer).
//!
//! Quand une faute de page d'un processus ne trouve plus de frame, le noyau ne tue pas le fil
//! qui faute (ce serait tirer au hasard : n'importe quel processus qui touche une page neuve à ce
//! moment-là) : comme Linux (`out_of_memory`), il choisit une **victime** — le processus qui
//! occupe le plus de mémoire —, lui envoie `SIGKILL`, attend que sa mémoire revienne, et la faute
//! est rejouée. Une seule victime à la fois : tant que la précédente n'est pas morte, les fautes
//! suivantes l'attendent au lieu d'en tuer une autre (le rôle de `oom_reaper` et du drapeau
//! `TIF_MEMDIE`).

use alloc::sync::{Arc, Weak};
use core::sync::atomic::Ordering;
use core::time::Duration;
use log::warn;
use sync_preemption::PreemptionSafeMutex as Mutex;

use crate::signal::{self, SigInfo};
use crate::wait::wait_event;
use crate::{Process, Thread};

/// Victime en cours (tuée, pas encore morte).
static VICTIM: Mutex<Option<Weak<Process>>> = Mutex::new(None);

/// Temps laissé à une victime pour rendre sa mémoire.
const GRACE: Duration = Duration::from_secs(2);

fn rss_of(p: &Process) -> usize {
    p.mm.lock().as_ref().map_or(0, |m| m.rss())
}

/// Score d'un processus, comme `oom_badness` de Linux : pages résidentes plus
/// `oom_score_adj` millièmes de la mémoire totale ; `None` : jamais tué (−1000).
fn badness(p: &Process, total: i64) -> Option<i64> {
    let adj = p.oom_score_adj.load(Ordering::Relaxed) as i64;
    if adj <= -1000 {
        return None;
    }
    Some(rss_of(p) as i64 + adj * total / 1000)
}

/// Processus au plus fort score, zombies exclus.
fn pick() -> Option<Arc<Process>> {
    let total = frame_allocator::stats().total_frames as i64;
    crate::all_processes()
        .into_iter()
        .filter(|p| !p.is_zombie())
        .filter_map(|p| badness(&p, total).map(|b| (b, p)))
        .max_by_key(|(b, _)| *b)
        .map(|(_, p)| p)
}

fn gone(p: &Weak<Process>) -> bool {
    p.upgrade().map_or(true, |p| p.is_zombie())
}

/// Mémoire épuisée pendant une faute du fil `t`. Rend `true` si la faute peut être rejouée (une
/// victime est morte ou a eu son délai), `false` si `t` appartient lui-même à la victime : il
/// doit simplement retourner vers l'espace utilisateur, où son `SIGKILL` l'attend.
pub fn out_of_memory(t: &Thread) -> bool {
    // D'abord rendre ce qui peut l'être sans tuer : les caches de fichiers inutilisés (le
    // *reclaim* de Linux passe avant l'OOM).
    if crate::pagecache::shrink() > 0 {
        return true;
    }
    let victim = {
        let mut g = VICTIM.lock();
        match g.as_ref().filter(|w| !gone(w)).and_then(|w| w.upgrade()) {
            Some(v) => v,
            None => {
                let Some(v) = pick() else { return false };
                let rss = rss_of(&v);
                warn!(
                    "MaiMM : memoire epuisee (faute du pid {}) : pid {} ({}) tue, RSS {} Kio",
                    t.process.pid,
                    v.pid,
                    v.exe.lock().as_str(),
                    rss * 4
                );
                signal::send_to_process(&v, SigInfo::new(signal::SIGKILL, signal::SI_KERNEL));
                *g = Some(Arc::downgrade(&v));
                v
            }
        }
    };
    if Arc::ptr_eq(&victim, &t.process) {
        return false;
    }
    // La victime meurt : `process_exit` rend son espace d'adresses.
    let w = Arc::downgrade(&victim);
    drop(victim);
    let deadline = time::Instant::now() + GRACE;
    while !gone(&w) && time::Instant::now() < deadline {
        // Sommeil de 5 ms non interruptible (un signal en attente ne doit pas en faire une
        // attente active) ; la sortie de notre propre groupe est vérifiée à chaque tour.
        let step = time::Instant::now() + Duration::from_millis(5);
        let _ = wait_event::<()>(&[], Some(step), None, || None);
        if t.process.group_exit.load(core::sync::atomic::Ordering::Acquire) {
            return false;
        }
    }
    let mut g = VICTIM.lock();
    if g.as_ref().map_or(false, |v| gone(v)) {
        *g = None;
    }
    true
}
