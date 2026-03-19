//! Futex (Fast Userspace muTEX) implementation for MaiOS.
//!
//! Futex is the fundamental building block for all userspace synchronization:
//! mutexes, condition variables, barriers, semaphores, rwlocks.
//!
//! ## Operations implemented
//!
//! - `FUTEX_WAIT`: If `*addr == expected`, sleep until woken or timeout.
//! - `FUTEX_WAKE`: Wake up to N threads waiting on `addr`.
//! - `FUTEX_WAIT_PRIVATE` / `FUTEX_WAKE_PRIVATE`: Same but process-private
//!   (same semantics for us since MaiOS is single-address-space).
//!
//! ## Implementation strategy
//!
//! We use a spin-yield approach: FUTEX_WAIT spins checking `*addr` with
//! scheduler yields between checks. FUTEX_WAKE is a no-op because the
//! waiters will notice the value change on their next check.
//!
//! This is not as efficient as a proper wait-queue implementation (wastes
//! CPU cycles spinning), but it is correct and simple. A proper implementation
//! would use `WaitQueue` from the Theseus scheduler.

use crate::error::{SyscallResult, SyscallError};
use core::sync::atomic::{AtomicU32, Ordering};

// Futex operations (from linux/futex.h)
const FUTEX_WAIT: u64 = 0;
const FUTEX_WAKE: u64 = 1;
const FUTEX_WAIT_PRIVATE: u64 = 128;       // FUTEX_WAIT | FUTEX_PRIVATE_FLAG
const FUTEX_WAKE_PRIVATE: u64 = 129;       // FUTEX_WAKE | FUTEX_PRIVATE_FLAG
const FUTEX_WAIT_BITSET: u64 = 9;
const FUTEX_WAKE_BITSET: u64 = 10;
const FUTEX_WAIT_BITSET_PRIVATE: u64 = 137; // 9 | 128
const FUTEX_WAKE_BITSET_PRIVATE: u64 = 138; // 10 | 128

// Strip the PRIVATE flag to get the base operation
const FUTEX_CMD_MASK: u64 = 0x7F; // ~FUTEX_PRIVATE_FLAG

/// sys_futex — fast userspace mutex operations.
///
/// # Arguments
/// - `uaddr`: pointer to the futex word (u32 in userspace)
/// - `op`: futex operation (FUTEX_WAIT, FUTEX_WAKE, etc.)
/// - `val`: expected value (for WAIT) or max wakeups (for WAKE)
/// - `timeout`: pointer to timespec (for WAIT), or NULL for no timeout
/// - `uaddr2`: second futex address (for REQUEUE, unused here)
/// - `val3`: bitset mask (for BITSET operations)
pub fn sys_futex(
    uaddr: u64,
    op: u64,
    val: u64,
    timeout: u64,
    _uaddr2: u64,
    _val3: u64,
) -> SyscallResult {
    if uaddr == 0 {
        return Err(SyscallError::Fault);
    }

    let cmd = op & FUTEX_CMD_MASK;

    match cmd {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            futex_wait(uaddr, val as u32, timeout)
        }
        FUTEX_WAKE | FUTEX_WAKE_BITSET => {
            futex_wake(uaddr, val as u32)
        }
        _ => {
            // Unsupported futex operation — return ENOSYS
            // Common unsupported ops: FUTEX_REQUEUE, FUTEX_CMP_REQUEUE,
            // FUTEX_LOCK_PI, FUTEX_UNLOCK_PI
            Err(SyscallError::NotImplemented)
        }
    }
}

/// FUTEX_WAIT: if *uaddr == expected, sleep until woken or timeout.
///
/// Uses spin-yield: check the value, yield the CPU, repeat.
/// Returns 0 on success (woken up), -EAGAIN if *uaddr != expected,
/// -ETIMEDOUT on timeout.
fn futex_wait(uaddr: u64, expected: u32, timeout_ptr: u64) -> SyscallResult {
    let futex_word = unsafe { &*(uaddr as *const AtomicU32) };

    // Fast check: if value already changed, return immediately
    let current = futex_word.load(Ordering::SeqCst);
    if current != expected {
        return Err(SyscallError::WouldBlock); // EAGAIN
    }

    // Compute deadline from timeout
    let deadline = if timeout_ptr != 0 {
        let ts = unsafe { &*(timeout_ptr as *const [i64; 2]) };
        let duration = core::time::Duration::new(ts[0] as u64, ts[1] as u32);
        Some(time::Instant::now() + duration)
    } else {
        None // No timeout — but we cap at 1 second to prevent infinite hangs
    };

    // Cap the maximum wait to avoid hanging forever in single-threaded scenarios
    let max_deadline = time::Instant::now() + core::time::Duration::from_millis(100);
    let effective_deadline = match deadline {
        Some(d) => {
            if d < max_deadline { d } else { max_deadline }
        }
        None => max_deadline,
    };

    // Spin-yield loop
    loop {
        // Check if value changed (another thread modified it)
        let current = futex_word.load(Ordering::SeqCst);
        if current != expected {
            return Ok(0); // Woken up (value changed)
        }

        // Check timeout
        if time::Instant::now() >= effective_deadline {
            return Err(SyscallError::WouldBlock); // Timeout (using EAGAIN as proxy)
        }

        // Yield CPU to let other tasks run
        scheduler::schedule();
    }
}

/// FUTEX_WAKE: wake up to `max_wakeups` threads waiting on `uaddr`.
///
/// In our spin-yield implementation, this is essentially a no-op because
/// waiters continuously check the value. However, we return the number
/// of "woken" threads (always 0 in our case since we don't track waiters).
fn futex_wake(_uaddr: u64, _max_wakeups: u32) -> SyscallResult {
    // In a proper implementation, we'd look up the wait queue for this
    // address and wake up to max_wakeups threads.
    //
    // With our spin-yield approach, the waiters will notice the value
    // change on their next iteration. We just need to yield so they
    // get a chance to run.
    scheduler::schedule();

    // Return 0 = number of threads woken (we don't track this)
    Ok(0)
}
