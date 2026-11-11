//! État « prêt » d'une description de fichier ouverte, commun à `poll`, `select` et `epoll`.

use crate::fd::{Node, OpenFile};
use crate::object::ev;
use crate::wait::WaitQueue;
use crate::Thread;

/// Événements prêts sur `f` (non filtrés : l'appelant garde ceux qu'il demande, plus `ERR` et
/// `HUP` toujours rapportés).
pub fn poll_file(f: &OpenFile, t: Option<&Thread>) -> u32 {
    match &f.node {
        Node::Pipe(end) => {
            let (readable, writable, no_writers, no_readers) = end.pipe.poll_state();
            let mut r = 0;
            if end.write {
                if writable {
                    r |= ev::WRITABLE;
                }
                if no_readers && !end.read {
                    r |= ev::ERR;
                }
            }
            if end.read {
                if readable && end.pipe.available() > 0 {
                    r |= ev::READABLE;
                }
                if no_writers && !end.write {
                    r |= ev::HUP;
                }
            }
            r
        }
        Node::Object(o) => o.poll(t),
        // Terminal : prêt en lecture s'il a des octets (ou sa fin) ; toujours prêt en écriture.
        Node::Console { input, output, .. } => {
            let mut r = 0;
            if f.readable() && input.as_ref().map_or(false, crate::console::readable) {
                r |= ev::READABLE;
            }
            if f.writable() && output.is_some() {
                r |= ev::WRITABLE;
            }
            r
        }
        // Fichiers, /dev : toujours prêts.
        _ => {
            let mut r = 0;
            if f.readable() {
                r |= ev::READABLE;
            }
            if f.writable() {
                r |= ev::WRITABLE;
            }
            r
        }
    }
}

/// Files d'attente réveillées quand l'état de `f` change (aucune : toujours prêt).
pub fn for_each_queue<'a>(f: &'a OpenFile, cb: &mut dyn FnMut(&'a WaitQueue)) {
    match &f.node {
        Node::Pipe(e) => {
            if e.write {
                cb(&e.pipe.write_wait);
            }
            if e.read {
                cb(&e.pipe.read_wait);
            }
        }
        Node::Object(o) => o.queues(cb),
        Node::Console { input: Some(_), .. } => cb(&crate::console::CONSOLE_WQ),
        _ => {}
    }
}
