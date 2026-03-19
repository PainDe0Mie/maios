//! This crate implements a token-based epoch scheduling policy.
//!
//! At the beginning of each scheduling epoch, a set of tokens is distributed
//! among all runnable tasks, based on their priority relative to all other
//! runnable tasks in the runqueue. The formula for this is:
//! ```ignore
//! tokens_assigned_to_task_i = ((priority_task_i + 1) / sum_of_(priority + 1)_all_runnable_tasks) * epoch_length;
//! ```
//! The `+1` offset ensures that tasks with priority 0 still receive tokens.
//!
//! * Each time a task is picked, its token count is decremented by 1.
//! * A task can only be selected for next execution if it has tokens remaining.
//! * When all tokens of all runnable tasks are exhausted, a new scheduling epoch begins.
//!
//! This epoch scheduler is also a priority-based scheduler, so it allows
//! getting and setting the priorities of each task.

#![no_std]

extern crate alloc;

use alloc::{boxed::Box, collections::VecDeque, vec::Vec};
use core::ops::{Deref, DerefMut};
use task::TaskRef;
use log::{error, info, debug};

const MAX_PRIORITY: u8 = 40;
const DEFAULT_PRIORITY: u8 = 20;
const INITIAL_TOKENS: usize = 10;

/// An instance of an epoch scheduler, typically one per CPU.
pub struct Scheduler {
    idle_task: TaskRef,
    queue: VecDeque<EpochTaskRef>,
}

impl Scheduler {
    /// Creates a new epoch scheduler instance with the given idle task.
    pub const fn new(idle_task: TaskRef) -> Self {
        Self {
            idle_task,
            queue: VecDeque::new(),
        }
    }

    /// Moves the `TaskRef` at the given `index` in this scheduler's runqueue
    /// to the end (back) of the runqueue.
    ///
    /// Sets the number of tokens for that task to the given `tokens`.
    ///
    /// Returns a cloned reference to the `TaskRef` at the given `index`.
    fn update_and_move_to_end(&mut self, index: usize, tokens: usize) -> Option<TaskRef> {
        let mut epoch_task_ref = self.queue.remove(index)?;
        epoch_task_ref.tokens_remaining = tokens;
        let task_ref = epoch_task_ref.task.clone();
        self.queue.push_back(epoch_task_ref);
        Some(task_ref)
    }

    fn try_next(&mut self) -> Option<TaskRef> {
        for (i, task) in self.queue.iter().enumerate() {
            info!("try_next: checking task {} (tokens={}) runnable={}", task.task.id, task.tokens_remaining, task.is_runnable());
            if task.is_runnable() && task.tokens_remaining > 0 {
                let new_tokens = task.tokens_remaining - 1;
                return self.update_and_move_to_end(i, new_tokens);
            }
        }
        None
    }

    fn assign_tokens(&mut self) {
        // Extract idle_task ref to avoid borrow conflicts when iterating queue.
        let idle_task = self.idle_task.clone();

        // Sum of (priority + 1) for all runnable, non-idle tasks.
        // The +1 ensures tasks with priority 0 still receive a nonzero allocation.
        let total_weight: usize = self.queue
            .iter()
            .filter(|t| t.is_runnable() && t.task != idle_task)
            .map(|t| (t.priority as usize).saturating_add(1))
            .sum();

        if total_weight == 0 {
            // No runnable non-idle tasks; nothing to assign.
            return;
        }

        // Each epoch lasts for at least 100 tokens. We scale up if total_weight
        // is larger to prevent low-priority tasks from being starved.
        let epoch: usize = total_weight.max(100);

        for t in self.queue.iter_mut() {
            if t.task == idle_task || !t.is_runnable() {
                t.tokens_remaining = 0;
                continue;
            }

            // Proportional allocation: tokens = epoch * weight / total_weight
            let weight = (t.priority as usize).saturating_add(1);
            t.tokens_remaining = epoch.saturating_mul(weight) / total_weight;
        }
    }
}

impl task::scheduler::Scheduler for Scheduler {
    fn next(&mut self) -> TaskRef {
        let task = self.try_next()
            .or_else(|| {
                self.assign_tokens();
                self.try_next()
            })
            .unwrap_or_else(|| self.idle_task.clone());
        info!("EpochScheduler::next: selected task {}", task.id);
        task
    }

    fn add(&mut self, task: TaskRef) {
        // Prevent duplicate scheduling
        if self.queue.iter().any(|t| t.task.id == task.id) {
            return;
        }
        info!("EpochScheduler::add: task {} ({})", task.id, task.name);
        self.queue.push_back(EpochTaskRef::new(task));
    }

    fn busyness(&self) -> usize {
        self.queue.len()
    }

    fn remove(&mut self, task: &TaskRef) -> bool {
        if let Some(index) = self.queue.iter().position(|t| t.task == *task) {
            self.queue.remove(index);
            true
        } else {
            false
        }
    }

    fn as_priority_scheduler(&mut self) -> Option<&mut dyn task::scheduler::PriorityScheduler> {
        Some(self)
    }

    fn drain(&mut self) -> Box<dyn Iterator<Item = TaskRef> + '_> {
        Box::new(self.queue.drain(..).map(|epoch_task| epoch_task.task))
    }

    fn tasks(&self) -> Vec<TaskRef> {
        self.queue
            .iter()
            .map(|epoch_task| epoch_task.task.clone())
            .collect()
    }
}

impl task::scheduler::PriorityScheduler for Scheduler {
    fn set_priority(&mut self, task: &TaskRef, priority: u8) -> bool {
        let priority = priority.min(MAX_PRIORITY);
        for epoch_task in self.queue.iter_mut() {
            if epoch_task.task == *task {
                epoch_task.priority = priority;
                return true;
            }
        }
        false
    }

    fn priority(&mut self, task: &TaskRef) -> Option<u8> {
        self.queue
            .iter()
            .find(|t| t.task == *task)
            .map(|t| t.priority)
    }
}

#[derive(Debug, Clone)]
struct EpochTaskRef {
    task: TaskRef,
    priority: u8,
    tokens_remaining: usize,
}

impl Deref for EpochTaskRef {
    type Target = TaskRef;

    fn deref(&self) -> &TaskRef {
        &self.task
    }
}

impl DerefMut for EpochTaskRef {
    fn deref_mut(&mut self) -> &mut TaskRef {
        &mut self.task
    }
}

impl EpochTaskRef {
    fn new(task: TaskRef) -> EpochTaskRef {
        EpochTaskRef {
            task,
            priority: DEFAULT_PRIORITY,
            tokens_remaining: INITIAL_TOKENS,
        }
    }
}
