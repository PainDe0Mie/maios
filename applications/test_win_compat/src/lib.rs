//! Test application for the Windows NT syscall compatibility layer.
//!
//! This app directly invokes the Windows NT syscall handler functions
//! to verify that the compatibility layer works correctly.
//! Tests cover: NtQueryPerformanceCounter, NtWriteFile, NtAllocateVirtualMemory,
//! NtFreeVirtualMemory, NtClose, and handle table management.

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

/// Format an NTSTATUS as a name
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
    log::warn!("test_win_compat: main() entered");
    println!("=== Windows NT Syscall Compatibility Test ===");
    log::warn!("test_win_compat: first println done");

    println!("");

    let mut passed = 0u32;
    let mut failed = 0u32;

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
            0x07,
            0,
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
            0x0B,
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
    // Test 6: NtAllocateVirtualMemory — allocate 4 KB
    // ---------------------------------------------------------------
    println!("[TEST 6] NtAllocateVirtualMemory (4 KB, PAGE_READWRITE)");
    {
        let mut base_addr: u64 = 0;
        let mut region_size: u64 = 0x1000; // 4 KB
        let base_ptr = &mut base_addr as *mut u64 as u64;
        let size_ptr = &mut region_size as *mut u64 as u64;

        // Args: ProcessHandle, *BaseAddress, ZeroBits, *RegionSize, AllocationType, Protect
        let status = handle_syscall(
            nr::NT_ALLOCATE_VIRTUAL_MEMORY,
            0xFFFF_FFFF_FFFF_FFFF, // current process
            base_ptr,
            0,        // ZeroBits
            size_ptr, // *RegionSize
            0x3000,   // MEM_COMMIT | MEM_RESERVE
            0x04,     // PAGE_READWRITE
        );

        if nt_success(status) && base_addr != 0 && region_size >= 0x1000 {
            println!("  PASS: allocated at {:#x}, size={:#x}", base_addr, region_size);

            // Verify we can write to the allocated memory
            let ptr = base_addr as *mut u8;
            unsafe {
                *ptr = 0xAA;
                let val = *ptr;
                if val == 0xAA {
                    println!("  PASS: memory is readable/writable (wrote 0xAA, read back 0xAA)");
                    passed += 1;
                } else {
                    println!("  FAIL: memory read back {:#x}, expected 0xAA", val);
                    failed += 1;
                }
            }

            // Test 7: Free the memory with NtFreeVirtualMemory
            println!("[TEST 7] NtFreeVirtualMemory (MEM_RELEASE)");
            {
                let mut free_base: u64 = base_addr;
                let mut free_size: u64 = 0; // MEM_RELEASE ignores size
                let free_base_ptr = &mut free_base as *mut u64 as u64;
                let free_size_ptr = &mut free_size as *mut u64 as u64;

                let free_status = handle_syscall(
                    nr::NT_FREE_VIRTUAL_MEMORY,
                    0xFFFF_FFFF_FFFF_FFFF, // current process
                    free_base_ptr,
                    free_size_ptr,
                    0x8000, // MEM_RELEASE
                    0, 0,
                );

                if nt_success(free_status) {
                    println!("  PASS: NtFreeVirtualMemory succeeded");
                    passed += 1;
                } else {
                    println!("  FAIL: status={}", status_str(free_status));
                    failed += 1;
                }
            }
        } else {
            println!("  FAIL: status={} base={:#x} size={:#x}", status_str(status), base_addr, region_size);
            failed += 1;
            // Skip test 7 since allocation failed
            println!("[TEST 7] NtFreeVirtualMemory (SKIPPED - allocation failed)");
            failed += 1;
        }
    }

    // ---------------------------------------------------------------
    // Test 8: NtClose on invalid handle (should return INVALID_HANDLE)
    // ---------------------------------------------------------------
    println!("[TEST 8] NtClose (invalid handle)");
    {
        let status = handle_syscall(
            nr::NT_CLOSE,
            0xDEAD, 0, 0, 0, 0, 0,
        );

        if status == ntstatus::STATUS_INVALID_HANDLE {
            println!("  PASS: correctly returned STATUS_INVALID_HANDLE");
            passed += 1;
        } else {
            println!("  FAIL: expected INVALID_HANDLE, got {}", status_str(status));
            failed += 1;
        }
    }

    // ---------------------------------------------------------------
    // Test 9: NtClose on console handle (should succeed, Windows behavior)
    // ---------------------------------------------------------------
    println!("[TEST 9] NtClose (console handle 0x07 - should succeed)");
    {
        let status = handle_syscall(
            nr::NT_CLOSE,
            0x07, 0, 0, 0, 0, 0,
        );

        if nt_success(status) {
            println!("  PASS: NtClose on console handle returns SUCCESS");
            passed += 1;
        } else {
            println!("  FAIL: status={}", status_str(status));
            failed += 1;
        }
    }

    // ---------------------------------------------------------------
    // Test 10: Unknown syscall number
    // ---------------------------------------------------------------
    println!("[TEST 10] Unknown NT syscall (should return NOT_IMPLEMENTED)");
    {
        let status = handle_syscall(
            0x0FFF, 0, 0, 0, 0, 0, 0,
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
    // Test 11: Win32k table (table index 1) should be rejected
    // ---------------------------------------------------------------
    println!("[TEST 11] Win32k syscall table (should be NOT_IMPLEMENTED)");
    {
        let win32k_syscall = 0x1000 | 0x0001;
        let status = handle_syscall(
            win32k_syscall, 0, 0, 0, 0, 0, 0,
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
    // Test 12: NtAllocateVirtualMemory with invalid params
    // ---------------------------------------------------------------
    println!("[TEST 12] NtAllocateVirtualMemory (null size ptr -> should fail)");
    {
        let status = handle_syscall(
            nr::NT_ALLOCATE_VIRTUAL_MEMORY,
            0xFFFF_FFFF_FFFF_FFFF,
            0, 0,
            0, // null RegionSize pointer
            0x3000, 0x04,
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
    // Summary
    // ---------------------------------------------------------------
    let total = passed + failed;
    println!("");
    println!("=== Results ===");
    println!("  Passed:       {}", passed);
    println!("  Failed:       {}", failed);
    println!("  Total tests:  {}", total);
    println!("");

    if failed > 0 {
        println!("RESULT: SOME TESTS FAILED");
        -1
    } else {
        println!("RESULT: ALL {} TESTS PASSED", total);
        0
    }
}
