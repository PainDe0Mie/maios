//! CPU C-state (idle power state) management.
//!
//! C-states reduce power consumption when a CPU core is idle by
//! progressively disabling clocks, caches, and voltage regulators.
//!
//! Based on: ACPI Specification 6.5 Chapter 8, Linux cpuidle subsystem,
//! "The cpuidle subsystem" (Wysocki, LWN 2013).
//!
//! ## C-state hierarchy
//!
//! | State | Name           | Latency   | Power   | Description                    |
//! |-------|----------------|-----------|---------|--------------------------------|
//! | C0    | Active         | 0         | 100%    | CPU executing instructions     |
//! | C1    | Halt           | ~1 µs     | ~70%    | Clock gated, instant wakeup    |
//! | C1E   | Enhanced Halt  | ~10 µs    | ~50%    | + voltage reduction            |
//! | C3    | Sleep          | ~100 µs   | ~20%    | L1/L2 cache flushed            |
//! | C6    | Deep Sleep     | ~200 µs   | ~5%     | Core voltage off, state saved  |
//! | C7    | Package Sleep  | ~300 µs   | ~2%     | L3 flushed, near power-off     |
//!
//! ## Governor: menu algorithm
//!
//! The menu governor (Linux default) predicts idle duration and selects
//! the deepest C-state whose exit latency is acceptable:
//! 1. Estimate expected idle time from scheduler + timer data.
//! 2. Apply a correction factor based on recent prediction accuracy.
//! 3. Select the deepest state where `exit_latency < predicted_idle / 2`.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// A C-state definition.
#[derive(Debug, Clone, Copy)]
pub struct CStateInfo {
    /// C-state index (0 = C0, 1 = C1, etc.).
    pub index: u8,
    /// Human-readable name.
    pub name: &'static str,
    /// Exit latency in microseconds (time to return to C0).
    pub exit_latency_us: u32,
    /// Target residency in microseconds (minimum time to stay for efficiency).
    pub target_residency_us: u32,
    /// Relative power consumption (C0 = 1000).
    pub power_usage: u32,
}

/// Table of supported C-states (x86_64 typical values).
pub const CSTATES: &[CStateInfo] = &[
    CStateInfo { index: 0, name: "C0-Active",     exit_latency_us: 0,   target_residency_us: 0,    power_usage: 1000 },
    CStateInfo { index: 1, name: "C1-Halt",        exit_latency_us: 1,   target_residency_us: 2,    power_usage: 700 },
    CStateInfo { index: 2, name: "C1E-EnhHalt",    exit_latency_us: 10,  target_residency_us: 20,   power_usage: 500 },
    CStateInfo { index: 3, name: "C3-Sleep",        exit_latency_us: 100, target_residency_us: 300,  power_usage: 200 },
    CStateInfo { index: 4, name: "C6-DeepSleep",    exit_latency_us: 200, target_residency_us: 800,  power_usage: 50 },
    CStateInfo { index: 5, name: "C7-PackageSleep", exit_latency_us: 300, target_residency_us: 1500, power_usage: 20 },
];

/// Per-CPU C-state governor state (menu governor).
pub struct CStateGovernor {
    /// Last predicted idle duration in microseconds.
    pub last_predicted_us: AtomicU64,
    /// Last actual idle duration in microseconds.
    pub last_actual_us: AtomicU64,
    /// Correction factor (fixed-point, 1024 = 1.0).
    /// Adjusted based on prediction accuracy.
    pub correction_factor: AtomicU32,
    /// Total time spent in each C-state (microseconds).
    pub time_in_state: [AtomicU64; 6],
    /// Number of entries into each C-state.
    pub entries: [AtomicU64; 6],
}

impl CStateGovernor {
    /// Create a new governor with default state.
    pub const fn new() -> Self {
        CStateGovernor {
            last_predicted_us: AtomicU64::new(0),
            last_actual_us: AtomicU64::new(0),
            correction_factor: AtomicU32::new(1024), // 1.0
            time_in_state: [
                AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
                AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
            ],
            entries: [
                AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
                AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
            ],
        }
    }

    /// Select the optimal C-state for the predicted idle duration.
    ///
    /// Menu governor algorithm:
    /// 1. Apply correction factor to predicted duration.
    /// 2. Select the deepest state where exit_latency < corrected_prediction / 2.
    /// 3. Ensure target_residency < corrected_prediction.
    pub fn select(&self, predicted_idle_us: u64, max_cstate: u32) -> u8 {
        self.last_predicted_us.store(predicted_idle_us, Ordering::Relaxed);

        let correction = self.correction_factor.load(Ordering::Relaxed) as u64;
        let corrected = predicted_idle_us * correction / 1024;

        let mut best_state = 0u8;

        for cstate in CSTATES.iter() {
            if cstate.index as u32 > max_cstate {
                break;
            }
            if cstate.exit_latency_us == 0 {
                continue; // Skip C0 (active state).
            }

            let latency = cstate.exit_latency_us as u64;
            let residency = cstate.target_residency_us as u64;

            // Only enter this state if:
            // 1. We'll be idle long enough for the exit latency to be worthwhile.
            // 2. We'll meet the minimum residency requirement.
            if latency * 2 <= corrected && residency <= corrected {
                best_state = cstate.index;
            }
        }

        best_state
    }

    /// Update the correction factor based on actual vs. predicted idle time.
    ///
    /// If we consistently over-predict, reduce the factor (enter shallower states).
    /// If we consistently under-predict, increase the factor (allow deeper states).
    pub fn update(&self, actual_idle_us: u64) {
        self.last_actual_us.store(actual_idle_us, Ordering::Relaxed);

        let predicted = self.last_predicted_us.load(Ordering::Relaxed);
        if predicted == 0 {
            return;
        }

        let old_factor = self.correction_factor.load(Ordering::Relaxed);

        // Compute new factor: actual / predicted, clamped to [256, 4096] (0.25x to 4.0x).
        let new_factor = if predicted > 0 {
            ((actual_idle_us * 1024) / predicted)
                .max(256)
                .min(4096) as u32
        } else {
            1024
        };

        // Exponential moving average: new = 7/8 * old + 1/8 * measured.
        let blended = (old_factor * 7 + new_factor) / 8;
        self.correction_factor.store(blended, Ordering::Relaxed);
    }

    /// Record entering a C-state.
    pub fn record_entry(&self, cstate_index: u8) {
        if (cstate_index as usize) < self.entries.len() {
            self.entries[cstate_index as usize].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Record time spent in a C-state.
    pub fn record_residency(&self, cstate_index: u8, duration_us: u64) {
        if (cstate_index as usize) < self.time_in_state.len() {
            self.time_in_state[cstate_index as usize].fetch_add(duration_us, Ordering::Relaxed);
        }
    }
}
