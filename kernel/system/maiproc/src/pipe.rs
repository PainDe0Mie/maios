//! Tubes (`pipe2`) : anneau de 64 Kio par défaut (`F_SETPIPE_SZ` jusqu'à 1 Mio), écritures
//! atomiques jusqu'à `PIPE_BUF` (4096 octets), fin de fichier quand le dernier écrivain ferme,
//! `SIGPIPE`/`EPIPE` quand le dernier lecteur a fermé, mode non bloquant, attentes
//! interruptibles par signal.
//!
//! Chaque extrémité est un objet ([`PipeEnd`]) porté par une description de fichier ouverte :
//! `dup` et `fork` partagent la description, et l'extrémité n'est fermée qu'avec la dernière
//! référence — la sémantique POSIX des descriptions de fichier.

use alloc::{collections::VecDeque, sync::Arc};
use core::sync::atomic::{AtomicU64, Ordering};
use sync_preemption::PreemptionSafeMutex as Mutex;

use crate::linux::errno::*;
use crate::wait::{wait_event, WaitError, WaitQueue};
use crate::Thread;

pub const PIPE_BUF: usize = 4096;
const DEFAULT_CAPACITY: usize = 64 * 1024;
const MAX_CAPACITY: usize = 1024 * 1024;

static NEXT_INO: AtomicU64 = AtomicU64::new(0x1000);

struct Inner {
    data: VecDeque<u8>,
    capacity: usize,
    readers: usize,
    writers: usize,
    /// Nombre d'ouvertures en lecture / en écriture depuis la création (tube nommé : l'ouverture
    /// attend l'*arrivée* d'un partenaire, pas sa présence, comme `wait_for_partner` de Linux —
    /// un partenaire venu et reparti entre-temps la libère aussi).
    r_counter: u64,
    w_counter: u64,
}

/// Tube.
pub struct Pipe {
    inner: Mutex<Inner>,
    /// Lecteurs qui attendent des données (ou la fin).
    pub read_wait: WaitQueue,
    /// Écrivains qui attendent de la place (ou la fermeture des lecteurs).
    pub write_wait: WaitQueue,
    pub ino: u64,
}

/// Extrémité d'un tube : lecture, écriture, ou les deux (tube nommé ouvert en `O_RDWR`).
pub struct PipeEnd {
    pub pipe: Arc<Pipe>,
    pub write: bool,
    pub read: bool,
    /// Tube nommé : le nœud du système de fichiers ouvert (pour `fstat`).
    pub file: Option<fs_node::FileRef>,
}

impl Drop for PipeEnd {
    fn drop(&mut self) {
        {
            let mut i = self.pipe.inner.lock();
            if self.write {
                i.writers -= 1;
            }
            if self.read {
                i.readers -= 1;
            }
        }
        // Les lecteurs voient la fin, les écrivains l'absence de lecteur.
        self.pipe.read_wait.wake_all();
        self.pipe.write_wait.wake_all();
    }
}

/// Crée un tube : (lecture, écriture).
pub fn new_pipe() -> (Arc<PipeEnd>, Arc<PipeEnd>) {
    let pipe = bare_pipe(1, 1);
    (
        Arc::new(PipeEnd { pipe: pipe.clone(), write: false, read: true, file: None }),
        Arc::new(PipeEnd { pipe, write: true, read: false, file: None }),
    )
}

fn bare_pipe(readers: usize, writers: usize) -> Arc<Pipe> {
    Arc::new(Pipe {
        inner: Mutex::new(Inner {
            data: VecDeque::new(),
            capacity: DEFAULT_CAPACITY,
            readers,
            writers,
            r_counter: readers as u64,
            w_counter: writers as u64,
        }),
        read_wait: WaitQueue::new(),
        write_wait: WaitQueue::new(),
        ino: NEXT_INO.fetch_add(1, Ordering::Relaxed),
    })
}

/// Tube d'un tube nommé, sans extrémité (elles viennent avec les ouvertures).
pub fn new_fifo() -> Arc<Pipe> {
    bare_pipe(0, 0)
}

/// Ouvre une extrémité de plus sur un tube nommé, et réveille les ouvertures qui attendaient un
/// partenaire de l'autre sorte.
pub fn open_end(pipe: &Arc<Pipe>, read: bool, write: bool, file: fs_node::FileRef) -> Arc<PipeEnd> {
    {
        let mut i = pipe.inner.lock();
        if read {
            i.readers += 1;
            i.r_counter += 1;
        }
        if write {
            i.writers += 1;
            i.w_counter += 1;
        }
    }
    if read {
        pipe.write_wait.wake_all();
    }
    if write {
        pipe.read_wait.wake_all();
    }
    Arc::new(PipeEnd { pipe: pipe.clone(), write, read, file: Some(file) })
}

fn interrupted(e: WaitError) -> i64 {
    match e {
        WaitError::Interrupted => crate::signal::ERESTARTSYS,
        WaitError::TimedOut => EAGAIN,
    }
}

impl Pipe {
    /// (lecteurs, écrivains, ouvertures en lecture, ouvertures en écriture).
    pub fn ends(&self) -> (usize, usize, u64, u64) {
        let i = self.inner.lock();
        (i.readers, i.writers, i.r_counter, i.w_counter)
    }

    /// Octets disponibles (`FIONREAD`).
    pub fn available(&self) -> usize { self.inner.lock().data.len() }

    pub fn capacity(&self) -> usize { self.inner.lock().capacity }

    /// `F_SETPIPE_SZ` : arrondi à la page supérieure (puissance de deux), au moins le contenu.
    pub fn set_capacity(&self, size: usize) -> Result<usize, i64> {
        if size > MAX_CAPACITY {
            return Err(EPERM);
        }
        let want = size.max(PIPE_BUF).next_power_of_two();
        let mut i = self.inner.lock();
        if want < i.data.len() {
            return Err(EBUSY);
        }
        i.capacity = want;
        drop(i);
        self.write_wait.wake_all();
        Ok(want)
    }

    /// État pour `poll` : (lisible, inscriptible, raccroché côté écriture, erreur côté lecture).
    pub fn poll_state(&self) -> (bool, bool, bool, bool) {
        let i = self.inner.lock();
        let readable = !i.data.is_empty() || i.writers == 0;
        let writable = i.readers > 0 && i.capacity - i.data.len() >= PIPE_BUF;
        (readable, writable, i.writers == 0, i.readers == 0)
    }
}

/// Lecture par le noyau, sans jamais attendre : `Ok(0)` à la fin (plus d'écrivain), `EAGAIN`
/// si le tube est vide. Pour s'en faire réveiller : observateur de `pipe.read_wait`.
pub fn kernel_read(end: &PipeEnd, kb: &mut [u8]) -> Result<usize, i64> {
    let pipe = &end.pipe;
    let n = {
        let mut i = pipe.inner.lock();
        if i.data.is_empty() {
            return if i.writers == 0 { Ok(0) } else { Err(EAGAIN) };
        }
        let n = kb.len().min(i.data.len());
        for (d, s) in kb.iter_mut().zip(i.data.drain(..n)) {
            *d = s;
        }
        n
    };
    pipe.write_wait.wake_all();
    Ok(n)
}

/// Écriture par le noyau, sans attente ni `SIGPIPE` : octets écrits (peut-être moins que
/// demandé), `EAGAIN` si le tube est plein, `EPIPE` sans lecteur. Pour s'en faire réveiller :
/// observateur de `pipe.write_wait`.
pub fn kernel_write(end: &PipeEnd, kb: &[u8]) -> Result<usize, i64> {
    if kb.is_empty() {
        return Ok(0);
    }
    let pipe = &end.pipe;
    let n = {
        let mut i = pipe.inner.lock();
        if i.readers == 0 {
            return Err(EPIPE);
        }
        let n = i.capacity.saturating_sub(i.data.len()).min(kb.len());
        if n == 0 {
            return Err(EAGAIN);
        }
        i.data.extend(&kb[..n]);
        n
    };
    pipe.read_wait.wake_all();
    Ok(n)
}

/// Lecture dans `kb` (au plus sa taille). Bloque tant que le tube est vide et a un écrivain.
pub fn read(t: &Thread, end: &PipeEnd, kb: &mut [u8], nonblock: bool) -> Result<usize, i64> {
    if kb.is_empty() {
        return Ok(0);
    }
    let pipe = &end.pipe;
    let got = wait_event(&[&pipe.read_wait], None, Some(t), || {
        let mut i = pipe.inner.lock();
        if !i.data.is_empty() {
            let n = kb.len().min(i.data.len());
            for (d, s) in kb.iter_mut().zip(i.data.drain(..n)) {
                *d = s;
            }
            return Some(Ok(n));
        }
        if i.writers == 0 {
            return Some(Ok(0));
        }
        if nonblock {
            return Some(Err(EAGAIN));
        }
        None
    });
    let r = match got {
        Ok(r) => r,
        Err(e) => Err(interrupted(e)),
    };
    if matches!(r, Ok(n) if n > 0) {
        pipe.write_wait.wake_all();
    }
    r
}

/// Écriture de `kb`. Une écriture de `PIPE_BUF` octets ou moins est atomique ; au-delà, elle peut
/// être découpée. Bloque tant qu'il manque de la place (sauf `nonblock`).
pub fn write(t: &Thread, end: &PipeEnd, kb: &[u8], nonblock: bool) -> Result<usize, i64> {
    if kb.is_empty() {
        return Ok(0);
    }
    let pipe = &end.pipe;
    let atomic = kb.len() <= PIPE_BUF;
    let mut done = 0usize;
    while done < kb.len() {
        let step = wait_event(&[&pipe.write_wait], None, Some(t), || {
            let mut i = pipe.inner.lock();
            if i.readers == 0 {
                return Some(Err(EPIPE));
            }
            let free = i.capacity.saturating_sub(i.data.len());
            let rest = kb.len() - done;
            let need = if atomic { rest } else { 1 };
            if free >= need {
                let n = free.min(rest);
                i.data.extend(&kb[done..done + n]);
                return Some(Ok(n));
            }
            if nonblock {
                return Some(Err(EAGAIN));
            }
            None
        });
        match step {
            Ok(Ok(n)) => {
                done += n;
                pipe.read_wait.wake_all();
            }
            Ok(Err(EPIPE)) => {
                // SIGPIPE au fil qui écrit (`EPIPE` si le signal est ignoré ou géré).
                if let Some(me) = crate::current_thread_arc(t) {
                    crate::signal::send_to_thread(&me, crate::signal::SigInfo::from_process(crate::signal::SIGPIPE, crate::signal::SI_USER, &t.process, &t.process), false);
                }
                return if done > 0 { Ok(done) } else { Err(EPIPE) };
            }
            Ok(Err(e)) => return if done > 0 { Ok(done) } else { Err(e) },
            Err(e) => return if done > 0 { Ok(done) } else { Err(interrupted(e)) },
        }
    }
    Ok(done)
}
