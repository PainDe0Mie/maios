//! MBR and GPT partition table parsing for storage devices.
//!
//! This crate reads and parses partition tables from block storage devices.
//! It supports both legacy MBR (Master Boot Record) partition tables and modern
//! GPT (GUID Partition Table) layouts. A protective MBR with type `0xEE` is
//! recognized automatically, triggering GPT parsing from LBA 1.
//!
//! # Usage
//! ```rust,no_run
//! let partitions = partition_table::detect_partitions(&storage_device_ref);
//! for p in &partitions {
//!     log::info!("Partition {}: start_lba={}, sectors={}", p.index, p.start_lba, p.size_sectors);
//! }
//! ```

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use io::IoError;
use storage_device::StorageDeviceRef;

// ---------------------------------------------------------------------------
// Little-endian helpers
// ---------------------------------------------------------------------------

#[inline]
fn read_le_u16(buf: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([buf[offset], buf[offset + 1]])
}

#[inline]
fn read_le_u32(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ])
}

#[inline]
fn read_le_u64(buf: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
        buf[offset + 4],
        buf[offset + 5],
        buf[offset + 6],
        buf[offset + 7],
    ])
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Describes the type of a partition entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartitionType {
    /// MBR partition type byte (e.g. `0x83` for Linux, `0x07` for NTFS).
    MbrType(u8),
    /// GPT partition type GUID stored as a raw 16-byte mixed-endian UUID.
    GptType([u8; 16]),
}

/// Information about a single partition on a storage device.
#[derive(Debug, Clone)]
pub struct PartitionInfo {
    /// Zero-based partition number in the order it was found.
    pub index: usize,
    /// Starting LBA of the partition.
    pub start_lba: u64,
    /// Size of the partition in sectors.
    pub size_sectors: u64,
    /// Partition type identifier (MBR byte or GPT GUID).
    pub partition_type: PartitionType,
    /// Human-readable name. For GPT this comes from the entry name field;
    /// for MBR it is synthesized as `"Partition N"`.
    pub name: String,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// MBR boot signature at bytes 510-511.
const MBR_SIGNATURE: u16 = 0xAA55;

/// MBR partition type indicating a protective MBR (GPT disk).
const MBR_TYPE_PROTECTIVE: u8 = 0xEE;

/// GPT header magic: "EFI PART".
const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";

/// Offsets of the four MBR partition entries within the 512-byte sector.
const MBR_ENTRY_OFFSETS: [usize; 4] = [446, 462, 478, 494];

// ---------------------------------------------------------------------------
// Reading helper
// ---------------------------------------------------------------------------

/// Read `num_sectors` starting at `lba` from the device into a new `Vec<u8>`.
/// The device mutex is locked, the read is performed, and the lock is released.
fn read_sectors(
    device: &StorageDeviceRef,
    lba: usize,
    num_sectors: usize,
) -> Result<Vec<u8>, IoError> {
    let mut dev = device.lock();
    let block_size = dev.block_size();
    let total_bytes = num_sectors * block_size;
    let mut buf = vec![0u8; total_bytes];
    dev.read_blocks(&mut buf, lba)?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// MBR parsing
// ---------------------------------------------------------------------------

/// Result of inspecting the MBR sector.
enum MbrResult {
    /// The MBR contains a protective entry (0xEE) -- caller should parse GPT.
    Protective,
    /// Regular MBR partitions were found.
    Partitions(Vec<PartitionInfo>),
    /// No valid MBR signature or no partitions at all.
    None,
}

/// Parse the 512-byte MBR sector and return what was found.
fn parse_mbr(sector: &[u8]) -> MbrResult {
    // Validate the MBR boot signature.
    if sector.len() < 512 {
        log::warn!("partition_table: MBR sector too short ({} bytes)", sector.len());
        return MbrResult::None;
    }

    let sig = read_le_u16(sector, 510);
    if sig != MBR_SIGNATURE {
        log::debug!("partition_table: no MBR signature (found {:#06X})", sig);
        return MbrResult::None;
    }

    let mut partitions = Vec::new();
    let mut has_protective = false;

    for (idx, &offset) in MBR_ENTRY_OFFSETS.iter().enumerate() {
        let entry = &sector[offset..offset + 16];
        let ptype = entry[4];

        // Skip empty entries.
        if ptype == 0x00 {
            continue;
        }

        if ptype == MBR_TYPE_PROTECTIVE {
            has_protective = true;
            log::info!("partition_table: protective MBR entry detected, disk uses GPT");
            continue;
        }

        let _status = entry[0];
        let lba_start = read_le_u32(entry, 8) as u64;
        let sector_count = read_le_u32(entry, 12) as u64;

        log::info!(
            "partition_table: MBR partition {}: type={:#04X}, start_lba={}, sectors={}",
            idx,
            ptype,
            lba_start,
            sector_count
        );

        partitions.push(PartitionInfo {
            index: idx,
            start_lba: lba_start,
            size_sectors: sector_count,
            partition_type: PartitionType::MbrType(ptype),
            name: {
                use alloc::format;
                format!("Partition {}", idx)
            },
        });
    }

    if has_protective {
        MbrResult::Protective
    } else if partitions.is_empty() {
        MbrResult::None
    } else {
        MbrResult::Partitions(partitions)
    }
}

// ---------------------------------------------------------------------------
// GPT parsing
// ---------------------------------------------------------------------------

/// Parse the GPT header and its partition entries.
/// `device` is the storage device reference, `block_size` the device block size.
fn parse_gpt(device: &StorageDeviceRef, block_size: usize) -> Vec<PartitionInfo> {
    // Read LBA 1 (GPT header).
    let header_sector = match read_sectors(device, 1, 1) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("partition_table: failed to read GPT header at LBA 1: {:?}", e);
            return Vec::new();
        }
    };

    // Validate the GPT signature.
    if &header_sector[0..8] != GPT_SIGNATURE {
        log::warn!("partition_table: invalid GPT signature");
        return Vec::new();
    }

    let revision = read_le_u32(&header_sector, 8);
    let header_size = read_le_u32(&header_sector, 12);
    let my_lba = read_le_u64(&header_sector, 24);
    let first_usable_lba = read_le_u64(&header_sector, 40);
    let last_usable_lba = read_le_u64(&header_sector, 48);

    let mut disk_guid = [0u8; 16];
    disk_guid.copy_from_slice(&header_sector[56..72]);

    let partition_entries_lba = read_le_u64(&header_sector, 72);
    let num_partition_entries = read_le_u32(&header_sector, 80);
    let partition_entry_size = read_le_u32(&header_sector, 84);

    log::info!(
        "partition_table: GPT header: revision={:#010X}, header_size={}, my_lba={}, \
         first_usable={}, last_usable={}, entries_lba={}, num_entries={}, entry_size={}",
        revision,
        header_size,
        my_lba,
        first_usable_lba,
        last_usable_lba,
        partition_entries_lba,
        num_partition_entries,
        partition_entry_size
    );

    // Sanity checks.
    if partition_entry_size == 0 || num_partition_entries == 0 {
        log::warn!("partition_table: GPT header has zero entries or zero entry size");
        return Vec::new();
    }

    // Calculate how many sectors we need to read all partition entries.
    let total_entries_bytes = (num_partition_entries as usize) * (partition_entry_size as usize);
    let sectors_needed = (total_entries_bytes + block_size - 1) / block_size;

    let entries_data = match read_sectors(device, partition_entries_lba as usize, sectors_needed) {
        Ok(d) => d,
        Err(e) => {
            log::warn!("partition_table: failed to read GPT entries: {:?}", e);
            return Vec::new();
        }
    };

    let mut partitions = Vec::new();
    let entry_sz = partition_entry_size as usize;

    for i in 0..(num_partition_entries as usize) {
        let base = i * entry_sz;
        if base + entry_sz > entries_data.len() {
            break;
        }

        let entry = &entries_data[base..base + entry_sz];

        // Type GUID (bytes 0-15). All zeros means an unused entry.
        let mut type_guid = [0u8; 16];
        type_guid.copy_from_slice(&entry[0..16]);
        if type_guid == [0u8; 16] {
            continue;
        }

        let mut unique_guid = [0u8; 16];
        unique_guid.copy_from_slice(&entry[16..32]);

        let first_lba = read_le_u64(entry, 32);
        let last_lba = read_le_u64(entry, 40);
        let _attributes = read_le_u64(entry, 48);

        // Parse the UTF-16LE name field (bytes 56..128).
        let name = parse_gpt_name(&entry[56..entry_sz.min(128)]);

        let size_sectors = if last_lba >= first_lba {
            last_lba - first_lba + 1
        } else {
            0
        };

        log::info!(
            "partition_table: GPT partition {}: first_lba={}, last_lba={}, sectors={}, name=\"{}\"",
            partitions.len(),
            first_lba,
            last_lba,
            size_sectors,
            name
        );

        partitions.push(PartitionInfo {
            index: partitions.len(),
            start_lba: first_lba,
            size_sectors,
            partition_type: PartitionType::GptType(type_guid),
            name,
        });
    }

    log::info!("partition_table: found {} GPT partitions", partitions.len());
    partitions
}

/// Decode a GPT partition name from its UTF-16LE byte slice.
/// The name is null-terminated; trailing nulls are stripped.
fn parse_gpt_name(raw: &[u8]) -> String {
    // raw length must be even for UTF-16 code units.
    let units = raw.len() / 2;
    let mut chars: Vec<u16> = Vec::with_capacity(units);
    for i in 0..units {
        let c = u16::from_le_bytes([raw[i * 2], raw[i * 2 + 1]]);
        if c == 0 {
            break;
        }
        chars.push(c);
    }
    String::from_utf16_lossy(&chars)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Detect and parse the partition table from a storage device.
///
/// The function reads sector 0 to look for an MBR. If the MBR contains a
/// protective entry (type `0xEE`), GPT parsing is performed from sector 1.
/// If regular MBR partitions are found they are returned directly.
///
/// Returns an empty `Vec` when no valid partition table is detected (the
/// device may contain a raw filesystem without a partition table).
pub fn detect_partitions(device: &StorageDeviceRef) -> Vec<PartitionInfo> {
    let block_size = {
        let dev = device.lock();
        dev.block_size()
    };

    // Read the first sector (MBR / protective MBR).
    let sector0 = match read_sectors(device, 0, 1) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("partition_table: failed to read sector 0: {:?}", e);
            return Vec::new();
        }
    };

    match parse_mbr(&sector0) {
        MbrResult::Protective => {
            log::info!("partition_table: parsing GPT after protective MBR");
            parse_gpt(device, block_size)
        }
        MbrResult::Partitions(parts) => {
            log::info!("partition_table: found {} MBR partitions", parts.len());
            parts
        }
        MbrResult::None => {
            log::debug!("partition_table: no partition table detected on device");
            Vec::new()
        }
    }
}
