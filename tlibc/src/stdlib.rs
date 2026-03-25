use libc::{c_int, c_char, c_void, c_long, c_ulong, c_double, size_t};
use errno::*;
use core::ptr;
use core::sync::atomic::{AtomicU64, Ordering};

use alloc::{
    alloc::{alloc, alloc_zeroed, dealloc, Layout},
    collections::BTreeMap,
    string::String,
    vec::Vec,
};
use spin::Mutex;


/// A map from the set of pointers that have been malloc-ed to the layouts they were malloc-ed with.
static POINTER_LAYOUTS: Mutex<BTreeMap<usize, Layout>> = Mutex::new(BTreeMap::new());

// Minimum alignment for malloc (8 bytes on x86_64 for proper alignment)
const MALLOC_ALIGN: usize = 8;

// ---------------------------------------------------------------------------
// Memory allocation
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn malloc(size: size_t) -> *mut c_void {
    if size == 0 { return ptr::null_mut(); }
    let layout = match Layout::from_size_align(size, MALLOC_ALIGN) {
        Ok(l)   => l,
        Err(_e) => {
            errno = EINVAL;
            return ptr::null_mut();
        }
    };
    let p = alloc(layout);
    if p.is_null() {
        errno = ENOMEM;
        return ptr::null_mut();
    }
    POINTER_LAYOUTS.lock().insert(p as usize, layout);
    p as *mut c_void
}

#[no_mangle]
pub unsafe extern "C" fn calloc(nelem: size_t, elsize: size_t) -> *mut c_void {
    let total = match nelem.checked_mul(elsize) {
        Some(t) if t > 0 => t,
        _ => return ptr::null_mut(),
    };
    let layout = match Layout::from_size_align(total, MALLOC_ALIGN) {
        Ok(l) => l,
        Err(_) => { errno = ENOMEM; return ptr::null_mut(); }
    };
    let p = alloc_zeroed(layout);
    if p.is_null() {
        errno = ENOMEM;
        return ptr::null_mut();
    }
    POINTER_LAYOUTS.lock().insert(p as usize, layout);
    p as *mut c_void
}

#[no_mangle]
pub unsafe extern "C" fn realloc(old_ptr: *mut c_void, new_size: size_t) -> *mut c_void {
    if old_ptr.is_null() {
        return malloc(new_size);
    }
    if new_size == 0 {
        free(old_ptr);
        return ptr::null_mut();
    }
    let old_layout = match POINTER_LAYOUTS.lock().get(&(old_ptr as usize)).copied() {
        Some(l) => l,
        None => { errno = EINVAL; return ptr::null_mut(); }
    };
    let new_layout = match Layout::from_size_align(new_size, MALLOC_ALIGN) {
        Ok(l) => l,
        Err(_) => { errno = ENOMEM; return ptr::null_mut(); }
    };
    let new_ptr = alloc::alloc::realloc(old_ptr as *mut u8, old_layout, new_size);
    if new_ptr.is_null() {
        errno = ENOMEM;
        return ptr::null_mut();
    }
    let mut layouts = POINTER_LAYOUTS.lock();
    layouts.remove(&(old_ptr as usize));
    layouts.insert(new_ptr as usize, new_layout);
    new_ptr as *mut c_void
}

#[no_mangle]
pub unsafe extern "C" fn free(ptr: *mut c_void) {
    if ptr.is_null() { return; }
    if let Some(layout) = POINTER_LAYOUTS.lock().remove(&(ptr as usize)) {
        dealloc(ptr as *mut u8, layout);
    } else {
        error!("free(): failed to free non-malloced pointer {:#X}", ptr as usize);
    }
}

#[no_mangle]
pub unsafe extern "C" fn aligned_alloc(alignment: size_t, size: size_t) -> *mut c_void {
    if size == 0 { return ptr::null_mut(); }
    let layout = match Layout::from_size_align(size, alignment) {
        Ok(l) => l,
        Err(_) => { errno = EINVAL; return ptr::null_mut(); }
    };
    let p = alloc(layout);
    if p.is_null() { errno = ENOMEM; return ptr::null_mut(); }
    POINTER_LAYOUTS.lock().insert(p as usize, layout);
    p as *mut c_void
}

#[no_mangle]
pub unsafe extern "C" fn posix_memalign(memptr: *mut *mut c_void, alignment: size_t, size: size_t) -> c_int {
    if memptr.is_null() || !alignment.is_power_of_two() || alignment < core::mem::size_of::<*mut c_void>() {
        return EINVAL;
    }
    let p = aligned_alloc(alignment, size);
    if p.is_null() { return ENOMEM; }
    *memptr = p;
    0
}

// ---------------------------------------------------------------------------
// Process control
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn abort() -> ! {
    core::intrinsics::abort();
}

static ATEXIT_HANDLERS: Mutex<Vec<extern "C" fn()>> = Mutex::new(Vec::new());

#[no_mangle]
pub extern "C" fn atexit(func: extern "C" fn()) -> c_int {
    ATEXIT_HANDLERS.lock().push(func);
    0
}

#[no_mangle]
pub unsafe extern "C" fn exit(status: c_int) -> ! {
    // Run atexit handlers in reverse order
    let handlers: Vec<extern "C" fn()> = {
        let mut h = ATEXIT_HANDLERS.lock();
        let v = h.clone();
        h.clear();
        v
    };
    for f in handlers.iter().rev() {
        f();
    }
    // Kill current task
    if let Some(curr) = task::get_my_current_task() {
        curr.kill(task::KillReason::Requested);
    }
    loop { core::hint::spin_loop(); }
}

// ---------------------------------------------------------------------------
// String → number conversions
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn atoi(s: *const c_char) -> c_int {
    strtol(s, ptr::null_mut(), 10) as c_int
}

#[no_mangle]
pub unsafe extern "C" fn atol(s: *const c_char) -> c_long {
    strtol(s, ptr::null_mut(), 10)
}

#[no_mangle]
pub unsafe extern "C" fn atof(s: *const c_char) -> c_double {
    strtod(s, ptr::null_mut())
}

#[no_mangle]
pub unsafe extern "C" fn strtol(
    nptr: *const c_char,
    endptr: *mut *mut c_char,
    base: c_int,
) -> c_long {
    if nptr.is_null() { return 0; }
    let mut p = nptr;

    // Skip whitespace
    while *p != 0 && (*p == b' ' as c_char || (*p >= 9 && *p <= 13)) { p = p.add(1); }

    // Sign
    let neg = *p == b'-' as c_char;
    if *p == b'+' as c_char || *p == b'-' as c_char { p = p.add(1); }

    // Determine base
    let mut b = base;
    if b == 0 {
        if *p == b'0' as c_char {
            p = p.add(1);
            if *p == b'x' as c_char || *p == b'X' as c_char {
                b = 16; p = p.add(1);
            } else {
                b = 8;
            }
        } else {
            b = 10;
        }
    } else if b == 16 && *p == b'0' as c_char {
        p = p.add(1);
        if *p == b'x' as c_char || *p == b'X' as c_char { p = p.add(1); }
    }

    let mut result: c_long = 0;
    loop {
        let c = *p as u8;
        let digit = match c {
            b'0'..=b'9' => (c - b'0') as c_long,
            b'a'..=b'z' => (c - b'a' + 10) as c_long,
            b'A'..=b'Z' => (c - b'A' + 10) as c_long,
            _ => break,
        };
        if digit >= b as c_long { break; }
        result = result.wrapping_mul(b as c_long).wrapping_add(digit);
        p = p.add(1);
    }

    if !endptr.is_null() { *endptr = p as *mut c_char; }
    if neg { result.wrapping_neg() } else { result }
}

#[no_mangle]
pub unsafe extern "C" fn strtoul(
    nptr: *const c_char,
    endptr: *mut *mut c_char,
    base: c_int,
) -> c_ulong {
    strtol(nptr, endptr, base) as c_ulong
}

#[no_mangle]
pub unsafe extern "C" fn strtod(nptr: *const c_char, endptr: *mut *mut c_char) -> c_double {
    if nptr.is_null() { return 0.0; }
    let mut p = nptr;

    // Skip whitespace
    while *p != 0 && (*p == b' ' as c_char || (*p >= 9 && *p <= 13)) { p = p.add(1); }

    let neg = *p == b'-' as c_char;
    if *p == b'+' as c_char || *p == b'-' as c_char { p = p.add(1); }

    let mut result: c_double = 0.0;
    // Integer part
    while *p >= b'0' as c_char && *p <= b'9' as c_char {
        result = result * 10.0 + (*p - b'0' as c_char) as c_double;
        p = p.add(1);
    }
    // Fractional part
    if *p == b'.' as c_char {
        p = p.add(1);
        let mut frac: c_double = 0.1;
        while *p >= b'0' as c_char && *p <= b'9' as c_char {
            result += (*p - b'0' as c_char) as c_double * frac;
            frac *= 0.1;
            p = p.add(1);
        }
    }
    // Exponent
    if *p == b'e' as c_char || *p == b'E' as c_char {
        p = p.add(1);
        let exp_neg = *p == b'-' as c_char;
        if *p == b'+' as c_char || *p == b'-' as c_char { p = p.add(1); }
        let mut exp: c_int = 0;
        while *p >= b'0' as c_char && *p <= b'9' as c_char {
            exp = exp * 10 + (*p - b'0' as c_char) as c_int;
            p = p.add(1);
        }
        let mut mult: c_double = 1.0;
        for _ in 0..exp { mult *= 10.0; }
        if exp_neg { result /= mult; } else { result *= mult; }
    }

    if !endptr.is_null() { *endptr = p as *mut c_char; }
    if neg { -result } else { result }
}

#[no_mangle]
pub unsafe extern "C" fn strtof(nptr: *const c_char, endptr: *mut *mut c_char) -> f32 {
    strtod(nptr, endptr) as f32
}

#[no_mangle]
pub unsafe extern "C" fn strtoll(nptr: *const c_char, endptr: *mut *mut c_char, base: c_int) -> i64 {
    strtol(nptr, endptr, base) as i64
}

#[no_mangle]
pub unsafe extern "C" fn strtoull(nptr: *const c_char, endptr: *mut *mut c_char, base: c_int) -> u64 {
    strtol(nptr, endptr, base) as u64
}

// ---------------------------------------------------------------------------
// Sorting & searching
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn qsort(
    base: *mut c_void,
    nmemb: size_t,
    size: size_t,
    compar: extern "C" fn(*const c_void, *const c_void) -> c_int,
) {
    if base.is_null() || nmemb <= 1 || size == 0 { return; }
    // Simple insertion sort (sufficient for small arrays; Mesa doesn't qsort huge arrays)
    let base_ptr = base as *mut u8;
    let mut tmp = alloc::vec![0u8; size];
    for i in 1..nmemb {
        let mut j = i;
        while j > 0 {
            let a = base_ptr.add(j * size) as *const c_void;
            let b = base_ptr.add((j - 1) * size) as *const c_void;
            if compar(a, b) < 0 {
                // Swap
                ptr::copy_nonoverlapping(base_ptr.add(j * size), tmp.as_mut_ptr(), size);
                ptr::copy_nonoverlapping(base_ptr.add((j - 1) * size), base_ptr.add(j * size), size);
                ptr::copy_nonoverlapping(tmp.as_ptr(), base_ptr.add((j - 1) * size), size);
                j -= 1;
            } else {
                break;
            }
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn bsearch(
    key: *const c_void,
    base: *const c_void,
    nmemb: size_t,
    size: size_t,
    compar: extern "C" fn(*const c_void, *const c_void) -> c_int,
) -> *mut c_void {
    if base.is_null() || nmemb == 0 { return ptr::null_mut(); }
    let base_ptr = base as *const u8;
    let mut lo: usize = 0;
    let mut hi: usize = nmemb;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let elem = base_ptr.add(mid * size) as *const c_void;
        let cmp = compar(key, elem);
        if cmp < 0 { hi = mid; }
        else if cmp > 0 { lo = mid + 1; }
        else { return elem as *mut c_void; }
    }
    ptr::null_mut()
}

// ---------------------------------------------------------------------------
// Pseudo-random number generation
// ---------------------------------------------------------------------------

static RAND_SEED: AtomicU64 = AtomicU64::new(1);

#[no_mangle]
pub extern "C" fn srand(seed: u32) {
    RAND_SEED.store(seed as u64, Ordering::Relaxed);
}

#[no_mangle]
pub extern "C" fn rand() -> c_int {
    // Linear congruential generator (glibc parameters)
    let old = RAND_SEED.load(Ordering::Relaxed);
    let new = old.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    RAND_SEED.store(new, Ordering::Relaxed);
    ((new >> 33) & 0x7FFF_FFFF) as c_int
}

pub const RAND_MAX: c_int = 0x7FFF_FFFF;

// ---------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------

// Simple in-memory environment
static ENV_VARS: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());

#[no_mangle]
pub unsafe extern "C" fn getenv(name: *const c_char) -> *mut c_char {
    if name.is_null() { return ptr::null_mut(); }
    let key = cstr_to_string(name);
    let env = ENV_VARS.lock();
    match env.get(&key) {
        Some(val) => val.as_ptr() as *mut c_char,
        None => ptr::null_mut(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn setenv(name: *const c_char, value: *const c_char, overwrite: c_int) -> c_int {
    if name.is_null() { return -1; }
    let key = cstr_to_string(name);
    let val = if value.is_null() { String::new() } else { cstr_to_string(value) };
    let mut env = ENV_VARS.lock();
    if overwrite != 0 || !env.contains_key(&key) {
        env.insert(key, val);
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn unsetenv(name: *const c_char) -> c_int {
    if name.is_null() { return -1; }
    let key = cstr_to_string(name);
    ENV_VARS.lock().remove(&key);
    0
}

// ---------------------------------------------------------------------------
// Misc
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn abs(n: c_int) -> c_int {
    if n < 0 { -n } else { n }
}

#[no_mangle]
pub extern "C" fn labs(n: c_long) -> c_long {
    if n < 0 { -n } else { n }
}

#[no_mangle]
pub extern "C" fn llabs(n: i64) -> i64 {
    if n < 0 { -n } else { n }
}

/// div_t for div()
#[repr(C)]
pub struct div_t {
    pub quot: c_int,
    pub rem: c_int,
}

#[no_mangle]
pub extern "C" fn div(numer: c_int, denom: c_int) -> div_t {
    div_t { quot: numer / denom, rem: numer % denom }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

unsafe fn cstr_to_string(s: *const c_char) -> String {
    let mut len = 0;
    while *s.add(len) != 0 { len += 1; }
    let bytes = core::slice::from_raw_parts(s as *const u8, len);
    String::from(core::str::from_utf8_unchecked(bytes))
}
