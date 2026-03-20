//! Unified syscall service layer for MaiOS.
//!
//! This crate is the single source of truth for all syscall implementations.
//! Both `linux_syscall` and `windows_syscall` are thin translation layers
//! that map their respective ABI-specific syscall numbers to MaiOS native
//! numbers, then call `dispatch()` here.
//!
//! ## Architecture
//!
//! ```text
//! Linux ELF → linux_syscall (translate nr + args) ─┐
//! Windows PE → windows_syscall (translate + adapt) ─┼→ maios_syscall::dispatch(nr)
//! MaiOS native → direct call ──────────────────────┘        │
//!                                              SYSCALL_TABLE[nr] → handler fn
//! ```
//!
//! ## Performance
//!
//! - Dispatch is O(1) via a flat function pointer array
//! - No locks on the dispatch path (ExecMode is AtomicU8 in Task struct)
//! - Single resource table per task (replaces separate fd_table + handle_table)

#![no_std]

extern crate alloc;

pub mod error;
pub mod resource;
pub mod file_io;
pub mod memory;
pub mod process;
pub mod time;
pub mod system;
pub mod event_io;
pub mod extended;
pub mod futex;
pub mod socket;
pub mod signals;
pub mod trace;

use core::sync::atomic::{AtomicBool, Ordering};
use error::{SyscallResult, SyscallError};
use log::{debug, warn};

// ---------------------------------------------------------------------------
// MaiOS native syscall numbers
// ---------------------------------------------------------------------------

/// MaiOS native syscall numbers, organized by category.
///
/// Each category occupies a 256-slot range (0xCC00..0xCCFF).
/// This avoids collision with both Linux (0-335+) and NT (0x0000-0x00FF) numbers.
pub mod nr {
    // === 0x00xx: Process & Thread ===
    pub const SYS_EXIT: u16             = 0x0000;
    pub const SYS_GETPID: u16          = 0x0001;
    pub const SYS_GETPPID: u16         = 0x0002;
    pub const SYS_GETTID: u16          = 0x0003;
    pub const SYS_EXECVE: u16          = 0x0004;
    pub const SYS_SPAWN: u16           = 0x0005;
    pub const SYS_KILL: u16            = 0x0006;
    pub const SYS_WAIT: u16            = 0x0007;
    pub const SYS_EXIT_GROUP: u16      = 0x0008;
    pub const SYS_GETUID: u16          = 0x0009;
    pub const SYS_GETGID: u16          = 0x000A;
    pub const SYS_GETEUID: u16         = 0x000B;
    pub const SYS_GETEGID: u16         = 0x000C;
    pub const SYS_SET_TID_ADDRESS: u16 = 0x000D;
    pub const SYS_SET_ROBUST_LIST: u16 = 0x000E;
    pub const SYS_PRLIMIT64: u16       = 0x000F;

    // === 0x01xx: Memory ===
    pub const SYS_MMAP: u16            = 0x0100;
    pub const SYS_MUNMAP: u16          = 0x0101;
    pub const SYS_MPROTECT: u16        = 0x0102;
    pub const SYS_BRK: u16             = 0x0103;
    pub const SYS_ALLOC_VM: u16        = 0x0104;
    pub const SYS_FREE_VM: u16         = 0x0105;

    // === 0x02xx: File I/O ===
    pub const SYS_READ: u16            = 0x0200;
    pub const SYS_WRITE: u16           = 0x0201;
    pub const SYS_OPEN: u16            = 0x0202;
    pub const SYS_CLOSE: u16           = 0x0203;
    pub const SYS_STAT: u16            = 0x0204;
    pub const SYS_FSTAT: u16           = 0x0205;
    pub const SYS_LSEEK: u16           = 0x0206;
    pub const SYS_IOCTL: u16           = 0x0207;
    pub const SYS_DUP: u16             = 0x0208;
    pub const SYS_DUP2: u16            = 0x0209;
    pub const SYS_PIPE: u16            = 0x020A;
    pub const SYS_OPENAT: u16          = 0x020B;
    pub const SYS_FCNTL: u16           = 0x020C;
    pub const SYS_WRITEV: u16          = 0x020D;
    pub const SYS_READV: u16           = 0x020E;
    pub const SYS_PREAD64: u16         = 0x020F;
    pub const SYS_ACCESS: u16          = 0x0210;
    pub const SYS_PIPE2: u16           = 0x0211;
    pub const SYS_DUP3: u16            = 0x0212;
    pub const SYS_GETCWD: u16          = 0x0213;
    pub const SYS_GETDENTS64: u16      = 0x0214;
    pub const SYS_CHDIR: u16           = 0x0215;
    pub const SYS_MKDIR: u16           = 0x0216;
    pub const SYS_UNLINK: u16          = 0x0217;
    pub const SYS_READLINK: u16        = 0x0218;
    pub const SYS_NEWFSTATAT: u16      = 0x0219;
    pub const SYS_FACCESSAT: u16       = 0x021A;
    pub const SYS_PWRITE64: u16        = 0x021B;

    // === 0x03xx: Time ===
    pub const SYS_CLOCK_GETTIME: u16   = 0x0300;
    pub const SYS_NANOSLEEP: u16       = 0x0301;
    pub const SYS_PERF_COUNTER: u16    = 0x0302;

    // === 0x04xx: System Info & Signals ===
    pub const SYS_UNAME: u16           = 0x0400;
    pub const SYS_ARCH_PRCTL: u16      = 0x0401;
    pub const SYS_GETRANDOM: u16       = 0x0402;
    pub const SYS_RT_SIGACTION: u16    = 0x0403;
    pub const SYS_RT_SIGPROCMASK: u16  = 0x0404;
    pub const SYS_RT_SIGRETURN: u16    = 0x0405;
    pub const SYS_SCHED_YIELD: u16     = 0x0406;
    pub const SYS_GETTIMEOFDAY: u16    = 0x0407;
    pub const SYS_CLOCK_GETRES: u16    = 0x0408;
    pub const SYS_SCHED_GETAFFINITY: u16 = 0x0409;
    pub const SYS_PRCTL: u16           = 0x040A;
    pub const SYS_MADVISE: u16         = 0x040B;
    pub const SYS_POLL: u16            = 0x040C;
    pub const SYS_EPOLL_CREATE1: u16   = 0x040D;
    pub const SYS_EPOLL_CTL: u16       = 0x040E;
    pub const SYS_EPOLL_WAIT: u16      = 0x040F;
    pub const SYS_WAIT4: u16           = 0x0410;
    pub const SYS_TGKILL: u16         = 0x0411;
    pub const SYS_SYSINFO: u16         = 0x0412;
    pub const SYS_GETRUSAGE: u16       = 0x0413;
    pub const SYS_UMASK: u16           = 0x0414;
    pub const SYS_FTRUNCATE: u16       = 0x0415;
    pub const SYS_MREMAP: u16          = 0x0416;
    pub const SYS_EVENTFD2: u16        = 0x0417;
    pub const SYS_RENAME: u16          = 0x0418;
    pub const SYS_SOCKET: u16          = 0x0500;
    pub const SYS_CONNECT: u16         = 0x0501;
    pub const SYS_SENDTO: u16          = 0x0502;
    pub const SYS_RECVFROM: u16        = 0x0503;
    pub const SYS_BIND: u16            = 0x0504;
    pub const SYS_LISTEN: u16          = 0x0505;
    pub const SYS_ACCEPT4: u16         = 0x0506;
    pub const SYS_SETSOCKOPT: u16      = 0x0507;
    pub const SYS_GETSOCKOPT: u16      = 0x0508;
    pub const SYS_SHUTDOWN: u16        = 0x0509;
    pub const SYS_GETSOCKNAME: u16     = 0x050A;
    pub const SYS_GETPEERNAME: u16     = 0x050B;
    pub const SYS_SOCKETPAIR: u16      = 0x050C;
    pub const SYS_SENDMSG: u16         = 0x050D;
    pub const SYS_RECVMSG: u16         = 0x050E;

    // === 0x06xx: Synchronization ===
    pub const SYS_FUTEX: u16           = 0x0600;

    // === 0x07xx: Extended System ===
    pub const SYS_SELECT: u16          = 0x0700;
    pub const SYS_STATFS: u16          = 0x0701;
    pub const SYS_FSTATFS: u16         = 0x0702;
    pub const SYS_PERSONALITY: u16     = 0x0703;
    pub const SYS_MEMFD_CREATE: u16    = 0x0704;
    pub const SYS_TIMERFD_CREATE: u16  = 0x0705;
    pub const SYS_TIMERFD_SETTIME: u16 = 0x0706;
    pub const SYS_TIMERFD_GETTIME: u16 = 0x0707;
    pub const SYS_SIGNALFD4: u16       = 0x0708;
    pub const SYS_PPOLL: u16           = 0x0709;
    pub const SYS_PSELECT6: u16        = 0x070A;
    pub const SYS_CLONE: u16           = 0x070B;
    pub const SYS_MINCORE: u16         = 0x070C;
    pub const SYS_MSYNC: u16           = 0x070D;

    // === 0x08xx: MaiOS-specific (future) ===
    pub const SYS_CREATE_WINDOW: u16   = 0x0800;
    pub const SYS_DESTROY_WINDOW: u16  = 0x0801;
    pub const SYS_MAP_FRAMEBUFFER: u16 = 0x0802;
    pub const SYS_PRESENT: u16         = 0x0803;
    pub const SYS_GET_EVENT: u16       = 0x0804;
    pub const SYS_AUDIO_WRITE: u16     = 0x0805;
}

// ---------------------------------------------------------------------------
// Syscall descriptor & table
// ---------------------------------------------------------------------------

/// Metadata for a single syscall entry.
///
/// Used for tracing, debugging, and strace-like output.
#[allow(dead_code)]
pub struct SyscallDescriptor {
    /// Human-readable name (e.g., "sys_read").
    pub name: &'static str,
    /// Number of meaningful arguments (0..6).
    pub arg_count: u8,
    /// Behavioral flags.
    pub flags: u8,
}

/// Syscall descriptor flags.
pub const FLAG_NORETURN: u8 = 1 << 0;
#[allow(dead_code)]
pub const FLAG_BLOCKING: u8 = 1 << 1;

/// The handler function signature.
///
/// All 6 arguments are always passed; unused ones are ignored by the handler.
pub type SyscallHandler = fn(u64, u64, u64, u64, u64, u64) -> SyscallResult;

/// Maximum syscall number + 1. Sized to cover all categories up to 0x08FF.
/// 0x0900 = 2304 entries × 8 bytes (Option<fn ptr>) ≈ 18 KB — acceptable.
const MAX_SYSCALL_NR: usize = 0x0900;

/// The global function pointer table.
///
/// Indexed by MaiOS syscall number for O(1) dispatch.
/// `None` entries indicate unimplemented syscalls.
///
/// # Safety
///
/// This table is written once during `init()` (single-threaded boot)
/// and then only read from the syscall hot path. The `AtomicBool` guard
/// ensures no concurrent access during initialization.
static mut SYSCALL_TABLE: [Option<SyscallHandler>; MAX_SYSCALL_NR] =
    [None; MAX_SYSCALL_NR];

/// Parallel table storing the arg_count for each syscall (used by tracing).
static mut SYSCALL_ARG_COUNT: [u8; MAX_SYSCALL_NR] = [0; MAX_SYSCALL_NR];

/// Whether the syscall table has been initialized.
static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Runtime toggle for syscall tracing via COM1 serial.
///
/// When enabled, every syscall logs its name, arguments, and return value
/// directly to COM1. Can be toggled at any time without recompilation.
///
/// Overhead when disabled: single `AtomicBool::load(Relaxed)` ≈ 1 cycle.
static TRACE_ENABLED: AtomicBool = AtomicBool::new(false);

/// Enable syscall tracing at runtime.
pub fn enable_trace() {
    TRACE_ENABLED.store(true, Ordering::Relaxed);
    log::info!("maios_syscall: syscall tracing ENABLED (COM1)");
}

/// Disable syscall tracing at runtime.
pub fn disable_trace() {
    TRACE_ENABLED.store(false, Ordering::Relaxed);
    log::info!("maios_syscall: syscall tracing DISABLED");
}

/// Check whether syscall tracing is currently enabled.
pub fn is_trace_enabled() -> bool {
    TRACE_ENABLED.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Registration & initialization
// ---------------------------------------------------------------------------

/// Register a syscall handler in the table.
///
/// # Safety
/// Must only be called during `init()` before `INITIALIZED` is set.
unsafe fn register(nr: u16, handler: SyscallHandler, _name: &'static str, arg_count: u8, _flags: u8) {
    let idx = nr as usize;
    debug_assert!(idx < MAX_SYSCALL_NR, "syscall number {:#x} exceeds table size", nr);
    SYSCALL_TABLE[idx] = Some(handler);
    SYSCALL_ARG_COUNT[idx] = arg_count;
}

/// Initialize the unified syscall table.
///
/// Must be called once during boot, before any syscall can fire.
/// Populates all function pointer entries for implemented syscalls.
pub fn init() {
    if INITIALIZED.load(Ordering::SeqCst) {
        return;
    }

    log::info!("maios_syscall: initializing unified syscall table...");

    unsafe {
        // --- Process & Thread (0x00xx) ---
        register(nr::SYS_EXIT,       process::sys_exit,       "sys_exit",       1, FLAG_NORETURN);
        register(nr::SYS_GETPID,     process::sys_getpid,     "sys_getpid",     0, 0);
        register(nr::SYS_GETPPID,    process::sys_getppid,    "sys_getppid",    0, 0);
        register(nr::SYS_GETTID,     process::sys_gettid,     "sys_gettid",     0, 0);
        register(nr::SYS_EXECVE,     process::sys_execve,     "sys_execve",     3, FLAG_NORETURN);
        register(nr::SYS_EXIT_GROUP, process::sys_exit_group, "sys_exit_group", 1, FLAG_NORETURN);
        register(nr::SYS_GETUID,     process::sys_getuid,     "sys_getuid",     0, 0);
        register(nr::SYS_GETGID,     process::sys_getgid,     "sys_getgid",     0, 0);
        register(nr::SYS_GETEUID,    process::sys_geteuid,    "sys_geteuid",    0, 0);
        register(nr::SYS_GETEGID,    process::sys_getegid,    "sys_getegid",    0, 0);
        register(nr::SYS_SET_TID_ADDRESS, process::sys_set_tid_address, "sys_set_tid_address", 1, 0);
        register(nr::SYS_SET_ROBUST_LIST, process::sys_set_robust_list, "sys_set_robust_list", 2, 0);
        register(nr::SYS_PRLIMIT64, process::sys_prlimit64, "sys_prlimit64",  4, 0);
        register(nr::SYS_SCHED_YIELD, process::sys_sched_yield, "sys_sched_yield", 0, 0);
        register(nr::SYS_GETTIMEOFDAY, process::sys_gettimeofday, "sys_gettimeofday", 2, 0);

        // --- Memory (0x01xx) ---
        register(nr::SYS_MMAP,       memory::sys_mmap,        "sys_mmap",       6, 0);
        register(nr::SYS_MUNMAP,     memory::sys_munmap,      "sys_munmap",     2, 0);
        register(nr::SYS_MPROTECT,   memory::sys_mprotect,    "sys_mprotect",   3, 0);
        register(nr::SYS_BRK,        memory::sys_brk,         "sys_brk",        1, 0);
        register(nr::SYS_ALLOC_VM,   memory::sys_alloc_vm,    "sys_alloc_vm",   3, 0);
        register(nr::SYS_FREE_VM,    memory::sys_free_vm,     "sys_free_vm",    2, 0);

        // --- File I/O (0x02xx) ---
        register(nr::SYS_READ,       file_io::sys_read,       "sys_read",       3, 0);
        register(nr::SYS_WRITE,      file_io::sys_write,      "sys_write",      3, 0);
        register(nr::SYS_OPEN,       file_io::sys_open,       "sys_open",       3, 0);
        register(nr::SYS_CLOSE,      file_io::sys_close,      "sys_close",      1, 0);
        register(nr::SYS_STAT,       file_io::sys_stat,       "sys_stat",       2, 0);
        register(nr::SYS_FSTAT,      file_io::sys_fstat,      "sys_fstat",      2, 0);
        register(nr::SYS_LSEEK,      file_io::sys_lseek,      "sys_lseek",      3, 0);
        register(nr::SYS_IOCTL,      file_io::sys_ioctl,      "sys_ioctl",      3, 0);
        register(nr::SYS_DUP,        file_io::sys_dup,        "sys_dup",        1, 0);
        register(nr::SYS_DUP2,       file_io::sys_dup2,       "sys_dup2",       2, 0);
        register(nr::SYS_PIPE,       file_io::sys_pipe,       "sys_pipe",       1, 0);
        register(nr::SYS_OPENAT,     file_io::sys_openat,     "sys_openat",     4, 0);
        register(nr::SYS_FCNTL,      file_io::sys_fcntl,      "sys_fcntl",      3, 0);
        register(nr::SYS_WRITEV,     file_io::sys_writev,     "sys_writev",     3, 0);
        register(nr::SYS_READV,      file_io::sys_readv,      "sys_readv",      3, 0);
        register(nr::SYS_PREAD64,    file_io::sys_pread64,    "sys_pread64",    4, 0);
        register(nr::SYS_ACCESS,     file_io::sys_access,     "sys_access",     2, 0);
        register(nr::SYS_PIPE2,      file_io::sys_pipe2,      "sys_pipe2",      2, 0);
        register(nr::SYS_DUP3,       file_io::sys_dup3,       "sys_dup3",       3, 0);
        register(nr::SYS_GETCWD,     file_io::sys_getcwd,     "sys_getcwd",     2, 0);
        register(nr::SYS_GETDENTS64, file_io::sys_getdents64, "sys_getdents64", 3, 0);
        register(nr::SYS_CHDIR,      file_io::sys_chdir,      "sys_chdir",      1, 0);
        register(nr::SYS_MKDIR,      file_io::sys_mkdir,      "sys_mkdir",      2, 0);
        register(nr::SYS_UNLINK,     file_io::sys_unlink,     "sys_unlink",     1, 0);
        register(nr::SYS_READLINK,   file_io::sys_readlink,   "sys_readlink",   3, 0);
        register(nr::SYS_NEWFSTATAT, file_io::sys_newfstatat, "sys_newfstatat", 4, 0);
        register(nr::SYS_FACCESSAT,  file_io::sys_faccessat,  "sys_faccessat",  4, 0);
        register(nr::SYS_PWRITE64,   file_io::sys_pwrite64,   "sys_pwrite64",   4, 0);

        // --- Time (0x03xx) ---
        register(nr::SYS_CLOCK_GETTIME, time::sys_clock_gettime, "sys_clock_gettime", 2, 0);
        register(nr::SYS_NANOSLEEP,     time::sys_nanosleep,     "sys_nanosleep",     2, 0);
        register(nr::SYS_PERF_COUNTER,  time::sys_perf_counter,  "sys_perf_counter",  2, 0);

        // --- System Info (0x04xx) ---
        register(nr::SYS_UNAME,       system::sys_uname,       "sys_uname",       1, 0);
        register(nr::SYS_ARCH_PRCTL,   system::sys_arch_prctl,  "sys_arch_prctl",  2, 0);
        register(nr::SYS_GETRANDOM,    system::sys_getrandom,   "sys_getrandom",   3, 0);
        register(nr::SYS_RT_SIGACTION, signals::sys_rt_sigaction, "sys_rt_sigaction", 4, 0);
        register(nr::SYS_RT_SIGPROCMASK, signals::sys_rt_sigprocmask, "sys_rt_sigprocmask", 4, 0);
        register(nr::SYS_RT_SIGRETURN, signals::sys_rt_sigreturn, "sys_rt_sigreturn", 0, 0);
        register(nr::SYS_CLOCK_GETRES, system::sys_clock_getres, "sys_clock_getres", 2, 0);
        register(nr::SYS_SCHED_GETAFFINITY, system::sys_sched_getaffinity, "sys_sched_getaffinity", 3, 0);
        register(nr::SYS_PRCTL,       system::sys_prctl,       "sys_prctl",       5, 0);
        register(nr::SYS_MADVISE,     system::sys_madvise,     "sys_madvise",     3, 0);
        register(nr::SYS_POLL,        event_io::sys_poll,      "sys_poll",        3, 0);
        register(nr::SYS_EPOLL_CREATE1, event_io::sys_epoll_create1, "sys_epoll_create1", 1, 0);
        register(nr::SYS_EPOLL_CTL,   event_io::sys_epoll_ctl, "sys_epoll_ctl",  4, 0);
        register(nr::SYS_EPOLL_WAIT,  event_io::sys_epoll_wait, "sys_epoll_wait", 4, 0);
        register(nr::SYS_KILL,        process::sys_kill,       "sys_kill",         2, 0);
        register(nr::SYS_WAIT4,       process::sys_wait4,      "sys_wait4",       4, 0);
        register(nr::SYS_TGKILL,      process::sys_tgkill,     "sys_tgkill",      3, 0);
        register(nr::SYS_SYSINFO,     system::sys_sysinfo,     "sys_sysinfo",     1, 0);
        register(nr::SYS_GETRUSAGE,   system::sys_getrusage,   "sys_getrusage",   2, 0);
        register(nr::SYS_UMASK,       system::sys_umask,       "sys_umask",       1, 0);
        register(nr::SYS_EVENTFD2,    system::sys_eventfd2,    "sys_eventfd2",    2, 0);
        register(nr::SYS_FTRUNCATE,   file_io::sys_ftruncate,  "sys_ftruncate",   2, 0);
        register(nr::SYS_RENAME,      file_io::sys_rename,     "sys_rename",      2, 0);
        register(nr::SYS_MREMAP,      memory::sys_mremap,      "sys_mremap",      4, 0);

        // --- Sockets (0x05xx) — stubs ---
        register(nr::SYS_SOCKET,      socket::sys_socket,      "sys_socket",      3, 0);
        register(nr::SYS_CONNECT,     socket::sys_connect,     "sys_connect",     3, 0);
        register(nr::SYS_SENDTO,      socket::sys_sendto,      "sys_sendto",      6, 0);
        register(nr::SYS_RECVFROM,    socket::sys_recvfrom,    "sys_recvfrom",    6, 0);
        register(nr::SYS_BIND,        socket::sys_bind,        "sys_bind",        3, 0);
        register(nr::SYS_LISTEN,      socket::sys_listen,      "sys_listen",      2, 0);
        register(nr::SYS_ACCEPT4,     socket::sys_accept4,     "sys_accept4",     4, 0);
        register(nr::SYS_SETSOCKOPT,  socket::sys_setsockopt,  "sys_setsockopt",  5, 0);
        register(nr::SYS_GETSOCKOPT,  socket::sys_getsockopt,  "sys_getsockopt",  5, 0);
        register(nr::SYS_SHUTDOWN,    socket::sys_shutdown,    "sys_shutdown",    2, 0);
        register(nr::SYS_GETSOCKNAME, socket::sys_getsockname, "sys_getsockname", 3, 0);
        register(nr::SYS_GETPEERNAME, socket::sys_getpeername, "sys_getpeername", 3, 0);
        register(nr::SYS_SOCKETPAIR,  socket::sys_socketpair,  "sys_socketpair",  4, 0);
        register(nr::SYS_SENDMSG,     socket::sys_sendmsg,     "sys_sendmsg",     3, 0);
        register(nr::SYS_RECVMSG,     socket::sys_recvmsg,     "sys_recvmsg",     3, 0);

        // --- Synchronization (0x06xx) ---
        register(nr::SYS_FUTEX,       futex::sys_futex,        "sys_futex",       6, 0);

        // --- Extended (0x07xx) ---
        register(nr::SYS_SELECT,       extended::sys_select,       "sys_select",       5, 0);
        register(nr::SYS_STATFS,       extended::sys_statfs,       "sys_statfs",       2, 0);
        register(nr::SYS_FSTATFS,      extended::sys_fstatfs,      "sys_fstatfs",      2, 0);
        register(nr::SYS_PERSONALITY,  extended::sys_personality,  "sys_personality",  1, 0);
        register(nr::SYS_MEMFD_CREATE, extended::sys_memfd_create, "sys_memfd_create", 2, 0);
        register(nr::SYS_TIMERFD_CREATE, extended::sys_timerfd_create, "sys_timerfd_create", 2, 0);
        register(nr::SYS_TIMERFD_SETTIME, extended::sys_timerfd_settime, "sys_timerfd_settime", 4, 0);
        register(nr::SYS_TIMERFD_GETTIME, extended::sys_timerfd_gettime, "sys_timerfd_gettime", 2, 0);
        register(nr::SYS_SIGNALFD4,    extended::sys_signalfd4,    "sys_signalfd4",    4, 0);
        register(nr::SYS_PPOLL,        extended::sys_ppoll,        "sys_ppoll",        5, 0);
        register(nr::SYS_PSELECT6,     extended::sys_pselect6,     "sys_pselect6",     6, 0);
        register(nr::SYS_CLONE,        extended::sys_clone,        "sys_clone",        5, FLAG_NORETURN);
        register(nr::SYS_MINCORE,      extended::sys_mincore,      "sys_mincore",      3, 0);
        register(nr::SYS_MSYNC,        extended::sys_msync,        "sys_msync",        3, 0);
    }

    INITIALIZED.store(true, Ordering::SeqCst);

    // Count registered syscalls for the log message.
    let count = unsafe {
        SYSCALL_TABLE.iter().filter(|e| e.is_some()).count()
    };
    log::info!("maios_syscall: {} syscalls registered in unified table", count);
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Dispatch a MaiOS-native syscall number.
///
/// Performs an O(1) array lookup into `SYSCALL_TABLE` and calls the handler.
/// Returns `Err(NotImplemented)` for unknown or unregistered syscalls.
///
/// # Arguments
///
/// - `nr`: MaiOS syscall number (0x0000..0x08FF)
/// - `a0..a5`: Up to 6 arguments passed from the ABI-specific translation layer
pub fn dispatch(nr: u16, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> SyscallResult {
    let idx = nr as usize;
    if idx >= MAX_SYSCALL_NR {
        warn!("maios_syscall: syscall number {:#x} out of range", nr);
        return Err(SyscallError::NotImplemented);
    }

    // Safety: SYSCALL_TABLE is initialized before any syscall can fire,
    // and is only read after initialization.
    let handler = unsafe { SYSCALL_TABLE[idx] };

    // Runtime trace: ~1 cycle overhead when disabled (single atomic load)
    if TRACE_ENABLED.load(Ordering::Relaxed) && trace::should_trace(nr) {
        let arg_count = unsafe { SYSCALL_ARG_COUNT[idx] };
        let args = [a0, a1, a2, a3, a4, a5];
        trace::trace_entry(nr, &args, arg_count);
    }

    let result = match handler {
        Some(f) => f(a0, a1, a2, a3, a4, a5),
        None => {
            warn!("maios_syscall: unimplemented syscall {:#06x}", nr);
            Err(SyscallError::NotImplemented)
        }
    };

    if TRACE_ENABLED.load(Ordering::Relaxed) && trace::should_trace(nr) {
        trace::trace_exit(nr, &result);
    }

    result
}

/// Get the name of a syscall by its number, for tracing purposes.
pub fn syscall_name(nr: u16) -> &'static str {
    trace::syscall_name(nr)
}
