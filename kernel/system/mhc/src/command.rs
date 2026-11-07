//! MHC Command Buffer — GPU command encoding.
//!
//! Command buffers batch multiple GPU operations into a single submission,
//! reducing per-operation overhead. The buffer is built on the CPU side
//! and submitted to a device queue atomically.
//!
//! ## Design
//!
//! Commands are stored as an enum vector (not raw bytes) for type safety.
//! The driver translates them to hardware-specific command streams during
//! `submit()`. This approach sacrifices some encoding density for safety
//! and debuggability — appropriate for a research OS.

#![allow(dead_code)]

use alloc::vec::Vec;

use crate::fence::FenceId;
use crate::memory::GpuAddress;
use crate::shader::ShaderHandle;
use crate::device::BufferBinding;

// ---------------------------------------------------------------------------
// Pipeline stage flags (for barriers)
// ---------------------------------------------------------------------------

bitflags::bitflags! {
    /// Pipeline stages for synchronization barriers.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct PipelineStage: u32 {
        /// Top of pipe (before any work).
        const TOP            = 0x01;
        /// Compute shader execution.
        const COMPUTE        = 0x02;
        /// DMA transfer operations.
        const TRANSFER       = 0x04;
        /// Host (CPU) access.
        const HOST           = 0x08;
        /// Bottom of pipe (after all work).
        const BOTTOM         = 0x10;
        /// All graphics stages.
        const ALL_GRAPHICS   = 0x20;
    }
}

// ---------------------------------------------------------------------------
// GPU commands
// ---------------------------------------------------------------------------

/// A single GPU command within a command buffer.
#[derive(Clone, Debug)]
pub enum GpuCommand {
    // -- Compute ------------------------------------------------------------

    /// Dispatch a compute shader.
    DispatchCompute {
        /// Handle to a loaded shader module.
        shader: ShaderHandle,
        /// Number of workgroups in [x, y, z].
        workgroups: [u32; 3],
        /// Buffer bindings for this dispatch.
        bindings: Vec<BufferBinding>,
        /// Push constants (small inline data, max 128 bytes).
        push_constants: Vec<u8>,
    },

    // -- Memory operations --------------------------------------------------

    /// Copy data between two GPU buffers.
    CopyBuffer {
        src: GpuAddress,
        dst: GpuAddress,
        size: usize,
    },

    /// Fill a GPU buffer with a 32-bit value.
    FillBuffer {
        dst: GpuAddress,
        size: usize,
        value: u32,
    },

    /// Copy data from host memory to a GPU buffer.
    UploadBuffer {
        /// Source data (will be copied during encoding).
        data: Vec<u8>,
        dst: GpuAddress,
    },

    // -- Synchronization ----------------------------------------------------

    /// Pipeline barrier: ensures all work in `src_stage` completes before
    /// any work in `dst_stage` begins.
    PipelineBarrier {
        src_stage: PipelineStage,
        dst_stage: PipelineStage,
    },

    /// Signal a fence (increment timeline semaphore).
    SignalFence(FenceId),

    /// Wait for a fence before proceeding.
    WaitFence(FenceId),

    // -- Graphics (for MGI integration) -------------------------------------

    /// Blit (copy with optional format conversion) between two GPU images.
    Blit {
        src: GpuAddress,
        dst: GpuAddress,
        src_width: u32,
        src_height: u32,
        dst_width: u32,
        dst_height: u32,
    },

    /// Clear a GPU image to a solid color.
    Clear {
        target: GpuAddress,
        color: [u8; 4], // RGBA
        width: u32,
        height: u32,
    },
}

// ---------------------------------------------------------------------------
// Command buffer
// ---------------------------------------------------------------------------

/// State of a command buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandBufferState {
    /// Accepting new commands.
    Recording,
    /// Finalized, ready for submission.
    Executable,
    /// Currently being executed by the GPU.
    Pending,
    /// Execution completed.
    Completed,
}

/// A batch of GPU commands to be submitted atomically.
pub struct CommandBuffer {
    commands: Vec<GpuCommand>,
    state: CommandBufferState,
}

impl CommandBuffer {
    /// Create a new empty command buffer in recording state.
    pub fn new() -> Self {
        CommandBuffer {
            commands: Vec::new(),
            state: CommandBufferState::Recording,
        }
    }

    /// Create a command buffer with pre-allocated capacity.
    pub fn with_capacity(cap: usize) -> Self {
        CommandBuffer {
            commands: Vec::with_capacity(cap),
            state: CommandBufferState::Recording,
        }
    }

    /// Append a command. Panics if the buffer is not in recording state.
    pub fn push(&mut self, cmd: GpuCommand) {
        assert_eq!(self.state, CommandBufferState::Recording,
            "MHC: cannot record into a non-recording command buffer");
        self.commands.push(cmd);
    }

    /// Convenience: record a compute dispatch.
    pub fn dispatch(
        &mut self,
        shader: ShaderHandle,
        workgroups: [u32; 3],
        bindings: Vec<BufferBinding>,
    ) -> &mut Self {
        self.push(GpuCommand::DispatchCompute {
            shader,
            workgroups,
            bindings,
            push_constants: Vec::new(),
        });
        self
    }

    /// Convenience: record a buffer copy.
    pub fn copy(&mut self, src: GpuAddress, dst: GpuAddress, size: usize) -> &mut Self {
        self.push(GpuCommand::CopyBuffer { src, dst, size });
        self
    }

    /// Convenience: record a buffer fill.
    pub fn fill(&mut self, dst: GpuAddress, size: usize, value: u32) -> &mut Self {
        self.push(GpuCommand::FillBuffer { dst, size, value });
        self
    }

    /// Convenience: record a pipeline barrier.
    pub fn barrier(&mut self, src: PipelineStage, dst: PipelineStage) -> &mut Self {
        self.push(GpuCommand::PipelineBarrier { src_stage: src, dst_stage: dst });
        self
    }

    /// Finalize the command buffer. No more commands can be recorded.
    pub fn finish(&mut self) {
        assert_eq!(self.state, CommandBufferState::Recording,
            "MHC: cannot finish a non-recording command buffer");
        self.state = CommandBufferState::Executable;
    }

    /// Mark as pending (called by the driver during submit).
    pub(crate) fn mark_pending(&mut self) {
        self.state = CommandBufferState::Pending;
    }

    /// Mark as completed (called by the driver after execution).
    pub(crate) fn mark_completed(&mut self) {
        self.state = CommandBufferState::Completed;
    }

    /// Get the recorded commands.
    pub fn commands(&self) -> &[GpuCommand] {
        &self.commands
    }

    /// Number of recorded commands.
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    /// Whether the buffer has no commands.
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// Current state.
    pub fn state(&self) -> CommandBufferState {
        self.state
    }
}

impl Default for CommandBuffer {
    fn default() -> Self { Self::new() }
}
