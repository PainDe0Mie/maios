//! `signalfd` : signaux reçus en lisant un descripteur (structures `signalfd_siginfo`), pour les
//! boucles d'événements (`systemd`, `libuv`, Wayland…). Les signaux lus sont retirés des files du
//! fil qui lit et de son processus, comme `rt_sigtimedwait` ; ils doivent être bloqués pour ne
//! pas être livrés avant.

use alloc::{format, string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};

use crate::fd::{Node, OpenFile, O_NONBLOCK, O_RDWR};
use crate::linux::errno::*;
use crate::linux::fs::get_fd;
use crate::linux::{fault, SysResult};
use crate::object::{ev, new_ino, object_any, wait_errno, FileObject};
use crate::signal::{self, SigInfo};
use crate::wait::{wait_event, WaitQueue};
use crate::Thread;

const SFD_NONBLOCK: u64 = 0x800;
const SFD_CLOEXEC: u64 = 0x80000;
/// Taille d'un `struct signalfd_siginfo`.
const RECORD: usize = 128;

pub struct SignalFd {
    mask: AtomicU64,
    /// File de signaux du processus créateur (les signaux d'un autre processus n'y sont pas).
    process: Arc<crate::Process>,
    ino: u64,
}

impl SignalFd {
    fn pending(&self, t: &Thread) -> bool {
        let m = self.mask.load(Ordering::Acquire);
        t.pending.lock().mask & m != 0 || t.process.shared_pending.lock().mask & m != 0
    }
}

/// `siginfo_t` → `signalfd_siginfo`.
fn record(info: &SigInfo) -> [u8; RECORD] {
    let mut r = [0u8; RECORD];
    let signo = info.signo();
    let put32 = |r: &mut [u8; RECORD], off: usize, v: u32| r[off..off + 4].copy_from_slice(&v.to_le_bytes());
    let put64 = |r: &mut [u8; RECORD], off: usize, v: u64| r[off..off + 8].copy_from_slice(&v.to_le_bytes());
    put32(&mut r, 0, signo as u32);
    put32(&mut r, 4, info.get_i32(4) as u32); // ssi_errno
    put32(&mut r, 8, info.get_i32(8) as u32); // ssi_code
    match signo {
        signal::SIGCHLD => {
            put32(&mut r, 12, info.get_i32(16) as u32); // ssi_pid
            put32(&mut r, 16, info.get_i32(20) as u32); // ssi_uid
            put32(&mut r, 40, info.get_i32(24) as u32); // ssi_status
            put64(&mut r, 56, info.get_u64(32)); // ssi_utime
            put64(&mut r, 64, info.get_u64(40)); // ssi_stime
        }
        signal::SIGSEGV | signal::SIGBUS | signal::SIGILL | signal::SIGFPE | signal::SIGTRAP => {
            put64(&mut r, 72, info.get_u64(16)); // ssi_addr
        }
        _ => {
            put32(&mut r, 12, info.get_i32(16) as u32); // ssi_pid (ou si_tid d'un minuteur)
            put32(&mut r, 16, info.get_i32(20) as u32); // ssi_uid (ou si_overrun)
            put32(&mut r, 44, info.get_i32(24) as u32); // ssi_int
            put64(&mut r, 48, info.get_u64(24)); // ssi_ptr
        }
    }
    r
}

impl FileObject for SignalFd {
    object_any!();

    fn link_name(&self) -> String {
        String::from("anon_inode:[signalfd]")
    }

    fn stat(&self) -> (u32, u64) {
        (0o600, self.ino)
    }

    fn read(&self, t: &Thread, kb: &mut [u8], nonblock: bool) -> Result<usize, i64> {
        let max = kb.len() / RECORD;
        if max == 0 {
            return Err(EINVAL);
        }
        let got: Vec<SigInfo> = wait_event(&[&self.process.signal_wait], None, Some(t), || {
            let m = self.mask.load(Ordering::Acquire);
            let mut v = Vec::new();
            while v.len() < max {
                match signal::dequeue_from(t, m) {
                    Some(i) => v.push(i),
                    None => break,
                }
            }
            if !v.is_empty() {
                return Some(Ok(v));
            }
            nonblock.then_some(Err(EAGAIN))
        })
        .map_err(wait_errno)??;
        for (i, info) in got.iter().enumerate() {
            kb[i * RECORD..(i + 1) * RECORD].copy_from_slice(&record(info));
        }
        Ok(got.len() * RECORD)
    }

    fn poll(&self, t: Option<&Thread>) -> u32 {
        match t {
            Some(t) if self.pending(t) => ev::READABLE,
            Some(_) => 0,
            // Sans fil (inscription dans `epoll`) : la file du processus seule.
            None => {
                if self.process.shared_pending.lock().mask & self.mask.load(Ordering::Acquire) != 0 {
                    ev::READABLE
                } else {
                    0
                }
            }
        }
    }

    fn queues<'a>(&'a self, cb: &mut dyn FnMut(&'a WaitQueue)) {
        cb(&self.process.signal_wait);
    }

    fn fdinfo(&self) -> String {
        format!("sigmask:\t{:016x}\n", self.mask.load(Ordering::Relaxed))
    }
}

/// `signalfd4(fd, mask, sizemask, flags)` ; `fd = -1` crée, sinon change le masque.
pub fn signalfd4(t: &Thread, fd: u64, mask_ptr: u64, sizemask: u64, flags: u64) -> SysResult {
    if sizemask != 8 || flags & !(SFD_NONBLOCK | SFD_CLOEXEC) != 0 {
        return Err(EINVAL);
    }
    let mask = signal::sanitize_mask(t.mm().read_user::<u64>(mask_ptr as usize).map_err(fault)?);
    if fd as i32 != -1 {
        let f = get_fd(t, fd)?;
        let s = f.object_as::<SignalFd>().ok_or(EINVAL)?;
        s.mask.store(mask, Ordering::Release);
        return Ok(fd as i64);
    }
    let obj = Arc::new(SignalFd { mask: AtomicU64::new(mask), process: t.process.clone(), ino: new_ino() });
    let status = O_RDWR | if flags & SFD_NONBLOCK != 0 { O_NONBLOCK } else { 0 };
    let file = OpenFile::new(Node::Object(obj), status);
    t.process.files.lock().alloc(file, 0, flags & SFD_CLOEXEC != 0).map(|fd| fd as i64).ok_or(EMFILE)
}
