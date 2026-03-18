//! MHC VirtIO-GPU Driver — GPU driver for QEMU/KVM virtual machines.
//!
//! ## VirtIO-GPU Specification
//!
//! Implements VirtIO GPU Device (device type 16) per the VirtIO 1.2 spec:
//! - PCI vendor 0x1AF4, device 0x1050 (transitional) or 0x1040+16 (modern)
//! - Two virtqueues: controlq (commands) and cursorq (cursor updates)
//! - Supports 2D (scanout, transfer, resource management)
//! - Optionally supports 3D via virgl (OpenGL) or venus (Vulkan)
//!
//! ## Current Status
//!
//! This is a skeleton driver providing the structure for VirtIO-GPU integration.
//! Full implementation requires the VirtIO transport layer and PCI device
//! access, which will be connected when the `virtio` feature is enabled.
//!
//! ## References
//!
//! - VirtIO Spec 1.2, Section 5.7: GPU Device
//! - virgl (Mesa): 3D acceleration over virtio-gpu
//! - venus (Mesa): Vulkan pass-through over virtio-gpu

#![allow(dead_code)]

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::Mutex;

use crate::command::CommandBuffer;
use crate::device::*;
use crate::fence::{FenceId, FencePool};
use crate::memory::{GpuAddress, GpuAllocation, GpuMemFlags};
use crate::shader::ShaderModule;

// ---------------------------------------------------------------------------
// VirtIO-GPU constants
// ---------------------------------------------------------------------------

/// VirtIO-GPU command types (from spec 5.7.6.7).
#[repr(u32)]
#[derive(Clone, Copy, Debug)]
pub enum VirtioGpuCmd {
    // 2D commands
    GetDisplayInfo     = 0x0100,
    ResourceCreate2d   = 0x0101,
    ResourceUnref      = 0x0102,
    SetScanout         = 0x0103,
    ResourceFlush      = 0x0104,
    TransferToHost2d   = 0x0105,
    ResourceAttachBacking = 0x0106,
    ResourceDetachBacking = 0x0107,
    GetCapsetInfo      = 0x0108,
    GetCapset          = 0x0109,
    GetEdid            = 0x010A,

    // 3D commands (virgl)
    CtxCreate          = 0x0200,
    CtxDestroy         = 0x0201,
    CtxAttachResource  = 0x0202,
    CtxDetachResource  = 0x0203,
    ResourceCreate3d   = 0x0204,
    TransferToHost3d   = 0x0205,
    TransferFromHost3d = 0x0206,
    SubmitCmd3d        = 0x0207,
    ResourceMapBlob    = 0x0208,
    ResourceUnmapBlob  = 0x0209,

    // Cursor commands
    UpdateCursor       = 0x0300,
    MoveCursor         = 0x0301,
}

/// VirtIO-GPU response types.
#[repr(u32)]
#[derive(Clone, Copy, Debug)]
pub enum VirtioGpuResp {
    OkNodata          = 0x1100,
    OkDisplayInfo     = 0x1101,
    OkCapsetInfo      = 0x1102,
    OkCapset          = 0x1103,
    OkEdid            = 0x1104,
    OkResourceUuid    = 0x1105,
    OkMapInfo         = 0x1106,

    ErrUnspec         = 0x1200,
    ErrOutOfMemory    = 0x1201,
    ErrInvalidScanoutId = 0x1202,
    ErrInvalidResourceId = 0x1203,
    ErrInvalidContextId  = 0x1204,
    ErrInvalidParameter  = 0x1205,
}

/// VirtIO-GPU pixel formats.
#[repr(u32)]
#[derive(Clone, Copy, Debug)]
pub enum VirtioGpuFormat {
    B8G8R8A8Unorm = 1,
    B8G8R8X8Unorm = 2,
    A8R8G8B8Unorm = 3,
    X8R8G8B8Unorm = 4,
    R8G8B8A8Unorm = 67,
    X8B8G8R8Unorm = 68,
    A8B8G8R8Unorm = 121,
    R8G8B8X8Unorm = 134,
}

// ---------------------------------------------------------------------------
// VirtIO-GPU device state
// ---------------------------------------------------------------------------

/// VirtIO-GPU device driver.
///
/// This is the skeleton structure. Full implementation requires:
/// 1. PCI device discovery (via `kernel/pci`)
/// 2. VirtQueue setup (control + cursor queues)
/// 3. MMIO register mapping
/// 4. Interrupt handler registration
pub struct VirtioGpuDevice {
    /// Device capabilities.
    caps: GpuCapabilities,
    /// Fence pool for tracking command completion.
    fences: FencePool,
    /// Whether the device has been fully initialized.
    initialized: bool,
    /// Number of scanout displays.
    num_scanouts: u32,
    /// Whether 3D (virgl/venus) is supported.
    has_virgl: bool,
    /// Whether blob resources are supported (for zero-copy).
    has_blob: bool,
}

impl VirtioGpuDevice {
    /// Create a new VirtIO-GPU device instance.
    ///
    /// This does NOT initialize the device. Call `probe()` to detect and
    /// initialize the hardware.
    pub fn new() -> Self {
        VirtioGpuDevice {
            caps: GpuCapabilities {
                max_workgroup_size: [256, 256, 64],
                max_workgroup_invocations: 256,
                max_shared_memory: 32768,
                supports_compute: false, // Only with virgl/venus 3D
                supports_graphics: true,
                supports_unified_memory: false,
                max_queues: 2, // controlq + cursorq
                shader_formats: ShaderFormats::empty(),
                compute_units: 0,
                device_memory_bytes: 0,
            },
            fences: FencePool::new(1), // Device ID 1 for VirtIO
            initialized: false,
            num_scanouts: 0,
            has_virgl: false,
            has_blob: false,
        }
    }

    /// Probe for a VirtIO-GPU device on the PCI bus.
    ///
    /// Returns `Ok(())` if a device was found and initialized.
    /// Returns `Err` with a description if no device was found.
    pub fn probe(&mut self) -> Result<(), &'static str> {
        // TODO: Use kernel/pci to find VirtIO-GPU device
        // PCI vendor: 0x1AF4
        // PCI device: 0x1050 (transitional) or 0x1040+16=0x1050 (modern)
        // Subsystem ID: 16 (GPU)
        //
        // Steps:
        // 1. Enumerate PCI devices
        // 2. Find VirtIO-GPU device
        // 3. Map BAR0 for device registers
        // 4. Initialize virtqueues (controlq index=0, cursorq index=1)
        // 5. Negotiate features (VIRGL_SUPPORTED, EDID_SUPPORTED, etc.)
        // 6. Read display info via VIRTIO_GPU_CMD_GET_DISPLAY_INFO
        // 7. Register interrupt handler

        log::warn!("MHC/VirtIO-GPU: probe() not yet implemented — \
                    requires PCI + virtio transport integration");
        Err("VirtIO-GPU probe not yet implemented")
    }
}

impl Default for VirtioGpuDevice {
    fn default() -> Self { Self::new() }
}

impl GpuDevice for VirtioGpuDevice {
    fn name(&self) -> &str { "VirtIO-GPU" }

    fn vendor(&self) -> GpuVendor { GpuVendor::VirtIO }

    fn capabilities(&self) -> &GpuCapabilities { &self.caps }

    fn create_queue(&self, kind: QueueKind) -> Result<QueueHandle, GpuError> {
        if !self.initialized {
            return Err(GpuError::DeviceNotFound);
        }
        match kind {
            QueueKind::Graphics | QueueKind::Universal => Ok(QueueHandle(0)), // controlq
            QueueKind::Transfer => Ok(QueueHandle(0)), // Same queue for transfers
            QueueKind::Compute => {
                if self.has_virgl {
                    Ok(QueueHandle(0))
                } else {
                    Err(GpuError::Unsupported("compute requires virgl/venus 3D support"))
                }
            }
        }
    }

    fn destroy_queue(&self, _queue: QueueHandle) -> Result<(), GpuError> {
        Ok(())
    }

    fn submit(&self, _queue: QueueHandle, _cmds: &CommandBuffer) -> Result<FenceId, GpuError> {
        if !self.initialized {
            return Err(GpuError::DeviceNotFound);
        }
        // TODO: Translate commands to VirtIO-GPU protocol messages
        // and send via the control virtqueue.
        let fence = self.fences.alloc_fence();
        // For now, immediately signal (no actual GPU work)
        self.fences.signal(fence);
        Ok(fence)
    }

    fn wait_fence(&self, fence: FenceId, timeout_ns: u64) -> Result<(), GpuError> {
        self.fences.wait(fence, timeout_ns).map_err(|_| GpuError::Timeout)
    }

    fn poll_fence(&self, fence: FenceId) -> bool {
        self.fences.poll(fence)
    }

    fn alloc(&self, _size: usize, _flags: GpuMemFlags) -> Result<GpuAllocation, GpuError> {
        if !self.initialized {
            return Err(GpuError::DeviceNotFound);
        }
        // TODO: Create a VirtIO-GPU 2D resource or 3D blob resource
        Err(GpuError::Unsupported("VirtIO-GPU alloc not yet implemented"))
    }

    fn free(&self, _alloc: GpuAllocation) -> Result<(), GpuError> {
        // TODO: Unref the VirtIO-GPU resource
        Err(GpuError::Unsupported("VirtIO-GPU free not yet implemented"))
    }

    fn dispatch_compute(
        &self,
        _queue: QueueHandle,
        _shader: &ShaderModule,
        _workgroups: [u32; 3],
        _bindings: &[BufferBinding],
    ) -> Result<FenceId, GpuError> {
        if !self.has_virgl {
            return Err(GpuError::Unsupported(
                "compute dispatch requires virgl/venus 3D support"
            ));
        }
        // TODO: Encode as virgl/venus 3D command and submit via VIRTIO_GPU_CMD_SUBMIT_3D
        Err(GpuError::Unsupported("VirtIO-GPU compute not yet implemented"))
    }

    fn blit_to_scanout(
        &self,
        _queue: QueueHandle,
        _src: GpuAddress,
        _src_stride: u32,
        _src_width: u32,
        _src_height: u32,
    ) -> Result<FenceId, GpuError> {
        if !self.initialized {
            return Err(GpuError::DeviceNotFound);
        }
        // TODO: Use VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D + VIRTIO_GPU_CMD_RESOURCE_FLUSH
        Err(GpuError::Unsupported("VirtIO-GPU blit not yet implemented"))
    }
}

/// Attempt to probe and register a VirtIO-GPU device.
/// Returns `Some(device_id)` if successful, `None` if no device found.
pub fn try_init() -> Option<usize> {
    let mut device = VirtioGpuDevice::new();
    match device.probe() {
        Ok(()) => {
            let id = crate::device::register_device(Arc::new(device));
            log::info!("MHC/VirtIO-GPU: initialized (device_id={})", id);
            Some(id)
        }
        Err(msg) => {
            log::debug!("MHC/VirtIO-GPU: {}", msg);
            None
        }
    }
}
