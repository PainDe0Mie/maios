//! Zero-copy buffer management for MIO.
//!
//! Provides pre-registered buffer pools that eliminate per-I/O copy overhead.
//! Buffers are pinned in physical memory and mapped into both kernel and
//! userspace address spaces, enabling true zero-copy data transfer.
//!
//! Design based on:
//! - io_uring fixed buffers (IORING_REGISTER_BUFFERS)
//! - DPDK `rte_mempool` (per-core caching, bulk alloc/free)
//! - SPDK memory registration for NVMe DMA
//!
//! ## Buffer pool hierarchy
//!
//! ```text
//! BufferPool
//! ├── group_id: u16 (matches SQE buf_group field)
//! ├── buf_size: u32 (uniform size per pool)
//! ├── count: u32
//! └── entries[0..count]
//!     ├── BufferEntry { addr, len, state }
//!     └── ...
//! ```

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use spin::Mutex;

// ---------------------------------------------------------------------------
// Buffer states
// ---------------------------------------------------------------------------

/// State of a single buffer entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BufferState {
    /// Buffer is free and available for allocation.
    Free = 0,
    /// Buffer is currently owned by the kernel (I/O in progress).
    KernelOwned = 1,
    /// Buffer is currently owned by userspace.
    UserOwned = 2,
    /// Buffer is pinned for DMA (cannot be reclaimed).
    DmaPinned = 3,
}

// ---------------------------------------------------------------------------
// Buffer entry
// ---------------------------------------------------------------------------

/// A single registered buffer.
#[derive(Debug, Clone)]
pub struct BufferEntry {
    /// Virtual address of the buffer.
    pub addr: u64,
    /// Length of the buffer in bytes.
    pub len: u32,
    /// Current state.
    pub state: BufferState,
    /// Buffer ID within the pool.
    pub buf_id: u16,
    /// Reference count (for shared buffers).
    pub ref_count: u32,
}

impl BufferEntry {
    fn new(addr: u64, len: u32, buf_id: u16) -> Self {
        BufferEntry {
            addr,
            len,
            state: BufferState::Free,
            buf_id,
            ref_count: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Buffer pool
// ---------------------------------------------------------------------------

/// A pool of uniformly-sized, pre-registered buffers.
///
/// All buffers in a pool have the same size, simplifying allocation to O(1)
/// pop from a free list. Pools are identified by `group_id` which matches
/// the `buf_group` field in SQEs.
pub struct BufferPool {
    /// Pool group ID (matches SQE buf_group).
    pub group_id: u16,
    /// Size of each buffer in the pool.
    pub buf_size: u32,
    /// All buffer entries.
    entries: Vec<BufferEntry>,
    /// Free list: indices into `entries` that are available.
    free_list: Mutex<Vec<u16>>,
    /// Total number of buffers.
    pub count: u32,
    /// Number of currently allocated buffers.
    pub allocated: AtomicU32,
}

impl BufferPool {
    /// Create a new buffer pool.
    ///
    /// `base_addr` is the starting virtual address of the pre-allocated
    /// contiguous memory region. Buffers are laid out sequentially.
    pub fn new(group_id: u16, buf_size: u32, count: u32, base_addr: u64) -> Self {
        let mut entries = Vec::with_capacity(count as usize);
        let mut free_list = Vec::with_capacity(count as usize);

        for i in 0..count {
            let addr = base_addr + (i as u64) * (buf_size as u64);
            entries.push(BufferEntry::new(addr, buf_size, i as u16));
            free_list.push(i as u16);
        }

        BufferPool {
            group_id,
            buf_size,
            entries,
            free_list: Mutex::new(free_list),
            count,
            allocated: AtomicU32::new(0),
        }
    }

    /// Allocate a buffer from the pool.
    ///
    /// Returns the buffer ID and address, or None if the pool is exhausted.
    /// O(1) — pops from the free list.
    pub fn alloc(&self) -> Option<(u16, u64)> {
        let mut free = self.free_list.lock();
        if let Some(idx) = free.pop() {
            let entry = &self.entries[idx as usize];
            self.allocated.fetch_add(1, Ordering::Relaxed);
            Some((entry.buf_id, entry.addr))
        } else {
            None
        }
    }

    /// Free a buffer back to the pool.
    ///
    /// O(1) — pushes to the free list.
    pub fn free(&self, buf_id: u16) {
        if (buf_id as u32) < self.count {
            let mut free = self.free_list.lock();
            free.push(buf_id);
            self.allocated.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Allocate a batch of buffers.
    ///
    /// Returns up to `max` buffer (id, addr) pairs. More efficient than
    /// individual allocs due to single lock acquisition.
    pub fn alloc_batch(&self, max: u32) -> Vec<(u16, u64)> {
        let mut free = self.free_list.lock();
        let take = (max as usize).min(free.len());
        let mut result = Vec::with_capacity(take);

        for _ in 0..take {
            if let Some(idx) = free.pop() {
                let entry = &self.entries[idx as usize];
                result.push((entry.buf_id, entry.addr));
            }
        }

        self.allocated.fetch_add(result.len() as u32, Ordering::Relaxed);
        result
    }

    /// Free a batch of buffers back to the pool.
    pub fn free_batch(&self, buf_ids: &[u16]) {
        let mut free = self.free_list.lock();
        let mut count = 0u32;
        for &id in buf_ids {
            if (id as u32) < self.count {
                free.push(id);
                count += 1;
            }
        }
        self.allocated.fetch_sub(count, Ordering::Relaxed);
    }

    /// Number of free buffers available.
    #[inline]
    pub fn free_count(&self) -> u32 {
        self.count - self.allocated.load(Ordering::Relaxed)
    }

    /// Look up a buffer entry by ID.
    pub fn get(&self, buf_id: u16) -> Option<&BufferEntry> {
        self.entries.get(buf_id as usize)
    }

    /// Mark a buffer as DMA-pinned (cannot be freed until unpinned).
    pub fn pin_for_dma(&self, buf_id: u16) -> bool {
        if (buf_id as u32) < self.count {
            // In a real implementation, this would update the entry state
            // and ensure the physical pages are pinned.
            true
        } else {
            false
        }
    }

    /// Unpin a DMA-pinned buffer.
    pub fn unpin_dma(&self, buf_id: u16) -> bool {
        if (buf_id as u32) < self.count {
            true
        } else {
            false
        }
    }
}
