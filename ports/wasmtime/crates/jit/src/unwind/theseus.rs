//! Module for registering unwind information with Theseus.

use anyhow::Result;
use theseus_external_unwind_info::{register_unwind_info, deregister_unwind_info};

/// Represents a registry of function unwind information for Theseus.
pub struct UnwindRegistration {
    // Needed to deregister a piece of registered unwind info (see drop handler).
    text_section_base_address: usize,
}

impl UnwindRegistration {
    /// Register a new unwind information section, e.g., `.eh_frame` contents.
    /// 
    /// This exposes a C-style API because the rest of wasmtime expects it (for other platforms).
    /// 
    /// # Arguments
    /// * `text_section_base_address`: the base address of the text section
    /// * `text_section_len`: the length in bytes of the text section
    /// * `unwind_info`: a pointer to the beginning of the unwind info section (.eh_frame)
    /// * `unwind_len`: the length in bytes of the unwind info section (.eh_frame)
    pub unsafe fn new(
        text_section_base_address: *mut u8,
        text_section_len: usize,
        unwind_info: *mut u8,
        unwind_len: usize,
    ) -> Result<UnwindRegistration> {
        log::warn!("registering new unwind info with Theseus:
            text_section_base_address: {:#X},
            text_section_len: {:#X},
            unwind_info: {:#X},
            unwind_len: {:#X}",
            text_section_base_address as usize, text_section_len, unwind_info as usize, unwind_len
        );

        register_unwind_info(
            text_section_base_address,
            text_section_len,
            unwind_info,
            unwind_len,
        )
            .map_err(|_| "failed to register external unwind info with Theseus")
            .map_err(anyhow::Error::msg)?;

        Ok(UnwindRegistration {
            text_section_base_address: text_section_base_address as usize,
        })
    }

    pub fn section_name() -> &'static str {
        // Note: this is based on `src/instantiate.rs::wasm_section_name()`
        ".eh_frame.wasm"
    }
}

impl Drop for UnwindRegistration {
    fn drop(&mut self) {
        unsafe {
            if let Err(_e) = deregister_unwind_info(self.text_section_base_address as *mut u8) {
                log::error!("Failed to deregister Theseus's unwind info for {:#X}", self.text_section_base_address);
            }
        }
    }
}
