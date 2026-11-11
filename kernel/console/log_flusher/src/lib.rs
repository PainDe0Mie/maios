//! Tâche de vidage du journal asynchrone (`log_flusher`).
//!
//! Une fois lancée par [`start`], les `warn!`/`info!` ne font plus qu'une copie dans l'anneau du
//! logger ; cette tâche les émet sur le port série sans jamais attendre la ligne :
//!
//! * [`logger::drain`] émet une rafale (FIFO du 16550 quand il est vide) et rend `Busy` si le
//!   périphérique est occupé : on dort alors le temps que le FIFO se vide (16 octets à 115 200
//!   bauds ≈ 1,4 ms) au lieu de tourner interruptions coupées ;
//! * anneau vide : la tâche dort sur une sonnette. La sonnerie ([`kick`]) est sûre en
//!   interruption et ne coûte qu'un échange atomique tant que la tâche est déjà réveillée ;
//! * attente selon la règle de MaiSched : se bloquer, **puis** revérifier (sonnette et anneau).
//!
//! Les journaux du démarrage restent synchrones : `captain` ne lance cette tâche qu'à la fin de
//! son initialisation, si bien qu'un blocage pendant le démarrage ne laisse rien dans l'anneau.

#![no_std]

extern crate alloc;

use core::{sync::atomic::{AtomicBool, Ordering}, time::Duration};
use spin::Once;
use task::TaskRef;

static RUNG: AtomicBool = AtomicBool::new(false);
static TASK: Once<TaskRef> = Once::new();

/// Sonnette : réveille la tâche si elle ne l'était pas déjà. Ne journalise jamais.
fn kick() {
    if !RUNG.swap(true, Ordering::AcqRel) {
        if let Some(t) = TASK.get() {
            let _ = t.unblock();
        }
    }
}

/// Lance la tâche de vidage ; le journal passe en asynchrone dès qu'elle tourne.
pub fn start() -> Result<(), &'static str> {
    spawn::new_task_builder(flusher, ())
        .name(alloc::string::String::from("log_flusher"))
        .spawn()?;
    Ok(())
}

fn flusher(_: ()) -> isize {
    let me = match task::get_my_current_task() {
        Some(t) => t,
        None => return -1,
    };
    TASK.call_once(|| me.clone());
    logger::start_async(kick);
    loop {
        match logger::drain() {
            logger::Drain::Progress => {}
            logger::Drain::Busy => {
                let _ = sleep::sleep(Duration::from_micros(500));
            }
            logger::Drain::Empty => wait(&me),
        }
    }
}

/// Dort jusqu'à la prochaine sonnerie. Peut revenir plus tôt : la boucle revérifie l'anneau.
fn wait(me: &TaskRef) {
    if RUNG.swap(false, Ordering::AcqRel) {
        return;
    }
    if me.block().is_err() {
        return;
    }
    if RUNG.swap(false, Ordering::AcqRel) || logger::pending() {
        let _ = me.unblock();
        return;
    }
    task::schedule();
}
