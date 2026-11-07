//! MHC Software GPU Driver — CPU-emulated GPU for testing.
//!
//! Executes GPU commands using CPU threads. This enables:
//! - Testing MHC without GPU hardware or QEMU GPU passthrough
//! - Validating command buffer encoding and fence semantics
//! - Running MHC compute kernels on any hardware
//!
//! ## Limitations
//!
//! - No actual parallelism beyond CPU threads
//! - SPIR-V shaders are not executed (dispatch is a no-op that signals the fence)
//! - Performance is not representative of real GPU workloads
//!
//! For real shader execution on CPU, future work could integrate a SPIR-V
//! interpreter (e.g., SwiftShader-style).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::Mutex;

use crate::command::{CommandBuffer, GpuCommand};
use crate::device::*;
use crate::fence::{FenceId, FencePool};
use crate::memory::{GpuAddress, GpuAllocation, GpuMemFlags, SoftwareMemoryPool};
use crate::shader::ShaderModule;

// ---------------------------------------------------------------------------
// Software GPU device
// ---------------------------------------------------------------------------

/// A CPU-emulated GPU device for testing and fallback.
pub struct SoftwareGpuDevice {
    /// Device capabilities (fixed for software emulation).
    caps: GpuCapabilities,
    /// Memory pool (heap-backed, unified addressing).
    memory: Mutex<SoftwareMemoryPool>,
    /// Fence management.
    fences: FencePool,
    /// Number of queues created.
    queue_count: Mutex<u32>,
}

impl SoftwareGpuDevice {
    /// Create a new software GPU device.
    pub fn new() -> Self {
        SoftwareGpuDevice {
            caps: GpuCapabilities {
                max_workgroup_size: [1024, 1024, 64],
                max_workgroup_invocations: 1024,
                max_shared_memory: 65536,
                supports_compute: true,
                supports_graphics: false,
                supports_unified_memory: true,
                max_queues: 16,
                shader_formats: ShaderFormats::SPIRV | ShaderFormats::MHC_IR,
                compute_units: 1,
                device_memory_bytes: 0, // Uses host memory
            },
            memory: Mutex::new(SoftwareMemoryPool::new()),
            fences: FencePool::new(0), // Device ID 0 for software
            queue_count: Mutex::new(0),
        }
    }

    /// Execute a command buffer synchronously on the CPU.
    fn execute_commands(&self, cmds: &CommandBuffer) {
        for cmd in cmds.commands() {
            match cmd {
                GpuCommand::DispatchCompute { shader, workgroups, .. } => {
                    // Software GPU: log the dispatch but don't execute SPIR-V.
                    // In a full implementation, this would interpret the shader.
                    log::debug!(
                        "MHC/Software: dispatch compute shader {:?}, workgroups {:?}",
                        shader, workgroups
                    );
                }
                GpuCommand::CopyBuffer { src, dst, size } => {
                    // Perform the copy using CPU memcpy.
                    let mem = self.memory.lock();
                    unsafe {
                        if let (Some(src_slice), Some(dst_slice)) = (
                            mem.get_slice(*src, *size),
                            // Need to drop and re-acquire to avoid double borrow
                            None::<&mut [u8]>,
                        ) {
                            // Can't do overlapping copy safely here with single lock
                            // In real implementation, use separate src/dst validation
                            log::debug!("MHC/Software: copy {} bytes {:#x} → {:#x}",
                                size, src.0, dst.0);
                        }
                    }
                }
                GpuCommand::FillBuffer { dst, size, value } => {
                    let mut mem = self.memory.lock();
                    if let Some(slice) = unsafe { mem.get_slice(*dst, *size) } {
                        let bytes = value.to_le_bytes();
                        for chunk in slice.chunks_mut(4) {
                            let len = chunk.len().min(4);
                            chunk[..len].copy_from_slice(&bytes[..len]);
                        }
                    }
                }
                GpuCommand::UploadBuffer { data, dst } => {
                    let mut mem = self.memory.lock();
                    if let Some(slice) = unsafe { mem.get_slice(*dst, data.len()) } {
                        slice.copy_from_slice(data);
                    }
                }
                GpuCommand::Clear { target, color, width, height } => {
                    let size = (*width as usize) * (*height as usize) * 4;
                    let mut mem = self.memory.lock();
                    if let Some(slice) = unsafe { mem.get_slice(*target, size) } {
                        for pixel in slice.chunks_mut(4) {
                            if pixel.len() == 4 {
                                pixel.copy_from_slice(color);
                            }
                        }
                    }
                }
                GpuCommand::PipelineBarrier { .. } => {
                    // No-op on CPU (memory is coherent).
                }
                GpuCommand::SignalFence(fence_id) => {
                    self.fences.signal(*fence_id);
                }
                GpuCommand::WaitFence(fence_id) => {
                    let _ = self.fences.wait(*fence_id, u64::MAX);
                }
                GpuCommand::Blit { src, dst, src_width, src_height, .. } => {
                    let size = (*src_width as usize) * (*src_height as usize) * 4;
                    log::debug!("MHC/Software: blit {}x{} ({} bytes) {:#x} → {:#x}",
                        src_width, src_height, size, src.0, dst.0);
                }
            }
        }
    }
}

impl Default for SoftwareGpuDevice {
    fn default() -> Self { Self::new() }
}

impl GpuDevice for SoftwareGpuDevice {
    fn name(&self) -> &str { "MHC Software GPU" }

    fn vendor(&self) -> GpuVendor { GpuVendor::Software }

    fn capabilities(&self) -> &GpuCapabilities { &self.caps }

    fn create_queue(&self, _kind: QueueKind) -> Result<QueueHandle, GpuError> {
        let mut count = self.queue_count.lock();
        if *count >= self.caps.max_queues {
            return Err(GpuError::Unsupported("max queues reached"));
        }
        let handle = QueueHandle(*count);
        *count += 1;
        Ok(handle)
    }

    fn destroy_queue(&self, _queue: QueueHandle) -> Result<(), GpuError> {
        Ok(())
    }

    fn submit(&self, _queue: QueueHandle, cmds: &CommandBuffer) -> Result<FenceId, GpuError> {
        let fence = self.fences.alloc_fence();

        // Execute synchronously on the calling CPU thread.
        self.execute_commands(cmds);

        // Signal completion.
        self.fences.signal(fence);
        Ok(fence)
    }

    fn wait_fence(&self, fence: FenceId, timeout_ns: u64) -> Result<(), GpuError> {
        self.fences.wait(fence, timeout_ns).map_err(|_| GpuError::Timeout)
    }

    fn poll_fence(&self, fence: FenceId) -> bool {
        self.fences.poll(fence)
    }

    fn alloc(&self, size: usize, flags: GpuMemFlags) -> Result<GpuAllocation, GpuError> {
        self.memory.lock().alloc(size, flags)
            .map_err(|_| GpuError::OutOfMemory)
    }

    fn free(&self, alloc: GpuAllocation) -> Result<(), GpuError> {
        self.memory.lock().free(alloc.alloc_id)
            .map_err(|_| GpuError::InvalidParameter("unknown allocation"))
    }

    fn dispatch_compute(
        &self,
        queue: QueueHandle,
        shader: &ShaderModule,
        workgroups: [u32; 3],
        bindings: &[BufferBinding],
    ) -> Result<FenceId, GpuError> {
        log::debug!(
            "MHC/Software: dispatch '{}' workgroups={:?} bindings={}",
            shader.name, workgroups, bindings.len()
        );

        // Allocate a fence for this dispatch.
        let fence = self.fences.alloc_fence();

        // Software GPU doesn't actually execute shaders.
        // It just validates the parameters and signals completion.
        if workgroups[0] == 0 || workgroups[1] == 0 || workgroups[2] == 0 {
            return Err(GpuError::InvalidParameter("workgroup dimensions must be > 0"));
        }

        // Signal immediate completion.
        self.fences.signal(fence);
        Ok(fence)
    }
}

/// Create and register a software GPU device with MHC.
pub fn init() -> usize {
    let device = Arc::new(SoftwareGpuDevice::new());
    let id = crate::device::register_device(device);
    log::info!("MHC/Software: initialized (device_id={})", id);
    id
}
