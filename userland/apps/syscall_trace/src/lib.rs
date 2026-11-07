//! Toggle syscall tracing on/off at runtime.
//!
//! Usage:
//!   syscall_trace on    — enable tracing (output to COM1 serial)
//!   syscall_trace off   — disable tracing
//!   syscall_trace       — show current status

#![no_std]

extern crate alloc;

use alloc::{string::String, vec::Vec};

pub fn main(args: Vec<String>) -> isize {
    let cmd = args.get(0).map(|s| s.as_str()).unwrap_or("");

    match cmd {
        "on" | "enable" | "1" => {
            maios_syscall::enable_trace();
            log::info!("Syscall tracing ENABLED — output on COM1 serial");
        }
        "off" | "disable" | "0" => {
            maios_syscall::disable_trace();
            log::info!("Syscall tracing DISABLED");
        }
        _ => {
            let status = if maios_syscall::is_trace_enabled() { "ON" } else { "OFF" };
            log::info!("Syscall tracing is currently: {}", status);
            log::info!("Usage: syscall_trace [on|off]");
        }
    }
    0
}
