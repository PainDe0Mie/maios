//! Power domain and budget management.
//!
//! Implements per-package power budgeting based on Intel RAPL
//! (Running Average Power Limit) and AMD PPT (Package Power Tracking).
//!
//! Based on:
//! - "Running Average Power Limit" (Intel Software Developer's Manual, Vol. 3B)
//! - "AMD PPT" (AMD Platform Power Tracking)
//! - "PowerCap" framework in Linux kernel
//!
//! ## Power budgeting
//!
//! Each CPU package has two power limits:
//! - **PL1** (long-term): sustained power limit, typically = TDP.
//! - **PL2** (short-term): burst power limit, typically = 1.25× TDP.
//!
//! The budget manager monitors power consumption and caps P-states
//! when the budget is exceeded over a sliding window.

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicBool, Ordering};

/// Power limit configuration for a package.
#[derive(Debug, Clone, Copy)]
pub struct PowerLimits {
    /// PL1: long-term sustained power limit in milliwatts (= TDP).
    pub pl1_mw: u32,
    /// PL1 time window in milliseconds (typically 8000-28000ms).
    pub pl1_window_ms: u32,
    /// PL2: short-term burst power limit in milliwatts (typically 1.25× PL1).
    pub pl2_mw: u32,
    /// PL2 time window in milliseconds (typically 2500-10000ms).
    pub pl2_window_ms: u32,
    /// Whether PL1 is enforced.
    pub pl1_enabled: bool,
    /// Whether PL2 is enforced.
    pub pl2_enabled: bool,
}

impl Default for PowerLimits {
    fn default() -> Self {
        PowerLimits {
            pl1_mw: 65_000,       // 65W TDP
            pl1_window_ms: 8_000, // 8 seconds
            pl2_mw: 81_250,       // 1.25× PL1
            pl2_window_ms: 2_500, // 2.5 seconds
            pl1_enabled: true,
            pl2_enabled: true,
        }
    }
}

/// Sliding window energy tracker for budget enforcement.
///
/// Uses a circular buffer of energy samples taken at regular intervals.
/// The sum of the buffer gives energy consumed over the window.
pub struct EnergyWindow {
    /// Circular buffer of energy samples (millijoules per sample period).
    samples: [AtomicU32; 64],
    /// Current write index.
    write_idx: AtomicU32,
    /// Number of valid samples.
    valid_count: AtomicU32,
    /// Sample period in milliseconds.
    pub sample_period_ms: u32,
}

impl EnergyWindow {
    pub const fn new(sample_period_ms: u32) -> Self {
        // Can't use a loop in const fn, so we use a macro-style approach.
        const ZERO: AtomicU32 = AtomicU32::new(0);
        EnergyWindow {
            samples: [
                ZERO, ZERO, ZERO, ZERO, ZERO, ZERO, ZERO, ZERO,
                ZERO, ZERO, ZERO, ZERO, ZERO, ZERO, ZERO, ZERO,
                ZERO, ZERO, ZERO, ZERO, ZERO, ZERO, ZERO, ZERO,
                ZERO, ZERO, ZERO, ZERO, ZERO, ZERO, ZERO, ZERO,
                ZERO, ZERO, ZERO, ZERO, ZERO, ZERO, ZERO, ZERO,
                ZERO, ZERO, ZERO, ZERO, ZERO, ZERO, ZERO, ZERO,
                ZERO, ZERO, ZERO, ZERO, ZERO, ZERO, ZERO, ZERO,
                ZERO, ZERO, ZERO, ZERO, ZERO, ZERO, ZERO, ZERO,
            ],
            write_idx: AtomicU32::new(0),
            valid_count: AtomicU32::new(0),
            sample_period_ms,
        }
    }

    /// Add a new energy sample (millijoules consumed since last sample).
    pub fn push(&self, energy_mj: u32) {
        let idx = self.write_idx.fetch_add(1, Ordering::Relaxed) as usize % 64;
        self.samples[idx].store(energy_mj, Ordering::Relaxed);
        let _ = self.valid_count.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |c| {
            if c < 64 { Some(c + 1) } else { None }
        });
    }

    /// Compute total energy in the window (millijoules).
    pub fn total_mj(&self) -> u32 {
        let count = self.valid_count.load(Ordering::Relaxed).min(64) as usize;
        let mut sum = 0u32;
        for i in 0..count {
            sum = sum.saturating_add(self.samples[i].load(Ordering::Relaxed));
        }
        sum
    }

    /// Compute average power over the window (milliwatts).
    pub fn average_power_mw(&self) -> u32 {
        let count = self.valid_count.load(Ordering::Relaxed).min(64);
        if count == 0 {
            return 0;
        }
        let total = self.total_mj();
        let window_ms = count * self.sample_period_ms;
        if window_ms == 0 {
            return 0;
        }
        // P(mW) = E(mJ) / t(ms) × 1000
        (total as u64 * 1000 / window_ms as u64) as u32
    }
}

/// Per-package power budget controller.
pub struct PackageBudget {
    /// Power limits.
    pub limits: PowerLimits,
    /// Long-term energy window (for PL1).
    pub long_term: EnergyWindow,
    /// Short-term energy window (for PL2).
    pub short_term: EnergyWindow,
    /// Whether PL1 is currently being exceeded.
    pub pl1_exceeded: AtomicBool,
    /// Whether PL2 is currently being exceeded.
    pub pl2_exceeded: AtomicBool,
    /// Number of times PL1 was exceeded.
    pub pl1_violations: AtomicU64,
    /// Number of times PL2 was exceeded.
    pub pl2_violations: AtomicU64,
    /// Current budget headroom in milliwatts (positive = under budget).
    pub headroom_mw: AtomicU32,
}

impl PackageBudget {
    /// Create a new budget controller with default limits.
    pub fn new(limits: PowerLimits) -> Self {
        let pl1_sample_period = limits.pl1_window_ms / 64;
        let pl2_sample_period = limits.pl2_window_ms / 64;

        PackageBudget {
            limits,
            long_term: EnergyWindow::new(pl1_sample_period.max(1)),
            short_term: EnergyWindow::new(pl2_sample_period.max(1)),
            pl1_exceeded: AtomicBool::new(false),
            pl2_exceeded: AtomicBool::new(false),
            pl1_violations: AtomicU64::new(0),
            pl2_violations: AtomicU64::new(0),
            headroom_mw: AtomicU32::new(limits.pl1_mw),
        }
    }

    /// Check if the current power consumption exceeds limits.
    ///
    /// Returns `true` if any limit is exceeded (throttling needed).
    pub fn check(&self) -> bool {
        let mut exceeded = false;

        if self.limits.pl1_enabled {
            let avg_power = self.long_term.average_power_mw();
            if avg_power > self.limits.pl1_mw {
                self.pl1_exceeded.store(true, Ordering::Release);
                self.pl1_violations.fetch_add(1, Ordering::Relaxed);
                exceeded = true;
                self.headroom_mw.store(0, Ordering::Relaxed);
            } else {
                self.pl1_exceeded.store(false, Ordering::Release);
                self.headroom_mw.store(
                    self.limits.pl1_mw - avg_power,
                    Ordering::Relaxed,
                );
            }
        }

        if self.limits.pl2_enabled {
            let avg_power = self.short_term.average_power_mw();
            if avg_power > self.limits.pl2_mw {
                self.pl2_exceeded.store(true, Ordering::Release);
                self.pl2_violations.fetch_add(1, Ordering::Relaxed);
                exceeded = true;
            } else {
                self.pl2_exceeded.store(false, Ordering::Release);
            }
        }

        exceeded
    }

    /// Get the remaining power budget headroom in milliwatts.
    #[inline]
    pub fn headroom(&self) -> u32 {
        self.headroom_mw.load(Ordering::Relaxed)
    }

    /// Check if currently over budget.
    #[inline]
    pub fn is_over_budget(&self) -> bool {
        self.pl1_exceeded.load(Ordering::Relaxed) || self.pl2_exceeded.load(Ordering::Relaxed)
    }
}
