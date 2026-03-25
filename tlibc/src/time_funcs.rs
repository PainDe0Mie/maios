//! C `<time.h>` functions backed by MaiOS kernel time subsystem.

use libc::{c_int, c_long, time_t, clockid_t};
use errno::*;

// ---------------------------------------------------------------------------
// Structs matching POSIX definitions
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct timespec {
    pub tv_sec: time_t,
    pub tv_nsec: c_long,
}

#[repr(C)]
pub struct timeval {
    pub tv_sec: time_t,
    pub tv_usec: c_long,
}

#[repr(C)]
pub struct tm {
    pub tm_sec: c_int,
    pub tm_min: c_int,
    pub tm_hour: c_int,
    pub tm_mday: c_int,
    pub tm_mon: c_int,
    pub tm_year: c_int,
    pub tm_wday: c_int,
    pub tm_yday: c_int,
    pub tm_isdst: c_int,
}

pub const CLOCK_REALTIME: clockid_t = 0;
pub const CLOCK_MONOTONIC: clockid_t = 1;

// ---------------------------------------------------------------------------
// Functions
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn clock_gettime(clock_id: clockid_t, tp: *mut timespec) -> c_int {
    if tp.is_null() {
        errno = EFAULT;
        return -1;
    }
    let now = time::Instant::now();
    let dur = now.duration_since(time::Instant::ZERO);
    (*tp).tv_sec = dur.as_secs() as time_t;
    (*tp).tv_nsec = dur.subsec_nanos() as c_long;
    0
}

#[no_mangle]
pub unsafe extern "C" fn gettimeofday(tv: *mut timeval, _tz: *mut c_void) -> c_int {
    use libc::c_void;
    if tv.is_null() {
        return -1;
    }
    let now = time::Instant::now();
    let dur = now.duration_since(time::Instant::ZERO);
    (*tv).tv_sec = dur.as_secs() as time_t;
    (*tv).tv_usec = dur.subsec_micros() as c_long;
    0
}

#[no_mangle]
pub unsafe extern "C" fn time(tloc: *mut time_t) -> time_t {
    let now = time::Instant::now();
    let secs = now.duration_since(time::Instant::ZERO).as_secs() as time_t;
    if !tloc.is_null() {
        *tloc = secs;
    }
    secs
}

#[no_mangle]
pub unsafe extern "C" fn nanosleep(req: *const timespec, _rem: *mut timespec) -> c_int {
    if req.is_null() {
        errno = EFAULT;
        return -1;
    }
    let dur = core::time::Duration::new((*req).tv_sec as u64, (*req).tv_nsec as u32);
    let _ = sleep::sleep_until(time::Instant::now() + dur);
    0
}

#[no_mangle]
pub extern "C" fn clock() -> i64 {
    let now = time::Instant::now();
    let dur = now.duration_since(time::Instant::ZERO);
    // CLOCKS_PER_SEC = 1_000_000 on POSIX
    dur.as_micros() as i64
}

// Static buffer for localtime (not thread-safe, matching POSIX)
static mut TM_BUF: tm = tm {
    tm_sec: 0, tm_min: 0, tm_hour: 0, tm_mday: 1,
    tm_mon: 0, tm_year: 70, tm_wday: 4, tm_yday: 0, tm_isdst: 0,
};

#[no_mangle]
pub unsafe extern "C" fn localtime(timer: *const time_t) -> *mut tm {
    if timer.is_null() {
        return core::ptr::null_mut();
    }
    let t = *timer;
    // Simple UTC breakdown (no timezone support)
    let secs_per_day: time_t = 86400;
    let mut days = t / secs_per_day;
    let day_secs = t % secs_per_day;

    TM_BUF.tm_hour = (day_secs / 3600) as c_int;
    TM_BUF.tm_min = ((day_secs % 3600) / 60) as c_int;
    TM_BUF.tm_sec = (day_secs % 60) as c_int;

    // Days since epoch (1970-01-01 is Thursday = wday 4)
    TM_BUF.tm_wday = (((days % 7) + 4) % 7) as c_int;

    // Year calculation
    let mut year: c_int = 1970;
    loop {
        let days_in_year: time_t = if is_leap(year) { 366 } else { 365 };
        if days < days_in_year { break; }
        days -= days_in_year;
        year += 1;
    }
    TM_BUF.tm_year = year - 1900;
    TM_BUF.tm_yday = days as c_int;

    // Month calculation
    let leap = is_leap(year);
    let mdays: [c_int; 12] = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut mon = 0;
    let mut remaining = days as c_int;
    while mon < 12 && remaining >= mdays[mon as usize] {
        remaining -= mdays[mon as usize];
        mon += 1;
    }
    TM_BUF.tm_mon = mon;
    TM_BUF.tm_mday = remaining + 1;
    TM_BUF.tm_isdst = 0;

    &mut TM_BUF
}

#[no_mangle]
pub unsafe extern "C" fn gmtime(timer: *const time_t) -> *mut tm {
    localtime(timer) // MaiOS has no timezone support yet
}

fn is_leap(year: c_int) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

#[no_mangle]
pub unsafe extern "C" fn difftime(t1: time_t, t0: time_t) -> f64 {
    (t1 - t0) as f64
}

#[no_mangle]
pub unsafe extern "C" fn mktime(t: *mut tm) -> time_t {
    if t.is_null() { return -1; }
    let mdays: [c_int; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let year = (*t).tm_year + 1900;
    let mut days: time_t = 0;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }
    for m in 0..(*t).tm_mon {
        days += mdays[m as usize] as time_t;
        if m == 1 && is_leap(year) { days += 1; }
    }
    days += ((*t).tm_mday - 1) as time_t;
    days * 86400 + (*t).tm_hour as time_t * 3600 + (*t).tm_min as time_t * 60 + (*t).tm_sec as time_t
}
