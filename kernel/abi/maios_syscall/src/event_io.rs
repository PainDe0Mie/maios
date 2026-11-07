//! Event-driven I/O syscalls for MaiOS.
//!
//! Implements poll, epoll_create1, epoll_ctl, epoll_wait.
//! These are the backbone of every GUI event loop (SDL2, X11, Wayland).
//!
//! Current implementation: simplified stubs that handle the common case
//! of "poll with a timeout" (used as a sleep-then-check pattern).
//! Full implementation would need a per-fd readiness notification system.

use crate::error::{SyscallResult, SyscallError};

/// Linux `struct pollfd` layout.
#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,   // requested events
    revents: i16,  // returned events
}

// poll event flags
const POLLIN: i16 = 0x0001;
const POLLOUT: i16 = 0x0004;
const POLLERR: i16 = 0x0008;
const POLLHUP: i16 = 0x0010;
const POLLNVAL: i16 = 0x0020;

/// sys_poll — wait for events on file descriptors.
///
/// Simplified implementation:
/// - If timeout_ms > 0: sleep for the timeout, then report all fds as ready
/// - If timeout_ms == 0: non-blocking check, report stdin/stdout ready
/// - If timeout_ms < 0: block indefinitely (yield loop)
///
/// This is sufficient for programs that use poll as a "sleep + check input" pattern,
/// which is how most game loops work.
pub fn sys_poll(fds_ptr: u64, nfds: u64, timeout_ms: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    let timeout_ms = timeout_ms as i32;

    if fds_ptr == 0 && nfds > 0 {
        return Err(SyscallError::Fault);
    }

    // Sleep for the requested timeout
    if timeout_ms > 0 {
        let duration = core::time::Duration::from_millis(timeout_ms as u64);
        let deadline = time::Instant::now() + duration;
        while time::Instant::now() < deadline {
            scheduler::schedule();
        }
    } else if timeout_ms == 0 {
        // Non-blocking: just check and return
    }
    // timeout_ms < 0: would block forever, but we just return immediately
    // to avoid hanging. TODO: implement proper blocking with wait queues.

    // Set revents for each fd: report stdout/stderr as writable, stdin as readable
    let mut ready_count: u64 = 0;
    if nfds > 0 && fds_ptr != 0 {
        let fds = unsafe {
            core::slice::from_raw_parts_mut(fds_ptr as *mut PollFd, nfds as usize)
        };
        for pfd in fds.iter_mut() {
            if pfd.fd < 0 {
                pfd.revents = 0;
                continue;
            }

            let mut revents: i16 = 0;
            match pfd.fd as u64 {
                0 => {
                    // stdin: report readable if POLLIN requested
                    if pfd.events & POLLIN != 0 {
                        revents |= POLLIN;
                    }
                }
                1 | 2 => {
                    // stdout/stderr: always writable
                    if pfd.events & POLLOUT != 0 {
                        revents |= POLLOUT;
                    }
                }
                _ => {
                    // Unknown fd: report as invalid
                    revents = POLLNVAL;
                }
            }

            pfd.revents = revents;
            if revents != 0 {
                ready_count += 1;
            }
        }
    }

    Ok(ready_count)
}

// =============================================================================
// epoll — scalable I/O event notification
// =============================================================================

/// sys_epoll_create1 — create an epoll instance.
///
/// Stub: returns a fake fd. Real implementation would need an internal
/// epoll registry (BTreeMap<fd, events>) per epoll instance.
pub fn sys_epoll_create1(_flags: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    // Return a fake fd (high number to avoid collisions)
    // TODO: allocate a real Resource::Epoll in the ResourceTable
    Ok(1000)
}

/// sys_epoll_ctl — control an epoll instance.
///
/// Stub: accept and silently ignore all operations.
pub fn sys_epoll_ctl(_epfd: u64, _op: u64, _fd: u64, _event: u64, _: u64, _: u64) -> SyscallResult {
    // EPOLL_CTL_ADD=1, EPOLL_CTL_DEL=2, EPOLL_CTL_MOD=3
    Ok(0)
}

/// sys_epoll_wait — wait for events on an epoll instance.
///
/// Stub: sleep for the timeout and return 0 events.
/// Real implementation would check registered fds for readiness.
pub fn sys_epoll_wait(_epfd: u64, _events: u64, _maxevents: u64, timeout_ms: u64, _: u64, _: u64) -> SyscallResult {
    let timeout_ms = timeout_ms as i32;

    if timeout_ms > 0 {
        let duration = core::time::Duration::from_millis(timeout_ms as u64);
        let deadline = time::Instant::now() + duration;
        while time::Instant::now() < deadline {
            scheduler::schedule();
        }
    }

    // Return 0 = no events (timeout)
    Ok(0)
}
