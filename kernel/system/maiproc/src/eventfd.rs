//! `eventfd` : compteur de 64 bits partagé, pour les notifications entre fils, processus et
//! boucles d'événements (`epoll`). Mode sémaphore (`EFD_SEMAPHORE`) : chaque lecture rend 1.

use alloc::{format, string::String, sync::Arc};
use sync_preemption::PreemptionSafeMutex as Mutex;

use crate::fd::{Node, OpenFile, O_NONBLOCK, O_RDWR};
use crate::linux::errno::*;
use crate::linux::SysResult;
use crate::object::{ev, new_ino, object_any, wait_errno, FileObject};
use crate::wait::{wait_event, WaitQueue};
use crate::Thread;

const EFD_SEMAPHORE: u64 = 1;
const EFD_NONBLOCK: u64 = 0x800;
const EFD_CLOEXEC: u64 = 0x80000;
/// Valeur maximale du compteur.
const MAX: u64 = u64::MAX - 1;

pub struct EventFd {
    count: Mutex<u64>,
    semaphore: bool,
    wq: WaitQueue,
    ino: u64,
}

impl EventFd {
    /// Ajoute `n` au compteur sans attendre (notifications du noyau : io_uring), borné à la
    /// valeur maximale comme `eventfd_signal`.
    pub fn signal(&self, n: u64) {
        {
            let mut c = self.count.lock();
            *c = c.saturating_add(n).min(MAX);
        }
        self.wq.wake_all();
    }
}

impl FileObject for EventFd {
    object_any!();

    fn link_name(&self) -> String {
        String::from("anon_inode:[eventfd]")
    }

    fn stat(&self) -> (u32, u64) {
        (0o600, self.ino)
    }

    fn read(&self, t: &Thread, kb: &mut [u8], nonblock: bool) -> Result<usize, i64> {
        if kb.len() < 8 {
            return Err(EINVAL);
        }
        let v = wait_event(&[&self.wq], None, Some(t), || {
            let mut c = self.count.lock();
            if *c > 0 {
                let v = if self.semaphore { 1 } else { *c };
                *c -= v;
                return Some(Ok(v));
            }
            nonblock.then_some(Err(EAGAIN))
        })
        .map_err(wait_errno)??;
        kb[..8].copy_from_slice(&v.to_le_bytes());
        // De la place pour les écrivains bloqués.
        self.wq.wake_all();
        Ok(8)
    }

    fn write(&self, t: &Thread, kb: &[u8], nonblock: bool) -> Result<usize, i64> {
        if kb.len() < 8 {
            return Err(EINVAL);
        }
        let add = u64::from_le_bytes(kb[..8].try_into().unwrap());
        if add == u64::MAX {
            return Err(EINVAL);
        }
        wait_event(&[&self.wq], None, Some(t), || {
            let mut c = self.count.lock();
            if MAX - *c >= add {
                *c += add;
                return Some(Ok(()));
            }
            nonblock.then_some(Err(EAGAIN))
        })
        .map_err(wait_errno)??;
        if add > 0 {
            self.wq.wake_all();
        }
        Ok(8)
    }

    fn poll(&self, _t: Option<&Thread>) -> u32 {
        let c = *self.count.lock();
        let mut r = 0;
        if c > 0 {
            r |= ev::READABLE;
        }
        if c < MAX {
            r |= ev::WRITABLE;
        }
        r
    }

    fn queues<'a>(&'a self, cb: &mut dyn FnMut(&'a WaitQueue)) {
        cb(&self.wq);
    }

    fn fdinfo(&self) -> String {
        format!("eventfd-count: {:16x}\neventfd-id: {}\neventfd-semaphore: {}\n", *self.count.lock(), self.ino, self.semaphore as u8)
    }
}

/// `eventfd2(initval, flags)` (et `eventfd` avec `flags = 0`).
pub fn eventfd2(t: &Thread, initval: u64, flags: u64) -> SysResult {
    if flags & !(EFD_SEMAPHORE | EFD_NONBLOCK | EFD_CLOEXEC) != 0 {
        return Err(EINVAL);
    }
    let obj = Arc::new(EventFd { count: Mutex::new(initval as u32 as u64), semaphore: flags & EFD_SEMAPHORE != 0, wq: WaitQueue::new(), ino: new_ino() });
    let status = O_RDWR | if flags & EFD_NONBLOCK != 0 { O_NONBLOCK } else { 0 };
    let file = OpenFile::new(Node::Object(obj), status);
    t.process.files.lock().alloc(file, 0, flags & EFD_CLOEXEC != 0).map(|fd| fd as i64).ok_or(EMFILE)
}
