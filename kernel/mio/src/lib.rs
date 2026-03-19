//! MIO — Mai I/O Subsystem
//!
//! A high-performance, async I/O subsystem for MaiOS implementing:
//!
//! 1. **io_uring-inspired ring buffers** (Axboe, 2019)
//!    Lock-free Submission Queue (SQ) and Completion Queue (CQ) shared between
//!    user and kernel. Amortises syscall overhead by batching N operations per
//!    submit, achieving O(1) amortised submission cost.
//!
//! 2. **Zero-copy buffer registration**
//!    Pre-registered buffer pools avoid per-I/O `copy_from_user`/`copy_to_user`.
//!    Based on: DPDK rte_mempool design + io_uring fixed buffers.
//!
//! 3. **Kernel-side SQ polling** (SQPOLL mode)
//!    A dedicated kernel thread drains the SQ, eliminating submit syscalls
//!    entirely for latency-critical paths. Based on: io_uring IORING_SETUP_SQPOLL.
//!
//! 4. **Linked operations**
//!    Chain dependent I/O ops (e.g., read → process → write) so the kernel
//!    executes them sequentially without returning to userspace between steps.
//!    Based on: io_uring IOSQE_IO_LINK.
//!
//! 5. **Completion event batching**
//!    Multiple completions are reaped in one call, reducing context-switch
//!    overhead. Based on: "Asynchronous I/O Stack: A Low-latency Kernel I/O
//!    Stack for Ultra-Low Latency SSDs" (Kim et al., USENIX ATC 2019).
//!
//! ## Design invariants
//!
//! - Ring indices are monotonically increasing u32; masking gives slot index.
//! - SQ and CQ are power-of-two sized for efficient modulo via bitmask.
//! - All hot-path operations are lock-free (atomic load/store on indices).
//! - Memory ordering: producer uses Release on tail; consumer uses Acquire on tail.
//! - No allocation on the submission/completion hot path.

#![no_std]
#![allow(dead_code)]

extern crate alloc;

pub mod ring;
pub mod sqe;
pub mod cqe;
pub mod ops;
pub mod buffer;
pub mod poll;
pub mod worker;
pub mod stats;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicBool, Ordering};
use spin::Mutex;

use ring::{SubmissionQueue, CompletionQueue};
use buffer::BufferPool;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default SQ depth (must be power of two).
pub const DEFAULT_SQ_DEPTH: u32 = 256;

/// Default CQ depth (must be power of two, typically 2× SQ depth).
pub const DEFAULT_CQ_DEPTH: u32 = 512;

/// Maximum SQ depth allowed.
pub const MAX_SQ_DEPTH: u32 = 4096;

/// Maximum CQ depth allowed.
pub const MAX_CQ_DEPTH: u32 = 8192;

/// Maximum number of registered buffer pools per MIO instance.
pub const MAX_BUFFER_POOLS: usize = 16;

/// Maximum number of concurrent MIO instances (one per task group).
pub const MAX_INSTANCES: usize = 256;

/// SQPOLL idle timeout in milliseconds before the polling thread parks.
pub const SQPOLL_IDLE_TIMEOUT_MS: u64 = 1000;

/// Maximum number of SQEs processed per SQPOLL iteration.
pub const SQPOLL_BATCH_SIZE: u32 = 32;

// ---------------------------------------------------------------------------
// Setup flags
// ---------------------------------------------------------------------------

/// Flags passed to `mio_setup()` to configure an MIO instance.
pub mod setup_flags {
    /// Enable kernel-side SQ polling (SQPOLL thread).
    pub const SQPOLL: u32     = 1 << 0;
    /// Create the CQ with 2× the SQ depth (default: 2×, this forces 4×).
    pub const CQ_4X: u32      = 1 << 1;
    /// Attach to an existing MIO instance (share kernel worker threads).
    pub const ATTACH_WQ: u32  = 1 << 2;
    /// Disable automatic CQ overflow handling (advanced).
    pub const NO_CQ_OVERFLOW: u32 = 1 << 3;
    /// Enable single-issuer mode (only one thread submits, relaxes ordering).
    pub const SINGLE_ISSUER: u32 = 1 << 4;
}

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

/// Global MIO subsystem state.
static MIO: spin::Once<MaiIO> = spin::Once::new();

/// Initialize the MIO subsystem.
///
/// Called once during kernel boot. Sets up the instance table and global
/// worker pool.
pub fn init() {
    MIO.call_once(|| MaiIO::new());
}

/// Access the global MIO subsystem.
///
/// # Panics
/// Panics if called before `init()`.
#[inline]
pub fn get() -> &'static MaiIO {
    MIO.get().expect("MIO: subsystem not initialized — call mio::init() first")
}

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// MIO error codes, modelled after io_uring's errno mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum MioError {
    /// Success (not an error).
    Success = 0,
    /// Invalid argument (bad ring depth, null pointer, etc.).
    InvalidArg = -22,       // EINVAL
    /// Out of memory.
    NoMemory = -12,         // ENOMEM
    /// Resource busy (e.g., instance limit reached).
    Busy = -16,             // EBUSY
    /// Operation not supported.
    NotSupported = -95,     // EOPNOTSUPP
    /// Bad file descriptor.
    BadFd = -9,             // EBADF
    /// I/O error on underlying device.
    IoError = -5,           // EIO
    /// Operation cancelled.
    Cancelled = -125,       // ECANCELED
    /// Ring overflow (CQ full when completion posted).
    Overflow = -75,         // EOVERFLOW
    /// Operation timed out.
    TimedOut = -110,        // ETIMEDOUT
    /// Would block (non-blocking mode, no completions ready).
    WouldBlock = -11,       // EAGAIN
}

impl MioError {
    /// Convert to raw i32 error code.
    #[inline]
    pub fn as_i32(self) -> i32 {
        self as i32
    }
}

// ---------------------------------------------------------------------------
// MIO instance
// ---------------------------------------------------------------------------

/// Setup parameters for creating a new MIO instance.
#[derive(Debug, Clone)]
pub struct MioParams {
    /// Number of SQ entries (will be rounded up to next power of two).
    pub sq_depth: u32,
    /// Number of CQ entries (0 = auto: 2× sq_depth).
    pub cq_depth: u32,
    /// Setup flags (see `setup_flags`).
    pub flags: u32,
    /// SQPOLL idle timeout override (0 = use default).
    pub sq_thread_idle_ms: u64,
}

impl Default for MioParams {
    fn default() -> Self {
        MioParams {
            sq_depth: DEFAULT_SQ_DEPTH,
            cq_depth: 0,
            flags: 0,
            sq_thread_idle_ms: 0,
        }
    }
}

/// A single MIO instance — the kernel-side state for one io_uring-like ring pair.
///
/// Each task (or task group) that wants async I/O creates an instance via
/// `mio_setup()`. The instance owns the SQ, CQ, registered buffers, and
/// (optionally) a SQPOLL thread reference.
pub struct MioInstance {
    /// Unique instance ID.
    pub id: u32,
    /// Submission queue.
    pub sq: SubmissionQueue,
    /// Completion queue.
    pub cq: CompletionQueue,
    /// Registered buffer pools for zero-copy I/O.
    pub buffer_pools: Mutex<Vec<BufferPool>>,
    /// Setup flags this instance was created with.
    pub flags: u32,
    /// Whether the SQPOLL thread is active for this instance.
    pub sqpoll_active: AtomicBool,
    /// Total submissions processed.
    pub total_submissions: AtomicU64,
    /// Total completions posted.
    pub total_completions: AtomicU64,
    /// Whether this instance slot is in use.
    pub active: AtomicBool,
}

impl MioInstance {
    /// Create a new MIO instance with the given parameters.
    fn new(id: u32, params: &MioParams) -> Result<Self, MioError> {
        let sq_depth = next_power_of_two(params.sq_depth).min(MAX_SQ_DEPTH);
        let cq_depth = if params.cq_depth == 0 {
            if params.flags & setup_flags::CQ_4X != 0 {
                (sq_depth * 4).min(MAX_CQ_DEPTH)
            } else {
                (sq_depth * 2).min(MAX_CQ_DEPTH)
            }
        } else {
            next_power_of_two(params.cq_depth).min(MAX_CQ_DEPTH)
        };

        if sq_depth == 0 || cq_depth == 0 {
            return Err(MioError::InvalidArg);
        }

        Ok(MioInstance {
            id,
            sq: SubmissionQueue::new(sq_depth),
            cq: CompletionQueue::new(cq_depth),
            buffer_pools: Mutex::new(Vec::new()),
            flags: params.flags,
            sqpoll_active: AtomicBool::new(false),
            total_submissions: AtomicU64::new(0),
            total_completions: AtomicU64::new(0),
            active: AtomicBool::new(true),
        })
    }

    /// Submit all pending SQEs and optionally wait for `min_complete` completions.
    ///
    /// This is the primary entry point for submitting I/O. It:
    /// 1. Flushes the SQ tail to make new SQEs visible to the kernel.
    /// 2. Processes ready SQEs (dispatches them to the I/O subsystem).
    /// 3. If `min_complete > 0`, blocks until that many CQEs are available.
    ///
    /// Returns the number of SQEs successfully submitted.
    pub fn submit(&self, min_complete: u32) -> Result<u32, MioError> {
        let submitted = self.sq.flush_pending();
        self.total_submissions.fetch_add(submitted as u64, Ordering::Relaxed);

        // Process submitted SQEs.
        let processed = self.process_submissions(submitted);

        // If caller wants to wait for completions, spin until available.
        if min_complete > 0 {
            self.wait_for_completions(min_complete)?;
        }

        Ok(processed)
    }

    /// Process up to `count` pending SQEs from the submission queue.
    fn process_submissions(&self, count: u32) -> u32 {
        let mut processed = 0u32;
        for _ in 0..count {
            match self.sq.consume_one() {
                Some(sqe) => {
                    let result = ops::dispatch(&sqe);
                    self.post_completion(sqe.user_data, result);
                    processed += 1;
                }
                None => break,
            }
        }
        processed
    }

    /// Post a completion event to the CQ.
    fn post_completion(&self, user_data: u64, result: i32) {
        let cqe = cqe::CompletionEntry {
            user_data,
            result,
            flags: 0,
        };
        if self.cq.produce(cqe).is_ok() {
            self.total_completions.fetch_add(1, Ordering::Relaxed);
        }
        // If CQ is full and NO_CQ_OVERFLOW is not set, the CQE is dropped.
        // A production system would use an overflow list here.
    }

    /// Block until at least `min_complete` CQEs are available.
    fn wait_for_completions(&self, min_complete: u32) -> Result<(), MioError> {
        // Spin-wait with backoff. In a real implementation this would
        // integrate with the MKS scheduler to yield the CPU.
        let mut spins = 0u32;
        loop {
            if self.cq.ready_count() >= min_complete {
                return Ok(());
            }
            spins += 1;
            if spins > 1_000_000 {
                return Err(MioError::TimedOut);
            }
            core::hint::spin_loop();
        }
    }

    /// Reap up to `max` completions from the CQ.
    ///
    /// Returns a Vec of (user_data, result) pairs.
    pub fn reap_completions(&self, max: u32) -> Vec<(u64, i32)> {
        let mut results = Vec::new();
        for _ in 0..max {
            match self.cq.consume_one() {
                Some(cqe) => results.push((cqe.user_data, cqe.result)),
                None => break,
            }
        }
        results
    }

    /// Register a buffer pool for zero-copy I/O.
    pub fn register_buffers(&self, pool: BufferPool) -> Result<u32, MioError> {
        let mut pools = self.buffer_pools.lock();
        if pools.len() >= MAX_BUFFER_POOLS {
            return Err(MioError::Busy);
        }
        let idx = pools.len() as u32;
        pools.push(pool);
        Ok(idx)
    }

    /// Tear down this instance, releasing all resources.
    fn destroy(&self) {
        self.sqpoll_active.store(false, Ordering::Release);
        self.active.store(false, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// Global MIO manager
// ---------------------------------------------------------------------------

/// The global Mai I/O subsystem manager.
///
/// Manages all MIO instances and the shared worker thread pool.
pub struct MaiIO {
    /// All active MIO instances. Indexed by instance ID.
    instances: Mutex<Vec<Option<MioInstance>>>,
    /// Next instance ID to allocate.
    next_id: AtomicU32,
    /// Total instances ever created.
    pub total_created: AtomicU64,
    /// Currently active instance count.
    pub active_count: AtomicU32,
}

impl MaiIO {
    fn new() -> Self {
        MaiIO {
            instances: Mutex::new(Vec::new()),
            next_id: AtomicU32::new(0),
            total_created: AtomicU64::new(0),
            active_count: AtomicU32::new(0),
        }
    }

    /// Create a new MIO instance with the given parameters.
    ///
    /// Returns the instance ID on success.
    pub fn setup(&self, params: &MioParams) -> Result<u32, MioError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let instance = MioInstance::new(id, params)?;

        let mut instances = self.instances.lock();

        // Find a free slot or push a new one.
        let slot = instances.iter().position(|s| s.is_none());
        match slot {
            Some(idx) => {
                instances[idx] = Some(instance);
            }
            None => {
                if instances.len() >= MAX_INSTANCES {
                    return Err(MioError::Busy);
                }
                instances.push(Some(instance));
            }
        }

        self.total_created.fetch_add(1, Ordering::Relaxed);
        self.active_count.fetch_add(1, Ordering::Relaxed);

        Ok(id)
    }

    /// Destroy an MIO instance by ID.
    pub fn teardown(&self, instance_id: u32) -> Result<(), MioError> {
        let mut instances = self.instances.lock();
        for slot in instances.iter_mut() {
            if let Some(inst) = slot {
                if inst.id == instance_id {
                    inst.destroy();
                    *slot = None;
                    self.active_count.fetch_sub(1, Ordering::Relaxed);
                    return Ok(());
                }
            }
        }
        Err(MioError::InvalidArg)
    }

    /// Access an instance by ID.
    ///
    /// The callback `f` receives a reference to the instance while the lock
    /// is held. This avoids exposing the lock to callers.
    pub fn with_instance<F, R>(&self, instance_id: u32, f: F) -> Result<R, MioError>
    where
        F: FnOnce(&MioInstance) -> R,
    {
        let instances = self.instances.lock();
        for slot in instances.iter() {
            if let Some(inst) = slot {
                if inst.id == instance_id && inst.active.load(Ordering::Acquire) {
                    return Ok(f(inst));
                }
            }
        }
        Err(MioError::InvalidArg)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Round up to the next power of two.
#[inline]
fn next_power_of_two(v: u32) -> u32 {
    if v == 0 { return 1; }
    1u32 << (32 - (v - 1).leading_zeros())
}
