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

extern crate alloc;

mod fd_table;

use alloc::vec;
use log::{debug, warn};

#[cfg(target_arch = "x86_64")]
use alloc::collections::BTreeMap;
#[cfg(target_arch = "x86_64")]
use alloc::vec::Vec;
#[cfg(target_arch = "x86_64")]
use spin::Mutex;
#[cfg(target_arch = "x86_64")]
use memory::MappedPages;
#[cfg(target_arch = "x86_64")]
use pte_flags::PteFlags;

// =============================================================================
// Static state for mmap tracking
// =============================================================================

/// Tracks all mmap-allocated MappedPages so they are not dropped prematurely.
/// Keyed by the starting virtual address of the mapping.
#[cfg(target_arch = "x86_64")]
static MMAP_REGIONS: Mutex<BTreeMap<usize, MappedPages>> = Mutex::new(BTreeMap::new());

/// Program break state for sys_brk.
/// The initial break address is set high enough to not collide with kernel mappings.
#[cfg(target_arch = "x86_64")]
static BRK_STATE: Mutex<BrkState> = Mutex::new(BrkState {
    current_brk: 0x6000_0000,
    initial_brk: 0x6000_0000,
});

/// Holds MappedPages allocated by brk so they are not dropped.
#[cfg(target_arch = "x86_64")]
static BRK_PAGES: Mutex<Vec<MappedPages>> = Mutex::new(Vec::new());

#[cfg(target_arch = "x86_64")]
struct BrkState {
    current_brk: usize,
    initial_brk: usize,
}

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
        nr::SYS_EXECVE => sys_execve(arg0, arg1, arg2),
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
// Helpers
// =============================================================================

/// Get the current MaiOS task ID, or 0 if unavailable.
fn current_task_id() -> usize {
    task::get_my_current_task_id()
}

/// Read a null-terminated C string from a userspace pointer.
/// Returns `None` if the pointer is null or the string is not valid UTF-8.
///
/// # Safety
/// The caller must ensure `ptr` points to readable memory that contains
/// a null-terminated byte sequence.
unsafe fn read_c_string(ptr: u64) -> Option<alloc::string::String> {
    if ptr == 0 {
        return None;
    }
    let mut p = ptr as *const u8;
    let mut len = 0usize;
    // Scan for the null terminator (cap at 4096 to avoid runaway reads).
    while len < 4096 {
        if *p == 0 {
            break;
        }
        p = p.add(1);
        len += 1;
    }
    if len == 0 || len >= 4096 {
        return None;
    }
    let slice = core::slice::from_raw_parts(ptr as *const u8, len);
    core::str::from_utf8(slice).ok().map(|s| alloc::string::String::from(s))
}

// =============================================================================
// File I/O syscalls
// =============================================================================

fn sys_read(fd: u64, buf_ptr: u64, count: u64) -> i64 {
    debug!("sys_read(fd={}, buf={:#x}, count={})", fd, buf_ptr, count);

    if buf_ptr == 0 || count == 0 {
        return if count == 0 { 0 } else { errno::EFAULT };
    }

    let tid = current_task_id();

    fd_table::with_table_mut(tid, |table| {
        let entry = match table.get_mut(fd) {
            Some(e) => e,
            None => return errno::EBADF,
        };

        match entry {
            fd_table::FdEntry::Stdin => {
                // Try to read from app_io stdin if available.
                match app_io::stdin() {
                    Ok(reader) => {
                        let buf = unsafe {
                            core::slice::from_raw_parts_mut(buf_ptr as *mut u8, count as usize)
                        };
                        match reader.read(buf) {
                            Ok(n) => n as i64,
                            Err(_) => errno::EAGAIN,
                        }
                    }
                    Err(_) => errno::EAGAIN,
                }
            }
            fd_table::FdEntry::Stdout | fd_table::FdEntry::Stderr => {
                errno::EBADF // cannot read from stdout/stderr
            }
            fd_table::FdEntry::File { file, offset } => {
                let mut buf = vec![0u8; count as usize];
                let mut locked = file.lock();
                match locked.read_at(&mut buf, *offset) {
                    Ok(bytes_read) => {
                        // Copy to userspace buffer.
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                buf.as_ptr(),
                                buf_ptr as *mut u8,
                                bytes_read,
                            );
                        }
                        *offset += bytes_read;
                        bytes_read as i64
                    }
                    Err(_) => errno::EIO,
                }
            }
        }
    })
}

fn sys_write(fd: u64, buf_ptr: u64, count: u64) -> i64 {
    debug!("sys_write(fd={}, buf={:#x}, count={})", fd, buf_ptr, count);

    if buf_ptr == 0 && count > 0 {
        return errno::EFAULT;
    }
    if count == 0 {
        return 0;
    }

    let slice = unsafe {
        core::slice::from_raw_parts(buf_ptr as *const u8, count as usize)
    };

    let tid = current_task_id();

    fd_table::with_table_mut(tid, |table| {
        let entry = match table.get_mut(fd) {
            Some(e) => e,
            None => return errno::EBADF,
        };

        match entry {
            fd_table::FdEntry::Stdout => {
                // Try app_io first, fall back to kernel log.
                if let Ok(w) = app_io::stdout() {
                    if let Ok(n) = w.write(slice) {
                        return n as i64;
                    }
                }
                // Fallback: kernel log.
                if let Ok(s) = core::str::from_utf8(slice) {
                    log::info!("[userspace] {}", s);
                }
                count as i64
            }
            fd_table::FdEntry::Stderr => {
                if let Ok(w) = app_io::stderr() {
                    if let Ok(n) = w.write(slice) {
                        return n as i64;
                    }
                }
                if let Ok(s) = core::str::from_utf8(slice) {
                    log::info!("[userspace] {}", s);
                }
                count as i64
            }
            fd_table::FdEntry::Stdin => {
                errno::EBADF // cannot write to stdin
            }
            fd_table::FdEntry::File { file, offset } => {
                let mut locked = file.lock();
                match locked.write_at(slice, *offset) {
                    Ok(bytes_written) => {
                        *offset += bytes_written;
                        bytes_written as i64
                    }
                    Err(_) => errno::EIO,
                }
            }
        }
    })
}

fn sys_open(path_ptr: u64, _flags: i32, _mode: u32) -> i64 {
    debug!("sys_open(path={:#x}, flags={:#x}, mode={:#o})", path_ptr, _flags, _mode);

    let path_str = match unsafe { read_c_string(path_ptr) } {
        Some(s) => s,
        None => return errno::EFAULT,
    };
    debug!("sys_open: resolved path string = \"{}\"", path_str);

    // Resolve through MaiOS VFS.
    let root_dir = root::get_root();
    let p = path::Path::new(&path_str);
    let file_ref = match p.get_file(root_dir) {
        Some(f) => f,
        None => {
            debug!("sys_open: ENOENT for \"{}\"", path_str);
            return errno::ENOENT;
        }
    };

    let tid = current_task_id();
    let fd = fd_table::with_table_mut(tid, |table| table.open(file_ref));
    debug!("sys_open: \"{}\" -> fd {}", path_str, fd);
    fd as i64
}

fn sys_close(fd: u64) -> i64 {
    debug!("sys_close(fd={})", fd);

    let tid = current_task_id();
    let closed = fd_table::with_table_mut(tid, |table| table.close(fd));
    if closed { 0 } else { errno::EBADF }
}

fn sys_stat(path_ptr: u64, stat_buf: u64) -> i64 {
    debug!("sys_stat(path={:#x}, buf={:#x})", path_ptr, stat_buf);

    if stat_buf == 0 {
        return errno::EFAULT;
    }

    let path_str = match unsafe { read_c_string(path_ptr) } {
        Some(s) => s,
        None => return errno::EFAULT,
    };

    let root_dir = root::get_root();
    let p = path::Path::new(&path_str);
    match p.get(root_dir) {
        Some(fs_node::FileOrDir::File(f)) => {
            let size = f.lock().len();
            fill_stat_buf(stat_buf, size, false);
            0
        }
        Some(fs_node::FileOrDir::Dir(_)) => {
            fill_stat_buf(stat_buf, 0, true);
            0
        }
        None => errno::ENOENT,
    }
}

fn sys_fstat(fd: u64, stat_buf: u64) -> i64 {
    debug!("sys_fstat(fd={}, buf={:#x})", fd, stat_buf);

    if stat_buf == 0 {
        return errno::EFAULT;
    }

    let tid = current_task_id();

    fd_table::with_table(tid, |table| {
        let entry = match table.get(fd) {
            Some(e) => e,
            None => return errno::EBADF,
        };

        match entry {
            fd_table::FdEntry::Stdin
            | fd_table::FdEntry::Stdout
            | fd_table::FdEntry::Stderr => {
                // Report as a character device (terminal).
                fill_stat_buf_chardev(stat_buf);
                0
            }
            fd_table::FdEntry::File { file, .. } => {
                let size = file.lock().len();
                fill_stat_buf(stat_buf, size, false);
                0
            }
        }
    })
}

fn sys_lseek(fd: u64, offset: i64, whence: i32) -> i64 {
    debug!("sys_lseek(fd={}, offset={}, whence={})", fd, offset, whence);

    const SEEK_SET: i32 = 0;
    const SEEK_CUR: i32 = 1;
    const SEEK_END: i32 = 2;

    let tid = current_task_id();

    fd_table::with_table_mut(tid, |table| {
        let entry = match table.get_mut(fd) {
            Some(e) => e,
            None => return errno::EBADF,
        };

        match entry {
            fd_table::FdEntry::File { file, offset: cur_off } => {
                let file_len = file.lock().len() as i64;
                let new_offset = match whence {
                    SEEK_SET => offset,
                    SEEK_CUR => *cur_off as i64 + offset,
                    SEEK_END => file_len + offset,
                    _ => return errno::EINVAL,
                };
                if new_offset < 0 {
                    return errno::EINVAL;
                }
                *cur_off = new_offset as usize;
                new_offset
            }
            // Stdin/stdout/stderr are not seekable.
            _ => errno::ESPIPE,
        }
    })
}

fn sys_ioctl(fd: u64, request: u64, arg: u64) -> i64 {
    debug!("sys_ioctl(fd={}, request={:#x}, arg={:#x})", fd, request, arg);

    // TIOCGWINSZ (0x5413) -- report a default terminal size.
    const TIOCGWINSZ: u64 = 0x5413;

    if request == TIOCGWINSZ && arg != 0 {
        // struct winsize { unsigned short ws_row, ws_col, ws_xpixel, ws_ypixel; }
        let ws = unsafe { &mut *(arg as *mut [u16; 4]) };
        ws[0] = 25;  // rows
        ws[1] = 80;  // cols
        ws[2] = 0;   // xpixel
        ws[3] = 0;   // ypixel
        return 0;
    }

    // Default: return ENOTTY for unknown ioctls on non-terminal fds.
    errno::ENOTTY
}

// =============================================================================
// Linux stat structure helpers
// =============================================================================

/// Linux x86_64 `struct stat` layout (total 144 bytes).
#[repr(C)]
struct LinuxStat {
    st_dev: u64,
    st_ino: u64,
    st_nlink: u64,
    st_mode: u32,
    st_uid: u32,
    st_gid: u32,
    __pad0: u32,
    st_rdev: u64,
    st_size: i64,
    st_blksize: i64,
    st_blocks: i64,
    st_atime: i64,
    st_atime_nsec: i64,
    st_mtime: i64,
    st_mtime_nsec: i64,
    st_ctime: i64,
    st_ctime_nsec: i64,
    __unused: [i64; 3],
}

/// Fill a stat buffer for a regular file or directory.
fn fill_stat_buf(stat_ptr: u64, size: usize, is_dir: bool) {
    // S_IFREG = 0o100000, S_IFDIR = 0o040000
    let mode: u32 = if is_dir { 0o040755 } else { 0o100644 };
    let stat = unsafe { &mut *(stat_ptr as *mut LinuxStat) };
    // Zero the entire struct first.
    unsafe { core::ptr::write_bytes(stat as *mut LinuxStat, 0, 1); }
    stat.st_dev = 1;
    stat.st_ino = 1;
    stat.st_nlink = 1;
    stat.st_mode = mode;
    stat.st_size = size as i64;
    stat.st_blksize = 4096;
    stat.st_blocks = ((size + 511) / 512) as i64;
}

/// Fill a stat buffer as a character device (for stdin/stdout/stderr).
fn fill_stat_buf_chardev(stat_ptr: u64) {
    // S_IFCHR = 0o020000
    let stat = unsafe { &mut *(stat_ptr as *mut LinuxStat) };
    unsafe { core::ptr::write_bytes(stat as *mut LinuxStat, 0, 1); }
    stat.st_dev = 1;
    stat.st_ino = 1;
    stat.st_nlink = 1;
    stat.st_mode = 0o020666; // character device, rw for all
    stat.st_rdev = 0x0501;   // /dev/tty-like major 5, minor 1
    stat.st_blksize = 1024;
}

// =============================================================================
// Memory management syscalls
// =============================================================================

/// Linux mmap flag constants.
#[cfg(target_arch = "x86_64")]
#[allow(dead_code)]
mod mmap_flags {
    pub const MAP_ANONYMOUS: u64 = 0x20;
    pub const MAP_PRIVATE: u64 = 0x02;
}

/// Linux mmap protection constants.
#[cfg(target_arch = "x86_64")]
#[allow(dead_code)]
mod mmap_prot {
    pub const PROT_READ: u64 = 0x1;
    pub const PROT_WRITE: u64 = 0x2;
    pub const PROT_EXEC: u64 = 0x4;
}

/// Converts Linux mmap PROT_* flags into MaiOS PteFlags.
#[cfg(target_arch = "x86_64")]
fn linux_prot_to_pte_flags(prot: u64) -> PteFlags {
    let mut flags = PteFlags::new(); // default: ACCESSED | NOT_EXECUTABLE
    if prot & mmap_prot::PROT_WRITE != 0 {
        flags = flags.writable(true);
    }
    if prot & mmap_prot::PROT_EXEC != 0 {
        flags = flags.executable(true);
    }
    // PROT_READ is implicit in the VALID bit which create_mapping sets.
    flags
}

fn sys_brk(addr: u64) -> i64 {
    debug!("sys_brk(addr={:#x})", addr);

    #[cfg(target_arch = "x86_64")]
    {
        let mut state = BRK_STATE.lock();
        let addr = addr as usize;

        // If addr == 0 or addr < initial_brk, return the current break.
        if addr == 0 || addr < state.initial_brk {
            return state.current_brk as i64;
        }

        // If addr <= current_brk, we are shrinking or staying the same.
        // We don't actually free pages on shrink for simplicity; just update the break.
        if addr <= state.current_brk {
            state.current_brk = addr;
            return addr as i64;
        }

        // addr > current_brk: need to allocate more memory.
        let growth = addr - state.current_brk;
        let pte_flags = PteFlags::new().writable(true);
        match memory::create_mapping(growth, pte_flags) {
            Ok(mp) => {
                // Zero the newly allocated memory (brk contract).
                let vaddr = mp.start_address().value();
                let size = mp.size_in_bytes();
                unsafe {
                    core::ptr::write_bytes(vaddr as *mut u8, 0, size);
                }
                BRK_PAGES.lock().push(mp);
                state.current_brk = addr;
                addr as i64
            }
            Err(e) => {
                warn!("sys_brk: memory::create_mapping failed: {}", e);
                // On failure, return the current break (not an error code).
                state.current_brk as i64
            }
        }
    }

    #[cfg(not(target_arch = "x86_64"))]
    { errno::ENOSYS }
}

fn sys_mmap(addr: u64, length: u64, prot: u64, flags: u64, fd: u64, offset: u64) -> i64 {
    debug!("sys_mmap(addr={:#x}, len={}, prot={:#x}, flags={:#x}, fd={}, off={})",
        addr, length, prot, flags, fd, offset);

    #[cfg(target_arch = "x86_64")]
    {
        // We only support anonymous private mappings for now.
        if flags & mmap_flags::MAP_ANONYMOUS == 0 {
            warn!("sys_mmap: non-anonymous mapping not supported (flags={:#x})", flags);
            return errno::ENOSYS;
        }

        if length == 0 {
            return errno::EINVAL;
        }

        let pte_flags = linux_prot_to_pte_flags(prot);

        match memory::create_mapping(length as usize, pte_flags) {
            Ok(mp) => {
                let vaddr = mp.start_address().value();
                let size = mp.size_in_bytes();

                // Zero the allocated memory (MAP_ANONYMOUS contract).
                unsafe {
                    core::ptr::write_bytes(vaddr as *mut u8, 0, size);
                }

                debug!("sys_mmap: mapped {} bytes at {:#x}", size, vaddr);
                MMAP_REGIONS.lock().insert(vaddr, mp);
                vaddr as i64
            }
            Err(e) => {
                warn!("sys_mmap: memory::create_mapping failed: {}", e);
                errno::ENOMEM
            }
        }
    }

    #[cfg(not(target_arch = "x86_64"))]
    { errno::ENOSYS }
}

fn sys_munmap(addr: u64, length: u64) -> i64 {
    debug!("sys_munmap(addr={:#x}, len={})", addr, length);

    #[cfg(target_arch = "x86_64")]
    {
        let addr = addr as usize;
        let mut regions = MMAP_REGIONS.lock();

        // Look up the mapping by its start address.
        // If found, remove it; the MappedPages will be dropped, unmapping the pages.
        if regions.remove(&addr).is_some() {
            debug!("sys_munmap: unmapped region at {:#x}", addr);
            return 0;
        }

        // If we didn't find an exact match, it might be a partial unmap or
        // an address within a larger region. For now, just succeed silently
        // to avoid breaking callers that unmap sub-ranges.
        warn!("sys_munmap: no mapping found at exact address {:#x}, returning success", addr);
        0
    }

    #[cfg(not(target_arch = "x86_64"))]
    { errno::ENOSYS }
}

fn sys_mprotect(addr: u64, length: u64, prot: u64) -> i64 {
    debug!("sys_mprotect(addr={:#x}, len={}, prot={:#x})", addr, length, prot);
    // Stub: return success. Changing page flags on existing MappedPages
    // is not straightforward in MaiOS without page table walking support.
    // Most callers just want to set permissions that are already compatible
    // with the initial mapping.
    0
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

fn sys_execve(path_ptr: u64, _argv_ptr: u64, _envp_ptr: u64) -> i64 {
    debug!("sys_execve(path={:#x}, argv={:#x}, envp={:#x})", path_ptr, _argv_ptr, _envp_ptr);

    // 1. Read the executable path from userspace.
    let path_str = match unsafe { read_c_string(path_ptr) } {
        Some(s) => s,
        None => return errno::EFAULT,
    };
    debug!("sys_execve: path = \"{}\"", path_str);

    // 2. Resolve the file in MaiOS VFS.
    let root_dir = root::get_root();
    let p = path::Path::new(&path_str);
    let file_ref = match p.get_file(root_dir) {
        Some(f) => f,
        None => {
            warn!("sys_execve: ENOENT for \"{}\"", path_str);
            return errno::ENOENT;
        }
    };

    // 3. Read the entire file into a buffer.
    let file_len = {
        let locked = file_ref.lock();
        io::KnownLength::len(&*locked)
    };
    if file_len == 0 {
        return errno::ENOEXEC;
    }

    let mut elf_data = vec![0u8; file_len];
    {
        let mut locked = file_ref.lock();
        match io::ByteReader::read_at(&mut *locked, &mut elf_data, 0) {
            Ok(_) => {}
            Err(_) => return errno::EIO,
        }
    }

    // 4. Validate ELF header before spawning.
    if elf_loader::parse_header(&elf_data).is_err() {
        warn!("sys_execve: \"{}\" is not a valid ELF64 binary", path_str);
        return errno::ENOEXEC;
    }

    // 5. Spawn a new MaiOS task that loads and jumps to the ELF entry point.
    //    We use spawn::new_task_builder with a closure that:
    //    a) loads the ELF segments into memory
    //    b) calls the entry point as a C ABI function
    //    This is a "spawn + kill self" model rather than true process replacement,
    //    because MaiOS tasks can't replace their own code regions safely.
    let task_name = alloc::format!("elf_{}", path_str);

    let task_result = spawn::new_task_builder(move |_: ()| -> isize {
        match elf_loader::load(&elf_data) {
            Ok(loaded) => {
                let entry = loaded.entry_point.value();
                debug!("sys_execve: jumping to ELF entry point at {:#x}", entry);

                // The loaded segments are kept alive by moving them into this closure.
                // They will be dropped when the task exits.
                let _segments = loaded.segments;

                // Call the ELF entry point as a C function: int main(void).
                // Most static ELF binaries have _start → __libc_start_main → main.
                // _start expects argc/argv/envp on the stack. For now, we call with
                // an empty argc=0, argv=NULL, envp=NULL environment.
                let entry_fn: extern "C" fn() -> ! = unsafe {
                    core::mem::transmute(entry)
                };
                entry_fn();
                // entry_fn is divergent (-> !), but if it somehow returns:
            }
            Err(e) => {
                log::error!("sys_execve: elf_loader::load failed: {}", e);
                -1
            }
        }
    }, ())
    .name(task_name)
    .spawn();

    match task_result {
        Ok(_join_handle) => {
            debug!("sys_execve: ELF task spawned, killing current task");
            // Kill the current task (execve replaces the calling process).
            sys_exit(0);
            0 // unreachable
        }
        Err(e) => {
            warn!("sys_execve: failed to spawn task: {}", e);
            errno::ENOMEM
        }
    }
}

fn sys_exit(status: i32) -> i64 {
    debug!("sys_exit(status={})", status);

    #[cfg(target_arch = "x86_64")]
    {
        // Clean up the file descriptor table for the exiting task.
        fd_table::remove_table(current_task_id());

        // Kill the current task, then yield the CPU so we never return.
        let kill_result = task::with_current_task(|t| {
            t.kill(task::KillReason::Requested)
        });

        match kill_result {
            Ok(Ok(())) => {
                debug!("sys_exit: task killed successfully, scheduling away");
            }
            Ok(Err(state)) => {
                warn!("sys_exit: could not kill task (state: {:?})", state);
            }
            Err(e) => {
                warn!("sys_exit: no current task: {}", e);
            }
        }

        // Yield the CPU. The killed task should not be rescheduled.
        task::scheduler::schedule();
    }

    // If we somehow return (shouldn't happen), return 0.
    0
}

fn sys_exit_group(status: i32) -> i64 {
    debug!("sys_exit_group(status={})", status);
    // In MaiOS single-threaded model, exit_group == exit.
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
