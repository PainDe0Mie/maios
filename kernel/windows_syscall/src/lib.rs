//! Windows NT syscall compatibility layer for MaiOS.
//!
//! Implements the Windows NT kernel syscall ABI, mapping NT system service
//! numbers to their MaiOS kernel equivalents. This allows Windows PE binaries
//! to run on MaiOS without a translation layer.
//!
//! ## NT Syscall Convention (x86_64)
//!
//! - RAX = syscall number (includes service table index in bits 12..13)
//! - Arguments: R10 (original RCX), RDX, R8, R9, then stack
//! - Return value: NTSTATUS in RAX
//!
//! ## NT Syscall Number Format
//!
//! ```text
//! Bits  0..11 : System service number within the table
//! Bits 12..13 : Service table index (0 = Nt*, 1 = Win32k)
//! ```

#![no_std]

extern crate alloc;

use alloc::collections::BTreeMap;
use log::{debug, warn, info};
use memory::MappedPages;
use spin::Mutex;

/// NT status codes.
pub mod ntstatus {
    pub const STATUS_SUCCESS: i64 = 0x0000_0000;
    pub const STATUS_NOT_IMPLEMENTED: i64 = 0xC000_0002_u32 as i32 as i64;
    pub const STATUS_INVALID_PARAMETER: i64 = 0xC000_000D_u32 as i32 as i64;
    pub const STATUS_ACCESS_DENIED: i64 = 0xC000_0022_u32 as i32 as i64;
    pub const STATUS_NO_MEMORY: i64 = 0xC000_0017_u32 as i32 as i64;
    pub const STATUS_INVALID_HANDLE: i64 = 0xC000_0008_u32 as i32 as i64;
    pub const STATUS_OBJECT_NAME_NOT_FOUND: i64 = 0xC000_0034_u32 as i32 as i64;
    pub const STATUS_BUFFER_TOO_SMALL: i64 = 0xC000_0023_u32 as i32 as i64;
}

/// Windows NT syscall numbers (Windows 10 21H2+ / build 19044+).
pub mod nr {
    // --- Process & Thread ---
    pub const NT_CLOSE: u64 = 0x000F;
    pub const NT_CREATE_PROCESS: u64 = 0x00B4;
    pub const NT_CREATE_THREAD: u64 = 0x004E;
    pub const NT_TERMINATE_PROCESS: u64 = 0x002C;
    pub const NT_TERMINATE_THREAD: u64 = 0x0053;
    pub const NT_QUERY_INFORMATION_PROCESS: u64 = 0x0019;
    pub const NT_QUERY_INFORMATION_THREAD: u64 = 0x0025;
    pub const NT_SET_INFORMATION_THREAD: u64 = 0x000D;

    // --- Memory ---
    pub const NT_ALLOCATE_VIRTUAL_MEMORY: u64 = 0x0018;
    pub const NT_FREE_VIRTUAL_MEMORY: u64 = 0x001E;
    pub const NT_PROTECT_VIRTUAL_MEMORY: u64 = 0x0050;
    pub const NT_QUERY_VIRTUAL_MEMORY: u64 = 0x0023;
    pub const NT_READ_VIRTUAL_MEMORY: u64 = 0x003F;
    pub const NT_WRITE_VIRTUAL_MEMORY: u64 = 0x003A;

    // --- File I/O ---
    pub const NT_CREATE_FILE: u64 = 0x0055;
    pub const NT_READ_FILE: u64 = 0x0006;
    pub const NT_WRITE_FILE: u64 = 0x0008;
    pub const NT_QUERY_INFORMATION_FILE: u64 = 0x0011;
    pub const NT_SET_INFORMATION_FILE: u64 = 0x0027;

    // --- Registry (stubs) ---
    pub const NT_OPEN_KEY: u64 = 0x0012;
    pub const NT_QUERY_VALUE_KEY: u64 = 0x0017;
    pub const NT_SET_VALUE_KEY: u64 = 0x0060;

    // --- Synchronization ---
    pub const NT_WAIT_FOR_SINGLE_OBJECT: u64 = 0x0004;
    pub const NT_SIGNAL_AND_WAIT_FOR_SINGLE_OBJECT: u64 = 0x001C;
    pub const NT_CREATE_EVENT: u64 = 0x0048;
    pub const NT_CREATE_MUTANT: u64 = 0x0076;

    // --- System info ---
    pub const NT_QUERY_SYSTEM_INFORMATION: u64 = 0x0036;
    pub const NT_QUERY_PERFORMANCE_COUNTER: u64 = 0x0031;
}

// =============================================================================
// Windows memory protection constants
// =============================================================================

#[allow(dead_code)]
mod mem_protect {
    pub const PAGE_NOACCESS: u64 = 0x01;
    pub const PAGE_READONLY: u64 = 0x02;
    pub const PAGE_READWRITE: u64 = 0x04;
    pub const PAGE_WRITECOPY: u64 = 0x08;
    pub const PAGE_EXECUTE: u64 = 0x10;
    pub const PAGE_EXECUTE_READ: u64 = 0x20;
    pub const PAGE_EXECUTE_READWRITE: u64 = 0x40;
}

mod mem_type {
    pub const MEM_COMMIT: u64 = 0x1000;
    pub const MEM_RESERVE: u64 = 0x2000;
    pub const MEM_DECOMMIT: u64 = 0x4000;
    pub const MEM_RELEASE: u64 = 0x8000;
}

/// Convert Windows PAGE_* protection flags to MaiOS PteFlags.
fn win_protect_to_pte_flags(protect: u64) -> pte_flags::PteFlags {
    let mut flags = pte_flags::PteFlags::new();
    match protect {
        mem_protect::PAGE_READWRITE | mem_protect::PAGE_WRITECOPY => {
            flags = flags.writable(true);
        }
        mem_protect::PAGE_EXECUTE_READ => {
            flags = flags.executable(true);
        }
        mem_protect::PAGE_EXECUTE_READWRITE => {
            flags = flags.writable(true).executable(true);
        }
        mem_protect::PAGE_EXECUTE => {
            flags = flags.executable(true);
        }
        _ => {
            // PAGE_READONLY, PAGE_NOACCESS, or unknown — read-only, no exec
        }
    }
    flags
}

// =============================================================================
// Handle table — tracks NT kernel object handles per-task
// =============================================================================

/// Represents the type of object behind an NT handle.
#[derive(Debug)]
enum HandleEntry {
    /// Console stdin
    Stdin,
    /// Console stdout
    Stdout,
    /// Console stderr
    Stderr,
    /// A memory region allocated by NtAllocateVirtualMemory
    VirtualMemory { base: usize, #[allow(dead_code)] size: usize },
}

/// Per-task handle table.
struct HandleTable {
    entries: BTreeMap<u64, HandleEntry>,
    next_handle: u64,
}

impl HandleTable {
    fn new() -> Self {
        let mut entries = BTreeMap::new();
        // Standard console handles (Windows convention)
        entries.insert(0x03, HandleEntry::Stdin);
        entries.insert(0x07, HandleEntry::Stdout);
        entries.insert(0x0B, HandleEntry::Stderr);
        HandleTable {
            entries,
            next_handle: 0x100, // User handles start here (4-aligned like Windows)
        }
    }

    fn allocate(&mut self, entry: HandleEntry) -> u64 {
        let handle = self.next_handle;
        self.next_handle += 4; // Windows handles are 4-byte aligned
        self.entries.insert(handle, entry);
        handle
    }

    fn close(&mut self, handle: u64) -> Option<HandleEntry> {
        // Don't allow closing standard console handles
        if handle == 0x03 || handle == 0x07 || handle == 0x0B {
            return None;
        }
        self.entries.remove(&handle)
    }

    #[allow(dead_code)]
    fn get(&self, handle: u64) -> Option<&HandleEntry> {
        self.entries.get(&handle)
    }
}

/// Global handle tables, keyed by task ID.
static HANDLE_TABLES: Mutex<BTreeMap<usize, HandleTable>> = Mutex::new(BTreeMap::new());

/// Get or create a handle table for the given task.
fn with_handle_table<F, R>(task_id: usize, f: F) -> R
where
    F: FnOnce(&mut HandleTable) -> R,
{
    let mut tables = HANDLE_TABLES.lock();
    let table = tables.entry(task_id).or_insert_with(HandleTable::new);
    f(table)
}

// =============================================================================
// Virtual memory tracking
// =============================================================================

/// Tracks all NtAllocateVirtualMemory allocations so they aren't dropped.
/// Keyed by starting virtual address.
static VM_REGIONS: Mutex<BTreeMap<usize, MappedPages>> = Mutex::new(BTreeMap::new());

// =============================================================================
// Main entry point
// =============================================================================

/// Main entry point for Windows NT syscall handling.
pub fn handle_syscall(
    num: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
) -> i64 {
    let table_index = (num >> 12) & 0x3;
    let service_num = num & 0xFFF;

    if table_index != 0 {
        warn!("windows_syscall: Win32k syscall table not implemented (table={}, service={})",
            table_index, service_num);
        return ntstatus::STATUS_NOT_IMPLEMENTED;
    }

    match service_num {
        // --- File I/O ---
        nr::NT_READ_FILE => nt_read_file(arg0, arg1, arg2, arg3),
        nr::NT_WRITE_FILE => nt_write_file(arg0, arg1, arg2, arg3),
        nr::NT_CREATE_FILE => nt_create_file(arg0, arg1, arg2, arg3),
        nr::NT_CLOSE => nt_close(arg0),
        nr::NT_QUERY_INFORMATION_FILE => nt_query_information_file(arg0, arg1, arg2, arg3),

        // --- Memory ---
        nr::NT_ALLOCATE_VIRTUAL_MEMORY => nt_allocate_virtual_memory(arg0, arg1, arg2, arg3, arg4, arg5),
        nr::NT_FREE_VIRTUAL_MEMORY => nt_free_virtual_memory(arg0, arg1, arg2, arg3),
        nr::NT_PROTECT_VIRTUAL_MEMORY => nt_protect_virtual_memory(arg0, arg1, arg2, arg3),

        // --- Process ---
        nr::NT_TERMINATE_PROCESS => nt_terminate_process(arg0, arg1),
        nr::NT_QUERY_INFORMATION_PROCESS => nt_query_information_process(arg0, arg1, arg2, arg3),

        // --- System info ---
        nr::NT_QUERY_SYSTEM_INFORMATION => nt_query_system_information(arg0, arg1, arg2, arg3),
        nr::NT_QUERY_PERFORMANCE_COUNTER => nt_query_performance_counter(arg0, arg1),

        _ => {
            warn!("windows_syscall: unimplemented NT syscall {:#x} (args: {:#x}, {:#x}, {:#x}, {:#x})",
                service_num, arg0, arg1, arg2, arg3);
            ntstatus::STATUS_NOT_IMPLEMENTED
        }
    }
}

// =============================================================================
// File I/O
// =============================================================================

fn nt_read_file(handle: u64, _event: u64, buffer: u64, length: u64) -> i64 {
    debug!("NtReadFile(handle={:#x}, buf={:#x}, len={})", handle, buffer, length);
    ntstatus::STATUS_NOT_IMPLEMENTED
}

fn nt_write_file(handle: u64, _event: u64, buffer: u64, length: u64) -> i64 {
    debug!("NtWriteFile(handle={:#x}, buf={:#x}, len={})", handle, buffer, length);

    // Handle stdout/stderr (console handles)
    if handle == 0x07 || handle == 0x0B {
        if buffer == 0 || length == 0 {
            return ntstatus::STATUS_INVALID_PARAMETER;
        }
        let slice = unsafe {
            core::slice::from_raw_parts(buffer as *const u8, length as usize)
        };
        if let Ok(s) = core::str::from_utf8(slice) {
            info!("[win-userspace] {}", s);
        }
        return ntstatus::STATUS_SUCCESS;
    }

    ntstatus::STATUS_NOT_IMPLEMENTED
}

fn nt_create_file(
    handle_out: u64,
    _desired_access: u64,
    _obj_attributes: u64,
    _io_status: u64,
) -> i64 {
    debug!("NtCreateFile(handle_out={:#x})", handle_out);
    ntstatus::STATUS_NOT_IMPLEMENTED
}

fn nt_close(handle: u64) -> i64 {
    debug!("NtClose(handle={:#x})", handle);

    let task_id = task::get_my_current_task_id();

    let closed_entry = with_handle_table(task_id, |table| table.close(handle));

    match closed_entry {
        Some(HandleEntry::VirtualMemory { base, .. }) => {
            // Free the associated memory region
            if VM_REGIONS.lock().remove(&base).is_some() {
                debug!("NtClose: freed virtual memory at {:#x}", base);
            }
            ntstatus::STATUS_SUCCESS
        }
        Some(_) => {
            // Other handle types (file, event, etc.) — just close
            ntstatus::STATUS_SUCCESS
        }
        None => {
            // Console handles or unknown handle
            if handle == 0x03 || handle == 0x07 || handle == 0x0B {
                // Can't close console handles, but return success (Windows behavior)
                ntstatus::STATUS_SUCCESS
            } else {
                ntstatus::STATUS_INVALID_HANDLE
            }
        }
    }
}

fn nt_query_information_file(handle: u64, _io_status: u64, _info: u64, _info_class: u64) -> i64 {
    debug!("NtQueryInformationFile(handle={:#x})", handle);
    ntstatus::STATUS_NOT_IMPLEMENTED
}

// =============================================================================
// Memory management
// =============================================================================

/// NtAllocateVirtualMemory — allocates or reserves virtual memory.
///
/// Arguments (Windows ABI):
///   arg0: ProcessHandle (-1 = current process)
///   arg1: *BaseAddress (IN/OUT pointer to PVOID)
///   arg2: ZeroBits
///   arg3: *RegionSize (IN/OUT pointer to SIZE_T)
///   arg4: AllocationType (MEM_COMMIT, MEM_RESERVE, etc.)
///   arg5: Protect (PAGE_READWRITE, etc.)
fn nt_allocate_virtual_memory(
    _process_handle: u64,
    base_addr_ptr: u64,
    _zero_bits: u64,
    region_size_ptr: u64,
    alloc_type: u64,
    protect: u64,
) -> i64 {
    // Read the requested size from the pointer
    if region_size_ptr == 0 {
        return ntstatus::STATUS_INVALID_PARAMETER;
    }

    let requested_size = unsafe { *(region_size_ptr as *const u64) } as usize;

    debug!("NtAllocateVirtualMemory(base_ptr={:#x}, size={:#x}, type={:#x}, protect={:#x})",
        base_addr_ptr, requested_size, alloc_type, protect);

    if requested_size == 0 {
        return ntstatus::STATUS_INVALID_PARAMETER;
    }

    // We only support MEM_COMMIT (and optionally MEM_RESERVE combined)
    if alloc_type & mem_type::MEM_COMMIT == 0 && alloc_type & mem_type::MEM_RESERVE == 0 {
        return ntstatus::STATUS_INVALID_PARAMETER;
    }

    let pte_flags = win_protect_to_pte_flags(protect);

    match memory::create_mapping(requested_size, pte_flags) {
        Ok(mp) => {
            let vaddr = mp.start_address().value();
            let actual_size = mp.size_in_bytes();

            // Zero the allocated memory (committed pages are zero-filled)
            unsafe {
                core::ptr::write_bytes(vaddr as *mut u8, 0, actual_size);
            }

            debug!("NtAllocateVirtualMemory: mapped {} bytes at {:#x}", actual_size, vaddr);

            // Write back the base address and size to the caller's pointers
            if base_addr_ptr != 0 {
                unsafe { *(base_addr_ptr as *mut u64) = vaddr as u64; }
            }
            unsafe { *(region_size_ptr as *mut u64) = actual_size as u64; }

            // Track the allocation
            let task_id = task::get_my_current_task_id();
            with_handle_table(task_id, |table| {
                table.allocate(HandleEntry::VirtualMemory { base: vaddr, size: actual_size });
            });
            VM_REGIONS.lock().insert(vaddr, mp);

            ntstatus::STATUS_SUCCESS
        }
        Err(e) => {
            warn!("NtAllocateVirtualMemory: memory::create_mapping failed: {}", e);
            ntstatus::STATUS_NO_MEMORY
        }
    }
}

/// NtFreeVirtualMemory — frees or decommits virtual memory.
///
/// Arguments:
///   arg0: ProcessHandle
///   arg1: *BaseAddress (IN/OUT pointer to PVOID)
///   arg2: *RegionSize (IN/OUT pointer to SIZE_T)
///   arg3: FreeType (MEM_DECOMMIT or MEM_RELEASE)
fn nt_free_virtual_memory(
    _process_handle: u64,
    base_addr_ptr: u64,
    _region_size_ptr: u64,
    free_type: u64,
) -> i64 {
    if base_addr_ptr == 0 {
        return ntstatus::STATUS_INVALID_PARAMETER;
    }

    let base_addr = unsafe { *(base_addr_ptr as *const u64) } as usize;

    debug!("NtFreeVirtualMemory(base={:#x}, type={:#x})", base_addr, free_type);

    if free_type & mem_type::MEM_RELEASE != 0 {
        // MEM_RELEASE: free the entire region
        if VM_REGIONS.lock().remove(&base_addr).is_some() {
            // Also remove from handle table
            let task_id = task::get_my_current_task_id();
            with_handle_table(task_id, |table| {
                // Find and close the handle for this memory region
                let handle_to_close: Option<u64> = table.entries.iter()
                    .find(|(_, v)| matches!(v, HandleEntry::VirtualMemory { base, .. } if *base == base_addr))
                    .map(|(k, _)| *k);
                if let Some(h) = handle_to_close {
                    table.close(h);
                }
            });
            debug!("NtFreeVirtualMemory: released region at {:#x}", base_addr);
            ntstatus::STATUS_SUCCESS
        } else {
            warn!("NtFreeVirtualMemory: no region found at {:#x}", base_addr);
            ntstatus::STATUS_INVALID_PARAMETER
        }
    } else if free_type & mem_type::MEM_DECOMMIT != 0 {
        // MEM_DECOMMIT: we treat this as a no-op success for now
        // (decommitting individual pages within a reservation isn't supported yet)
        debug!("NtFreeVirtualMemory: MEM_DECOMMIT treated as no-op");
        ntstatus::STATUS_SUCCESS
    } else {
        ntstatus::STATUS_INVALID_PARAMETER
    }
}

fn nt_protect_virtual_memory(
    _process_handle: u64,
    base_addr: u64,
    _region_size: u64,
    new_protect: u64,
) -> i64 {
    debug!("NtProtectVirtualMemory(base={:#x}, prot={:#x})", base_addr, new_protect);
    // Stub: return success (like Linux mprotect stub)
    ntstatus::STATUS_SUCCESS
}

// =============================================================================
// Process management
// =============================================================================

fn nt_terminate_process(process_handle: u64, exit_status: u64) -> i64 {
    debug!("NtTerminateProcess(handle={:#x}, status={:#x})", process_handle, exit_status);

    // Handle -1 (NtCurrentProcess) means current process
    if process_handle == 0xFFFF_FFFF_FFFF_FFFF || process_handle == 0 {
        // Clean up handle table
        let task_id = task::get_my_current_task_id();
        HANDLE_TABLES.lock().remove(&task_id);

        // Kill the current task
        let kill_result = task::with_current_task(|t| {
            t.kill(task::KillReason::Requested)
        });

        match kill_result {
            Ok(Ok(())) => {
                debug!("NtTerminateProcess: task killed, scheduling away");
                task::scheduler::schedule();
            }
            Ok(Err(state)) => {
                warn!("NtTerminateProcess: could not kill task (state: {:?})", state);
            }
            Err(e) => {
                warn!("NtTerminateProcess: no current task: {}", e);
            }
        }
    }

    ntstatus::STATUS_SUCCESS
}

fn nt_query_information_process(
    process_handle: u64,
    info_class: u64,
    _info_buffer: u64,
    _buffer_length: u64,
) -> i64 {
    debug!("NtQueryInformationProcess(handle={:#x}, class={})", process_handle, info_class);
    ntstatus::STATUS_NOT_IMPLEMENTED
}

// =============================================================================
// System information
// =============================================================================

fn nt_query_system_information(
    info_class: u64,
    _buffer: u64,
    _buffer_length: u64,
    _return_length: u64,
) -> i64 {
    debug!("NtQuerySystemInformation(class={})", info_class);
    ntstatus::STATUS_NOT_IMPLEMENTED
}

fn nt_query_performance_counter(counter_out: u64, frequency_out: u64) -> i64 {
    debug!("NtQueryPerformanceCounter(counter={:#x}, freq={:#x})", counter_out, frequency_out);

    if counter_out == 0 {
        return ntstatus::STATUS_INVALID_PARAMETER;
    }

    // Use TSC as performance counter
    let tsc: u64 = unsafe {
        let lo: u32;
        let hi: u32;
        core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi);
        ((hi as u64) << 32) | (lo as u64)
    };

    unsafe {
        *(counter_out as *mut u64) = tsc;
        if frequency_out != 0 {
            // Report a nominal 1 GHz frequency
            *(frequency_out as *mut u64) = 1_000_000_000;
        }
    }

    ntstatus::STATUS_SUCCESS
}
