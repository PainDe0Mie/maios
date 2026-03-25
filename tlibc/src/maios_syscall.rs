//! MaiOS native syscall bridge for C programs.
//!
//! Since tlibc runs in kernel space (Theseus single-address-space model),
//! we dispatch syscalls directly to the kernel's `maios_syscall` crate
//! rather than going through a `syscall` instruction.

use libc::c_int;

/// Bridge function called by C code via `maios.h` inline functions.
///
/// Routes MaiOS native syscall numbers (0x0000..0x08FF) to the kernel
/// dispatcher. This avoids the `int 0x80` / `syscall` overhead entirely
/// since we're already in kernel space.
#[no_mangle]
pub unsafe extern "C" fn _maios_syscall(
    nr: c_int,
    a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64,
) -> i64 {
    match maios_syscall::dispatch(nr as u16, a0, a1, a2, a3, a4, a5) {
        Ok(val) => val as i64,
        Err(e) => e.to_linux_errno(),
    }
}
