//! MHC Scheduler Plugin — MKS integration for GPU-aware CPU scheduling.
//!
//! ## Research basis
//!
//! - **ghOSt** (Humphries et al., SOSP 2021): user-space scheduling delegation.
//!   MHC extends this concept to GPU scheduling: the MHC plugin intercepts
//!   MKS events to track CPU↔GPU dependencies.
//!
//! ## What this plugin does
//!
//! When registered with MKS, this plugin:
//! 1. **Blocks CPU tasks waiting for GPU work**: if a CPU task has a pending
//!    GPU dependency, the plugin returns `PluginAction::Block` to prevent
//!    it from spinning/wasting CPU cycles.
//! 2. **Wakes CPU tasks when GPU fences complete**: on each tick, the plugin
//!    polls GPU fences and wakes any CPU tasks whose GPU dependencies are
//!    now satisfied.
//! 3. **Propagates deadlines**: if a CPU deadline task submits GPU work,
//!    the GPU task inherits the CPU deadline minus a safety margin.

#![allow(dead_code)]

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::Mutex;

use crate::fence::FenceId;

// ---------------------------------------------------------------------------
// GPU dependency tracker
// ---------------------------------------------------------------------------

/// Tracks which CPU tasks are waiting for which GPU fences.
///
/// When a CPU task submits GPU work, it registers a dependency here.
/// The MHC plugin polls these fences on each scheduler tick and wakes
/// CPU tasks whose dependencies are satisfied.
pub struct GpuDependencyTracker {
    /// Map from fence ID to the CPU task IDs waiting on it.
    /// When the fence is signaled, these tasks should be unblocked.
    fence_to_waiters: BTreeMap<FenceId, Vec<usize>>,
    /// Map from CPU task ID to the fences it's waiting on.
    task_to_fences: BTreeMap<usize, Vec<FenceId>>,
}

impl GpuDependencyTracker {
    pub fn new() -> Self {
        GpuDependencyTracker {
            fence_to_waiters: BTreeMap::new(),
            task_to_fences: BTreeMap::new(),
        }
    }

    /// Register that CPU task `task_id` is waiting for `fence`.
    pub fn register_dependency(&mut self, task_id: usize, fence: FenceId) {
        self.fence_to_waiters
            .entry(fence)
            .or_default()
            .push(task_id);
        self.task_to_fences
            .entry(task_id)
            .or_default()
            .push(fence);
    }

    /// Remove all dependencies for a task (called on task exit).
    pub fn remove_task(&mut self, task_id: usize) {
        if let Some(fences) = self.task_to_fences.remove(&task_id) {
            for fence in fences {
                if let Some(waiters) = self.fence_to_waiters.get_mut(&fence) {
                    waiters.retain(|&id| id != task_id);
                    if waiters.is_empty() {
                        self.fence_to_waiters.remove(&fence);
                    }
                }
            }
        }
    }

    /// Check which tasks should be woken because their fence completed.
    /// Returns the list of task IDs to wake.
    pub fn drain_completed(
        &mut self,
        poll_fn: &dyn Fn(FenceId) -> bool,
    ) -> Vec<usize> {
        let mut to_wake = Vec::new();
        let mut completed_fences = Vec::new();

        for (&fence, waiters) in &self.fence_to_waiters {
            if poll_fn(fence) {
                completed_fences.push(fence);
                to_wake.extend(waiters.iter());
            }
        }

        // Clean up completed fences
        for fence in completed_fences {
            self.fence_to_waiters.remove(&fence);
        }

        // Clean up task-to-fence mappings for woken tasks
        for &task_id in &to_wake {
            if let Some(fences) = self.task_to_fences.get_mut(&task_id) {
                fences.retain(|f| !poll_fn(*f));
                if fences.is_empty() {
                    self.task_to_fences.remove(&task_id);
                }
            }
        }

        to_wake
    }

    /// Check if a specific task has unresolved GPU dependencies.
    pub fn has_pending_deps(&self, task_id: usize) -> bool {
        self.task_to_fences
            .get(&task_id)
            .map_or(false, |fences| !fences.is_empty())
    }

    /// Number of tracked dependencies.
    pub fn dependency_count(&self) -> usize {
        self.fence_to_waiters.len()
    }
}

impl Default for GpuDependencyTracker {
    fn default() -> Self { Self::new() }
}

// ---------------------------------------------------------------------------
// Global dependency tracker
// ---------------------------------------------------------------------------

static TRACKER: spin::Once<Mutex<GpuDependencyTracker>> = spin::Once::new();

/// Access the global GPU dependency tracker.
pub fn tracker() -> &'static Mutex<GpuDependencyTracker> {
    TRACKER.call_once(|| Mutex::new(GpuDependencyTracker::new()))
}

/// Register that a CPU task is waiting for a GPU fence.
pub fn register_gpu_dependency(task_id: usize, fence: FenceId) {
    tracker().lock().register_dependency(task_id, fence);
}

/// Remove all GPU dependencies for a task.
pub fn remove_task_deps(task_id: usize) {
    tracker().lock().remove_task(task_id);
}
