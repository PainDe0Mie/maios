//! MKS Statistics — per-CPU and global scheduler metrics.
//!
//! Exposed via /proc/mks/stats (MaiOS procfs) for observability.
//! These metrics directly correspond to Linux's /proc/schedstat and
//! perf sched measurements.

use task_struct::Task;

/// Per-CPU scheduler statistics.
pub struct CpuStats {
    pub cpu_id: usize,

    /// Total context switches on this CPU.
    pub context_switches: u64,
    /// Total nanoseconds spent running tasks (not idle).
    pub run_time_ns: u64,
    /// Total nanoseconds spent idle.
    pub idle_time_ns: u64,
    /// Total work-stealing events (tasks stolen from this CPU by others).
    pub tasks_stolen_out: u64,
    /// Total work-stealing events (tasks this CPU stole from others).
    pub tasks_stolen_in: u64,
    /// Running average of scheduling latency (wakeup → run) in ns.
    pub avg_wakeup_latency_ns: u64,
    /// Maximum scheduling latency observed.
    pub max_wakeup_latency_ns: u64,
    /// Number of deadline misses (task ran after its abs_deadline).
    pub deadline_misses: u64,
}

impl CpuStats {
    pub fn new(cpu_id: usize) -> Self {
        CpuStats {
            cpu_id,
            context_switches: 0,
            run_time_ns: 0,
            idle_time_ns: 0,
            tasks_stolen_out: 0,
            tasks_stolen_in: 0,
            avg_wakeup_latency_ns: 0,
            max_wakeup_latency_ns: 0,
            deadline_misses: 0,
        }
    }

    /// Record a context switch from `from` to `to`.
    pub fn record_switch(&mut self, from: &Task, to: &Task) {
        self.context_switches += 1;

        // Measure wakeup latency: time from when `to` became runnable to now.
        let now = monotonic_ns();
        let wakeup_ns = to.sched.wakeup_time_ns;
        if wakeup_ns > 0 && now >= wakeup_ns {
            let latency = now - wakeup_ns;
            // Exponential moving average (alpha = 1/8).
            self.avg_wakeup_latency_ns =
                (self.avg_wakeup_latency_ns * 7 + latency) / 8;
            if latency > self.max_wakeup_latency_ns {
                self.max_wakeup_latency_ns = latency;
            }
        }
    }
}

/// Read the monotonic clock in nanoseconds.
/// Uses TSC on x86_64, generic_timer on AArch64.
#[inline]
pub fn monotonic_ns() -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        // Use TSC calibrated at boot. For now approximate with rdtsc / 3GHz.
        // In production, multiply by the TSC→ns ratio calibrated against HPET.
        unsafe {
            let lo: u32;
            let hi: u32;
            core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi);
            let tsc = ((hi as u64) << 32) | (lo as u64);
            tsc / 3 // ~3 GHz → ns approximation
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        // Use CNTVCT_EL0 (virtual counter).
        let cnt: u64;
        unsafe { core::arch::asm!("mrs {}, cntvct_el0", out(reg) cnt) };
        // Divide by frequency (usually 24 MHz or 100 MHz) and convert to ns.
        // TODO: read CNTFRQ_EL0 and calibrate.
        cnt * 10 // placeholder: 100 MHz → 10ns per tick
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    { 0 }
}