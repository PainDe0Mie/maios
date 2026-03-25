//! C `<setjmp.h>` for MaiOS (x86_64).
//!
//! setjmp/longjmp are implemented in assembly since they manipulate
//! registers directly. Here we provide the Rust-side type definitions
//! and extern declarations for linking.

use libc::c_int;

/// jmp_buf: saves callee-saved registers on x86_64.
/// Layout: [rbx, rbp, r12, r13, r14, r15, rsp, rip] = 8 × 8 = 64 bytes.
#[repr(C, align(16))]
pub struct jmp_buf {
    pub regs: [u64; 8],
}

/// setjmp saves the current execution context into `env`.
/// Returns 0 on direct call, or the value passed to longjmp on return.
///
/// This MUST be implemented in assembly for correctness (it needs to
/// capture the caller's registers, not its own). We provide a minimal
/// inline asm implementation here.
#[no_mangle]
pub unsafe extern "C" fn setjmp(env: *mut jmp_buf) -> c_int {
    // Save callee-saved registers + RSP + return address
    core::arch::asm!(
        "mov [rdi],      rbx",
        "mov [rdi + 8],  rbp",
        "mov [rdi + 16], r12",
        "mov [rdi + 24], r13",
        "mov [rdi + 32], r14",
        "mov [rdi + 40], r15",
        "lea rax, [rsp + 8]",   // RSP after setjmp returns
        "mov [rdi + 48], rax",
        "mov rax, [rsp]",       // return address
        "mov [rdi + 56], rax",
        "xor eax, eax",         // return 0
        in("rdi") env,
        out("rax") _,
        options(nostack),
    );
    0
}

/// longjmp restores the context saved by setjmp and makes setjmp
/// return `val` (or 1 if val == 0).
#[no_mangle]
pub unsafe extern "C" fn longjmp(env: *mut jmp_buf, val: c_int) -> ! {
    let ret_val = if val == 0 { 1 } else { val };
    core::arch::asm!(
        "mov rbx, [rdi]",
        "mov rbp, [rdi + 8]",
        "mov r12, [rdi + 16]",
        "mov r13, [rdi + 24]",
        "mov r14, [rdi + 32]",
        "mov r15, [rdi + 40]",
        "mov rsp, [rdi + 48]",
        "mov rax, {0:e}",       // set return value
        "jmp [rdi + 56]",       // jump to saved return address
        in(reg) ret_val,
        in("rdi") env,
        options(noreturn, nostack),
    );
}
