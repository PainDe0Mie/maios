//! Tubes nommés (`mkfifo`, `mknod S_IFIFO`).
//!
//! Le nœud du système de fichiers (tmpfs) ne porte rien : à l'ouverture, MaiProc cherche le tube
//! de cet inode (`(st_dev, st_ino)`) dans un registre, ou le crée. Le tube vit tant qu'une
//! extrémité est ouverte ; ensuite ses données disparaissent, comme sous Linux. Ouverture
//! (`fifo_open` de Linux) :
//! - lecture seule : attend l'arrivée d'un écrivain (pas en `O_NONBLOCK`) ;
//! - écriture seule : attend l'arrivée d'un lecteur ; en `O_NONBLOCK` sans lecteur : `ENXIO` ;
//! - lecture et écriture : jamais d'attente (le processus est son propre partenaire).
//!
//! L'attente porte sur le **compteur d'ouvertures** du partenaire, pas sur sa présence : un
//! écrivain qui ouvre puis ferme aussitôt libère quand même le lecteur qui attendait.

use alloc::{collections::BTreeMap, sync::Arc, sync::Weak};
use fs_node::FileRef;
use sync_preemption::PreemptionSafeMutex as Mutex;

use crate::fd::{Node, O_RDWR, O_WRONLY};
use crate::linux::errno::*;
use crate::pipe::{self, Pipe};
use crate::wait::{wait_event, WaitError};
use crate::Thread;

/// Tubes des tubes nommés ouverts, par `(st_dev, st_ino)`.
static FIFOS: Mutex<BTreeMap<(u64, u64), Weak<Pipe>>> = Mutex::new(BTreeMap::new());

fn pipe_of(key: (u64, u64)) -> Arc<Pipe> {
    let mut g = FIFOS.lock();
    if let Some(p) = g.get(&key).and_then(|w| w.upgrade()) {
        return p;
    }
    g.retain(|_, w| w.strong_count() > 0);
    let p = pipe::new_fifo();
    g.insert(key, Arc::downgrade(&p));
    p
}

/// Ouvre le tube nommé `file` ; `acc` : mode d'accès (`O_RDONLY`, `O_WRONLY`, `O_RDWR`).
pub fn open(t: &Thread, file: &FileRef, acc: u32, nonblock: bool) -> Result<Node, i64> {
    let m = file.lock().metadata().ok_or(ENXIO)?;
    let pipe = pipe_of((m.dev, m.ino));
    match acc {
        O_RDWR => Ok(Node::Pipe(pipe::open_end(&pipe, true, true, file.clone()))),
        O_WRONLY => {
            let (readers, _, r_seen, _) = pipe.ends();
            if nonblock && readers == 0 {
                return Err(ENXIO);
            }
            let end = pipe::open_end(&pipe, false, true, file.clone());
            if readers == 0 {
                wait_partner(t, &pipe, true, r_seen)?;
            }
            Ok(Node::Pipe(end))
        }
        _ => {
            let (_, writers, _, w_seen) = pipe.ends();
            let end = pipe::open_end(&pipe, true, false, file.clone());
            if writers == 0 && !nonblock {
                wait_partner(t, &pipe, false, w_seen)?;
            }
            Ok(Node::Pipe(end))
        }
    }
}

/// Attend qu'un partenaire (un lecteur si `want_reader`, sinon un écrivain) ait ouvert le tube
/// depuis que le compteur valait `seen`. Interrompue par un signal : `ERESTARTSYS` (l'extrémité
/// déjà ouverte est relâchée par l'appelant en abandonnant le `Node`).
fn wait_partner(t: &Thread, pipe: &Arc<Pipe>, want_reader: bool, seen: u64) -> Result<(), i64> {
    let q = if want_reader { &pipe.write_wait } else { &pipe.read_wait };
    match wait_event(&[q], None, Some(t), || {
        let (_, _, r, w) = pipe.ends();
        let now = if want_reader { r } else { w };
        (now != seen).then_some(())
    }) {
        Ok(()) => Ok(()),
        Err(WaitError::Interrupted) => Err(crate::signal::ERESTARTSYS),
        Err(WaitError::TimedOut) => Err(EAGAIN),
    }
}
