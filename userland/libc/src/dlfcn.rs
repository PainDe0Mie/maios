//! C `<dlfcn.h>` dynamic loading stubs for MaiOS.
//!
//! Theseus uses its own crate-level module system rather than .so/.dll.
//! These stubs allow C programs that call dlopen/dlsym to link,
//! but actual dynamic loading uses Theseus's `mod_mgmt` infrastructure.

use libc::{c_int, c_char, c_void};
use core::ptr;

pub const RTLD_LAZY: c_int = 1;
pub const RTLD_NOW: c_int = 2;
pub const RTLD_GLOBAL: c_int = 0x100;
pub const RTLD_LOCAL: c_int = 0;

static mut LAST_ERROR: [u8; 128] = [0; 128];

#[no_mangle]
pub unsafe extern "C" fn dlopen(filename: *const c_char, _flags: c_int) -> *mut c_void {
    if filename.is_null() {
        return 1 as *mut c_void; // sentinel for RTLD_DEFAULT
    }
    set_error(b"dlopen: dynamic loading not supported in static environment\0");
    ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn dlsym(_handle: *mut c_void, symbol: *const c_char) -> *mut c_void {
    if symbol.is_null() {
        set_error(b"dlsym: null symbol\0");
        return ptr::null_mut();
    }
    set_error(b"dlsym: symbol not found\0");
    ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn dlclose(_handle: *mut c_void) -> c_int {
    // Theseus crates are reference-counted; we don't explicitly unload here.
    0
}

#[no_mangle]
pub unsafe extern "C" fn dlerror() -> *mut c_char {
    if LAST_ERROR[0] == 0 {
        return ptr::null_mut();
    }
    let ptr = LAST_ERROR.as_mut_ptr() as *mut c_char;
    // Clear error after read
    LAST_ERROR[0] = 0;
    ptr
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

unsafe fn set_error(msg: &[u8]) {
    let len = msg.len().min(LAST_ERROR.len());
    LAST_ERROR[..len].copy_from_slice(&msg[..len]);
    if len < LAST_ERROR.len() {
        LAST_ERROR[len] = 0;
    }
}

unsafe fn cstr_to_str<'a>(s: *const c_char) -> &'a str {
    let mut len = 0;
    while *s.add(len) != 0 { len += 1; }
    let bytes = core::slice::from_raw_parts(s as *const u8, len);
    core::str::from_utf8_unchecked(bytes)
}
