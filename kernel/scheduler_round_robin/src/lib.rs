//! This crate picks the next task in round-robin fashion.
//! Each time the task at the front of the queue is picked.
//! This task is then moved to the back of the queue.

#![no_std]

extern crate alloc;
extern crate cpu;
use alloc::{boxed::Box, collections::VecDeque, vec::Vec};
use task::TaskRef;

pub struct Scheduler {
    idle_task: TaskRef,
    queue: VecDeque<TaskRef>,
}

impl Scheduler {
    pub const fn new(idle_task: TaskRef) -> Self {
        Self {
            idle_task,
            queue: VecDeque::new(),
        }
    }
}

impl task::scheduler::Scheduler for Scheduler {
    fn next(&mut self) -> TaskRef {
        let current_id = task::get_my_current_task_id();

        if let Some((task_index, _)) = self
            .queue
            .iter()
            .enumerate()
            .find(|(_, task)| task.is_runnable() && task.id != current_id)
        {
            let task = self.queue.swap_remove_front(task_index).unwrap();
            self.queue.push_back(task.clone());
            task
        } else {
            self.idle_task.clone()
        }
    }

    fn busyness(&self) -> usize {
        self.queue.len()
    }

    fn add(&mut self, task: TaskRef) {
        // Prevent duplicate scheduling: if this task is already in the queue,
        // don't add it again. Double-queueing causes two CPUs to run the same
        // task on the same stack, leading to catastrophic corruption.
        if self.queue.iter().any(|t| t.id == task.id) {
            return;
        }
        self.queue.push_back(task);
    }

    fn remove(&mut self, task: &TaskRef) -> bool {
        let old_len = self.queue.len();
        self.queue.retain(|t| t != task);
        self.queue.len() != old_len
    }

    fn as_priority_scheduler(&mut self) -> Option<&mut dyn task::scheduler::PriorityScheduler> {
        None
    }

    fn drain(&mut self) -> Box<dyn Iterator<Item = TaskRef> + '_> {
        Box::new(self.queue.drain(..))
    }

    fn tasks(&self) -> Vec<TaskRef> {
        self.queue.iter().cloned().collect()
    }
}
