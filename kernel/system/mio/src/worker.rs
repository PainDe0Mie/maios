//! Async I/O worker thread pool for MIO.
//!
//! Operations flagged with `ASYNC` or that would block are offloaded to
//! a pool of kernel worker threads. This prevents the submitting task
//! from blocking on slow I/O.
//!
//! Design based on:
//! - io_uring's `io-wq` worker pool (bounded + unbounded workers)
//! - Work-stealing thread pool (Tokio, Rayon influence)
//!
//! ## Worker types
//!
//! - **Bounded workers**: for operations that can block on I/O (disk, network).
//!   Limited to `num_cpus` threads to prevent thread explosion.
//! - **Unbounded workers**: for CPU-bound post-processing (e.g., checksumming,
//!   compression). Can scale beyond CPU count for short-lived tasks.
//!
//! ## Work stealing
//!
//! Each worker has a local deque. When a worker's deque is empty, it steals
//! from other workers' deques (LIFO steal from tail). This balances load
//! without a central queue bottleneck.

use alloc::vec::Vec;
use alloc::collections::VecDeque;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicBool, Ordering};
use spin::Mutex;

use crate::sqe::SubmissionEntry;

// ---------------------------------------------------------------------------
// Work item
// ---------------------------------------------------------------------------

/// A work item queued for async execution.
#[derive(Clone)]
pub struct WorkItem {
    /// The SQE to execute.
    pub sqe: SubmissionEntry,
    /// Instance ID that submitted this work item (for posting CQE).
    pub instance_id: u32,
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

/// State of a single worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WorkerState {
    /// Worker is idle, waiting for work.
    Idle = 0,
    /// Worker is executing a work item.
    Busy = 1,
    /// Worker is shutting down.
    Exiting = 2,
}

/// A single worker thread's state.
pub struct Worker {
    /// Worker ID.
    pub id: u32,
    /// Local work deque.
    pub deque: Mutex<VecDeque<WorkItem>>,
    /// Current state.
    pub state: AtomicU32,
    /// Total work items processed by this worker.
    pub processed: AtomicU64,
}

impl Worker {
    fn new(id: u32) -> Self {
        Worker {
            id,
            deque: Mutex::new(VecDeque::with_capacity(64)),
            state: AtomicU32::new(WorkerState::Idle as u32),
            processed: AtomicU64::new(0),
        }
    }

    /// Push a work item to this worker's local deque.
    pub fn push(&self, item: WorkItem) {
        self.deque.lock().push_back(item);
    }

    /// Pop a work item from the front (FIFO for local work).
    pub fn pop(&self) -> Option<WorkItem> {
        self.deque.lock().pop_front()
    }

    /// Steal a work item from the back (LIFO steal).
    pub fn steal(&self) -> Option<WorkItem> {
        self.deque.lock().pop_back()
    }

    /// Number of items in the local deque.
    pub fn pending_count(&self) -> usize {
        self.deque.lock().len()
    }

    /// Mark worker as busy.
    pub fn set_busy(&self) {
        self.state.store(WorkerState::Busy as u32, Ordering::Release);
    }

    /// Mark worker as idle.
    pub fn set_idle(&self) {
        self.state.store(WorkerState::Idle as u32, Ordering::Release);
    }

    /// Check if worker is idle.
    #[inline]
    pub fn is_idle(&self) -> bool {
        self.state.load(Ordering::Acquire) == WorkerState::Idle as u32
    }
}

// ---------------------------------------------------------------------------
// Worker pool
// ---------------------------------------------------------------------------

/// The MIO async worker thread pool.
///
/// Manages bounded and unbounded workers for offloading blocking and
/// CPU-intensive I/O operations.
pub struct WorkerPool {
    /// Bounded workers (for blocking I/O, limited to num_cpus).
    pub bounded: Vec<Worker>,
    /// Number of bounded workers.
    pub bounded_count: u32,
    /// Global overflow queue (when all workers' deques are full).
    pub overflow: Mutex<VecDeque<WorkItem>>,
    /// Next worker to assign work to (round-robin).
    next_worker: AtomicU32,
    /// Total work items submitted.
    pub total_submitted: AtomicU64,
    /// Total work items completed.
    pub total_completed: AtomicU64,
    /// Whether the pool is accepting new work.
    pub active: AtomicBool,
}

impl WorkerPool {
    /// Create a new worker pool with the given number of bounded workers.
    pub fn new(num_bounded: u32) -> Self {
        let mut bounded = Vec::with_capacity(num_bounded as usize);
        for i in 0..num_bounded {
            bounded.push(Worker::new(i));
        }

        WorkerPool {
            bounded,
            bounded_count: num_bounded,
            overflow: Mutex::new(VecDeque::with_capacity(256)),
            next_worker: AtomicU32::new(0),
            total_submitted: AtomicU64::new(0),
            total_completed: AtomicU64::new(0),
            active: AtomicBool::new(true),
        }
    }

    /// Submit a work item to the pool.
    ///
    /// Assignment policy:
    /// 1. Try the least-loaded idle worker.
    /// 2. If all busy, round-robin to the next worker.
    /// 3. If all deques are full, push to the overflow queue.
    pub fn submit(&self, item: WorkItem) -> bool {
        if !self.active.load(Ordering::Acquire) {
            return false;
        }

        self.total_submitted.fetch_add(1, Ordering::Relaxed);

        // Try to find an idle worker with the shortest deque.
        let mut best_idx = None;
        let mut best_pending = usize::MAX;

        for (i, w) in self.bounded.iter().enumerate() {
            if w.is_idle() {
                let pending = w.pending_count();
                if pending < best_pending {
                    best_pending = pending;
                    best_idx = Some(i);
                }
            }
        }

        if let Some(idx) = best_idx {
            self.bounded[idx].push(item);
            return true;
        }

        // All busy: round-robin.
        let idx = self.next_worker.fetch_add(1, Ordering::Relaxed) % self.bounded_count;
        self.bounded[idx as usize].push(item);
        true
    }

    /// Try to steal work from other workers (called by an idle worker).
    ///
    /// Returns a work item if one was successfully stolen.
    pub fn try_steal(&self, thief_id: u32) -> Option<WorkItem> {
        // First check the overflow queue.
        {
            let mut overflow = self.overflow.lock();
            if let Some(item) = overflow.pop_front() {
                return Some(item);
            }
        }

        // Steal from the worker with the most work.
        let mut best_victim = None;
        let mut best_pending = 0;

        for (i, w) in self.bounded.iter().enumerate() {
            if i as u32 == thief_id {
                continue;
            }
            let pending = w.pending_count();
            if pending > best_pending {
                best_pending = pending;
                best_victim = Some(i);
            }
        }

        if let Some(victim) = best_victim {
            return self.bounded[victim].steal();
        }

        None
    }

    /// Record a completed work item.
    pub fn record_completion(&self) {
        self.total_completed.fetch_add(1, Ordering::Relaxed);
    }

    /// Shut down the pool (no new work accepted, drain existing).
    pub fn shutdown(&self) {
        self.active.store(false, Ordering::Release);
    }

    /// Number of pending work items across all workers.
    pub fn total_pending(&self) -> usize {
        let worker_pending: usize = self.bounded.iter()
            .map(|w| w.pending_count())
            .sum();
        let overflow_pending = self.overflow.lock().len();
        worker_pending + overflow_pending
    }

    /// Number of idle workers.
    pub fn idle_count(&self) -> u32 {
        self.bounded.iter()
            .filter(|w| w.is_idle())
            .count() as u32
    }
}
