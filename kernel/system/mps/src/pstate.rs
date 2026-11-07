//! CPU P-state (performance state) and DVFS management.
//!
//! P-states control CPU operating frequency and voltage. Lower P-states
//! reduce power consumption at the cost of reduced throughput.
//!
//! Based on:
//! - Intel Enhanced SpeedStep Technology (EIST)
//! - AMD Cool'n'Quiet / Precision Boost
//! - ACPI CPPC (Collaborative Processor Performance Control)
//! - Linux `schedutil` cpufreq governor (Rafael Wysocki, 2016)
//!
//! ## P-state selection: schedutil governor
//!
//! The schedutil governor ties frequency directly to scheduler utilisation:
//!
//! ```text
//! target_freq = C × max_freq × (util / max_util)
//! ```
//!
//! Where `C` is a headroom factor (1.25 by default) to avoid running at
//! 100% utilisation which would increase latency. This is the approach
//! used in modern Linux kernels (replacing ondemand/conservative).
//!
//! ## Frequency table
//!
//! P-states map to discrete frequency/voltage pairs. On modern Intel CPUs
//! with HWP (Hardware P-states), the OS sets a range and the hardware
//! selects the exact operating point. MPS supports both table-driven
//! (legacy) and range-driven (HWP/CPPC) modes.

use core::sync::atomic::{AtomicU32, Ordering};

/// Maximum number of discrete P-states per CPU.
pub const MAX_PSTATES: usize = 32;

/// Headroom factor for schedutil-style governor (fixed-point, 1024 = 1.0).
/// 1.25 = 1280/1024: target 80% utilisation at max frequency.
pub const SCHEDUTIL_HEADROOM: u32 = 1280;

/// A discrete P-state entry.
#[derive(Debug, Clone, Copy)]
pub struct PStateEntry {
    /// P-state index (0 = highest performance).
    pub index: u8,
    /// Frequency in MHz.
    pub freq_mhz: u32,
    /// Voltage in millivolts.
    pub voltage_mv: u32,
    /// Relative power at this P-state (P0 = 1000).
    pub power_ratio: u32,
}

/// P-state operating mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PStateMode {
    /// Table-driven: OS selects from a fixed set of P-states.
    /// Used on older CPUs or when HWP is disabled.
    TableDriven,
    /// Range-driven: OS sets min/max, hardware selects optimally.
    /// Used with Intel HWP or ACPI CPPC.
    HardwareManaged,
}

/// Per-CPU P-state controller.
pub struct PStateController {
    /// Operating mode.
    pub mode: PStateMode,
    /// Number of valid entries in `table`.
    pub num_pstates: u8,
    /// P-state table (highest freq first).
    pub table: [PStateEntry; MAX_PSTATES],
    /// Current P-state index.
    pub current: AtomicU32,
    /// Minimum allowed P-state index (set by thermal throttling or policy).
    pub min_pstate: AtomicU32,
    /// Maximum allowed P-state index (set by user policy).
    pub max_pstate: AtomicU32,
    /// For HWP mode: desired performance (0-255, where 255 = max).
    pub hwp_desired: AtomicU32,
    /// For HWP mode: energy-performance preference (0 = max perf, 255 = max efficiency).
    pub hwp_epp: AtomicU32,
}

impl PStateController {
    /// Create a new controller with a default P-state table.
    pub fn new() -> Self {
        let mut table = [PStateEntry {
            index: 0, freq_mhz: 0, voltage_mv: 0, power_ratio: 0,
        }; MAX_PSTATES];

        // Default table: 8 P-states from 4000 MHz down to 800 MHz.
        let defaults = [
            (4000, 1100, 1000),
            (3500, 1050,  750),
            (3000, 1000,  560),
            (2500,  950,  400),
            (2000,  900,  280),
            (1500,  850,  180),
            (1000,  800,  100),
            ( 800,  750,   65),
        ];

        for (i, &(freq, volt, power)) in defaults.iter().enumerate() {
            table[i] = PStateEntry {
                index: i as u8,
                freq_mhz: freq,
                voltage_mv: volt,
                power_ratio: power,
            };
        }

        PStateController {
            mode: PStateMode::TableDriven,
            num_pstates: defaults.len() as u8,
            table,
            current: AtomicU32::new(0),
            min_pstate: AtomicU32::new(0),
            max_pstate: AtomicU32::new(defaults.len() as u32 - 1),
            hwp_desired: AtomicU32::new(0),
            hwp_epp: AtomicU32::new(128), // balanced
        }
    }

    /// Select the target P-state based on current utilisation.
    ///
    /// Implements the schedutil algorithm:
    /// `target_freq = headroom × max_freq × (util / 1000)`
    ///
    /// Then maps the target frequency to the nearest P-state.
    pub fn select_for_util(&self, util: u32) -> u8 {
        let max_freq = self.table[0].freq_mhz;
        if max_freq == 0 || self.num_pstates == 0 {
            return 0;
        }

        // schedutil formula with headroom.
        let target_freq = (SCHEDUTIL_HEADROOM as u64 * max_freq as u64
            * util.min(1000) as u64) / (1024 * 1000);

        // Find the lowest-frequency P-state that meets the target.
        let min_p = self.min_pstate.load(Ordering::Relaxed) as u8;
        let max_p = self.max_pstate.load(Ordering::Relaxed) as u8;

        let mut best = min_p;
        for i in (min_p..=max_p).rev() {
            if (i as usize) < self.num_pstates as usize {
                if self.table[i as usize].freq_mhz as u64 >= target_freq {
                    best = i;
                }
            }
        }

        best
    }

    /// Set the target P-state (will be applied on next governor tick).
    pub fn set_target(&self, pstate: u8) {
        let min_p = self.min_pstate.load(Ordering::Relaxed);
        let max_p = self.max_pstate.load(Ordering::Relaxed);
        let clamped = (pstate as u32).clamp(min_p, max_p);
        self.current.store(clamped, Ordering::Release);
    }

    /// Apply the current P-state to hardware.
    ///
    /// In a real implementation, this writes to MSR_IA32_PERF_CTL (Intel)
    /// or equivalent. Here we update the state tracking.
    pub fn apply(&self) -> u32 {
        let pstate = self.current.load(Ordering::Acquire) as usize;
        if pstate < self.num_pstates as usize {
            self.table[pstate].freq_mhz
        } else {
            0
        }
    }

    /// Cap the maximum P-state (used by thermal throttling).
    ///
    /// `cap` is the highest P-state index allowed (0 = max freq only).
    pub fn set_thermal_cap(&self, cap: u8) {
        let new_min = (cap as u32).min(self.num_pstates as u32 - 1);
        self.min_pstate.store(new_min, Ordering::Release);

        // If current is below the cap, force it.
        let current = self.current.load(Ordering::Relaxed);
        if current < new_min {
            self.current.store(new_min, Ordering::Release);
        }
    }

    /// Remove thermal cap (restore full frequency range).
    pub fn clear_thermal_cap(&self) {
        self.min_pstate.store(0, Ordering::Release);
    }

    /// Get the frequency for a given P-state index.
    pub fn freq_for_pstate(&self, pstate: u8) -> u32 {
        if (pstate as usize) < self.num_pstates as usize {
            self.table[pstate as usize].freq_mhz
        } else {
            0
        }
    }

    /// Get the power ratio for a given P-state index.
    pub fn power_for_pstate(&self, pstate: u8) -> u32 {
        if (pstate as usize) < self.num_pstates as usize {
            self.table[pstate as usize].power_ratio
        } else {
            0
        }
    }
}
