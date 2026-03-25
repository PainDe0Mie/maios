//! Syscalls liés au temps pour MaiOS.
//!
//! - `sys_clock_gettime`: CLOCK_MONOTONIC via `time::Instant`, CLOCK_REALTIME via RTC + monotonic
//! - `sys_nanosleep`: sleep via `time::Duration`
//! - `sys_perf_counter`: raw RDTSC pour NtQueryPerformanceCounter

use crate::error::{SyscallResult, SyscallError};

// Linux clock IDs
const CLOCK_REALTIME: u64 = 0;
const CLOCK_MONOTONIC: u64 = 1;
const CLOCK_MONOTONIC_RAW: u64 = 4;
const CLOCK_REALTIME_COARSE: u64 = 5;
const CLOCK_MONOTONIC_COARSE: u64 = 6;
const CLOCK_BOOTTIME: u64 = 7;

/// Linux `struct timespec` layout (x86_64).
#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

/// Boot epoch in seconds since Unix epoch (1970-01-01 00:00:00 UTC).
/// Computed once lazily from RTC + monotonic offset.
static BOOT_EPOCH_SECS: core::sync::atomic::AtomicI64 = core::sync::atomic::AtomicI64::new(0);
static BOOT_EPOCH_SET: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Convert RTC calendar time to Unix timestamp (seconds since epoch).
///
/// Simplified algorithm — assumes UTC, ignores leap seconds.
fn rtc_to_unix_secs(year: u16, month: u8, day: u8, hour: u8, min: u8, sec: u8) -> i64 {
    // Days from year 0 to 1970 is not needed; we compute from 1970 directly.
    let y = year as i64;
    let m = month as i64;
    let d = day as i64;

    // Adjust for months (January = 1, February = 2, ..., March = 3 starts the "year")
    let (y_adj, m_adj) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };

    // Days since epoch using a simplified formula
    let days = 365 * y_adj
        + y_adj / 4 - y_adj / 100 + y_adj / 400
        + (m_adj * 306 + 5) / 10
        + (d - 1)
        - 719468; // offset to Unix epoch (1970-01-01)

    days * 86400 + (hour as i64) * 3600 + (min as i64) * 60 + (sec as i64)
}

/// Get the boot epoch. Lazily initialized from RTC on first call.
fn boot_epoch_secs() -> i64 {
    if BOOT_EPOCH_SET.load(core::sync::atomic::Ordering::Acquire) {
        return BOOT_EPOCH_SECS.load(core::sync::atomic::Ordering::Relaxed);
    }

    // Read RTC and compute epoch
    let rtc = rtc::read_rtc();

    // RTC years register is 2-digit; assume 2000+ if < 70, else 1900+
    let full_year = if rtc.years < 70 { 2000 + rtc.years as u16 } else { 1900 + rtc.years as u16 };

    // Get current monotonic time to compute the boot epoch offset
    let mono_now = time::Instant::now().duration_since(time::Instant::ZERO);
    let rtc_now_secs = rtc_to_unix_secs(full_year, rtc.months, rtc.days, rtc.hours, rtc.minutes, rtc.seconds);
    let boot_epoch = rtc_now_secs - mono_now.as_secs() as i64;

    BOOT_EPOCH_SECS.store(boot_epoch, core::sync::atomic::Ordering::Relaxed);
    BOOT_EPOCH_SET.store(true, core::sync::atomic::Ordering::Release);

    boot_epoch
}

/// sys_clock_gettime — get the current time for a given clock.
///
/// Supports CLOCK_MONOTONIC (nanoseconds since boot) and
/// CLOCK_REALTIME (Unix timestamp computed from RTC + monotonic).
///
/// # Arguments
/// - `clock_id`: CLOCK_REALTIME (0), CLOCK_MONOTONIC (1), etc.
/// - `tp`: pointer to `struct timespec { tv_sec: i64, tv_nsec: i64 }`
pub fn sys_clock_gettime(clock_id: u64, tp: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    if tp == 0 {
        return Err(SyscallError::Fault);
    }

    let ts_ptr = tp as *mut Timespec;

    match clock_id {
        CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE | CLOCK_BOOTTIME => {
            // Nanoseconds since boot
            let elapsed = time::Instant::now().duration_since(time::Instant::ZERO);
            unsafe {
                (*ts_ptr).tv_sec = elapsed.as_secs() as i64;
                (*ts_ptr).tv_nsec = elapsed.subsec_nanos() as i64;
            }
            Ok(0)
        }
        CLOCK_REALTIME | CLOCK_REALTIME_COARSE => {
            // Unix timestamp = boot epoch + monotonic elapsed
            let elapsed = time::Instant::now().duration_since(time::Instant::ZERO);
            let boot_epoch = boot_epoch_secs();
            let total_secs = boot_epoch + elapsed.as_secs() as i64;

            unsafe {
                (*ts_ptr).tv_sec = total_secs;
                (*ts_ptr).tv_nsec = elapsed.subsec_nanos() as i64;
            }
            Ok(0)
        }
        _ => Err(SyscallError::InvalidArgument),
    }
}

/// sys_nanosleep — sleep for a specified duration.
///
/// # Arguments
/// - `req`: pointer to `struct timespec` with requested sleep duration
/// - `rem`: pointer to `struct timespec` for remaining time (ignored, we always sleep fully)
pub fn sys_nanosleep(req: u64, _rem: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    if req == 0 {
        return Err(SyscallError::Fault);
    }

    let ts = unsafe { &*(req as *const Timespec) };

    if ts.tv_sec < 0 || ts.tv_nsec < 0 || ts.tv_nsec >= 1_000_000_000 {
        return Err(SyscallError::InvalidArgument);
    }

    let duration = core::time::Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32);

    if duration.is_zero() {
        // Just yield the CPU
        let _ = sleep::sleep(sleep::Duration::from_millis(1));
        return Ok(0);
    }

    // Use the kernel sleep subsystem
    let deadline = time::Instant::now() + duration;
    // Busy-wait with yield until deadline (sleep crate uses wait_until internally)
    while time::Instant::now() < deadline {
        let _ = sleep::sleep(sleep::Duration::from_millis(1));
    }

    Ok(0)
}

/// sys_perf_counter — compteur de performance haute résolution.
///
/// Utilise RDTSC comme source. Fréquence nominale reportée à 1 GHz.
/// Compatible avec NtQueryPerformanceCounter (sortie via pointeurs).
///
/// Arguments : counter_ptr, frequency_ptr
pub fn sys_perf_counter(counter_ptr: u64, freq_ptr: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    if counter_ptr == 0 {
        return Err(SyscallError::InvalidArgument);
    }

    let tsc: u64 = unsafe {
        let lo: u32;
        let hi: u32;
        core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi);
        ((hi as u64) << 32) | (lo as u64)
    };

    unsafe {
        *(counter_ptr as *mut u64) = tsc;
        if freq_ptr != 0 {
            *(freq_ptr as *mut u64) = 1_000_000_000; // 1 GHz nominal
        }
    }

    Ok(0)
}
