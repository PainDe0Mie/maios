//! Syscall dispatcher for MaiOS.
//!
//! This crate sets up the x86_64 `SYSCALL`/`SYSRET` mechanism via MSRs
//! and provides the low-level entry point that dispatches to either
//! the Linux, Windows, or native MaiOS syscall handler based on the
//! task's execution mode.
//!
//! ## Architecture
//!
//! MaiOS supports three syscall ABIs:
//!
//! - **Native MaiOS ABI**: Uses MaiOS syscall numbers, dispatched directly
//!   to the unified `maios_syscall` table. Arguments in RDI, RSI, RDX, R10, R8, R9.
//!
//! - **Linux ABI**: Syscall number in RAX. Arguments in RDI, RSI, RDX, R10, R8, R9.
//!   Translated via `linux_syscall` mapper to MaiOS numbers.
//!
//! - **Windows NT ABI**: Syscall number in RAX (service table index in bits 12..13).
//!   Arguments in R10 (original RCX), RDX, R8, R9.
//!   Translated via `windows_syscall` mapper to MaiOS numbers.
//!
//! ## Performance
//!
//! ExecMode is stored as an `AtomicU8` directly in the Task struct —
//! zero lock contention on the syscall hot path.

#![no_std]
#![feature(naked_functions)]
#![feature(asm_const)]

extern crate alloc;

use log::{info, debug};
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// Whether the syscall subsystem has been initialized.
static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// The execution mode of a userspace process, determining which syscall ABI it uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecMode {
    /// Native MaiOS application — uses MaiOS syscall numbers directly.
    Native = 0,
    /// Linux ELF binary — uses Linux syscall ABI.
    Linux = 1,
    /// Windows PE binary — uses Windows NT syscall ABI.
    Windows = 2,
}

/// Saved register state from a syscall entry.
#[repr(C)]
#[derive(Debug, Clone)]
pub struct SyscallFrame {
    pub rax: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub r10: u64,
    pub r8: u64,
    pub r9: u64,
    pub rcx: u64,
    pub r11: u64,
    pub rsp: u64,
}

/// Initialize the syscall subsystem.
///
/// Sets up the x86_64 MSRs for SYSCALL/SYSRET and initializes
/// the unified syscall table.
pub fn init() -> Result<(), &'static str> {
    if INITIALIZED.load(Ordering::SeqCst) {
        return Ok(());
    }

    // Initialize the unified syscall table first
    maios_syscall::init();

    info!("Initializing syscall subsystem...");

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

    // IA32_EFER (0xC0000080): Set SCE bit (bit 0)
    let efer_lo: u32;
    let efer_hi: u32;
    asm!(
        "rdmsr",
        in("ecx") 0xC000_0080u32,
        out("eax") efer_lo,
        out("edx") efer_hi,
    );
    let efer = ((efer_hi as u64) << 32) | (efer_lo as u64);
    let new_efer = efer | 1;
    asm!(
        "wrmsr",
        in("ecx") 0xC000_0080u32,
        in("eax") new_efer as u32,
        in("edx") (new_efer >> 32) as u32,
    );

    // IA32_STAR (0xC0000081): Set segment selectors
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

    // IA32_FMASK (0xC0000084): Mask IF, DF, TF on entry
    let fmask: u64 = (1 << 9) | (1 << 10) | (1 << 8);
    asm!(
        "wrmsr",
        in("ecx") 0xC000_0084u32,
        in("eax") fmask as u32,
        in("edx") (fmask >> 32) as u32,
    );
}

/// Naked assembly entry point for SYSCALL.
#[naked]
unsafe extern "C" fn syscall_entry_naked() {
    core::arch::asm!(
        "mov gs:[0x0], rsp",
        "mov rsp, gs:[0x8]",

        "push 0",
        "push r11",
        "push rcx",
        "push r9",
        "push r8",
        "push r10",
        "push rdx",
        "push rsi",
        "push rdi",
        "push rax",

        "mov rax, gs:[0x0]",
        "mov [rsp + 72], rax",

        "mov rdi, rsp",
        "call {dispatcher}",

        "mov rax, [rsp + 0]",
        "mov rdi, [rsp + 8]",
        "mov rsi, [rsp + 16]",
        "mov rdx, [rsp + 24]",
        "mov r10, [rsp + 32]",
        "mov r8, [rsp + 40]",
        "mov r9, [rsp + 48]",
        "mov rcx, [rsp + 56]",
        "mov r11, [rsp + 64]",

        "mov rsp, [rsp + 72]",

        "sysretq",

        dispatcher = sym syscall_dispatcher,
        options(noreturn),
    );
}

/// Register the execution mode for a task.
///
/// Should be called when loading a binary (ELF → Linux, PE → Windows).
/// Uses AtomicU8 in the Task struct — zero lock contention.
pub fn set_task_exec_mode(task_id: usize, mode: ExecMode) {
    debug!("Setting ExecMode::{:?} for task {}", mode, task_id);
    if let Some(task_ref) = task::get_task(task_id) {
        task_ref.0.exec_mode.store(mode as u8, Ordering::Release);
    }
}

/// Remove the execution mode for a task (reset to Native on task exit).
pub fn remove_task_exec_mode(task_id: usize) {
    if let Some(task_ref) = task::get_task(task_id) {
        task_ref.0.exec_mode.store(ExecMode::Native as u8, Ordering::Release);
    }
}

/// Get the execution mode for the current task.
///
/// Lock-free: reads an AtomicU8 from the Task struct.
/// Single instruction, zero contention.
fn current_exec_mode() -> ExecMode {
    let mode = task::with_current_task(|t| {
        t.0.exec_mode.load(Ordering::Relaxed)
    }).unwrap_or(ExecMode::Linux as u8);

    match mode {
        0 => ExecMode::Native,
        1 => ExecMode::Linux,
        2 => ExecMode::Windows,
        _ => ExecMode::Linux, // fallback
    }
}

/// High-level syscall dispatcher called from assembly.
///
/// Determines the execution mode of the calling task and routes
/// the syscall to the appropriate handler.
#[no_mangle]
extern "C" fn syscall_dispatcher(frame: &mut SyscallFrame) {
    let syscall_num = frame.rax;

    let result = match current_exec_mode() {
        ExecMode::Windows => {
            // Windows NT ABI: R10=arg0, RDX=arg1, R8=arg2, R9=arg3
            windows_syscall::handle_syscall(
                syscall_num,
                frame.r10,
                frame.rdx,
                frame.r8,
                frame.r9,
                0,
                0,
            )
        }
        ExecMode::Linux => {
            // Linux ABI: RDI=arg0, RSI=arg1, RDX=arg2, R10=arg3, R8=arg4, R9=arg5
            linux_syscall::handle_syscall(
                syscall_num,
                frame.rdi,
                frame.rsi,
                frame.rdx,
                frame.r10,
                frame.r8,
                frame.r9,
            )
        }
        ExecMode::Native => {
            // MaiOS native: syscall number is a MaiOS number directly.
            // Uses Linux register convention for arguments.
            let nr = syscall_num as u16;
            let result = maios_syscall::dispatch(
                nr,
                frame.rdi,
                frame.rsi,
                frame.rdx,
                frame.r10,
                frame.r8,
                frame.r9,
            );
            maios_syscall::error::result_to_linux(result)
        }
    };

    frame.rax = result as u64;
}

/// Initialize the syscall subsystem for an Application Processor (AP).
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
