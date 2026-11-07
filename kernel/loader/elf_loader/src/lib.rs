//! ELF64 binary loader for MaiOS.
//!
//! This crate parses ELF64 headers and loads PT_LOAD segments into virtual
//! memory (at their link-time addresses for ET_EXEC binaries), returning the
//! entry point address and the [`MappedPages`] that keep the binary alive.
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
use memory::{MappedPages, Page};
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
    /// Owned page mappings of the loaded image (one per run of pages sharing
    /// the same permissions). Dropping these will unmap the binary from memory.
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

/// A validated `PT_LOAD` segment.
struct LoadSegment {
    vaddr: usize,
    memsz: usize,
    filesz: usize,
    offset: usize,
    flags: u32,
}

#[inline]
fn align_down(v: usize) -> usize { v & !(PAGE_SIZE - 1) }

#[inline]
fn align_up(v: usize) -> Option<usize> {
    v.checked_add(PAGE_SIZE - 1).map(align_down)
}

/// Load an ELF64 binary from raw bytes into virtual memory.
///
/// All `PT_LOAD` segments are placed in **one contiguous virtual range**, so the
/// distances between segments chosen by the linker are preserved:
/// - `ET_EXEC` binaries (e.g. Doom, linked at `0x400000`) are mapped at their exact
///   link-time addresses, so `e_entry` and every absolute address stay valid;
/// - `ET_DYN` binaries are placed anywhere and the entry point is rebased
///   (no dynamic relocations are applied: only position-independent code works).
///
/// The whole range is zeroed, each segment's file data is copied in (the rest up to
/// `p_memsz` stays zero, i.e. the BSS), then every page is remapped with the
/// permissions of the segment(s) that cover it: executable for `PF_X`, writable
/// for `PF_W`, read-only otherwise.
///
/// # Errors
///
/// Returns a static error string if:
/// - The ELF header is invalid or unsupported.
/// - A segment's file data extends beyond the provided `data` slice.
/// - The binary's address range is already in use, or page allocation/mapping fails.
pub fn load(data: &[u8]) -> Result<LoadedElf, &'static str> {
    let header = parse_header(data)?;
    let phdrs = program_headers(data, header)?;

    // 1. Validate the PT_LOAD segments and compute the page-aligned span they cover.
    let mut segs: Vec<LoadSegment> = Vec::new();
    let mut span_lo = usize::MAX;
    let mut span_hi = 0usize;
    for phdr in phdrs {
        if phdr.p_type != PT_LOAD || phdr.p_memsz == 0 {
            continue;
        }
        let seg = LoadSegment {
            vaddr: phdr.p_vaddr as usize,
            memsz: phdr.p_memsz as usize,
            filesz: phdr.p_filesz as usize,
            offset: phdr.p_offset as usize,
            flags: phdr.p_flags,
        };
        debug!(
            "elf_loader: PT_LOAD vaddr={:#x} memsz={:#x} filesz={:#x} flags={:#x}",
            seg.vaddr, seg.memsz, seg.filesz, seg.flags
        );
        if seg.filesz > seg.memsz {
            return Err("elf_loader: segment p_filesz is larger than p_memsz");
        }
        let file_end = seg.offset
            .checked_add(seg.filesz)
            .ok_or("elf_loader: segment file offset + filesz overflow")?;
        if file_end > data.len() {
            error!(
                "elf_loader: segment file data [{:#x}..{:#x}) exceeds input size {:#x}",
                seg.offset, file_end, data.len()
            );
            return Err("elf_loader: segment file data exceeds ELF input bounds");
        }
        let seg_end = seg.vaddr
            .checked_add(seg.memsz)
            .and_then(align_up)
            .ok_or("elf_loader: segment address range overflow")?;
        span_lo = span_lo.min(align_down(seg.vaddr));
        span_hi = span_hi.max(seg_end);
        segs.push(seg);
    }
    if segs.is_empty() {
        return Err("elf_loader: no PT_LOAD segments found in ELF binary");
    }
    let span = span_hi - span_lo;

    // 2. Reserve the virtual pages: at the exact link address for ET_EXEC,
    //    anywhere for ET_DYN.
    let pages = if header.e_type == ET_EXEC {
        let at = VirtualAddress::new(span_lo)
            .ok_or("elf_loader: ET_EXEC load address is not canonical")?;
        memory::allocate_pages_by_bytes_at(at, span).map_err(|e| {
            error!(
                "elf_loader: cannot reserve [{:#x}..{:#x}) for ET_EXEC binary: {}",
                span_lo, span_hi, e
            );
            "elf_loader: the binary's link address range is already in use"
        })?
    } else {
        memory::allocate_pages_by_bytes(span)
            .ok_or("elf_loader: couldn't allocate pages for ET_DYN binary")?
    };
    // Difference between where the image lives and where it was linked (0 for ET_EXEC).
    let load_bias = pages.start_address().value().wrapping_sub(span_lo);

    // 3. Map writable, zero everything, copy the file-backed parts.
    let kernel_mmi = memory::get_kernel_mmi_ref()
        .ok_or("elf_loader: kernel MMI is not initialized")?;
    let mut image = kernel_mmi.lock().page_table
        .map_allocated_pages(pages, PteFlagsArch::new().valid(true).writable(true))?;
    {
        let dest: &mut [u8] = image.as_slice_mut(0, span)
            .map_err(|_| "elf_loader: failed to obtain mutable slice for the image")?;
        dest.fill(0);
        for seg in &segs {
            let off = seg.vaddr - span_lo;
            dest[off..off + seg.filesz]
                .copy_from_slice(&data[seg.offset..seg.offset + seg.filesz]);
        }
    }

    // 4. Final permissions, page by page: union of the flags of every segment
    //    touching the page (pages covered by no segment stay read-only).
    let num_pages = span / PAGE_SIZE;
    let mut page_flags: Vec<u32> = alloc::vec![PF_R; num_pages];
    for seg in &segs {
        let first = (align_down(seg.vaddr) - span_lo) / PAGE_SIZE;
        let last = (align_up(seg.vaddr + seg.memsz).unwrap_or(span_hi) - span_lo) / PAGE_SIZE;
        for f in &mut page_flags[first..last] {
            *f |= seg.flags;
        }
    }

    // Split the image into runs of pages sharing the same flags and remap each run.
    let mut segments = Vec::new();
    let mut rest = image;
    let mut run_start = 0usize;
    while run_start < num_pages {
        let flags = page_flags[run_start];
        let mut run_end = run_start + 1;
        while run_end < num_pages && page_flags[run_end] == flags {
            run_end += 1;
        }
        let mut piece = if run_end < num_pages {
            let split_at = Page::containing_address(
                rest.start_address() + (run_end - run_start) * PAGE_SIZE,
            );
            let (piece, tail) = rest.split(split_at)
                .map_err(|_| "elf_loader: failed to split the mapped image")?;
            rest = tail;
            piece
        } else {
            core::mem::replace(&mut rest, MappedPages::empty())
        };
        piece.remap(&mut kernel_mmi.lock().page_table, elf_flags_to_pte_flags(flags))?;
        segments.push(piece);
        run_start = run_end;
    }

    let entry = VirtualAddress::new(load_bias.wrapping_add(header.e_entry as usize))
        .ok_or("elf_loader: entry point is not a canonical address")?;

    debug!(
        "elf_loader: loaded {} PT_LOAD segments at [{:#x}..{:#x}) (bias {:#x}), entry point at {:#x}",
        segs.len(),
        span_lo.wrapping_add(load_bias),
        span_hi.wrapping_add(load_bias),
        load_bias,
        entry.value()
    );

    Ok(LoadedElf {
        entry_point: entry,
        segments,
    })
}
