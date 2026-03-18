//! MHC — Mai Heterogeneous Compute
//!
//! A unified heterogeneous compute engine for MaiOS that treats CPU cores,
//! GPU shader units, and future accelerators (NPUs, FPGAs) as a single pool
//! of schedulable compute resources.
//!
//! # Architecture
//!
//! ```text
//!  ┌───────────────────────────────────────────────────┐
//!  │              Application Layer                     │
//!  │       mhc::dispatch()   mhc::submit()             │
//!  ├───────────────────────────────────────────────────┤
//!  │  ┌──────────┐  ┌──────────┐  ┌──────────────┐    │
//!  │  │ GpuTask  │  │ CmdBuf   │  │ Shader Cache │    │
//!  │  └────┬─────┘  └────┬─────┘  └──────────────┘    │
//!  │       │              │                             │
//!  │  ┌────▼──────────────▼────────────────────────┐   │
//!  │  │  HeteroScheduler (EEVDF for GPU)           │   │
//!  │  │  Per-GPU run queues + MKS plugin           │   │
//!  │  └────┬───────────────────────────────────────┘   │
//!  │       │                                            │
//!  │  ┌────▼─────────┐  ┌─────────────────┐           │
//!  │  │ GpuDevice    │  │ Memory Manager  │           │
//!  │  │ (trait)      │  │ (unified addr)  │           │
//!  │  └────┬─────────┘  └─────────────────┘           │
//!  │       │                                            │
//!  │  ┌────▼────────────┐                              │
//!  │  │ Drivers         │                              │
//!  │  │ ├ software.rs   │  ← CPU fallback              │
//!  │  │ └ virtio_gpu.rs │  ← QEMU primary              │
//!  │  └─────────────────┘                              │
//!  └───────────────────────────────────────────────────┘
//! ```
//!
//! # Research basis
//!
//! | Concept                     | Paper / Source                              |
//! |-----------------------------|--------------------------------------------|
//! | Unified CPU+GPU scheduling  | HEXO (ASPLOS 2023), Harmonize (MICRO 2021) |
//! | Software GPU scheduling     | TimeGraph (ATC 2011), Gdev (ATC 2012)      |
//! | Persistent GPU contexts     | Persistent Threads (ISC 2012)              |
//! | EEVDF for GPU fairness      | Linux 6.6 EEVDF (extended to GPU vruntime) |
//! | ghOSt-style delegation      | ghOSt (SOSP 2021) — MKS already supports   |
//! | Unified virtual memory      | NVIDIA UVM, AMD HSA, VAST (ISCA 2020)      |
//! | Timeline semaphores         | Vulkan 1.2 specification                   |
//!
//! # Usage
//!
//! ```rust,no_run
//! // Initialize MHC (call once during boot)
//! mhc::init();
//!
//! // Allocate GPU memory
//! let buf = mhc::alloc(1024, GpuMemFlags::default()).unwrap();
//!
//! // Submit compute work
//! let mut cmds = CommandBuffer::new();
//! cmds.fill(buf.gpu_addr, 1024, 0xDEADBEEF);
//! cmds.finish();
//!
//! let fence = mhc::submit(&cmds, GpuPriority::Normal).unwrap();
//! mhc::wait(fence).unwrap();
//!
//! // Free memory
//! mhc::free(buf).unwrap();
//! ```

#![no_std]

extern crate alloc;
#[macro_use]
extern crate log;
extern crate spin;
extern crate bitflags;

pub mod device;
pub mod memory;
pub mod command;
pub mod fence;
pub mod queue;
pub mod task;
pub mod scheduler;
pub mod shader;
pub mod plugin;
pub mod drivers;

use alloc::sync::Arc;
use spin::Once;

use device::{GpuDevice, GpuError, QueueHandle, QueueKind};
use command::CommandBuffer;
use fence::FenceId;
use memory::{GpuAllocation, GpuMemFlags};
use queue::GpuPriority;
use scheduler::PerGpuRunQueue;

// ---------------------------------------------------------------------------
// Global MHC state
// ---------------------------------------------------------------------------

/// Global MHC instance — initialized once by `init()`.
struct MhcState {
    /// Primary device used for the convenience API.
    primary_device: Arc<dyn GpuDevice>,
    /// Default queue on the primary device.
    default_queue: QueueHandle,
    /// Per-GPU scheduler run queues.
    gpu_run_queues: spin::Mutex<alloc::vec::Vec<PerGpuRunQueue>>,
}

static MHC: Once<MhcState> = Once::new();

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

/// Initialize the MHC subsystem.
///
/// This function:
/// 1. Registers the software GPU driver (always available)
/// 2. Probes for VirtIO-GPU (if `virtio` feature is enabled)
/// 3. Sets up the default command queue on the primary device
/// 4. Initializes the GPU scheduler run queues
///
/// Must be called once during kernel boot (typically from `captain`).
pub fn init() -> Result<(), &'static str> {
    // Step 1: Always register the software GPU driver
    let sw_id = drivers::software::init();

    // Step 2: Try to initialize VirtIO-GPU
    #[cfg(feature = "virtio")]
    let _virtio_id = drivers::virtio_gpu::try_init();

    // Step 3: Get the primary device
    let primary = device::primary_device()
        .ok_or("MHC: no GPU device registered")?;

    // Step 4: Create a default queue
    let default_queue = primary.create_queue(QueueKind::Universal)
        .or_else(|_| primary.create_queue(QueueKind::Compute))
        .map_err(|_| "MHC: failed to create default queue")?;

    // Step 5: Initialize per-GPU run queues
    let num_devices = device::device_count();
    let mut run_queues = alloc::vec::Vec::with_capacity(num_devices);
    for i in 0..num_devices {
        run_queues.push(PerGpuRunQueue::new(i));
    }

    MHC.call_once(|| MhcState {
        primary_device: primary,
        default_queue,
        gpu_run_queues: spin::Mutex::new(run_queues),
    });

    info!("MHC: initialized with {} GPU device(s)", num_devices);
    for (id, name) in device::list_devices() {
        info!("  GPU {}: {}", id, name);
    }

    Ok(())
}

/// Check if MHC has been initialized.
pub fn is_initialized() -> bool {
    MHC.get().is_some()
}

// ---------------------------------------------------------------------------
// Convenience API (uses primary device)
// ---------------------------------------------------------------------------

/// Get a reference to the MHC state (panics if not initialized).
fn state() -> &'static MhcState {
    MHC.get().expect("MHC not initialized — call mhc::init() first")
}

/// Allocate GPU memory on the primary device.
pub fn alloc(size: usize, flags: GpuMemFlags) -> Result<GpuAllocation, GpuError> {
    state().primary_device.alloc(size, flags)
}

/// Free GPU memory.
pub fn free(alloc: GpuAllocation) -> Result<(), GpuError> {
    state().primary_device.free(alloc)
}

/// Submit a command buffer to the primary device's default queue.
pub fn submit(cmds: &CommandBuffer, _priority: GpuPriority) -> Result<FenceId, GpuError> {
    let s = state();
    s.primary_device.submit(s.default_queue, cmds)
}

/// Wait for a fence to be signaled (blocking).
pub fn wait(fence: FenceId) -> Result<(), GpuError> {
    state().primary_device.wait_fence(fence, u64::MAX)
}

/// Wait for a fence with a timeout (in nanoseconds).
pub fn wait_timeout(fence: FenceId, timeout_ns: u64) -> Result<(), GpuError> {
    state().primary_device.wait_fence(fence, timeout_ns)
}

/// Poll whether a fence has been signaled (non-blocking).
pub fn poll(fence: FenceId) -> bool {
    state().primary_device.poll_fence(fence)
}

/// Get the primary GPU device.
pub fn primary() -> Arc<dyn GpuDevice> {
    state().primary_device.clone()
}

/// Get a GPU device by ID.
pub fn gpu(id: usize) -> Option<Arc<dyn GpuDevice>> {
    device::get_device(id)
}

/// Number of registered GPU devices.
pub fn gpu_count() -> usize {
    device::device_count()
}
