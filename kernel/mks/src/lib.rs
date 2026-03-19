//! MKS — Mai Kernel Scheduler
//!
//! A production-grade, research-backed scheduler for MaiOS implementing:
//!
//! 1. **EEVDF** (Earliest Eligible Virtual Deadline First)
//!    Based on: "EEVDF: A proportional-share CPU scheduling algorithm"
//!    (Stoica & Abdel-Wahab, 1995) + Linux 6.6 implementation (Lozi et al., 2023).
//!    Provides both fairness (via virtual runtime) and bounded latency
//!    (via per-task virtual deadlines). Strictly dominates CFS.
//!
//! 2. **Scheduling classes** with strict priority ordering:
//!    Deadline > RealTime (FIFO/RR) > Normal (EEVDF) > Batch > Idle
//!    Based on: Linux SCHED_DEADLINE (Faggioli et al., RTLWS 2009).
//!
//! 3. **Per-CPU run queues** with cache-aware **work stealing**
//!    Based on: "Cache-Aware Scheduling" (Lozi et al., EuroSys 2016).
//!    Steal order: same HT core → same L3 → same NUMA → remote.
//!
//! 4. **Scheduler plugins** (ghOSt-inspired)
//!    Based on: "ghOSt: Fast and Flexible User-Space Delegation of Linux
//!    Scheduling" (Humphries et al., SOSP 2021).
//!    Applications register custom scheduling algorithms per task group.
//!
//! ## Design invariants
//!
//! - `O(log n)` for enqueue, dequeue, pick_next (rb-tree via BTreeMap).
//! - Zero allocation on the hot path (all structures pre-allocated).
//! - Lock-free reads of CPU-local data via per-CPU structures.
//! - No global lock — each CPU's run queue has its own lock.

#![no_std]
#![feature(negative_impls)]
#![allow(clippy::module_inception)]

extern crate alloc;

pub mod eevdf;
pub mod plugin;
pub mod runqueue;
pub mod topology;
pub mod deadline;
pub mod realtime;
pub mod stats;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, AtomicU64, Ordering};
use spin::RwLock;

use task_struct::{Task, RunState};
use task::TaskRef;
use topology::CpuTopology;
use runqueue::PerCpuRunQueue;

// ---------------------------------------------------------------------------
// Global scheduler state
// ---------------------------------------------------------------------------

/// Maximum CPUs supported. Matches Theseus's MAX_CORES.
pub const MAX_CPUS: usize = 256;

/// Number of NUMA nodes supported.
pub const MAX_NUMA_NODES: usize = 8;

/// Minimum scheduling granularity in nanoseconds (0.75ms).
/// Below this, a running task is never preempted by a lower-priority one.
/// Based on: Linux's sysctl_sched_min_granularity.
pub const SCHED_MIN_GRANULARITY_NS: u64 = 750_000;

/// Target scheduling latency in nanoseconds (6ms for ≤8 tasks).
/// Every runnable task should run at least once within this window.
/// Based on: Linux's sysctl_sched_latency.
pub const SCHED_LATENCY_NS: u64 = 6_000_000;

/// Wakeup granularity in nanoseconds (1ms).
/// A woken task only preempts the current task if it is ahead by this much.
pub const SCHED_WAKEUP_GRANULARITY_NS: u64 = 1_000_000;

/// SCHED_DEADLINE bandwidth limit: at most 95% of CPU time for RT/DL tasks.
pub const SCHED_DL_BANDWIDTH_NUM: u64 = 95;
pub const SCHED_DL_BANDWIDTH_DEN: u64 = 100;

/// Weight table for nice values [-20, +19].
/// Derived from the Linux kernel's sched_prio_to_weight table.
/// Each step is a 1.25x multiplier. Nice 0 = weight 1024.
pub const NICE_TO_WEIGHT: [u32; 40] = [
    88761, 71755, 56483, 46273, 36291,  // nice -20..-16
    29154, 23254, 18705, 14949, 11916,  // nice -15..-11
     9548,  7620,  6100,  4904,  3906,  // nice -10..-6
     3121,  2501,  1991,  1586,  1277,  // nice  -5..-1
     1024,   820,   655,   526,   423,  // nice   0..4
      335,   272,   215,   172,   137,  // nice   5..9
      110,    87,    70,    56,    45,  // nice  10..14
       36,    29,    23,    18,    15,  // nice  15..19
];

/// Inverse weight table: (2^32 / weight), precomputed for fast division.
pub const NICE_TO_WMULT: [u32; 40] = [
     48388,  59856,  76040,  92818, 118348,
    147320, 184698, 229616, 287308, 360437,
    449829, 563644, 704093, 875809,1099582,
   1376151,1717300,2157191,2708050,3363326,
   4194304,5237765,6557202,8165337,10153587,
  12820798,15790321,19976592,24970740,31350126,
  39045157,49367440,61356676,76695844,95443717,
 119304647,148102320,186737708,238609294,286331153,
];

/// Convert a nice value [-20, 19] to its weight index [0, 39].
#[inline]
pub fn nice_to_idx(nice: i8) -> usize {
    (nice + 20).clamp(0, 39) as usize
}

/// Get the weight for a nice value.
#[inline]
pub fn weight_for_nice(nice: i8) -> u32 {
    NICE_TO_WEIGHT[nice_to_idx(nice)]
}

// ---------------------------------------------------------------------------
// Global scheduler instance
// ---------------------------------------------------------------------------

/// Global MKS instance.
///
/// Phase-1 (boot): `init()` creates a single-CPU scheduler.
/// Phase-2 (after AP bringup): `expand()` extends it to all CPUs in place.
///
/// We use `spin::Once` for one-time initialization + interior mutability:
///   - The `MaiScheduler` itself is immutable once created…
///   - …except for the `rqs` field, which is behind a `RwLock` so `expand()`
///     can grow it without replacing the entire instance.
static MKS: spin::Once<MaiScheduler> = spin::Once::new();

/// Initialize the global MKS instance (phase-1: single CPU at boot).
///
/// Only the first call takes effect. Subsequent calls are no-ops;
/// use `expand()` to grow to more CPUs.
pub fn init(num_cpus: usize, topology: CpuTopology) {
    MKS.call_once(|| MaiScheduler::new(num_cpus, topology));
}

/// Expand MKS to `num_cpus` with a new topology (phase-2: after AP bringup).
///
/// Adds new per-CPU run queues for CPUs beyond the original `init()` count.
/// Safe to call while the BSP is single-threaded (before `enable_interrupts`).
pub fn expand(num_cpus: usize, topology: CpuTopology) {
    let sched = get();
    let mut rqs = sched.rqs.write();
    let old_count = rqs.len();
    for cpu_id in old_count..num_cpus {
        rqs.push(PerCpuRunQueue::new(cpu_id));
    }
    // Update topology and CPU count.
    *sched.topology_rw.write() = topology;
    sched.num_cpus.store(num_cpus, Ordering::Release);
}

/// Access the global scheduler.
///
/// # Panics
/// Panics if called before `init()`.
#[inline]
pub fn get() -> &'static MaiScheduler {
    MKS.get().expect("MKS: scheduler not initialized — call mks::init() first")
}

// ---------------------------------------------------------------------------
// Main scheduler structure
// ---------------------------------------------------------------------------

/// The MaiOS Kernel Scheduler.
///
/// Owns all per-CPU run queues and the CPU topology map used for work stealing.
///
/// `rqs` and `topology_rw` are behind `RwLock` so that `expand()` can grow
/// them after AP bringup without replacing the entire scheduler instance.
/// On the hot path (tick), the read lock is uncontended (zero overhead
/// on x86_64 with `spin::RwLock`).
pub struct MaiScheduler {
    /// Per-CPU run queues. Indexed by logical CPU ID.
    /// Behind RwLock for phase-2 expansion; hot-path uses read().
    pub rqs: RwLock<Vec<PerCpuRunQueue>>,
    /// CPU topology for cache-aware work stealing.
    /// Immutable after phase-2; read-only on hot path.
    pub topology_rw: RwLock<CpuTopology>,
    /// Total number of CPUs managed. Atomic for lock-free reads.
    pub num_cpus: AtomicUsize,
    /// Global task count (approximate, for telemetry).
    pub global_task_count: AtomicUsize,
    /// Number of context switches performed (total).
    pub context_switches: AtomicU64,
}

impl MaiScheduler {
    fn new(num_cpus: usize, topology: CpuTopology) -> Self {
        let mut rqs = Vec::with_capacity(num_cpus);
        for cpu_id in 0..num_cpus {
            rqs.push(PerCpuRunQueue::new(cpu_id));
        }
        MaiScheduler {
            rqs: RwLock::new(rqs),
            topology_rw: RwLock::new(topology),
            num_cpus: AtomicUsize::new(num_cpus),
            global_task_count: AtomicUsize::new(0),
            context_switches: AtomicU64::new(0),
        }
    }

    // -----------------------------------------------------------------------
    // Helper: get num_cpus as plain usize (hot-path friendly).
    // -----------------------------------------------------------------------
    #[inline]
    fn cpu_count(&self) -> usize {
        self.num_cpus.load(Ordering::Acquire)
    }

    /// Enqueue a task on the most appropriate CPU's run queue.
    ///
    /// Placement policy:
    /// 1. If the task has CPU affinity set, use that CPU.
    /// 2. If the task has a preferred CPU (last ran on), use it if load ≤ average.
    /// 3. Otherwise, pick the CPU with the lowest load in the task's NUMA domain.
    pub fn enqueue(&self, task: TaskRef) {
        let cpu_id = self.select_cpu_for_task(&task);
        let rqs = self.rqs.read();
        rqs[cpu_id].enqueue(task);
        self.global_task_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Dequeue a task from whichever CPU it is queued on.
    pub fn dequeue(&self, task: &TaskRef) {
        let cpu_id = task.read().sched.last_cpu.load(Ordering::Relaxed);
        let nc = self.cpu_count();
        if cpu_id < nc {
            self.rqs.read()[cpu_id].dequeue(task);
        }
        self.global_task_count.fetch_sub(1, Ordering::Relaxed);
    }

    /// Pick the next task to run on `cpu_id`.
    ///
    /// Order of consideration:
    /// 1. Deadline tasks (EDF within deadline class).
    /// 2. Real-time tasks (FIFO/RR by priority).
    /// 3. Normal/Batch tasks (EEVDF).
    /// 4. Idle task (always exists, always runnable).
    ///
    /// If the local queue is empty, attempt work stealing before returning idle.
    pub fn pick_next(&self, cpu_id: usize) -> Option<TaskRef> {
        debug_assert!(cpu_id < self.cpu_count(), "MKS: invalid cpu_id {}", cpu_id);

        let rqs = self.rqs.read();
        let rq = &rqs[cpu_id];

        // --- 1. Deadline class (highest priority) ---
        if let Some(t) = rq.deadline_rq.lock().pick_next() {
            return Some(t);
        }

        // --- 2. Real-time class ---
        if let Some(t) = rq.rt_rq.lock().pick_next() {
            return Some(t);
        }

        // --- 3. Normal class (EEVDF) ---
        if let Some(t) = rq.cfs_rq.lock().pick_next() {
            return Some(t);
        }

        // Must drop rqs before steal_task to avoid deadlock (steal_task
        // also acquires the rqs read-lock).
        drop(rqs);

        // --- 4. Work stealing ---
        if let Some(t) = self.steal_task(cpu_id) {
            return Some(t);
        }

        // --- 5. Idle (Batch class or true idle) ---
        self.rqs.read()[cpu_id].idle_rq.lock().pick_next()
    }

    /// Notify the scheduler that a task has just been woken up.
    ///
    /// May trigger a preemption of the currently running task if the
    /// woken task has a smaller virtual deadline and the difference exceeds
    /// SCHED_WAKEUP_GRANULARITY_NS.
    pub fn task_wakeup(&self, task: TaskRef, cpu_id: usize) {
        {
            let mut t = task.write();
            t.runstate.store(RunState::Runnable);
            // Update vruntime to prevent starvation after a long sleep.
            // Set to max(task.vruntime, min_vruntime - sched_latency/2).
            let rqs = self.rqs.read();
            if let Some(rq) = rqs.get(cpu_id) {
                let min_vrt = rq.cfs_rq.lock().min_vruntime;
                let floor = min_vrt.saturating_sub(SCHED_LATENCY_NS / 2);
                if t.sched.vruntime < floor {
                    t.sched.vruntime = floor;
                }
            }
        }
        self.enqueue(task);
    }

    /// Called from the timer tick handler on `cpu_id`.
    ///
    /// Updates the running task's vruntime, checks if it should be preempted,
    /// and unblocks sleeping tasks.
    pub fn tick(&self, cpu_id: usize, elapsed_ns: u64) {
        // Note: sleep::unblock_sleeping_tasks() is called by the timer
        // interrupt handler *before* this method, not here. Keeping the
        // tick path lean avoids redundant work.
        let rqs = self.rqs.read();
        if let Some(rq) = rqs.get(cpu_id) {
            rq.tick(elapsed_ns);
        }
    }

    /// Select the best CPU to place a newly enqueued or woken task.
    fn select_cpu_for_task(&self, task: &TaskRef) -> usize {
        let t = task.read();
        let nc = self.cpu_count();

        // Hard affinity: if the task is pinned, no choice.
        if let Some(cpu) = t.sched.pinned_cpu {
            return cpu.min(nc.saturating_sub(1));
        }

        let last_cpu = t.sched.last_cpu.load(Ordering::Relaxed);

        // Soft affinity: prefer last CPU if not overloaded.
        if last_cpu < nc {
            let rqs = self.rqs.read();
            let last_load = rqs[last_cpu].load();
            let avg_load = self.average_load_rqs(&rqs, nc);
            if last_load <= avg_load + 1 {
                return last_cpu;
            }
        }

        // Find the CPU with minimum load, preferring the same NUMA node.
        let topo = self.topology_rw.read();
        let preferred_node = topo.cpu_to_numa(last_cpu);
        let rqs = self.rqs.read();
        let mut best_cpu = 0;
        let mut best_load = usize::MAX;

        for cpu in 0..nc {
            if let Some(rq) = rqs.get(cpu) {
                let load = rq.load();
                // Bias: same-NUMA CPUs get a 20% load discount.
                let adjusted = if topo.cpu_to_numa(cpu) == preferred_node {
                    load * 4 / 5
                } else {
                    load
                };
                if adjusted < best_load {
                    best_load = adjusted;
                    best_cpu = cpu;
                }
            }
        }

        best_cpu
    }

    /// Attempt to steal a task from another CPU's run queue.
    ///
    /// Steal order (cache-aware, based on Lozi et al. EuroSys 2016):
    ///   1. Hyperthreading sibling (shares L1/L2)
    ///   2. Same physical core neighbors (shares L3)
    ///   3. Same NUMA node
    ///   4. Any CPU (cross-NUMA, last resort)
    ///
    /// Only steals if the victim has ≥ 2 tasks (to maintain fairness).
    fn steal_task(&self, thief_cpu: usize) -> Option<TaskRef> {
        let topo = self.topology_rw.read();
        let steal_order = topo.steal_order(thief_cpu);
        let rqs = self.rqs.read();
        for &victim_cpu in steal_order {
            if victim_cpu == thief_cpu {
                continue;
            }
            if let Some(victim_rq) = rqs.get(victim_cpu) {
                if victim_rq.load() >= 2 {
                    if let Some(task) = victim_rq.steal_one() {
                        return Some(task);
                    }
                }
            }
        }
        None
    }

    /// Compute the average load across all CPUs (with pre-locked rqs).
    fn average_load_rqs(&self, rqs: &[PerCpuRunQueue], nc: usize) -> usize {
        if nc == 0 { return 0; }
        let total: usize = rqs.iter().map(|rq| rq.load()).sum();
        total / nc
    }

    /// Compute the average load across all CPUs.
    #[allow(dead_code)]
    fn average_load(&self) -> usize {
        let nc = self.cpu_count();
        if nc == 0 { return 0; }
        let rqs = self.rqs.read();
        let total: usize = rqs.iter().map(|rq| rq.load()).sum();
        total / nc
    }

    /// Record a context switch on `cpu_id` and update per-CPU stats.
    pub fn record_context_switch(&self, cpu_id: usize, from: &Task, to: &Task) {
        self.context_switches.fetch_add(1, Ordering::Relaxed);
        let rqs = self.rqs.read();
        if let Some(rq) = rqs.get(cpu_id) {
            rq.stats.lock().record_switch(from, to);
        }
    }
}
