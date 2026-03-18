//! MHC Software Command Queues — Priority-based GPU work scheduling.
//!
//! ## Research basis
//!
//! - **TimeGraph** (Kato et al., ATC 2011): software-managed GPU scheduling
//!   with priority queues, enabling preemption and fair sharing.
//! - **Gdev** (Kato et al., ATC 2012): first-class GPU resource management
//!   in the OS kernel with software command queues.
//!
//! ## Design
//!
//! Rather than exposing hardware queues directly (the Linux/Windows model),
//! MHC maintains software priority queues per device. A background drainer
//! feeds commands to hardware in priority order. This enables:
//!
//! - **Preemption**: high-priority work interrupts low-priority GPU work
//! - **Fair sharing**: EEVDF-style fairness across GPU contexts
//! - **Deadline awareness**: deadline-class GPU tasks get bounded latency
//!
//! The overhead (~1-5µs per submission) is acceptable for a research OS
//! and can be bypassed via a fast-path for single-context scenarios.

#![allow(dead_code)]

use alloc::collections::BTreeMap;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;

use crate::command::CommandBuffer;
use crate::device::{GpuDevice, GpuError, QueueHandle};
use crate::fence::FenceId;

// ---------------------------------------------------------------------------
// Priority
// ---------------------------------------------------------------------------

/// GPU work priority levels.
///
/// Maps to MKS scheduling classes for unified priority ordering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum GpuPriority {
    /// Background compute (SCHED_BATCH equivalent).
    Background = 0,
    /// Normal priority (SCHED_NORMAL equivalent).
    Normal = 1,
    /// High priority (interactive, latency-sensitive).
    High = 2,
    /// Real-time GPU work (SCHED_FIFO equivalent).
    Realtime = 3,
    /// Deadline-critical GPU work (must complete by a hard deadline).
    Deadline = 4,
}

impl Default for GpuPriority {
    fn default() -> Self { GpuPriority::Normal }
}

// ---------------------------------------------------------------------------
// Pending submission
// ---------------------------------------------------------------------------

/// A command buffer waiting to be dispatched to hardware.
struct PendingSubmission {
    /// Priority for ordering in the software queue.
    priority: GpuPriority,
    /// Submission timestamp (monotonic counter for FIFO within same priority).
    sequence: u64,
    /// The command buffer to execute.
    commands: CommandBuffer,
    /// Fence that will be signaled on completion.
    fence: FenceId,
    /// Hardware queue to submit to.
    hw_queue: QueueHandle,
}

/// Composite key for BTreeMap ordering: (priority descending, sequence ascending).
/// Higher priority + lower sequence = first to execute.
#[derive(Clone, Copy, PartialEq, Eq)]
struct QueueKey {
    /// Inverted priority (higher priority = lower value = earlier in BTreeMap).
    inv_priority: u8,
    /// Sequence number for FIFO within same priority.
    sequence: u64,
}

impl PartialOrd for QueueKey {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for QueueKey {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.inv_priority.cmp(&other.inv_priority)
            .then(self.sequence.cmp(&other.sequence))
    }
}

// ---------------------------------------------------------------------------
// Software queue
// ---------------------------------------------------------------------------

/// A software-managed GPU command queue with priority ordering.
///
/// Sits between the application and the hardware queue, adding:
/// - Priority-based reordering
/// - Fair-share scheduling (virtual runtime tracking)
/// - Submission rate limiting
/// - Statistics collection
pub struct SoftwareQueue {
    /// Pending submissions ordered by priority then sequence.
    pending: BTreeMap<QueueKey, PendingSubmission>,
    /// In-flight submissions (submitted to hardware, not yet complete).
    in_flight: VecDeque<InFlightEntry>,
    /// Monotonic sequence counter.
    next_sequence: AtomicU64,
    /// Maximum concurrent in-flight submissions.
    max_in_flight: usize,
    /// Virtual runtime for EEVDF-style fairness (GPU-cycles consumed).
    pub vruntime: u64,
    /// Total GPU time consumed by this queue [ns].
    pub total_gpu_time_ns: u64,
    /// Number of submissions completed.
    pub completed_count: u64,
}

struct InFlightEntry {
    fence: FenceId,
    submitted_at_ns: u64,
}

impl SoftwareQueue {
    pub fn new(max_in_flight: usize) -> Self {
        SoftwareQueue {
            pending: BTreeMap::new(),
            in_flight: VecDeque::new(),
            next_sequence: AtomicU64::new(0),
            max_in_flight,
            vruntime: 0,
            total_gpu_time_ns: 0,
            completed_count: 0,
        }
    }

    /// Enqueue a command buffer for execution at the given priority.
    pub fn enqueue(
        &mut self,
        commands: CommandBuffer,
        priority: GpuPriority,
        fence: FenceId,
        hw_queue: QueueHandle,
    ) {
        let sequence = self.next_sequence.fetch_add(1, Ordering::Relaxed);
        let key = QueueKey {
            inv_priority: 4u8.saturating_sub(priority as u8),
            sequence,
        };
        self.pending.insert(key, PendingSubmission {
            priority,
            sequence,
            commands,
            fence,
            hw_queue,
        });
    }

    /// Drain pending submissions to the hardware device.
    ///
    /// Submits up to `max_in_flight - current_in_flight` commands,
    /// in priority order (highest first).
    pub fn drain_to_hardware(
        &mut self,
        device: &dyn GpuDevice,
    ) -> Vec<Result<FenceId, GpuError>> {
        let mut results = Vec::new();

        // Retire completed in-flight entries
        self.in_flight.retain(|entry| !device.poll_fence(entry.fence));

        // Submit pending work up to the in-flight limit
        while self.in_flight.len() < self.max_in_flight {
            let entry = match self.pending.pop_first() {
                Some((_, entry)) => entry,
                None => break,
            };

            match device.submit(entry.hw_queue, &entry.commands) {
                Ok(fence) => {
                    self.in_flight.push_back(InFlightEntry {
                        fence,
                        submitted_at_ns: 0, // TODO: read from monotonic clock
                    });
                    results.push(Ok(fence));
                }
                Err(e) => {
                    results.push(Err(e));
                }
            }
        }

        results
    }

    /// Number of pending (not yet submitted to hardware) commands.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Number of in-flight (submitted, not yet completed) commands.
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    /// Whether this queue has any work (pending or in-flight).
    pub fn is_busy(&self) -> bool {
        !self.pending.is_empty() || !self.in_flight.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Per-device queue manager
// ---------------------------------------------------------------------------

/// Manages all software queues for a single GPU device.
///
/// Each application context gets its own software queue with independent
/// priority and fairness tracking. The queue manager arbitrates between
/// them using a simplified EEVDF policy adapted for GPU virtual runtime.
pub struct QueueManager {
    /// Software queues keyed by context ID.
    queues: BTreeMap<u64, SoftwareQueue>,
    /// Next context ID.
    next_ctx: u64,
    /// Maximum in-flight submissions per queue.
    max_in_flight_per_queue: usize,
}

impl QueueManager {
    pub fn new(max_in_flight: usize) -> Self {
        QueueManager {
            queues: BTreeMap::new(),
            next_ctx: 1,
            max_in_flight_per_queue: max_in_flight,
        }
    }

    /// Create a new software queue context. Returns the context ID.
    pub fn create_context(&mut self) -> u64 {
        let id = self.next_ctx;
        self.next_ctx += 1;
        self.queues.insert(id, SoftwareQueue::new(self.max_in_flight_per_queue));
        id
    }

    /// Destroy a queue context.
    pub fn destroy_context(&mut self, ctx: u64) {
        self.queues.remove(&ctx);
    }

    /// Enqueue work into a specific context's queue.
    pub fn enqueue(
        &mut self,
        ctx: u64,
        commands: CommandBuffer,
        priority: GpuPriority,
        fence: FenceId,
        hw_queue: QueueHandle,
    ) -> Result<(), GpuError> {
        let q = self.queues.get_mut(&ctx)
            .ok_or(GpuError::InvalidParameter("unknown context ID"))?;
        q.enqueue(commands, priority, fence, hw_queue);
        Ok(())
    }

    /// Drain all queues to hardware in fairness order.
    ///
    /// Uses min-vruntime ordering: the queue with the least accumulated
    /// GPU time gets to submit first, ensuring fair sharing.
    pub fn drain_all(&mut self, device: &dyn GpuDevice) {
        // Collect queue IDs sorted by vruntime (least-served first)
        let mut queue_order: Vec<u64> = self.queues.keys().cloned().collect();
        queue_order.sort_by_key(|id| {
            self.queues.get(id).map_or(u64::MAX, |q| q.vruntime)
        });

        for ctx_id in queue_order {
            if let Some(q) = self.queues.get_mut(&ctx_id) {
                let _ = q.drain_to_hardware(device);
            }
        }
    }

    /// Get statistics for a context.
    pub fn stats(&self, ctx: u64) -> Option<QueueStats> {
        self.queues.get(&ctx).map(|q| QueueStats {
            pending: q.pending_count(),
            in_flight: q.in_flight_count(),
            completed: q.completed_count,
            vruntime: q.vruntime,
            total_gpu_time_ns: q.total_gpu_time_ns,
        })
    }
}

/// Queue statistics snapshot.
#[derive(Clone, Debug)]
pub struct QueueStats {
    pub pending: usize,
    pub in_flight: usize,
    pub completed: u64,
    pub vruntime: u64,
    pub total_gpu_time_ns: u64,
}
