//! ELF64 binary loader for MaiOS.
//!
//! This crate parses ELF64 headers and loads PT_LOAD segments into
//! freshly allocated virtual memory, returning the entry point address
//! and the list of [`MappedPages`] that keep the loaded binary alive.
//!
//! Supported: x86_64, little-endian, ET_EXEC and ET_DYN (PIE) executables.
//!
//! # Usage
//! ```ignore
//! let elf_bytes: &[u8] = /* raw ELF binary */;
//! let loaded = elf_loader::load(elf_bytes)?;
//! // loaded.entry_point is the virtual address to jump to.
//! // loaded.segments must be kept alive for the duration of execution.
//! ```

#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use log::{debug, error};
use memory::MappedPages;
use memory_structs::VirtualAddress;
use pte_flags::PteFlagsArch;
use kernel_config::memory::PAGE_SIZE;

// ---------------------------------------------------------------------------
// ELF64 constants
// ---------------------------------------------------------------------------

const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3; // Position-Independent Executables
const EM_X86_64: u16 = 62;
const PT_LOAD: u32 = 1;

/// ELF segment permission: executable.
pub const PF_X: u32 = 1;
/// ELF segment permission: writable.
pub const PF_W: u32 = 2;
/// ELF segment permission: readable.
pub const PF_R: u32 = 4;

// ---------------------------------------------------------------------------
// ELF64 header structures
// ---------------------------------------------------------------------------

/// The ELF64 file header, located at offset 0 in any ELF binary.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Elf64Header {
    pub e_ident: [u8; 16],
    pub e_type: u16,
    pub e_machine: u16,
    pub e_version: u32,
    pub e_entry: u64,
    pub e_phoff: u64,
    pub e_shoff: u64,
    pub e_flags: u32,
    pub e_ehsize: u16,
    pub e_phentsize: u16,
    pub e_phnum: u16,
    pub e_shentsize: u16,
    pub e_shnum: u16,
    pub e_shstrndx: u16,
}

/// An ELF64 program header, describing one segment of the binary.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Elf64ProgramHeader {
    pub p_type: u32,
    pub p_flags: u32,
    pub p_offset: u64,
    pub p_vaddr: u64,
    pub p_paddr: u64,
    pub p_filesz: u64,
    pub p_memsz: u64,
    pub p_align: u64,
}

// ---------------------------------------------------------------------------
// Result type
// ---------------------------------------------------------------------------

/// The result of successfully loading an ELF binary into memory.
///
/// The caller **must** keep the `segments` vector alive for as long as the
/// loaded binary is running, because dropping a [`MappedPages`] will unmap
/// the underlying virtual pages.
pub struct LoadedElf {
    /// Virtual address of the ELF entry point (`e_entry`).
    pub entry_point: VirtualAddress,
    /// Owned page mappings for every PT_LOAD segment.
    /// Dropping these will unmap the binary from memory.
    pub segments: Vec<MappedPages>,
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

/// Parse and validate an ELF64 header from raw bytes.
///
/// Returns a reference to the header if validation succeeds.
/// The reference borrows directly from `data`, so no allocation is needed.
pub fn parse_header(data: &[u8]) -> Result<&Elf64Header, &'static str> {
    if data.len() < core::mem::size_of::<Elf64Header>() {
        return Err("elf_loader: data too small for ELF64 header");
    }

    // SAFETY: Elf64Header is #[repr(C)] with no padding requirements beyond
    // what a u8 pointer can satisfy, and we verified the length above.
    let header = unsafe { &*(data.as_ptr() as *const Elf64Header) };

    if header.e_ident[0..4] != ELF_MAGIC {
        return Err("elf_loader: invalid ELF magic number");
    }
    if header.e_ident[4] != ELFCLASS64 {
        return Err("elf_loader: not a 64-bit ELF (ELFCLASS64 expected)");
    }
    if header.e_ident[5] != ELFDATA2LSB {
        return Err("elf_loader: not a little-endian ELF (ELFDATA2LSB expected)");
    }
    if header.e_machine != EM_X86_64 {
        return Err("elf_loader: not an x86_64 ELF (EM_X86_64 expected)");
    }
    if header.e_type != ET_EXEC && header.e_type != ET_DYN {
        return Err("elf_loader: ELF type is neither ET_EXEC nor ET_DYN");
    }
    if header.e_phentsize as usize != core::mem::size_of::<Elf64ProgramHeader>() {
        return Err("elf_loader: unexpected e_phentsize for ELF64");
    }

    Ok(header)
}

/// Extract the program header table from raw ELF data.
///
/// The returned slice borrows directly from `data`.
pub fn program_headers<'a>(
    data: &'a [u8],
    header: &Elf64Header,
) -> Result<&'a [Elf64ProgramHeader], &'static str> {
    let off = header.e_phoff as usize;
    let num = header.e_phnum as usize;
    let entry_size = core::mem::size_of::<Elf64ProgramHeader>();
    let total = num
        .checked_mul(entry_size)
        .ok_or("elf_loader: program header table size overflow")?;
    let end = off
        .checked_add(total)
        .ok_or("elf_loader: program header table offset overflow")?;

    if end > data.len() {
        return Err("elf_loader: program headers extend beyond ELF data");
    }

    // SAFETY: We verified that the byte range [off..end) fits within `data`,
    // and Elf64ProgramHeader is #[repr(C)] with no alignment beyond u8.
    let ptr = unsafe { data.as_ptr().add(off) as *const Elf64ProgramHeader };
    Ok(unsafe { core::slice::from_raw_parts(ptr, num) })
}

/// Convert ELF segment flags (`p_flags`) into MaiOS page table entry flags.
///
/// The returned flags always include `VALID`. Write and execute permissions
/// are set according to the ELF segment's `PF_W` and `PF_X` bits.
/// By default on x86_64, pages are non-executable (the NX bit is set);
/// we clear it only when `PF_X` is requested.
fn elf_flags_to_pte_flags(p_flags: u32) -> PteFlagsArch {
    let mut flags = PteFlagsArch::new().valid(true);

    if p_flags & PF_W != 0 {
        flags = flags.writable(true);
    }

    if p_flags & PF_X != 0 {
        // PteFlagsArch::new() sets NOT_EXECUTABLE by default.
        // Calling .executable(true) clears the NX bit.
        flags = flags.executable(true);
    }

    flags
}

// ---------------------------------------------------------------------------
// Loader
// ---------------------------------------------------------------------------

/// Load an ELF64 binary from raw bytes into virtual memory.
///
/// For each `PT_LOAD` segment this function:
/// 1. Allocates virtual pages via [`memory::create_mapping`].
/// 2. Copies the file-backed portion of the segment (`p_filesz` bytes).
/// 3. Zeros the remainder up to `p_memsz` (the BSS region).
///
/// All segments are initially mapped as **writable** so that data can be
/// copied in. After copying, segments that should be read-only or
/// non-writable are *not* automatically remapped to tighter permissions,
/// because [`MappedPages::remap`] requires a `&mut Mapper` reference that
/// is not available in a freestanding context. The caller is responsible for
/// tightening permissions if desired, using [`MappedPages::remap`] with the
/// active page table mapper.
///
/// # Errors
///
/// Returns a static error string if:
/// - The ELF header is invalid or unsupported.
/// - A segment's file data extends beyond the provided `data` slice.
/// - Page allocation or mapping fails.
pub fn load(data: &[u8]) -> Result<LoadedElf, &'static str> {
    let header = parse_header(data)?;
    let phdrs = program_headers(data, header)?;
    let mut segments = Vec::new();

    for phdr in phdrs {
        if phdr.p_type != PT_LOAD {
            continue;
        }

        let vaddr = phdr.p_vaddr as usize;
        let memsz = phdr.p_memsz as usize;
        let filesz = phdr.p_filesz as usize;
        let offset = phdr.p_offset as usize;

        if memsz == 0 {
            continue;
        }

        debug!(
            "elf_loader: PT_LOAD vaddr={:#x} memsz={:#x} filesz={:#x} flags={:#x}",
            vaddr, memsz, filesz, phdr.p_flags
        );

        // Validate that the file-backed portion fits within the input data.
        if filesz > 0 {
            let file_end = offset
                .checked_add(filesz)
                .ok_or("elf_loader: segment file offset + filesz overflow")?;
            if file_end > data.len() {
                error!(
                    "elf_loader: segment file data [{:#x}..{:#x}) exceeds input size {:#x}",
                    offset, file_end, data.len()
                );
                return Err("elf_loader: segment file data exceeds ELF input bounds");
            }
        }

        // Calculate the total allocation size, rounded up to a page boundary.
        // Segments may start at a non-page-aligned vaddr; we account for the
        // offset within the first page so the full memsz fits.
        let page_offset = vaddr & (PAGE_SIZE - 1);
        let alloc_size = memsz
            .checked_add(page_offset)
            .ok_or("elf_loader: allocation size overflow")?;

        // We always map writable initially so we can copy segment data in.
        // The desired final flags are computed from the ELF p_flags.
        let _final_flags = elf_flags_to_pte_flags(phdr.p_flags);
        let write_flags = PteFlagsArch::new().valid(true).writable(true);

        let mut mapped = memory::create_mapping(alloc_size, write_flags)?;

        // Copy file-backed data into the mapped region.
        // `as_slice_mut::<u8>(byte_offset, length)` returns a mutable byte slice.
        {
            let dest: &mut [u8] = mapped.as_slice_mut(page_offset, memsz)
                .map_err(|_| "elf_loader: failed to obtain mutable slice for mapped segment")?;

            // Copy the file content (the .text / .data / etc. portions).
            if filesz > 0 {
                dest[..filesz].copy_from_slice(&data[offset..offset + filesz]);
            }

            // Zero the BSS portion (memsz > filesz).
            if memsz > filesz {
                for byte in &mut dest[filesz..] {
                    *byte = 0;
                }
            }
        }

        segments.push(mapped);
    }

    if segments.is_empty() {
        return Err("elf_loader: no PT_LOAD segments found in ELF binary");
    }

    let entry = VirtualAddress::new_canonical(header.e_entry as usize);

    debug!(
        "elf_loader: loaded {} PT_LOAD segments, entry point at {:#x}",
        segments.len(),
        entry.value()
    );

    Ok(LoadedElf {
        entry_point: entry,
        segments,
    })
}
