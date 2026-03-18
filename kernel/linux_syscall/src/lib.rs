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
    pub const SYS_FCNTL: u64 = 72;
    pub const SYS_GETCWD: u64 = 79;
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
static LINUX_TO_MAIOS: [u16; 335] = {
    let mut table = [UNMAPPED; 335];

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
    table[110] = maios_syscall::nr::SYS_GETPPID;    // getppid
    table[186] = maios_syscall::nr::SYS_GETTID;     // gettid
    table[231] = maios_syscall::nr::SYS_EXIT_GROUP;  // exit_group

    // Identity (stubs)
    table[102] = maios_syscall::nr::SYS_GETUID;     // getuid
    table[104] = maios_syscall::nr::SYS_GETGID;     // getgid
    table[107] = maios_syscall::nr::SYS_GETEUID;    // geteuid
    table[108] = maios_syscall::nr::SYS_GETEGID;    // getegid

    // Time
    table[35]  = maios_syscall::nr::SYS_NANOSLEEP;   // nanosleep
    table[228] = maios_syscall::nr::SYS_CLOCK_GETTIME; // clock_gettime

    // File I/O extras
    table[22]  = maios_syscall::nr::SYS_PIPE;        // pipe
    table[32]  = maios_syscall::nr::SYS_DUP;         // dup
    table[33]  = maios_syscall::nr::SYS_DUP2;        // dup2

    // System info
    table[63]  = maios_syscall::nr::SYS_UNAME;      // uname
    table[158] = maios_syscall::nr::SYS_ARCH_PRCTL;  // arch_prctl
    table[318] = maios_syscall::nr::SYS_GETRANDOM;  // getrandom

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
