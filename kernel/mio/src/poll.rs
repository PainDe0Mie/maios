//! Kernel-side SQ polling (SQPOLL) for MIO.
//!
//! When an MIO instance is created with `SQPOLL` flag, a dedicated kernel
//! thread continuously drains the submission queue, eliminating the need
//! for submit syscalls on the hot path.
//!
//! Based on: io_uring IORING_SETUP_SQPOLL mode.
//!
//! ## Behaviour
//!
//! The SQPOLL thread runs a tight loop:
//! 1. Check if new SQEs are available (tail > last_seen_tail).
//! 2. If yes: process them, reset idle counter.
//! 3. If no:  increment idle counter; if idle > threshold, park the thread.
//! 4. A new submission wakes the parked thread via an atomic flag.
//!
//! ## Power efficiency
//!
//! The idle-park mechanism ensures the SQPOLL thread does not burn CPU
//! when the application is not submitting I/O. The park/wake protocol
//! uses a single atomic variable (no futex needed in kernel context).

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::{SQPOLL_IDLE_TIMEOUT_MS, SQPOLL_BATCH_SIZE};

/// State of the SQPOLL thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SqPollState {
    /// Thread is actively polling the SQ.
    Running = 0,
    /// Thread is parked (idle timeout exceeded, waiting for wake signal).
    Parked = 1,
    /// Thread has been requested to stop.
    Stopping = 2,
    /// Thread has exited.
    Stopped = 3,
}

/// SQPOLL thread context.
///
/// Each MIO instance with SQPOLL enabled has one of these. The context
/// is shared between the polling thread and the instance owner.
pub struct SqPollContext {
    /// Current thread state.
    state: AtomicU32,
    /// Wake signal: set by submitter to wake a parked thread.
    needs_wakeup: AtomicBool,
    /// Number of SQEs processed by this thread.
    pub sqes_processed: AtomicU64,
    /// Number of times the thread parked due to idle.
    pub park_count: AtomicU64,
    /// Number of times the thread was woken from park.
    pub wake_count: AtomicU64,
    /// Idle timeout in milliseconds.
    pub idle_timeout_ms: u64,
    /// Maximum SQEs to process per polling iteration.
    pub batch_size: u32,
    /// Consecutive idle iterations (no SQEs found).
    idle_iterations: AtomicU32,
}

impl SqPollContext {
    /// Create a new SQPOLL context.
    pub fn new(idle_timeout_ms: u64, batch_size: u32) -> Self {
        SqPollContext {
            state: AtomicU32::new(SqPollState::Running as u32),
            needs_wakeup: AtomicBool::new(false),
            sqes_processed: AtomicU64::new(0),
            park_count: AtomicU64::new(0),
            wake_count: AtomicU64::new(0),
            idle_timeout_ms: if idle_timeout_ms == 0 {
                SQPOLL_IDLE_TIMEOUT_MS
            } else {
                idle_timeout_ms
            },
            batch_size: if batch_size == 0 {
                SQPOLL_BATCH_SIZE
            } else {
                batch_size
            },
            idle_iterations: AtomicU32::new(0),
        }
    }

    /// Get the current state of the SQPOLL thread.
    #[inline]
    pub fn state(&self) -> SqPollState {
        match self.state.load(Ordering::Acquire) {
            0 => SqPollState::Running,
            1 => SqPollState::Parked,
            2 => SqPollState::Stopping,
            _ => SqPollState::Stopped,
        }
    }

    /// Signal the SQPOLL thread to wake up (called by submitter).
    ///
    /// If the thread is parked, this sets the wake flag. The thread
    /// checks this flag on each iteration when parked.
    pub fn wake(&self) {
        self.needs_wakeup.store(true, Ordering::Release);
        self.wake_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Check and clear the wake flag (called by SQPOLL thread).
    #[inline]
    pub fn check_and_clear_wake(&self) -> bool {
        self.needs_wakeup.swap(false, Ordering::AcqRel)
    }

    /// Request the SQPOLL thread to stop.
    pub fn request_stop(&self) {
        self.state.store(SqPollState::Stopping as u32, Ordering::Release);
        self.needs_wakeup.store(true, Ordering::Release);
    }

    /// Check if the thread should stop.
    #[inline]
    pub fn should_stop(&self) -> bool {
        self.state.load(Ordering::Acquire) == SqPollState::Stopping as u32
    }

    /// Mark the thread as stopped (called by SQPOLL thread on exit).
    pub fn mark_stopped(&self) {
        self.state.store(SqPollState::Stopped as u32, Ordering::Release);
    }

    /// Record that SQEs were processed (resets idle counter).
    pub fn record_work(&self, count: u32) {
        self.sqes_processed.fetch_add(count as u64, Ordering::Relaxed);
        self.idle_iterations.store(0, Ordering::Relaxed);
        self.state.store(SqPollState::Running as u32, Ordering::Release);
    }

    /// Record an idle iteration (no SQEs available).
    ///
    /// Returns `true` if the thread should park (idle threshold exceeded).
    pub fn record_idle(&self) -> bool {
        let idles = self.idle_iterations.fetch_add(1, Ordering::Relaxed) + 1;

        // Convert idle timeout to approximate iteration count.
        // Assuming ~1µs per poll iteration, 1ms = 1000 iterations.
        let threshold = (self.idle_timeout_ms * 1000) as u32;

        if idles >= threshold {
            self.state.store(SqPollState::Parked as u32, Ordering::Release);
            self.park_count.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// The main polling loop body.
    ///
    /// Returns the number of SQEs processed, or 0 if none were available.
    /// The caller (the SQPOLL kernel thread) should call this in a loop:
    ///
    /// ```ignore
    /// loop {
    ///     if ctx.should_stop() { break; }
    ///     let processed = ctx.poll_iteration(&instance.sq);
    ///     if processed == 0 {
    ///         if ctx.record_idle() {
    ///             // Park: spin on needs_wakeup
    ///             while !ctx.check_and_clear_wake() && !ctx.should_stop() {
    ///                 core::hint::spin_loop();
    ///             }
    ///         }
    ///     } else {
    ///         ctx.record_work(processed);
    ///     }
    /// }
    /// ctx.mark_stopped();
    /// ```
    pub fn poll_iteration_count(&self, sq_ready: u32) -> u32 {
        sq_ready.min(self.batch_size)
    }

    /// Check if the thread is currently parked.
    #[inline]
    pub fn is_parked(&self) -> bool {
        self.state() == SqPollState::Parked
    }

    /// Check if the thread needs to be woken.
    #[inline]
    pub fn needs_wakeup(&self) -> bool {
        self.needs_wakeup.load(Ordering::Acquire)
    }
}
