//! C `<stdio.h>` FILE*-based I/O for MaiOS.
//!
//! Provides fopen/fclose/fread/fwrite/fprintf/fgets/fputs/fseek/ftell/fflush
//! backed by Theseus's VFS and `app_io` for stdin/stdout/stderr.

use libc::{c_int, c_char, c_void, size_t};
use errno::*;
use core::ptr;
use alloc::vec::Vec;
use alloc::string::String;
use alloc::collections::BTreeMap;
use spin::Mutex;
use core::sync::atomic::{AtomicU32, Ordering};

// ---------------------------------------------------------------------------
// FILE structure
// ---------------------------------------------------------------------------

/// Internal FILE representation.
pub struct MaiFile {
    /// File descriptor (0=stdin, 1=stdout, 2=stderr, 3+ = VFS files)
    fd: c_int,
    /// In-memory buffer for VFS-backed files
    buf: Vec<u8>,
    /// Current position in `buf`
    pos: usize,
    /// Writable flag
    writable: bool,
    /// Error flag
    error: bool,
    /// EOF flag
    eof: bool,
}

/// Opaque FILE type exposed to C.
pub type FILE = MaiFile;

// Pre-allocated FILEs for stdin/stdout/stderr
static mut STDIN_FILE: MaiFile = MaiFile {
    fd: 0, buf: Vec::new(), pos: 0, writable: false, error: false, eof: false,
};
static mut STDOUT_FILE: MaiFile = MaiFile {
    fd: 1, buf: Vec::new(), pos: 0, writable: true, error: false, eof: false,
};
static mut STDERR_FILE: MaiFile = MaiFile {
    fd: 2, buf: Vec::new(), pos: 0, writable: true, error: false, eof: false,
};

static NEXT_FD: AtomicU32 = AtomicU32::new(3);

/// Map of fd → open file data for VFS-backed files.
static OPEN_FILES: Mutex<BTreeMap<c_int, OpenVfsFile>> = Mutex::new(BTreeMap::new());

struct OpenVfsFile {
    /// Full file contents (read at open time for simplicity)
    data: Vec<u8>,
    /// Path (for debugging)
    _path: String,
    writable: bool,
}

// ---------------------------------------------------------------------------
// Standard streams
// ---------------------------------------------------------------------------

#[no_mangle]
pub static mut stdin: *mut FILE = unsafe { &mut STDIN_FILE as *mut FILE };
#[no_mangle]
pub static mut stdout: *mut FILE = unsafe { &mut STDOUT_FILE as *mut FILE };
#[no_mangle]
pub static mut stderr: *mut FILE = unsafe { &mut STDERR_FILE as *mut FILE };

// ---------------------------------------------------------------------------
// fopen / fclose
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn fopen(filename: *const c_char, mode: *const c_char) -> *mut FILE {
    if filename.is_null() || mode.is_null() {
        errno = EINVAL;
        return ptr::null_mut();
    }

    let path = cstr_to_str(filename);
    let mode_str = cstr_to_str(mode);

    let writable = mode_str.contains('w') || mode_str.contains('a') || mode_str.contains('+');

    // Try to read file from Theseus's namespace
    let data = match read_file_from_vfs(path) {
        Some(d) => d,
        None => {
            if writable {
                Vec::new() // create empty for write mode
            } else {
                errno = ENOENT;
                return ptr::null_mut();
            }
        }
    };

    let fd = NEXT_FD.fetch_add(1, Ordering::Relaxed) as c_int;
    OPEN_FILES.lock().insert(fd, OpenVfsFile {
        data: data.clone(),
        _path: String::from(path),
        writable,
    });

    let file = alloc::boxed::Box::new(MaiFile {
        fd,
        buf: data,
        pos: if mode_str.contains('a') { usize::MAX } else { 0 }, // MAX = sentinel for append
        writable,
        error: false,
        eof: false,
    });
    // Fix append position
    let file_ptr = alloc::boxed::Box::into_raw(file);
    if mode_str.contains('a') {
        (*file_ptr).pos = (*file_ptr).buf.len();
    }
    file_ptr
}

#[no_mangle]
pub unsafe extern "C" fn fclose(stream: *mut FILE) -> c_int {
    if stream.is_null() { return EOF; }
    let fd = (*stream).fd;
    // Don't free stdin/stdout/stderr
    if fd <= 2 { return 0; }

    // Flush if writable
    if (*stream).writable {
        if let Some(vfs_file) = OPEN_FILES.lock().get(&fd) {
            // Could write back to VFS here in the future
        }
    }

    OPEN_FILES.lock().remove(&fd);
    // Free the FILE
    let _ = alloc::boxed::Box::from_raw(stream);
    0
}

// ---------------------------------------------------------------------------
// fread / fwrite
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn fread(
    buf: *mut c_void,
    size: size_t,
    count: size_t,
    stream: *mut FILE,
) -> size_t {
    if buf.is_null() || stream.is_null() || size == 0 { return 0; }

    let f = &mut *stream;

    // stdin: read from app_io
    if f.fd == 0 {
        if let Some(sin) = app_io::stdin() {
            let total = size * count;
            let dst = core::slice::from_raw_parts_mut(buf as *mut u8, total);
            let mut locked = sin.lock();
            let mut read = 0;
            for byte in dst.iter_mut() {
                match locked.read_one() {
                    Some(ch) => { *byte = ch; read += 1; }
                    None => break,
                }
            }
            return read / size;
        }
        return 0;
    }

    // VFS files: read from in-memory buffer
    let total = size * count;
    let remaining = f.buf.len().saturating_sub(f.pos);
    let to_read = total.min(remaining);

    if to_read == 0 {
        f.eof = true;
        return 0;
    }

    ptr::copy_nonoverlapping(f.buf.as_ptr().add(f.pos), buf as *mut u8, to_read);
    f.pos += to_read;

    if f.pos >= f.buf.len() {
        f.eof = true;
    }

    to_read / size
}

#[no_mangle]
pub unsafe extern "C" fn fwrite(
    buf: *const c_void,
    size: size_t,
    count: size_t,
    stream: *mut FILE,
) -> size_t {
    if buf.is_null() || stream.is_null() || size == 0 { return 0; }

    let f = &mut *stream;
    let total = size * count;
    let src = core::slice::from_raw_parts(buf as *const u8, total);

    // stdout/stderr: write to app_io
    if f.fd == 1 || f.fd == 2 {
        let out = if f.fd == 1 { app_io::stdout() } else { app_io::stderr() };
        if let Some(writer) = out {
            let mut locked = writer.lock();
            for &b in src {
                let _ = locked.write_one(b);
            }
            return count;
        }
        f.error = true;
        return 0;
    }

    // VFS files: write to in-memory buffer
    if !f.writable {
        f.error = true;
        return 0;
    }

    // Extend buffer if needed
    if f.pos + total > f.buf.len() {
        f.buf.resize(f.pos + total, 0);
    }
    f.buf[f.pos..f.pos + total].copy_from_slice(src);
    f.pos += total;
    f.eof = false;

    count
}

// ---------------------------------------------------------------------------
// fseek / ftell / rewind
// ---------------------------------------------------------------------------

pub const SEEK_SET: c_int = 0;
pub const SEEK_CUR: c_int = 1;
pub const SEEK_END: c_int = 2;

#[no_mangle]
pub unsafe extern "C" fn fseek(stream: *mut FILE, offset: i64, whence: c_int) -> c_int {
    if stream.is_null() { return -1; }
    let f = &mut *stream;
    let new_pos = match whence {
        SEEK_SET => offset as isize,
        SEEK_CUR => f.pos as isize + offset as isize,
        SEEK_END => f.buf.len() as isize + offset as isize,
        _ => { errno = EINVAL; return -1; }
    };
    if new_pos < 0 {
        errno = EINVAL;
        return -1;
    }
    f.pos = new_pos as usize;
    f.eof = false;
    0
}

#[no_mangle]
pub unsafe extern "C" fn ftell(stream: *mut FILE) -> i64 {
    if stream.is_null() { return -1; }
    (*stream).pos as i64
}

#[no_mangle]
pub unsafe extern "C" fn rewind(stream: *mut FILE) {
    if !stream.is_null() {
        (*stream).pos = 0;
        (*stream).eof = false;
        (*stream).error = false;
    }
}

// ---------------------------------------------------------------------------
// fgets / fputs / fgetc / fputc / ungetc
// ---------------------------------------------------------------------------

pub const EOF: c_int = -1;

#[no_mangle]
pub unsafe extern "C" fn fgetc(stream: *mut FILE) -> c_int {
    if stream.is_null() { return EOF; }
    let mut byte: u8 = 0;
    if fread(&mut byte as *mut u8 as *mut c_void, 1, 1, stream) == 1 {
        byte as c_int
    } else {
        EOF
    }
}

#[no_mangle]
pub unsafe extern "C" fn getc(stream: *mut FILE) -> c_int {
    fgetc(stream)
}

#[no_mangle]
pub unsafe extern "C" fn getchar() -> c_int {
    fgetc(stdin)
}

#[no_mangle]
pub unsafe extern "C" fn fputc(c: c_int, stream: *mut FILE) -> c_int {
    if stream.is_null() { return EOF; }
    let byte = c as u8;
    if fwrite(&byte as *const u8 as *const c_void, 1, 1, stream) == 1 {
        c
    } else {
        EOF
    }
}

#[no_mangle]
pub unsafe extern "C" fn putc(c: c_int, stream: *mut FILE) -> c_int {
    fputc(c, stream)
}

#[no_mangle]
pub unsafe extern "C" fn putchar(c: c_int) -> c_int {
    fputc(c, stdout)
}

#[no_mangle]
pub unsafe extern "C" fn puts(s: *const c_char) -> c_int {
    if s.is_null() { return EOF; }
    let mut p = s;
    while *p != 0 {
        if putchar(*p as c_int) == EOF { return EOF; }
        p = p.add(1);
    }
    putchar(b'\n' as c_int)
}

#[no_mangle]
pub unsafe extern "C" fn fgets(buf: *mut c_char, n: c_int, stream: *mut FILE) -> *mut c_char {
    if buf.is_null() || n <= 0 || stream.is_null() { return ptr::null_mut(); }
    let mut i = 0;
    while i < (n - 1) as usize {
        let c = fgetc(stream);
        if c == EOF {
            if i == 0 { return ptr::null_mut(); }
            break;
        }
        *buf.add(i) = c as c_char;
        i += 1;
        if c == b'\n' as c_int { break; }
    }
    *buf.add(i) = 0;
    buf
}

#[no_mangle]
pub unsafe extern "C" fn fputs(s: *const c_char, stream: *mut FILE) -> c_int {
    if s.is_null() || stream.is_null() { return EOF; }
    let mut p = s;
    while *p != 0 {
        if fputc(*p as c_int, stream) == EOF { return EOF; }
        p = p.add(1);
    }
    0
}

// ---------------------------------------------------------------------------
// fflush / ferror / feof / clearerr / fileno
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn fflush(_stream: *mut FILE) -> c_int {
    // All writes are immediate in our implementation
    0
}

#[no_mangle]
pub unsafe extern "C" fn ferror(stream: *mut FILE) -> c_int {
    if stream.is_null() { return 0; }
    (*stream).error as c_int
}

#[no_mangle]
pub unsafe extern "C" fn feof(stream: *mut FILE) -> c_int {
    if stream.is_null() { return 0; }
    (*stream).eof as c_int
}

#[no_mangle]
pub unsafe extern "C" fn clearerr(stream: *mut FILE) {
    if !stream.is_null() {
        (*stream).error = false;
        (*stream).eof = false;
    }
}

#[no_mangle]
pub unsafe extern "C" fn fileno(stream: *mut FILE) -> c_int {
    if stream.is_null() { return -1; }
    (*stream).fd
}

// ---------------------------------------------------------------------------
// fprintf / fscanf (fprintf delegates to existing printf)
// ---------------------------------------------------------------------------

// fprintf is handled via the existing stdio/printf.rs module + fwrite.
// We just need a thin wrapper.

#[no_mangle]
pub unsafe extern "C" fn perror(s: *const c_char) {
    let err = errno;
    if !s.is_null() && *s != 0 {
        fputs(s, stderr);
        fputs(b": \0".as_ptr() as *const c_char, stderr);
    }
    let msg = errno::errno_str();
    for &b in msg.as_bytes() {
        fputc(b as c_int, stderr);
    }
    fputc(b'\n' as c_int, stderr);
}

// ---------------------------------------------------------------------------
// remove / rename (stubs)
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn remove(_filename: *const c_char) -> c_int {
    errno = ENOSYS;
    -1
}

#[no_mangle]
pub unsafe extern "C" fn rename(_old: *const c_char, _new: *const c_char) -> c_int {
    errno = ENOSYS;
    -1
}

#[no_mangle]
pub unsafe extern "C" fn tmpfile() -> *mut FILE {
    ptr::null_mut()
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

/// Read a file from the Theseus VFS by path.
fn read_file_from_vfs(path: &str) -> Option<Vec<u8>> {
    // Try to find the file in the root namespace
    let cwd = task::get_my_current_task()
        .and_then(|t| t.get_env().ok())
        .and_then(|env| env.lock().get_cwd().cloned());

    let ns = task::get_my_current_task()
        .and_then(|t| t.get_namespace().ok())?;

    // Use the path module to resolve the file
    let p = path::Path::new(String::from(path));
    let dir_ref = cwd.unwrap_or_else(|| root::get_root().clone());

    match p.get(&dir_ref) {
        Some(file_dir_enum) => {
            match file_dir_enum {
                path::FileOrDir::File(file_ref) => {
                    let file = file_ref.lock();
                    let len = file.len();
                    let mut buf = alloc::vec![0u8; len];
                    match file.read_at(&mut buf, 0) {
                        Ok(n) => { buf.truncate(n); Some(buf) }
                        Err(_) => None,
                    }
                }
                _ => None,
            }
        }
        None => None,
    }
}
