//! This crate contains the basic `Task` structure.
#![no_std]
#![feature(panic_info_message)]
#![feature(negative_impls)]
#![allow(clippy::type_complexity)]

extern crate alloc;

use core::{
    any::Any,
    fmt,
    hash::{Hash, Hasher},
    sync::atomic::{AtomicU8, AtomicUsize, Ordering},
    task::Waker,
};

// ---------------------------------------------------------------------------
// Scheduler types — used by MKS (Mai Kernel Scheduler)
// ---------------------------------------------------------------------------

/// Scheduling policy / class for a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedClass {
    /// Background/batch tasks (lowest priority, no fairness guarantee).
    Idle,
    /// Batch processing (lower than Normal, no interactive boost).
    Batch,
    /// Normal timesharing (EEVDF fairness).
    Normal,
    /// GPU-compute task (eligible for MHC offload).
    Compute,
    /// Real-time round-robin with `timeslice_ns` nanoseconds per slice.
    RoundRobin(u64),
    /// Real-time FIFO (runs until blocked or preempted by higher RT).
    Fifo,
    /// Hard real-time: EDF within deadline class.
    Deadline { period_ns: u64, runtime_ns: u64 },
}

impl Default for SchedClass {
    fn default() -> Self { SchedClass::Normal }
}

/// Per-task scheduling metadata — owned by MKS, stored inside `Task`.
pub struct TaskSchedInfo {
    /// EEVDF virtual runtime (ns, weighted by inverse nice).
    pub vruntime: u64,
    /// Virtual eligible time — earliest point this task may be picked.
    pub ve: u64,
    /// Virtual deadline — used for EEVDF preemption decisions.
    pub vdeadline: u64,
    /// Scheduling weight derived from nice value (1024 = nice 0).
    pub weight: u64,
    /// Inverse weight for fast fixed-point division (2^32 / weight).
    pub wmult: u32,
    /// Nice value in [-20, 19].
    pub nice: i8,
    /// Real-time priority [0, 99] (FIFO/RR tasks only).
    pub rt_priority: u8,
    /// Remaining RR timeslice in nanoseconds.
    pub rr_timeslice_remaining: u64,
    /// Hard CPU affinity: task is pinned to this CPU if `Some`.
    pub pinned_cpu: Option<usize>,
    /// Current scheduling class.
    pub policy: SchedClass,
    /// Last CPU this task executed on (atomic for lock-free reads by stealer).
    pub last_cpu: AtomicUsize,
    /// Absolute timestamp (ns) at which a sleeping task should wake.
    pub wakeup_time_ns: u64,
}

impl Default for TaskSchedInfo {
    fn default() -> Self {
        TaskSchedInfo {
            vruntime: 0,
            ve: 0,
            vdeadline: 0,
            weight: 1024,      // nice 0
            wmult: 4_194_304,  // NICE_TO_WMULT[20]
            nice: 0,
            rt_priority: 0,
            rr_timeslice_remaining: 100_000_000, // 100 ms
            pinned_cpu: None,
            policy: SchedClass::Normal,
            last_cpu: AtomicUsize::new(0),
            wakeup_time_ns: 0,
        }
    }
}
use alloc::{
    boxed::Box,
    string::String,
    sync::Arc,
    vec::Vec,
};
use cpu::{OptionalCpuId}; // On garde juste OptionalCpuId
use crossbeam_utils::atomic::AtomicCell;
use sync_irq::IrqSafeMutex;
use memory::MmiRef;
use stack::Stack;
use mod_mgmt::{AppCrateRef, CrateNamespace, TlsDataImage};
use environment::Environment;
use spin::Mutex;

// --- AJOUT MAI_OS ---
use memory::paging::address_space::AddressSpace; 

/// The generic type of a callback that will be invoked when a task panics.
pub type KillHandler = Box<dyn Fn(&Task, KillReason) + Send + Sync + 'static>;

/// A structure that contains the contextual execution states of a thread.
pub struct Task {
    /// The unique ID of this task.
    pub id: usize,
    /// The name of this task.
    pub name: String,
    /// The definition of this task's runstate (Running, Blocked, etc).
    pub runstate: AtomicCell<RunState>,
    /// The CPU that this task is currently running on.
    pub running_on_cpu: AtomicCell<OptionalCpuId>,
    /// The stack of this task.
    pub kstack: Stack,
    /// The memory management info (page table) for this task (kernel default).
    pub mmi: MmiRef,
    /// The namespace of crates/symbols that this task has access to.
    pub namespace: Arc<CrateNamespace>,
    /// The environment variables for this task.
    pub env: Arc<Mutex<Environment>>,
    /// The specific application crate that this task is running.
    pub app_crate: Option<Arc<AppCrateRef>>,
    /// The Thread-Local Storage (TLS) area for this task.
    pub tls_area: TlsDataImage,
    /// The saved stack pointer (used for context switching).
    pub saved_sp: usize,
    /// A custom kill handler.
    pub kill_handler: IrqSafeMutex<Option<KillHandler>>,
    /// Whether this task is pinned to a specific core.
    pub pinned_core: AtomicCell<OptionalCpuId>,
    /// User-defined context data.
    pub context_data: IrqSafeMutex<Option<Box<dyn Any + Send + Sync>>>,
    /// Wakers for async tasks.
    pub wakers: IrqSafeMutex<Vec<Waker>>,
    
    /// Syscall ABI mode: 0 = Native, 1 = Linux, 2 = Windows.
    /// Stored as AtomicU8 for lock-free access from the syscall hot path.
    pub exec_mode: AtomicU8,

    pub address_space: Option<AddressSpace>,

    pub is_an_idle_task: bool,

    pub restart_info: Option<RestartInfo>,

    /// MKS scheduling metadata (vruntime, nice, policy, etc.).
    pub sched: TaskSchedInfo,
}

pub struct RestartInfo {
    pub argument: alloc::boxed::Box<dyn core::any::Any + Send + Sync + 'static>,
    pub func:     alloc::boxed::Box<dyn core::any::Any + Send + Sync + 'static>,
}

impl Task {
    /// Creates a new Task structure.
    pub fn new(
        id: usize,
        name: String,
        kstack: Stack,
        mmi: MmiRef,
        namespace: Arc<CrateNamespace>,
        env: Arc<Mutex<Environment>>,
        app_crate: Option<Arc<AppCrateRef>>,
        tls_area: TlsDataImage,
    ) -> Task {
        Task {
            id,
            name,
            runstate: AtomicCell::new(RunState::Runnable),
            running_on_cpu: AtomicCell::new(None.into()),
            kstack,
            mmi,
            namespace,
            env,
            app_crate,
            tls_area,
            saved_sp: 0,
            kill_handler: IrqSafeMutex::new(None),
            pinned_core: AtomicCell::new(None.into()),
            context_data: IrqSafeMutex::new(None),
            wakers: IrqSafeMutex::new(Vec::new()),
            exec_mode: AtomicU8::new(0), // 0 = Native
            address_space: None,
            is_an_idle_task: false,
            restart_info: None,
            sched: TaskSchedInfo::default(),
        }
    }

    /// Update scheduling weight table after a nice value change.
    ///
    /// Must be called whenever `sched.nice` is modified.
    pub fn update_weight(&mut self) {
        let idx = (self.sched.nice + 20).clamp(0, 39) as usize;
        // Linux-derived weight table: each step is a 1.25× multiplier.
        const WEIGHT: [u32; 40] = [
            88761, 71755, 56483, 46273, 36291,
            29154, 23254, 18705, 14949, 11916,
             9548,  7620,  6100,  4904,  3906,
             3121,  2501,  1991,  1586,  1277,
             1024,   820,   655,   526,   423,
              335,   272,   215,   172,   137,
              110,    87,    70,    56,    45,
               36,    29,    23,    18,    15,
        ];
        const WMULT: [u32; 40] = [
              48388,   59856,   76040,   92818,  118348,
             147320,  184698,  229616,  287308,  360437,
             449829,  563644,  704093,  875809, 1099582,
            1376151, 1717300, 2157191, 2708050, 3363326,
            4194304, 5237765, 6557202, 8165337,10153587,
           12820798,15790321,19976592,24970740,31350126,
           39045157,49367440,61356676,76695844,95443717,
          119304647,148102320,186737708,238609294,286331153,
        ];
        self.sched.weight = WEIGHT[idx] as u64;
        self.sched.wmult  = WMULT[idx];
    }

    /// Set nice value and recompute scheduling weight.
    pub fn set_nice(&mut self, nice: i8) {
        self.sched.nice = nice.clamp(-20, 19);
        self.update_weight();
    }

    pub fn block_initing_task(&mut self) -> Result<(), RunState> {
        match self.runstate.compare_exchange(RunState::Initing, RunState::Blocked) {
            Ok(_) => Ok(()),
            Err(s) => Err(s),
        }
    }

    pub fn make_inited_task_runnable(&mut self) -> Result<(), RunState> {
        match self.runstate.compare_exchange(RunState::Initing, RunState::Runnable) {
            Ok(_) => Ok(()),
            Err(s) => Err(s),
        }
    }

    pub fn get_namespace(&self) -> &Arc<CrateNamespace> { &self.namespace }
    pub fn get_environment(&self) -> &Arc<Mutex<Environment>> { &self.env }
    pub fn get_app_crate(&self) -> Option<&Arc<AppCrateRef>> { self.app_crate.as_ref() }
}

impl PartialEq for Task {
    fn eq(&self, other: &Task) -> bool { self.id == other.id }
}
impl Eq for Task { }
impl Hash for Task {
    fn hash<H: Hasher>(&self, state: &mut H) { self.id.hash(state); }
}
impl fmt::Debug for Task {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Task {{ id: {}, name: '{}' }}", self.id, self.name)
    }
}
impl fmt::Display for Task {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}{{{}}}", self.name, self.id)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RunState {
    Initing,
    Runnable,
    Blocked,
    Exited(ExitValue),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ExitValue {
    Completed(isize),
    Killed(KillReason),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum KillReason {
    Requested,
    Panic,
    Exception(u8),
}

pub enum InheritedStates<'t> {
    FromTask(&'t Task),
    Custom {
        mmi: MmiRef,
        namespace: Arc<CrateNamespace>,
        env: Arc<Mutex<Environment>>,
        app_crate: Option<Arc<AppCrateRef>>,
    }
}
