//! Syscalls liés au temps pour MaiOS.

use log::debug;
use crate::error::{SyscallResult, SyscallError};

/// sys_clock_gettime — obtenir l'heure courante (stub).
pub fn sys_clock_gettime(clock_id: u64, tp: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    debug!("sys_clock_gettime(clock_id={}, tp={:#x})", clock_id, tp);
    // TODO: Implémenter avec le sous-système timer MaiOS (TSC, PIT, ou HPET)
    Err(SyscallError::NotImplemented)
}

/// sys_perf_counter — compteur de performance haute résolution.
///
/// Utilise RDTSC comme source. Fréquence nominale reportée à 1 GHz.
/// Compatible avec NtQueryPerformanceCounter (sortie via pointeurs).
///
/// Arguments : counter_ptr, frequency_ptr
pub fn sys_perf_counter(counter_ptr: u64, freq_ptr: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    debug!("sys_perf_counter(counter={:#x}, freq={:#x})", counter_ptr, freq_ptr);

    if counter_ptr == 0 {
        return Err(SyscallError::InvalidArgument);
    }

    let tsc: u64 = unsafe {
        let lo: u32;
        let hi: u32;
        core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi);
        ((hi as u64) << 32) | (lo as u64)
    };

    unsafe {
        *(counter_ptr as *mut u64) = tsc;
        if freq_ptr != 0 {
            *(freq_ptr as *mut u64) = 1_000_000_000; // 1 GHz nominal
        }
    }

    Ok(0)
}
