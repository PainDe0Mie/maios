//! Signal handling stubs for MaiOS.
//!
//! Signals are not fully implemented yet. These stubs exist so that
//! glibc/musl don't crash at startup — they call rt_sigaction and
//! rt_sigprocmask during initialization, and expect them to succeed.
//!
//! A full signal implementation would require:
//! 1. Per-task signal mask and pending signal bitmap
//! 2. Signal delivery on return-to-userspace
//! 3. sigaltstack support
//! 4. Signal frame setup/restore on the user stack
//!
//! For now, we just record the handlers silently and never deliver signals.

use crate::error::{SyscallResult, SyscallError};

/// Maximum signal number (Linux: SIGRTMAX = 64, _NSIG = 65).
const MAX_SIGNALS: usize = 65;

/// sys_rt_sigaction — install a signal handler.
///
/// Stub: accepts and silently ignores the handler. glibc calls this
/// at startup for SIGSEGV, SIGFPE, SIGBUS, etc.
///
/// # Arguments
/// - `signum`: signal number (1..64)
/// - `act`: pointer to `struct sigaction` (new handler), can be NULL
/// - `oldact`: pointer to `struct sigaction` (previous handler), can be NULL
/// - `sigsetsize`: size of signal set (must be 8 on x86_64)
pub fn sys_rt_sigaction(signum: u64, _act: u64, oldact: u64, sigsetsize: u64, _: u64, _: u64) -> SyscallResult {
    if signum == 0 || signum as usize >= MAX_SIGNALS {
        return Err(SyscallError::InvalidArgument);
    }
    // SIGKILL (9) and SIGSTOP (19) cannot have handlers
    if signum == 9 || signum == 19 {
        return Err(SyscallError::InvalidArgument);
    }
    if sigsetsize != 8 {
        return Err(SyscallError::InvalidArgument);
    }

    // If oldact is provided, zero it out (no previous handler)
    if oldact != 0 {
        // struct sigaction on x86_64 is 32 bytes (sa_handler + sa_flags + sa_restorer + sa_mask)
        unsafe {
            core::ptr::write_bytes(oldact as *mut u8, 0, 32);
        }
    }

    // We accept and silently ignore the new handler.
    // TODO: store handlers in per-task signal table for future signal delivery.
    Ok(0)
}

/// sys_rt_sigprocmask — block/unblock signals.
///
/// Stub: accepts and silently ignores the mask change.
/// glibc calls this during thread creation and around critical sections.
///
/// # Arguments
/// - `how`: SIG_BLOCK (0), SIG_UNBLOCK (1), SIG_SETMASK (2)
/// - `set`: pointer to new signal mask (can be NULL = query only)
/// - `oldset`: pointer to store previous mask (can be NULL)
/// - `sigsetsize`: must be 8
pub fn sys_rt_sigprocmask(how: u64, _set: u64, oldset: u64, sigsetsize: u64, _: u64, _: u64) -> SyscallResult {
    if sigsetsize != 8 {
        return Err(SyscallError::InvalidArgument);
    }
    if how > 2 {
        return Err(SyscallError::InvalidArgument);
    }

    // Return empty old mask if requested
    if oldset != 0 {
        unsafe {
            *(oldset as *mut u64) = 0;
        }
    }

    // Accept and silently ignore the mask change.
    Ok(0)
}

/// sys_rt_sigreturn — return from a signal handler.
///
/// Stub: this should never be called since we never deliver signals.
/// If it is called, just return success.
pub fn sys_rt_sigreturn(_: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    // In a real implementation, this would restore the pre-signal register state
    // from the signal frame on the user stack.
    Ok(0)
}
