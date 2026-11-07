//! Personnalité Linux x86_64 : appels système par numéro (`rax`), arguments dans
//! `rdi, rsi, rdx, r10, r8, r9`, résultat dans `rax` (`-errno` en cas d'erreur).
//!
//! Toutes les données utilisateur passent par les copies de [`uvm`] : une adresse invalide donne
//! `EFAULT`, jamais une faute du noyau.

mod fs;
mod mem;
mod sys;

use crate::Thread;
use uentry::UserRegs;
use uvm::VmError;

/// Suite à donner à un appel système.
pub enum Outcome {
    /// Valeur de retour (déjà négative pour une erreur).
    Return(i64),
    /// Fin du fil avec ce statut (`wait4`).
    Exit(i32),
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
    pub const EBADF: i64 = 9;
    pub const ECHILD: i64 = 10;
    pub const EAGAIN: i64 = 11;
    pub const ENOMEM: i64 = 12;
    pub const EACCES: i64 = 13;
    pub const EFAULT: i64 = 14;
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
    pub const ENOTEMPTY: i64 = 39;
    pub const ELOOP: i64 = 40;
    pub const ENOTSUP: i64 = 95;
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
    let raw = t.process.mm.read_cstr(ptr as usize, 4096).map_err(|e| match e {
        VmError::Invalid => ENAMETOOLONG,
        e => fault(e),
    })?;
    alloc::string::String::from_utf8(raw).map_err(|_| EINVAL)
}

/// Aiguillage par numéro Linux.
pub fn dispatch(t: &Thread, regs: &mut UserRegs) -> Outcome {
    let nr = regs.rax;
    let a = [regs.rdi, regs.rsi, regs.rdx, regs.r10, regs.r8, regs.r9];
    let r: SysResult = match nr {
        // --- Fichiers ---
        0 => fs::read(t, a[0], a[1], a[2]),
        1 => fs::write(t, a[0], a[1], a[2]),
        2 => fs::openat(t, fs::AT_FDCWD, a[0], a[1], a[2]),
        3 => fs::close(t, a[0]),
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
        90 | 91 | 92 | 93 | 94 | 260 | 268 => fs::chmod_like(t),
        95 => Ok(t.process.umask.swap(a[0] as u32 & 0o777, core::sync::atomic::Ordering::AcqRel) as i64),
        132 | 235 | 280 => Ok(0), // utime, utimes, utimensat : horodatages non conservés par le VFS
        137 => fs::statfs(t, a[1]),
        138 => fs::statfs(t, a[1]),
        217 => fs::getdents64(t, a[0], a[1], a[2]),
        257 => fs::openat(t, a[0] as i32 as i64, a[1], a[2], a[3]),
        258 => fs::mkdirat(t, a[0] as i32 as i64, a[1], a[2]),
        262 => fs::fstatat(t, a[0] as i32 as i64, a[1], a[2], a[3]),
        263 => fs::unlinkat(t, a[0] as i32 as i64, a[1], a[2]),
        264 | 316 => fs::renameat(t, a[0] as i32 as i64, a[1], a[2] as i32 as i64, a[3]),
        267 => fs::readlinkat(t, a[0] as i32 as i64, a[1], a[2], a[3]),
        269 | 439 => fs::faccessat(t, a[0] as i32 as i64, a[1], a[2], a[3]),
        271 => fs::ppoll(t, a[0], a[1], a[2]),
        292 => fs::dup3(t, a[0], a[1], a[2], false),
        295 => fs::readv(t, a[0], a[1], a[2], Some(a[3] as i64)),
        296 => fs::writev(t, a[0], a[1], a[2], Some(a[3] as i64)),
        332 => fs::statx(t, a[0] as i32 as i64, a[1], a[2], a[3], a[4]),

        // --- Mémoire ---
        9 => mem::mmap(t, a[0], a[1], a[2], a[3], a[4], a[5]),
        10 => mem::mprotect(t, a[0], a[1], a[2]),
        11 => mem::munmap(t, a[0], a[1]),
        12 => Ok(t.process.mm.brk(a[0] as usize) as i64),
        25 => mem::mremap(t, a[0], a[1], a[2], a[3], a[4]),
        26 => Ok(0), // msync : pas de projection partagée de fichier
        27 => mem::mincore(t, a[0], a[1], a[2]),
        28 => mem::madvise(t, a[0], a[1], a[2]),
        149 | 150 | 151 | 152 => Ok(0), // mlock, munlock, mlockall, munlockall : tout est verrouillé

        // --- Processus, identité, signaux ---
        39 => Ok(t.process.pid as i64),
        110 => Ok(t.process.ppid as i64),
        186 => Ok(t.tid as i64),
        102 | 104 | 107 | 108 => Ok(0), // getuid, getgid, geteuid, getegid : root
        105 | 106 | 113 | 114 | 117 | 119 => Ok(0), // setuid, setgid, setreuid, setregid, setresuid, setresgid
        118 | 120 => sys::getresid(t, a[0], a[1], a[2]),
        111 => Ok(t.process.pid as i64), // getpgrp
        109 => Ok(0), // setpgid
        121 => Ok(t.process.pid as i64), // getpgid
        112 => Ok(t.process.pid as i64), // setsid
        124 => Ok(t.process.pid as i64), // getsid
        115 => Ok(0), // getgroups : aucun groupe supplémentaire
        13 => sys::rt_sigaction(t, a[0], a[1], a[2], a[3]),
        14 => sys::rt_sigprocmask(t, a[0], a[1], a[2], a[3]),
        127 => sys::rt_sigpending(t, a[0], a[1]),
        131 => Ok(0), // sigaltstack : accepté (livraison des signaux : M2)
        60 => return sys::exit(t, a[0] as i32, false),
        231 => return sys::exit(t, a[0] as i32, true),
        62 => match sys::kill(t, a[0] as i64, a[1] as i32) {
            Ok(Some(status)) => return Outcome::Exit(status),
            Ok(None) => Ok(0),
            Err(e) => Err(e),
        },
        200 | 234 => {
            let (tid, sig) = if nr == 200 { (a[0] as i64, a[1] as i32) } else { (a[1] as i64, a[2] as i32) };
            match sys::tkill(t, tid, sig) {
                Ok(Some(status)) => return Outcome::Exit(status),
                Ok(None) => Ok(0),
                Err(e) => Err(e),
            }
        }
        61 => Err(ECHILD), // wait4 : pas encore d'enfants (fork : M2)
        218 => {
            t.clear_child_tid.store(a[0] as usize, core::sync::atomic::Ordering::Release);
            Ok(t.tid as i64)
        }
        273 => {
            t.robust_list.store(a[0] as usize, core::sync::atomic::Ordering::Release);
            Ok(0)
        }
        274 => Err(ENOSYS), // get_robust_list
        157 => sys::prctl(t, a[0], a[1]),
        158 => sys::arch_prctl(t, regs, a[0], a[1]),
        97 => sys::getrlimit(t, 0, a[0], 0, a[1]),
        160 => Ok(0), // setrlimit
        302 => sys::getrlimit(t, a[0], a[1], a[2], a[3]),
        98 => sys::getrusage(t, a[0], a[1]),
        100 => sys::times(t, a[0]),
        140 | 141 => Ok(0), // getpriority (20 - nice), setpriority
        203 => Ok(0), // sched_setaffinity
        204 => sys::sched_getaffinity(t, a[1], a[2]),
        24 => {
            task::scheduler::schedule();
            Ok(0)
        }
        334 => Err(ENOSYS), // rseq : la libc s'en passe
        435 | 56 | 57 | 58 | 59 | 322 => Err(ENOSYS), // clone3, clone, fork, vfork, execve, execveat : M2

        // --- Temps ---
        228 => sys::clock_gettime(t, a[0], a[1]),
        229 => sys::clock_getres(t, a[0], a[1]),
        96 => sys::gettimeofday(t, a[0]),
        201 => sys::time(t, a[0]),
        35 => sys::nanosleep(t, a[0], a[1]),
        230 => sys::clock_nanosleep(t, a[0], a[1], a[2], a[3]),

        // --- Système ---
        63 => sys::uname(t, a[0]),
        99 => sys::sysinfo(t, a[0]),
        318 => sys::getrandom(t, a[0], a[1], a[2]),
        202 => sys::futex(t, a[0], a[1], a[2], a[3]),

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
