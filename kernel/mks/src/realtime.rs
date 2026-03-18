//! Real-Time scheduling class (SCHED_FIFO / SCHED_RR)
//!
//! Based on POSIX real-time scheduling (IEEE Std 1003.1) and
//! Linux's rt_sched_class.
//!
//! Priority range: 1 (lowest RT) to 99 (highest RT).
//! All RT tasks take precedence over any Normal/Batch task.
//!
//! SCHED_FIFO: once running, task runs until it voluntarily blocks
//!             or is preempted by a higher-priority RT task.
//! SCHED_RR:   like FIFO but with a timeslice (100ms default);
//!             same-priority tasks round-robin.

use alloc::collections::BTreeMap;
use task::TaskRef;
use task_struct::SchedClass;

/// Number of RT priority levels.
pub const RT_PRIO_LEVELS: usize = 100;

/// Default SCHED_RR timeslice in nanoseconds (100ms).
pub const RT_RR_TIMESLICE_NS: u64 = 100_000_000;

/// A run queue for real-time tasks.
///
/// Implemented as an array of per-priority FIFO queues (O(1) enqueue/dequeue).
/// pick_next is O(1) via a bitmask of non-empty priorities.
pub struct RtRunQueue {
    /// Per-priority queues: prio 99 = highest.
    /// Each queue is a VecDeque for FIFO ordering within same priority.
    queues: [alloc::collections::VecDeque<TaskRef>; RT_PRIO_LEVELS],
    /// Bitmask of non-empty priority levels (u128 covers prio 0..127).
    bitmap: u128,
    /// Total number of RT tasks in this queue.
    pub nr_running: usize,
}

impl RtRunQueue {
    pub fn new() -> Self {
        // SAFETY: VecDeque::new() is const-compatible via Default.
        let queues = core::array::from_fn(|_| alloc::collections::VecDeque::new());
        RtRunQueue {
            queues,
            bitmap: 0,
            nr_running: 0,
        }
    }

    /// Enqueue a real-time task. Priority 99 = highest.
    ///
    /// SCHED_FIFO/RR: append to the back of the priority queue.
    pub fn enqueue(&mut self, task: TaskRef) {
        let prio = {
            let t = task.read();
            t.sched.rt_priority.clamp(0, 99) as usize
        };
        self.queues[prio].push_back(task);
        self.bitmap |= 1u128 << prio;
        self.nr_running += 1;
    }

    /// Dequeue a specific task (e.g., on block or exit).
    pub fn dequeue(&mut self, task: &TaskRef) {
        let task_id = task.read().id;
        for prio in (0..RT_PRIO_LEVELS).rev() {
            if self.bitmap & (1u128 << prio) == 0 {
                continue;
            }
            let q = &mut self.queues[prio];
            if let Some(pos) = q.iter().position(|t| t.read().id == task_id) {
                q.remove(pos);
                if q.is_empty() {
                    self.bitmap &= !(1u128 << prio);
                }
                self.nr_running = self.nr_running.saturating_sub(1);
                return;
            }
        }
    }

    /// Pick the highest-priority runnable RT task.
    ///
    /// For SCHED_RR: the task is re-queued at the back after its timeslice.
    /// For SCHED_FIFO: the task is removed and must be re-queued on block.
    ///
    /// Returns `None` if no RT task is runnable.
    pub fn pick_next(&mut self) -> Option<TaskRef> {
        if self.bitmap == 0 {
            return None;
        }
        // Find the highest set bit (highest priority).
        let top_prio = 127 - self.bitmap.leading_zeros() as usize;
        let q = &mut self.queues[top_prio];
        let task = q.pop_front()?;

        // For SCHED_RR: re-enqueue at the back (round-robin within priority).
        let is_rr = matches!(task.read().sched.policy, SchedClass::RoundRobin(_));
        if is_rr {
            q.push_back(task.clone());
        } else {
            // SCHED_FIFO: clear bitmap bit if queue is now empty.
            if q.is_empty() {
                self.bitmap &= !(1u128 << top_prio);
            }
            self.nr_running = self.nr_running.saturating_sub(1);
        }

        Some(task)
    }

    /// Called on each timer tick for SCHED_RR tasks.
    /// Decrements the timeslice; returns true if the task should yield.
    pub fn tick(&mut self, task: &TaskRef, elapsed_ns: u64) -> bool {
        let mut t = task.write();
        match t.sched.policy {
            SchedClass::RoundRobin(_) => {
                if t.sched.rr_timeslice_remaining > elapsed_ns {
                    t.sched.rr_timeslice_remaining -= elapsed_ns;
                    false
                } else {
                    // Timeslice expired: reset and signal preemption.
                    t.sched.rr_timeslice_remaining = RT_RR_TIMESLICE_NS;
                    true
                }
            }
            SchedClass::Fifo => false, // FIFO never preempts on time.
            _ => false,
        }
    }
}

impl Default for RtRunQueue {
    fn default() -> Self { Self::new() }
}

// ---------------------------------------------------------------------------
// SCHED_DEADLINE (EDF + CBS — Constant Bandwidth Server)
// ---------------------------------------------------------------------------
//
// Based on: "SCHED_DEADLINE: SMP-enabled earliest-deadline-first scheduling
//           in the Linux Kernel" (Faggioli et al., RTLWS 2009)
//
// Each DEADLINE task has three parameters:
//   - runtime  (C_i): maximum CPU time per period [ns]
//   - deadline (D_i): relative deadline [ns]
//   - period   (P_i): task period [ns]; P_i >= D_i
//
// The CBS (Constant Bandwidth Server) rule ensures that DEADLINE tasks
// cannot monopolize the CPU:
//   - Task has a "budget" which starts at C_i per period.
//   - Each ns of CPU time decrements the budget.
//   - If budget exhausted before deadline, the task is throttled until
//     the start of the next period.
//
// Admission control: sum(C_i / P_i) <= SCHED_DL_BANDWIDTH (95%).

use crate::SCHED_DL_BANDWIDTH_NUM;

/// A DEADLINE task entry.
#[derive(Clone)]
pub struct DlTask {
    pub task: TaskRef,
    /// Absolute deadline (nanoseconds from boot).
    pub abs_deadline: u64,
    /// Remaining runtime budget for current period.
    pub budget_remaining: u64,
}

/// EDF run queue for SCHED_DEADLINE tasks.
/// Ordered by absolute deadline (EDF = pick soonest deadline).
pub struct DeadlineRunQueue {
    /// (abs_deadline, task_id) → DlTask, for O(log n) EDF selection.
    tasks: BTreeMap<(u64, usize), DlTask>,
    /// Total admitted bandwidth = sum(C_i / P_i) * 1000 (fixed-point).
    admitted_bandwidth: u64,
    /// Number of deadline tasks.
    pub nr_running: usize,
}

impl DeadlineRunQueue {
    pub fn new() -> Self {
        DeadlineRunQueue {
            tasks: BTreeMap::new(),
            admitted_bandwidth: 0,
            nr_running: 0,
        }
    }

    /// Admit and enqueue a deadline task.
    ///
    /// Returns `Err` if admission control would exceed bandwidth limit.
    pub fn enqueue(&mut self, task: TaskRef, now_ns: u64) -> Result<(), &'static str> {
        let (runtime, period, task_id) = {
            let t = task.read();
            match t.sched.policy {
                SchedClass::Deadline { period_ns, runtime_ns } => {
                    (runtime_ns, period_ns, t.id)
                }
                _ => return Err("MKS/DL: task is not SCHED_DEADLINE"),
            }
        };

        // Admission control: check bandwidth.
        // bandwidth_contribution = runtime * 1000 / period (‰ units).
        if period == 0 {
            return Err("MKS/DL: period cannot be zero");
        }
        let bw = runtime * 1000 / period;
        let new_bw = self.admitted_bandwidth + bw;
        let max_bw = SCHED_DL_BANDWIDTH_NUM * 10; // 950 in ‰
        if new_bw > max_bw {
            return Err("MKS/DL: admission control: bandwidth exceeded");
        }
        self.admitted_bandwidth = new_bw;

        // In SCHED_DEADLINE, deadline defaults to period.
        let abs_deadline = now_ns + period;
        let entry = DlTask {
            task,
            abs_deadline,
            budget_remaining: runtime,
        };
        self.tasks.insert((abs_deadline, task_id), entry);
        self.nr_running += 1;
        Ok(())
    }

    /// Dequeue a deadline task.
    pub fn dequeue(&mut self, task: &TaskRef) {
        let task_id = task.read().id;
        // Scan for the task (we don't cache the key, but DL tasks are few).
        if let Some(key) = self.tasks.keys().find(|k| k.1 == task_id).cloned() {
            if let Some(entry) = self.tasks.remove(&key) {
                // Reclaim bandwidth.
                let (runtime, period) = {
                    let t = entry.task.read();
                    match t.sched.policy {
                        SchedClass::Deadline { period_ns, runtime_ns } => (runtime_ns, period_ns),
                        _ => (0, 1),
                    }
                };
                let bw = runtime * 1000 / period.max(1);
                self.admitted_bandwidth = self.admitted_bandwidth.saturating_sub(bw);
                self.nr_running = self.nr_running.saturating_sub(1);
            }
        }
    }

    /// EDF: pick the task with the earliest absolute deadline.
    pub fn pick_next(&mut self) -> Option<TaskRef> {
        // First entry = earliest deadline.
        let (&key, _) = self.tasks.iter().next()?;
        let entry = self.tasks.remove(&key)?;

        // If budget is exhausted, throttle: re-insert with next period.
        if entry.budget_remaining == 0 {
            let (runtime, period) = {
                let t = entry.task.read();
                match t.sched.policy {
                    SchedClass::Deadline { period_ns, runtime_ns } => (runtime_ns, period_ns),
                    _ => return None,
                }
            };
            let task_id = entry.task.read().id;
            let new_deadline = entry.abs_deadline + period;
            let re = DlTask {
                task: entry.task,
                abs_deadline: new_deadline,
                budget_remaining: runtime,
            };
            self.tasks.insert((new_deadline, task_id), re);
            // Return None: throttled, try next class.
            return None;
        }

        self.nr_running = self.nr_running.saturating_sub(1);
        Some(entry.task)
    }

    /// Account for elapsed CPU time, decreasing budget.
    /// Returns true if the task should be throttled (budget exhausted).
    pub fn tick(&mut self, task: &TaskRef, elapsed_ns: u64) -> bool {
        let task_id = task.read().id;
        if let Some(key) = self.tasks.keys().find(|k| k.1 == task_id).cloned() {
            if let Some(entry) = self.tasks.get_mut(&key) {
                if entry.budget_remaining > elapsed_ns {
                    entry.budget_remaining -= elapsed_ns;
                    false
                } else {
                    entry.budget_remaining = 0;
                    true // throttled
                }
            } else {
                false
            }
        } else {
            false
        }
    }
}

impl Default for DeadlineRunQueue {
    fn default() -> Self { Self::new() }
}