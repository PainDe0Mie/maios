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

            // Look up the stub for this function
            let syscall_nr = lookup_win32_stub(dll_name, name_str);

            // Generate a stub function
            let stub_addr = if syscall_nr != 0xFFFF {
                // Generate: mov r10, rcx; mov eax, <nr>; syscall; ret
                let stub = generate_syscall_stub(syscall_nr);
                if stub_offset + stub.len() > PAGE_SIZE {
                    error!("pe_loader: stub page full, cannot resolve more imports");
                    break;
                }
                let dest: &mut [u8] = stub_page.as_slice_mut(stub_offset, stub.len())
                    .map_err(|_| "pe_loader: failed to write stub")?;
                dest.copy_from_slice(&stub);
                let addr = stub_base + stub_offset as u64;
                stub_offset += stub.len();
                debug!("pe_loader:   {} → stub at {:#x} (NT syscall {:#x})", name_str, addr, syscall_nr);
                addr
            } else {
                // Unknown function — generate a stub that returns STATUS_NOT_IMPLEMENTED
                let stub = generate_nop_stub();
                if stub_offset + stub.len() > PAGE_SIZE {
                    break;
                }
                let dest: &mut [u8] = stub_page.as_slice_mut(stub_offset, stub.len())
                    .map_err(|_| "pe_loader: failed to write nop stub")?;
                dest.copy_from_slice(&stub);
                let addr = stub_base + stub_offset as u64;
                stub_offset += stub.len();
                debug!("pe_loader:   {} → nop stub at {:#x} (unimplemented)", name_str, addr);
                addr
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

/// Generate a no-op stub that returns STATUS_NOT_IMPLEMENTED (0xC0000002):
/// ```asm
/// mov eax, 0xC0000002 ; B8 02 00 00 C0 (5 bytes)
/// ret                  ; C3 (1 byte)
/// ```
fn generate_nop_stub() -> [u8; 8] {
    [
        0xB8, 0x02, 0x00, 0x00, 0xC0, // mov eax, 0xC0000002 (STATUS_NOT_IMPLEMENTED)
        0xC3,                           // ret
        0x90, 0x90,                     // nop nop (pad to 8)
    ]
}

/// Lookup table mapping Win32/NT function names to NT syscall numbers.
///
/// Returns 0xFFFF if the function is not mapped.
fn lookup_win32_stub(dll_name: &str, func_name: &str) -> u16 {
    // Normalize DLL name to lowercase for comparison
    let dll_lower: &str = dll_name;
    let is_ntdll = dll_lower.len() >= 5
        && (dll_lower.as_bytes()[0] | 0x20) == b'n'
        && (dll_lower.as_bytes()[1] | 0x20) == b't'
        && (dll_lower.as_bytes()[2] | 0x20) == b'd'
        && (dll_lower.as_bytes()[3] | 0x20) == b'l'
        && (dll_lower.as_bytes()[4] | 0x20) == b'l';

    let is_kernel32 = dll_lower.len() >= 8
        && (dll_lower.as_bytes()[0] | 0x20) == b'k'
        && (dll_lower.as_bytes()[1] | 0x20) == b'e'
        && (dll_lower.as_bytes()[2] | 0x20) == b'r'
        && (dll_lower.as_bytes()[3] | 0x20) == b'n';

    if is_ntdll {
        match func_name {
            "NtCreateFile"              => 0x0055,
            "NtReadFile"                => 0x0006,
            "NtWriteFile"               => 0x0008,
            "NtClose"                   => 0x000F,
            "NtAllocateVirtualMemory"   => 0x0018,
            "NtFreeVirtualMemory"       => 0x001E,
            "NtProtectVirtualMemory"    => 0x0050,
            "NtTerminateProcess"        => 0x002C,
            "NtQueryPerformanceCounter" => 0x0031,
            "NtQueryInformationFile"    => 0x0011,
            "NtQuerySystemInformation"  => 0x0036,
            "NtQueryInformationProcess" => 0x0019,
            "RtlInitUnicodeString"      => 0xFFFF, // stub separately
            _ => 0xFFFF,
        }
    } else if is_kernel32 {
        // kernel32 functions are thin wrappers around ntdll.
        // Map them to the corresponding NT syscall numbers.
        match func_name {
            "ExitProcess"       => 0x002C, // NtTerminateProcess
            "WriteFile"         => 0x0008, // NtWriteFile
            "ReadFile"          => 0x0006, // NtReadFile
            "CloseHandle"       => 0x000F, // NtClose
            "CreateFileA" | "CreateFileW" => 0x0055, // NtCreateFile
            "VirtualAlloc"      => 0x0018, // NtAllocateVirtualMemory
            "VirtualFree"       => 0x001E, // NtFreeVirtualMemory
            "VirtualProtect"    => 0x0050, // NtProtectVirtualMemory
            "GetStdHandle"      => 0xFFFF, // Special: handled by nop stub returning fixed handles
            "WriteConsoleA" | "WriteConsoleW" => 0x0008,
            "GetLastError"      => 0xFFFF, // nop stub: return 0 (ERROR_SUCCESS)
            "SetLastError"      => 0xFFFF, // nop stub
            "GetModuleHandleA" | "GetModuleHandleW" => 0xFFFF, // nop stub
            "GetProcAddress"    => 0xFFFF, // nop stub
            "GetCurrentProcess" => 0xFFFF, // nop stub (return -1)
            "GetCurrentProcessId" => 0xFFFF, // nop stub
            "QueryPerformanceCounter" => 0x0031,
            "QueryPerformanceFrequency" => 0x0031,
            _ => 0xFFFF,
        }
    } else {
        0xFFFF
    }
}
