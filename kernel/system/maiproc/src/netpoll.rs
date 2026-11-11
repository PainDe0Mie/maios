//! Tâche des minuteurs du réseau.
//!
//! La réception est déjà menée par l'interruption de la carte (tâche différée de `e1000`, qui
//! sonde l'interface : le modèle de Mogul et Ramakrishnan, *Eliminating Receive Livelock*, 1997,
//! que NAPI a repris). Ce qui manquait, ce sont les **minuteurs** : retransmission, ACK différé,
//! sondes de fenêtre nulle, `TIME_WAIT`. Cette tâche dort jusqu'à la prochaine échéance annoncée
//! par smoltcp (`poll_delay`), sans tick périodique ; un envoi qui arme un minuteur plus proche
//! la réveille ([`kick`]).
//!
//! Elle ramasse aussi les connexions fermées par leur processus : comme dans Linux, un `close`
//! n'abandonne pas la connexion, qui finit sa fermeture (FIN, `TIME_WAIT`) sans propriétaire.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use core::time::Duration;
use net::{tcp, NetworkInterface, SocketHandle};
use sync_preemption::PreemptionSafeMutex as Mutex;

use crate::wait::{wait_event, WaitQueue};

static STARTED: spin::Once<()> = spin::Once::new();
static KICK: AtomicBool = AtomicBool::new(false);
static QUEUE: WaitQueue = WaitQueue::new();
/// Sockets de smoltcp ouvertes par des processus (vivantes ou en fermeture).
pub static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// Connexion fermée par son processus, en fin de fermeture.
struct Grave {
    iface: Arc<NetworkInterface>,
    h: SocketHandle,
    /// Au-delà, la connexion est abandonnée (`RST`) : `tcp_fin_timeout` + marge.
    deadline: time::Instant,
}

static GRAVES: Mutex<Vec<Grave>> = Mutex::new(Vec::new());

/// Démarre la tâche au premier usage.
pub fn ensure_started() {
    STARTED.call_once(|| {
        if let Err(e) = spawn::new_task_builder(main, ()).name(alloc::string::String::from("maiproc_netpoll")).spawn() {
            log::error!("maiproc : tâche des minuteurs réseau non créée : {}", e);
        }
    });
}

/// Réveille la tâche : un minuteur plus proche a peut-être été armé.
pub fn kick() {
    if !KICK.swap(true, Ordering::AcqRel) {
        QUEUE.wake_all();
    }
}

/// Confie une connexion fermée à la tâche.
pub fn bury(iface: Arc<NetworkInterface>, h: SocketHandle) {
    GRAVES.lock().push(Grave { iface, h, deadline: time::Instant::now() + Duration::from_secs(90) });
    kick();
}

fn reap() {
    let graves = core::mem::take(&mut *GRAVES.lock());
    let now = time::Instant::now();
    let mut keep = Vec::new();
    for g in graves {
        let done = {
            let mut set = g.iface.sockets.lock();
            let s = set.get_mut::<tcp::Socket>(g.h);
            if now >= g.deadline {
                s.abort();
            }
            matches!(s.state(), tcp::State::Closed)
        };
        if done {
            g.iface.sockets.lock().remove(g.h);
            ACTIVE.fetch_sub(1, Ordering::AcqRel);
        } else {
            keep.push(g);
        }
    }
    if !keep.is_empty() {
        GRAVES.lock().extend(keep);
    }
}

fn main(_: ()) {
    loop {
        let active = ACTIVE.load(Ordering::Acquire) > 0;
        // Avec des sockets ouvertes : au plus 10 ms entre deux passages (mode hybride de NAPI :
        // la réception est menée par l'interruption quand elle arrive, par ce sondage sinon) ;
        // sans socket de processus, un passage par seconde suffit.
        let mut next = if active { Duration::from_millis(10) } else { Duration::from_secs(1) };
        let ifaces: Vec<Arc<NetworkInterface>> = net::get_interfaces().lock().clone();
        for i in &ifaces {
            i.poll();
            if let Some(d) = i.poll_delay() {
                next = next.min(d);
            }
        }
        reap();
        let deadline = time::Instant::now() + next.max(Duration::from_micros(200));
        let _ = wait_event(&[&QUEUE], Some(deadline), None, || KICK.swap(false, Ordering::AcqRel).then_some(()));
    }
}
