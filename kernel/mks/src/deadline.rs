//! SCHED_DEADLINE helpers and admission control.
//!
//! The EDF queue itself is in `realtime.rs` (DeadlineRunQueue).
//! This module provides:
//!   - System-wide bandwidth accounting (sum of C_i/P_i across all CPUs).
//!   - Helper to validate deadline parameters before admission.
//!   - `DlOverrunAction`: what to do when a task exceeds its runtime budget.

use core::sync::atomic::{AtomicU64, Ordering};
use crate::{SCHED_DL_BANDWIDTH_NUM, SCHED_DL_BANDWIDTH_DEN};

// ---------------------------------------------------------------------------
// System-wide bandwidth tracker
// ---------------------------------------------------------------------------

/// System-wide admitted bandwidth, in units of runtime/period * 10^6.
/// Protected by a single atomic (approximate — exact admission per CPU is in
/// DeadlineRunQueue).
static GLOBAL_DL_BANDWIDTH: AtomicU64 = AtomicU64::new(0);

/// Maximum bandwidth in the same units (SCHED_DL_BANDWIDTH_NUM / DEN * 10^6).
const MAX_BW: u64 = SCHED_DL_BANDWIDTH_NUM * 1_000_000 / SCHED_DL_BANDWIDTH_DEN;

/// Validate deadline task parameters.
///
/// Returns `Ok(())` if the parameters are admissible, `Err` with a reason otherwise.
pub fn validate_dl_params(runtime: u64, deadline: u64, period: u64) -> Result<(), &'static str> {
    if period == 0 {
        return Err("SCHED_DEADLINE: period must be > 0");
    }
    if runtime == 0 {
        return Err("SCHED_DEADLINE: runtime must be > 0");
    }
    if deadline > period {
        return Err("SCHED_DEADLINE: deadline must be <= period");
    }
    if runtime > deadline {
        return Err("SCHED_DEADLINE: runtime must be <= deadline");
    }
    // Minimum period: 1ms (avoid spinning tasks).
    if period < 1_000_000 {
        return Err("SCHED_DEADLINE: period must be >= 1ms (1_000_000 ns)");
    }
    Ok(())
}

/// Try to admit a new deadline task globally.
/// Returns `Ok(contribution)` on success; the caller must call `release_bandwidth`
/// with the same value when the task exits.
pub fn admit_bandwidth(runtime: u64, period: u64) -> Result<u64, &'static str> {
    let contribution = runtime * 1_000_000 / period;
    loop {
        let current = GLOBAL_DL_BANDWIDTH.load(Ordering::Relaxed);
        if current + contribution > MAX_BW {
            return Err("SCHED_DEADLINE: global bandwidth limit exceeded");
        }
        match GLOBAL_DL_BANDWIDTH.compare_exchange_weak(
            current, current + contribution,
            Ordering::AcqRel, Ordering::Relaxed
        ) {
            Ok(_) => return Ok(contribution),
            Err(_) => continue, // retry
        }
    }
}

/// Release bandwidth when a deadline task exits.
pub fn release_bandwidth(contribution: u64) {
    GLOBAL_DL_BANDWIDTH.fetch_sub(contribution, Ordering::Relaxed);
}

/// Query the current global bandwidth utilization [0..1000] (‰).
pub fn bandwidth_utilization_permille() -> u64 {
    let bw = GLOBAL_DL_BANDWIDTH.load(Ordering::Relaxed);
    bw * 1000 / MAX_BW.max(1)
}

// ---------------------------------------------------------------------------
// Overrun action
// ---------------------------------------------------------------------------

/// What happens when a DEADLINE task exhausts its runtime budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DlOverrunAction {
    /// Throttle: the task cannot run until the next period starts.
    /// This is the CBS (Constant Bandwidth Server) policy.
    Throttle,
    /// Kill the task with SIGXCPU (Linux-compatible behavior for strict tasks).
    Kill,
    /// Warn and continue (useful during development).
    WarnAndContinue,
}

impl Default for DlOverrunAction {
    fn default() -> Self { DlOverrunAction::Throttle }
}