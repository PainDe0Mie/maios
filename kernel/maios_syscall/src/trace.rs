//! Syscall tracing via direct COM1 serial output.
//!
//! Activated by the `syscall-trace` feature flag. When enabled, every syscall
//! invocation logs its number, name, arguments, and return value directly to
//! the serial port — no locks, no allocations, no dependency on stdio/app_io.
//!
//! This is designed to remain functional even when the rest of the I/O stack
//! is broken (which is exactly when you need it most).
//!
//! # Usage
//!
//! In `kernel/maios_syscall/Cargo.toml`:
//! ```toml
//! [features]
//! syscall-trace = []
//! ```
//!
//! Enable at build time:
//! ```sh
//! cargo build --features maios_syscall/syscall-trace
//! ```

/// Write a single byte to COM1, busy-waiting for TX ready.
///
/// # Safety
/// Uses direct port I/O. Safe in kernel context on x86_64.
#[inline(always)]
unsafe fn serial_byte(b: u8) {
    loop {
        let status: u8;
        core::arch::asm!("in al, dx", out("al") status, in("dx") 0x3FDu16);
        if status & 0x20 != 0 {
            break;
        }
    }
    core::arch::asm!("out dx, al", in("al") b, in("dx") 0x3F8u16);
}

/// Write a string to COM1.
fn serial_str(s: &str) {
    for b in s.bytes() {
        unsafe { serial_byte(b); }
    }
}

/// Write a u64 in hexadecimal to COM1 (e.g., "0x1A3F").
fn serial_hex(val: u64) {
    serial_str("0x");
    if val == 0 {
        unsafe { serial_byte(b'0'); }
        return;
    }
    // Find the highest non-zero nibble
    let mut started = false;
    for i in (0..16).rev() {
        let nibble = ((val >> (i * 4)) & 0xF) as u8;
        if nibble != 0 {
            started = true;
        }
        if started {
            let ch = if nibble < 10 { b'0' + nibble } else { b'a' + nibble - 10 };
            unsafe { serial_byte(ch); }
        }
    }
}

/// Write an i64 in decimal to COM1 (e.g., "-42").
fn serial_i64(val: i64) {
    if val < 0 {
        unsafe { serial_byte(b'-'); }
        serial_u64((-val) as u64);
    } else {
        serial_u64(val as u64);
    }
}

/// Write a u64 in decimal to COM1.
fn serial_u64(val: u64) {
    if val == 0 {
        unsafe { serial_byte(b'0'); }
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = 0;
    let mut v = val;
    while v > 0 {
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        i += 1;
    }
    for j in (0..i).rev() {
        unsafe { serial_byte(buf[j]); }
    }
}

/// Syscall name lookup table. Returns a short human-readable name.
pub fn syscall_name(nr: u16) -> &'static str {
    match nr {
        // Process & Thread (0x00xx)
        0x0000 => "exit",
        0x0001 => "getpid",
        0x0002 => "getppid",
        0x0003 => "gettid",
        0x0004 => "execve",
        0x0005 => "spawn",
        0x0006 => "kill",
        0x0007 => "wait",
        0x0008 => "exit_group",
        0x0009 => "getuid",
        0x000A => "getgid",
        0x000B => "geteuid",
        0x000C => "getegid",
        0x000D => "set_tid_address",
        0x000E => "set_robust_list",
        0x000F => "prlimit64",

        // Memory (0x01xx)
        0x0100 => "mmap",
        0x0101 => "munmap",
        0x0102 => "mprotect",
        0x0103 => "brk",
        0x0104 => "alloc_vm",
        0x0105 => "free_vm",

        // File I/O (0x02xx)
        0x0200 => "read",
        0x0201 => "write",
        0x0202 => "open",
        0x0203 => "close",
        0x0204 => "stat",
        0x0205 => "fstat",
        0x0206 => "lseek",
        0x0207 => "ioctl",
        0x0208 => "dup",
        0x0209 => "dup2",
        0x020A => "pipe",
        0x020B => "openat",
        0x020C => "fcntl",
        0x020D => "writev",
        0x020E => "readv",
        0x020F => "pread64",
        0x0210 => "access",
        0x0211 => "pipe2",
        0x0212 => "dup3",
        0x0213 => "getcwd",
        0x0214 => "getdents64",
        0x0215 => "chdir",
        0x0216 => "mkdir",
        0x0217 => "unlink",
        0x0218 => "readlink",
        0x0219 => "newfstatat",
        0x021A => "faccessat",
        0x021B => "pwrite64",

        // Time (0x03xx)
        0x0300 => "clock_gettime",
        0x0301 => "nanosleep",
        0x0302 => "perf_counter",

        // System Info (0x04xx)
        0x0400 => "uname",
        0x0401 => "arch_prctl",
        0x0402 => "getrandom",
        0x0403 => "rt_sigaction",
        0x0404 => "rt_sigprocmask",
        0x0405 => "rt_sigreturn",
        0x0406 => "sched_yield",
        0x0407 => "gettimeofday",
        0x0408 => "clock_getres",
        0x0409 => "sched_getaffinity",
        0x040A => "prctl",
        0x040B => "madvise",

        // MaiOS-specific (0x08xx)
        0x0800 => "create_window",
        0x0801 => "destroy_window",
        0x0802 => "map_framebuffer",
        0x0803 => "present",
        0x0804 => "get_event",
        0x0805 => "audio_write",

        _ => "?",
    }
}

/// Log a syscall entry (before execution).
///
/// Format: `[SYSCALL] name(arg0, arg1, ...) [nr=0xNNNN]`
pub fn trace_entry(nr: u16, args: &[u64; 6], arg_count: u8) {
    serial_str("[SYSCALL] ");
    serial_str(syscall_name(nr));
    serial_str("(");
    let count = core::cmp::min(arg_count as usize, 6);
    for i in 0..count {
        if i > 0 {
            serial_str(", ");
        }
        serial_hex(args[i]);
    }
    serial_str(")");
}

/// Log a syscall result (after execution).
///
/// Format: ` = OK(value)` or ` = ERR(code)\n`
pub fn trace_exit(nr: u16, result: &crate::error::SyscallResult) {
    match result {
        Ok(val) => {
            serial_str(" = ");
            serial_i64(*val as i64);
        }
        Err(e) => {
            serial_str(" = ERR(");
            serial_str(e.as_str());
            serial_str(")");
        }
    }
    // Syscall number for quick grep
    serial_str(" [");
    serial_hex(nr as u64);
    serial_str("]\n");

    // Suppress unused warning when feature is disabled
    let _ = nr;
}
