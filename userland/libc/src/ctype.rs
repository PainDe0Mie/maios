//! C `<ctype.h>` character classification functions.

use libc::c_int;

#[no_mangle]
pub extern "C" fn isalpha(c: c_int) -> c_int {
    ((c >= 0x41 && c <= 0x5A) || (c >= 0x61 && c <= 0x7A)) as c_int
}

#[no_mangle]
pub extern "C" fn isdigit(c: c_int) -> c_int {
    (c >= 0x30 && c <= 0x39) as c_int
}

#[no_mangle]
pub extern "C" fn isalnum(c: c_int) -> c_int {
    (isalpha(c) != 0 || isdigit(c) != 0) as c_int
}

#[no_mangle]
pub extern "C" fn isspace(c: c_int) -> c_int {
    (c == 0x20 || (c >= 0x09 && c <= 0x0D)) as c_int
}

#[no_mangle]
pub extern "C" fn isupper(c: c_int) -> c_int {
    (c >= 0x41 && c <= 0x5A) as c_int
}

#[no_mangle]
pub extern "C" fn islower(c: c_int) -> c_int {
    (c >= 0x61 && c <= 0x7A) as c_int
}

#[no_mangle]
pub extern "C" fn toupper(c: c_int) -> c_int {
    if islower(c) != 0 { c - 32 } else { c }
}

#[no_mangle]
pub extern "C" fn tolower(c: c_int) -> c_int {
    if isupper(c) != 0 { c + 32 } else { c }
}

#[no_mangle]
pub extern "C" fn isprint(c: c_int) -> c_int {
    (c >= 0x20 && c <= 0x7E) as c_int
}

#[no_mangle]
pub extern "C" fn isgraph(c: c_int) -> c_int {
    (c > 0x20 && c <= 0x7E) as c_int
}

#[no_mangle]
pub extern "C" fn ispunct(c: c_int) -> c_int {
    (isgraph(c) != 0 && isalnum(c) == 0) as c_int
}

#[no_mangle]
pub extern "C" fn isxdigit(c: c_int) -> c_int {
    (isdigit(c) != 0 || (c >= 0x41 && c <= 0x46) || (c >= 0x61 && c <= 0x66)) as c_int
}

#[no_mangle]
pub extern "C" fn iscntrl(c: c_int) -> c_int {
    (c < 0x20 || c == 0x7F) as c_int
}

#[no_mangle]
pub extern "C" fn isblank(c: c_int) -> c_int {
    (c == 0x20 || c == 0x09) as c_int
}

#[no_mangle]
pub extern "C" fn isascii(c: c_int) -> c_int {
    (c >= 0 && c <= 0x7F) as c_int
}

#[no_mangle]
pub extern "C" fn toascii(c: c_int) -> c_int {
    c & 0x7F
}
