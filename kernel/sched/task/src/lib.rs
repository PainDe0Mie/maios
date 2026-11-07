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
    /// for `&**self` (kept for source compatibility).
    #[inline(always)]
    pub fn read(&self) -> &Task {
        &**self
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
                        notify_exit(self);
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

#[derive(Clone)]
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

/// Tâche qui vient de céder le CPU, en attente de `finish_task_switch()` sur le cœur d'arrivée.
#[cls::cpu_local]
static SWITCH_PREV: Option<TaskRef> = None;

/// Changement de contexte de `prev` vers `next` (appelé par `scheduler::schedule()`, verrou de
/// file relâché, interruptions coupées).
///
/// `next.running_on_cpu` est posé *avant* le changement ; celui de `prev` n'est effacé qu'*après*,
/// par la tâche d'arrivée (`finish_task_switch`). Tant qu'il est posé, aucun autre cœur ne peut
/// reprendre `prev` (réveil et vol l'attendent ou l'ignorent).
///
/// Renvoie, une fois `prev` de nouveau élue plus tard, la tâche qui lui a cédé le CPU : l'appelant
/// la libère après avoir rétabli les interruptions.
pub(crate) fn switch_to(prev: TaskRef, next: TaskRef, cpu_id: CpuId) -> Option<TaskRef> {
    if next.saved_sp < 0x10000 {
        panic!("BUG: MaiSched: task '{}' (id {}) has an invalid saved_sp {:#X}", next.name, next.id, next.saved_sp);
    }
    let none_val: cpu::OptionalCpuId = Option::<CpuId>::None.into();
    let some_val: cpu::OptionalCpuId = Option::<CpuId>::Some(cpu_id).into();
    if let Err(other) = next.running_on_cpu.compare_exchange(none_val, some_val) {
        panic!(
            "BUG: MaiSched: tache '{}' (id {}) choisie sur le coeur {} alors qu'elle tourne sur {:?} \
             (prev '{}' id {}, file {}, etat {:?}, inactive {}, nouvelle {})",
            next.name, next.id, cpu_id.value(), Option::<CpuId>::from(other), prev.name, prev.id,
            next.sched.cpu.load(core::sync::atomic::Ordering::Relaxed), next.runstate.load(),
            next.is_an_idle_task, Arc::ptr_eq(&prev.0, &next.0),
        );
    }
    // MaiProc : fil utilisateur qui prend le cœur → sa pile noyau (RSP0, `syscall`), son espace
    // d'adresses (PCID) ; état vectoriel échangé si l'une des deux tâches est utilisateur.
    #[cfg(target_arch = "x86_64")]
    {
        let prev_user = prev.user.get();
        let next_user = next.user.get();
        if let Some(u) = next_user {
            uentry::set_kernel_stack(u.kstack_top);
            memory::paging::address_space::switch_to(&u.space.lock());
        }
        if prev_user.is_some() || next_user.is_some() {
            // SAFETY : interruptions coupées, sur le cœur qui quitte `prev` pour `next`.
            unsafe { uentry::switch_fpu(prev_user.map(|u| &u.fpu), next_user.map(|u| &u.fpu)) };
        }
    }
    let old_sp_ptr = &prev.saved_sp as *const usize as *mut usize;
    let new_sp_val = next.saved_sp;
    // Les références vivent dans les variables par cœur, pas sur la pile de `prev` : une tâche
    // morte qui ne reprendra jamais ne garde donc rien en vie.
    CURRENT_TASK.set(Some(next));
    SWITCH_PREV.set(Some(prev));
    unsafe {
        context_switch::context_switch(old_sp_ptr, new_sp_val);
    }
    finish_task_switch()
}

/// Seconde moitié du changement de contexte, exécutée par la tâche qui arrive sur le CPU :
/// libère la tâche précédente pour les autres cœurs.
pub fn finish_task_switch() -> Option<TaskRef> {
    let prev = SWITCH_PREV.replace(None);
    if let Some(p) = &prev {
        p.running_on_cpu.store(Option::<CpuId>::None.into());
    }
    prev
}

/// Réveille les tâches bloquées dans `join()` sur `task` (à appeler après le passage à `Exited`).
pub fn notify_exit(task: &TaskRef) {
    let waiters = core::mem::take(&mut *task.exit_waiters.lock());
    for w in waiters {
        let _ = TaskRef(w).unblock();
    }
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
