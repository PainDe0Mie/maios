//! POSIX `<dirent.h>` directory traversal for MaiOS.

use libc::{c_int, c_char};
use errno::*;
use core::ptr;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use spin::Mutex;
use core::sync::atomic::{AtomicU32, Ordering};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

pub const DT_UNKNOWN: u8 = 0;
pub const DT_REG: u8 = 8;
pub const DT_DIR: u8 = 4;

#[repr(C)]
pub struct dirent {
    pub d_ino: u64,
    pub d_off: i64,
    pub d_reclen: u16,
    pub d_type: u8,
    pub d_name: [c_char; 256],
}

pub struct DIR {
    entries: Vec<DirEntry>,
    pos: usize,
    current: dirent,
}

struct DirEntry {
    name: String,
    is_dir: bool,
}

static NEXT_DIR_ID: AtomicU32 = AtomicU32::new(1);

// ---------------------------------------------------------------------------
// Functions
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn opendir(name: *const c_char) -> *mut DIR {
    if name.is_null() {
        errno = EINVAL;
        return ptr::null_mut();
    }

    let path_str = cstr_to_str(name);

    // Resolve directory in Theseus VFS
    let dir_ref = match resolve_dir(path_str) {
        Some(d) => d,
        None => {
            errno = ENOENT;
            return ptr::null_mut();
        }
    };

    // List entries
    let mut entries = Vec::new();
    let locked = dir_ref.lock();
    for child_name in locked.list() {
        let is_dir = locked.get(&child_name)
            .map(|fod| matches!(fod, path::FileOrDir::Dir(_)))
            .unwrap_or(false);
        entries.push(DirEntry { name: child_name, is_dir });
    }

    let dir = alloc::boxed::Box::new(DIR {
        entries,
        pos: 0,
        current: core::mem::zeroed(),
    });
    alloc::boxed::Box::into_raw(dir)
}

#[no_mangle]
pub unsafe extern "C" fn readdir(dirp: *mut DIR) -> *mut dirent {
    if dirp.is_null() { return ptr::null_mut(); }
    let d = &mut *dirp;

    if d.pos >= d.entries.len() {
        return ptr::null_mut(); // End of directory
    }

    let entry = &d.entries[d.pos];
    d.pos += 1;

    // Fill the dirent struct
    d.current.d_ino = d.pos as u64;
    d.current.d_off = d.pos as i64;
    d.current.d_reclen = core::mem::size_of::<dirent>() as u16;
    d.current.d_type = if entry.is_dir { DT_DIR } else { DT_REG };

    // Copy name
    let name_bytes = entry.name.as_bytes();
    let copy_len = name_bytes.len().min(255);
    for i in 0..copy_len {
        d.current.d_name[i] = name_bytes[i] as c_char;
    }
    d.current.d_name[copy_len] = 0;

    &mut d.current
}

#[no_mangle]
pub unsafe extern "C" fn closedir(dirp: *mut DIR) -> c_int {
    if dirp.is_null() { return -1; }
    let _ = alloc::boxed::Box::from_raw(dirp);
    0
}

#[no_mangle]
pub unsafe extern "C" fn rewinddir(dirp: *mut DIR) {
    if !dirp.is_null() {
        (*dirp).pos = 0;
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

unsafe fn cstr_to_str<'a>(s: *const c_char) -> &'a str {
    let mut len = 0;
    while *s.add(len) != 0 { len += 1; }
    let bytes = core::slice::from_raw_parts(s as *const u8, len);
    core::str::from_utf8_unchecked(bytes)
}

fn resolve_dir(path_str: &str) -> Option<path::DirRef> {
    let cwd = task::get_my_current_task()
        .and_then(|t| t.get_env().ok())
        .and_then(|env| env.lock().get_cwd().cloned());

    let p = path::Path::new(alloc::string::String::from(path_str));
    let dir_ref = cwd.unwrap_or_else(|| root::get_root().clone());

    match p.get(&dir_ref) {
        Some(path::FileOrDir::Dir(d)) => Some(d),
        _ => None,
    }
}
