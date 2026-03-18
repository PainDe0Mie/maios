//! MHC Heterogeneous Scheduler — Unified CPU+GPU scheduling bridge.
//!
//! ## Research basis
//!
//! - **HEXO** (Ganguly et al., ASPLOS 2023): heterogeneous-aware OS scheduling
//!   that co-schedules CPU and GPU tasks with dependency awareness.
//! - **Harmonize** (Arafa et al., MICRO 2021): unified CPU+GPU task scheduling
//!   using task-graph analysis for optimal placement.
//!
//! ## Design
//!
//! The heterogeneous scheduler maintains per-GPU run queues analogous to
//! MKS's per-CPU run queues. Each GPU run queue uses an EEVDF adaptation
//! where:
//! - Virtual runtime is measured in "GPU-cycles" (normalized by compute units)
//! - Weight reflects the task's compute share (analogous to nice values)
//! - Deadlines propagate from CPU deadline tasks to their GPU dependencies
//!
//! The scheduler integrates with MKS via the plugin system: an MHC plugin
//! intercepts task events to track GPU dependencies and wake CPU tasks
//! when their GPU work completes.

#![allow(dead_code)]

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use spin::Mutex;

use crate::device::GpuError;
use crate::fence::FenceId;
use crate::queue::GpuPriority;
use crate::task::{GpuTask, GpuTaskState};

// ---------------------------------------------------------------------------
// Per-GPU run queue
// ---------------------------------------------------------------------------

/// EEVDF-adapted run queue for GPU tasks.
///
/// Virtual runtime is measured in estimated GPU-cycles rather than
/// wall-clock nanoseconds. This accounts for the fact that GPU tasks
/// have variable parallelism — a task dispatching 1024 workgroups consumes
/// more "GPU resource" than one dispatching 4.
pub struct GpuEevdfQueue {
    /// Tasks eligible for dispatch, ordered by (vdeadline, task_id).
    eligible: BTreeMap<GpuVtKey, Arc<GpuTask>>,
    /// Tasks not yet eligible (dependencies pending), ordered by (ve, task_id).
    ineligible: BTreeMap<GpuVtKey, Arc<GpuTask>>,
    /// Queue clock: minimum vruntime of all enqueued tasks.
    pub min_vruntime: u64,
    /// Total weight of enqueued tasks.
    total_weight: u64,
    /// Number of tasks.
    pub nr_tasks: usize,
}

/// Composite key for GPU task ordering.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct GpuVtKey(pub u64, pub u64);

impl GpuEevdfQueue {
    pub fn new() -> Self {
        GpuEevdfQueue {
            eligible: BTreeMap::new(),
            ineligible: BTreeMap::new(),
            min_vruntime: 0,
            total_weight: 0,
            nr_tasks: 0,
        }
    }

    /// Enqueue a GPU task.
    pub fn enqueue(&mut self, task: Arc<GpuTask>) {
        let cost = task.estimated_cost().max(1);
        let weight = priority_to_weight(task.priority);

        // Compute virtual times
        let vruntime = task.vruntime.load(Ordering::Relaxed);
        let ve = vruntime.max(self.min_vruntime.saturating_sub(cost));
        let slice = self.compute_slice(weight);
        let vdeadline = ve + slice;

        task.vruntime.store(ve, Ordering::Relaxed);
        task.vdeadline.store(vdeadline, Ordering::Relaxed);

        self.total_weight += weight;
        self.nr_tasks += 1;

        let key = GpuVtKey(vdeadline, task.id);

        if ve <= self.min_vruntime {
            self.eligible.insert(key, task);
        } else {
            self.ineligible.insert(GpuVtKey(ve, task.id), task);
        }
    }

    /// Pick the next task to dispatch: min vdeadline among eligible tasks.
    pub fn pick_next(&mut self) -> Option<Arc<GpuTask>> {
        self.advance_eligibility();

        let (&key, _) = self.eligible.iter().next()?;
        let task = self.eligible.remove(&key)?;
        self.total_weight = self.total_weight.saturating_sub(
            priority_to_weight(task.priority)
        );
        self.nr_tasks = self.nr_tasks.saturating_sub(1);
        Some(task)
    }

    /// Remove a specific task from the queue.
    pub fn remove(&mut self, task_id: u64) -> Option<Arc<GpuTask>> {
        // Search eligible first
        let key = self.eligible.keys()
            .find(|k| k.1 == task_id)
            .copied();
        if let Some(k) = key {
            let task = self.eligible.remove(&k)?;
            self.total_weight = self.total_weight.saturating_sub(
                priority_to_weight(task.priority)
            );
            self.nr_tasks = self.nr_tasks.saturating_sub(1);
            return Some(task);
        }

        // Then ineligible
        let key = self.ineligible.keys()
            .find(|k| k.1 == task_id)
            .copied();
        if let Some(k) = key {
            let task = self.ineligible.remove(&k)?;
            self.total_weight = self.total_weight.saturating_sub(
                priority_to_weight(task.priority)
            );
            self.nr_tasks = self.nr_tasks.saturating_sub(1);
            return Some(task);
        }

        None
    }

    fn compute_slice(&self, task_weight: u64) -> u64 {
        const GPU_LATENCY_TARGET: u64 = 10_000_000; // 10ms
        const GPU_MIN_GRANULARITY: u64 = 1_000_000;  // 1ms
        if self.total_weight == 0 {
            return GPU_LATENCY_TARGET;
        }
        let slice = GPU_LATENCY_TARGET * task_weight / (self.total_weight + task_weight);
        slice.clamp(GPU_MIN_GRANULARITY, GPU_LATENCY_TARGET)
    }

    fn advance_eligibility(&mut self) {
        let mut to_promote = Vec::new();
        for (&key, _) in self.ineligible.iter() {
            if key.0 <= self.min_vruntime {
                to_promote.push(key);
            } else {
                break;
            }
        }
        for key in to_promote {
            if let Some(task) = self.ineligible.remove(&key) {
                let vdeadline = task.vdeadline.load(Ordering::Relaxed);
                self.eligible.insert(GpuVtKey(vdeadline, task.id), task);
            }
        }
    }
}

impl Default for GpuEevdfQueue {
    fn default() -> Self { Self::new() }
}

// ---------------------------------------------------------------------------
// Per-GPU run queue (aggregate)
// ---------------------------------------------------------------------------

/// Complete per-GPU scheduling state, analogous to MKS's `PerCpuRunQueue`.
pub struct PerGpuRunQueue {
    /// GPU device ID.
    pub device_id: usize,
    /// Compute tasks (EEVDF-scheduled).
    pub compute_rq: Mutex<GpuEevdfQueue>,
    /// Load approximation (number of tasks).
    pub load: AtomicUsize,
    /// Statistics.
    pub stats: Mutex<GpuSchedStats>,
}

impl PerGpuRunQueue {
    pub fn new(device_id: usize) -> Self {
        PerGpuRunQueue {
            device_id,
            compute_rq: Mutex::new(GpuEevdfQueue::new()),
            load: AtomicUsize::new(0),
            stats: Mutex::new(GpuSchedStats::new()),
        }
    }

    /// Enqueue a GPU task.
    pub fn enqueue(&self, task: Arc<GpuTask>) {
        self.compute_rq.lock().enqueue(task);
        self.load.fetch_add(1, Ordering::Relaxed);
    }

    /// Pick the next task to dispatch.
    pub fn pick_next(&self) -> Option<Arc<GpuTask>> {
        let task = self.compute_rq.lock().pick_next()?;
        self.load.fetch_sub(1, Ordering::Relaxed);
        self.stats.lock().dispatches += 1;
        Some(task)
    }

    /// Current load (approximate task count).
    pub fn load(&self) -> usize {
        self.load.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// GPU scheduling statistics
// ---------------------------------------------------------------------------

/// Per-GPU scheduling statistics.
pub struct GpuSchedStats {
    /// Total dispatches completed.
    pub dispatches: u64,
    /// Total GPU-time consumed [ns].
    pub gpu_time_ns: u64,
    /// Number of deadline misses.
    pub deadline_misses: u64,
    /// Number of preemptions.
    pub preemptions: u64,
}

impl GpuSchedStats {
    pub fn new() -> Self {
        GpuSchedStats {
            dispatches: 0,
            gpu_time_ns: 0,
            deadline_misses: 0,
            preemptions: 0,
        }
    }
}

impl Default for GpuSchedStats {
    fn default() -> Self { Self::new() }
}

// ---------------------------------------------------------------------------
// Priority to weight conversion
// ---------------------------------------------------------------------------

/// Convert GPU priority to a numeric weight (higher = more GPU time share).
fn priority_to_weight(priority: GpuPriority) -> u64 {
    match priority {
        GpuPriority::Background => 64,
        GpuPriority::Normal     => 1024,
        GpuPriority::High       => 4096,
        GpuPriority::Realtime   => 16384,
        GpuPriority::Deadline   => 65536,
    }
}
