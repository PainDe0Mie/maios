//! Test application for the Windows NT syscall compatibility layer.
//!
//! This app directly invokes the Windows NT syscall handler functions
//! to verify that the compatibility layer works correctly.
//! It tests: NtQueryPerformanceCounter, NtWriteFile (stdout),
//! NtAllocateVirtualMemory, NtClose, and NtTerminateProcess stubs.

#![no_std]

extern crate alloc;
#[macro_use] extern crate app_io;
extern crate windows_syscall;

use alloc::vec::Vec;
use alloc::string::String;
use windows_syscall::{handle_syscall, ntstatus, nr};

/// NT_SUCCESS macro equivalent
fn nt_success(status: i64) -> bool {
    status >= 0
}

/// Format an NTSTATUS as hex string
fn status_str(status: i64) -> &'static str {
    match status {
        s if s == ntstatus::STATUS_SUCCESS => "STATUS_SUCCESS",
        s if s == ntstatus::STATUS_NOT_IMPLEMENTED => "STATUS_NOT_IMPLEMENTED",
        s if s == ntstatus::STATUS_INVALID_PARAMETER => "STATUS_INVALID_PARAMETER",
        s if s == ntstatus::STATUS_INVALID_HANDLE => "STATUS_INVALID_HANDLE",
        s if s == ntstatus::STATUS_NO_MEMORY => "STATUS_NO_MEMORY",
        _ => "UNKNOWN",
    }
}

pub fn main(_args: Vec<String>) -> isize {
    println!("=== Windows NT Syscall Compatibility Test ===");
    println!("");

    let mut passed = 0u32;
    let mut failed = 0u32;
    let mut not_impl = 0u32;

    // ---------------------------------------------------------------
    // Test 1: NtQueryPerformanceCounter
    // ---------------------------------------------------------------
    println!("[TEST 1] NtQueryPerformanceCounter");
    {
        let mut counter: u64 = 0;
        let mut frequency: u64 = 0;
        let counter_ptr = &mut counter as *mut u64 as u64;
        let freq_ptr = &mut frequency as *mut u64 as u64;

        let status = handle_syscall(
            nr::NT_QUERY_PERFORMANCE_COUNTER,
            counter_ptr, freq_ptr, 0, 0, 0, 0,
        );

        if nt_success(status) && counter > 0 && frequency > 0 {
            println!("  PASS: counter={}, freq={} Hz", counter, frequency);
            passed += 1;
        } else {
            println!("  FAIL: status={} counter={} freq={}", status_str(status), counter, frequency);
            failed += 1;
        }
    }

    // ---------------------------------------------------------------
    // Test 2: NtQueryPerformanceCounter with NULL pointer (should fail)
    // ---------------------------------------------------------------
    println!("[TEST 2] NtQueryPerformanceCounter (NULL ptr -> should fail)");
    {
        let status = handle_syscall(
            nr::NT_QUERY_PERFORMANCE_COUNTER,
            0, 0, 0, 0, 0, 0,
        );

        if status == ntstatus::STATUS_INVALID_PARAMETER {
            println!("  PASS: correctly returned STATUS_INVALID_PARAMETER");
            passed += 1;
        } else {
            println!("  FAIL: expected INVALID_PARAMETER, got {}", status_str(status));
            failed += 1;
        }
    }

    // ---------------------------------------------------------------
    // Test 3: NtWriteFile to stdout (handle 0x07)
    // ---------------------------------------------------------------
    println!("[TEST 3] NtWriteFile (stdout)");
    {
        let msg = b"  Hello from Windows NT compatibility layer!\n";
        let status = handle_syscall(
            nr::NT_WRITE_FILE,
            0x07, // stdout handle
            0,    // event (unused)
            msg.as_ptr() as u64,
            msg.len() as u64,
            0, 0,
        );

        if nt_success(status) {
            println!("  PASS: NtWriteFile to stdout succeeded");
            passed += 1;
        } else {
            println!("  FAIL: status={}", status_str(status));
            failed += 1;
        }
    }

    // ---------------------------------------------------------------
    // Test 4: NtWriteFile to stderr (handle 0x0B)
    // ---------------------------------------------------------------
    println!("[TEST 4] NtWriteFile (stderr)");
    {
        let msg = b"  Hello from stderr via NT syscall!\n";
        let status = handle_syscall(
            nr::NT_WRITE_FILE,
            0x0B, // stderr handle
            0,
            msg.as_ptr() as u64,
            msg.len() as u64,
            0, 0,
        );

        if nt_success(status) {
            println!("  PASS: NtWriteFile to stderr succeeded");
            passed += 1;
        } else {
            println!("  FAIL: status={}", status_str(status));
            failed += 1;
        }
    }

    // ---------------------------------------------------------------
    // Test 5: NtWriteFile with NULL buffer (should fail)
    // ---------------------------------------------------------------
    println!("[TEST 5] NtWriteFile (NULL buffer -> should fail)");
    {
        let status = handle_syscall(
            nr::NT_WRITE_FILE,
            0x07, 0, 0, 10, 0, 0,
        );

        if status == ntstatus::STATUS_INVALID_PARAMETER {
            println!("  PASS: correctly returned STATUS_INVALID_PARAMETER");
            passed += 1;
        } else {
            println!("  FAIL: expected INVALID_PARAMETER, got {}", status_str(status));
            failed += 1;
        }
    }

    // ---------------------------------------------------------------
    // Test 6: NtClose (not yet implemented)
    // ---------------------------------------------------------------
    println!("[TEST 6] NtClose (stub check)");
    {
        let status = handle_syscall(
            nr::NT_CLOSE,
            0x42, 0, 0, 0, 0, 0,
        );

        if status == ntstatus::STATUS_NOT_IMPLEMENTED {
            println!("  INFO: NtClose returns STATUS_NOT_IMPLEMENTED (expected for stub)");
            not_impl += 1;
        } else if nt_success(status) {
            println!("  PASS: NtClose succeeded");
            passed += 1;
        } else {
            println!("  FAIL: unexpected status {}", status_str(status));
            failed += 1;
        }
    }

    // ---------------------------------------------------------------
    // Test 7: NtAllocateVirtualMemory (stub check)
    // ---------------------------------------------------------------
    println!("[TEST 7] NtAllocateVirtualMemory (stub check)");
    {
        let status = handle_syscall(
            nr::NT_ALLOCATE_VIRTUAL_MEMORY,
            0xFFFF_FFFF_FFFF_FFFF, // current process
            0, 0, 0x1000,
            0, 0,
        );

        if status == ntstatus::STATUS_NOT_IMPLEMENTED {
            println!("  INFO: NtAllocateVirtualMemory returns NOT_IMPLEMENTED (expected)");
            not_impl += 1;
        } else if nt_success(status) {
            println!("  PASS: NtAllocateVirtualMemory succeeded");
            passed += 1;
        } else {
            println!("  FAIL: unexpected status {}", status_str(status));
            failed += 1;
        }
    }

    // ---------------------------------------------------------------
    // Test 8: NtQuerySystemInformation (stub check)
    // ---------------------------------------------------------------
    println!("[TEST 8] NtQuerySystemInformation (stub check)");
    {
        let status = handle_syscall(
            nr::NT_QUERY_SYSTEM_INFORMATION,
            0, // SystemBasicInformation
            0, 0, 0, 0, 0,
        );

        if status == ntstatus::STATUS_NOT_IMPLEMENTED {
            println!("  INFO: NtQuerySystemInformation returns NOT_IMPLEMENTED");
            not_impl += 1;
        } else if nt_success(status) {
            println!("  PASS: NtQuerySystemInformation succeeded");
            passed += 1;
        } else {
            println!("  FAIL: unexpected status {}", status_str(status));
            failed += 1;
        }
    }

    // ---------------------------------------------------------------
    // Test 9: Unknown syscall number
    // ---------------------------------------------------------------
    println!("[TEST 9] Unknown NT syscall (should return NOT_IMPLEMENTED)");
    {
        let status = handle_syscall(
            0x0FFF, // high service number, unlikely to be implemented
            0, 0, 0, 0, 0, 0,
        );

        if status == ntstatus::STATUS_NOT_IMPLEMENTED {
            println!("  PASS: unknown syscall correctly returns NOT_IMPLEMENTED");
            passed += 1;
        } else {
            println!("  FAIL: expected NOT_IMPLEMENTED, got {}", status_str(status));
            failed += 1;
        }
    }

    // ---------------------------------------------------------------
    // Test 10: Win32k table (table index 1) should be rejected
    // ---------------------------------------------------------------
    println!("[TEST 10] Win32k syscall table (should be NOT_IMPLEMENTED)");
    {
        // Set bit 12 to indicate table index 1 (Win32k)
        let win32k_syscall = 0x1000 | 0x0001;
        let status = handle_syscall(
            win32k_syscall,
            0, 0, 0, 0, 0, 0,
        );

        if status == ntstatus::STATUS_NOT_IMPLEMENTED {
            println!("  PASS: Win32k table correctly rejected");
            passed += 1;
        } else {
            println!("  FAIL: expected NOT_IMPLEMENTED, got {}", status_str(status));
            failed += 1;
        }
    }

    // ---------------------------------------------------------------
    // Summary
    // ---------------------------------------------------------------
    println!("");
    println!("=== Results ===");
    println!("  Passed:          {}", passed);
    println!("  Failed:          {}", failed);
    println!("  Not implemented: {}", not_impl);
    println!("  Total tests:     {}", passed + failed + not_impl);
    println!("");

    if failed > 0 {
        println!("RESULT: SOME TESTS FAILED");
        -1
    } else {
        println!("RESULT: ALL IMPLEMENTED SYSCALLS WORK CORRECTLY");
        0
    }
}
