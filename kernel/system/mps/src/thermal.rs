//! Thermal monitoring and throttling.
//!
//! Monitors per-core CPU temperatures and enforces thermal limits via
//! progressive P-state capping with hysteresis to avoid oscillation.
//!
//! Based on:
//! - Intel Digital Thermal Sensor (DTS) / Package Thermal Management
//! - Linux thermal framework (thermal_zone, cooling_device)
//! - "Thermal Management in Modern Processors" (Brooks & Martonosi, HPCA 2001)
//!
//! ## Throttling strategy
//!
//! Instead of binary on/off throttling (which causes visible stutter),
//! MPS uses progressive throttling with 4 levels:
//!
//! | Level | Trigger (°C) | Action                              |
//! |-------|-------------|-------------------------------------|
//! | 0     | < 80        | No throttling — full P-state range  |
//! | 1     | 80-89       | Cap to P2 (~75% max freq)           |
//! | 2     | 90-99       | Cap to P4 (~50% max freq)           |
//! | 3     | 100-104     | Cap to P6 (~25% max freq)           |
//! | 4     | ≥ 105       | Emergency — halt non-critical tasks  |
//!
//! Hysteresis: each level has a 5°C clearance margin to prevent rapid
//! toggling between levels.

use core::sync::atomic::{AtomicU32, Ordering};

/// Thermal throttling level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum ThrottleLevel {
    /// No throttling.
    None = 0,
    /// Mild: cap to ~75% max frequency.
    Mild = 1,
    /// Moderate: cap to ~50% max frequency.
    Moderate = 2,
    /// Severe: cap to ~25% max frequency.
    Severe = 3,
    /// Critical: emergency measures.
    Critical = 4,
}

/// Thermal zone thresholds (°C).
pub struct ThermalThresholds {
    /// Temperature at which throttling level 1 engages.
    pub mild_temp: u32,
    /// Temperature at which throttling level 2 engages.
    pub moderate_temp: u32,
    /// Temperature at which throttling level 3 engages.
    pub severe_temp: u32,
    /// Temperature at which critical measures are taken.
    pub critical_temp: u32,
    /// Hysteresis margin in °C.
    pub hysteresis: u32,
}

impl Default for ThermalThresholds {
    fn default() -> Self {
        ThermalThresholds {
            mild_temp: 80,
            moderate_temp: 90,
            severe_temp: 100,
            critical_temp: 105,
            hysteresis: 5,
        }
    }
}

/// P-state caps for each throttle level.
/// Index is the P-state index (0 = max freq, higher = lower freq).
pub const THROTTLE_PSTATE_CAPS: [u8; 5] = [
    0,  // None: no cap
    2,  // Mild: P2
    4,  // Moderate: P4
    6,  // Severe: P6
    7,  // Critical: P7 (minimum)
];

/// Per-CPU thermal state.
pub struct ThermalState {
    /// Current temperature in °C.
    pub temperature: AtomicU32,
    /// Current throttle level.
    pub throttle_level: AtomicU32,
    /// Peak temperature observed.
    pub peak_temperature: AtomicU32,
    /// Thresholds for this CPU.
    pub thresholds: ThermalThresholds,
}

impl ThermalState {
    pub fn new() -> Self {
        ThermalState {
            temperature: AtomicU32::new(0),
            throttle_level: AtomicU32::new(ThrottleLevel::None as u32),
            peak_temperature: AtomicU32::new(0),
            thresholds: ThermalThresholds::default(),
        }
    }

    /// Update temperature and compute the new throttle level.
    ///
    /// Returns the new throttle level (may differ from current due to hysteresis).
    pub fn update(&self, temp_c: u32) -> ThrottleLevel {
        self.temperature.store(temp_c, Ordering::Relaxed);

        // Update peak.
        let _ = self.peak_temperature.fetch_update(
            Ordering::Relaxed, Ordering::Relaxed,
            |peak| if temp_c > peak { Some(temp_c) } else { None },
        );

        let current_level = self.throttle_level.load(Ordering::Relaxed);
        let t = &self.thresholds;

        // Determine target level based on temperature.
        let target_level = if temp_c >= t.critical_temp {
            ThrottleLevel::Critical
        } else if temp_c >= t.severe_temp {
            ThrottleLevel::Severe
        } else if temp_c >= t.moderate_temp {
            ThrottleLevel::Moderate
        } else if temp_c >= t.mild_temp {
            ThrottleLevel::Mild
        } else {
            ThrottleLevel::None
        };

        let target_val = target_level as u32;

        // Apply hysteresis: only decrease level if temperature is below
        // the threshold minus hysteresis margin.
        let new_level = if target_val < current_level {
            // Cooling down: check hysteresis.
            let clear_temp = match current_level {
                4 => t.critical_temp.saturating_sub(t.hysteresis),
                3 => t.severe_temp.saturating_sub(t.hysteresis),
                2 => t.moderate_temp.saturating_sub(t.hysteresis),
                1 => t.mild_temp.saturating_sub(t.hysteresis),
                _ => 0,
            };
            if temp_c <= clear_temp {
                target_val
            } else {
                current_level // Stay at current level (hysteresis).
            }
        } else {
            target_val // Heating up: apply immediately.
        };

        self.throttle_level.store(new_level, Ordering::Release);

        match new_level {
            0 => ThrottleLevel::None,
            1 => ThrottleLevel::Mild,
            2 => ThrottleLevel::Moderate,
            3 => ThrottleLevel::Severe,
            _ => ThrottleLevel::Critical,
        }
    }

    /// Get the P-state cap for the current throttle level.
    pub fn pstate_cap(&self) -> u8 {
        let level = self.throttle_level.load(Ordering::Relaxed) as usize;
        THROTTLE_PSTATE_CAPS[level.min(THROTTLE_PSTATE_CAPS.len() - 1)]
    }
}

/// Check thermal state for a CPU and apply throttling.
///
/// Called periodically from the MPS tick handler.
pub fn check(mps: &super::MaiPower, cpu_id: usize) {
    if let Some(cpu) = mps.cpus.get(cpu_id) {
        let temp = cpu.temperature_c.load(Ordering::Relaxed);

        // In a real implementation, we would read the temperature from
        // the DTS MSR (IA32_THERM_STATUS) or ACPI thermal zone here.
        // For now, the temperature is updated externally.

        if temp >= super::THERMAL_THROTTLE_TEMP_C {
            if !cpu.throttled.load(Ordering::Relaxed) {
                cpu.throttled.store(true, Ordering::Release);
                mps.any_throttled.store(true, Ordering::Release);
            }
        } else if temp <= super::THERMAL_CLEAR_TEMP_C {
            if cpu.throttled.load(Ordering::Relaxed) {
                cpu.throttled.store(false, Ordering::Release);

                // Check if any other CPU is still throttled.
                let still_throttled = mps.cpus.iter()
                    .any(|c| c.throttled.load(Ordering::Relaxed));
                if !still_throttled {
                    mps.any_throttled.store(false, Ordering::Release);
                }
            }
        }

        // Update package temperature (max of all cores in package).
        if let Some(pkg) = mps.packages.get(cpu.package_id as usize) {
            let _ = pkg.temperature_c.fetch_update(
                Ordering::Relaxed, Ordering::Relaxed,
                |current| if temp > current { Some(temp) } else { None },
            );
        }
    }
}
