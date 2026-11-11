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
    /// Attend la fin de la tâche **en dormant** : l'appelant se bloque et s'inscrit dans
    /// `exit_waiters` ; la sortie de la tâche le réveille (`notify_exit`). L'ancienne version
    /// tournait en boucle sur `schedule()` et consommait un cœur entier (40 % mesurés pour `run`
    /// pendant Doom).
    pub fn join(&self) -> Result<ExitValue, &'static str> {
        loop {
            if let RunState::Exited(exit_val) = self.0.runstate.load() {
                return Ok(exit_val);
            }
            let Some(me) = crate::get_my_current_task() else {
                crate::scheduler::schedule();
                continue;
            };
            if Arc::ptr_eq(&me.0, &self.0 .0) {
                return Err("a task cannot join itself");
            }
            {
                let mut waiters = self.0.exit_waiters.lock();
                // Revérifié sous le verrou : `notify_exit` pose `Exited` avant de le prendre.
                if matches!(self.0.runstate.load(), RunState::Exited(_)) {
                    continue;
                }
                if me.block().is_err() {
                    drop(waiters);
                    crate::scheduler::schedule();
                    continue;
                }
                waiters.push(me.0.clone());
            }
            crate::scheduler::schedule();
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
        let r = match self.0.runstate.compare_exchange(RunState::Runnable, RunState::Exited(ExitValue::Completed(0))) {
            Ok(_) => Ok(()),
            // Also accept Blocked (e.g. bootstrap task)
            Err(RunState::Blocked) => {
                self.0.runstate.store(RunState::Exited(ExitValue::Completed(0)));
                Ok(())
            }
            Err(s) => Err(s),
        };
        crate::notify_exit(&self.0);
        r
    }

    /// Marks this task as killed with the given reason.
    pub fn mark_as_killed(&self, kill_reason: KillReason) -> Result<(), RunState> {
        let r = match self.0.runstate.compare_exchange(RunState::Runnable, RunState::Exited(ExitValue::Killed(kill_reason))) {
            Ok(_) => Ok(()),
            Err(RunState::Blocked) => {
                self.0.runstate.store(RunState::Exited(ExitValue::Killed(kill_reason)));
                Ok(())
            }
            Err(s) => Err(s),
        };
        crate::notify_exit(&self.0);
        r
    }

    /// Called immediately after a new task is first switched to.
    /// In the full Theseus this recovers the preemption guard passed from the
    /// previous task; in mai_os we simply hold preemption so it is released
    /// when the returned guard is dropped.
    /// Première exécution d'une tâche neuve : termine le changement de contexte qui l'a élue.
    pub fn post_context_switch_action(&self) -> PreemptionGuard {
        drop(crate::finish_task_switch());
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
        // Références attendues : TASK_LIST, ExitableTaskRef, CURRENT_TASK, et celle de la file
        // d'ordonnancement tant que la tâche y est. Au-delà, quelqu'un la garde (JoinableTaskRef).
        let in_rq = self.0.sched.snapshot().on_rq as usize;
        if Arc::strong_count(&self.0.0) <= 3 + in_rq {
            // La référence retirée est relâchée hors du verrou (interruptions coupées).
            let removed = crate::TASK_LIST.lock().remove(&id);
            drop(removed);
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
