//! MHC GPU Task — Compute dispatches as first-class schedulable units.
//!
//! ## Research basis
//!
//! - **HEXO** (Ganguly et al., ASPLOS 2023): heterogeneous-aware OS scheduling
//!   that models GPU work items alongside CPU tasks in a unified scheduler.
//! - **Harmonize** (Arafa et al., MICRO 2021): unified CPU+GPU scheduling
//!   with task-graph awareness.
//!
//! ## Design
//!
//! Each GPU compute dispatch becomes a `GpuTask` that is visible to MKS.
//! The key insight is that GPU tasks have dependencies (on other GPU tasks
//! or CPU tasks) and deadlines (derived from application requirements).
//! By making these visible to the kernel scheduler, we enable:
//!
//! - Priority inversion avoidance across CPU↔GPU boundaries
//! - Deadline propagation from CPU deadline tasks to their GPU work
//! - Fair GPU sharing using EEVDF virtual runtime
//! - Workload-aware CPU scheduling (e.g., don't schedule a CPU task
//!   that's waiting for GPU work to complete)

#![allow(dead_code)]

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;

use crate::device::BufferBinding;
use crate::fence::FenceId;
use crate::queue::GpuPriority;
use crate::shader::ShaderModule;

// ---------------------------------------------------------------------------
// GPU task state
// ---------------------------------------------------------------------------

/// Lifecycle state of a GPU task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuTaskState {
    /// Waiting for dependencies to complete.
    Pending,
    /// All dependencies satisfied, eligible for dispatch.
    Ready,
    /// Submitted to a hardware queue.
    Dispatched,
    /// Actively executing on the GPU.
    Running,
    /// Preempted by higher-priority work (GPU supports preemption).
    Preempted,
    /// Execution completed successfully.
    Completed,
    /// Execution failed.
    Failed,
}

// ---------------------------------------------------------------------------
// GPU task
// ---------------------------------------------------------------------------

/// A GPU compute dispatch modeled as a schedulable unit.
///
/// Each `GpuTask` has:
/// - A shader to execute with workgroup dimensions and buffer bindings
/// - Priority and optional deadline for scheduling
/// - Dependency tracking via fence IDs
/// - A completion fence for downstream synchronization
///
/// The MKS integration (when `mks_integration` feature is enabled) creates
/// a shadow `TaskRef` with `SchedClass::Compute` for each GpuTask, enabling
/// the MKS EEVDF algorithm to reason about GPU work alongside CPU work.
pub struct GpuTask {
    /// Unique task identifier.
    pub id: u64,

    // -- Shader dispatch parameters -----------------------------------------

    /// The compute shader to execute.
    pub shader: Arc<ShaderModule>,
    /// Number of workgroups [x, y, z].
    pub workgroups: [u32; 3],
    /// Buffer bindings for this dispatch.
    pub bindings: Vec<BufferBinding>,

    // -- Scheduling parameters ----------------------------------------------

    /// Priority level for the software queue.
    pub priority: GpuPriority,
    /// Optional absolute deadline [ns since boot].
    /// If set, the scheduler treats this as a deadline-class GPU task.
    pub deadline_ns: Option<u64>,
    /// Target GPU device ID.
    pub device_id: usize,

    // -- Dependency tracking ------------------------------------------------

    /// Fences that must be signaled before this task can execute.
    pub dependencies: Vec<FenceId>,
    /// Fence signaled when this task completes.
    pub completion_fence: FenceId,

    // -- State --------------------------------------------------------------

    /// Current lifecycle state.
    pub state: Mutex<GpuTaskState>,

    // -- EEVDF fields (for heterogeneous scheduling) ------------------------

    /// Virtual runtime: GPU-cycles consumed, normalized by compute weight.
    /// Analogous to MKS's `SchedMeta.vruntime` but measured in GPU time.
    pub vruntime: AtomicU64,
    /// Virtual deadline: derived from priority and requested GPU time.
    pub vdeadline: AtomicU64,
}

impl GpuTask {
    /// Create a new GPU task.
    pub fn new(
        id: u64,
        shader: Arc<ShaderModule>,
        workgroups: [u32; 3],
        bindings: Vec<BufferBinding>,
        priority: GpuPriority,
        device_id: usize,
    ) -> Self {
        GpuTask {
            id,
            shader,
            workgroups,
            bindings,
            priority,
            deadline_ns: None,
            device_id,
            dependencies: Vec::new(),
            completion_fence: FenceId::NONE,
            state: Mutex::new(GpuTaskState::Pending),
            vruntime: AtomicU64::new(0),
            vdeadline: AtomicU64::new(0),
        }
    }

    /// Add a dependency fence (this task waits for `fence` before executing).
    pub fn depends_on(&mut self, fence: FenceId) {
        if !fence.is_none() {
            self.dependencies.push(fence);
        }
    }

    /// Set a hard deadline for this GPU task.
    pub fn set_deadline(&mut self, deadline_ns: u64) {
        self.deadline_ns = Some(deadline_ns);
    }

    /// Check whether all dependencies are satisfied.
    pub fn are_dependencies_met(&self, poll_fn: &dyn Fn(FenceId) -> bool) -> bool {
        self.dependencies.iter().all(|f| poll_fn(*f))
    }

    /// Transition to a new state. Returns the previous state.
    pub fn transition(&self, new_state: GpuTaskState) -> GpuTaskState {
        let mut state = self.state.lock();
        let old = *state;
        *state = new_state;
        old
    }

    /// Get the current state.
    pub fn current_state(&self) -> GpuTaskState {
        *self.state.lock()
    }

    /// Whether this task has completed (successfully or with failure).
    pub fn is_terminal(&self) -> bool {
        matches!(self.current_state(),
            GpuTaskState::Completed | GpuTaskState::Failed)
    }

    /// Estimated compute cost (workgroups × invocations).
    /// Used by the scheduler for load balancing.
    pub fn estimated_cost(&self) -> u64 {
        let [x, y, z] = self.workgroups;
        (x as u64) * (y as u64) * (z as u64)
    }
}

// ---------------------------------------------------------------------------
// GPU task ID generator
// ---------------------------------------------------------------------------

static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);

/// Allocate a globally unique GPU task ID.
pub fn alloc_task_id() -> u64 {
    NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed)
}
