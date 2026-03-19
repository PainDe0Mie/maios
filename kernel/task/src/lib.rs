//! Key types and functions for multitasking.
#![no_std]
#![feature(thread_local)]
#![feature(naked_functions)]
#![feature(let_chains)]

extern crate alloc;
extern crate log;
extern crate spin;
extern crate irq_safety;
extern crate context_switch;
extern crate cls;
extern crate cpu;
extern crate environment;
extern crate memory;
extern crate mod_mgmt;
extern crate no_drop;
extern crate stack;
extern crate sync_irq;
extern crate task_struct;

use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
    format,
    collections::BTreeMap,
};
use spin::Mutex;

// --- MODULES ---
pub mod joinable;
pub mod scheduler;
pub use self::joinable::{JoinableTaskRef, ExitableTaskRef};
pub use self::scheduler::schedule;
use preemption::PreemptionGuard;

// --- RE-EXPORTS ---
pub use task_struct::{Task, RunState, ExitValue, KillReason, InheritedStates, KillHandler, RestartInfo};

use cpu::CpuId;
use memory::MmiRef;
use mod_mgmt::{AppCrateRef, CrateNamespace, TlsDataImage};
use environment::Environment;
use stack::Stack;

// =========================================================================
// GLOBAL TASK REGISTRY
// =========================================================================
static TASK_LIST: Mutex<BTreeMap<usize, TaskRef>> = Mutex::new(BTreeMap::new());

// =========================================================================
// CURRENT TASK (CPU-local storage)
// =========================================================================
#[cls::cpu_local]
static CURRENT_TASK: Option<TaskRef> = None;

// =========================================================================
// STRUCTURES
// =========================================================================

/// A shareable, strong reference to a `Task`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TaskRef(pub Arc<Task>);

impl TaskRef {
    pub fn new(
        task_id: usize,
        name: String,
        kstack: Stack,
        mmi: MmiRef,
        namespace: Arc<CrateNamespace>,
        env: Arc<Mutex<Environment>>,
        app_crate: Option<Arc<AppCrateRef>>,
        tls_area: TlsDataImage,
    ) -> TaskRef {
        let task_ref = TaskRef(Arc::new(Task::new(
            task_id, name, kstack, mmi, namespace, env, app_crate, tls_area,
        )));

        // Register in the global task list at creation time.
        TASK_LIST.lock().insert(task_id, task_ref.clone());

        task_ref
    }

    pub fn from_task(task: Task) -> TaskRef {
        let id = task.id;
        let task_ref = TaskRef(alloc::sync::Arc::new(task));
        TASK_LIST.lock().insert(id, task_ref.clone());
        task_ref
    }

    /// Returns a weak reference to this task (does not keep the task alive).
    pub fn downgrade(&self) -> WeakTaskRef {
        WeakTaskRef(Arc::downgrade(&self.0))
    }

    /// Read access to the task (immutable borrow).
    ///
    /// Since `TaskRef: Deref<Target = Task>`, this is a zero-cost alias
    /// for `&**self`. It exists so MKS can write `task.read().sched.xxx`
    /// in a symmetric style with `task.write()`.
    #[inline(always)]
    pub fn read(&self) -> &Task {
        &**self
    }

    /// Write (mutable) access to the task for MKS scheduler internals.
    ///
    /// # Safety contract (enforced by call sites)
    /// The MKS run-queue lock (per-CPU `spin::Mutex`) ensures that at most
    /// one CPU holds a `write()` guard on a given task at a time.
    /// All callers of this method must be inside a locked run-queue section.
    #[inline(always)]
    pub fn write(&self) -> MksSchedGuard<'_> {
        MksSchedGuard {
            // SAFETY: Guaranteed unique access via per-CPU run-queue lock.
            task: unsafe { &mut *(alloc::sync::Arc::as_ptr(&self.0) as *mut Task) },
        }
    }

    pub fn unblock(&self) -> Result<(), RunState> {
        match self.runstate.compare_exchange(RunState::Blocked, RunState::Runnable) {
            Ok(_) => {
                scheduler::add_task(self.clone());
                Ok(())
            }
            Err(current_state) => {
                if current_state == RunState::Runnable {
                    Ok(())
                } else {
                    Err(current_state)
                }
            }
        }
    }

    pub fn block(&self) -> Result<(), RunState> {
        match self.runstate.compare_exchange(RunState::Runnable, RunState::Blocked) {
            Ok(_) => {
                scheduler::remove_task(self);
                Ok(())
            }
            Err(RunState::Blocked) => {
                scheduler::remove_task(self);
                Ok(())
            }
            Err(current_state) => Err(current_state),
        }
    }

    pub fn is_running(&self) -> bool {
        Option::<CpuId>::from(self.running_on_cpu.load()).is_some()
    }

    pub fn is_runnable(&self) -> bool {
        self.runstate.load() == RunState::Runnable
    }

    /// Kills this task with the given `reason`.
    ///
    /// A task can only be killed if it is `Runnable` or `Blocked`.
    /// If it has already been killed, this will succeed.
    /// If it has already completed, this will fail.
    pub fn kill(&self, reason: KillReason) -> Result<(), RunState> {
        loop {
            let curr_state = self.runstate.load();
            match curr_state {
                RunState::Runnable | RunState::Blocked => {
                    if self.runstate.compare_exchange(curr_state, RunState::Exited(ExitValue::Killed(reason))).is_ok() {
                        // successfully killed it, now remove it from the scheduler's runqueue
                        scheduler::remove_task(self);
                        return Ok(());
                    }
                    // if compare_exchange failed, loop to try again.
                }
                RunState::Exited(ExitValue::Killed(_)) => return Ok(()), // already killed, that's fine.
                state => return Err(state), // already exited, initing, or other state
            }
        }
    }
}

impl core::ops::Deref for TaskRef {
    type Target = Task;
    fn deref(&self) -> &Task { &self.0 }
}

/// Mutable guard returned by `TaskRef::write()`.
///
/// Provides `DerefMut<Target = Task>` so MKS can mutate scheduling fields
/// (vruntime, nice, policy, etc.) while holding the per-CPU run-queue lock.
pub struct MksSchedGuard<'a> {
    task: &'a mut Task,
}

impl<'a> core::ops::Deref for MksSchedGuard<'a> {
    type Target = Task;
    #[inline(always)]
    fn deref(&self) -> &Task { self.task }
}

impl<'a> core::ops::DerefMut for MksSchedGuard<'a> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Task { self.task }
}

/// A weak reference to a `Task` (does not keep the task alive).
#[derive(Clone, Debug)]
pub struct WeakTaskRef(pub Weak<Task>);

impl WeakTaskRef {
    pub fn upgrade(&self) -> Option<TaskRef> {
        self.0.upgrade().map(TaskRef)
    }
}

// =========================================================================
// GLOBAL TASK MANAGEMENT API
// =========================================================================

/// Retrieves a task by its ID.
pub fn get_task(id: usize) -> Option<TaskRef> {
    TASK_LIST.lock().get(&id).cloned()
}

/// Returns every active task as `(id, TaskRef)` pairs.
pub fn all_tasks() -> Vec<(usize, TaskRef)> {
    TASK_LIST.lock().iter().map(|(k, v)| (*k, v.clone())).collect()
}

/// Switches context from the current task to `next`.
pub fn task_switch(next: TaskRef, cpu_id: CpuId, preemption_guard: PreemptionGuard) -> (bool, PreemptionGuard) {
    let curr = match get_my_current_task() {
        Some(t) => t,
        None => {
            CURRENT_TASK.set(Some(next));
            return (true, preemption_guard);
        }
    };

    // *** GUARD: ne jamais switcher vers une tâche non-initialisée ***
    if next.saved_sp == 0 {
        log::warn!("task_switch: skipping switch to task '{}' (id {}) because saved_sp == 0",
            next.name, next.id);
        return (false, preemption_guard);
    }

    // *** DEBUG GUARD: Vérifier si saved_sp est suspect (trop bas) ***
    if next.saved_sp < 0x10000 {
        log::error!("CRITICAL BUG: task_switch: attempting to switch to task '{}' (id {}) with suspicious saved_sp: {:#X}", next.name, next.id, next.saved_sp);
        // On pourrait panic! ici, mais le log nous aidera à identifier la tâche coupable.
    }

    // *** GUARD: Check if the task is already dead ***
    if let RunState::Exited(_) = next.runstate.load() {
        log::error!("CRITICAL BUG: task_switch: attempting to switch to dead task '{}' (id {})", next.name, next.id);
        return (false, preemption_guard);
    }

    // Ne pas switcher vers soi-même
    if curr.id == next.id {
        return (false, preemption_guard);
    }

    curr.running_on_cpu.store(Option::<CpuId>::None.into());
    next.running_on_cpu.store(Option::<CpuId>::Some(cpu_id).into());
    CURRENT_TASK.set(Some(next.clone()));

    if let Some(ref new_space) = next.address_space {
        unsafe { new_space.switch_to(); }
    }

    // unsafe {
    //     let old_sp_ptr = &mut (*curr.0.as_ptr()).saved_sp as *mut usize;
    //     let new_sp_val = next.saved_sp;
    //     mai_context_switch(old_sp_ptr, new_sp_val);
    // }

    unsafe {
        let old_sp_ptr = &curr.saved_sp as *const usize as *mut usize;
        let new_sp_val = next.saved_sp;
        // By using the context_switch function from the `context_switch` crate,
        // we ensure that the registers saved/restored here match the `Context`
        // struct that is created in the `spawn` crate, as both crates depend
        // on the `context_switch` crate.
        // The previous implementation defined its own `mai_context_switch` function
        // which was out of sync with the `Context` struct, causing stack corruption.
        context_switch::context_switch(old_sp_ptr, new_sp_val);
    }

    (true, preemption_guard)
}

pub fn get_my_current_task() -> Option<TaskRef> {
    CURRENT_TASK.update(|t| t.clone())
}

pub fn get_my_current_task_id() -> usize {
    get_my_current_task().map(|t| t.id).unwrap_or(0)
}

pub fn set_kill_handler(handler: KillHandler) -> Result<(), &'static str> {
    let current_task = get_my_current_task().ok_or("could not get current task")?;
    *current_task.kill_handler.lock() = Some(handler);
    Ok(())
}

pub fn take_kill_handler() -> Option<KillHandler> {
    get_my_current_task().and_then(|t| t.kill_handler.lock().take())
}

pub fn with_current_task<F, R>(f: F) -> Result<R, &'static str>
where
    F: FnOnce(&TaskRef) -> R,
{
    let current_task = get_my_current_task().ok_or("no current task")?;
    Ok(f(&current_task))
}

// =========================================================================
// BOOTSTRAP
// =========================================================================

pub fn init_bootstrap_task(
    cpu_id: CpuId,
    kstack: Stack,
    mmi: MmiRef,
    namespace: Arc<CrateNamespace>,
    env: Arc<Mutex<Environment>>,
) -> Result<TaskRef, &'static str> {
    // Bootstrap tasks do not need a TLS data image — use the dedicated
    // empty constructor instead of `default()`, which does not exist on
    // `LocalStorageDataImage`.
    let tls: TlsDataImage = TlsDataImage::new();

    let bootstrap_task = TaskRef::new(
        0,
        format!("bootstrap_cpu_{}", cpu_id),
        kstack,
        mmi,
        namespace,
        env,
        None,
        tls,
    );

    bootstrap_task.running_on_cpu.store(Option::<CpuId>::Some(cpu_id).into());
    CURRENT_TASK.set(Some(bootstrap_task.clone()));
    Ok(bootstrap_task)
}

pub fn bootstrap_task_to_joinable(
    bootstrap_task: TaskRef,
) -> (JoinableTaskRef, ExitableTaskRef) {
    joinable::bootstrap_task_to_joinable(bootstrap_task)
}

pub struct ScheduleOnDrop;
impl Drop for ScheduleOnDrop {
    fn drop(&mut self) {
        scheduler::schedule();
    }
}

pub type FailureCleanupFunction = fn(crate::ExitableTaskRef, KillReason) -> !;

static NEXT_TASK_ID: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(1); // 0 is reserved for bootstrap

pub fn get_next_task_id() -> usize {
    NEXT_TASK_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
}

/// Creates the bootstrap task for a CPU and returns (JoinableTaskRef, ExitableTaskRef).
pub fn bootstrap_task(
    cpu_id: CpuId,
    stack: no_drop::NoDrop<stack::Stack>,
    mmi: MmiRef,
    namespace: alloc::sync::Arc<mod_mgmt::CrateNamespace>,
    env: alloc::sync::Arc<spin::Mutex<environment::Environment>>,
) -> Result<(JoinableTaskRef, ExitableTaskRef), &'static str> {
    let tls = mod_mgmt::TlsDataImage::new();
    let bootstrap_task = TaskRef::new(
        0,
        alloc::format!("bootstrap_cpu_{}", cpu_id),
        no_drop::NoDrop::into_inner(stack),
        mmi,
        namespace,
        env,
        None,
        tls,
    );
    bootstrap_task.running_on_cpu.store(Option::<CpuId>::Some(cpu_id).into());
    CURRENT_TASK.set(Some(bootstrap_task.clone()));
    Ok(joinable::bootstrap_task_to_joinable(bootstrap_task))
}

pub fn init_current_task(
    task_id: usize,
    _extra: Option<()>,
) -> Result<ExitableTaskRef, &'static str> {
    let task_ref = get_task(task_id)
        .ok_or("init_current_task: task ID not found in TASK_LIST")?;
    CURRENT_TASK.set(Some(task_ref.clone()));
    task_ref.running_on_cpu.store(Option::<CpuId>::Some(cpu::current_cpu()).into());
    Ok(ExitableTaskRef(task_ref))
}
