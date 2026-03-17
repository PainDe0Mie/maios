//! Syscall dispatcher for MaiOS.
//!
//! This crate sets up the x86_64 `SYSCALL`/`SYSRET` mechanism via MSRs
//! and provides the low-level entry point that dispatches to either
//! the Linux or Windows syscall handler based on the calling convention.
//!
//! ## Architecture
//!
//! MaiOS supports two syscall ABIs natively:
//!
//! - **Linux ABI**: Uses `syscall` instruction with syscall number in RAX.
//!   Arguments in RDI, RSI, RDX, R10, R8, R9.
//!
//! - **Windows NT ABI**: Uses `syscall` instruction with syscall number in RAX.
//!   Arguments on the stack (shadow space convention) or in RCX, RDX, R8, R9.
//!   NT syscall numbers have a service table index in bits 12..13.
//!
//! The dispatcher determines which ABI to use based on the task's registered
//! execution mode (set when loading a Linux ELF or Windows PE binary).

#![no_std]
#![feature(naked_functions)]
#![feature(asm_const)]

use log::{info, debug};
use core::sync::atomic::{AtomicBool, Ordering};

/// Whether the syscall subsystem has been initialized.
static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// The execution mode of a userspace process, determining which syscall ABI it uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecMode {
    /// Native MaiOS application (direct kernel calls, no syscall translation).
    Native,
    /// Linux ELF binary — uses Linux syscall ABI.
    Linux,
    /// Windows PE binary — uses Windows NT syscall ABI.
    Windows,
}

/// Saved register state from a syscall entry.
///
/// This struct captures the userspace register context when a syscall is invoked
/// via the `SYSCALL` instruction. It is passed to the high-level dispatcher.
#[repr(C)]
#[derive(Debug, Clone)]
pub struct SyscallFrame {
    /// RAX — syscall number
    pub rax: u64,
    /// RDI — arg0 (Linux ABI)
    pub rdi: u64,
    /// RSI — arg1 (Linux ABI)
    pub rsi: u64,
    /// RDX — arg2 (Linux/Windows ABI)
    pub rdx: u64,
    /// R10 — arg3 (Linux ABI, replaces RCX which is clobbered by SYSCALL)
    pub r10: u64,
    /// R8  — arg4
    pub r8: u64,
    /// R9  — arg5
    pub r9: u64,
    /// RCX — saved RIP (set by SYSCALL instruction)
    pub rcx: u64,
    /// R11 — saved RFLAGS (set by SYSCALL instruction)
    pub r11: u64,
    /// RSP — userspace stack pointer (must be saved manually)
    pub rsp: u64,
}

/// Initialize the syscall subsystem.
///
/// This sets up the x86_64 Model-Specific Registers (MSRs) required for
/// the `SYSCALL`/`SYSRET` instruction pair:
///
/// - `IA32_STAR` (0xC0000081): Segment selectors for SYSCALL/SYSRET
/// - `IA32_LSTAR` (0xC0000082): RIP target for SYSCALL (64-bit)
/// - `IA32_FMASK` (0xC0000084): RFLAGS mask on SYSCALL entry
/// - `IA32_EFER` (0xC0000080): Enable SCE (syscall extensions) bit
pub fn init() -> Result<(), &'static str> {
    if INITIALIZED.load(Ordering::SeqCst) {
        return Ok(());
    }

    info!("Initializing syscall subsystem...");

    // Get the kernel and user segment selectors from the GDT.
    // STAR MSR format:
    //   [63:48] = SYSRET CS/SS base (user CS = this + 16, user SS = this + 8)
    //   [47:32] = SYSCALL CS/SS base (kernel CS = this, kernel SS = this + 8)
    //   [31:0]  = Reserved (SYSCALL EIP for 32-bit, unused in 64-bit)
    let kernel_cs = gdt::AvailableSegmentSelector::KernelCode
        .get()
        .ok_or("syscall::init: kernel code selector not available")?;
    let kernel_ds = gdt::AvailableSegmentSelector::KernelData
        .get()
        .ok_or("syscall::init: kernel data selector not available")?;
    let user_cs = gdt::AvailableSegmentSelector::UserCode64
        .get()
        .ok_or("syscall::init: user code 64 selector not available")?;
    let user_ds = gdt::AvailableSegmentSelector::UserData64
        .get()
        .ok_or("syscall::init: user data 64 selector not available")?;

    debug!(
        "Syscall selectors: kernel_cs={:#x} kernel_ds={:#x} user_cs={:#x} user_ds={:#x}",
        kernel_cs.0, kernel_ds.0, user_cs.0, user_ds.0,
    );

    unsafe {
        setup_syscall_msrs(kernel_cs.0, user_cs.0);
    }

    INITIALIZED.store(true, Ordering::SeqCst);
    info!("Syscall subsystem initialized successfully.");
    Ok(())
}

/// Write the MSRs to enable the SYSCALL/SYSRET instruction pair.
///
/// # Safety
/// Must only be called once during init, with valid GDT selectors.
unsafe fn setup_syscall_msrs(kernel_cs_index: u16, user_cs_index: u16) {
    use core::arch::asm;

    // IA32_EFER (0xC0000080): Set SCE bit (bit 0) to enable SYSCALL/SYSRET
    let efer_lo: u32;
    let efer_hi: u32;
    asm!(
        "rdmsr",
        in("ecx") 0xC000_0080u32,
        out("eax") efer_lo,
        out("edx") efer_hi,
    );
    let efer = ((efer_hi as u64) << 32) | (efer_lo as u64);
    let new_efer = efer | 1; // Set SCE (bit 0)
    asm!(
        "wrmsr",
        in("ecx") 0xC000_0080u32,
        in("eax") new_efer as u32,
        in("edx") (new_efer >> 32) as u32,
    );

    // IA32_STAR (0xC0000081): Set segment selectors
    // [63:48] = sysret_cs_ss_base — For SYSRET: CS = base+16 (64-bit), SS = base+8
    // [47:32] = syscall_cs_ss_base — For SYSCALL: CS = base, SS = base+8
    //
    // The user_cs_index should point to the 32-bit user code segment.
    // SYSRET in 64-bit mode adds 16 to get the 64-bit CS.
    // So we need: sysret_base = user_cs_64 - 16 (in selector units)
    // Since selectors are in units of 8 bytes, subtract 2 selector entries.
    let sysret_base = (user_cs_index & !0x3).wrapping_sub(16);
    let syscall_base = kernel_cs_index & !0x3;
    let star_value: u64 = ((sysret_base as u64) << 48) | ((syscall_base as u64) << 32);
    asm!(
        "wrmsr",
        in("ecx") 0xC000_0081u32,
        in("eax") star_value as u32,
        in("edx") (star_value >> 32) as u32,
    );

    // IA32_LSTAR (0xC0000082): Set the 64-bit SYSCALL entry point
    let handler_addr = syscall_entry_naked as usize as u64;
    asm!(
        "wrmsr",
        in("ecx") 0xC000_0082u32,
        in("eax") handler_addr as u32,
        in("edx") (handler_addr >> 32) as u32,
    );

    // IA32_FMASK (0xC0000084): Mask RFLAGS on SYSCALL entry
    // Clear IF (bit 9) to disable interrupts, clear DF (bit 10), clear TF (bit 8)
    let fmask: u64 = (1 << 9) | (1 << 10) | (1 << 8); // IF | DF | TF
    asm!(
        "wrmsr",
        in("ecx") 0xC000_0084u32,
        in("eax") fmask as u32,
        in("edx") (fmask >> 32) as u32,
    );
}

/// The naked assembly entry point for SYSCALL.
///
/// On SYSCALL entry:
/// - RCX = saved RIP (return address)
/// - R11 = saved RFLAGS
/// - CS/SS are set to kernel segments
/// - RSP is NOT changed (still points to user stack!)
///
/// We must:
/// 1. Switch to the kernel stack (from TSS RSP0)
/// 2. Save user registers
/// 3. Call the Rust dispatcher
/// 4. Restore registers and SYSRET back
#[naked]
unsafe extern "C" fn syscall_entry_naked() {
    core::arch::asm!(
        // Swap to kernel stack: save user RSP in a scratch register,
        // then load kernel RSP from the per-CPU area.
        // For now, we use SWAPGS to access per-CPU data if available,
        // or we can use a dedicated memory location.
        //
        // Simple approach: use a per-CPU scratch area via GS base.
        // TODO: Implement proper per-CPU storage. For now, use a
        // simple global scratch space (single-core safe).

        // Save user RSP to a scratch location
        "mov gs:[0x0], rsp",       // Save user RSP to per-CPU scratch

        // Load kernel RSP (TODO: proper per-CPU kernel stack pointer)
        // For now we use a temporary kernel stack area
        "mov rsp, gs:[0x8]",       // Load kernel RSP from per-CPU area

        // Build SyscallFrame on kernel stack
        "push 0",                   // placeholder for rsp (will fill from gs:[0x0])
        "push r11",                 // saved RFLAGS
        "push rcx",                 // saved RIP
        "push r9",                  // arg5
        "push r8",                  // arg4
        "push r10",                 // arg3 (Linux) / original RCX (Windows)
        "push rdx",                 // arg2
        "push rsi",                 // arg1
        "push rdi",                 // arg0
        "push rax",                 // syscall number

        // Fill in the saved user RSP from scratch area
        "mov rax, gs:[0x0]",
        "mov [rsp + 72], rax",     // offset 9*8 = 72 for rsp field

        // Call the Rust dispatcher with pointer to SyscallFrame
        "mov rdi, rsp",            // first arg = pointer to SyscallFrame
        "call {dispatcher}",

        // Return value is in RAX (syscall result)
        // Restore registers from SyscallFrame
        "mov rax, [rsp + 0]",     // restore RAX (now contains return value, set by dispatcher)
        "mov rdi, [rsp + 8]",
        "mov rsi, [rsp + 16]",
        "mov rdx, [rsp + 24]",
        "mov r10, [rsp + 32]",
        "mov r8, [rsp + 40]",
        "mov r9, [rsp + 48]",
        "mov rcx, [rsp + 56]",    // saved RIP for SYSRET
        "mov r11, [rsp + 64]",    // saved RFLAGS for SYSRET

        // Restore user RSP
        "mov rsp, [rsp + 72]",

        // Return to userspace
        "sysretq",

        dispatcher = sym syscall_dispatcher,
        options(noreturn),
    );
}

/// High-level syscall dispatcher called from assembly.
///
/// Determines the execution mode of the calling task and routes
/// the syscall to the appropriate handler (Linux or Windows).
///
/// Returns the syscall result in the frame's RAX field.
#[no_mangle]
extern "C" fn syscall_dispatcher(frame: &mut SyscallFrame) {
    let syscall_num = frame.rax;

    // TODO: Determine exec mode from the current task's metadata.
    // For now, we detect based on syscall number ranges:
    // - Linux syscalls: 0..~450 (standard Linux x86_64 syscall table)
    // - Windows NT syscalls: numbers with service table bits set,
    //   or we check task metadata.
    //
    // The proper approach is to check the task's ExecMode field,
    // which is set when the binary is loaded (ELF → Linux, PE → Windows).

    // For the initial implementation, default to Linux ABI detection
    // since it's the most common use case.

    // Try Linux first (most common case for compatibility)
    let result = linux_syscall::handle_syscall(
        syscall_num,
        frame.rdi,
        frame.rsi,
        frame.rdx,
        frame.r10,
        frame.r8,
        frame.r9,
    );

    // Store result back
    frame.rax = result as u64;
}

/// Initialize the syscall subsystem for an Application Processor (AP).
///
/// Each CPU core needs its own SYSCALL MSR configuration since
/// IA32_LSTAR and other MSRs are per-core.
pub fn init_ap() -> Result<(), &'static str> {
    if !INITIALIZED.load(Ordering::SeqCst) {
        return Err("syscall::init_ap: BSP has not initialized syscall subsystem yet");
    }

    let kernel_cs = gdt::AvailableSegmentSelector::KernelCode
        .get()
        .ok_or("syscall::init_ap: kernel code selector not available")?;
    let user_cs = gdt::AvailableSegmentSelector::UserCode64
        .get()
        .ok_or("syscall::init_ap: user code 64 selector not available")?;

    unsafe {
        setup_syscall_msrs(kernel_cs.0, user_cs.0);
    }

    debug!("Syscall MSRs configured for AP CPU {}", cpu::current_cpu());
    Ok(())
}
