//! Exception handlers that are task-aware, and will kill a task on an exception.

#![no_std]
#![feature(abi_x86_interrupt)]

extern crate memory_swap;
extern crate memory_x86_64;

use log::{error, warn, debug, trace};
use memory::{VirtualAddress, Page};
use signal_handler::{Signal, SignalContext, ErrorCode};
use x86_64::{
    registers::control::Cr2,
    structures::idt::{
        InterruptStackFrame,
        PageFaultErrorCode
    },
};
use locked_idt::LockedIdt;
use fault_log::log_exception;


/// Initialize the given `idt` with fully-featured exception handlers.
/// 
/// This only sets the exception `Entry`s in the `IDT`, i.e.,
/// entries from `0` to `31` (inclusive).
/// Entries from `32` to `255` (inclusive) are not modified, 
/// as those are for custom OS-specfici interrupt handlers.
pub fn init(idt_ref: &'static LockedIdt) {
    { 
        let mut idt = idt_ref.lock(); // withholds interrupts

        // SET UP FIXED EXCEPTION HANDLERS
        idt.divide_error.set_handler_fn(divide_error_handler);
        idt.debug.set_handler_fn(debug_handler);
        idt.non_maskable_interrupt.set_handler_fn(nmi_handler);
        idt.breakpoint.set_handler_fn(breakpoint_handler);
        idt.overflow.set_handler_fn(overflow_handler);
        idt.bound_range_exceeded.set_handler_fn(bound_range_exceeded_handler);
        idt.invalid_opcode.set_handler_fn(invalid_opcode_handler);
        idt.device_not_available.set_handler_fn(device_not_available_handler);
        let options = idt.double_fault.set_handler_fn(double_fault_handler);
        unsafe { 
            options.set_stack_index(tss::DOUBLE_FAULT_IST_INDEX as u16);
        }
        // reserved: 0x09 coprocessor segment overrun exception
        idt.invalid_tss.set_handler_fn(invalid_tss_handler);
        idt.segment_not_present.set_handler_fn(segment_not_present_handler);
        idt.stack_segment_fault.set_handler_fn(stack_segment_fault_handler);
        idt.general_protection_fault.set_handler_fn(general_protection_fault_handler);
        idt.page_fault.set_handler_fn(page_fault_handler);
        // reserved: 0x0F
        idt.x87_floating_point.set_handler_fn(x87_floating_point_handler);
        idt.alignment_check.set_handler_fn(alignment_check_handler);
        idt.machine_check.set_handler_fn(machine_check_handler);
        idt.simd_floating_point.set_handler_fn(simd_floating_point_handler);
        idt.virtualization.set_handler_fn(virtualization_handler);
        // reserved: 0x15 - 0x1C
        idt.vmm_communication_exception.set_handler_fn(vmm_communication_exception_handler);
        idt.security_exception.set_handler_fn(security_exception_handler);
        // reserved: 0x1F
    }

    idt_ref.load();
}


static EMERGENCY_LOCK: core::sync::atomic::AtomicBool = 
    core::sync::atomic::AtomicBool::new(false);

fn emergency_print(s: &str) {
    #[cfg(target_arch = "x86_64")]
    {
        // Acquire spinlock
        while self::EMERGENCY_LOCK.compare_exchange(
            false, true,
            core::sync::atomic::Ordering::Acquire,
            core::sync::atomic::Ordering::Relaxed,
        ).is_err() {
            core::hint::spin_loop();
        }
        
        for byte in s.bytes() {
            unsafe {
                for _ in 0..10_000 {
                    let lsr = x86_64::instructions::port::PortReadOnly::<u8>::new(0x3F8 + 5).read();
                    if lsr & 0x20 != 0 { break; }
                    core::hint::spin_loop();
                }
                x86_64::instructions::port::PortWriteOnly::<u8>::new(0x3F8).write(byte);
            }
        }
        
        EMERGENCY_LOCK.store(false, core::sync::atomic::Ordering::Release);
    }
}

struct EmergencyWriter;
impl core::fmt::Write for EmergencyWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        emergency_print(s);
        Ok(())
    }
}

macro_rules! emergency_println {
    ($fmt:expr) => {{
        emergency_print(concat!($fmt, "\n"));
    }};
    ($fmt:expr, $($arg:tt)*) => {{
        use core::fmt::Write;
        let _ = core::write!(EmergencyWriter, concat!($fmt, "\n"), $($arg)*);
    }};
}

/// Kills the current task (the one that caused an exception) by unwinding it.
/// 
/// # Important Note
/// Currently, unwinding a task after an exception does not fully work like it does for panicked tasks.
/// The problem is that unwinding cleanup routines (landing pads) are generated *only if* a panic can actually occur. 
/// Since machine exceptions can occur anywhere at any time (beneath the language level),
/// 
/// Currently, what will happen is that all stack frames will be unwound properly **except**
/// for the one during which the exception actually occurred; 
/// the "excepted"/interrupted frame may be cleaned up properly, but it is unlikely. 
/// 
/// However, stack traces / backtraces work, so we are correctly traversing call stacks with exception frames.
/// 

#[inline(never)]
fn kill_and_halt(
    exception_number: u8,
    stack_frame: &InterruptStackFrame,
    error_code: Option<ErrorCode>,
    print_stack_trace: bool
) {
    {
        let (err, addr) = match error_code {
            Some(ErrorCode::PageFaultError {accessed_address, pf_error}) => (Some(pf_error.bits()), Some(accessed_address)),
            Some(ErrorCode::Other(e)) => (Some(e), None),
            None => (None, None),
        };
        log_exception(exception_number, stack_frame.instruction_pointer.as_u64() as usize, err, addr);
    }


    #[cfg(unwind_exceptions)] {
        emergency_println!("Unwinding {:?} due to exception {}.", task::get_my_current_task(), exception_number);
    }
    #[cfg(not(unwind_exceptions))] {
        emergency_println!("Killing task without unwinding {:?} due to exception {}. (cfg `unwind_exceptions` is not set.)", task::get_my_current_task(), exception_number);
    }
    
    // Dump some info about the this loaded app crate
    // and test out using debug info for recovery
    if false {
        let curr_task = task::get_my_current_task().expect("kill_and_halt: no current task");
        let app_crate = curr_task.app_crate.as_ref().expect("kill_and_halt: no app_crate").clone_shallow();
        let debug_symbols_file = {
            let krate = app_crate.lock_as_ref();
            trace!("============== Crate {} =================", krate.crate_name);
            for s in krate.sections.values() {
                trace!("   {:?}", s);
            }
            krate.debug_symbols_file.clone()
        };

        if false {
            let mut debug = debug_info::DebugSymbols::Unloaded(debug_symbols_file);
            let debug_sections = debug.load(&app_crate, &curr_task.namespace).unwrap();
            let instr_ptr = stack_frame.instruction_pointer.as_u64() as usize - 1; // points to the next instruction (at least for a page fault)

            let res = debug_sections.find_subprogram_containing(VirtualAddress::new_canonical(instr_ptr));
            debug!("Result of find_subprogram_containing: {:?}", res);
        }
    }

    // print a stack trace
    if print_stack_trace {
        emergency_println!("------------------ Stack Trace (DWARF) ---------------------------");
        let stack_trace_result = stack_trace::stack_trace(
            &mut |stack_frame, stack_frame_iter| {
                let symbol_offset = stack_frame_iter.namespace().get_section_containing_address(
                    VirtualAddress::new_canonical(stack_frame.call_site_address() as usize),
                    false
                ).map(|(sec, offset)| (sec.name.clone(), offset));
                if let Some((symbol_name, offset)) = symbol_offset {
                    emergency_println!("  {:>#018X} in {} + {:#X}", stack_frame.call_site_address(), symbol_name, offset);
                } else {
                    emergency_println!("  {:>#018X} in ??", stack_frame.call_site_address());
                }
                true
            },
            None,
        );
        match stack_trace_result {
            Ok(()) => { emergency_println!("  Beginning of stack"); }
            Err(e) => { emergency_println!("  {}", e); }
        }
        emergency_println!("---------------------- End of Stack Trace ------------------------");
    }

    let cause = task::KillReason::Exception(exception_number);

    // Call this task's kill handler, if it has one.
    if let Some(ref kh_func) = task::take_kill_handler() {
        debug!("Found kill handler callback to invoke in Task {:?}", task::get_my_current_task());
        if let Some(curr) = task::get_my_current_task() {
            kh_func(&curr, cause);
        }
    } else {
        debug!("No kill handler callback in Task {:?}", task::get_my_current_task());
    }

    // Invoke the proper signal handler registered for this task, if one exists.
    if let Some(signal) = exception_to_signal(exception_number) {
        if let Some(handler) = signal_handler::take_signal_handler(signal) {
            warn!("Invoking signal handler for {:?}", signal);
            let signal_context = SignalContext {
                instruction_pointer: VirtualAddress::new_canonical(stack_frame.instruction_pointer.as_u64() as usize),
                stack_pointer: VirtualAddress::new_canonical(stack_frame.stack_pointer.as_u64() as usize),
                signal,
                error_code,
            };
            if handler(&signal_context).is_ok() {
                warn!("Signal handler for {:?} returned Ok. Returning from exception handler is disabled and untested.", signal);
                // TODO: test and enable this return;
            }
        }
    }

    // Unwind the current task that failed due to the given exception.
    // This doesn't always work perfectly, so it's disabled by default for now.
    #[cfg(unwind_exceptions)] {
        // skip 2 frames: `start_unwinding` and `kill_and_halt`
        match unwind::start_unwinding(cause, 2) {
            Ok(_) => {
                emergency_println!("BUG: when handling exception {}, start_unwinding() returned an Ok() value, \
                    which is unexpected because it means no unwinding actually occurred. Task: {:?}.", 
                    exception_number,
                    task::get_my_current_task()
                );
            }
            Err(e) => {
                emergency_println!("Task {:?} was unable to start unwinding procedure after exception {}, error: {}.",
                    task::get_my_current_task(), exception_number, e
                );
            }
        }
    }
    #[cfg(not(unwind_exceptions))] {
        let res = task::with_current_task(|t| {
            use task_struct::{RunState, ExitValue};
            t.runstate.store(RunState::Exited(ExitValue::Killed(cause)));
            task::scheduler::remove_task(t);
            let kill_result: Result<(), ()> = Ok(());
            match kill_result {
                Ok(()) => { emergency_println!("Task {:?} killed itself successfully", t); }
                Err(e) => { emergency_println!("Task {:?} was unable to kill itself. Error: {:?}", t, e); }
            }
            kill_result
        });
        if res.is_err() {
            emergency_println!("BUG: kill_and_halt(): Couldn't get current task in order to kill it.");
        }
    }

    // If we failed to handle the exception and unwind the task, there's not really much we can do about it,
    // other than just let the thread spin endlessly (which doesn't hurt correctness but is inefficient). 
    // But in general, this task should have already been marked as killed and thus no longer schedulable,
    // so it should not reach this point. 
    // Only exceptions during the early OS initialization process will get here, meaning that the OS will basically stop.
    loop { core::hint::spin_loop() }
}


/// Checks whether the given `vaddr` falls within a stack guard page, indicating stack overflow. 
fn is_stack_overflow(vaddr: VirtualAddress) -> bool {
    let page = Page::containing_address(vaddr);
    task::with_current_task(|t|
        t.kstack.guard_page().contains(&page)
    ).unwrap_or(false)
}

/// Converts the given `exception_number` into a [`Signal`] category, if relevant.
fn exception_to_signal(exception_number: u8) -> Option<Signal> {
    match exception_number {
        0x00 | 0x04 | 0x10 | 0x13         => Some(Signal::ArithmeticError),
        0x05 | 0x0E | 0x0C                => Some(Signal::InvalidAddress),
        0x06 | 0x07 | 0x08 | 0x0A | 0x0D  => Some(Signal::IllegalInstruction),
        0x0B | 0x11                       => Some(Signal::BusError),
        _                                 => None,
    }
}


/// exception 0x00
extern "x86-interrupt" fn divide_error_handler(stack_frame: InterruptStackFrame) {
    emergency_println!("\nEXCEPTION: DIVIDE ERROR\n{:#X?}\n", stack_frame);
    kill_and_halt(0x0, &stack_frame, None, true)
}

/// exception 0x01
extern "x86-interrupt" fn debug_handler(stack_frame: InterruptStackFrame) {
    // Ne jamais formater stack_frame ici — peut être corrompu si DR fire hors contexte.
    // Juste logger l'adresse RIP brute, sans accéder aux autres champs.
    let rip = stack_frame.instruction_pointer.as_u64();
    emergency_print("EXCEPTION: DEBUG at RIP=0x");
    // print hex sans formatter
    let mut buf = [0u8; 16];
    let mut n = rip;
    let mut i = 16usize;
    loop {
        i -= 1;
        buf[i] = b"0123456789ABCDEF"[(n & 0xF) as usize];
        n >>= 4;
        if n == 0 || i == 0 { break; }
    }
    emergency_print(core::str::from_utf8(&buf[i..]).unwrap_or("?"));
    emergency_print("\n");
    // Ne pas appeler kill_and_halt — debug exception est non-fatale.
    // Vider les DR pour éviter des tirs parasites.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!(
            "xor rax, rax",
            "mov dr7, rax",  // désarme tous les breakpoints hardware
            out("rax") _,
        );
    }
}

/// Exception 0x02 is a Non-Maskable Interrupt (NMI).
///
/// Theseus uses this for TLB Shootdown IPIs and sampling interrupts.
///
/// # Important Note
/// Acquiring ANY locks in this function, even irq-safe ones, could cause a deadlock
/// because this interrupt takes priority over everything else and can interrupt
/// another regular interrupt. 
/// This includes printing to the log (e.g., `debug!()`) or the screen.
extern "x86-interrupt" fn nmi_handler(stack_frame: InterruptStackFrame) {
    // trace!("nmi_handler (CPU {})", cpu::current_cpu());
    let mut expected_nmi = false;

    if tlb_shootdown::handle_tlb_shootdown_ipi() {
        return;
    }

    // Performance monitoring hardware uses NMIs to trigger a sampling interrupt.
    match pmu_x86::handle_sample(&stack_frame) {
        // A PMU sample did occur and was properly handled, so this NMI was expected. 
        Ok(true) => expected_nmi = true,
        // No PMU sample occurred, so this NMI was unexpected.
        Ok(false) => { }
        // A PMU sample did occur but wasn't properly handled, so this NMI was expected. 
        Err(_e) => {
            emergency_println!("nmi_handler: pmu_x86 failed to record sample: {:?}", _e);
            expected_nmi = true;
        }
    }

    if expected_nmi {
        return;
    }

    emergency_println!("\nEXCEPTION: NON-MASKABLE INTERRUPT at {:#X}\n{:#X?}\n",
        stack_frame.instruction_pointer,
        stack_frame,
    );

    log_exception(0x2, stack_frame.instruction_pointer.as_u64() as usize, None, None);
    kill_and_halt(0x2, &stack_frame, None, true)
}


/// exception 0x03
extern "x86-interrupt" fn breakpoint_handler(stack_frame: InterruptStackFrame) {
    emergency_println!("\nEXCEPTION: BREAKPOINT\n{:#X?}", stack_frame);
    // don't halt here, this isn't a fatal/permanent failure, just a brief pause.
}

/// exception 0x04
extern "x86-interrupt" fn overflow_handler(stack_frame: InterruptStackFrame) {
    emergency_println!("\nEXCEPTION: OVERFLOW\n{:#X?}", stack_frame);
    kill_and_halt(0x4, &stack_frame, None, true)
}

// exception 0x05
extern "x86-interrupt" fn bound_range_exceeded_handler(stack_frame: InterruptStackFrame) {
    emergency_println!("\nEXCEPTION: BOUND RANGE EXCEEDED\n{:#X?}", stack_frame);
    kill_and_halt(0x5, &stack_frame, None, true)
}

/// exception 0x06
extern "x86-interrupt" fn invalid_opcode_handler(stack_frame: InterruptStackFrame) {
    emergency_println!("\nEXCEPTION: INVALID OPCODE\n{:#X?}", stack_frame);
    kill_and_halt(0x6, &stack_frame, None, true)
}

/// exception 0x07
///
/// For more information about "spurious interrupts", 
/// see [here](http://wiki.osdev.org/I_Cant_Get_Interrupts_Working#I_keep_getting_an_IRQ7_for_no_apparent_reason).
extern "x86-interrupt" fn device_not_available_handler(stack_frame: InterruptStackFrame) {
    emergency_println!("\nEXCEPTION: DEVICE NOT AVAILABLE\n{:#X?}", stack_frame);
    kill_and_halt(0x7, &stack_frame, None, true)
}

/// exception 0x08
extern "x86-interrupt" fn double_fault_handler(stack_frame: InterruptStackFrame, error_code: u64) -> ! {
    let accessed_vaddr = Cr2::read_raw();
    emergency_println!("\nEXCEPTION: DOUBLE FAULT\n{:#X?}\nTried to access {:#X}
        Note: double faults in Theseus are typically caused by stack overflow, is the stack large enough?",
        stack_frame, accessed_vaddr,
    );
    if is_stack_overflow(VirtualAddress::new_canonical(accessed_vaddr as usize)) {
        emergency_println!("--> This double fault was definitely caused by stack overflow, tried to access {:#X}.\n", accessed_vaddr);
    }
    
    kill_and_halt(0x8, &stack_frame, Some(error_code.into()), false);
    loop { core::hint::spin_loop() }
}

/// exception 0x0A
extern "x86-interrupt" fn invalid_tss_handler(stack_frame: InterruptStackFrame, error_code: u64) {
    emergency_println!("\nEXCEPTION: INVALID TSS\n{:#X?}\nError code: {:#b}", stack_frame, error_code);
    kill_and_halt(0xA, &stack_frame, Some(error_code.into()), true)
}

/// exception 0x0B
extern "x86-interrupt" fn segment_not_present_handler(stack_frame: InterruptStackFrame, error_code: u64) {
    emergency_println!("\nEXCEPTION: SEGMENT NOT PRESENT\n{:#X?}\nError code: {:#b}", stack_frame, error_code);
    kill_and_halt(0xB, &stack_frame, Some(error_code.into()), true)
}

/// exception 0x0C
extern "x86-interrupt" fn stack_segment_fault_handler(stack_frame: InterruptStackFrame, error_code: u64) {
    emergency_println!("\nEXCEPTION: STACK SEGMENT FAULT\n{:#X?}\nError code: {:#b}", stack_frame, error_code);
    kill_and_halt(0xC, &stack_frame, Some(error_code.into()), true)
}

/// exception 0x0D
extern "x86-interrupt" fn general_protection_fault_handler(stack_frame: InterruptStackFrame, error_code: u64) {
    error!("GPF at RIP={:#x} err_code={:#x}", stack_frame.instruction_pointer.as_u64(), error_code);
    emergency_println!("\nEXCEPTION: GENERAL PROTECTION FAULT\n{:#X?}\nError code: {:#b}", stack_frame, error_code);
    kill_and_halt(0xD, &stack_frame, Some(error_code.into()), true)
}

/// exception 0x0E
extern "x86-interrupt" fn page_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    let accessed_vaddr = Cr2::read_raw() as usize;

    error!("PAGE FAULT: RIP={:#x} accessing={:#x} error={:?}",
        stack_frame.instruction_pointer.as_u64(), accessed_vaddr, error_code);
    error!("PAGE FAULT at RIP={:#x} accessing {:#x} err={:?}",
        stack_frame.instruction_pointer.as_u64(), accessed_vaddr, error_code);

    let is_valid_user_addr = accessed_vaddr >= 0x1000
        && accessed_vaddr < 0x0000_8000_0000_0000;

    if is_valid_user_addr && !error_code.contains(PageFaultErrorCode::PROTECTION_VIOLATION) {
        let vaddr_aligned = accessed_vaddr & !(memory_swap::PAGE_SIZE - 1);
        let vaddr_obj = VirtualAddress::new_canonical(vaddr_aligned);

        if let Some(pte_raw) = memory::paging::get_pte_raw(vaddr_obj) {
            if memory_swap::is_swap_pte(pte_raw) {
                match memory_swap::swap_in(pte_raw) {
                    Ok(new_paddr) => {
                        let new_pte = (new_paddr.value() as u64) | 0b111;
                        unsafe {
                            if memory::paging::set_pte_raw(vaddr_obj, new_pte) {
                                memory_x86_64::tlb_flush_virt_addr(vaddr_obj);
                                return;
                            }
                        }
                        warn!("[memory_swap] set_pte_raw échoué pour {:#x}", vaddr_aligned);
                    }
                    Err(e) => {
                        warn!("[memory_swap] swap_in échoué pour {:#x}: {}", vaddr_aligned, e);
                    }
                }
            }
        }
    }

    emergency_println!(
        "\nEXCEPTION: PAGE FAULT while accessing {:#x}\n\
        error code: {:?}\n{:#X?}",
        accessed_vaddr, error_code, stack_frame
    );
    if is_stack_overflow(VirtualAddress::new_canonical(accessed_vaddr)) {
        emergency_println!(
            "--> Page fault was caused by stack overflow, tried to access {:#X}\n.",
            accessed_vaddr
        );
    }
    kill_and_halt(
        0xE,
        &stack_frame,
        Some(ErrorCode::PageFaultError {
            accessed_address: accessed_vaddr,
            pf_error: error_code,
        }),
        true,
    )
}


/// exception 0x10
extern "x86-interrupt" fn x87_floating_point_handler(stack_frame: InterruptStackFrame) {
    emergency_println!("\nEXCEPTION: x87 FLOATING POINT\n{:#X?}", stack_frame);
    kill_and_halt(0x10, &stack_frame, None, true)
}

/// exception 0x11
extern "x86-interrupt" fn alignment_check_handler(stack_frame: InterruptStackFrame, error_code: u64) {
    emergency_println!("\nEXCEPTION: ALIGNMENT CHECK\n{:#X?}\nError code: {:#b}", stack_frame, error_code);
    kill_and_halt(0x11, &stack_frame, Some(error_code.into()), true)
}

/// exception 0x12
extern "x86-interrupt" fn machine_check_handler(stack_frame: InterruptStackFrame) -> ! {
    emergency_println!("\nEXCEPTION: MACHINE CHECK\n{:#X?}", stack_frame);
    kill_and_halt(0x12, &stack_frame, None, true);
    loop { core::hint::spin_loop() }
}

/// exception 0x13
extern "x86-interrupt" fn simd_floating_point_handler(stack_frame: InterruptStackFrame) {
    emergency_println!("\nEXCEPTION: SIMD FLOATING POINT\n{:#X?}", stack_frame);
    kill_and_halt(0x13, &stack_frame, None, true)
}

/// exception 0x14
extern "x86-interrupt" fn virtualization_handler(stack_frame: InterruptStackFrame) {
    emergency_println!("\nEXCEPTION: VIRTUALIZATION\n{:#X?}", stack_frame);
    kill_and_halt(0x14, &stack_frame, None, true)
}

/// exception 0x1D
extern "x86-interrupt" fn vmm_communication_exception_handler(stack_frame: InterruptStackFrame, error_code: u64) {
    emergency_println!("\nEXCEPTION: VMM COMMUNICATION EXCEPTION\n{:#X?}\nError code: {:#b}", stack_frame, error_code);
    kill_and_halt(0x1D, &stack_frame, Some(error_code.into()),true)
}

/// exception 0x1E
extern "x86-interrupt" fn security_exception_handler(stack_frame: InterruptStackFrame, error_code: u64) {
    emergency_println!("\nEXCEPTION: SECURITY EXCEPTION\n{:#X?}\nError code: {:#b}", stack_frame, error_code);
    kill_and_halt(0x1E, &stack_frame, Some(error_code.into()), true)
}
