//! `pidfd` : descripteur stable d'un processus (Linux 5.3+). Contrairement à un pid, il ne peut
//! pas désigner un autre processus après la fin du sien : `pidfd_send_signal` vise toujours le
//! bon processus, et le descripteur devient lisible (`poll`, `epoll`) à sa fin.

use alloc::{format, string::String, sync::Arc};

use crate::fd::{Node, OpenFile, O_NONBLOCK, O_RDWR};
use crate::linux::errno::*;
use crate::linux::fs::get_fd;
use crate::linux::{fault, SysResult};
use crate::object::{ev, new_ino, object_any, FileObject};
use crate::signal::{self, SigInfo};
use crate::wait::WaitQueue;
use crate::{find_process_v, Process, Thread};

const PIDFD_NONBLOCK: u64 = 0x800;

pub struct PidFd {
    pub process: Arc<Process>,
    ino: u64,
}

impl FileObject for PidFd {
    object_any!();

    fn link_name(&self) -> String {
        String::from("anon_inode:[pidfd]")
    }

    fn stat(&self) -> (u32, u64) {
        (0o600, self.ino)
    }

    fn poll(&self, _t: Option<&Thread>) -> u32 {
        if self.process.is_zombie() { ev::READABLE | ev::HUP } else { 0 }
    }

    fn queues<'a>(&'a self, cb: &mut dyn FnMut(&'a WaitQueue)) {
        cb(&self.process.exit_wait);
    }

    /// `Pid:` vu de l'espace pid du lecteur (−1 si le processus est recueilli ou invisible),
    /// `NSpid:` de cet espace jusqu'à celui du processus (`pidfd_show_fdinfo`).
    fn fdinfo(&self) -> String {
        let viewer = crate::current_process().map_or_else(crate::pidns::init_pid_ns, |c| c.pid_ns.clone());
        match self.process.pid_ref() {
            Some(r) if viewer.sees(self.process.pid) => {
                let list = r.numbers_from(&viewer).iter().map(|n| format!("{}", n)).collect::<alloc::vec::Vec<_>>().join("\t");
                format!("Pid:\t{}\nNSpid:\t{}\n", viewer.nr_of(self.process.pid), list)
            }
            _ => String::from("Pid:\t-1\nNSpid:\t-1\n"),
        }
    }
}

/// Processus désigné par un `pidfd`.
pub fn process_of(t: &Thread, pidfd: u64) -> Result<Arc<Process>, i64> {
    let f = get_fd(t, pidfd)?;
    f.object_as::<PidFd>().map(|p| p.process.clone()).ok_or(EBADF)
}

/// Cible de `pidfd_send_signal` : un pidfd, ou un dossier `/proc/<pid>` ouvert (Linux 5.1).
fn signal_target(t: &Thread, fd: u64) -> Result<Arc<Process>, i64> {
    let f = get_fd(t, fd)?;
    if let Some(p) = f.object_as::<PidFd>() {
        return Ok(p.process.clone());
    }
    if let Node::VirtDir { path, .. } = &f.node {
        if let Some(pid) = path.strip_prefix("/proc/").and_then(|r| r.parse::<usize>().ok()) {
            return find_process_v(t, pid).ok_or(ESRCH);
        }
    }
    Err(EBADF)
}

pub fn pidfd_open(t: &Thread, pid: u64, flags: u64) -> SysResult {
    if flags & !PIDFD_NONBLOCK != 0 || pid as i32 <= 0 {
        return Err(EINVAL);
    }
    let p = find_process_v(t, pid as usize).ok_or(ESRCH)?;
    install(t, p, flags)
}

/// Nouveau pidfd de `p` dans la table de `t` (`pidfd_open`, `CLONE_PIDFD`) ; toujours
/// `O_CLOEXEC`, comme Linux.
pub(crate) fn install(t: &Thread, p: Arc<Process>, flags: u64) -> SysResult {
    let obj = Arc::new(PidFd { process: p, ino: new_ino() });
    let status = O_RDWR | if flags & PIDFD_NONBLOCK != 0 { O_NONBLOCK } else { 0 };
    let file = OpenFile::new(Node::Object(obj), status);
    t.process.files.lock().alloc(file, 0, true).map(|fd| fd as i64).ok_or(EMFILE)
}

pub fn pidfd_send_signal(t: &Thread, pidfd: u64, sig: u64, uinfo: u64, flags: u64) -> SysResult {
    if flags != 0 {
        return Err(EINVAL);
    }
    let p = signal_target(t, pidfd)?;
    let sig = sig as i32;
    if !(0..=64).contains(&sig) {
        return Err(EINVAL);
    }
    if p.is_zombie() {
        return Err(ESRCH);
    }
    if !crate::linux::proc::kill_permitted(&t.process, &p, sig) {
        return Err(EPERM);
    }
    if sig == 0 {
        return Ok(0);
    }
    let info = if uinfo != 0 {
        let mut i = SigInfo { bytes: [0; 128] };
        t.mm().copy_from_user(&mut i.bytes, uinfo as usize).map_err(fault)?;
        if i.signo() != sig {
            return Err(EINVAL);
        }
        i
    } else {
        SigInfo::from_process(sig, signal::SI_USER, &t.process, &p)
    };
    let outside = !p.pid_ns.sees(t.process.pid);
    signal::send_to_process_as(&p, info, outside);
    Ok(0)
}

/// `pidfd_getfd` : copie d'un descripteur d'un autre processus (même description ouverte).
pub fn pidfd_getfd(t: &Thread, pidfd: u64, targetfd: u64, flags: u64) -> SysResult {
    if flags != 0 {
        return Err(EINVAL);
    }
    let p = process_of(t, pidfd)?;
    if p.is_zombie() {
        return Err(ESRCH);
    }
    let f = p.files.lock().get(targetfd as usize).ok_or(EBADF)?;
    t.process.files.lock().alloc(f, 0, true).map(|fd| fd as i64).ok_or(EMFILE)
}
