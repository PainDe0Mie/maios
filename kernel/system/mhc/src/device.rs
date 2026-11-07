//! GPU device abstraction and global device registry.
//!
//! Every GPU backend (VirtIO-GPU, software emulator, future native drivers)
//! implements the [`GpuDevice`] trait. Devices self-register via
//! [`register_device`] during PCI probe or manual initialization.
//!
//! ## Research basis
//!
//! The trait design follows the "Gdev" kernel-level GPU abstraction
//! (Kato et al., ATC 2012) but extends it with:
//! - Timeline-semaphore fences (Vulkan 1.2)
//! - Unified memory mapping (AMD HSA / NVIDIA UVM)
//! - Heterogeneous scheduling hooks (HEXO, ASPLOS 2023)

#![allow(dead_code)]

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::{Mutex, Once};

use crate::command::CommandBuffer;
use crate::fence::FenceId;
use crate::memory::{GpuAddress, GpuAllocation, GpuMemFlags};
use crate::shader::ShaderModule;

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// GPU subsystem errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GpuError {
    /// Device not found or not initialized.
    DeviceNotFound,
    /// Out of device or host memory.
    OutOfMemory,
    /// Invalid parameter passed to a GPU operation.
    InvalidParameter(&'static str),
    /// Operation not supported by this device.
    Unsupported(&'static str),
    /// Fence wait timed out.
    Timeout,
    /// Device lost (fatal hardware error).
    DeviceLost,
    /// Shader compilation or validation failed.
    ShaderError(&'static str),
    /// Queue submission failed.
    SubmissionFailed,
    /// Internal driver error.
    DriverError(&'static str),
}

// ---------------------------------------------------------------------------
// Device capabilities
// ---------------------------------------------------------------------------

/// GPU vendor identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuVendor {
    /// VirtIO virtual GPU (QEMU/KVM).
    VirtIO,
    /// Software emulator (CPU fallback).
    Software,
    /// Intel integrated/discrete GPU.
    Intel,
    /// AMD/ATI GPU.
    Amd,
    /// NVIDIA GPU.
    Nvidia,
    /// Unknown vendor.
    Unknown(u16),
}

/// Supported shader bytecode formats.
bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct ShaderFormats: u32 {
        /// SPIR-V bytecode (Vulkan/OpenCL standard).
        const SPIRV    = 0x1;
        /// MHC intermediate representation (future).
        const MHC_IR   = 0x2;
        /// Pre-compiled native ISA.
        const NATIVE   = 0x4;
    }
}

/// Describes the capabilities of a GPU device.
#[derive(Clone, Debug)]
pub struct GpuCapabilities {
    /// Maximum workgroup dimensions [x, y, z].
    pub max_workgroup_size: [u32; 3],
    /// Maximum total invocations per workgroup.
    pub max_workgroup_invocations: u32,
    /// Maximum shared memory per workgroup (bytes).
    pub max_shared_memory: usize,
    /// Whether compute dispatch is supported.
    pub supports_compute: bool,
    /// Whether graphics rendering is supported.
    pub supports_graphics: bool,
    /// Whether CPU and GPU share a unified address space.
    pub supports_unified_memory: bool,
    /// Maximum number of concurrent command queues.
    pub max_queues: u32,
    /// Supported shader formats.
    pub shader_formats: ShaderFormats,
    /// Number of compute units / shader cores.
    pub compute_units: u32,
    /// Device-local memory size (bytes, 0 if unified-only).
    pub device_memory_bytes: u64,
}

impl Default for GpuCapabilities {
    fn default() -> Self {
        GpuCapabilities {
            max_workgroup_size: [256, 256, 64],
            max_workgroup_invocations: 256,
            max_shared_memory: 32768,
            supports_compute: true,
            supports_graphics: false,
            supports_unified_memory: true,
            max_queues: 4,
            shader_formats: ShaderFormats::SPIRV,
            compute_units: 1,
            device_memory_bytes: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Queue types
// ---------------------------------------------------------------------------

/// Identifies a command queue on a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct QueueHandle(pub u32);

/// The kind of work a queue can execute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueKind {
    /// General-purpose compute dispatch.
    Compute,
    /// Graphics rendering (draw calls, blits).
    Graphics,
    /// DMA transfer (copy, fill).
    Transfer,
    /// Can handle compute, graphics, and transfer.
    Universal,
}

/// Buffer binding for shader dispatch.
#[derive(Clone, Debug)]
pub struct BufferBinding {
    /// Binding slot index (matches shader layout).
    pub binding: u32,
    /// GPU-visible address of the buffer.
    pub address: GpuAddress,
    /// Size of the buffer in bytes.
    pub size: usize,
    /// Whether the shader writes to this buffer.
    pub writable: bool,
}

// ---------------------------------------------------------------------------
// GpuDevice trait
// ---------------------------------------------------------------------------

/// Trait implemented by all GPU backends.
///
/// Each method is documented with its expected complexity and latency.
/// Implementations must be `Send + Sync` because the MHC scheduler may
/// call device methods from any CPU's timer-tick context.
pub trait GpuDevice: Send + Sync + 'static {
    /// Human-readable device name (e.g., "VirtIO-GPU", "MHC Software GPU").
    fn name(&self) -> &str;

    /// GPU vendor.
    fn vendor(&self) -> GpuVendor;

    /// Device capabilities (cached, O(1)).
    fn capabilities(&self) -> &GpuCapabilities;

    // -- Queue management ---------------------------------------------------

    /// Create a new command queue of the specified kind.
    /// Returns a handle used to identify this queue in subsequent calls.
    fn create_queue(&self, kind: QueueKind) -> Result<QueueHandle, GpuError>;

    /// Destroy a previously created queue. Waits for all pending work.
    fn destroy_queue(&self, queue: QueueHandle) -> Result<(), GpuError>;

    /// Submit a command buffer to a queue for execution.
    /// Returns a fence ID that will be signaled on completion.
    fn submit(&self, queue: QueueHandle, cmds: &CommandBuffer) -> Result<FenceId, GpuError>;

    /// Block the calling thread until a fence is signaled or timeout expires.
    /// `timeout_ns = 0` means non-blocking poll.
    /// `timeout_ns = u64::MAX` means wait indefinitely.
    fn wait_fence(&self, fence: FenceId, timeout_ns: u64) -> Result<(), GpuError>;

    /// Poll whether a fence has been signaled (non-blocking).
    fn poll_fence(&self, fence: FenceId) -> bool;

    // -- Memory management --------------------------------------------------

    /// Allocate device-accessible memory.
    fn alloc(&self, size: usize, flags: GpuMemFlags) -> Result<GpuAllocation, GpuError>;

    /// Free a previously allocated GPU memory region.
    fn free(&self, alloc: GpuAllocation) -> Result<(), GpuError>;

    /// Map a host physical address range into GPU-visible address space.
    /// Requires IOMMU support on the device.
    fn map_host_memory(
        &self,
        host_phys: u64,
        size: usize,
        flags: GpuMemFlags,
    ) -> Result<GpuAddress, GpuError> {
        let _ = (host_phys, size, flags);
        Err(GpuError::Unsupported("map_host_memory not implemented"))
    }

    /// Unmap a previously mapped host memory region.
    fn unmap_host_memory(&self, addr: GpuAddress) -> Result<(), GpuError> {
        let _ = addr;
        Err(GpuError::Unsupported("unmap_host_memory not implemented"))
    }

    // -- Compute dispatch ---------------------------------------------------

    /// Dispatch a compute shader.
    ///
    /// This is a convenience method that encodes a single dispatch into
    /// a command buffer and submits it. For batched work, prefer building
    /// a `CommandBuffer` manually and calling `submit()`.
    fn dispatch_compute(
        &self,
        queue: QueueHandle,
        shader: &ShaderModule,
        workgroups: [u32; 3],
        bindings: &[BufferBinding],
    ) -> Result<FenceId, GpuError>;

    // -- Graphics (optional, for MGI integration) ---------------------------

    /// Blit a GPU buffer to the scanout/framebuffer.
    /// Default implementation returns `Unsupported`.
    fn blit_to_scanout(
        &self,
        _queue: QueueHandle,
        _src: GpuAddress,
        _src_stride: u32,
        _src_width: u32,
        _src_height: u32,
    ) -> Result<FenceId, GpuError> {
        Err(GpuError::Unsupported("blit_to_scanout not implemented"))
    }
}

// ---------------------------------------------------------------------------
// Global device registry
// ---------------------------------------------------------------------------

/// Entry in the device registry.
pub struct DeviceEntry {
    pub id: usize,
    pub device: Arc<dyn GpuDevice>,
}

/// Global registry of all GPU devices discovered in the system.
struct DeviceRegistry {
    devices: Vec<DeviceEntry>,
    next_id: usize,
}

impl DeviceRegistry {
    fn new() -> Self {
        DeviceRegistry {
            devices: Vec::new(),
            next_id: 0,
        }
    }
}

static REGISTRY: Once<Mutex<DeviceRegistry>> = Once::new();

fn registry() -> &'static Mutex<DeviceRegistry> {
    REGISTRY.call_once(|| Mutex::new(DeviceRegistry::new()))
}

/// Register a new GPU device. Returns the device ID.
pub fn register_device(device: Arc<dyn GpuDevice>) -> usize {
    let mut reg = registry().lock();
    let id = reg.next_id;
    log::info!("MHC: registered GPU device {} — '{}'", id, device.name());
    reg.devices.push(DeviceEntry { id, device });
    reg.next_id += 1;
    id
}

/// Get a device by its ID.
pub fn get_device(id: usize) -> Option<Arc<dyn GpuDevice>> {
    let reg = registry().lock();
    reg.devices.iter().find(|e| e.id == id).map(|e| e.device.clone())
}

/// Get the primary (first registered) GPU device.
pub fn primary_device() -> Option<Arc<dyn GpuDevice>> {
    let reg = registry().lock();
    reg.devices.first().map(|e| e.device.clone())
}

/// Returns the number of registered GPU devices.
pub fn device_count() -> usize {
    let reg = registry().lock();
    reg.devices.len()
}

/// List all registered devices (id, name pairs).
pub fn list_devices() -> Vec<(usize, String)> {
    let reg = registry().lock();
    reg.devices.iter().map(|e| (e.id, String::from(e.device.name()))).collect()
}
