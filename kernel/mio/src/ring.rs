//! Lock-free ring buffers for MIO.
//!
//! Implements single-producer / single-consumer (SPSC) ring buffers used as
//! the Submission Queue (SQ) and Completion Queue (CQ).
//!
//! The design follows io_uring's memory layout:
//! - **Indices** (`head`, `tail`) are monotonically increasing `u32` values.
//! - The slot index is obtained by masking: `idx & (depth - 1)` (power-of-two depth).
//! - **Producer** writes to `entries[tail & mask]`, then publishes with `Release` store on `tail`.
//! - **Consumer** reads from `entries[head & mask]`, then advances with `Release` store on `head`.
//!
//! Memory ordering:
//! - Producer: `Release` on tail ensures entries are visible before consumer sees the new tail.
//! - Consumer: `Acquire` on tail ensures it sees all entries written before that tail value.
//! - Consumer: `Release` on head frees the slot for re-use by producer.
//!
//! Based on: "Efficient I/O with io_uring" (Axboe, 2019), Linux kernel
//! `io_uring.c` ring buffer design.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use spin::Mutex;

use crate::sqe::SubmissionEntry;
use crate::cqe::CompletionEntry;

// ---------------------------------------------------------------------------
// Submission Queue
// ---------------------------------------------------------------------------

/// The Submission Queue (SQ).
///
/// Userspace (producer) writes SQEs into the ring, kernel (consumer) reads them.
///
/// Layout:
/// ```text
/// ┌────────────────────────────────────────────────┐
/// │  head (consumer/kernel advances after reading)  │
/// │  tail (producer/user advances after writing)    │
/// │  entries[0..depth-1]                            │
/// │  pending_tail (local shadow, not yet published) │
/// └────────────────────────────────────────────────┘
/// ```
pub struct SubmissionQueue {
    /// Consumer position — advanced by kernel after processing an SQE.
    head: AtomicU32,
    /// Published producer position — entries in [head..tail) are ready.
    tail: AtomicU32,
    /// Local producer position (not yet flushed to `tail`).
    /// This allows batching: user pushes N entries, then flushes once.
    pending_tail: AtomicU32,
    /// Ring entries.
    entries: Vec<Mutex<SubmissionEntry>>,
    /// Depth (always a power of two).
    depth: u32,
    /// Bitmask: `depth - 1`.
    mask: u32,
}

impl SubmissionQueue {
    /// Create a new SQ with the given depth (must be power of two).
    pub fn new(depth: u32) -> Self {
        debug_assert!(depth.is_power_of_two(), "SQ depth must be power of two");
        let mut entries = Vec::with_capacity(depth as usize);
        for _ in 0..depth {
            entries.push(Mutex::new(SubmissionEntry::zeroed()));
        }
        SubmissionQueue {
            head: AtomicU32::new(0),
            tail: AtomicU32::new(0),
            pending_tail: AtomicU32::new(0),
            entries,
            depth,
            mask: depth - 1,
        }
    }

    /// Push an SQE into the ring (does not make it visible to kernel yet).
    ///
    /// Returns `Err(sqe)` if the ring is full.
    pub fn push(&self, sqe: SubmissionEntry) -> Result<(), SubmissionEntry> {
        let head = self.head.load(Ordering::Acquire);
        let pending = self.pending_tail.load(Ordering::Relaxed);

        // Full if pending_tail - head == depth.
        if pending.wrapping_sub(head) >= self.depth {
            return Err(sqe);
        }

        let idx = (pending & self.mask) as usize;
        *self.entries[idx].lock() = sqe;
        self.pending_tail.store(pending.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    /// Flush all pending SQEs, making them visible to the kernel consumer.
    ///
    /// Returns the number of newly flushed entries.
    pub fn flush_pending(&self) -> u32 {
        let pending = self.pending_tail.load(Ordering::Acquire);
        let old_tail = self.tail.load(Ordering::Relaxed);
        let count = pending.wrapping_sub(old_tail);
        if count > 0 {
            // Publish: make entries [old_tail..pending) visible.
            self.tail.store(pending, Ordering::Release);
        }
        count
    }

    /// Consume one SQE from the ring (kernel side).
    ///
    /// Returns `None` if the ring is empty (head == tail).
    pub fn consume_one(&self) -> Option<SubmissionEntry> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);

        if head == tail {
            return None;
        }

        let idx = (head & self.mask) as usize;
        let sqe = self.entries[idx].lock().clone();
        self.head.store(head.wrapping_add(1), Ordering::Release);
        Some(sqe)
    }

    /// Number of SQEs ready for the kernel to consume.
    #[inline]
    pub fn ready_count(&self) -> u32 {
        let tail = self.tail.load(Ordering::Acquire);
        let head = self.head.load(Ordering::Relaxed);
        tail.wrapping_sub(head)
    }

    /// Number of free slots available for the producer.
    #[inline]
    pub fn free_count(&self) -> u32 {
        self.depth - self.pending_tail.load(Ordering::Relaxed)
            .wrapping_sub(self.head.load(Ordering::Acquire))
    }

    /// Ring depth.
    #[inline]
    pub fn depth(&self) -> u32 {
        self.depth
    }
}

// ---------------------------------------------------------------------------
// Completion Queue
// ---------------------------------------------------------------------------

/// The Completion Queue (CQ).
///
/// Kernel (producer) writes CQEs after completing I/O, userspace (consumer)
/// reads them.
///
/// Same ring structure as SQ but with swapped producer/consumer roles.
pub struct CompletionQueue {
    /// Consumer position — advanced by userspace after reading a CQE.
    head: AtomicU32,
    /// Producer position — advanced by kernel after writing a CQE.
    tail: AtomicU32,
    /// Ring entries.
    entries: Vec<Mutex<CompletionEntry>>,
    /// Depth (power of two).
    depth: u32,
    /// Bitmask.
    mask: u32,
    /// Overflow counter: incremented when a CQE is dropped due to full ring.
    overflow: AtomicU32,
}

impl CompletionQueue {
    /// Create a new CQ with the given depth.
    pub fn new(depth: u32) -> Self {
        debug_assert!(depth.is_power_of_two(), "CQ depth must be power of two");
        let mut entries = Vec::with_capacity(depth as usize);
        for _ in 0..depth {
            entries.push(Mutex::new(CompletionEntry::zeroed()));
        }
        CompletionQueue {
            head: AtomicU32::new(0),
            tail: AtomicU32::new(0),
            entries,
            depth,
            mask: depth - 1,
            overflow: AtomicU32::new(0),
        }
    }

    /// Produce a CQE (kernel side).
    ///
    /// Returns `Err(cqe)` if the ring is full.
    pub fn produce(&self, cqe: CompletionEntry) -> Result<(), CompletionEntry> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);

        if tail.wrapping_sub(head) >= self.depth {
            self.overflow.fetch_add(1, Ordering::Relaxed);
            return Err(cqe);
        }

        let idx = (tail & self.mask) as usize;
        *self.entries[idx].lock() = cqe;
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    /// Consume one CQE (userspace side).
    ///
    /// Returns `None` if no completions are available.
    pub fn consume_one(&self) -> Option<CompletionEntry> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);

        if head == tail {
            return None;
        }

        let idx = (head & self.mask) as usize;
        let cqe = self.entries[idx].lock().clone();
        self.head.store(head.wrapping_add(1), Ordering::Release);
        Some(cqe)
    }

    /// Number of CQEs ready for the consumer.
    #[inline]
    pub fn ready_count(&self) -> u32 {
        let tail = self.tail.load(Ordering::Acquire);
        let head = self.head.load(Ordering::Relaxed);
        tail.wrapping_sub(head)
    }

    /// Number of CQEs that were dropped due to ring overflow.
    #[inline]
    pub fn overflow_count(&self) -> u32 {
        self.overflow.load(Ordering::Relaxed)
    }

    /// Ring depth.
    #[inline]
    pub fn depth(&self) -> u32 {
        self.depth
    }
}
