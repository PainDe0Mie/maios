//! EEVDF Run Queue — Earliest Eligible Virtual Deadline First
//!
//! ## Algorithm (Stoica & Abdel-Wahab, 1995 + Linux 6.6 refinements)
//!
//! Each task tracks:
//!   - `vruntime`  (V_i): total weighted CPU time consumed. Normalized so
//!                        that a task with weight W running for time t
//!                        accumulates t * (WEIGHT_1024 / W) of vruntime.
//!   - `vdeadline` (d_i): virtual deadline = ve_i + r_i/w_i where
//!                        ve_i is the eligibility time and r_i is the
//!                        requested slice, w_i is the weight.
//!   - `ve`        (e_i): virtual eligibility time = time at which
//!                        the task became eligible to run.
//!
//! **Eligible** tasks: those with ve_i <= min_vruntime (the queue clock).
//! **Scheduling decision**: among eligible tasks, pick min(vdeadline).
//!
//! This gives:
//!   - **Fairness**: vruntime tracks proportional CPU share.
//!   - **Bounded latency**: tasks with small requested slices get short
//!     deadlines and thus low latency, regardless of other tasks' weights.
//!   - **No starvation**: ineligible tasks become eligible as min_vruntime
//!     advances.
//!
//! ## Implementation
//!
//! We maintain two BTreeMaps:
//!   1. `eligible`:   (vdeadline, task_id) → TaskRef  (for pick_next O(log n))
//!   2. `ineligible`: (ve, task_id) → TaskRef         (for eligibility updates)
//!
//! On each tick, we move tasks from `ineligible` to `eligible` as
//! min_vruntime advances past their ve. This is O(k log n) where k is the
//! number of tasks becoming eligible.

use alloc::collections::BTreeMap;

use task_struct::TaskRef;
use crate::{SCHED_MIN_GRANULARITY_NS, SCHED_LATENCY_NS, weight_for_nice, NICE_TO_WMULT};

// ---------------------------------------------------------------------------
// Composite key for ordered BTreeMap
// ---------------------------------------------------------------------------

/// Composite key: (virtual_time, task_id).
/// The task_id acts as a tiebreaker to ensure uniqueness and a stable
/// ordering when two tasks have the same virtual time.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct VtKey(pub u64, pub usize);

// ---------------------------------------------------------------------------
// EEVDF Run Queue
// ---------------------------------------------------------------------------

/// An EEVDF-based run queue for normal (non-RT, non-deadline) tasks.
///
/// Thread safety: the caller holds the per-CPU run-queue lock.
pub struct EevdfRunQueue {
    /// Tasks eligible to run, ordered by virtual deadline (ascending).
    /// pick_next = first entry = minimum vdeadline among eligible tasks.
    eligible: BTreeMap<VtKey, TaskRef>,

    /// Tasks not yet eligible (ve > min_vruntime), ordered by ve (ascending).
    /// On each tick advance, we scan the front of this map.
    ineligible: BTreeMap<VtKey, TaskRef>,

    /// The queue clock: min vruntime among all tasks currently in this queue.
    /// Advances monotonically as tasks run.
    pub min_vruntime: u64,

    /// Total weight of all enqueued tasks (used to normalize slice computation).
    total_weight: u64,

    /// Number of tasks in this queue (eligible + ineligible).
    pub nr_running: usize,
}

impl EevdfRunQueue {
    pub fn new() -> Self {
        EevdfRunQueue {
            eligible: BTreeMap::new(),
            ineligible: BTreeMap::new(),
            min_vruntime: 0,
            total_weight: 0,
            nr_running: 0,
        }
    }

    // -----------------------------------------------------------------------
    // Enqueue / Dequeue
    // -----------------------------------------------------------------------

    /// Add a task to this run queue.
    ///
    /// The task's ve and vdeadline are set if not already initialized, or
    /// adjusted for wakeup (to prevent vruntime catch-up attacks).
    pub fn enqueue(&mut self, task: TaskRef, is_wakeup: bool) {
        let (ve, vdeadline, weight, task_id) = {
            let mut t = task.write();
            let sched = &mut t.sched;

            // For new tasks or tasks returning from sleep, set initial ve.
            // We use max(task.vruntime, min_vruntime) to ensure the task is
            // not immediately eligible if it has accumulated less vruntime
            // than peers (which would be unfair to tasks that kept running).
            if sched.vruntime == 0 || is_wakeup {
                // Wakeup: place at min_vruntime to avoid starvation after sleep,
                // but cap the "bonus" to sched_latency to prevent CPU hogging.
                let catchup = self.min_vruntime.saturating_sub(SCHED_LATENCY_NS / 2);
                sched.vruntime = sched.vruntime.max(catchup);
            }

            let weight = weight_for_nice(sched.nice) as u64;
            // Slice = target latency * (task weight / total weight).
            // Clamped to [min_granularity, latency].
            let slice = self.compute_slice(weight);

            // ve = vruntime (start of eligibility window).
            // vdeadline = ve + slice / weight_normalized.
            // Since vruntime is already weight-normalized, vdeadline = vruntime + slice_ns.
            // (slice_ns is itself divided by weight for normalization.)
            let ve = sched.vruntime;
            let vdeadline = ve + slice;

            sched.ve = ve;
            sched.vdeadline = vdeadline;
            sched.weight = weight;
            sched.on_rq = true;

            (ve, vdeadline, weight, t.id)
        };

        self.total_weight += weight;
        self.nr_running += 1;

        // Determine eligibility.
        if ve <= self.min_vruntime {
            self.eligible.insert(VtKey(vdeadline, task_id), task);
        } else {
            self.ineligible.insert(VtKey(ve, task_id), task);
        }
    }

    /// Remove a task from this run queue (voluntary or preemption).
    pub fn dequeue(&mut self, task: &TaskRef) {
        let (ve, vdeadline, weight, task_id, on_rq) = {
            let mut t = task.write();
            let s = &mut t.sched;
            let was_on = s.on_rq;
            s.on_rq = false;
            (s.ve, s.vdeadline, s.weight, t.id, was_on)
        };

        if !on_rq {
            return; // Already dequeued (idempotent).
        }

        // Try eligible first, then ineligible.
        if self.eligible.remove(&VtKey(vdeadline, task_id)).is_none() {
            self.ineligible.remove(&VtKey(ve, task_id));
        }

        self.total_weight = self.total_weight.saturating_sub(weight);
        self.nr_running = self.nr_running.saturating_sub(1);
    }

    // -----------------------------------------------------------------------
    // Pick next
    // -----------------------------------------------------------------------

    /// Pick the next task to run: min vdeadline among eligible tasks.
    /// Returns `None` if no eligible task exists.
    pub fn pick_next(&mut self) -> Option<TaskRef> {
        // Advance eligibility: move tasks whose ve <= min_vruntime to eligible.
        self.advance_eligibility();

        // The first entry of `eligible` is the minimum-vdeadline task.
        let (&key, _) = self.eligible.iter().next()?;
        self.eligible.remove(&key)
    }

    /// Put a task back after it has run its slice.
    ///
    /// Updates its vruntime, computes a new vdeadline, and re-enqueues.
    pub fn put_prev(&mut self, task: TaskRef) {
        // `enqueue` with is_wakeup=false will use the updated vruntime.
        self.enqueue(task, false);
    }

    // -----------------------------------------------------------------------
    // Tick — update running task's vruntime
    // -----------------------------------------------------------------------

    /// Called from the timer tick. Updates `current_task`'s vruntime by
    /// `elapsed_ns`, checks for preemption, updates min_vruntime.
    ///
    /// Returns `true` if the current task should be preempted.
    pub fn tick(&mut self, current_task: &TaskRef, elapsed_ns: u64) -> bool {
        let (new_vruntime, vdeadline, nice) = {
            let mut t = current_task.write();
            let s = &mut t.sched;
            // vruntime delta = elapsed * (WEIGHT_NICE0 / task_weight).
            // We use fixed-point: delta = elapsed * wmult >> 32.
            let wmult = NICE_TO_WMULT[crate::nice_to_idx(s.nice)] as u64;
            let delta_vt = (elapsed_ns * wmult) >> 32;
            s.vruntime += delta_vt;
            s.exec_runtime += elapsed_ns;
            (s.vruntime, s.vdeadline, s.nice)
        };

        // Update min_vruntime: max(current_min, min(vruntime of eligible tasks)).
        self.update_min_vruntime(new_vruntime);

        // Preempt if:
        //   a) Task has exceeded its vdeadline (overran slice), OR
        //   b) There is an eligible task with smaller vdeadline by more than
        //      SCHED_MIN_GRANULARITY_NS (wakeup preemption throttle).
        let overran = new_vruntime >= vdeadline;
        let preempt_by_wakeup = self.eligible.iter().next().map_or(false, |(&k, _)| {
            // k.0 is the vdeadline of the best eligible task.
            // Preempt if best.vdeadline + granularity < current.vdeadline.
            k.0 + SCHED_MIN_GRANULARITY_NS < vdeadline
        });

        overran || preempt_by_wakeup
    }

    // -----------------------------------------------------------------------
    // Work stealing
    // -----------------------------------------------------------------------

    /// Steal the task with the largest vruntime (most-recently-run).
    /// We steal the "heaviest" task to balance load, while leaving the
    /// fast-deadline tasks to avoid latency spikes on the victim CPU.
    ///
    /// Returns `None` if there are ≤ 1 tasks (no stealing from 1-task queues).
    pub fn steal_one(&mut self) -> Option<TaskRef> {
        if self.nr_running <= 1 {
            return None;
        }
        // Steal from ineligible (highest ve = most recently blocked/ran).
        if let Some((&key, _)) = self.ineligible.iter().next_back() {
            return self.ineligible.remove(&key);
        }
        // Fall back to eligible with highest vdeadline.
        if let Some((&key, _)) = self.eligible.iter().next_back() {
            let task = self.eligible.remove(&key)?;
            {
                let weight = task.read().sched.weight;
                self.total_weight = self.total_weight.saturating_sub(weight);
                self.nr_running = self.nr_running.saturating_sub(1);
            }
            // Caller will re-enqueue on the thief CPU.
            return Some(task);
        }
        None
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Compute the time slice for a task with the given weight.
    ///
    /// slice = SCHED_LATENCY * (task_weight / total_weight)
    ///
    /// Clamped to [SCHED_MIN_GRANULARITY, SCHED_LATENCY].
    fn compute_slice(&self, task_weight: u64) -> u64 {
        if self.total_weight == 0 {
            return SCHED_LATENCY_NS;
        }
        let slice = SCHED_LATENCY_NS * task_weight / (self.total_weight + task_weight);
        slice.clamp(SCHED_MIN_GRANULARITY_NS, SCHED_LATENCY_NS)
    }

    /// Move tasks from `ineligible` to `eligible` as min_vruntime advances.
    fn advance_eligibility(&mut self) {
        // Collect keys to move (can't mutate while iterating).
        let mut to_promote: alloc::vec::Vec<VtKey> = alloc::vec::Vec::new();
        for (&key, _) in self.ineligible.iter() {
            if key.0 <= self.min_vruntime {
                to_promote.push(key);
            } else {
                break; // BTreeMap is sorted; no need to continue.
            }
        }
        for key in to_promote {
            if let Some(task) = self.ineligible.remove(&key) {
                let (ve, vdeadline, task_id) = {
                    let t = task.read();
                    (t.sched.ve, t.sched.vdeadline, t.id)
                };
                self.eligible.insert(VtKey(vdeadline, task_id), task);
            }
        }
    }

    /// Update min_vruntime: the "clock" of this run queue.
    ///
    /// min_vruntime = max(min_vruntime, min(running.vruntime, eligible_min_vdeadline))
    /// It only moves forward, never backward.
    fn update_min_vruntime(&mut self, running_vruntime: u64) {
        let eligible_min = self.eligible.iter().next().map(|(&k, _)| k.0);
        let new_min = match eligible_min {
            Some(e) => running_vruntime.min(e),
            None => running_vruntime,
        };
        if new_min > self.min_vruntime {
            self.min_vruntime = new_min;
        }
    }
}

impl Default for EevdfRunQueue {
    fn default() -> Self { Self::new() }
}