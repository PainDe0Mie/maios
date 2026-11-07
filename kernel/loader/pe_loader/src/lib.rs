//! PE/COFF binary loader for MaiOS.
//!
//! This crate parses PE32+ (64-bit PE) headers and loads sections into
//! freshly allocated virtual memory, returning the entry point address
//! and the list of [`MappedPages`] that keep the loaded binary alive.
//!
//! Supported: x86_64, little-endian PE32+ (IMAGE_FILE_MACHINE_AMD64).
//!
//! # PE Format Overview
//!
//! ```text
//! Offset 0x00: DOS Header ("MZ" signature, e_lfanew at offset 0x3C)
//! Offset e_lfanew: PE Signature ("PE\0\0")
//! Offset e_lfanew+4: COFF Header (20 bytes)
//! Offset e_lfanew+24: Optional Header (PE32+ = 240 bytes)
//! Following: Section Headers (40 bytes each)
//! ```

#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use log::{debug, error};
use memory::MappedPages;
use memory_structs::VirtualAddress;
use pte_flags::PteFlagsArch;
// PAGE_SIZE will be used when we add page-aligned loading
#[allow(unused_imports)]
use kernel_config::memory::PAGE_SIZE;

// ---------------------------------------------------------------------------
// PE/COFF constants
// ---------------------------------------------------------------------------

const DOS_MAGIC: [u8; 2] = [b'M', b'Z'];
const PE_SIGNATURE: [u8; 4] = [b'P', b'E', 0, 0];
const IMAGE_FILE_MACHINE_AMD64: u16 = 0x8664;
const PE32_PLUS_MAGIC: u16 = 0x020B;

/// Section characteristic flags.
#[allow(dead_code)]
mod section_flags {
    pub const IMAGE_SCN_CNT_CODE: u32 = 0x0000_0020;
    pub const IMAGE_SCN_CNT_INITIALIZED_DATA: u32 = 0x0000_0040;
    pub const IMAGE_SCN_CNT_UNINITIALIZED_DATA: u32 = 0x0000_0080;
    pub const IMAGE_SCN_MEM_EXECUTE: u32 = 0x2000_0000;
    pub const IMAGE_SCN_MEM_READ: u32 = 0x4000_0000;
    pub const IMAGE_SCN_MEM_WRITE: u32 = 0x8000_0000;
}

// ---------------------------------------------------------------------------
// PE header structures
// ---------------------------------------------------------------------------

/// The DOS header at the very start of a PE file.
/// Only `e_magic` and `e_lfanew` are relevant for PE loading.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct DosHeader {
    pub e_magic: [u8; 2],
    _padding: [u8; 58],
    /// File offset to the PE signature.
    pub e_lfanew: u32,
}

/// The COFF file header (20 bytes), immediately after the PE signature.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct CoffHeader {
    pub machine: u16,
    pub number_of_sections: u16,
    pub time_date_stamp: u32,
    pub pointer_to_symbol_table: u32,
    pub number_of_symbols: u32,
    pub size_of_optional_header: u16,
    pub characteristics: u16,
}

/// The PE32+ Optional Header (first 112 bytes of the standard + Windows fields).
/// We only parse the fields we need for loading.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct OptionalHeader64 {
    pub magic: u16,
    pub major_linker_version: u8,
    pub minor_linker_version: u8,
    pub size_of_code: u32,
    pub size_of_initialized_data: u32,
    pub size_of_uninitialized_data: u32,
    pub address_of_entry_point: u32,
    pub base_of_code: u32,
    pub image_base: u64,
    pub section_alignment: u32,
    pub file_alignment: u32,
    pub major_os_version: u16,
    pub minor_os_version: u16,
    pub major_image_version: u16,
    pub minor_image_version: u16,
    pub major_subsystem_version: u16,
    pub minor_subsystem_version: u16,
    pub win32_version_value: u32,
    pub size_of_image: u32,
    pub size_of_headers: u32,
    pub checksum: u32,
    pub subsystem: u16,
    pub dll_characteristics: u16,
    pub size_of_stack_reserve: u64,
    pub size_of_stack_commit: u64,
    pub size_of_heap_reserve: u64,
    pub size_of_heap_commit: u64,
    pub loader_flags: u32,
    pub number_of_rva_and_sizes: u32,
}

/// A PE section header (40 bytes each).
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct SectionHeader {
    pub name: [u8; 8],
    pub virtual_size: u32,
    pub virtual_address: u32,
    pub size_of_raw_data: u32,
    pub pointer_to_raw_data: u32,
    pub pointer_to_relocations: u32,
    pub pointer_to_linenumbers: u32,
    pub number_of_relocations: u16,
    pub number_of_linenumbers: u16,
    pub characteristics: u32,
}

impl SectionHeader {
    /// Returns the section name as a string (truncated at first null byte).
    pub fn name_str(&self) -> &str {
        let end = self.name.iter().position(|&b| b == 0).unwrap_or(8);
        core::str::from_utf8(&self.name[..end]).unwrap_or("???")
    }
}

// ---------------------------------------------------------------------------
// Result type
// ---------------------------------------------------------------------------

/// The result of successfully loading a PE binary into memory.
pub struct LoadedPe {
    /// Virtual address of the PE entry point (ImageBase + AddressOfEntryPoint).
    pub entry_point: VirtualAddress,
    /// The image base address used for loading.
    pub image_base: u64,
    /// Owned page mappings for every loaded section.
    /// Dropping these will unmap the binary from memory.
    pub sections: Vec<MappedPages>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read a little-endian u32 from a byte slice at the given offset.
fn read_u32(data: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}

/// Convert PE section characteristic flags to MaiOS PTE flags.
fn pe_section_flags_to_pte(characteristics: u32) -> PteFlagsArch {
    let mut flags = PteFlagsArch::new().valid(true);

    if characteristics & section_flags::IMAGE_SCN_MEM_WRITE != 0 {
        flags = flags.writable(true);
    }
    if characteristics & section_flags::IMAGE_SCN_MEM_EXECUTE != 0 {
        flags = flags.executable(true);
    }

    flags
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse and validate a PE32+ header from raw bytes.
///
/// Returns `(coff_header, optional_header, section_headers_offset)`.
pub fn parse_header(data: &[u8]) -> Result<(&CoffHeader, &OptionalHeader64, usize), &'static str> {
    // 1. DOS header
    if data.len() < core::mem::size_of::<DosHeader>() {
        return Err("pe_loader: data too small for DOS header");
    }

    if data[0..2] != DOS_MAGIC {
        return Err("pe_loader: invalid DOS magic (expected MZ)");
    }

    let e_lfanew = read_u32(data, 0x3C) as usize;

    // 2. PE signature
    if e_lfanew + 4 > data.len() {
        return Err("pe_loader: e_lfanew points beyond data");
    }
    if data[e_lfanew..e_lfanew + 4] != PE_SIGNATURE {
        return Err("pe_loader: invalid PE signature (expected PE\\0\\0)");
    }

    // 3. COFF header
    let coff_off = e_lfanew + 4;
    let coff_end = coff_off + core::mem::size_of::<CoffHeader>();
    if coff_end > data.len() {
        return Err("pe_loader: data too small for COFF header");
    }
    let coff = unsafe { &*(data.as_ptr().add(coff_off) as *const CoffHeader) };

    let coff_machine = coff.machine;
    if coff_machine != IMAGE_FILE_MACHINE_AMD64 {
        return Err("pe_loader: not an x86_64 PE (IMAGE_FILE_MACHINE_AMD64 expected)");
    }

    // 4. Optional header (PE32+)
    let opt_off = coff_end;
    let opt_end = opt_off + core::mem::size_of::<OptionalHeader64>();
    if opt_end > data.len() {
        return Err("pe_loader: data too small for PE32+ optional header");
    }
    let opt = unsafe { &*(data.as_ptr().add(opt_off) as *const OptionalHeader64) };

    let opt_magic = opt.magic;
    if opt_magic != PE32_PLUS_MAGIC {
        return Err("pe_loader: not a PE32+ binary (expected magic 0x020B)");
    }

    // 5. Section headers start after the optional header
    let opt_header_size = coff.size_of_optional_header as usize;
    let section_off = opt_off + opt_header_size;

    Ok((coff, opt, section_off))
}

/// Extract section headers from raw PE data.
pub fn section_headers<'a>(
    data: &'a [u8],
    section_off: usize,
    count: u16,
) -> Result<&'a [SectionHeader], &'static str> {
    let entry_size = core::mem::size_of::<SectionHeader>();
    let total = (count as usize)
        .checked_mul(entry_size)
        .ok_or("pe_loader: section header table size overflow")?;
    let end = section_off
        .checked_add(total)
        .ok_or("pe_loader: section header table offset overflow")?;

    if end > data.len() {
        return Err("pe_loader: section headers extend beyond PE data");
    }

    let ptr = unsafe { data.as_ptr().add(section_off) as *const SectionHeader };
    Ok(unsafe { core::slice::from_raw_parts(ptr, count as usize) })
}

// ---------------------------------------------------------------------------
// Loader
// ---------------------------------------------------------------------------

/// Load a PE32+ binary from raw bytes into virtual memory.
///
/// For each section this function:
/// 1. Allocates virtual pages via [`memory::create_mapping`].
/// 2. Copies the raw data portion (`SizeOfRawData` bytes).
/// 3. Zeros the remainder up to `VirtualSize` (BSS-like regions).
///
/// Returns the loaded PE with entry point and owned page mappings.
pub fn load(data: &[u8]) -> Result<LoadedPe, &'static str> {
    let (coff, opt, section_off) = parse_header(data)?;
    let num_sections = coff.number_of_sections;
    let sections_raw = section_headers(data, section_off, num_sections)?;
    let mut sections = Vec::new();

    let image_base = opt.image_base;
    let entry_rva = opt.address_of_entry_point;

    debug!(
        "pe_loader: PE32+ image_base={:#x} entry_rva={:#x} sections={}",
        image_base, entry_rva, num_sections
    );

    for shdr in sections_raw {
        let vsize = shdr.virtual_size as usize;
        let raw_size = shdr.size_of_raw_data as usize;
        let raw_offset = shdr.pointer_to_raw_data as usize;
        let chars = shdr.characteristics;
        let section_vaddr = shdr.virtual_address as usize;

        if vsize == 0 && raw_size == 0 {
            continue;
        }

        let section_size = vsize.max(raw_size);
        if section_size == 0 {
            continue;
        }

        debug!(
            "pe_loader: section '{}' vaddr={:#x} vsize={:#x} raw_off={:#x} raw_size={:#x} chars={:#x}",
            shdr.name_str(), section_vaddr, vsize, raw_offset, raw_size, chars
        );

        // Skip sections that have no memory representation
        if chars & (section_flags::IMAGE_SCN_CNT_CODE
            | section_flags::IMAGE_SCN_CNT_INITIALIZED_DATA
            | section_flags::IMAGE_SCN_CNT_UNINITIALIZED_DATA
            | section_flags::IMAGE_SCN_MEM_READ
            | section_flags::IMAGE_SCN_MEM_WRITE
            | section_flags::IMAGE_SCN_MEM_EXECUTE) == 0
        {
            debug!("pe_loader: skipping section '{}' (no memory flags)", shdr.name_str());
            continue;
        }

        // Validate raw data bounds
        if raw_size > 0 {
            let raw_end = raw_offset
                .checked_add(raw_size)
                .ok_or("pe_loader: section raw data offset + size overflow")?;
            if raw_end > data.len() {
                error!(
                    "pe_loader: section '{}' raw data [{:#x}..{:#x}) exceeds input size {:#x}",
                    shdr.name_str(), raw_offset, raw_end, data.len()
                );
                return Err("pe_loader: section raw data exceeds PE input bounds");
            }
        }

        // Allocate pages (writable initially for copying)
        let _final_flags = pe_section_flags_to_pte(chars);
        let write_flags = PteFlagsArch::new().valid(true).writable(true);

        let page_offset = 0; // PE sections are page-aligned by convention
        let alloc_size = section_size;

        let mut mapped = memory::create_mapping(alloc_size, write_flags)?;

        // Copy raw data and zero BSS
        {
            let dest: &mut [u8] = mapped.as_slice_mut(page_offset, section_size)
                .map_err(|_| "pe_loader: failed to obtain mutable slice for section")?;

            if raw_size > 0 {
                let copy_len = raw_size.min(section_size);
                dest[..copy_len].copy_from_slice(&data[raw_offset..raw_offset + copy_len]);
            }

            // Zero uninitialized portion
            let zeroed_start = raw_size.min(section_size);
            if zeroed_start < section_size {
                for byte in &mut dest[zeroed_start..] {
                    *byte = 0;
                }
            }
        }

        sections.push(mapped);
    }

    if sections.is_empty() {
        return Err("pe_loader: no loadable sections found in PE binary");
    }

    // The entry point is ImageBase + AddressOfEntryPoint.
    // Since we load at arbitrary addresses (not at ImageBase), we need
    // to use the first section's actual mapped address as a base.
    // For now, compute the nominal entry point — the caller must handle
    // relocation if ImageBase differs from the actual load address.
    let entry_vaddr = image_base.wrapping_add(entry_rva as u64) as usize;
    let entry = VirtualAddress::new_canonical(entry_vaddr);

    debug!(
        "pe_loader: loaded {} sections, entry point at {:#x}",
        sections.len(),
        entry.value()
    );

    Ok(LoadedPe {
        entry_point: entry,
        image_base,
        sections,
    })
}

/// Validate that the given data is a PE32+ binary without loading it.
pub fn is_pe(data: &[u8]) -> bool {
    parse_header(data).is_ok()
}
