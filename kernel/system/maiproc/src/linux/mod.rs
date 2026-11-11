//! Personnalité Linux x86_64 : appels système par numéro (`rax`), arguments dans
//! `rdi, rsi, rdx, r10, r8, r9`, résultat dans `rax` (`-errno` en cas d'erreur).
//!
//! Toutes les données utilisateur passent par les copies de [`uvm`] : une adresse invalide donne
//! `EFAULT`, jamais une faute du noyau. Un appel interrompu par un signal renvoie un code de
//! relance interne (`ERESTARTSYS`…) que le chemin de retour transforme en relance ou en `EINTR`
//! selon `SA_RESTART` ([`crate::signal`]).

pub(crate) mod compat;
pub(crate) mod fs;
mod mem;
pub(crate) mod proc;
pub(crate) mod sys;
mod prio;

use crate::Thread;
use uentry::UserRegs;
use uvm::VmError;

/// Suite à donner à un appel système.
pub enum Outcome {
    /// Valeur de retour (déjà négative pour une erreur).
    Return(i64),
    /// Registres déjà fixés par l'appel (`execve`, `rt_sigreturn`) : `rax` n'est pas touché.
    Keep,
    /// Fin du fil avec ce statut (`wait4`).
    Exit(i32),
}

/// Suite d'un appel interrompu, reprise par `restart_syscall`.
pub enum Restart {
    /// `nanosleep` / `clock_nanosleep` relatif : échéance (monotone) et reste à écrire.
    Sleep { deadline: time::Instant, rem: u64 },
}

/// Résultat interne : `Err` porte un `errno` positif.
pub(crate) type SysResult = Result<i64, i64>;

#[allow(dead_code)]
pub(crate) mod errno {
    pub const EPERM: i64 = 1;
    pub const ENOENT: i64 = 2;
    pub const ESRCH: i64 = 3;
    pub const EINTR: i64 = 4;
    pub const EIO: i64 = 5;
    pub const ENXIO: i64 = 6;
    pub const E2BIG: i64 = 7;
    pub const ENOEXEC: i64 = 8;
    pub const EBADF: i64 = 9;
    pub const ECHILD: i64 = 10;
    pub const EAGAIN: i64 = 11;
    pub const ENOMEM: i64 = 12;
    pub const EACCES: i64 = 13;
    pub const EFAULT: i64 = 14;
    pub const EDEADLK: i64 = 35;
    pub const EOVERFLOW: i64 = 75;
    pub const EBUSY: i64 = 16;
    pub const EEXIST: i64 = 17;
    pub const EXDEV: i64 = 18;
    pub const ENODEV: i64 = 19;
    pub const ENOTDIR: i64 = 20;
    pub const EISDIR: i64 = 21;
    pub const EINVAL: i64 = 22;
    pub const EMFILE: i64 = 24;
    pub const ENOTTY: i64 = 25;
    pub const EFBIG: i64 = 27;
    pub const ENOSPC: i64 = 28;
    pub const ESPIPE: i64 = 29;
    pub const EROFS: i64 = 30;
    pub const EPIPE: i64 = 32;
    pub const ERANGE: i64 = 34;
    pub const ENAMETOOLONG: i64 = 36;
    pub const ENOSYS: i64 = 38;
    pub const ENOMSG: i64 = 42;
    pub const EIDRM: i64 = 43;
    pub const ENOTEMPTY: i64 = 39;
    pub const ELOOP: i64 = 40;
    pub const ENOTSOCK: i64 = 88;
    pub const EDESTADDRREQ: i64 = 89;
    pub const EMSGSIZE: i64 = 90;
    pub const EPROTOTYPE: i64 = 91;
    pub const ENOPROTOOPT: i64 = 92;
    pub const EPROTONOSUPPORT: i64 = 93;
    pub const ESOCKTNOSUPPORT: i64 = 94;
    pub const ENOTSUP: i64 = 95;
    pub const EOPNOTSUPP: i64 = 95;
    pub const EAFNOSUPPORT: i64 = 97;
    pub const EADDRINUSE: i64 = 98;
    pub const EADDRNOTAVAIL: i64 = 99;
    pub const ENETDOWN: i64 = 100;
    pub const ENETUNREACH: i64 = 101;
    pub const ECONNABORTED: i64 = 103;
    pub const ECONNRESET: i64 = 104;
    pub const ENOBUFS: i64 = 105;
    pub const EISCONN: i64 = 106;
    pub const ENOTCONN: i64 = 107;
    pub const ETIMEDOUT: i64 = 110;
    pub const ECONNREFUSED: i64 = 111;
    pub const EHOSTUNREACH: i64 = 113;
    pub const EALREADY: i64 = 114;
    pub const EINPROGRESS: i64 = 115;
    pub const EUSERS: i64 = 87;
    pub const ENOTBLK: i64 = 15;
    pub const ETXTBSY: i64 = 26;
}
use errno::*;

impl From<VmError> for Errno {
    fn from(e: VmError) -> Errno {
        Errno(match e {
            VmError::NoMemory => ENOMEM,
            VmError::Invalid => EINVAL,
            VmError::NoMapping | VmError::Access => EFAULT,
        })
    }
}

/// `errno` positif (conversion depuis les erreurs internes).
pub(crate) struct Errno(pub i64);
impl From<Errno> for i64 {
    fn from(e: Errno) -> i64 { e.0 }
}

/// `VmError` d'une copie → `EFAULT` (ou `ENOMEM`).
#[inline]
pub(crate) fn fault(e: VmError) -> i64 {
    Errno::from(e).0
}

/// Lit un chemin (au plus `PATH_MAX`).
pub(crate) fn user_path(t: &Thread, ptr: u64) -> Result<alloc::string::String, i64> {
    if ptr == 0 {
        return Err(EFAULT);
    }
    let raw = t.mm().read_cstr(ptr as usize, 4096).map_err(|e| match e {
        VmError::Invalid => ENAMETOOLONG,
        e => fault(e),
    })?;
    // Octets quelconques (PEP 383) : décodés sans perte, réencodés à la sortie.
    Ok(fs_node::rawname::decode(&raw))
}

/// Délai relatif (`timespec`) en échéance monotone ; `absolute` : échéance absolue monotone.
pub(crate) fn futex_deadline(t: &Thread, ts: u64, absolute: bool) -> Result<Option<time::Instant>, i64> {
    crate::futex::deadline_from(t, ts, absolute, false)
}

/// Délai en millisecondes (`-1` ou négatif : sans limite).
fn ms_timeout(v: u64) -> Option<core::time::Duration> {
    let ms = v as i32;
    if ms < 0 { None } else { Some(core::time::Duration::from_millis(ms as u64)) }
}

/// Aiguillage par numéro Linux (mesuré si `/proc/maios/syscalls` est allumé).
pub fn dispatch(t: &Thread, regs: &mut UserRegs) -> Outcome {
    if crate::strace::enabled() {
        let nr = regs.rax;
        let a = [regs.rdi, regs.rsi, regs.rdx, regs.r10, regs.r8, regs.r9];
        let o = dispatch_timed(t, regs);
        let ret = match &o {
            Outcome::Return(v) => Some(*v),
            _ => None,
        };
        crate::strace::record(t, nr, &a, ret);
        return o;
    }
    dispatch_timed(t, regs)
}

fn dispatch_timed(t: &Thread, regs: &mut UserRegs) -> Outcome {
    if !crate::sysstats::enabled() {
        return dispatch_inner(t, regs);
    }
    let nr = regs.rax;
    let start = crate::sysstats::now_ns();
    let o = dispatch_inner(t, regs);
    crate::sysstats::record_syscall(nr, start);
    o
}

/// Arguments de type C `unsigned int` (ou drapeaux `int`) de l'appel `nr`, en masque de rangs.
/// Linux les tronque à 32 bits (`SYSCALL_DEFINE`) : un programme qui passe par `syscall()`
/// (arguments variadiques) peut laisser n'importe quoi dans les bits hauts du registre
/// (Firefox : `memfd_create(nom, 0x1_0000_000b)`). Les arguments signés (descripteur de
/// dossier, délais, horloges, pid) sont lus par `i32a`, qui ne regarde que les 32 bits bas.
fn u32_args(nr: u64) -> u8 {
    const A0: u8 = 1;
    const A1: u8 = 2;
    const A2: u8 = 4;
    const A3: u8 = 8;
    const A4: u8 = 16;
    const A5: u8 = 32;
    match nr {
        16 => A0 | A1,              // ioctl(fd, cmd, arg)
        72 => A0 | A1,              // fcntl(fd, cmd, arg)
        202 => A1 | A2 | A5,        // futex(uaddr, op, val, timeout, uaddr2, val3)
        203 | 204 => A1,            // sched_{set,get}affinity(pid, len, mask)
        257 => A2 | A3,             // openat(dfd, path, flags, mode)
        269 => A2,                  // faccessat(dfd, path, mode)
        439 => A2 | A3,             // faccessat2
        262 => A3,                  // newfstatat(dfd, path, buf, flag)
        280 => A3,                  // utimensat
        283 => A1,                  // timerfd_create(clockid, flags)
        284 => A0,                  // eventfd(count)
        290 => A0 | A1,             // eventfd2(count, flags)
        291 => A0,                  // epoll_create1(flags)
        292 => A0 | A1 | A2,        // dup3(old, new, flags)
        293 => A1,                  // pipe2(fds, flags)
        288 => A3,                  // accept4(fd, addr, len, flags)
        41 => A0 | A1 | A2,         // socket(domain, type, protocol)
        53 => A0 | A1 | A2,         // socketpair
        302 => A1,                  // prlimit64(pid, resource, new, old)
        316 => A4,                  // renameat2(.., flags)
        318 => A2,                  // getrandom(buf, len, flags)
        319 => A1,                  // memfd_create(name, flags)
        324 => A1,                  // membarrier(cmd, flags, cpu_id)
        326 => A5,                  // copy_file_range(.., flags)
        332 => A2 | A3,             // statx(dfd, path, flags, mask, buf)
        334 => A1 | A3,             // rseq(rseq, len, flags, sig)
        424 => A3,                  // pidfd_send_signal(pidfd, sig, info, flags)
        425 => A0,                  // io_uring_setup(entries, params)
        426 => A0 | A1 | A2 | A3,   // io_uring_enter(fd, to_submit, min_complete, flags, ..)
        427 => A0 | A1 | A3,        // io_uring_register(fd, opcode, arg, nr_args)
        434 => A1,                  // pidfd_open(pid, flags)
        272 => A0,                  // unshare(flags)
        308 => A0 | A1,             // setns(fd, nstype)
        166 => A1,                  // umount2(target, flags)
        157 => A0,                  // prctl(option, ..)
        438 => A1 | A2,             // pidfd_getfd(pidfd, targetfd, flags)
        _ => 0,
    }
}

fn dispatch_inner(t: &Thread, regs: &mut UserRegs) -> Outcome {
    let nr = regs.rax;
    let mut a = [regs.rdi, regs.rsi, regs.rdx, regs.r10, regs.r8, regs.r9];
    let narrow = u32_args(nr);
    if narrow != 0 {
        for (i, v) in a.iter_mut().enumerate() {
            if narrow & (1 << i) != 0 {
                *v = *v as u32 as u64;
            }
        }
    }
    let a = a;
    let i32a = |i: usize| a[i] as i32 as i64;
    let r: SysResult = match nr {
        // --- Fichiers ---
        0 => fs::read(t, a[0], a[1], a[2]),
        1 => fs::write(t, a[0], a[1], a[2]),
        2 => fs::openat(t, fs::AT_FDCWD, a[0], a[1], a[2]),
        3 => fs::close(t, a[0]),
        436 => fs::close_range(t, a[0], a[1], a[2]),
        437 => fs::openat2(t, i32a(0), a[1], a[2], a[3]),
        73 => fs::flock(t, a[0], a[1]),
        327 => fs::rw_iov2(t, a[0], a[1], a[2], a[3] as i64, a[5], false), // preadv2
        328 => fs::rw_iov2(t, a[0], a[1], a[2], a[3] as i64, a[5], true), // pwritev2
        326 => fs::copy_file_range(t, a[0], a[1], a[2], a[3], a[4], a[5]),
        275 => fs::splice(t, a[0], a[1], a[2], a[3], a[4], a[5]),
        285 => fs::fallocate(t, a[0], a[1], a[2] as i64, a[3] as i64),
        319 => fs::memfd_create(t, a[0], a[1]),
        // IPC System V : mémoire partagée (`ipc.rs`).
        29 => crate::ipc::shmget(t, a[0] as i32, a[1], a[2] as u32 as u64),
        30 => crate::ipc::shmat(t, i32a(0), a[1], a[2] as u32 as u64),
        31 => crate::ipc::shmctl(t, i32a(0), a[1] as u32 as u64, a[2]),
        67 => crate::ipc::shmdt(t, a[0]),
        133 => fs::mknodat(t, fs::AT_FDCWD, a[0], a[1], a[2]),
        259 => fs::mknodat(t, i32a(0), a[1], a[2], a[3]),
        // Attributs étendus : aucun volume ne les gère encore (`ENOTSUP`, comme un système de
        // fichiers sans xattr ; les programmes s'en passent).
        188..=199 => Err(EOPNOTSUPP),
        162 => Ok(0), // sync : tout est en mémoire ou en lecture seule
        306 => fs::fsync(t, a[0]), // syncfs
        221 => fs::fadvise64(t, a[0], a[1] as i64, a[2] as i64, a[3]),
        187 => fs::readahead(t, a[0], a[1] as i64, a[2]),
        4 => fs::fstatat(t, fs::AT_FDCWD, a[0], a[1], 0),
        5 => fs::fstat(t, a[0], a[1]),
        6 => fs::fstatat(t, fs::AT_FDCWD, a[0], a[1], fs::AT_SYMLINK_NOFOLLOW),
        7 => fs::poll(t, a[0], a[1], a[2] as i32 as i64),
        8 => fs::lseek(t, a[0], a[1] as i64, a[2]),
        16 => fs::ioctl(t, a[0], a[1], a[2]),
        17 => fs::pread(t, a[0], a[1], a[2], a[3] as i64),
        18 => fs::pwrite(t, a[0], a[1], a[2], a[3] as i64),
        19 => fs::readv(t, a[0], a[1], a[2], None),
        20 => fs::writev(t, a[0], a[1], a[2], None),
        21 => fs::faccessat(t, fs::AT_FDCWD, a[0], a[1], 0),
        22 => fs::pipe2(t, a[0], 0),
        23 => fs::select(t, a[0], a[1], a[2], a[3], a[4]),
        32 => fs::dup(t, a[0]),
        33 => fs::dup3(t, a[0], a[1], 0, true),
        40 => fs::sendfile(t, a[0], a[1], a[2], a[3]),
        72 => fs::fcntl(t, a[0], a[1], a[2]),
        74 | 75 => fs::fsync(t, a[0]),
        76 => fs::truncate_path(t, a[0], a[1] as i64),
        77 => fs::ftruncate(t, a[0], a[1] as i64),
        79 => fs::getcwd(t, a[0], a[1]),
        80 => fs::chdir(t, a[0]),
        81 => fs::fchdir(t, a[0]),
        82 => fs::renameat(t, fs::AT_FDCWD, a[0], fs::AT_FDCWD, a[1]),
        83 => fs::mkdirat(t, fs::AT_FDCWD, a[0], a[1]),
        84 => fs::unlinkat(t, fs::AT_FDCWD, a[0], fs::AT_REMOVEDIR),
        85 => fs::openat(t, fs::AT_FDCWD, a[0], 0o1101, a[1]), // creat
        87 => fs::unlinkat(t, fs::AT_FDCWD, a[0], 0),
        89 => fs::readlinkat(t, fs::AT_FDCWD, a[0], a[1], a[2]),
        // chmod, chown, lchown, fchmodat ; fchmod, fchown ; fchownat.
        90 => fs::meta_change_path(t, fs::AT_FDCWD, a[0], 0, fs::MetaArg::Mode(a[1])),
        92 => fs::meta_change_path(t, fs::AT_FDCWD, a[0], 0, fs::MetaArg::Owner(a[1], a[2])),
        94 => fs::meta_change_path(t, fs::AT_FDCWD, a[0], fs::AT_SYMLINK_NOFOLLOW, fs::MetaArg::Owner(a[1], a[2])),
        268 => fs::meta_change_path(t, i32a(0), a[1], 0, fs::MetaArg::Mode(a[2])),
        // fchmodat2 (Linux 6.6) : avec drapeaux.
        452 => fs::meta_change_path(t, i32a(0), a[1], a[3], fs::MetaArg::Mode(a[2])),
        91 => fs::meta_change_fd(t, a[0], fs::MetaArg::Mode(a[1])),
        93 => fs::meta_change_fd(t, a[0], fs::MetaArg::Owner(a[1], a[2])),
        260 => fs::meta_change_path(t, i32a(0), a[1], a[4], fs::MetaArg::Owner(a[2], a[3])),
        // link, linkat, symlink, symlinkat.
        86 => fs::linkat(t, fs::AT_FDCWD, a[0], fs::AT_FDCWD, a[1], 0),
        265 => fs::linkat(t, i32a(0), a[1], i32a(2), a[3], a[4]),
        88 => fs::symlinkat(t, a[0], fs::AT_FDCWD, a[1]),
        266 => fs::symlinkat(t, a[0], i32a(1), a[2]),
        95 => Ok(t.process.umask.swap(a[0] as u32 & 0o777, core::sync::atomic::Ordering::AcqRel) as i64),
        // utime, utimes, utimensat (chemin nul : `futimens` sur le descripteur).
        132 => fs::meta_change_path(t, fs::AT_FDCWD, a[0], 0, fs::MetaArg::Utime(a[1])),
        235 => fs::meta_change_path(t, fs::AT_FDCWD, a[0], 0, fs::MetaArg::Utimes(a[1])),
        261 => fs::meta_change_path(t, i32a(0), a[1], 0, fs::MetaArg::Utimes(a[2])), // futimesat
        280 if a[1] == 0 => fs::meta_change_fd(t, a[0], fs::MetaArg::Utimens(a[2])),
        280 => fs::meta_change_path(t, i32a(0), a[1], a[3], fs::MetaArg::Utimens(a[2])),
        137 => fs::statfs_path(t, a[0], a[1], fs::StatfsLayout::Native),
        138 => fs::fstatfs(t, a[0], a[1], fs::StatfsLayout::Native),
        217 => fs::getdents64(t, a[0], a[1], a[2]),
        257 => fs::openat(t, i32a(0), a[1], a[2], a[3]),
        258 => fs::mkdirat(t, i32a(0), a[1], a[2]),
        262 => fs::fstatat(t, i32a(0), a[1], a[2], a[3]),
        263 => fs::unlinkat(t, i32a(0), a[1], a[2]),
        264 => fs::renameat(t, i32a(0), a[1], i32a(2), a[3]),
        316 => fs::renameat2(t, i32a(0), a[1], i32a(2), a[3], a[4]),
        267 => fs::readlinkat(t, i32a(0), a[1], a[2], a[3]),
        269 | 439 => fs::faccessat(t, i32a(0), a[1], a[2], a[3]),
        270 => fs::pselect6(t, a[0], a[1], a[2], a[3], a[4], a[5]),
        271 => fs::ppoll(t, a[0], a[1], a[2], a[3], a[4]),
        292 => fs::dup3(t, a[0], a[1], a[2], false),
        293 => fs::pipe2(t, a[0], a[1]),
        295 => fs::readv(t, a[0], a[1], a[2], Some(a[3] as i64)),
        296 => fs::writev(t, a[0], a[1], a[2], Some(a[3] as i64)),
        332 => fs::statx(t, i32a(0), a[1], a[2], a[3], a[4]),

        // --- Sockets ---
        41 => crate::socket::socket(t, a[0], a[1], a[2]),
        42 => crate::socket::connect(t, a[0], a[1], a[2]),
        43 => crate::socket::accept4(t, a[0], a[1], a[2], 0),
        288 => crate::socket::accept4(t, a[0], a[1], a[2], a[3]),
        44 => crate::socket::sendto(t, a[0], a[1], a[2], a[3], a[4], a[5]),
        45 => crate::socket::recvfrom(t, a[0], a[1], a[2], a[3], a[4], a[5]),
        46 => crate::socket::sendmsg(t, a[0], a[1], a[2]),
        47 => crate::socket::recvmsg(t, a[0], a[1], a[2]),
        307 => crate::socket::sendmmsg(t, a[0], a[1], a[2], a[3]),
        299 => crate::socket::recvmmsg(t, a[0], a[1], a[2], a[3], a[4]),
        48 => crate::socket::shutdown(t, a[0], a[1]),
        49 => crate::socket::bind(t, a[0], a[1], a[2]),
        50 => crate::socket::listen(t, a[0], a[1]),
        51 => crate::socket::getsockname(t, a[0], a[1], a[2]),
        52 => crate::socket::getpeername(t, a[0], a[1], a[2]),
        53 => crate::socket::socketpair(t, a[0], a[1], a[2], a[3]),
        54 => crate::socket::setsockopt(t, a[0], a[1], a[2], a[3], a[4]),
        55 => crate::socket::getsockopt(t, a[0], a[1], a[2], a[3], a[4]),

        // --- Événements : epoll, eventfd, timerfd, signalfd, pidfd ---
        213 => crate::epoll::epoll_create(t, a[0]),
        291 => crate::epoll::epoll_create1(t, a[0]),
        233 => crate::epoll::epoll_ctl(t, a[0], a[1], a[2], a[3]),
        232 => crate::epoll::epoll_wait(t, a[0], a[1], a[2], ms_timeout(a[3])),
        281 => {
            if a[4] != 0 && a[5] != 8 {
                Err(EINVAL)
            } else {
                fs::with_temp_mask(t, a[4], || crate::epoll::epoll_wait(t, a[0], a[1], a[2], ms_timeout(a[3])))
            }
        }
        425 => crate::uring::io_uring_setup(t, a[0], a[1]),
        426 => crate::uring::io_uring_enter(t, a[0], a[1], a[2], a[3], a[4], a[5]),
        427 => crate::uring::io_uring_register(t, a[0], a[1], a[2], a[3]),
        441 => {
            if a[4] != 0 && a[5] != 8 {
                Err(EINVAL)
            } else {
                match fs::read_ts(t, a[3]) {
                    Ok(tmo) => fs::with_temp_mask(t, a[4], || crate::epoll::epoll_wait(t, a[0], a[1], a[2], tmo)),
                    Err(e) => Err(e),
                }
            }
        }
        284 => crate::eventfd::eventfd2(t, a[0], 0),
        290 => crate::eventfd::eventfd2(t, a[0], a[1]),
        283 => crate::timerfd::timerfd_create(t, a[0], a[1]),
        286 => crate::timerfd::timerfd_settime(t, a[0], a[1], a[2], a[3]),
        287 => crate::timerfd::timerfd_gettime(t, a[0], a[1]),
        282 => crate::signalfd::signalfd4(t, a[0], a[1], a[2], 0),
        289 => crate::signalfd::signalfd4(t, a[0], a[1], a[2], a[3]),
        434 => crate::pidfd::pidfd_open(t, a[0], a[1]),
        424 => crate::pidfd::pidfd_send_signal(t, a[0], a[1], a[2], a[3]),
        438 => crate::pidfd::pidfd_getfd(t, a[0], a[1], a[2]),

        // --- Mémoire ---
        9 => mem::mmap(t, a[0], a[1], a[2], a[3], a[4], a[5]),
        10 => mem::mprotect(t, a[0], a[1], a[2]),
        11 => mem::munmap(t, a[0], a[1]),
        12 => Ok(t.mm().brk(a[0] as usize) as i64),
        25 => mem::mremap(t, a[0], a[1], a[2], a[3], a[4]),
        26 => mem::msync(t, a[0], a[1], a[2]),
        27 => mem::mincore(t, a[0], a[1], a[2]),
        28 => mem::madvise(t, a[0], a[1], a[2]),
        149..=152 => Ok(0), // mlock, munlock, mlockall, munlockall : tout est verrouillé

        // --- Processus ---
        56 => proc::clone(t, regs, a[0], a[1], a[2], a[3], a[4]),
        57 => proc::fork(t, regs),
        58 => proc::vfork(t, regs),
        435 => proc::clone3(t, regs, a[0], a[1]),
        59 => match proc::execve(t, regs, fs::AT_FDCWD, a[0], a[1], a[2], 0) {
            Ok(o) => return o,
            Err(e) => Err(e),
        },
        322 => match proc::execve(t, regs, i32a(0), a[1], a[2], a[3], a[4]) {
            Ok(o) => return o,
            Err(e) => Err(e),
        },
        61 => proc::wait4(t, i32a(0), a[1], a[2], a[3]),
        247 => proc::waitid(t, a[0], a[1], a[2], a[3], a[4]),
        60 => return proc::exit(a[0] as i32),
        231 => return proc::exit_group(t, a[0] as i32),

        // --- Identité, groupes, sessions ---
        // Numéros vus de l'espace pid de l'appelant (`task_tgid_vnr`, `task_ppid_nr_ns` : 0 pour
        // un parent hors de l'espace).
        39 => Ok(t.process.vpid as i64),
        110 => Ok(t.process.vnr(t.process.ppid.load(core::sync::atomic::Ordering::Relaxed)) as i64),
        186 => Ok(t.vtid() as i64),
        102 | 104 | 107 | 108 | 105 | 106 | 113 | 114 | 117 | 119 | 122 | 123 | 118 | 120 | 115 | 116 => {
            sys::identity(t, nr, a[0], a[1], a[2])
        }
        111 => Ok(t.process.vnr(t.process.pgid.load(core::sync::atomic::Ordering::Relaxed)) as i64), // getpgrp
        109 => proc::setpgid(t, a[0], a[1]),
        121 => proc::getpgid(t, a[0]),
        112 => proc::setsid(t),
        124 => proc::getsid(t, a[0]),

        // --- Signaux ---
        13 => proc::rt_sigaction(t, a[0], a[1], a[2], a[3]),
        14 => proc::rt_sigprocmask(t, a[0], a[1], a[2], a[3]),
        15 => return proc::rt_sigreturn(t, regs),
        34 => proc::pause(t),
        62 => proc::kill(t, a[0] as i32 as i64, a[1] as i32),
        127 => proc::rt_sigpending(t, a[0], a[1]),
        128 => proc::rt_sigtimedwait(t, a[0], a[1], a[2], a[3]),
        129 => proc::rt_sigqueueinfo(t, i32a(0), None, a[1] as i32, a[2]),
        130 => proc::rt_sigsuspend(t, a[0], a[1]),
        131 => proc::sigaltstack(t, regs, a[0], a[1]),
        200 => proc::tgkill(t, -1, i32a(0), a[1] as i32),
        234 => proc::tgkill(t, i32a(0), i32a(1), a[2] as i32),
        297 => proc::rt_sigqueueinfo(t, i32a(0), Some(i32a(1)), a[2] as i32, a[3]),
        37 => proc::alarm(t, a[0]),
        36 => proc::getitimer(t, a[0], a[1]),
        38 => proc::setitimer(t, a[0], a[1], a[2]),
        219 => sys::restart_syscall(t),

        // --- Fils, futex ---
        218 => {
            t.clear_child_tid.store(a[0] as usize, core::sync::atomic::Ordering::Release);
            Ok(t.vtid() as i64)
        }
        273 => {
            if a[1] != 24 {
                Err(EINVAL)
            } else {
                t.robust_list.store(a[0] as usize, core::sync::atomic::Ordering::Release);
                Ok(0)
            }
        }
        274 => sys::get_robust_list(t, a[0], a[1], a[2]),
        202 => crate::futex::futex(t, a[0], a[1], a[2], a[3], a[4], a[5]),
        449 => crate::futex::futex_waitv(t, a[0], a[1], a[2], a[3], a[4]),

        // --- Contrôle, limites ---
        157 => sys::prctl(t, a[0], a[1], a[2], a[3], a[4]),
        158 => sys::arch_prctl(t, regs, a[0], a[1]),
        97 => sys::getrlimit(t, 0, a[0], 0, a[1]),
        160 => Ok(0), // setrlimit
        302 => sys::getrlimit(t, a[0], a[1], a[2], a[3]),
        98 => proc::getrusage(t, a[0] as i32 as i64, a[1]),
        100 => proc::times(t, a[0]),
        140 => prio::getpriority(t, a[0], a[1]),
        141 => prio::setpriority(t, a[0], a[1], a[2]),
        251 => prio::ioprio_set(t, a[0], a[1], a[2]),
        252 => prio::ioprio_get(t, a[0], a[1]),
        253 => crate::inotify::inotify_init1(t, 0),
        294 => crate::inotify::inotify_init1(t, a[0]),
        254 => crate::inotify::inotify_add_watch(t, a[0], a[1], a[2]),
        255 => crate::inotify::inotify_rm_watch(t, a[0], a[1]),
        222 => crate::ptimer::timer_create(t, a[0], a[1], a[2]),
        223 => crate::ptimer::timer_settime(t, a[0], a[1], a[2], a[3]),
        224 => crate::ptimer::timer_gettime(t, a[0], a[1]),
        225 => crate::ptimer::timer_getoverrun(t, a[0]),
        226 => crate::ptimer::timer_delete(t, a[0]),
        203 => sys::sched_setaffinity(t, a[0], a[1], a[2]),
        142..=148 => sys::sched_policy(t, nr, a[0], a[1], a[2]),
        204 => sys::sched_getaffinity(t, a[0], a[1], a[2]),
        170 => sys::set_uts_name(t, 0, a[0], a[1]),
        171 => sys::set_uts_name(t, 1, a[0], a[1]),
        24 => {
            task::scheduler::schedule();
            Ok(0)
        }
        334 => Err(ENOSYS), // rseq : la libc s'en passe

        // --- Temps ---
        228 => sys::clock_gettime(t, a[0], a[1]),
        229 => sys::clock_getres(t, a[0], a[1]),
        96 => sys::gettimeofday(t, a[0]),
        201 => sys::time(t, a[0]),
        35 => sys::nanosleep(t, a[0], a[1]),
        230 => sys::clock_nanosleep(t, a[0], a[1], a[2], a[3]),
        227 => sys::clock_settime(t, a[0], a[1]),
        164 => sys::settimeofday(t, a[0]),

        // --- Espaces de noms, montages, capacités ---
        161 => crate::mount::sys_chroot(t, a[0]),
        155 => crate::mount::sys_pivot_root(t, a[0], a[1]),
        165 => crate::mount::sys_mount(t, a[0], a[1], a[2], a[3], a[4]),
        166 => crate::mount::sys_umount2(t, a[0], a[1]),
        272 => crate::ns::unshare(t, a[0]),
        308 => crate::ns::setns(t, a[0], a[1]),
        125 => sys::capget(t, a[0], a[1]),
        126 => sys::capset(t, a[0], a[1]),

        // --- Système ---
        63 => sys::uname(t, a[0]),
        99 => sys::sysinfo(t, a[0]),
        318 => sys::getrandom(t, a[0], a[1], a[2]),
        309 => sys::getcpu(t, a[0], a[1]),

        _ => {
            sys::unknown(t, nr);
            Err(ENOSYS)
        }
    };
    Outcome::Return(match r {
        Ok(v) => v,
        Err(e) => -e,
    })
}
