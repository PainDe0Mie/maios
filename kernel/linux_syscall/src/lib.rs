//! Couche de traduction Linux → MaiOS.
//!
//! Ce crate est un mapper mince qui traduit les numéros de syscall Linux
//! en numéros MaiOS natifs, puis appelle `maios_syscall::dispatch()`.
//!
//! Avant cette refactorisation, ce crate contenait toutes les implémentations
//! (1000+ lignes). Maintenant c'est ~100 lignes de mapping.
//!
//! ## Convention Linux x86_64
//!
//! - RAX = numéro de syscall
//! - Arguments : RDI, RSI, RDX, R10, R8, R9
//! - Retour : RAX (valeur négative = -errno)

#![no_std]

extern crate alloc;

use log::warn;
use maios_syscall::error::result_to_linux;

/// Numéros de syscall Linux x86_64.
pub mod nr {
    pub const SYS_READ: u64 = 0;
    pub const SYS_WRITE: u64 = 1;
    pub const SYS_OPEN: u64 = 2;
    pub const SYS_CLOSE: u64 = 3;
    pub const SYS_STAT: u64 = 4;
    pub const SYS_FSTAT: u64 = 5;
    pub const SYS_LSEEK: u64 = 8;
    pub const SYS_MMAP: u64 = 9;
    pub const SYS_MPROTECT: u64 = 10;
    pub const SYS_MUNMAP: u64 = 11;
    pub const SYS_BRK: u64 = 12;
    pub const SYS_IOCTL: u64 = 16;
    pub const SYS_GETPID: u64 = 39;
    pub const SYS_EXECVE: u64 = 59;
    pub const SYS_EXIT: u64 = 60;
    pub const SYS_UNAME: u64 = 63;
    pub const SYS_GETUID: u64 = 102;
    pub const SYS_GETGID: u64 = 104;
    pub const SYS_GETEUID: u64 = 107;
    pub const SYS_GETEGID: u64 = 108;
    pub const SYS_GETPPID: u64 = 110;
    pub const SYS_ARCH_PRCTL: u64 = 158;
    pub const SYS_GETTID: u64 = 186;
    pub const SYS_PREAD64: u64 = 17;
    pub const SYS_READV: u64 = 19;
    pub const SYS_WRITEV: u64 = 20;
    pub const SYS_ACCESS: u64 = 21;
    pub const SYS_PIPE: u64 = 22;
    pub const SYS_DUP: u64 = 32;
    pub const SYS_DUP2: u64 = 33;
    pub const SYS_NANOSLEEP: u64 = 35;
    pub const SYS_SCHED_YIELD: u64 = 24;
    pub const SYS_FCNTL: u64 = 72;
    pub const SYS_GETCWD: u64 = 79;
    pub const SYS_GETTIMEOFDAY: u64 = 96;
    pub const SYS_RT_SIGACTION: u64 = 13;
    pub const SYS_RT_SIGPROCMASK: u64 = 14;
    pub const SYS_RT_SIGRETURN: u64 = 15;
    pub const SYS_SET_TID_ADDRESS: u64 = 218;
    pub const SYS_SET_ROBUST_LIST: u64 = 273;
    pub const SYS_PRLIMIT64: u64 = 302;
    pub const SYS_PWRITE64: u64 = 18;
    pub const SYS_MADVISE: u64 = 28;
    pub const SYS_CHDIR: u64 = 80;
    pub const SYS_MKDIR: u64 = 83;
    pub const SYS_UNLINK: u64 = 87;
    pub const SYS_READLINK: u64 = 89;
    pub const SYS_PRCTL: u64 = 157;
    pub const SYS_SCHED_GETAFFINITY: u64 = 204;
    pub const SYS_GETDENTS64: u64 = 217;
    pub const SYS_CLOCK_GETRES: u64 = 229;
    pub const SYS_NEWFSTATAT: u64 = 262;
    pub const SYS_FACCESSAT: u64 = 269;
    pub const SYS_READLINKAT: u64 = 267;
    pub const SYS_POLL: u64 = 7;
    pub const SYS_WAIT4: u64 = 61;
    pub const SYS_KILL: u64 = 62;
    pub const SYS_EPOLL_WAIT: u64 = 232;
    pub const SYS_EPOLL_CTL: u64 = 233;
    pub const SYS_TGKILL: u64 = 234;
    pub const SYS_MREMAP: u64 = 25;
    pub const SYS_SOCKET: u64 = 41;
    pub const SYS_CONNECT: u64 = 42;
    pub const SYS_SENDTO: u64 = 44;
    pub const SYS_RECVFROM: u64 = 45;
    pub const SYS_SENDMSG: u64 = 46;
    pub const SYS_RECVMSG: u64 = 47;
    pub const SYS_SHUTDOWN: u64 = 48;
    pub const SYS_BIND: u64 = 49;
    pub const SYS_LISTEN: u64 = 50;
    pub const SYS_GETSOCKNAME: u64 = 51;
    pub const SYS_GETPEERNAME: u64 = 52;
    pub const SYS_SOCKETPAIR: u64 = 53;
    pub const SYS_SETSOCKOPT: u64 = 54;
    pub const SYS_GETSOCKOPT: u64 = 55;
    pub const SYS_FTRUNCATE: u64 = 77;
    pub const SYS_RENAME: u64 = 82;
    pub const SYS_UMASK: u64 = 95;
    pub const SYS_GETRUSAGE: u64 = 98;
    pub const SYS_SYSINFO: u64 = 99;
    pub const SYS_ACCEPT4: u64 = 288;
    pub const SYS_EVENTFD2: u64 = 290;
    pub const SYS_EPOLL_CREATE1: u64 = 291;
    pub const SYS_CLOCK_GETTIME: u64 = 228;
    pub const SYS_EXIT_GROUP: u64 = 231;
    pub const SYS_OPENAT: u64 = 257;
    pub const SYS_DUP3: u64 = 292;
    pub const SYS_PIPE2: u64 = 293;
    pub const SYS_GETRANDOM: u64 = 318;
}

/// Errno Linux pour compatibilité (utilisé par le code legacy).
pub mod errno {
    pub const ENOSYS: i64 = -38;
}

// =============================================================================
// Table de traduction Linux → MaiOS
// =============================================================================

/// Valeur sentinelle indiquant un syscall non mappé.
const UNMAPPED: u16 = 0xFFFF;

/// Table de correspondance : index = numéro Linux, valeur = numéro MaiOS.
///
/// Taille : 319 entrées × 2 octets = 638 octets. Lookup O(1).
static LINUX_TO_MAIOS: [u16; 450] = {
    let mut table = [UNMAPPED; 450];

    // File I/O
    table[0]   = maios_syscall::nr::SYS_READ;       // read
    table[1]   = maios_syscall::nr::SYS_WRITE;      // write
    table[2]   = maios_syscall::nr::SYS_OPEN;       // open
    table[3]   = maios_syscall::nr::SYS_CLOSE;      // close
    table[4]   = maios_syscall::nr::SYS_STAT;       // stat
    table[5]   = maios_syscall::nr::SYS_FSTAT;      // fstat
    table[8]   = maios_syscall::nr::SYS_LSEEK;      // lseek
    table[16]  = maios_syscall::nr::SYS_IOCTL;      // ioctl
    table[17]  = maios_syscall::nr::SYS_PREAD64;    // pread64
    table[19]  = maios_syscall::nr::SYS_READV;      // readv
    table[20]  = maios_syscall::nr::SYS_WRITEV;     // writev
    table[21]  = maios_syscall::nr::SYS_ACCESS;     // access
    table[72]  = maios_syscall::nr::SYS_FCNTL;      // fcntl
    table[79]  = maios_syscall::nr::SYS_GETCWD;     // getcwd
    table[257] = maios_syscall::nr::SYS_OPENAT;     // openat
    table[292] = maios_syscall::nr::SYS_DUP3;       // dup3
    table[288] = maios_syscall::nr::SYS_ACCEPT4;       // accept4
    table[290] = maios_syscall::nr::SYS_EVENTFD2;      // eventfd2
    table[291] = maios_syscall::nr::SYS_EPOLL_CREATE1; // epoll_create1
    table[293] = maios_syscall::nr::SYS_PIPE2;      // pipe2

    // Memory
    table[9]   = maios_syscall::nr::SYS_MMAP;       // mmap
    table[10]  = maios_syscall::nr::SYS_MPROTECT;   // mprotect
    table[11]  = maios_syscall::nr::SYS_MUNMAP;     // munmap
    table[12]  = maios_syscall::nr::SYS_BRK;        // brk

    // Process
    table[39]  = maios_syscall::nr::SYS_GETPID;     // getpid
    table[59]  = maios_syscall::nr::SYS_EXECVE;     // execve
    table[60]  = maios_syscall::nr::SYS_EXIT;       // exit
    table[61]  = maios_syscall::nr::SYS_WAIT4;      // wait4
    table[62]  = maios_syscall::nr::SYS_KILL;       // kill

    // Sockets
    table[41]  = maios_syscall::nr::SYS_SOCKET;      // socket
    table[42]  = maios_syscall::nr::SYS_CONNECT;     // connect
    table[44]  = maios_syscall::nr::SYS_SENDTO;      // sendto
    table[45]  = maios_syscall::nr::SYS_RECVFROM;    // recvfrom
    table[46]  = maios_syscall::nr::SYS_SENDMSG;     // sendmsg
    table[47]  = maios_syscall::nr::SYS_RECVMSG;     // recvmsg
    table[48]  = maios_syscall::nr::SYS_SHUTDOWN;    // shutdown
    table[49]  = maios_syscall::nr::SYS_BIND;        // bind
    table[50]  = maios_syscall::nr::SYS_LISTEN;      // listen
    table[51]  = maios_syscall::nr::SYS_GETSOCKNAME; // getsockname
    table[52]  = maios_syscall::nr::SYS_GETPEERNAME; // getpeername
    table[53]  = maios_syscall::nr::SYS_SOCKETPAIR;  // socketpair
    table[54]  = maios_syscall::nr::SYS_SETSOCKOPT;  // setsockopt
    table[55]  = maios_syscall::nr::SYS_GETSOCKOPT;  // getsockopt
    table[110] = maios_syscall::nr::SYS_GETPPID;    // getppid
    table[186] = maios_syscall::nr::SYS_GETTID;     // gettid
    table[231] = maios_syscall::nr::SYS_EXIT_GROUP;  // exit_group

    // Identity (stubs)
    table[102] = maios_syscall::nr::SYS_GETUID;     // getuid
    table[104] = maios_syscall::nr::SYS_GETGID;     // getgid
    table[107] = maios_syscall::nr::SYS_GETEUID;    // geteuid
    table[108] = maios_syscall::nr::SYS_GETEGID;    // getegid

    // Event I/O
    table[7]   = maios_syscall::nr::SYS_POLL;           // poll

    // Signals
    table[13]  = maios_syscall::nr::SYS_RT_SIGACTION;   // rt_sigaction
    table[14]  = maios_syscall::nr::SYS_RT_SIGPROCMASK; // rt_sigprocmask
    table[15]  = maios_syscall::nr::SYS_RT_SIGRETURN;   // rt_sigreturn

    // Time & scheduling
    table[24]  = maios_syscall::nr::SYS_SCHED_YIELD;    // sched_yield
    table[35]  = maios_syscall::nr::SYS_NANOSLEEP;      // nanosleep
    table[96]  = maios_syscall::nr::SYS_GETTIMEOFDAY;   // gettimeofday
    table[228] = maios_syscall::nr::SYS_CLOCK_GETTIME;  // clock_gettime

    // File I/O extras
    table[18]  = maios_syscall::nr::SYS_PWRITE64;    // pwrite64
    table[22]  = maios_syscall::nr::SYS_PIPE;        // pipe
    table[25]  = maios_syscall::nr::SYS_MREMAP;       // mremap
    table[28]  = maios_syscall::nr::SYS_MADVISE;     // madvise
    table[32]  = maios_syscall::nr::SYS_DUP;         // dup
    table[33]  = maios_syscall::nr::SYS_DUP2;        // dup2

    // System info
    table[63]  = maios_syscall::nr::SYS_UNAME;        // uname
    table[158] = maios_syscall::nr::SYS_ARCH_PRCTL;   // arch_prctl
    table[318] = maios_syscall::nr::SYS_GETRANDOM;    // getrandom

    // Filesystem
    table[77]  = maios_syscall::nr::SYS_FTRUNCATE;     // ftruncate
    table[80]  = maios_syscall::nr::SYS_CHDIR;         // chdir
    table[82]  = maios_syscall::nr::SYS_RENAME;        // rename
    table[83]  = maios_syscall::nr::SYS_MKDIR;         // mkdir
    table[87]  = maios_syscall::nr::SYS_UNLINK;        // unlink
    table[89]  = maios_syscall::nr::SYS_READLINK;      // readlink
    table[95]  = maios_syscall::nr::SYS_UMASK;         // umask
    table[98]  = maios_syscall::nr::SYS_GETRUSAGE;     // getrusage
    table[99]  = maios_syscall::nr::SYS_SYSINFO;       // sysinfo
    table[157] = maios_syscall::nr::SYS_PRCTL;         // prctl
    table[204] = maios_syscall::nr::SYS_SCHED_GETAFFINITY; // sched_getaffinity
    table[217] = maios_syscall::nr::SYS_GETDENTS64;    // getdents64
    table[229] = maios_syscall::nr::SYS_CLOCK_GETRES;  // clock_getres
    table[232] = maios_syscall::nr::SYS_EPOLL_WAIT;   // epoll_wait
    table[233] = maios_syscall::nr::SYS_EPOLL_CTL;    // epoll_ctl
    table[234] = maios_syscall::nr::SYS_TGKILL;       // tgkill
    table[262] = maios_syscall::nr::SYS_NEWFSTATAT;    // newfstatat
    table[267] = maios_syscall::nr::SYS_READLINK;      // readlinkat (simplified: ignore dirfd)
    table[269] = maios_syscall::nr::SYS_FACCESSAT;     // faccessat

    // Threading stubs
    table[218] = maios_syscall::nr::SYS_SET_TID_ADDRESS; // set_tid_address
    table[273] = maios_syscall::nr::SYS_SET_ROBUST_LIST; // set_robust_list
    table[302] = maios_syscall::nr::SYS_PRLIMIT64;       // prlimit64

    table
};

// =============================================================================
// Point d'entrée
// =============================================================================

/// Point d'entrée pour le handling des syscalls Linux.
///
/// Traduit le numéro Linux en numéro MaiOS via lookup O(1),
/// puis dispatch vers la table unifiée `maios_syscall`.
pub fn handle_syscall(
    num: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
) -> i64 {
    let linux_nr = num as usize;

    let maios_nr = if linux_nr < LINUX_TO_MAIOS.len() {
        LINUX_TO_MAIOS[linux_nr]
    } else {
        UNMAPPED
    };

    if maios_nr == UNMAPPED {
        warn!(
            "linux_syscall: unmapped syscall {} (args: {:#x}, {:#x}, {:#x}, {:#x}, {:#x}, {:#x})",
            num, arg0, arg1, arg2, arg3, arg4, arg5
        );
        return errno::ENOSYS;
    }

    // Les arguments Linux passent directement — pas d'adaptation nécessaire
    // pour la plupart des syscalls (même convention registre que MaiOS).
    let result = maios_syscall::dispatch(maios_nr, arg0, arg1, arg2, arg3, arg4, arg5);
    result_to_linux(result)
}
