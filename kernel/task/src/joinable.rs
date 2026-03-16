//! Wrapper types for Joinable and Exitable tasks.

use alloc::boxed::Box;
use alloc::sync::Arc;
use crate::TaskRef;
use core::ops::Deref;
use task_struct::{KillReason, RunState, ExitValue, RestartInfo};
use cpu::CpuId;
use preemption::PreemptionGuard;
use stack::Stack;

/// A wrapper around `TaskRef` that allows a task to be joined (waited on).
#[derive(Debug, Clone)]
pub struct JoinableTaskRef(pub TaskRef);

impl Deref for JoinableTaskRef {
    type Target = TaskRef;
    fn deref(&self) -> &TaskRef { &self.0 }
}

impl JoinableTaskRef {
    /// Blocks the current task until this task exits, then returns its exit value.
    pub fn join(&self) -> Result<ExitValue, &'static str> {
        loop {
            match self.0.runstate.load() {
                RunState::Exited(exit_val) => return Ok(exit_val),
                _ => { crate::scheduler::schedule(); }
            }
        }
    }

    /// Returns true if the task has already exited.
    pub fn has_exited(&self) -> bool {
        matches!(self.0.runstate.load(), RunState::Exited(_))
    }
}

/// A wrapper around `TaskRef` that represents a task that can exit.
#[derive(Debug, Clone)]
pub struct ExitableTaskRef(pub TaskRef);

impl Deref for ExitableTaskRef {
    type Target = TaskRef;
    fn deref(&self) -> &TaskRef { &self.0 }
}

impl ExitableTaskRef {
    /// Returns an `ExitableTaskRef` and a cleanup function for the unwinder.
    /// Takes `current_task` by value (unwinder transfers ownership).
    pub fn obtain_for_unwinder(
        current_task: TaskRef,
    ) -> (ExitableTaskRef, fn(ExitableTaskRef, KillReason) -> !) {
        (ExitableTaskRef(current_task), task_cleanup_failure)
    }

    /// Marks this task as exited successfully.
    /// The exit value is boxed; we record it as Completed(0) since we can't
    /// downcast a generic Box<dyn Any> to isize without type info here.
    pub fn mark_as_exited(&self, _exit_value: Box<dyn core::any::Any + Send>) -> Result<(), RunState> {
        match self.0.runstate.compare_exchange(RunState::Runnable, RunState::Exited(ExitValue::Completed(0))) {
            Ok(_) => Ok(()),
            // Also accept Blocked (e.g. bootstrap task)
            Err(RunState::Blocked) => {
                self.0.runstate.store(RunState::Exited(ExitValue::Completed(0)));
                Ok(())
            }
            Err(s) => Err(s),
        }
    }

    /// Marks this task as killed with the given reason.
    pub fn mark_as_killed(&self, kill_reason: KillReason) -> Result<(), RunState> {
        match self.0.runstate.compare_exchange(RunState::Runnable, RunState::Exited(ExitValue::Killed(kill_reason))) {
            Ok(_) => Ok(()),
            Err(RunState::Blocked) => {
                self.0.runstate.store(RunState::Exited(ExitValue::Killed(kill_reason)));
                Ok(())
            }
            Err(s) => Err(s),
        }
    }

    /// Called immediately after a new task is first switched to.
    /// In the full Theseus this recovers the preemption guard passed from the
    /// previous task; in mai_os we simply hold preemption so it is released
    /// when the returned guard is dropped.
    pub fn post_context_switch_action(&self) -> PreemptionGuard {
        preemption::hold_preemption()
    }

    /// Provides access to the task's kernel stack.
    pub fn with_kstack<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Stack) -> R,
    {
        f(&self.0.kstack)
    }

    /// Provides access to the task's `RestartInfo`, if any.
    pub fn with_restart_info<F, R>(&self, f: F) -> R
    where
        F: FnOnce(Option<&RestartInfo>) -> R,
    {
        f(self.0.restart_info.as_ref())
    }

    /// Returns the CPU this task is pinned to, if any.
    pub fn pinned_cpu(&self) -> Option<CpuId> {
        Option::<CpuId>::from(self.0.pinned_core.load())
    }

    /// Removes this task from TASK_LIST if nothing else is holding a reference
    /// to it (i.e. it was never joined / orphaned).
    pub fn reap_if_orphaned(&self) {
        let id = self.0.id;
        // Arc strong count: 1 in TASK_LIST + 1 in ExitableTaskRef (+ possibly 1 in CURRENT_TASK)
        // If only these references exist the task is orphaned.
        if Arc::strong_count(&self.0.0) <= 3 {
            crate::TASK_LIST.lock().remove(&id);
        }
    }
}

/// The cleanup function invoked at the end of unwinding when a task has failed.
fn task_cleanup_failure(task: ExitableTaskRef, cause: KillReason) -> ! {
    // This function is invoked when a task's unwinding process has completed.
    // We mark the task as killed, which sets its RunState to Exited.
    // We ignore the result because we are killing the task regardless.
    let _ = task.mark_as_killed(cause);
    crate::scheduler::remove_task(&task.0);
    loop { core::hint::spin_loop(); }
}

/// Converts a bootstrap task into a `(JoinableTaskRef, ExitableTaskRef)` pair.
pub fn bootstrap_task_to_joinable(
    bootstrap_task: TaskRef,
) -> (JoinableTaskRef, ExitableTaskRef) {
    (JoinableTaskRef(bootstrap_task.clone()), ExitableTaskRef(bootstrap_task))
}
