//! MHC Fence — Timeline semaphore synchronization.
//!
//! ## Research basis
//!
//! - **Vulkan 1.2 Timeline Semaphores** (VK_KHR_timeline_semaphore):
//!   a monotonically increasing u64 counter that generalizes both binary
//!   fences and traditional semaphores. Each signal increments the counter;
//!   a wait blocks until the counter reaches a threshold.
//!
//! - This supersedes the traditional fence model (signal once, reset, signal)
//!   used in OpenGL/early Vulkan, providing:
//!   - Multiple wait points on a single semaphore
//!   - CPU-side polling without kernel transitions
//!   - Dependency chains between command buffers
//!
//! ## Implementation
//!
//! Each `TimelineSemaphore` is an `AtomicU64` counter + a list of waiters.
//! Waiters are represented as `Waker`s that integrate with MKS's
//! `wait_condition` infrastructure for zero-overhead blocking.

#![allow(dead_code)]

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;

// ---------------------------------------------------------------------------
// Fence identifier
// ---------------------------------------------------------------------------

/// Globally unique fence identifier.
///
/// Composed of (device_id, local_fence_id) packed into a u64.
/// Bits 48..63: device ID (up to 65536 devices)
/// Bits  0..47: local fence counter (up to 281 trillion fences per device)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FenceId(pub u64);

impl FenceId {
    pub const NONE: FenceId = FenceId(0);

    /// Create a fence ID from device and local IDs.
    #[inline]
    pub fn new(device_id: u16, local_id: u64) -> Self {
        FenceId(((device_id as u64) << 48) | (local_id & 0x0000_FFFF_FFFF_FFFF))
    }

    /// Extract the device ID.
    #[inline]
    pub fn device_id(self) -> u16 { (self.0 >> 48) as u16 }

    /// Extract the local fence counter.
    #[inline]
    pub fn local_id(self) -> u64 { self.0 & 0x0000_FFFF_FFFF_FFFF }

    /// Check if this is the null fence.
    #[inline]
    pub fn is_none(self) -> bool { self.0 == 0 }
}

// ---------------------------------------------------------------------------
// Waiter callback
// ---------------------------------------------------------------------------

/// A callback invoked when a fence reaches a target value.
///
/// In a full MKS integration this would be a `task_struct::Waker`.
/// For now, we use a simple closure to remain self-contained.
pub type WakerFn = alloc::boxed::Box<dyn FnOnce() + Send + 'static>;

struct Waiter {
    threshold: u64,
    callback: WakerFn,
}

// ---------------------------------------------------------------------------
// Timeline semaphore
// ---------------------------------------------------------------------------

/// A monotonically increasing counter with waiter support.
///
/// ## Concurrency model
///
/// - `signal()` is lock-free on the fast path (atomic store + check).
/// - `wait()` acquires the waiter lock only when the value hasn't been reached.
/// - `poll()` is always lock-free (single atomic load).
pub struct TimelineSemaphore {
    /// Current counter value. Only moves forward.
    value: AtomicU64,
    /// Waiters blocked on this semaphore, sorted by threshold.
    waiters: Mutex<Vec<Waiter>>,
}

impl TimelineSemaphore {
    /// Create a new semaphore with initial value 0.
    pub fn new() -> Self {
        TimelineSemaphore {
            value: AtomicU64::new(0),
            waiters: Mutex::new(Vec::new()),
        }
    }

    /// Create a semaphore with a specific initial value.
    pub fn with_value(initial: u64) -> Self {
        TimelineSemaphore {
            value: AtomicU64::new(initial),
            waiters: Mutex::new(Vec::new()),
        }
    }

    /// Current counter value (lock-free read).
    #[inline]
    pub fn value(&self) -> u64 {
        self.value.load(Ordering::Acquire)
    }

    /// Signal the semaphore by advancing the counter.
    ///
    /// Wakes all waiters whose threshold ≤ `new_value`.
    /// The value must be greater than the current value (monotonic).
    pub fn signal(&self, new_value: u64) {
        let old = self.value.fetch_max(new_value, Ordering::AcqRel);
        if new_value <= old {
            return; // Already at or past this value
        }

        // Wake eligible waiters
        let mut waiters = self.waiters.lock();
        let mut i = 0;
        while i < waiters.len() {
            if waiters[i].threshold <= new_value {
                let w = waiters.swap_remove(i);
                (w.callback)();
                // Don't increment i — swap_remove moved the last element here
            } else {
                i += 1;
            }
        }
    }

    /// Poll whether the semaphore has reached `target` (lock-free).
    #[inline]
    pub fn poll(&self, target: u64) -> bool {
        self.value.load(Ordering::Acquire) >= target
    }

    /// Register a callback to be invoked when the semaphore reaches `target`.
    ///
    /// If the semaphore already reached `target`, the callback is invoked
    /// immediately (synchronously).
    pub fn wait_async(&self, target: u64, callback: WakerFn) {
        // Fast path: already signaled
        if self.poll(target) {
            callback();
            return;
        }

        // Slow path: register waiter
        let mut waiters = self.waiters.lock();
        // Double-check after acquiring lock
        if self.poll(target) {
            drop(waiters);
            callback();
            return;
        }
        waiters.push(Waiter { threshold: target, callback });
    }

    /// Blocking wait (spin-polls). Use `wait_async` for non-spinning waits.
    ///
    /// Returns `Ok(())` if signaled, `Err(())` if `timeout_ns` exceeded.
    /// `timeout_ns = u64::MAX` means wait indefinitely.
    pub fn wait_blocking(&self, target: u64, timeout_ns: u64) -> Result<(), ()> {
        if self.poll(target) {
            return Ok(());
        }

        // Simple spin-wait with bounded iterations.
        // In production, integrate with MKS sleep/wakeup.
        let max_spins = if timeout_ns == u64::MAX { u64::MAX } else { timeout_ns / 10 };
        for _ in 0..max_spins {
            if self.poll(target) {
                return Ok(());
            }
            core::hint::spin_loop();
        }

        if self.poll(target) { Ok(()) } else { Err(()) }
    }
}

impl Default for TimelineSemaphore {
    fn default() -> Self { Self::new() }
}

// ---------------------------------------------------------------------------
// Fence pool — per-device fence management
// ---------------------------------------------------------------------------

/// Manages timeline semaphores for a single GPU device.
///
/// Each `submit()` call gets a new fence ID with an associated semaphore.
/// When the device completes the submission, it signals the semaphore.
pub struct FencePool {
    device_id: u16,
    next_local: AtomicU64,
    semaphores: Mutex<BTreeMap<u64, TimelineSemaphore>>,
}

impl FencePool {
    pub fn new(device_id: u16) -> Self {
        FencePool {
            device_id,
            next_local: AtomicU64::new(1),
            semaphores: Mutex::new(BTreeMap::new()),
        }
    }

    /// Allocate a new fence and return its ID.
    pub fn alloc_fence(&self) -> FenceId {
        let local = self.next_local.fetch_add(1, Ordering::Relaxed);
        let id = FenceId::new(self.device_id, local);
        self.semaphores.lock().insert(local, TimelineSemaphore::with_value(0));
        id
    }

    /// Signal a fence (mark submission as complete).
    pub fn signal(&self, fence: FenceId) {
        let local = fence.local_id();
        if let Some(sem) = self.semaphores.lock().get(&local) {
            sem.signal(1);
        }
    }

    /// Poll whether a fence has been signaled.
    pub fn poll(&self, fence: FenceId) -> bool {
        let local = fence.local_id();
        self.semaphores.lock()
            .get(&local)
            .map_or(true, |s| s.poll(1)) // Unknown fence = already done
    }

    /// Blocking wait for a fence.
    pub fn wait(&self, fence: FenceId, timeout_ns: u64) -> Result<(), ()> {
        let local = fence.local_id();
        let sems = self.semaphores.lock();
        match sems.get(&local) {
            Some(sem) => {
                // Clone the value to release the lock before blocking
                let target_reached = sem.poll(1);
                drop(sems);
                if target_reached {
                    return Ok(());
                }
                // Re-acquire and wait
                let sems = self.semaphores.lock();
                if let Some(sem) = sems.get(&local) {
                    let result = sem.wait_blocking(1, timeout_ns);
                    drop(sems);
                    result
                } else {
                    Ok(()) // Fence was cleaned up = completed
                }
            }
            None => Ok(()), // Unknown fence = already completed and cleaned up
        }
    }

    /// Remove a completed fence to free memory.
    pub fn reclaim(&self, fence: FenceId) {
        let local = fence.local_id();
        self.semaphores.lock().remove(&local);
    }
}
