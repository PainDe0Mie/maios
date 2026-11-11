//! `/proc/maios/strace` : journal des appels système d'un programme, l'équivalent de
//! `strace -f` vu du noyau (sans `ptrace`, sans arrêter le traçé).
//!
//! ```text
//! echo "errors firefox" > /proc/maios/strace   # appels en échec des processus « *firefox* »
//! echo "all wlsub" > /proc/maios/strace        # tous les appels
//! echo "net firefox" > /proc/maios/strace      # appels de sockets (adresses, options)
//! echo off > /proc/maios/strace
//! ```
//!
//! Le motif est cherché dans le nom du processus (`comm`, 15 octets comme Linux) ; les fils et
//! les enfants qui gardent ce nom sont suivis. Chaque ligne va au journal du noyau
//! (`warn!`) : `strace <pid>/<tid> <nom>(<arguments>) = <résultat>`, avec le chemin pour les
//! appels qui en prennent un. En mode `errors`, `EAGAIN` (attente non bloquante) est tu.
//! Éteint, le coût est une lecture atomique par appel.

use alloc::{format, string::String, sync::Arc};
use core::sync::atomic::{AtomicU8, Ordering};
use sync_preemption::PreemptionSafeMutex as Mutex;

use crate::linux::errno::*;
use crate::object::{object_any, FileObject};
use crate::wait::WaitQueue;
use crate::Thread;

const OFF: u8 = 0;
const ERRORS: u8 = 1;
const ALL: u8 = 2;
/// Appels réseau seulement (réussis ou non).
const NET: u8 = 3;

static MODE: AtomicU8 = AtomicU8::new(OFF);
static PATTERN: Mutex<String> = Mutex::new(String::new());

#[inline]
pub fn enabled() -> bool {
    MODE.load(Ordering::Relaxed) != OFF
}

fn matches(t: &Thread) -> bool {
    let comm = *t.process.comm.lock();
    let len = comm.iter().position(|&b| b == 0).unwrap_or(comm.len());
    let name = core::str::from_utf8(&comm[..len]).unwrap_or("");
    let pat = PATTERN.lock();
    pat.is_empty() || name.contains(pat.as_str())
}

/// Nom d'un appel système (les plus courants ; les autres par leur numéro).
fn name(nr: u64) -> Option<&'static str> {
    Some(match nr {
        0 => "read",
        1 => "write",
        2 => "open",
        3 => "close",
        4 => "stat",
        5 => "fstat",
        6 => "lstat",
        7 => "poll",
        8 => "lseek",
        9 => "mmap",
        10 => "mprotect",
        11 => "munmap",
        12 => "brk",
        13 => "rt_sigaction",
        14 => "rt_sigprocmask",
        16 => "ioctl",
        17 => "pread64",
        19 => "readv",
        20 => "writev",
        21 => "access",
        22 => "pipe",
        25 => "mremap",
        28 => "madvise",
        32 => "dup",
        33 => "dup2",
        39 => "getpid",
        41 => "socket",
        42 => "connect",
        43 => "accept",
        44 => "sendto",
        45 => "recvfrom",
        46 => "sendmsg",
        47 => "recvmsg",
        49 => "bind",
        50 => "listen",
        51 => "getsockname",
        52 => "getpeername",
        53 => "socketpair",
        54 => "setsockopt",
        55 => "getsockopt",
        56 => "clone",
        57 => "fork",
        59 => "execve",
        61 => "wait4",
        62 => "kill",
        63 => "uname",
        72 => "fcntl",
        77 => "ftruncate",
        79 => "getcwd",
        80 => "chdir",
        81 => "fchdir",
        82 => "rename",
        83 => "mkdir",
        84 => "rmdir",
        87 => "unlink",
        89 => "readlink",
        90 => "chmod",
        95 => "umask",
        97 => "getrlimit",
        99 => "sysinfo",
        102 => "getuid",
        104 => "getgid",
        105 => "setuid",
        106 => "setgid",
        107 => "geteuid",
        108 => "getegid",
        110 => "getppid",
        112 => "setsid",
        113 => "setreuid",
        114 => "setregid",
        116 => "setgroups",
        117 => "setresuid",
        119 => "setresgid",
        125 => "capget",
        126 => "capset",
        137 => "statfs",
        138 => "fstatfs",
        155 => "pivot_root",
        157 => "prctl",
        158 => "arch_prctl",
        160 => "setrlimit",
        161 => "chroot",
        165 => "mount",
        166 => "umount2",
        170 => "sethostname",
        186 => "gettid",
        202 => "futex",
        203 => "sched_setaffinity",
        204 => "sched_getaffinity",
        217 => "getdents64",
        218 => "set_tid_address",
        228 => "clock_gettime",
        230 => "clock_nanosleep",
        231 => "exit_group",
        232 => "epoll_wait",
        233 => "epoll_ctl",
        234 => "tgkill",
        257 => "openat",
        258 => "mkdirat",
        262 => "newfstatat",
        266 => "symlinkat",
        267 => "readlinkat",
        269 => "faccessat",
        270 => "pselect6",
        271 => "ppoll",
        272 => "unshare",
        273 => "set_robust_list",
        281 => "epoll_pwait",
        283 => "timerfd_create",
        284 => "eventfd",
        288 => "accept4",
        289 => "signalfd4",
        290 => "eventfd2",
        291 => "epoll_create1",
        292 => "dup3",
        293 => "pipe2",
        302 => "prlimit64",
        308 => "setns",
        309 => "getcpu",
        314 => "sched_setattr",
        315 => "sched_getattr",
        317 => "seccomp",
        318 => "getrandom",
        319 => "memfd_create",
        322 => "execveat",
        324 => "membarrier",
        332 => "statx",
        334 => "rseq",
        435 => "clone3",
        437 => "openat2",
        439 => "faccessat2",
        _ => return None,
    })
}

/// Rang de l'argument chemin (`None` : l'appel n'en prend pas).
fn path_arg(nr: u64) -> Option<usize> {
    match nr {
        2 | 4 | 6 | 21 | 59 | 80 | 82 | 83 | 84 | 87 | 89 | 90 | 137 => Some(0),
        257 | 262 | 267 | 269 | 322 | 332 | 437 | 439 => Some(1),
        _ => None,
    }
}

/// Appel `nr` d'arguments `a` terminé avec `ret` (valeur brute de `rax`, négative en erreur ;
/// `None` : l'appel a fixé lui-même les registres).
pub fn record(t: &Thread, nr: u64, a: &[u64; 6], ret: Option<i64>) {
    let mode = MODE.load(Ordering::Relaxed);
    let failed = ret.is_some_and(|r| (-4095..0).contains(&r));
    if mode == OFF
        || (mode == ERRORS && (!failed || ret == Some(-EAGAIN)))
        || (mode == NET && !is_net(nr))
        || !matches(t)
    {
        return;
    }
    let call = match name(nr) {
        Some(n) => String::from(n),
        None => format!("syscall_{nr}"),
    };
    let path = path_arg(nr)
        .and_then(|i| t.mm().read_cstr(a[i] as usize, 512).ok())
        .map(|raw| format!(" \"{}\"", String::from_utf8_lossy(&raw)))
        .unwrap_or_default();
    let addr = sockaddr_arg(nr)
        .and_then(|(i, l)| sockaddr_string(t, a[i] as usize, a[l] as usize))
        .map(|s| format!(" {s}"))
        .or_else(|| sockopt_string(t, nr, a))
        .unwrap_or_default();
    let res = match ret {
        None => String::from("?"),
        Some(r) if failed => format!("-{} ({})", -r, errno_name(-r)),
        Some(r) => format!("{:#x}", r),
    };
    log::warn!(
        "strace {}/{} {}({:#x}, {:#x}, {:#x}, {:#x}){}{} = {}",
        t.process.pid, t.tid(), call, a[0], a[1], a[2], a[3], path, addr, res
    );
}

/// Appels de sockets (mode `net`).
fn is_net(nr: u64) -> bool {
    matches!(nr, 41..=55 | 288 | 299 | 307)
}

/// Valeur entière d'un `setsockopt`/`getsockopt` (niveau, option, valeur), si elle tient sur 4
/// octets.
fn sockopt_string(t: &Thread, nr: u64, a: &[u64; 6]) -> Option<String> {
    let val = match nr {
        54 if a[4] == 4 => t.mm().read_user::<u32>(a[3] as usize).ok()?,
        55 => {
            let len = t.mm().read_user::<u32>(a[4] as usize).ok()?;
            if len != 4 {
                return None;
            }
            t.mm().read_user::<u32>(a[3] as usize).ok()?
        }
        _ => return None,
    };
    Some(format!(" niveau {} option {} valeur {}", a[1] as i32, a[2] as i32, val as i32))
}

/// Rangs de l'adresse et de sa longueur (`connect`, `sendto`, `bind`).
fn sockaddr_arg(nr: u64) -> Option<(usize, usize)> {
    match nr {
        42 | 49 => Some((1, 2)),
        44 => Some((4, 5)),
        _ => None,
    }
}

/// Adresse de socket de l'appelant, lisible : `inet 1.2.3.4:80`, `inet6 [2001:db8::1]:443`,
/// `unix "/run/…"` (le `@` d'un nom abstrait).
fn sockaddr_string(t: &Thread, uaddr: usize, len: usize) -> Option<String> {
    if uaddr == 0 || len < 2 {
        return None;
    }
    let mut buf = [0u8; 110];
    let n = len.min(buf.len());
    t.mm().copy_from_user(&mut buf[..n], uaddr).ok()?;
    let b = &buf[..n];
    let port = |b: &[u8]| u16::from_be_bytes([b[2], b[3]]);
    Some(match u16::from_ne_bytes([b[0], b[1]]) {
        2 if n >= 8 => format!("inet {}.{}.{}.{}:{}", b[4], b[5], b[6], b[7], port(b)),
        10 if n >= 24 => {
            let mut s = String::from("inet6 [");
            for i in 0..8 {
                if i > 0 {
                    s.push(':');
                }
                s += &format!("{:x}", u16::from_be_bytes([b[8 + 2 * i], b[9 + 2 * i]]));
            }
            s + &format!("]:{}", port(b))
        }
        1 => {
            let path = &b[2..];
            let (abs, path) = match path.first() {
                Some(0) => ("@", &path[1..]),
                _ => ("", path),
            };
            let end = if abs.is_empty() { path.iter().position(|&c| c == 0).unwrap_or(path.len()) } else { path.len() };
            format!("unix \"{}{}\"", abs, String::from_utf8_lossy(&path[..end]))
        }
        f => format!("famille {f}"),
    })
}

fn errno_name(e: i64) -> &'static str {
    match e {
        EPERM => "EPERM",
        ENOENT => "ENOENT",
        ESRCH => "ESRCH",
        EINTR => "EINTR",
        EIO => "EIO",
        ENXIO => "ENXIO",
        EBADF => "EBADF",
        EAGAIN => "EAGAIN",
        ENOMEM => "ENOMEM",
        EACCES => "EACCES",
        EFAULT => "EFAULT",
        EBUSY => "EBUSY",
        EEXIST => "EEXIST",
        ENODEV => "ENODEV",
        ENOTDIR => "ENOTDIR",
        EISDIR => "EISDIR",
        EINVAL => "EINVAL",
        EMFILE => "EMFILE",
        ENOTTY => "ENOTTY",
        ENOSPC => "ENOSPC",
        ESPIPE => "ESPIPE",
        EROFS => "EROFS",
        EPIPE => "EPIPE",
        ERANGE => "ERANGE",
        ENOSYS => "ENOSYS",
        ENOTEMPTY => "ENOTEMPTY",
        ELOOP => "ELOOP",
        EOPNOTSUPP => "EOPNOTSUPP",
        ECONNREFUSED => "ECONNREFUSED",
        ETIMEDOUT => "ETIMEDOUT",
        ENOTSOCK => "ENOTSOCK",
        EAFNOSUPPORT => "EAFNOSUPPORT",
        EADDRINUSE => "EADDRINUSE",
        ENETUNREACH => "ENETUNREACH",
        ECONNRESET => "ECONNRESET",
        ENOTCONN => "ENOTCONN",
        EHOSTUNREACH => "EHOSTUNREACH",
        EALREADY => "EALREADY",
        EINPROGRESS => "EINPROGRESS",
        512 => "ERESTARTSYS",
        514 => "ERESTARTNOHAND",
        _ => "?",
    }
}

/// `/proc/maios/strace`, une ouverture.
struct StraceFile;

impl FileObject for StraceFile {
    object_any!();

    fn link_name(&self) -> String {
        String::from("/proc/maios/strace")
    }

    fn stat(&self) -> (u32, u64) {
        (0o100644, 0x6d61_6901)
    }

    fn read(&self, _t: &Thread, _kb: &mut [u8], _nonblock: bool) -> Result<usize, i64> {
        Ok(0)
    }

    fn write(&self, t: &Thread, kb: &[u8], _nonblock: bool) -> Result<usize, i64> {
        if t.process.creds.lock().euid != 0 {
            return Err(EPERM);
        }
        let text = core::str::from_utf8(kb).map_err(|_| EINVAL)?.trim();
        let (mode, pat) = text.split_once(' ').map_or((text, ""), |(m, p)| (m, p.trim()));
        let m = match mode {
            "off" | "0" => OFF,
            "errors" => ERRORS,
            "all" => ALL,
            "net" => NET,
            _ => return Err(EINVAL),
        };
        *PATTERN.lock() = String::from(pat);
        MODE.store(m, Ordering::Relaxed);
        Ok(kb.len())
    }

    fn poll(&self, _t: Option<&Thread>) -> u32 {
        crate::object::ev::READABLE | crate::object::ev::WRITABLE
    }

    fn queues<'a>(&'a self, _cb: &mut dyn FnMut(&'a WaitQueue)) {}
}

/// Nœud de `/proc/maios/strace`.
pub fn proc_node(abs: &str) -> Option<crate::fd::Node> {
    (abs == "/proc/maios/strace").then(|| crate::fd::Node::Object(Arc::new(StraceFile)))
}
