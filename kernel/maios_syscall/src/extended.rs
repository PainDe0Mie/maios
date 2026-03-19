//! Extended syscalls for MaiOS — miscellaneous stubs and simple implementations.
//!
//! These syscalls are less critical but commonly probed by libc and applications.

use crate::error::{SyscallResult, SyscallError};

/// sys_select — synchronous I/O multiplexing (legacy).
///
/// Simplified: sleep for the timeout, return 0 (nothing ready).
/// Most modern programs use poll/epoll instead.
pub fn sys_select(_nfds: u64, _readfds: u64, _writefds: u64, _exceptfds: u64, timeout: u64, _: u64) -> SyscallResult {
    if timeout != 0 {
        let tv = unsafe { &*(timeout as *const [i64; 2]) };
        let secs = tv[0] as u64;
        let usecs = tv[1] as u64;
        if secs > 0 || usecs > 0 {
            let duration = core::time::Duration::new(secs, (usecs * 1000) as u32);
            let deadline = time::Instant::now() + duration;
            while time::Instant::now() < deadline {
                scheduler::schedule();
            }
        }
    }
    Ok(0)
}

/// sys_pselect6 — select with signal mask and nanosecond timeout.
///
/// Stub: delegate to select behavior.
pub fn sys_pselect6(nfds: u64, readfds: u64, writefds: u64, exceptfds: u64, timeout: u64, _sigmask: u64) -> SyscallResult {
    // pselect uses struct timespec (sec, nsec) instead of timeval (sec, usec)
    // For our stub, the difference doesn't matter much
    if timeout != 0 {
        let ts = unsafe { &*(timeout as *const [i64; 2]) };
        let duration = core::time::Duration::new(ts[0] as u64, ts[1] as u32);
        let deadline = time::Instant::now() + duration;
        while time::Instant::now() < deadline {
            scheduler::schedule();
        }
        return Ok(0);
    }
    sys_select(nfds, readfds, writefds, exceptfds, 0, 0)
}

/// sys_ppoll — poll with signal mask and nanosecond timeout.
///
/// Delegates to poll (ignoring the signal mask).
pub fn sys_ppoll(fds: u64, nfds: u64, timeout: u64, _sigmask: u64, _: u64, _: u64) -> SyscallResult {
    let timeout_ms = if timeout != 0 {
        let ts = unsafe { &*(timeout as *const [i64; 2]) };
        let ms = ts[0] as u64 * 1000 + ts[1] as u64 / 1_000_000;
        ms
    } else {
        u64::MAX // infinite (but poll will cap it)
    };
    crate::event_io::sys_poll(fds, nfds, timeout_ms, 0, 0, 0)
}

/// sys_statfs — get filesystem statistics.
///
/// Returns a fake ext4-like statfs structure.
pub fn sys_statfs(_path: u64, buf: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    if buf == 0 {
        return Err(SyscallError::Fault);
    }
    // struct statfs is 120 bytes on x86_64. Fill with reasonable defaults.
    unsafe {
        core::ptr::write_bytes(buf as *mut u8, 0, 120);
        let ptr = buf as *mut u64;
        *ptr = 0xEF53;                               // f_type = EXT4_SUPER_MAGIC
        *ptr.add(1) = 4096;                           // f_bsize = block size
        *ptr.add(2) = 1024 * 1024;                    // f_blocks = total blocks (4GB)
        *ptr.add(3) = 512 * 1024;                     // f_bfree = free blocks (2GB)
        *ptr.add(4) = 512 * 1024;                     // f_bavail = available blocks
        *ptr.add(5) = 1024 * 1024;                    // f_files = total inodes
        *ptr.add(6) = 1024 * 1024;                    // f_ffree = free inodes
        // f_fsid, f_namelen, f_frsize, f_flags
        let namelen_ptr = (buf + 88) as *mut u64;
        *namelen_ptr = 255;                            // f_namelen
    }
    Ok(0)
}

/// sys_fstatfs — fstatfs on a file descriptor. Delegates to statfs.
pub fn sys_fstatfs(_fd: u64, buf: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    sys_statfs(0, buf, 0, 0, 0, 0)
}

/// sys_personality — set process execution domain.
///
/// Always returns PER_LINUX (0). Used by glibc to check execution mode.
pub fn sys_personality(persona: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    if persona == 0xFFFFFFFF {
        // Query: return current personality
        return Ok(0); // PER_LINUX
    }
    // Set: accept but always keep PER_LINUX
    Ok(0)
}

/// sys_memfd_create — create an anonymous file in memory.
///
/// Stub: returns NotImplemented. Would need a tmpfs-like mechanism.
pub fn sys_memfd_create(_name: u64, _flags: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    Err(SyscallError::NotImplemented)
}

/// sys_timerfd_create — create a timer file descriptor.
///
/// Stub: returns NotImplemented.
pub fn sys_timerfd_create(_clockid: u64, _flags: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    Err(SyscallError::NotImplemented)
}

/// sys_timerfd_settime — arm/disarm a timerfd.
pub fn sys_timerfd_settime(_fd: u64, _flags: u64, _new: u64, _old: u64, _: u64, _: u64) -> SyscallResult {
    Err(SyscallError::BadFileDescriptor)
}

/// sys_timerfd_gettime — get timerfd remaining time.
pub fn sys_timerfd_gettime(_fd: u64, _curr: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    Err(SyscallError::BadFileDescriptor)
}

/// sys_signalfd4 — receive signals via a file descriptor.
///
/// Stub: returns NotImplemented (needs signal delivery infrastructure).
pub fn sys_signalfd4(_fd: u64, _mask: u64, _masksize: u64, _flags: u64, _: u64, _: u64) -> SyscallResult {
    Err(SyscallError::NotImplemented)
}

/// sys_clone — create a new process/thread.
///
/// Stub: returns NotImplemented. A real implementation requires:
/// 1. Duplicating the task with shared address space (CLONE_VM)
/// 2. Setting up new stack and TLS (CLONE_SETTLS)
/// 3. Writing child tid (CLONE_CHILD_SETTID, CLONE_CHILD_CLEARTID)
/// 4. Starting execution at the child entry point
///
/// This is the most complex syscall to implement properly.
pub fn sys_clone(_flags: u64, _stack: u64, _ptid: u64, _ctid: u64, _tls: u64, _: u64) -> SyscallResult {
    Err(SyscallError::NotImplemented)
}

/// sys_mincore — determine whether pages are resident in memory.
///
/// Stub: report all pages as resident.
pub fn sys_mincore(addr: u64, length: u64, vec_ptr: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    if vec_ptr == 0 {
        return Err(SyscallError::Fault);
    }
    let pages = ((length + 4095) / 4096) as usize;
    unsafe {
        // Set all pages as resident (bit 0 = 1)
        core::ptr::write_bytes(vec_ptr as *mut u8, 1, pages);
    }
    Ok(0)
}

/// sys_msync — synchronize a memory mapping with its backing store.
///
/// Stub: no-op (all our mappings are anonymous/in-memory).
pub fn sys_msync(_addr: u64, _length: u64, _flags: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    Ok(0)
}
