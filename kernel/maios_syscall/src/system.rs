//! Syscalls d'information système pour MaiOS.

use log::debug;
use crate::error::{SyscallResult, SyscallError};

// =============================================================================
// uname
// =============================================================================

/// Layout de la structure `utsname` Linux.
#[repr(C)]
struct Utsname {
    sysname: [u8; 65],
    nodename: [u8; 65],
    release: [u8; 65],
    version: [u8; 65],
    machine: [u8; 65],
    domainname: [u8; 65],
}

fn fill_field(field: &mut [u8; 65], value: &str) {
    let bytes = value.as_bytes();
    let len = bytes.len().min(64);
    field[..len].copy_from_slice(&bytes[..len]);
    field[len] = 0;
}

/// sys_uname — informations système.
pub fn sys_uname(buf_ptr: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    debug!("sys_uname(buf={:#x})", buf_ptr);

    if buf_ptr == 0 {
        return Err(SyscallError::Fault);
    }

    let buf = unsafe { &mut *(buf_ptr as *mut Utsname) };
    fill_field(&mut buf.sysname, "MaiOS");
    fill_field(&mut buf.nodename, "maios");
    fill_field(&mut buf.release, "1.0.0-maios");
    fill_field(&mut buf.version, "MaiOS 1.0 (Linux compat)");
    fill_field(&mut buf.machine, "x86_64");
    fill_field(&mut buf.domainname, "");

    Ok(0)
}

// =============================================================================
// arch_prctl
// =============================================================================

/// sys_arch_prctl — configuration des registres FS/GS (TLS).
pub fn sys_arch_prctl(code: u64, addr: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    let code = code as i32;
    debug!("sys_arch_prctl(code={:#x}, addr={:#x})", code, addr);

    const ARCH_SET_GS: i32 = 0x1001;
    const ARCH_SET_FS: i32 = 0x1002;
    const ARCH_GET_FS: i32 = 0x1003;
    const ARCH_GET_GS: i32 = 0x1004;

    match code {
        ARCH_SET_FS => {
            unsafe {
                core::arch::asm!(
                    "wrmsr",
                    in("ecx") 0xC000_0100u32,
                    in("eax") addr as u32,
                    in("edx") (addr >> 32) as u32,
                );
            }
            Ok(0)
        }
        ARCH_SET_GS => {
            unsafe {
                core::arch::asm!(
                    "wrmsr",
                    in("ecx") 0xC000_0101u32,
                    in("eax") addr as u32,
                    in("edx") (addr >> 32) as u32,
                );
            }
            Ok(0)
        }
        ARCH_GET_FS => {
            let lo: u32;
            let hi: u32;
            unsafe {
                core::arch::asm!(
                    "rdmsr",
                    in("ecx") 0xC000_0100u32,
                    out("eax") lo,
                    out("edx") hi,
                );
                *(addr as *mut u64) = ((hi as u64) << 32) | (lo as u64);
            }
            Ok(0)
        }
        ARCH_GET_GS => {
            let lo: u32;
            let hi: u32;
            unsafe {
                core::arch::asm!(
                    "rdmsr",
                    in("ecx") 0xC000_0101u32,
                    out("eax") lo,
                    out("edx") hi,
                );
                *(addr as *mut u64) = ((hi as u64) << 32) | (lo as u64);
            }
            Ok(0)
        }
        _ => Err(SyscallError::InvalidArgument),
    }
}

// =============================================================================
// getrandom
// =============================================================================

/// sys_getrandom — remplir un buffer avec des octets aléatoires.
///
/// Utilise un PRNG xorshift64 seedé depuis TSC — NON cryptographique.
pub fn sys_getrandom(buf_ptr: u64, buf_len: u64, _flags: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    debug!("sys_getrandom(buf={:#x}, len={})", buf_ptr, buf_len);

    if buf_ptr == 0 {
        return Err(SyscallError::Fault);
    }

    let slice = unsafe {
        core::slice::from_raw_parts_mut(buf_ptr as *mut u8, buf_len as usize)
    };

    let mut state: u64 = unsafe {
        let lo: u32;
        let hi: u32;
        core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi);
        ((hi as u64) << 32) | (lo as u64)
    };

    for byte in slice.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = state as u8;
    }

    Ok(buf_len)
}

/// sys_clock_getres — get clock resolution.
///
/// Returns the resolution of the specified clock. Games use this
/// to determine timer precision for their game loops.
pub fn sys_clock_getres(clock_id: u64, res: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    if res == 0 {
        return Ok(0); // Just checking if clock_id is valid
    }

    // All our clocks have nanosecond resolution (TSC/HPET based)
    let (tv_sec, tv_nsec): (i64, i64) = match clock_id {
        0 | 1 | 4 | 5 | 6 | 7 => (0, 1), // 1 nanosecond
        _ => return Err(SyscallError::InvalidArgument),
    };

    unsafe {
        let ptr = res as *mut [i64; 2];
        (*ptr)[0] = tv_sec;
        (*ptr)[1] = tv_nsec;
    }
    Ok(0)
}

/// sys_sched_getaffinity — get CPU affinity mask.
///
/// Used by SDL2/games to detect the number of CPUs.
pub fn sys_sched_getaffinity(_pid: u64, cpusetsize: u64, mask_ptr: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    if mask_ptr == 0 || cpusetsize == 0 {
        return Err(SyscallError::Fault);
    }

    // Report 4 CPUs (matching our QEMU -smp 4 config)
    // Affinity mask: bits 0-3 set = CPUs 0,1,2,3
    let mask: u64 = 0b1111; // 4 CPUs
    let bytes_to_write = core::cmp::min(cpusetsize as usize, 8);

    unsafe {
        // Zero the buffer first
        core::ptr::write_bytes(mask_ptr as *mut u8, 0, cpusetsize as usize);
        // Write the mask
        core::ptr::copy_nonoverlapping(
            &mask as *const u64 as *const u8,
            mask_ptr as *mut u8,
            bytes_to_write,
        );
    }
    // Return the size of the cpuset
    Ok(core::cmp::min(cpusetsize, 8))
}

/// sys_prctl — process control operations.
///
/// Supports PR_SET_NAME (set thread name) and basic queries.
pub fn sys_prctl(option: u64, arg2: u64, _arg3: u64, _arg4: u64, _arg5: u64, _: u64) -> SyscallResult {
    const PR_SET_NAME: u64 = 15;
    const PR_GET_NAME: u64 = 16;
    const PR_SET_PDEATHSIG: u64 = 1;
    const PR_GET_PDEATHSIG: u64 = 2;

    match option {
        PR_SET_NAME => {
            // Accept and ignore the thread name (we don't track it yet)
            let _ = arg2;
            Ok(0)
        }
        PR_GET_NAME => {
            // Return "maios" as thread name
            if arg2 != 0 {
                let name = b"maios\0";
                unsafe {
                    core::ptr::copy_nonoverlapping(name.as_ptr(), arg2 as *mut u8, 6);
                }
            }
            Ok(0)
        }
        PR_SET_PDEATHSIG => Ok(0), // Ignore parent death signal
        PR_GET_PDEATHSIG => {
            if arg2 != 0 {
                unsafe { *(arg2 as *mut i32) = 0; }
            }
            Ok(0)
        }
        _ => Err(SyscallError::InvalidArgument),
    }
}

/// sys_madvise — advise kernel about memory usage patterns.
///
/// Stub: accept and ignore all advice. The most important one is
/// MADV_DONTNEED (used by allocators to release pages).
pub fn sys_madvise(_addr: u64, _length: u64, _advice: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    // Accept all advice silently. A real implementation would:
    // - MADV_DONTNEED: zero pages and mark as lazy-allocate
    // - MADV_WILLNEED: prefault pages
    // - MADV_SEQUENTIAL/RANDOM: hint for readahead
    Ok(0)
}
