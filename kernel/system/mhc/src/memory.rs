//! MHC Memory Manager — Unified virtual address space for CPU+GPU.
//!
//! ## Research basis
//!
//! - **NVIDIA Unified Memory** (CUDA 6+): transparent page migration between
//!   CPU and GPU. We adopt the concept but implement it via IOMMU rather than
//!   GPU-side page faulting.
//! - **AMD HSA** (Heterogeneous System Architecture): single virtual address
//!   space shared by all agents. MHC implements this model where the GPU sees
//!   the same virtual addresses as the CPU.
//! - **VAST** (ISCA 2020): virtualized GPU address translation that decouples
//!   GPU page table walks from the GPU's compute pipeline.
//!
//! ## Design
//!
//! MHC exposes a unified allocation API. Each allocation has:
//! - A CPU-visible virtual address (for kernel code to read/write)
//! - A GPU-visible address (for shader dispatch bindings)
//! - Flags controlling visibility, coherence, and access patterns
//!
//! When IOMMU is available, both addresses are identical (true unified memory).
//! Without IOMMU, allocations may require explicit staging copies.

#![allow(dead_code)]

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;

// ---------------------------------------------------------------------------
// Address types
// ---------------------------------------------------------------------------

/// A GPU-visible virtual address.
///
/// In unified memory mode this equals the CPU virtual address.
/// In discrete memory mode this is a device-local address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GpuAddress(pub u64);

impl GpuAddress {
    pub const NULL: GpuAddress = GpuAddress(0);

    #[inline]
    pub fn is_null(self) -> bool { self.0 == 0 }

    #[inline]
    pub fn offset(self, bytes: u64) -> Self { GpuAddress(self.0 + bytes) }
}

// ---------------------------------------------------------------------------
// Allocation flags
// ---------------------------------------------------------------------------

bitflags::bitflags! {
    /// Flags controlling GPU memory allocation behavior.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct GpuMemFlags: u32 {
        /// CPU can read/write this allocation.
        const HOST_VISIBLE  = 0x01;
        /// Prefer device-local memory (fast GPU access, slow CPU access).
        const DEVICE_LOCAL  = 0x02;
        /// No explicit flush/invalidate needed between CPU and GPU access.
        const HOST_COHERENT = 0x04;
        /// Shader can read from this buffer.
        const SHADER_READ   = 0x08;
        /// Shader can write to this buffer.
        const SHADER_WRITE  = 0x10;
        /// Buffer can be used as a DMA transfer source.
        const TRANSFER_SRC  = 0x20;
        /// Buffer can be used as a DMA transfer destination.
        const TRANSFER_DST  = 0x40;
    }
}

impl Default for GpuMemFlags {
    fn default() -> Self {
        GpuMemFlags::HOST_VISIBLE | GpuMemFlags::HOST_COHERENT
            | GpuMemFlags::SHADER_READ | GpuMemFlags::SHADER_WRITE
    }
}

// ---------------------------------------------------------------------------
// Allocation descriptor
// ---------------------------------------------------------------------------

/// Describes a GPU memory allocation.
#[derive(Clone, Debug)]
pub struct GpuAllocation {
    /// CPU-visible virtual address (valid if HOST_VISIBLE).
    pub cpu_addr: u64,
    /// GPU-visible address for shader bindings.
    pub gpu_addr: GpuAddress,
    /// Size in bytes.
    pub size: usize,
    /// Allocation flags.
    pub flags: GpuMemFlags,
    /// Internal ID for the allocator (used by `free`).
    pub(crate) alloc_id: u64,
}

// ---------------------------------------------------------------------------
// Software memory pool (used by the software GPU driver)
// ---------------------------------------------------------------------------

/// A simple bump allocator for the software GPU driver.
///
/// In a real hardware driver, allocations would map to device VRAM or
/// IOMMU-mapped host memory. This allocator uses host heap memory
/// and provides GPU addresses that are identical to CPU addresses
/// (true unified memory semantics).
pub struct SoftwareMemoryPool {
    /// All live allocations.
    allocations: Vec<SoftAllocation>,
    /// Monotonically increasing allocation ID.
    next_id: AtomicU64,
}

struct SoftAllocation {
    id: u64,
    /// Pointer to the heap allocation.
    ptr: *mut u8,
    /// Layout used for deallocation.
    size: usize,
    align: usize,
}

// SAFETY: The raw pointers in SoftAllocation are heap-allocated and
// exclusively owned by the pool. Access is protected by the pool's Mutex.
unsafe impl Send for SoftAllocation {}
unsafe impl Sync for SoftAllocation {}

impl SoftwareMemoryPool {
    pub fn new() -> Self {
        SoftwareMemoryPool {
            allocations: Vec::new(),
            next_id: AtomicU64::new(1),
        }
    }

    /// Allocate `size` bytes aligned to 16 bytes.
    pub fn alloc(&mut self, size: usize, flags: GpuMemFlags) -> Result<GpuAllocation, ()> {
        if size == 0 {
            return Err(());
        }

        let align = 16;
        let layout = match core::alloc::Layout::from_size_align(size, align) {
            Ok(l) => l,
            Err(_) => return Err(()),
        };

        let ptr = unsafe { alloc::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            return Err(());
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let cpu_addr = ptr as u64;

        self.allocations.push(SoftAllocation { id, ptr, size, align });

        Ok(GpuAllocation {
            cpu_addr,
            gpu_addr: GpuAddress(cpu_addr), // Unified: GPU addr == CPU addr
            size,
            flags,
            alloc_id: id,
        })
    }

    /// Free a previously allocated region.
    pub fn free(&mut self, alloc_id: u64) -> Result<(), ()> {
        let idx = self.allocations.iter().position(|a| a.id == alloc_id).ok_or(())?;
        let sa = self.allocations.swap_remove(idx);

        let layout = core::alloc::Layout::from_size_align(sa.size, sa.align)
            .map_err(|_| ())?;
        unsafe { alloc::alloc::dealloc(sa.ptr, layout); }
        Ok(())
    }

    /// Get a mutable slice to an allocation's backing memory.
    ///
    /// # Safety
    /// Caller must ensure no concurrent GPU access to this region.
    pub unsafe fn get_slice(&self, addr: GpuAddress, len: usize) -> Option<&mut [u8]> {
        // In software mode, GPU address == CPU address
        let ptr = addr.0 as *mut u8;
        if ptr.is_null() || len == 0 {
            return None;
        }
        // Verify this address belongs to one of our allocations
        let valid = self.allocations.iter().any(|a| {
            let start = a.ptr as u64;
            let end = start + a.size as u64;
            addr.0 >= start && addr.0 + len as u64 <= end
        });
        if valid {
            Some(core::slice::from_raw_parts_mut(ptr, len))
        } else {
            None
        }
    }
}

impl Drop for SoftwareMemoryPool {
    fn drop(&mut self) {
        // Free all remaining allocations
        for sa in self.allocations.drain(..) {
            if let Ok(layout) = core::alloc::Layout::from_size_align(sa.size, sa.align) {
                unsafe { alloc::alloc::dealloc(sa.ptr, layout); }
            }
        }
    }
}
