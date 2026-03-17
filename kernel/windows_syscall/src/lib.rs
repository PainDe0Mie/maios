//! Windows NT syscall compatibility layer for MaiOS.
//!
//! Implements the Windows NT kernel syscall ABI, mapping NT system service
//! numbers to their MaiOS kernel equivalents. This allows Windows PE binaries
//! to run on MaiOS without a translation layer.
//!
//! ## NT Syscall Convention (x86_64)
//!
//! - RAX = syscall number (includes service table index in bits 12..13)
//! - Arguments: RCX (clobbered by SYSCALL, but original value saved), RDX, R8, R9,
//!   then stack for 5th+ arguments (with 32-byte shadow space)
//! - Return value: NTSTATUS in RAX
//!
//! ## NT Syscall Number Format
//!
//! ```text
//! Bits  0..11 : System service number within the table
//! Bits 12..13 : Service table index (0 = Nt*, 1 = Win32k)
//! ```
//!
//! ## Implementation Notes
//!
//! Windows NT syscall numbers change between OS versions. MaiOS targets
//! Windows 10/11 (build 19041+) syscall numbers as the baseline.
//! A version detection mechanism can remap older numbers if needed.

#![no_std]

use log::{debug, warn};

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
///
/// These numbers are NOT stable across Windows versions.
/// This module targets the most common modern Windows builds.
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

/// Main entry point for Windows NT syscall handling.
///
/// The syscall number format includes a service table index:
/// - Bits 0..11: service number within the table
/// - Bits 12..13: table index (0 = ntoskrnl, 1 = win32k)
///
/// We only handle table 0 (ntoskrnl services) for now.
pub fn handle_syscall(
    num: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    _arg4: u64,
    _arg5: u64,
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
        nr::NT_ALLOCATE_VIRTUAL_MEMORY => nt_allocate_virtual_memory(arg0, arg1, arg2, arg3),
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
    // NT console handles are typically 0x03, 0x07, 0x0B for stdin/stdout/stderr
    // but this varies. For simplicity, handle known patterns.
    if handle == 0x07 || handle == 0x0B {
        if buffer == 0 || length == 0 {
            return ntstatus::STATUS_INVALID_PARAMETER;
        }
        let slice = unsafe {
            core::slice::from_raw_parts(buffer as *const u8, length as usize)
        };
        if let Ok(s) = core::str::from_utf8(slice) {
            log::info!("[win-userspace] {}", s);
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
    ntstatus::STATUS_NOT_IMPLEMENTED
}

fn nt_query_information_file(handle: u64, _io_status: u64, _info: u64, _info_class: u64) -> i64 {
    debug!("NtQueryInformationFile(handle={:#x})", handle);
    ntstatus::STATUS_NOT_IMPLEMENTED
}

// =============================================================================
// Memory management
// =============================================================================

fn nt_allocate_virtual_memory(
    process_handle: u64,
    base_addr: u64,
    _zero_bits: u64,
    region_size: u64,
) -> i64 {
    debug!("NtAllocateVirtualMemory(proc={:#x}, base={:#x}, size={:#x})",
        process_handle, base_addr, region_size);
    ntstatus::STATUS_NOT_IMPLEMENTED
}

fn nt_free_virtual_memory(
    process_handle: u64,
    base_addr: u64,
    _region_size: u64,
    free_type: u64,
) -> i64 {
    debug!("NtFreeVirtualMemory(proc={:#x}, base={:#x}, type={:#x})",
        process_handle, base_addr, free_type);
    ntstatus::STATUS_NOT_IMPLEMENTED
}

fn nt_protect_virtual_memory(
    process_handle: u64,
    base_addr: u64,
    _region_size: u64,
    new_protect: u64,
) -> i64 {
    debug!("NtProtectVirtualMemory(proc={:#x}, base={:#x}, prot={:#x})",
        process_handle, base_addr, new_protect);
    ntstatus::STATUS_NOT_IMPLEMENTED
}

// =============================================================================
// Process management
// =============================================================================

fn nt_terminate_process(process_handle: u64, exit_status: u64) -> i64 {
    debug!("NtTerminateProcess(handle={:#x}, status={:#x})", process_handle, exit_status);
    // TODO: Terminate the task associated with this handle
    ntstatus::STATUS_NOT_IMPLEMENTED
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
            // TODO: Detect actual TSC frequency from CPUID or calibration
            *(frequency_out as *mut u64) = 1_000_000_000;
        }
    }

    ntstatus::STATUS_SUCCESS
}
