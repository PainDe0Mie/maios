//! Theseus-specific implementation for iterating over a stack trace.

use core::ffi::c_void;
use alloc::sync::Arc;
use theseus_stack_trace::{CrateNamespace, stack_trace};

#[inline(always)]
pub fn trace(callback: &mut dyn FnMut(&super::Frame) -> bool) {
    let _ignored_result = stack_trace(
        &mut |stack_frame, stack_frame_iter| {
            // Construct a `Frame` that we can pass to the `callback` function.
            // See the doc comments for the `Frame` struct below for more info.
            let regs = stack_frame_iter.registers();
            let frame = Frame {
                ip: regs.return_address().expect("Theseus IP register was None") as usize,
                sp: regs.stack_pointer().expect("Theseus SP register was None") as usize,
                symbol_address: stack_frame.initial_address() as usize,
                namespace: stack_frame_iter.namespace().clone(),
            };

            let keep_going = callback(&super::Frame { inner: frame });
            keep_going
        },
        None, // no recursion max
    );
}

#[derive(Clone)]
pub struct Frame {
    /// According to the module-level docs for [super::Frame::ip()], the IP (instruction pointer)
    /// should be the "next instruction to execute in the frame",
    /// which is what Theseus calls the "return_address" for a given frame.
    /// The "call_site_address" is the instruction *right before* the return address.
    ip: usize,
    sp: usize,
    symbol_address: usize,
    // Theseus doesn't really have a module base address, though we could provide one
    // by resolving the symbol aggressively, which is expensive and unnecessary.
    // We could then provide the base address of the .text section for the crate 
    // containing this Frame's symbol_address.
    //
    // Using `None` for this is okay because the unix implementation for this crate does the same.
    //
    // base_address: usize,
    pub(crate) namespace: Arc<CrateNamespace>,
}

impl Frame {
    pub fn ip(&self) -> *mut c_void {
        self.ip as *mut _
    }

    pub fn sp(&self) -> *mut c_void {
        self.sp as *mut _
    }

    pub fn symbol_address(&self) -> *mut c_void {
        self.symbol_address as *mut _
    }

    pub fn module_base_address(&self) -> Option<*mut c_void> {
        None
    }
}
