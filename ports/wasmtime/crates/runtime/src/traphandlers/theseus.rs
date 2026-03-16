use crate::traphandlers::{tls, wasmtime_longjmp, Trap};
use ::alloc::boxed::Box;
use theseus_signal_handler::{register_signal_handler, SignalContext, Signal};


/// Function which may handle custom signals while processing traps.
pub type SignalHandler<'a> =
    dyn Fn(&SignalContext) -> bool + Send + Sync + 'a;

/// This isn't really unsafe in Theseus, but the rest of wasmtime expects it to be,
/// so we keep it an unsafe fn to match other platforms.
pub unsafe fn platform_init() {
    platform_init_internal();
}

fn platform_init_internal() {
    // Register the `trap_handler` function as a Theseus signal handler by default.
    register_signal_handler(Signal::InvalidAddress, Box::new(trap_handler))
        .expect("platform_init(): failed to register Signal::InvalidAddress handler");
    register_signal_handler(Signal::IllegalInstruction, Box::new(trap_handler))
        .expect("platform_init(): failed to register Signal::IllegalInstruction handler");
    register_signal_handler(Signal::BusError, Box::new(trap_handler))
        .expect("platform_init(): failed to register Signal::BusError handler");
    register_signal_handler(Signal::ArithmeticError, Box::new(trap_handler))
        .expect("platform_init(): failed to register Signal::ArithmeticError handler");
}

/// The default signal handler for wasmtime on Theseus.
/// 
/// Note: much of this was borrowed from `traphandlers/unix.rs`
fn trap_handler(signal_context: &SignalContext) -> Result<(), ()> {
    
    let handled: bool = tls::with(|info| {
        // If no wasm code is executing, we don't handle this as a wasm trap.
        let info = match info {
            Some(info) => info,
            None => return false,
        };

        // If we hit an exception while handling a previous trap, that's
        // quite bad, so bail out and let the system handle this
        // recursive segfault.
        //
        // Otherwise flag ourselves as handling a trap, do the trap
        // handling, and reset our trap handling flag. Then we figure
        // out what to do based on the result of the trap handling.
        let pc = signal_context.instruction_pointer.value() as *const u8;
        let jmp_buf = info.jmp_buf_if_trap(pc, |handler| handler(signal_context));

        // Figure out what to do based on the result of this handling of
        // the trap. Note that our sentinel value of 1 means that the
        // exception was handled by a custom exception handler, so we
        // keep executing.
        if jmp_buf.is_null() {
            return false;
        }
        if jmp_buf as usize == 1 {
            return true;
        }
        info.capture_backtrace(pc);
        
        unsafe { wasmtime_longjmp(jmp_buf) }
    }); // end of tls::with() closure

    if handled {
        // indicates we handled this trap, and the task can continue executing.
        Ok(())
    } else {
        // we didn't handle this trap, so we'll allow the Theseus system 
        // to continue on to its default action of likely killing the task.
        Err(())
    }
}

pub fn lazy_per_thread_init() -> Result<(), Box<Trap>> {
    // Unused on Theseus
    Ok(())
}
