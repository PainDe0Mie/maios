//! Power governor — orchestrates P-state and C-state decisions.
//!
//! The governor runs on every timer tick and:
//! 1. Reads current CPU utilisation from MKS.
//! 2. Selects the optimal P-state via the schedutil algorithm.
//! 3. Applies thermal caps if the CPU is throttled.
//! 4. Updates the hardware (writes to P-state MSR / CPPC register).
//!
//! Based on: Linux `schedutil` cpufreq governor (Rafael Wysocki, 2016).
//! The key insight is that the scheduler already knows the utilisation,
//! so the governor can react to load changes within a single tick
//! (~10ms) instead of sampling-based approaches (~100ms).

use core::sync::atomic::Ordering;

/// Governor policy: controls the balance between performance and efficiency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum GovernorPolicy {
    /// Maximum performance: always run at highest P-state.
    Performance = 0,
    /// Balanced: schedutil algorithm with default headroom.
    Balanced = 1,
    /// Power save: schedutil with reduced headroom (favour lower freq).
    PowerSave = 2,
    /// Minimal: always run at lowest P-state (for battery critical mode).
    Minimal = 3,
}

/// Headroom factors for each policy (fixed-point, 1024 = 1.0).
const POLICY_HEADROOM: [u32; 4] = [
    1024, // Performance: 1.0x (exact frequency match)
    1280, // Balanced: 1.25x (default schedutil)
    896,  // PowerSave: 0.875x (slightly under target)
    512,  // Minimal: 0.5x (half the target frequency)
];

/// Global governor policy.
static POLICY: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(
    GovernorPolicy::Balanced as u8
);

/// Set the global governor policy.
pub fn set_policy(policy: GovernorPolicy) {
    POLICY.store(policy as u8, core::sync::atomic::Ordering::Release);
}

/// Get the current governor policy.
pub fn policy() -> GovernorPolicy {
    match POLICY.load(core::sync::atomic::Ordering::Acquire) {
        0 => GovernorPolicy::Performance,
        1 => GovernorPolicy::Balanced,
        2 => GovernorPolicy::PowerSave,
        3 => GovernorPolicy::Minimal,
        _ => GovernorPolicy::Balanced,
    }
}

/// Per-CPU governor tick — main entry point.
///
/// Called from `MaiPower::tick()` every GOVERNOR_TICK_MS milliseconds.
pub fn tick(mps: &super::MaiPower, cpu_id: usize) {
    let cpu = match mps.cpus.get(cpu_id) {
        Some(c) => c,
        None => return,
    };

    let current_policy = policy();

    // Performance mode: always max frequency.
    if current_policy == GovernorPolicy::Performance {
        cpu.current_pstate.store(0, Ordering::Release);
        cpu.current_freq_mhz.store(cpu.max_freq_mhz, Ordering::Release);
        return;
    }

    // Minimal mode: always min frequency.
    if current_policy == GovernorPolicy::Minimal {
        let min_pstate = 7; // Lowest P-state index.
        cpu.current_pstate.store(min_pstate, Ordering::Release);
        cpu.current_freq_mhz.store(cpu.min_freq_mhz, Ordering::Release);
        return;
    }

    // schedutil-based selection.
    let util = cpu.utilization.load(Ordering::Relaxed);
    let headroom = POLICY_HEADROOM[current_policy as usize] as u64;

    let max_freq = cpu.max_freq_mhz as u64;
    if max_freq == 0 {
        return;
    }

    // target_freq = headroom * max_freq * (util / 1000) / 1024
    let target_freq = headroom * max_freq * (util.min(1000) as u64) / (1024 * 1000);

    // Map to P-state index (simple linear mapping).
    // P0 = max_freq, P7 = min_freq.
    let min_freq = cpu.min_freq_mhz as u64;
    let freq_range = max_freq.saturating_sub(min_freq);

    let pstate = if freq_range == 0 || target_freq >= max_freq {
        0
    } else if target_freq <= min_freq {
        7
    } else {
        // Linear interpolation: (max_freq - target) / (max_freq - min_freq) * 7
        let offset = max_freq.saturating_sub(target_freq);
        ((offset * 7) / freq_range).min(7) as u32
    };

    // Apply thermal cap.
    let capped_pstate = if cpu.throttled.load(Ordering::Relaxed) {
        // When throttled, don't go below P4 (50% freq).
        pstate.max(4)
    } else {
        pstate
    };

    // Apply power budget cap from package.
    let budget_pstate = if let Some(pkg) = mps.packages.get(cpu.package_id as usize) {
        if pkg.is_over_budget() {
            // Over budget: cap to at least P3.
            capped_pstate.max(3)
        } else {
            capped_pstate
        }
    } else {
        capped_pstate
    };

    cpu.current_pstate.store(budget_pstate, Ordering::Release);

    // Compute the actual frequency for this P-state.
    let actual_freq = if freq_range > 0 && budget_pstate <= 7 {
        max_freq - (budget_pstate as u64 * freq_range / 7)
    } else {
        max_freq
    };
    cpu.current_freq_mhz.store(actual_freq as u32, Ordering::Release);
}
