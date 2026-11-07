//! C `<signal.h>` stubs for MaiOS.
//!
//! MaiOS doesn't have full POSIX signal support yet, but we provide the
//! function signatures so that C programs (and Mesa) can link.

use libc::{c_int, c_void};

pub type sighandler_t = unsafe extern "C" fn(c_int);

pub const SIG_DFL: sighandler_t = sig_dfl;
pub const SIG_IGN: sighandler_t = sig_ign;

pub const SIGABRT: c_int = 6;
pub const SIGFPE: c_int = 8;
pub const SIGILL: c_int = 4;
pub const SIGINT: c_int = 2;
pub const SIGSEGV: c_int = 11;
pub const SIGTERM: c_int = 15;
pub const SIGPIPE: c_int = 13;
pub const SIGCHLD: c_int = 17;
pub const SIGUSR1: c_int = 10;
pub const SIGUSR2: c_int = 12;

unsafe extern "C" fn sig_dfl(_sig: c_int) {}
unsafe extern "C" fn sig_ign(_sig: c_int) {}

// Store up to 32 signal handlers
static mut HANDLERS: [sighandler_t; 32] = [sig_dfl; 32];

#[no_mangle]
pub unsafe extern "C" fn signal(sig: c_int, handler: sighandler_t) -> sighandler_t {
    if sig < 0 || sig >= 32 {
        return sig_dfl; // SIG_ERR equivalent
    }
    let old = HANDLERS[sig as usize];
    HANDLERS[sig as usize] = handler;
    old
}

#[no_mangle]
pub unsafe extern "C" fn raise(sig: c_int) -> c_int {
    if sig < 0 || sig >= 32 { return -1; }
    let handler = HANDLERS[sig as usize];
    handler(sig);
    0
}

// sigaction struct (simplified)
#[repr(C)]
pub struct sigaction {
    pub sa_handler: sighandler_t,
    pub sa_flags: c_int,
    pub sa_mask: u64, // simplified sigset_t
}

#[no_mangle]
pub unsafe extern "C" fn sigaction(
    sig: c_int,
    act: *const sigaction,
    oldact: *mut sigaction,
) -> c_int {
    if sig < 0 || sig >= 32 { return -1; }
    if !oldact.is_null() {
        (*oldact).sa_handler = HANDLERS[sig as usize];
        (*oldact).sa_flags = 0;
        (*oldact).sa_mask = 0;
    }
    if !act.is_null() {
        HANDLERS[sig as usize] = (*act).sa_handler;
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn sigprocmask(
    _how: c_int,
    _set: *const u64,
    _oldset: *mut u64,
) -> c_int {
    0 // stub
}

#[no_mangle]
pub unsafe extern "C" fn sigemptyset(set: *mut u64) -> c_int {
    if !set.is_null() { *set = 0; }
    0
}

#[no_mangle]
pub unsafe extern "C" fn sigfillset(set: *mut u64) -> c_int {
    if !set.is_null() { *set = !0u64; }
    0
}

#[no_mangle]
pub unsafe extern "C" fn sigaddset(set: *mut u64, signo: c_int) -> c_int {
    if set.is_null() || signo < 0 || signo >= 64 { return -1; }
    *set |= 1u64 << signo;
    0
}

#[no_mangle]
pub unsafe extern "C" fn sigdelset(set: *mut u64, signo: c_int) -> c_int {
    if set.is_null() || signo < 0 || signo >= 64 { return -1; }
    *set &= !(1u64 << signo);
    0
}
