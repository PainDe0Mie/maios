//! Minuteurs de temps processeur : `ITIMER_VIRTUAL` (`SIGVTALRM`) et `ITIMER_PROF` (`SIGPROF`).
//!
//! Linux les vérifie à chaque tick sur la tâche courante (`run_posix_cpu_timers`). MaiSched n'a
//! pas de tick fixe à offrir aux processus, et rien de ce qu'une interruption touche ne doit
//! prendre de verrou ordinaire : une tâche du noyau tient donc la liste des processus armés et
//! dort jusqu'au **plus tôt possible** des échéances. Le temps processeur d'un processus avance
//! au plus de `k` ns par ns d'horloge murale, `k` = min(fils, cœurs) ; une échéance distante de
//! `r` ns de temps processeur ne peut donc pas tomber avant `r / k` ns : on se réveille là, on
//! relit le temps consommé, et on recommence. Un processus endormi ne coûte qu'un réveil par
//! `r / k` ; un processus qui calcule voit son signal à moins d'une période de vérification près.
//!
//! MaiOS ne sépare pas encore temps utilisateur et temps système : `ITIMER_VIRTUAL` et
//! `ITIMER_PROF` décomptent tous deux le temps processeur total du processus.

use alloc::{sync::Arc, sync::Weak, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use spin::Mutex;

use crate::signal::{self, SigInfo};
use crate::wait::{wait_event, WaitQueue};
use crate::Process;

pub const VIRTUAL: usize = 0;
pub const PROF: usize = 1;

const SIGVTALRM: i32 = 26;
const SIGPROF: i32 = 27;

/// Un minuteur, en ns de temps processeur du processus (`expires` = 0 : désarmé).
#[derive(Clone, Copy, Default)]
pub struct CpuTimer {
    pub expires: u64,
    pub interval: u64,
}

static STARTED: spin::Once<()> = spin::Once::new();
static KICK: AtomicBool = AtomicBool::new(false);
static QUEUE: WaitQueue = WaitQueue::new();
/// Processus dont au moins un minuteur est armé.
static ARMED: Mutex<Vec<Weak<Process>>> = Mutex::new(Vec::new());
/// Minuteurs POSIX sur une horloge de temps processeur (`crate::ptimer`).
static POSIX: Mutex<Vec<Weak<crate::ptimer::PosixTimer>>> = Mutex::new(Vec::new());

/// Nombre de fils du processus qui peuvent consommer du temps en même temps (au moins 1).
pub(crate) fn parallelism(p: &Process) -> u64 {
    let mut n = 0u64;
    p.for_each_thread_until(|_| {
        n += 1;
        false
    });
    n.min(cpu::cpu_count() as u64).max(1)
}

/// `(reste, intervalle)` en ns ; reste 0 = désarmé.
pub fn get(p: &Process, which: usize) -> (u64, u64) {
    let tm = p.cpu_timers.lock()[which];
    if tm.expires == 0 {
        return (0, tm.interval);
    }
    let now = p.cpu_ns();
    // Échue mais pas encore vue par la tâche : il reste « 1 µs », comme Linux.
    (tm.expires.saturating_sub(now).max(1000), tm.interval)
}

/// Un minuteur POSIX de temps processeur vient d'être armé : la tâche le surveille.
pub(crate) fn arm_posix(t: &Arc<crate::ptimer::PosixTimer>) {
    {
        let mut list = POSIX.lock();
        list.retain(|w| w.strong_count() > 0);
        if !list.iter().any(|w| w.as_ptr() == Arc::as_ptr(t)) {
            list.push(Arc::downgrade(t));
        }
    }
    start_and_kick();
}

fn start_and_kick() {
    STARTED.call_once(|| {
        if let Err(e) = spawn::new_task_builder(main, ()).name(alloc::string::String::from("maiproc_cputimer")).spawn() {
            log::error!("maiproc : tache des minuteurs de temps processeur non creee : {}", e);
        }
    });
    if !KICK.swap(true, Ordering::AcqRel) {
        QUEUE.wake_all();
    }
}

/// Arme (`value` > 0) ou désarme (`value` = 0) un minuteur.
pub fn set(p: &Arc<Process>, which: usize, value: u64, interval: u64) {
    let now = if value != 0 { p.cpu_ns() } else { 0 };
    p.cpu_timers.lock()[which] = CpuTimer { expires: if value == 0 { 0 } else { now + value }, interval };
    if value == 0 {
        return;
    }
    {
        let mut armed = ARMED.lock();
        armed.retain(|w| w.strong_count() > 0);
        if !armed.iter().any(|w| w.as_ptr() == Arc::as_ptr(p)) {
            armed.push(Arc::downgrade(p));
        }
    }
    start_and_kick();
}

/// Vérifie les minuteurs d'un processus ; rend le délai mural (ns) avant la prochaine
/// vérification utile, `None` s'il n'y a plus rien d'armé.
fn check(p: &Arc<Process>) -> Option<u64> {
    if p.is_zombie() {
        return None;
    }
    let now = p.cpu_ns();
    let mut fire = [false; 2];
    let mut soonest = u64::MAX;
    {
        let mut ts = p.cpu_timers.lock();
        for (i, tm) in ts.iter_mut().enumerate() {
            if tm.expires == 0 {
                continue;
            }
            if now >= tm.expires {
                fire[i] = true;
                if tm.interval == 0 {
                    tm.expires = 0;
                    continue;
                }
                // Période suivante ; les périodes manquées se fondent dans un seul signal (un
                // signal standard n'est mis en file qu'une fois), comme le dépassement de Linux.
                let missed = (now - tm.expires) / tm.interval;
                tm.expires += (missed + 1) * tm.interval;
            }
            soonest = soonest.min(tm.expires - now);
        }
    }
    if fire[VIRTUAL] {
        signal::send_to_process(p, SigInfo::new(SIGVTALRM, signal::SI_KERNEL));
    }
    if fire[PROF] {
        signal::send_to_process(p, SigInfo::new(SIGPROF, signal::SI_KERNEL));
    }
    (soonest != u64::MAX).then(|| soonest / parallelism(p))
}

fn main(_: ()) {
    loop {
        let list: Vec<Weak<Process>> = ARMED.lock().clone();
        let mut next: Option<u64> = None;
        let mut done: Vec<*const Process> = Vec::new();
        for w in &list {
            let Some(p) = w.upgrade() else { continue };
            match check(&p) {
                Some(d) => next = Some(next.map_or(d, |n| n.min(d))),
                None => done.push(Arc::as_ptr(&p)),
            }
        }
        if !done.is_empty() {
            // Revérifié sous le verrou : un `set` concurrent a pu réarmer l'un d'eux.
            ARMED.lock().retain(|w| match w.upgrade() {
                Some(p) => !done.contains(&Arc::as_ptr(&p)) || p.cpu_timers.lock().iter().any(|t| t.expires != 0),
                None => false,
            });
        }
        // Minuteurs POSIX (le verrou de la liste n'est pas tenu pendant les vérifications).
        let posix: Vec<_> = POSIX.lock().clone();
        let mut gone = false;
        for w in &posix {
            match w.upgrade().and_then(|pt| crate::ptimer::check_cpu(&pt)) {
                Some(d) => next = Some(next.map_or(d, |n| n.min(d))),
                None => gone = true,
            }
        }
        if gone {
            // Un `timer_settime` concurrent a pu réarmer : `arm_posix` le remettra en liste.
            POSIX.lock().retain(|w| w.upgrade().map_or(false, |pt| crate::ptimer::cpu_armed(&pt)));
        }
        // Au moins 200 µs entre deux passages (pas de tempête de réveils pour un minuteur très
        // court), au plus 1 s (filet de sécurité).
        let deadline = next.map(|ns| time::Instant::now() + Duration::from_nanos(ns.clamp(200_000, 1_000_000_000)));
        let _ = wait_event(&[&QUEUE], deadline, None, || KICK.swap(false, Ordering::AcqRel).then_some(()));
    }
}
