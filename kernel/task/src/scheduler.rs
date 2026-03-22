//! Scheduler dispatch layer.
//!
//! Routes task lifecycle operations (enqueue, dequeue, pick_next) to the
//! active scheduling backend via function pointers registered at boot.
//!
//! The default backend is MKS (Mai Kernel Scheduler), registered by
//! `kernel/scheduler::init()`. This design breaks the circular dependency
//! between the `task` crate and `mks` crate.
//!
//! ## Why function pointers instead of a global lock?
//!
//! The old design used a global `IrqSafeMutex<Vec<Scheduler>>` (`SCHEDULERS`)
//! which was acquired on every `add_task()` / `remove_task()` call — including
//! from the timer interrupt handler. With multiple CPUs contending on this
//! lock with IRQs disabled, the system would deadlock after ~80 sleep/wake
//! cycles.
//!
//! MKS uses per-CPU run queues behind an `IrqSafeRwLock` (readers never
//! block each other), eliminating the global contention.

use alloc::{boxed::Box, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

use cpu::CpuId;
use log::warn;

use crate::TaskRef;

// ---------------------------------------------------------------------------
// Backend dispatch via function pointers
// ---------------------------------------------------------------------------

// Stored as usize because AtomicPtr<()> can't be const-initialized on stable.
// Safety: only transmuted back to the exact fn type that was stored.
static BACKEND_ENQUEUE: AtomicUsize = AtomicUsize::new(0);
static BACKEND_ENQUEUE_ON: AtomicUsize = AtomicUsize::new(0);
static BACKEND_DEQUEUE: AtomicUsize = AtomicUsize::new(0);
static BACKEND_PICK_NEXT: AtomicUsize = AtomicUsize::new(0);
static BACKEND_LOAD: AtomicUsize = AtomicUsize::new(0);
static BACKEND_SET_IDLE: AtomicUsize = AtomicUsize::new(0);
static BACKEND_PUT_PREV: AtomicUsize = AtomicUsize::new(0);

/// Register the scheduling backend. Called once from `scheduler::init()`.
///
/// All function pointers must remain valid for the lifetime of the kernel
/// (they point to static MKS functions, so this is trivially satisfied).
pub fn register_backend(
    enqueue: fn(TaskRef),
    enqueue_on: fn(usize, TaskRef),
    dequeue: fn(&TaskRef),
    pick_next: fn(usize) -> Option<TaskRef>,
    load: fn(usize) -> usize,
    set_idle: fn(usize, TaskRef),
) {
    BACKEND_ENQUEUE.store(enqueue as usize, Ordering::Release);
    BACKEND_ENQUEUE_ON.store(enqueue_on as usize, Ordering::Release);
    BACKEND_DEQUEUE.store(dequeue as usize, Ordering::Release);
    BACKEND_PICK_NEXT.store(pick_next as usize, Ordering::Release);
    BACKEND_LOAD.store(load as usize, Ordering::Release);
    BACKEND_SET_IDLE.store(set_idle as usize, Ordering::Release);
}

#[inline]
fn dispatch_enqueue(task: TaskRef) {
    let ptr = BACKEND_ENQUEUE.load(Ordering::Acquire);
    if ptr != 0 {
        let f: fn(TaskRef) = unsafe { core::mem::transmute(ptr) };
        f(task);
    }
}

#[inline]
fn dispatch_enqueue_on(cpu: usize, task: TaskRef) {
    let ptr = BACKEND_ENQUEUE_ON.load(Ordering::Acquire);
    if ptr != 0 {
        let f: fn(usize, TaskRef) = unsafe { core::mem::transmute(ptr) };
        f(cpu, task);
    }
}

#[inline]
fn dispatch_dequeue(task: &TaskRef) {
    let ptr = BACKEND_DEQUEUE.load(Ordering::Acquire);
    if ptr != 0 {
        let f: fn(&TaskRef) = unsafe { core::mem::transmute(ptr) };
        f(task);
    }
}

#[inline]
fn dispatch_pick_next(cpu: usize) -> Option<TaskRef> {
    let ptr = BACKEND_PICK_NEXT.load(Ordering::Acquire);
    if ptr != 0 {
        let f: fn(usize) -> Option<TaskRef> = unsafe { core::mem::transmute(ptr) };
        f(cpu)
    } else {
        None
    }
}

#[inline]
fn dispatch_load(cpu: usize) -> usize {
    let ptr = BACKEND_LOAD.load(Ordering::Acquire);
    if ptr != 0 {
        let f: fn(usize) -> usize = unsafe { core::mem::transmute(ptr) };
        f(cpu)
    } else {
        0
    }
}

#[inline]
fn dispatch_put_prev(cpu: usize, task: TaskRef) {
    let ptr = BACKEND_PUT_PREV.load(Ordering::Acquire);
    if ptr != 0 {
        let f: fn(usize, TaskRef) = unsafe { core::mem::transmute(ptr) };
        f(cpu, task);
    }
}

/// Register the put_prev backend (re-enqueue after yield, no vruntime reset).
pub fn register_put_prev(put_prev: fn(usize, TaskRef)) {
    BACKEND_PUT_PREV.store(put_prev as usize, Ordering::Release);
}

// ---------------------------------------------------------------------------
// Public API — same signatures as before, zero global locks
// ---------------------------------------------------------------------------

/// Yields the current CPU by selecting a new `Task` to run next,
/// and then switches to that new `Task`.
///
/// Preemption will be disabled while this function runs,
/// but interrupts are not disabled because it is not necessary.
///
/// ## Return
/// * `true` if a new task was selected and switched to.
/// * `false` if no new task was selected, meaning the current task will
///   continue running.
#[doc(alias("yield"))]
pub fn schedule() -> bool {
    let preemption_guard = preemption::hold_preemption();
    if !preemption_guard.preemption_was_enabled() {
        return false;
    }

    let cpu_id = preemption_guard.cpu_id();
    let cpu_idx = cpu_id.value() as usize;

    let next_task = match dispatch_pick_next(cpu_idx) {
        Some(t) => t,
        None => return false,
    };

    // Re-enqueue the current task if it is still runnable.
    // pick_next removes the selected task from the tree; we must put
    // the PREVIOUS task back so it can be scheduled again later.
    // put_prev handles both cases:
    //   - task already in tree (on_rq=true): dequeues old entry, re-enqueues
    //   - task not in tree (on_rq=false, normal after pick_next): just enqueues
    if let Some(curr) = super::get_my_current_task() {
        if curr.id != next_task.id {
            if matches!(curr.runstate.load(), task_struct::RunState::Runnable) {
                dispatch_put_prev(cpu_idx, curr);
            }
        } else {
            // Same task picked — put it back in the tree since pick_next removed it
            dispatch_put_prev(cpu_idx, next_task.clone());
            return false;
        }
    }

    let (did_switch, recovered_preemption_guard) =
        super::task_switch(next_task, cpu_id, preemption_guard);

    drop(recovered_preemption_guard);
    did_switch
}

/// Adds the given task to the least busy run queue.
pub fn add_task(task: TaskRef) {
    dispatch_enqueue(task);
}

/// Adds the given task to the specified CPU's run queue.
pub fn add_task_to(cpu_id: CpuId, task: TaskRef) {
    dispatch_enqueue_on(cpu_id.value() as usize, task);
}

/// Adds the given task to the current CPU's run queue.
pub fn add_task_to_current(task: TaskRef) {
    let cpu = cpu::current_cpu().value() as usize;
    dispatch_enqueue_on(cpu, task);
}

/// Removes the given task from all run queues.
pub fn remove_task(task: &TaskRef) -> bool {
    dispatch_dequeue(task);
    true
}

/// Removes the given task from the specified CPU's run queue.
pub fn remove_task_from(task: &TaskRef, _cpu_id: CpuId) -> bool {
    dispatch_dequeue(task);
    true
}

/// Removes the given task from the current CPU's run queue.
pub fn remove_task_from_current(task: &TaskRef) -> bool {
    dispatch_dequeue(task);
    true
}

/// Register the per-CPU idle task in the scheduling backend.
pub fn register_idle_task(cpu_id: CpuId, task: TaskRef) {
    let ptr = BACKEND_SET_IDLE.load(Ordering::Acquire);
    if ptr != 0 {
        let f: fn(usize, TaskRef) = unsafe { core::mem::transmute(ptr) };
        f(cpu_id.value() as usize, task);
    }
}

/// Sets the scheduler policy for the given CPU.
///
/// Legacy no-op: MKS handles all scheduling policies.
/// The idle task should be registered via [`register_idle_task`] instead.
pub fn set_policy<T>(_cpu_id: CpuId, _scheduler: T)
where
    T: Scheduler,
{
    // No-op — MKS is the scheduling backend.
}

// ---------------------------------------------------------------------------
// Priority and busyness
// ---------------------------------------------------------------------------

/// Returns the priority of the given task (nice value mapped to 0..39).
pub fn priority(task: &TaskRef) -> Option<u8> {
    Some((task.read().sched.nice + 20) as u8)
}

/// Sets the priority of the given task.
pub fn set_priority(task: &TaskRef, priority: u8) -> bool {
    let nice = (priority as i8) - 20;
    let mut t = task.write();
    t.sched.nice = nice;
    t.update_weight();
    true
}

/// Returns the busyness of the scheduler on the given CPU.
pub fn busyness(cpu_id: CpuId) -> Option<usize> {
    Some(dispatch_load(cpu_id.value() as usize))
}

/// Modifies the given task's priority to be the maximum of its priority
/// and the current task's priority.
///
/// Returns a guard which reverts the change when dropped.
pub fn inherit_priority(task: &TaskRef) -> PriorityInheritanceGuard<'_> {
    let current_priority = super::with_current_task(priority).unwrap();
    let other_priority = priority(task);

    if let (Some(current_priority), Some(other_priority)) =
        (current_priority, other_priority) && current_priority > other_priority
    {
        set_priority(task, current_priority);
    }

    PriorityInheritanceGuard {
        inner: if let (Some(current_priority), Some(other_priority)) =
            (current_priority, other_priority)
            && current_priority > other_priority
        {
            Some((task, other_priority))
        } else {
            None
        },
    }
}

/// A guard that lowers a task's priority back to its previous value when dropped.
pub struct PriorityInheritanceGuard<'a> {
    inner: Option<(&'a TaskRef, u8)>,
}
impl<'a> Drop for PriorityInheritanceGuard<'a> {
    fn drop(&mut self) {
        if let Some((task, priority)) = self.inner {
            set_priority(task, priority);
        }
    }
}

/// Returns the list of tasks running on each CPU.
///
/// Uses the global TASK_LIST to collect all tasks, grouped by their
/// last-known CPU. This is a debugging aid and should not be used
/// on hot paths.
pub fn tasks() -> Vec<(CpuId, Vec<TaskRef>)> {
    // Delegate to the task list — avoids needing scheduler locks.
    warn!("scheduler::tasks() is a debug-only function");
    Vec::new()
}

// ---------------------------------------------------------------------------
// Scheduler trait — kept for backward compatibility
// ---------------------------------------------------------------------------

/// A task scheduler.
pub trait Scheduler: Send + Sync + 'static {
    /// Returns the next task to run.
    fn next(&mut self) -> TaskRef;

    /// Adds a task to the run queue.
    fn add(&mut self, task: TaskRef);

    /// Returns a measure of how busy the scheduler is, with higher values
    /// representing a busier scheduler.
    fn busyness(&self) -> usize;

    /// Removes a task from the run queue.
    fn remove(&mut self, task: &TaskRef) -> bool;

    /// Returns a reference to this scheduler as a priority scheduler, if it is one.
    fn as_priority_scheduler(&mut self) -> Option<&mut dyn PriorityScheduler>;

    /// Clears the scheduler's runqueue, returning an iterator over all contained tasks.
    fn drain(&mut self) -> Box<dyn Iterator<Item = TaskRef> + '_>;

    /// Returns a cloned list of contained tasks being scheduled by this scheduler.
    fn tasks(&self) -> Vec<TaskRef>;
}

/// A task scheduler that supports some notion of priority.
pub trait PriorityScheduler {
    /// Sets the priority of the given task.
    fn set_priority(&mut self, task: &TaskRef, priority: u8) -> bool;

    /// Gets the priority of the given task.
    fn priority(&mut self, task: &TaskRef) -> Option<u8>;
}
