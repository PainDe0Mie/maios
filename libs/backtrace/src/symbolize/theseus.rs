//! Symbol resolution for backtraces on Theseus.

use super::{BytesOrWideString, ResolveWhat, SymbolName, adjust_ip};
use core::ffi::c_void;
use core::marker;
use alloc::sync::Arc;
use theseus_stack_trace::{CrateNamespace, StrongSectionRef};
use theseus_std::path::Path;


pub unsafe fn resolve(what: ResolveWhat<'_>, callback: &mut dyn FnMut(&super::Symbol)) {
    let (namespace, addr) = match what {
        // To resolve a random address, we can only search the current namespace.
        ResolveWhat::Address(addr) => {
            let ns = theseus_stack_trace::get_my_current_task()
                .expect("couldn't get Theseus's current task")
                .namespace
                .clone();
            (ns, addr)
        }
        // Resolving a frame is easier, as we already know the namespace.
        ResolveWhat::Frame(frame) => (frame.inner.namespace.clone(), frame.ip()),
    };

    if let Some((sec, _offset)) = namespace.get_section_containing_address(
        theseus_memory::VirtualAddress::new_canonical(adjust_ip(addr) as usize),
        false, // only search text section symbols
    ) {
        callback(&super::Symbol { inner: Symbol {
            sec,
            namespace,
            _marker: marker::PhantomData,
        }});
    }
}

pub struct Symbol<'a> {
    sec: StrongSectionRef,
    namespace: Arc<CrateNamespace>,
    _marker: marker::PhantomData<&'a ()>,
}

impl Symbol<'_> {
    pub fn name(&self) -> Option<SymbolName<'_>> {
        Some(SymbolName::new(self.sec.name.as_bytes()))
    }

    pub fn addr(&self) -> Option<*mut c_void> {
        Some(self.sec.virt_addr.value() as *mut _)
    }

    pub fn filename_raw(&self) -> Option<BytesOrWideString<'_>> {
        None // TODO: use debuginfo for this
    }

    pub fn filename(&self) -> Option<&Path> {
        None // TODO: use debuginfo for this
    }

    pub fn lineno(&self) -> Option<u32> {
        None // TODO: use debuginfo for this
    }

    pub fn colno(&self) -> Option<u32> {
        None // TODO: use debuginfo for this
    }
}

pub unsafe fn clear_symbol_cache() {
    // Theseus currently doesn't cache addr -> symbol resolution mappings
}
