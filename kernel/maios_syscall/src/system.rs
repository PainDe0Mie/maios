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
