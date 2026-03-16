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
    sync::atomic::AtomicUsize,
    task::Waker,
};
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
    
    pub address_space: Option<AddressSpace>,

    pub is_an_idle_task: bool,

    pub restart_info: Option<RestartInfo>,
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
            address_space: None, 
            is_an_idle_task: false,
            restart_info: None,
        }
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
