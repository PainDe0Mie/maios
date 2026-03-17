//! Linux syscall compatibility layer for MaiOS.
//!
//! Implements the Linux x86_64 syscall ABI, mapping Linux syscall numbers
//! to their MaiOS kernel equivalents. This allows unmodified Linux ELF
//! binaries to run on MaiOS without a translation layer.
//!
//! ## Syscall Convention (x86_64 Linux)
//!
//! - RAX = syscall number
//! - Arguments: RDI, RSI, RDX, R10, R8, R9
//! - Return value: RAX (negative values = -errno)
//!
//! ## Implementation Strategy
//!
//! Syscalls are grouped by subsystem for maintainability:
//! - File I/O: read, write, open, close, stat, etc.
//! - Process: fork, exec, exit, wait, getpid, etc.
//! - Memory: mmap, munmap, mprotect, brk, etc.
//! - Signals: kill, sigaction, sigprocmask, etc.
//! - Network: socket, bind, listen, accept, etc.

#![no_std]

use log::{debug, warn};

/// Linux errno values (negative return = error).
pub mod errno {
    pub const EPERM: i64 = -1;
    pub const ENOENT: i64 = -2;
    pub const ESRCH: i64 = -3;
    pub const EINTR: i64 = -4;
    pub const EIO: i64 = -5;
    pub const ENXIO: i64 = -6;
    pub const E2BIG: i64 = -7;
    pub const ENOEXEC: i64 = -8;
    pub const EBADF: i64 = -9;
    pub const ECHILD: i64 = -10;
    pub const EAGAIN: i64 = -11;
    pub const ENOMEM: i64 = -12;
    pub const EACCES: i64 = -13;
    pub const EFAULT: i64 = -14;
    pub const ENOTBLK: i64 = -15;
    pub const EBUSY: i64 = -16;
    pub const EEXIST: i64 = -17;
    pub const EXDEV: i64 = -18;
    pub const ENODEV: i64 = -19;
    pub const ENOTDIR: i64 = -20;
    pub const EISDIR: i64 = -21;
    pub const EINVAL: i64 = -22;
    pub const ENFILE: i64 = -23;
    pub const EMFILE: i64 = -24;
    pub const ENOTTY: i64 = -25;
    pub const ETXTBSY: i64 = -26;
    pub const EFBIG: i64 = -27;
    pub const ENOSPC: i64 = -28;
    pub const ESPIPE: i64 = -29;
    pub const EROFS: i64 = -30;
    pub const ENOSYS: i64 = -38;
}

/// Linux x86_64 syscall numbers.
pub mod nr {
    pub const SYS_READ: u64 = 0;
    pub const SYS_WRITE: u64 = 1;
    pub const SYS_OPEN: u64 = 2;
    pub const SYS_CLOSE: u64 = 3;
    pub const SYS_STAT: u64 = 4;
    pub const SYS_FSTAT: u64 = 5;
    pub const SYS_LSTAT: u64 = 6;
    pub const SYS_POLL: u64 = 7;
    pub const SYS_LSEEK: u64 = 8;
    pub const SYS_MMAP: u64 = 9;
    pub const SYS_MPROTECT: u64 = 10;
    pub const SYS_MUNMAP: u64 = 11;
    pub const SYS_BRK: u64 = 12;
    pub const SYS_IOCTL: u64 = 16;
    pub const SYS_ACCESS: u64 = 21;
    pub const SYS_PIPE: u64 = 22;
    pub const SYS_DUP: u64 = 32;
    pub const SYS_DUP2: u64 = 33;
    pub const SYS_GETPID: u64 = 39;
    pub const SYS_FORK: u64 = 57;
    pub const SYS_EXECVE: u64 = 59;
    pub const SYS_EXIT: u64 = 60;
    pub const SYS_WAIT4: u64 = 61;
    pub const SYS_KILL: u64 = 62;
    pub const SYS_UNAME: u64 = 63;
    pub const SYS_GETUID: u64 = 102;
    pub const SYS_GETGID: u64 = 104;
    pub const SYS_GETEUID: u64 = 107;
    pub const SYS_GETEGID: u64 = 108;
    pub const SYS_GETPPID: u64 = 110;
    pub const SYS_ARCH_PRCTL: u64 = 158;
    pub const SYS_GETTID: u64 = 186;
    pub const SYS_CLOCK_GETTIME: u64 = 228;
    pub const SYS_EXIT_GROUP: u64 = 231;
    pub const SYS_GETRANDOM: u64 = 318;
}

/// Main entry point for Linux syscall handling.
///
/// Routes the syscall number to the appropriate handler function.
/// Returns the result as an i64 (negative = -errno on error).
pub fn handle_syscall(
    num: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
) -> i64 {
    match num {
        // --- File I/O ---
        nr::SYS_READ => sys_read(arg0, arg1, arg2),
        nr::SYS_WRITE => sys_write(arg0, arg1, arg2),
        nr::SYS_OPEN => sys_open(arg0, arg1 as i32, arg2 as u32),
        nr::SYS_CLOSE => sys_close(arg0),
        nr::SYS_STAT => sys_stat(arg0, arg1),
        nr::SYS_FSTAT => sys_fstat(arg0, arg1),
        nr::SYS_LSEEK => sys_lseek(arg0, arg1 as i64, arg2 as i32),
        nr::SYS_IOCTL => sys_ioctl(arg0, arg1, arg2),

        // --- Memory management ---
        nr::SYS_BRK => sys_brk(arg0),
        nr::SYS_MMAP => sys_mmap(arg0, arg1, arg2, arg3, arg4, arg5),
        nr::SYS_MUNMAP => sys_munmap(arg0, arg1),
        nr::SYS_MPROTECT => sys_mprotect(arg0, arg1, arg2),

        // --- Process management ---
        nr::SYS_GETPID => sys_getpid(),
        nr::SYS_GETPPID => sys_getppid(),
        nr::SYS_GETTID => sys_gettid(),
        nr::SYS_EXIT => sys_exit(arg0 as i32),
        nr::SYS_EXIT_GROUP => sys_exit_group(arg0 as i32),

        // --- Identity (stub: MaiOS is single-user) ---
        nr::SYS_GETUID | nr::SYS_GETEUID => 0, // root
        nr::SYS_GETGID | nr::SYS_GETEGID => 0, // root

        // --- System info ---
        nr::SYS_UNAME => sys_uname(arg0),
        nr::SYS_ARCH_PRCTL => sys_arch_prctl(arg0 as i32, arg1),
        nr::SYS_CLOCK_GETTIME => sys_clock_gettime(arg0 as i32, arg1),
        nr::SYS_GETRANDOM => sys_getrandom(arg0, arg1, arg2 as u32),

        _ => {
            warn!("linux_syscall: unimplemented syscall {} (args: {:#x}, {:#x}, {:#x}, {:#x}, {:#x}, {:#x})",
                num, arg0, arg1, arg2, arg3, arg4, arg5);
            errno::ENOSYS
        }
    }
}

// =============================================================================
// File I/O syscalls
// =============================================================================

fn sys_read(fd: u64, buf_ptr: u64, count: u64) -> i64 {
    debug!("sys_read(fd={}, buf={:#x}, count={})", fd, buf_ptr, count);
    // TODO: Implement via MaiOS file descriptor table
    // For fd 0 (stdin), route through app_io::stdin()
    errno::ENOSYS
}

fn sys_write(fd: u64, buf_ptr: u64, count: u64) -> i64 {
    debug!("sys_write(fd={}, buf={:#x}, count={})", fd, buf_ptr, count);
    // TODO: Implement via MaiOS file descriptor table
    // For fd 1 (stdout) / fd 2 (stderr), route through app_io

    // Minimal stdout/stderr implementation for early testing:
    if fd == 1 || fd == 2 {
        // Safety: We trust that the userspace pointer is valid within
        // the task's address space. TODO: proper validation.
        let slice = unsafe {
            if buf_ptr == 0 || count == 0 {
                return errno::EFAULT;
            }
            core::slice::from_raw_parts(buf_ptr as *const u8, count as usize)
        };

        if let Ok(s) = core::str::from_utf8(slice) {
            // Use the kernel logger as a temporary output
            log::info!("[userspace] {}", s);
            return count as i64;
        } else {
            // Binary output — just report bytes written
            return count as i64;
        }
    }

    errno::EBADF
}

fn sys_open(path_ptr: u64, _flags: i32, _mode: u32) -> i64 {
    debug!("sys_open(path={:#x}, flags={}, mode={})", path_ptr, _flags, _mode);
    // TODO: Implement via MaiOS VFS
    errno::ENOSYS
}

fn sys_close(fd: u64) -> i64 {
    debug!("sys_close(fd={})", fd);
    // TODO: Implement file descriptor close
    errno::ENOSYS
}

fn sys_stat(path_ptr: u64, stat_buf: u64) -> i64 {
    debug!("sys_stat(path={:#x}, buf={:#x})", path_ptr, stat_buf);
    errno::ENOSYS
}

fn sys_fstat(fd: u64, stat_buf: u64) -> i64 {
    debug!("sys_fstat(fd={}, buf={:#x})", fd, stat_buf);
    errno::ENOSYS
}

fn sys_lseek(fd: u64, offset: i64, whence: i32) -> i64 {
    debug!("sys_lseek(fd={}, offset={}, whence={})", fd, offset, whence);
    errno::ENOSYS
}

fn sys_ioctl(fd: u64, request: u64, arg: u64) -> i64 {
    debug!("sys_ioctl(fd={}, request={:#x}, arg={:#x})", fd, request, arg);
    errno::ENOSYS
}

// =============================================================================
// Memory management syscalls
// =============================================================================

fn sys_brk(addr: u64) -> i64 {
    debug!("sys_brk(addr={:#x})", addr);
    // TODO: Implement program break management
    // Return current break address if addr == 0
    errno::ENOSYS
}

fn sys_mmap(addr: u64, length: u64, prot: u64, flags: u64, fd: u64, offset: u64) -> i64 {
    debug!("sys_mmap(addr={:#x}, len={}, prot={}, flags={}, fd={}, off={})",
        addr, length, prot, flags, fd, offset);
    // TODO: Implement via MaiOS memory allocator
    // This is critical for dynamic linking and heap allocation
    errno::ENOSYS
}

fn sys_munmap(addr: u64, length: u64) -> i64 {
    debug!("sys_munmap(addr={:#x}, len={})", addr, length);
    errno::ENOSYS
}

fn sys_mprotect(addr: u64, length: u64, prot: u64) -> i64 {
    debug!("sys_mprotect(addr={:#x}, len={}, prot={})", addr, length, prot);
    errno::ENOSYS
}

// =============================================================================
// Process management syscalls
// =============================================================================

fn sys_getpid() -> i64 {
    // Map MaiOS task ID to a Linux-style PID
    match task::with_current_task(|t| t.0.id) {
        Ok(id) => id as i64,
        Err(_) => 1, // fallback to init PID
    }
}

fn sys_getppid() -> i64 {
    // TODO: Track parent task relationships
    1 // Return init PID as parent
}

fn sys_gettid() -> i64 {
    sys_getpid() // In single-threaded model, TID == PID
}

fn sys_exit(status: i32) -> i64 {
    debug!("sys_exit(status={})", status);
    // TODO: Properly terminate the current task
    // For now, mark as killed
    0
}

fn sys_exit_group(status: i32) -> i64 {
    debug!("sys_exit_group(status={})", status);
    sys_exit(status)
}

// =============================================================================
// System info syscalls
// =============================================================================

/// Linux `utsname` structure layout.
#[repr(C)]
struct Utsname {
    sysname: [u8; 65],
    nodename: [u8; 65],
    release: [u8; 65],
    version: [u8; 65],
    machine: [u8; 65],
    domainname: [u8; 65],
}

fn sys_uname(buf_ptr: u64) -> i64 {
    debug!("sys_uname(buf={:#x})", buf_ptr);
    if buf_ptr == 0 {
        return errno::EFAULT;
    }

    let buf = unsafe { &mut *(buf_ptr as *mut Utsname) };

    fn fill_field(field: &mut [u8; 65], value: &str) {
        let bytes = value.as_bytes();
        let len = bytes.len().min(64);
        field[..len].copy_from_slice(&bytes[..len]);
        field[len] = 0;
    }

    fill_field(&mut buf.sysname, "MaiOS");
    fill_field(&mut buf.nodename, "maios");
    fill_field(&mut buf.release, "1.0.0-maios");
    fill_field(&mut buf.version, "MaiOS 1.0 (Linux compat)");
    fill_field(&mut buf.machine, "x86_64");
    fill_field(&mut buf.domainname, "");

    0
}

fn sys_arch_prctl(code: i32, addr: u64) -> i64 {
    debug!("sys_arch_prctl(code={:#x}, addr={:#x})", code, addr);

    const ARCH_SET_GS: i32 = 0x1001;
    const ARCH_SET_FS: i32 = 0x1002;
    const ARCH_GET_FS: i32 = 0x1003;
    const ARCH_GET_GS: i32 = 0x1004;

    match code {
        ARCH_SET_FS => {
            // Set FS base for TLS (Thread Local Storage)
            // This is critical for glibc/musl initialization
            unsafe {
                core::arch::asm!(
                    "wrmsr",
                    in("ecx") 0xC000_0100u32, // IA32_FS_BASE
                    in("eax") addr as u32,
                    in("edx") (addr >> 32) as u32,
                );
            }
            0
        }
        ARCH_SET_GS => {
            unsafe {
                core::arch::asm!(
                    "wrmsr",
                    in("ecx") 0xC000_0101u32, // IA32_GS_BASE
                    in("eax") addr as u32,
                    in("edx") (addr >> 32) as u32,
                );
            }
            0
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
            0
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
            0
        }
        _ => errno::EINVAL,
    }
}

fn sys_clock_gettime(clock_id: i32, tp: u64) -> i64 {
    debug!("sys_clock_gettime(clock_id={}, tp={:#x})", clock_id, tp);
    // TODO: Implement using MaiOS timer subsystem (TSC, PIT, or HPET)
    errno::ENOSYS
}

fn sys_getrandom(buf_ptr: u64, buf_len: u64, _flags: u32) -> i64 {
    debug!("sys_getrandom(buf={:#x}, len={}, flags={})", buf_ptr, buf_len, _flags);
    // TODO: Implement proper entropy source
    // For now, use a simple PRNG seeded from TSC
    if buf_ptr == 0 {
        return errno::EFAULT;
    }

    let slice = unsafe {
        core::slice::from_raw_parts_mut(buf_ptr as *mut u8, buf_len as usize)
    };

    // Simple xorshift64 PRNG seeded from TSC — NOT cryptographic!
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

    buf_len as i64
}
