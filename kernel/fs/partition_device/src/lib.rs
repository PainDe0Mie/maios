//! Virtual storage device representing a single partition on a disk.
//!
//! A [`PartitionDevice`] wraps an underlying [`StorageDeviceRef`] and translates
//! block offsets by adding the partition's starting LBA. All reads and writes
//! are bounds-checked against the partition's sector count before being
//! forwarded to the physical disk.

#![no_std]

extern crate alloc;
#[macro_use] extern crate log;

use alloc::sync::Arc;
use spin::Mutex;
use storage_device::{StorageDevice, StorageDeviceRef};
use io::{BlockIo, BlockReader, BlockWriter, IoError, KnownLength};

/// A virtual storage device that represents a single partition on a physical disk.
///
/// It holds a reference to the underlying disk and translates all block offsets
/// by the partition's starting LBA. Boundary checks ensure that no access
/// can escape the partition's region on disk.
pub struct PartitionDevice {
    /// The underlying physical disk.
    disk: StorageDeviceRef,
    /// Starting LBA of this partition on the disk.
    start_lba: u64,
    /// Total number of sectors (blocks) in this partition.
    sector_count: u64,
    /// Block size in bytes, inherited from the underlying disk.
    block_size: usize,
}

impl PartitionDevice {
    /// Creates a new `PartitionDevice` backed by the given disk,
    /// starting at `start_lba` and spanning `sector_count` sectors.
    ///
    /// Returns a `StorageDeviceRef` so the partition can be used anywhere
    /// a storage device is expected.
    pub fn new(disk: StorageDeviceRef, start_lba: u64, sector_count: u64) -> StorageDeviceRef {
        let block_size = disk.lock().block_size();
        info!(
            "PartitionDevice: start_lba={}, sectors={}, block_size={}",
            start_lba, sector_count, block_size
        );
        Arc::new(Mutex::new(PartitionDevice {
            disk,
            start_lba,
            sector_count,
            block_size,
        }))
    }
}

impl BlockIo for PartitionDevice {
    fn block_size(&self) -> usize {
        self.block_size
    }
}

impl BlockReader for PartitionDevice {
    fn read_blocks(&mut self, buffer: &mut [u8], block_offset: usize) -> Result<usize, IoError> {
        let blocks_requested = buffer.len() / self.block_size;
        if block_offset + blocks_requested > self.sector_count as usize {
            return Err(IoError::from(
                "PartitionDevice: read beyond partition boundary",
            ));
        }
        let actual_offset = self.start_lba as usize + block_offset;
        self.disk.lock().read_blocks(buffer, actual_offset)
    }
}

impl BlockWriter for PartitionDevice {
    fn write_blocks(&mut self, buffer: &[u8], block_offset: usize) -> Result<usize, IoError> {
        let blocks_requested = buffer.len() / self.block_size;
        if block_offset + blocks_requested > self.sector_count as usize {
            return Err(IoError::from(
                "PartitionDevice: write beyond partition boundary",
            ));
        }
        let actual_offset = self.start_lba as usize + block_offset;
        self.disk.lock().write_blocks(buffer, actual_offset)
    }

    fn flush(&mut self) -> Result<(), IoError> {
        self.disk.lock().flush()
    }
}

impl KnownLength for PartitionDevice {
    fn len(&self) -> usize {
        self.sector_count as usize * self.block_size
    }
}

impl StorageDevice for PartitionDevice {
    fn size_in_blocks(&self) -> usize {
        self.sector_count as usize
    }
}
