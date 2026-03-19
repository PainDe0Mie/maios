//! Per-CPU Run Queue
//!
//! Each logical CPU owns one `PerCpuRunQueue` which aggregates all
//! scheduling classes. The class priority order is:
//!
//!   Deadline > RealTime > Normal (EEVDF) > Batch > Idle
//!
//! The per-CPU lock is a `spin::Mutex` (IRQ-safe in the MaiOS interrupt model).
//! On the hot path (tick handler), we acquire only the one CPU's lock.
//! Work stealing acquires at most two locks (thief + victim), always in
//! CPU-ID order to prevent deadlocks.

use core::sync::atomic::{AtomicUsize, Ordering};
use spin::Mutex;

use task_struct::{RunState, SchedClass};
use task::TaskRef;
use crate::eevdf::EevdfRunQueue;
use crate::realtime::{RtRunQueue, DeadlineRunQueue};
use crate::stats::CpuStats;

// ---------------------------------------------------------------------------
// Idle run queue — always has exactly one task
// ---------------------------------------------------------------------------

/// A trivial run queue holding only the per-CPU idle task.
pub struct IdleRunQueue {
    idle_task: Option<TaskRef>,
}

impl IdleRunQueue {
    pub fn new() -> Self { IdleRunQueue { idle_task: None } }

    pub fn set_idle_task(&mut self, task: TaskRef) {
        self.idle_task = Some(task);
    }

    pub fn pick_next(&self) -> Option<TaskRef> {
        self.idle_task.clone()
    }
}

// ---------------------------------------------------------------------------
// Per-CPU Run Queue
// ---------------------------------------------------------------------------

/// The complete per-CPU scheduler state.
///
/// Fields are individually locked so that work stealing can grab only
/// the CFS sub-queue lock without blocking other class operations.
pub struct PerCpuRunQueue {
    /// This CPU's logical ID.
    pub cpu_id: usize,

    /// SCHED_DEADLINE (EDF) tasks — highest priority.
    pub deadline_rq: Mutex<DeadlineRunQueue>,

    /// SCHED_FIFO / SCHED_RR tasks — second priority.
    pub rt_rq: Mutex<RtRunQueue>,

    /// SCHED_NORMAL / SCHED_BATCH tasks via EEVDF — third priority.
    pub cfs_rq: Mutex<EevdfRunQueue>,

    /// SCHED_IDLE task — lowest priority.
    pub idle_rq: Mutex<IdleRunQueue>,

    /// Total number of runnable tasks (all classes). Approximate (relaxed atomic).
    load_count: AtomicUsize,

    /// Per-CPU statistics (latency, throughput, switches).
    pub stats: Mutex<CpuStats>,

    /// The task currently running on this CPU (`None` if idle).
    pub current: Mutex<Option<TaskRef>>,

    /// Whether this CPU needs a reschedule at the next safe point.
    pub need_resched: core::sync::atomic::AtomicBool,
}

impl PerCpuRunQueue {
    pub fn new(cpu_id: usize) -> Self {
        PerCpuRunQueue {
            cpu_id,
            deadline_rq: Mutex::new(DeadlineRunQueue::new()),
            rt_rq:       Mutex::new(RtRunQueue::new()),
            cfs_rq:      Mutex::new(EevdfRunQueue::new()),
            idle_rq:     Mutex::new(IdleRunQueue::new()),
            load_count:  AtomicUsize::new(0),
            stats:       Mutex::new(CpuStats::new(cpu_id)),
            current:     Mutex::new(None),
            need_resched: core::sync::atomic::AtomicBool::new(false),
        }
    }

    // -----------------------------------------------------------------------
    // Enqueue / Dequeue
    // -----------------------------------------------------------------------

    /// Enqueue a task into the appropriate sub-queue based on its class.
    pub fn enqueue(&self, task: TaskRef) {
        let class = task.read().sched.policy.class();
        match class {
            SchedClassId::Deadline => {
                let now_ns = crate::stats::monotonic_ns();
                let _ = self.deadline_rq.lock().enqueue(task, now_ns);
            }
            SchedClassId::Compute => {
                // GPU compute tasks are scheduled by MHC, not MKS.
                // They are enqueued in CFS with high weight as a fallback
                // so the CPU shadow task runs when GPU work completes.
                self.cfs_rq.lock().enqueue(task, /* is_wakeup */ true);
            }
            SchedClassId::RealTime => {
                self.rt_rq.lock().enqueue(task);
            }
            SchedClassId::Normal | SchedClassId::Batch => {
                self.cfs_rq.lock().enqueue(task, /* is_wakeup */ true);
            }
            SchedClassId::Idle => {
                self.idle_rq.lock().set_idle_task(task);
                return; // Idle task doesn't count towards load.
            }
        }
        self.load_count.fetch_add(1, Ordering::Relaxed);

        // Update last_cpu so pick_next on this CPU is fast.
        // (Done after enqueue so the task is visible in the queue.)
    }

    /// Dequeue a task (block, exit, migration).
    pub fn dequeue(&self, task: &TaskRef) {
        let class = task.read().sched.policy.class();
        match class {
            SchedClassId::Deadline => self.deadline_rq.lock().dequeue(task),
            SchedClassId::Compute => self.cfs_rq.lock().dequeue(task),
            SchedClassId::RealTime => self.rt_rq.lock().dequeue(task),
            SchedClassId::Normal | SchedClassId::Batch => self.cfs_rq.lock().dequeue(task),
            SchedClassId::Idle => return,
        }
        self.load_count.fetch_sub(1, Ordering::Relaxed);
    }

    // -----------------------------------------------------------------------
    // Tick
    // -----------------------------------------------------------------------

    /// Timer-tick accounting. Called with `elapsed_ns` since the last tick.
    ///
    /// Updates vruntime / budget for the currently running task,
    /// sets `need_resched` if preemption is warranted.
    pub fn tick(&self, elapsed_ns: u64) {
        let current_opt = self.current.lock().clone();
        let Some(current) = current_opt else { return };

        let class = current.read().sched.policy.class();
        let should_preempt = match class {
            SchedClassId::Deadline => {
                self.deadline_rq.lock().tick(&current, elapsed_ns)
            }
            SchedClassId::Compute => {
                // GPU compute shadow tasks use CFS tick logic
                self.cfs_rq.lock().tick(&current, elapsed_ns)
            }
            SchedClassId::RealTime => {
                self.rt_rq.lock().tick(&current, elapsed_ns)
            }
            SchedClassId::Normal | SchedClassId::Batch => {
                self.cfs_rq.lock().tick(&current, elapsed_ns)
            }
            SchedClassId::Idle => false,
        };

        if should_preempt {
            self.need_resched.store(true, Ordering::Release);
        }
    }

    // -----------------------------------------------------------------------
    // Context switch helpers
    // -----------------------------------------------------------------------

    /// Signal that the current task is voluntarily yielding.
    /// Re-enqueues the current task (if runnable) and clears `current`.
    pub fn yield_current(&self) {
        let task_opt = self.current.lock().take();
        if let Some(task) = task_opt {
            if task.read().runstate.load() == RunState::Runnable {
                self.enqueue(task);
            }
        }
    }

    /// Set the currently running task for this CPU.
    pub fn set_current(&self, task: TaskRef) {
        task.write().sched.last_cpu.store(self.cpu_id, Ordering::Relaxed);
        *self.current.lock() = Some(task);
        self.need_resched.store(false, Ordering::Relaxed);
    }

    // -----------------------------------------------------------------------
    // Work stealing
    // -----------------------------------------------------------------------

    /// Steal one task from this CPU's CFS queue (called by a different CPU).
    ///
    /// Returns `None` if the queue has ≤ 1 task (we never steal the last one).
    pub fn steal_one(&self) -> Option<TaskRef> {
        let task = self.cfs_rq.lock().steal_one()?;
        self.load_count.fetch_sub(1, Ordering::Relaxed);
        Some(task)
    }

    // -----------------------------------------------------------------------
    // Load query
    // -----------------------------------------------------------------------

    /// Returns the approximate number of runnable tasks on this CPU.
    #[inline]
    pub fn load(&self) -> usize {
        self.load_count.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// SchedClassId — lightweight enum for dispatch
// ---------------------------------------------------------------------------

/// Identifies a scheduling class without carrying extra data.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SchedClassId {
    Deadline,
    Compute,
    RealTime,
    Normal,
    Batch,
    Idle,
}

/// Extension trait — convert `SchedClass` → `SchedClassId` cheaply.
pub trait SchedClassExt {
    fn class(&self) -> SchedClassId;
}

impl SchedClassExt for SchedClass {
    fn class(&self) -> SchedClassId {
        match self {
            SchedClass::Deadline { .. } => SchedClassId::Deadline,
            SchedClass::Compute { .. } => SchedClassId::Compute,
            SchedClass::Fifo | SchedClass::RoundRobin(_) => SchedClassId::RealTime,
            SchedClass::Normal => SchedClassId::Normal,
            SchedClass::Batch => SchedClassId::Batch,
            SchedClass::Idle => SchedClassId::Idle,
        }
    }
}