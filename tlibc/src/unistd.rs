//! POSIX `<unistd.h>` functions — thin wrappers around MaiOS syscalls.
//!
//! Since tlibc runs in kernel space (Theseus single-address-space model),
//! we call kernel APIs directly rather than going through `syscall` instructions.

use libc::{c_int, c_char, c_void, size_t, ssize_t, off_t, pid_t};
use errno::*;
use core::ptr;

use app_io;
use task;

// ---------------------------------------------------------------------------
// File descriptor I/O  (fd 0 = stdin, 1 = stdout, 2 = stderr)
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn read(fd: c_int, buf: *mut c_void, count: size_t) -> ssize_t {
    if buf.is_null() {
        errno = EFAULT;
        return -1;
    }
    match fd {
        0 => {
            // stdin
            if let Some(stdin) = app_io::stdin() {
                let slice = core::slice::from_raw_parts_mut(buf as *mut u8, count);
                let mut locked = stdin.lock();
                let mut total = 0usize;
                for byte in slice.iter_mut() {
                    match locked.read_one() {
                        Some(ch) => { *byte = ch; total += 1; }
                        None => break,
                    }
                }
                total as ssize_t
            } else {
                errno = EBADF;
                -1
            }
        }
        _ => {
            // For other fds, use the resource table from maios_syscall (stub for now)
            errno = EBADF;
            -1
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn write(fd: c_int, buf: *const c_void, count: size_t) -> ssize_t {
    if buf.is_null() {
        errno = EFAULT;
        return -1;
    }
    let slice = core::slice::from_raw_parts(buf as *const u8, count);
    match fd {
        1 | 2 => {
            // stdout / stderr
            let out = if fd == 1 { app_io::stdout() } else { app_io::stderr() };
            if let Some(writer) = out {
                let mut locked = writer.lock();
                for &b in slice {
                    let _ = locked.write_one(b);
                }
                count as ssize_t
            } else {
                errno = EBADF;
                -1
            }
        }
        _ => {
            errno = EBADF;
            -1
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn close(_fd: c_int) -> c_int {
    // Stub: we don't have a real fd table in tlibc yet
    0
}

#[no_mangle]
pub unsafe extern "C" fn lseek(_fd: c_int, _offset: off_t, _whence: c_int) -> off_t {
    errno = ESPIPE;
    -1
}

// ---------------------------------------------------------------------------
// Process info
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn getpid() -> pid_t {
    task::get_my_current_task()
        .map(|t| t.id.into())
        .unwrap_or(1) as pid_t
}

#[no_mangle]
pub extern "C" fn getppid() -> pid_t {
    // Theseus doesn't track parent tasks — return 0 (init)
    0
}

#[no_mangle]
pub extern "C" fn getuid() -> u32 { 0 }
#[no_mangle]
pub extern "C" fn geteuid() -> u32 { 0 }
#[no_mangle]
pub extern "C" fn getgid() -> u32 { 0 }
#[no_mangle]
pub extern "C" fn getegid() -> u32 { 0 }

// ---------------------------------------------------------------------------
// Misc
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn getcwd(buf: *mut c_char, size: size_t) -> *mut c_char {
    if buf.is_null() || size == 0 {
        errno = EINVAL;
        return ptr::null_mut();
    }
    // Return "/" as the cwd (Theseus has a flat namespace)
    if size < 2 {
        errno = ERANGE;
        return ptr::null_mut();
    }
    *buf = b'/' as c_char;
    *buf.add(1) = 0;
    buf
}

#[no_mangle]
pub extern "C" fn chdir(_path: *const c_char) -> c_int {
    // Stub
    0
}

#[no_mangle]
pub extern "C" fn isatty(fd: c_int) -> c_int {
    // fd 0,1,2 are always a tty in MaiOS
    if fd >= 0 && fd <= 2 { 1 } else { 0 }
}

#[no_mangle]
pub extern "C" fn sysconf(name: c_int) -> i64 {
    const _SC_PAGESIZE: c_int = 30;
    const _SC_CLK_TCK: c_int = 2;
    const _SC_NPROCESSORS_ONLN: c_int = 84;

    match name {
        _SC_PAGESIZE => 4096,
        _SC_CLK_TCK => 100,
        _SC_NPROCESSORS_ONLN => 1,
        _ => -1,
    }
}

#[no_mangle]
pub unsafe extern "C" fn sleep(seconds: u32) -> u32 {
    let _ = sleep::sleep_until(
        time::Instant::now() + core::time::Duration::from_secs(seconds as u64)
    );
    0
}

#[no_mangle]
pub unsafe extern "C" fn usleep(usec: u32) -> c_int {
    let _ = sleep::sleep_until(
        time::Instant::now() + core::time::Duration::from_micros(usec as u64)
    );
    0
}

#[no_mangle]
pub unsafe extern "C" fn _exit(status: c_int) -> ! {
    if let Some(curr) = task::get_my_current_task() {
        curr.kill(task::KillReason::Requested);
    }
    loop { core::hint::spin_loop(); }
}
