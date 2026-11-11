//! Terminal du noyau (`Node::Console`) vu par les processus Linux : lectures qui dorment,
//! `O_NONBLOCK`, `poll`/`select`/`epoll` exacts.
//!
//! L'entrée d'un terminal est une file de `stdio` remplie par l'application terminal. Une lecture
//! ne l'attend jamais en boucle : elle essaie sans attendre ([`app_io::ImmutableRead::try_read`]),
//! sinon s'endort sur [`CONSOLE_WQ`] après avoir inscrit [`waker`] dans la file, que la prochaine
//! écriture (ou la fin de fichier) réveille — interruptible par les signaux, comme `n_tty_read`.
//! Une entrée qui ne sait pas lire sans attendre (terminal série `tty::Slave`, qui bloque déjà par
//! l'ordonnanceur) garde sa lecture bloquante et se dit toujours prête.

use alloc::sync::Arc;
use alloc::task::Wake;
use core::task::Waker;

use app_io::ImmutableRead;

use crate::linux::errno::*;
use crate::signal;
use crate::wait::{wait_event, WaitError, WaitQueue};
use crate::Thread;

/// Lecteurs et observateurs (`poll`, `epoll`) des terminaux.
pub static CONSOLE_WQ: WaitQueue = WaitQueue::new();

struct ConsoleWaker;

impl Wake for ConsoleWaker {
    fn wake(self: Arc<Self>) {
        CONSOLE_WQ.wake_all();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        CONSOLE_WQ.wake_all();
    }
}

static WAKER: spin::Once<Waker> = spin::Once::new();

/// Le *waker* inscrit dans les files d'entrée (un seul pour tous : il réveille [`CONSOLE_WQ`]).
pub fn waker() -> &'static Waker {
    WAKER.call_once(|| Waker::from(Arc::new(ConsoleWaker)))
}

/// `read` sur un terminal.
pub fn read(t: &Thread, input: &Arc<dyn ImmutableRead>, kb: &mut [u8], nonblock: bool) -> Result<usize, i64> {
    if !input.nonblocking() {
        return input.read(kb).map_err(|_| EIO);
    }
    if kb.is_empty() {
        return Ok(0);
    }
    if let Some(n) = input.try_read(kb) {
        return Ok(n);
    }
    if nonblock {
        return Err(EAGAIN);
    }
    let r = wait_event(&[&CONSOLE_WQ], None, Some(t), || {
        if let Some(n) = input.try_read(kb) {
            return Some(Ok(n));
        }
        // Inscrit avant la dernière vérification : une écriture entre les deux réveille.
        if input.poll_read(Some(waker())) {
            if let Some(n) = input.try_read(kb) {
                return Some(Ok(n));
            }
        }
        None
    });
    match r {
        Ok(v) => v,
        Err(WaitError::Interrupted) => Err(signal::ERESTARTSYS),
        Err(WaitError::TimedOut) => Err(EAGAIN),
    }
}

/// Entrée prête (octets ou fin de fichier) ; inscrit le *waker* pour le prochain changement.
pub fn readable(input: &Arc<dyn ImmutableRead>) -> bool {
    !input.nonblocking() || input.poll_read(Some(waker()))
}
