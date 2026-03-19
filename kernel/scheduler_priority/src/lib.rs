//! This scheduler implements a priority algorithm.
//!
//! Tasks are stored in a `BinaryHeap` ordered by priority (higher = first).
//! Among tasks of equal priority, those that ran longest ago are preferred,
//! which prevents starvation of equal-priority tasks.
//!
//! ## Note on blocked tasks
//! Until only runnable tasks are stored in the run queue, `next()` must scan
//! through the heap to skip blocked tasks. This is O(n) in the worst case
//! but avoids repeated heap rebuilds.

#![no_std]

extern crate alloc;

use alloc::{boxed::Box, collections::BinaryHeap, vec::Vec};
use core::cmp::Ordering;

use task::TaskRef;
use time::Instant;

use log::{error, info, debug};

const DEFAULT_PRIORITY: u8 = 0;

pub struct Scheduler {
    idle_task: TaskRef,
    queue: BinaryHeap<PriorityTaskRef>,
}

impl Scheduler {
    pub fn new(idle_task: TaskRef) -> Self {
        Self {
            idle_task,
            queue: BinaryHeap::new(),
        }
    }
}

impl task::scheduler::Scheduler for Scheduler {
    fn next(&mut self) -> TaskRef {
        info!("PriorityScheduler::next: entered");
        // Collect all tasks; pick the best runnable one; re-insert everything else.
        //
        // This is O(n) and unavoidable until blocked tasks are excluded from the
        // run queue. We use `drain()` + re-insert rather than repeated `pop()`
        // so that we touch each element exactly once instead of O(n log n) pops.
        let mut all_tasks: Vec<PriorityTaskRef> = self.queue.drain().collect();

        // Find the index of the highest-priority runnable task.
        // `all_tasks` came out of a max-heap so it is in heap order (not sorted),
        // but we need a linear scan anyway because some tasks may be blocked.
        let current_id = task::get_my_current_task_id();
        let chosen_index = all_tasks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.task.is_runnable() && t.task.id != current_id) // ← ajoute ça
            .max_by(|(_, a), (_, b)| a.cmp(b))
            .map(|(i, _)| i);

        let chosen = if let Some(idx) = chosen_index {
            let mut task = all_tasks.swap_remove(idx);
            task.last_ran = time::now::<time::Monotonic>();
            let task_ref = task.task.clone();
            all_tasks.push(task);
            task_ref
        } else {
            self.idle_task.clone()
        };

        // Rebuild the heap from the remaining tasks.
        self.queue.extend(all_tasks);
        chosen
    }

    fn add(&mut self, task: TaskRef) {
        // Prevent duplicate scheduling
        if self.queue.iter().any(|t| t.task.id == task.id) {
            return;
        }
        // New tasks start with `last_ran = Instant::ZERO` so they are treated
        // as having waited the longest among tasks of the same priority.
        self.queue.push(PriorityTaskRef::new(task, DEFAULT_PRIORITY));
    }

    fn busyness(&self) -> usize {
        self.queue.len()
    }

    fn remove(&mut self, task: &TaskRef) -> bool {
        let old_len = self.queue.len();
        self.queue.retain(|t| t.task != *task);
        let removed = self.queue.len() != old_len;
        debug_assert!(
            old_len - self.queue.len() <= 1,
            "removed more than one task from the priority run queue"
        );
        removed
    }

    fn as_priority_scheduler(&mut self) -> Option<&mut dyn task::scheduler::PriorityScheduler> {
        Some(self)
    }

    fn drain(&mut self) -> Box<dyn Iterator<Item = TaskRef> + '_> {
        Box::new(self.queue.drain().map(|t| t.task))
    }

    fn tasks(&self) -> Vec<TaskRef> {
        self.queue.iter().map(|t| t.task.clone()).collect()
    }
}

impl task::scheduler::PriorityScheduler for Scheduler {
    fn set_priority(&mut self, task: &TaskRef, priority: u8) -> bool {
        // `BinaryHeap` has no `iter_mut`, so we must remove + re-insert to change priority.
        let old_len = self.queue.len();
        self.queue.retain(|t| t.task != *task);

        if self.queue.len() != old_len {
            debug_assert_eq!(self.queue.len() + 1, old_len);
            self.queue.push(PriorityTaskRef {
                task: task.clone(),
                priority,
                // Preserve "waited longest" semantics: reset to ZERO so the
                // re-inserted task is treated as if it just entered the queue.
                last_ran: Instant::ZERO,
            });
            true
        } else {
            false
        }
    }

    fn priority(&mut self, task: &TaskRef) -> Option<u8> {
        self.queue
            .iter()
            .find(|t| t.task == *task)
            .map(|t| t.priority)
    }
}

#[derive(Clone, Debug, Eq)]
struct PriorityTaskRef {
    task: TaskRef,
    priority: u8,
    /// The last time this task ran.
    ///
    /// Among tasks with equal priority, the one that ran longest ago wins,
    /// preventing starvation. New and re-prioritized tasks start at
    /// [`Instant::ZERO`] so they are always considered the "oldest".
    last_ran: Instant,
}

impl PriorityTaskRef {
    pub const fn new(task: TaskRef, priority: u8) -> Self {
        Self {
            task,
            priority,
            last_ran: Instant::ZERO,
        }
    }
}

impl PartialEq for PriorityTaskRef {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.last_ran == other.last_ran
    }
}

impl PartialOrd for PriorityTaskRef {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PriorityTaskRef {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.priority.cmp(&other.priority) {
            // Among equal-priority tasks, prefer the one that ran longest ago.
            Ordering::Equal => other.last_ran.cmp(&self.last_ran),
            ordering => ordering,
        }
    }
}
