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
extern crate pci;
extern crate memory as kernel_memory;

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

use crate::device::{GpuDevice, GpuError, QueueHandle, QueueKind};
use crate::command::CommandBuffer;
use crate::fence::FenceId;
use crate::memory::{GpuAllocation, GpuMemFlags};
use crate::queue::GpuPriority;
use crate::scheduler::PerGpuRunQueue;

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
    /// VirtIO-GPU display scanout resource (set up by `setup_display()`).
    display_scanout: spin::Mutex<Option<crate::drivers::virtio_gpu::VirtioScanout>>,
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
    // Step 1: Try VirtIO-GPU first so it becomes the primary device when present.
    let _virtio_id = drivers::virtio_gpu::try_init();

    // Step 2: Software GPU fallback — always available (CPU emulation).
    let _sw_id = drivers::software::init();

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
        display_scanout: spin::Mutex::new(None),
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

// ---------------------------------------------------------------------------
// VirtIO-GPU display pipeline
// ---------------------------------------------------------------------------

/// Set up the VirtIO-GPU display output.
///
/// Must be called after `init()`.  Allocates a BGRA8888 DMA backing buffer,
/// creates a VirtIO-GPU 2D resource, attaches it, and sets it as scanout 0.
///
/// `width` and `height` should match the desired display resolution.
pub fn setup_display(width: u32, height: u32) -> Result<(), &'static str> {
    let s = MHC.get().ok_or("MHC not initialized")?;
    let dev = crate::drivers::virtio_gpu::device()
        .ok_or("MHC: VirtIO-GPU device not found")?;
    let scanout = dev.setup_display_resource(width, height)?;
    *s.display_scanout.lock() = Some(scanout);
    info!("MHC: VirtIO-GPU display output configured ({}x{})", width, height);
    Ok(())
}

/// Flush `pixels` (BGRA8888 u32 values, `width * height` of them) to the VirtIO-GPU scanout.
///
/// Returns `true` if the flush succeeded, `false` if the display is not set up
/// or the VirtIO-GPU device is unavailable.
pub fn flush_display(pixels: &[u32], width: u32, height: u32) -> bool {
    let s = match MHC.get() {
        Some(s) => s,
        None    => return false,
    };
    let lk = s.display_scanout.lock();
    let scanout = match lk.as_ref() {
        Some(sc) => sc,
        None     => return false,
    };
    if scanout.width != width || scanout.height != height { return false; }
    let dev = match crate::drivers::virtio_gpu::device() {
        Some(d) => d,
        None    => return false,
    };
    dev.update_scanout(scanout, pixels).is_ok()
}

/// Returns `true` if the VirtIO-GPU display has been configured via `setup_display()`.
pub fn has_display() -> bool {
    MHC.get().map_or(false, |s| s.display_scanout.lock().is_some())
}
