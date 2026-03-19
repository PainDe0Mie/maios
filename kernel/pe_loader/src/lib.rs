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
/// Allocates a single contiguous mapping for the entire image (SizeOfImage),
/// then copies each section to its correct RVA offset. This ensures that
/// RVA-based references (imports, relocations) work correctly.
///
/// After loading, applies base relocations if the actual load address
/// differs from the preferred ImageBase.
///
/// Returns the loaded PE with entry point and owned page mappings.
pub fn load(data: &[u8]) -> Result<LoadedPe, &'static str> {
    let (coff, opt, section_off) = parse_header(data)?;
    let num_sections = coff.number_of_sections;
    let sections_raw = section_headers(data, section_off, num_sections)?;

    let preferred_base = opt.image_base;
    let entry_rva = opt.address_of_entry_point;
    let size_of_image = opt.size_of_image as usize;
    let size_of_headers = opt.size_of_headers as usize;

    debug!(
        "pe_loader: PE32+ preferred_base={:#x} entry_rva={:#x} size_of_image={:#x} sections={}",
        preferred_base, entry_rva, size_of_image, num_sections
    );

    if size_of_image == 0 {
        return Err("pe_loader: SizeOfImage is zero");
    }

    // Allocate a single contiguous block for the entire image.
    // writable + executable so all sections can be accessed (we don't
    // enforce per-section permissions yet).
    let flags = PteFlagsArch::new().valid(true).writable(true).executable(true);
    let mut image_mapping = memory::create_mapping(size_of_image, flags)?;

    // Get the actual base address where the image was loaded
    let actual_base = {
        let slice: &[u8] = image_mapping.as_slice(0, 1)
            .map_err(|_| "pe_loader: failed to read image mapping base address")?;
        slice.as_ptr() as u64
    };

    debug!("pe_loader: image loaded at actual_base={:#x} (preferred={:#x})", actual_base, preferred_base);

    // Zero the entire image first
    {
        let dest: &mut [u8] = image_mapping.as_slice_mut(0, size_of_image)
            .map_err(|_| "pe_loader: failed to get mutable slice for image")?;
        for b in dest.iter_mut() { *b = 0; }
    }

    // Copy PE headers
    {
        let hdr_copy_len = size_of_headers.min(data.len()).min(size_of_image);
        let dest: &mut [u8] = image_mapping.as_slice_mut(0, hdr_copy_len)
            .map_err(|_| "pe_loader: failed to write headers")?;
        dest.copy_from_slice(&data[..hdr_copy_len]);
    }

    // Copy each section to its RVA offset within the image
    for shdr in sections_raw {
        let vsize = shdr.virtual_size as usize;
        let raw_size = shdr.size_of_raw_data as usize;
        let raw_offset = shdr.pointer_to_raw_data as usize;
        let section_rva = shdr.virtual_address as usize;
        let chars = shdr.characteristics;

        if vsize == 0 && raw_size == 0 { continue; }

        let section_size = vsize.max(raw_size);
        if section_size == 0 { continue; }

        // Skip non-loadable sections
        if chars & (section_flags::IMAGE_SCN_CNT_CODE
            | section_flags::IMAGE_SCN_CNT_INITIALIZED_DATA
            | section_flags::IMAGE_SCN_CNT_UNINITIALIZED_DATA
            | section_flags::IMAGE_SCN_MEM_READ
            | section_flags::IMAGE_SCN_MEM_WRITE
            | section_flags::IMAGE_SCN_MEM_EXECUTE) == 0
        {
            continue;
        }

        // Bounds check
        if section_rva + section_size > size_of_image {
            error!("pe_loader: section '{}' extends beyond SizeOfImage", shdr.name_str());
            continue;
        }

        debug!(
            "pe_loader: section '{}' rva={:#x} vsize={:#x} raw_off={:#x} raw_size={:#x}",
            shdr.name_str(), section_rva, vsize, raw_offset, raw_size
        );

        // Copy raw data to the correct RVA offset
        if raw_size > 0 && raw_offset + raw_size <= data.len() {
            let copy_len = raw_size.min(section_size);
            let dest: &mut [u8] = image_mapping.as_slice_mut(section_rva, copy_len)
                .map_err(|_| "pe_loader: failed to write section data")?;
            dest.copy_from_slice(&data[raw_offset..raw_offset + copy_len]);
        }
        // BSS portion is already zeroed from the initial memset
    }

    // Apply base relocations if loaded at a different address than preferred
    let delta = actual_base as i64 - preferred_base as i64;
    if delta != 0 {
        debug!("pe_loader: applying base relocations (delta={:#x})", delta);
        apply_base_relocations(&mut image_mapping, data, opt, delta)?;
    }

    // Compute entry point using the actual load address
    let entry_va = actual_base.wrapping_add(entry_rva as u64) as usize;
    let entry = VirtualAddress::new_canonical(entry_va);

    debug!(
        "pe_loader: loaded at {:#x}, entry point at {:#x}",
        actual_base, entry.value()
    );

    Ok(LoadedPe {
        entry_point: entry,
        image_base: actual_base,
        sections: alloc::vec![image_mapping],
    })
}

/// Apply PE base relocations (.reloc section).
///
/// The relocation table is found via Data Directory entry #5 (IMAGE_DIRECTORY_ENTRY_BASERELOC).
/// Each block contains a page RVA and a list of relocation entries.
fn apply_base_relocations(
    image: &mut MappedPages,
    _data: &[u8],
    opt: &OptionalHeader64,
    delta: i64,
) -> Result<(), &'static str> {
    let num_dd = opt.number_of_rva_and_sizes as usize;
    if num_dd < 6 {
        debug!("pe_loader: no relocation directory (only {} data dirs)", num_dd);
        return Ok(());
    }

    // Read the base relocation data directory (index 5)
    let opt_ptr = opt as *const OptionalHeader64 as usize;
    let dd_start = opt_ptr + core::mem::size_of::<OptionalHeader64>();
    let dd_entry_size = core::mem::size_of::<DataDirectory>();
    let reloc_dd = unsafe { &*((dd_start + 5 * dd_entry_size) as *const DataDirectory) };

    let reloc_rva = reloc_dd.rva as usize;
    let reloc_size = reloc_dd.size as usize;

    if reloc_rva == 0 || reloc_size == 0 {
        debug!("pe_loader: relocation table is empty");
        return Ok(());
    }

    debug!("pe_loader: reloc directory at RVA {:#x}, size {:#x}", reloc_rva, reloc_size);

    // The relocation data is now in our loaded image at offset reloc_rva
    let size_of_image = opt.size_of_image as usize;
    if reloc_rva + reloc_size > size_of_image {
        return Err("pe_loader: relocation table extends beyond image");
    }

    // We need to read from the image mapping and also write to it.
    // First, read all relocation data into a temporary buffer.
    let reloc_data = {
        let slice: &[u8] = image.as_slice(reloc_rva, reloc_size)
            .map_err(|_| "pe_loader: failed to read reloc data")?;
        let mut buf = alloc::vec![0u8; reloc_size];
        buf.copy_from_slice(slice);
        buf
    };

    // Process relocation blocks
    let mut offset = 0;
    while offset + 8 <= reloc_data.len() {
        let page_rva = u32::from_le_bytes([
            reloc_data[offset], reloc_data[offset+1],
            reloc_data[offset+2], reloc_data[offset+3],
        ]) as usize;
        let block_size = u32::from_le_bytes([
            reloc_data[offset+4], reloc_data[offset+5],
            reloc_data[offset+6], reloc_data[offset+7],
        ]) as usize;

        if block_size == 0 { break; }
        if block_size < 8 { break; }

        let num_entries = (block_size - 8) / 2;

        for i in 0..num_entries {
            let entry_off = offset + 8 + i * 2;
            if entry_off + 2 > reloc_data.len() { break; }

            let entry = u16::from_le_bytes([reloc_data[entry_off], reloc_data[entry_off + 1]]);
            let reloc_type = (entry >> 12) & 0xF;
            let reloc_offset = (entry & 0xFFF) as usize;

            match reloc_type {
                0 => {} // IMAGE_REL_BASED_ABSOLUTE — padding, skip
                3 => {
                    // IMAGE_REL_BASED_HIGHLOW (32-bit)
                    let fixup_rva = page_rva + reloc_offset;
                    if fixup_rva + 4 <= size_of_image {
                        let slice: &mut [u8] = image.as_slice_mut(fixup_rva, 4)
                            .map_err(|_| "pe_loader: reloc fixup write failed")?;
                        let val = u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]);
                        let new_val = (val as i64).wrapping_add(delta) as u32;
                        slice.copy_from_slice(&new_val.to_le_bytes());
                    }
                }
                10 => {
                    // IMAGE_REL_BASED_DIR64 (64-bit) — most common for PE32+
                    let fixup_rva = page_rva + reloc_offset;
                    if fixup_rva + 8 <= size_of_image {
                        let slice: &mut [u8] = image.as_slice_mut(fixup_rva, 8)
                            .map_err(|_| "pe_loader: reloc fixup write failed")?;
                        let val = u64::from_le_bytes([
                            slice[0], slice[1], slice[2], slice[3],
                            slice[4], slice[5], slice[6], slice[7],
                        ]);
                        let new_val = (val as i64).wrapping_add(delta) as u64;
                        slice.copy_from_slice(&new_val.to_le_bytes());
                    }
                }
                _ => {
                    debug!("pe_loader: unsupported relocation type {}", reloc_type);
                }
            }
        }

        offset += block_size;
    }

    debug!("pe_loader: base relocations applied successfully");
    Ok(())
}

/// Validate that the given data is a PE32+ binary without loading it.
pub fn is_pe(data: &[u8]) -> bool {
    parse_header(data).is_ok()
}

// ---------------------------------------------------------------------------
// Import resolution
// ---------------------------------------------------------------------------

/// Data Directory entry (RVA + Size), 8 bytes each.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
struct DataDirectory {
    rva: u32,
    size: u32,
}

/// IMAGE_IMPORT_DESCRIPTOR (20 bytes each).
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
struct ImportDescriptor {
    /// RVA to the Import Lookup Table (ILT) — original thunk array.
    original_first_thunk: u32,
    time_date_stamp: u32,
    forwarder_chain: u32,
    /// RVA to the DLL name (null-terminated ASCII).
    name_rva: u32,
    /// RVA to the Import Address Table (IAT) — thunk array to patch.
    first_thunk: u32,
}

/// Read a null-terminated ASCII string from PE data at a given RVA.
fn read_rva_string(data: &[u8], rva: u32) -> Option<&str> {
    let off = rva as usize;
    if off >= data.len() { return None; }
    let slice = &data[off..];
    let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len().min(256));
    core::str::from_utf8(&slice[..end]).ok()
}

/// Resolve PE imports by patching the IAT with stub function addresses.
///
/// For each imported function, generates a tiny x86_64 stub:
/// ```asm
/// mov r10, rcx      ; Windows x64 syscall convention
/// mov eax, <nr>     ; NT syscall number
/// syscall
/// ret
/// ```
///
/// The stubs are written to a single allocated page that must be kept alive
/// by the caller (returned as part of `LoadedPe::sections` in practice).
///
/// `image_base` is the nominal base address of the PE image — used to compute
/// RVA-to-VA translations. The caller must have loaded sections at this base
/// (or handle relocations).
pub fn resolve_imports(data: &[u8], image_base: u64) -> Result<MappedPages, &'static str> {
    let (_coff, opt, _section_off) = parse_header(data)?;

    // Data directories are immediately after the OptionalHeader64 fixed fields.
    // number_of_rva_and_sizes tells us how many entries there are.
    let num_dd = opt.number_of_rva_and_sizes as usize;
    if num_dd < 2 {
        debug!("pe_loader: no import directory (only {} data directories)", num_dd);
        // No import directory — nothing to resolve (static binary)
        let stub_page = memory::create_mapping(PAGE_SIZE, PteFlagsArch::new().valid(true).writable(true))?;
        return Ok(stub_page);
    }

    // Data directories start right after the OptionalHeader64 struct
    let opt_ptr = opt as *const OptionalHeader64 as usize;
    let dd_start = opt_ptr + core::mem::size_of::<OptionalHeader64>();
    let dd_entry_size = core::mem::size_of::<DataDirectory>();

    let import_dd = unsafe { &*((dd_start + 1 * dd_entry_size) as *const DataDirectory) };
    let import_rva = import_dd.rva as usize;
    let import_size = import_dd.size as usize;

    if import_rva == 0 || import_size == 0 {
        debug!("pe_loader: import directory is empty");
        let stub_page = memory::create_mapping(PAGE_SIZE, PteFlagsArch::new().valid(true).writable(true))?;
        return Ok(stub_page);
    }

    debug!("pe_loader: import directory at RVA {:#x}, size {:#x}", import_rva, import_size);

    // Allocate a page for stub functions (writable + executable)
    let stub_flags = PteFlagsArch::new().valid(true).writable(true).executable(true);
    let mut stub_page = memory::create_mapping(PAGE_SIZE, stub_flags)?;
    let stub_base = {
        let slice: &[u8] = stub_page.as_slice(0, 1)
            .map_err(|_| "pe_loader: failed to get stub page address")?;
        slice.as_ptr() as u64
    };
    let mut stub_offset: usize = 0;

    // Iterate through import descriptors
    let desc_size = core::mem::size_of::<ImportDescriptor>();
    let mut desc_off = import_rva;

    loop {
        if desc_off + desc_size > data.len() { break; }
        let desc = unsafe { &*(data.as_ptr().add(desc_off) as *const ImportDescriptor) };

        // Null descriptor terminates the list
        let ft = desc.first_thunk;
        let name_rva = desc.name_rva;
        if ft == 0 && name_rva == 0 {
            break;
        }

        let dll_name = read_rva_string(data, name_rva).unwrap_or("???");
        debug!("pe_loader: import DLL: \"{}\"", dll_name);

        // Walk the ILT (or IAT if ILT is missing) to enumerate imported functions
        let ilt_rva = if desc.original_first_thunk != 0 {
            desc.original_first_thunk as usize
        } else {
            ft as usize
        };
        let iat_rva = ft as usize;

        let mut thunk_idx = 0usize;
        loop {
            let ilt_off = ilt_rva + thunk_idx * 8;
            if ilt_off + 8 > data.len() { break; }

            let thunk_value = u64::from_le_bytes([
                data[ilt_off], data[ilt_off+1], data[ilt_off+2], data[ilt_off+3],
                data[ilt_off+4], data[ilt_off+5], data[ilt_off+6], data[ilt_off+7],
            ]);

            if thunk_value == 0 { break; } // Null terminator

            let func_name = if thunk_value & (1u64 << 63) != 0 {
                // Import by ordinal
                None
            } else {
                // Import by name: thunk_value is an RVA to IMAGE_IMPORT_BY_NAME
                // struct { u16 Hint; char Name[]; }
                let hint_rva = (thunk_value & 0x7FFF_FFFF) as u32;
                read_rva_string(data, hint_rva + 2) // +2 to skip Hint field
            };

            let name_str = func_name.unwrap_or("(ordinal)");

            // Look up the stub kind for this function
            let stub_kind = lookup_win32_stub(dll_name, name_str);

            // Generate the appropriate stub
            let stub_addr = match stub_kind {
                StubKind::Syscall(nr) => {
                    let stub = generate_syscall_stub(nr);
                    if stub_offset + stub.len() > PAGE_SIZE { break; }
                    let dest: &mut [u8] = stub_page.as_slice_mut(stub_offset, stub.len())
                        .map_err(|_| "pe_loader: failed to write stub")?;
                    dest.copy_from_slice(&stub);
                    let addr = stub_base + stub_offset as u64;
                    stub_offset += stub.len();
                    debug!("pe_loader:   {} → syscall stub {:#x}", name_str, nr);
                    addr
                }
                StubKind::ReturnValue(val) => {
                    let stub = generate_return_value_stub(val);
                    if stub_offset + stub.len() > PAGE_SIZE { break; }
                    let dest: &mut [u8] = stub_page.as_slice_mut(stub_offset, stub.len())
                        .map_err(|_| "pe_loader: failed to write return stub")?;
                    dest.copy_from_slice(&stub);
                    let addr = stub_base + stub_offset as u64;
                    stub_offset += stub.len();
                    debug!("pe_loader:   {} → return {:#x}", name_str, val);
                    addr
                }
                StubKind::Unknown => {
                    // Return 0 for unknown functions (safer than crashing)
                    let stub = generate_return_value_stub(0);
                    if stub_offset + stub.len() > PAGE_SIZE { break; }
                    let dest: &mut [u8] = stub_page.as_slice_mut(stub_offset, stub.len())
                        .map_err(|_| "pe_loader: failed to write unknown stub")?;
                    dest.copy_from_slice(&stub);
                    let addr = stub_base + stub_offset as u64;
                    stub_offset += stub.len();
                    debug!("pe_loader:   {} → unknown (ret 0)", name_str);
                    addr
                }
            };

            // Patch the IAT entry in the loaded image
            // IAT is at image_base + iat_rva + thunk_idx * 8
            let iat_va = image_base + iat_rva as u64 + (thunk_idx * 8) as u64;
            unsafe {
                *(iat_va as *mut u64) = stub_addr;
            }

            thunk_idx += 1;
        }

        desc_off += desc_size;
    }

    debug!("pe_loader: import resolution complete, {} bytes of stubs generated", stub_offset);
    Ok(stub_page)
}

/// Generate an x86_64 NT syscall stub (12 bytes):
/// ```asm
/// mov r10, rcx      ; 49 89 CA (3 bytes) — Windows x64 ABI: rcx → r10
/// mov eax, <nr>     ; B8 xx xx xx xx (5 bytes) — syscall number
/// syscall            ; 0F 05 (2 bytes)
/// ret                ; C3 (1 byte)
/// ; nop              ; 90 (1 byte, alignment)
/// ```
fn generate_syscall_stub(nr: u16) -> [u8; 12] {
    let nr32 = nr as u32;
    [
        0x49, 0x89, 0xCA,                               // mov r10, rcx
        0xB8, nr32 as u8, (nr32 >> 8) as u8, 0x00, 0x00, // mov eax, nr
        0x0F, 0x05,                                      // syscall
        0xC3,                                             // ret
        0x90,                                             // nop (pad to 12 bytes)
    ]
}

/// Generate a stub that returns a fixed 64-bit value in RAX.
/// ```asm
/// mov rax, <imm64>  ; 48 B8 xx xx xx xx xx xx xx xx (10 bytes)
/// ret                ; C3 (1 byte)
/// nop                ; 90 (pad to 12 bytes)
/// ```
fn generate_return_value_stub(value: u64) -> [u8; 12] {
    let b = value.to_le_bytes();
    [
        0x48, 0xB8, b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], // mov rax, imm64
        0xC3,                                                          // ret
        0x90,                                                          // nop
    ]
}

/// What kind of stub to generate for a Win32/NT function.
enum StubKind {
    /// NT syscall stub: mov r10,rcx; mov eax,nr; syscall; ret
    Syscall(u16),
    /// Return a fixed value: mov rax,value; ret
    ReturnValue(u64),
    /// Unknown/unimplemented function
    Unknown,
}

/// Lookup table mapping Win32/NT function names to stub kinds.
fn lookup_win32_stub(dll_name: &str, func_name: &str) -> StubKind {
    let dll = dll_name.as_bytes();
    let is_ntdll = dll.len() >= 5
        && (dll[0] | 0x20) == b'n'
        && (dll[1] | 0x20) == b't'
        && (dll[2] | 0x20) == b'd'
        && (dll[3] | 0x20) == b'l'
        && (dll[4] | 0x20) == b'l';

    let is_kernel32 = dll.len() >= 8
        && (dll[0] | 0x20) == b'k'
        && (dll[1] | 0x20) == b'e'
        && (dll[2] | 0x20) == b'r'
        && (dll[3] | 0x20) == b'n';

    if is_ntdll {
        match func_name {
            "NtCreateFile"              => StubKind::Syscall(0x0055),
            "NtReadFile"                => StubKind::Syscall(0x0006),
            "NtWriteFile"               => StubKind::Syscall(0x0008),
            "NtClose"                   => StubKind::Syscall(0x000F),
            "NtAllocateVirtualMemory"   => StubKind::Syscall(0x0018),
            "NtFreeVirtualMemory"       => StubKind::Syscall(0x001E),
            "NtProtectVirtualMemory"    => StubKind::Syscall(0x0050),
            "NtTerminateProcess"        => StubKind::Syscall(0x002C),
            "NtQueryPerformanceCounter" => StubKind::Syscall(0x0031),
            "NtQueryInformationFile"    => StubKind::Syscall(0x0011),
            "NtQuerySystemInformation"  => StubKind::Syscall(0x0036),
            "NtQueryInformationProcess" => StubKind::Syscall(0x0019),
            "RtlInitUnicodeString"      => StubKind::ReturnValue(0), // void function, return 0
            "RtlAllocateHeap"           => StubKind::ReturnValue(0), // return NULL (allocation failure)
            "RtlFreeHeap"               => StubKind::ReturnValue(1), // return TRUE
            _ => StubKind::Unknown,
        }
    } else if is_kernel32 {
        match func_name {
            "ExitProcess"       => StubKind::Syscall(0x002C),
            "WriteFile"         => StubKind::Syscall(0x0008),
            "ReadFile"          => StubKind::Syscall(0x0006),
            "CloseHandle"       => StubKind::Syscall(0x000F),
            "CreateFileA" | "CreateFileW" => StubKind::Syscall(0x0055),
            "VirtualAlloc"      => StubKind::Syscall(0x0018),
            "VirtualFree"       => StubKind::Syscall(0x001E),
            "VirtualProtect"    => StubKind::Syscall(0x0050),
            "WriteConsoleA" | "WriteConsoleW" => StubKind::Syscall(0x0008),
            "QueryPerformanceCounter"   => StubKind::Syscall(0x0031),
            "QueryPerformanceFrequency" => StubKind::Syscall(0x0031),
            // Win32 functions with fixed return values
            "GetStdHandle"      => StubKind::ReturnValue(0xFFFF_FFFF_FFFF_FFF5), // STD_OUTPUT_HANDLE=-11 → handle 7
            "GetLastError"      => StubKind::ReturnValue(0),   // ERROR_SUCCESS
            "SetLastError"      => StubKind::ReturnValue(0),   // void
            "GetCurrentProcess" => StubKind::ReturnValue(0xFFFF_FFFF_FFFF_FFFF), // pseudo-handle -1
            "GetCurrentProcessId" => StubKind::ReturnValue(1), // PID 1
            "GetCurrentThreadId"  => StubKind::ReturnValue(1), // TID 1
            "GetModuleHandleA" | "GetModuleHandleW" => StubKind::ReturnValue(0), // NULL (no module)
            "GetProcAddress"    => StubKind::ReturnValue(0),   // NULL (not found)
            "HeapCreate"        => StubKind::ReturnValue(0x1000), // fake heap handle
            "HeapAlloc"         => StubKind::ReturnValue(0),   // NULL (allocation failure)
            "HeapFree"          => StubKind::ReturnValue(1),   // TRUE
            "GetProcessHeap"    => StubKind::ReturnValue(0x1000), // fake heap handle
            "IsDebuggerPresent" => StubKind::ReturnValue(0),   // FALSE
            "GetCommandLineA"   => StubKind::ReturnValue(0),   // NULL
            "GetCommandLineW"   => StubKind::ReturnValue(0),   // NULL
            "GetSystemTimeAsFileTime" => StubKind::ReturnValue(0), // void
            "InitializeCriticalSectionAndSpinCount" => StubKind::ReturnValue(1), // TRUE
            "DeleteCriticalSection" => StubKind::ReturnValue(0), // void
            "EnterCriticalSection"  => StubKind::ReturnValue(0), // void
            "LeaveCriticalSection"  => StubKind::ReturnValue(0), // void
            "TlsAlloc"         => StubKind::ReturnValue(0),    // TLS index 0
            "TlsSetValue"      => StubKind::ReturnValue(1),    // TRUE
            "TlsGetValue"      => StubKind::ReturnValue(0),    // NULL
            "FlsAlloc"         => StubKind::ReturnValue(0),    // FLS index 0
            "FlsSetValue"      => StubKind::ReturnValue(1),    // TRUE
            "FlsGetValue"      => StubKind::ReturnValue(0),    // NULL
            "SetUnhandledExceptionFilter" => StubKind::ReturnValue(0), // NULL (previous filter)
            "UnhandledExceptionFilter"    => StubKind::ReturnValue(1), // EXCEPTION_EXECUTE_HANDLER
            "IsProcessorFeaturePresent"   => StubKind::ReturnValue(0), // FALSE
            _ => StubKind::Unknown,
        }
    } else {
        StubKind::Unknown
    }
}
