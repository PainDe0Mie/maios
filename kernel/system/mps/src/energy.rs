//! Energy-Aware Scheduling (EAS) integration.
//!
//! Provides energy models and cost functions for MKS to make power-aware
//! task placement decisions.
//!
//! Based on:
//! - ARM Energy-Aware Scheduling (EAS) in Linux 5.0+
//! - "Energy Aware Scheduling" (Morten Rasmussen, Linux Plumbers 2015)
//! - "Capacity-Aware Scheduling" (ARM big.LITTLE heterogeneous multiprocessing)
//! - "An Analysis of Energy Models for Energy-Aware Scheduling" (Imes et al., 2016)
//!
//! ## Energy Model
//!
//! Each CPU has an energy model describing its power consumption at
//! different operating points (OPPs = frequency/voltage pairs):
//!
//! ```text
//! Energy = Σ (power_at_opp × time_at_opp)
//!
//! For DVFS:  P_dynamic ∝ C × V² × f  (capacitance × voltage² × frequency)
//! Simplified: P ≈ k × f³  (since V scales roughly linearly with f)
//! ```
//!
//! ## Task energy estimation
//!
//! To estimate the energy cost of running a task on CPU `i`:
//! ```text
//! E(task, cpu_i) = P(opp_for_util) × (task_util / cpu_capacity)
//! ```
//! The scheduler picks the CPU that minimises total system energy.

use core::sync::atomic::{AtomicU32, Ordering};

/// An Operating Performance Point (OPP).
#[derive(Debug, Clone, Copy)]
pub struct Opp {
    /// Frequency in MHz.
    pub freq_mhz: u32,
    /// Dynamic power at this OPP in milliwatts.
    pub power_mw: u32,
    /// CPU capacity at this OPP (0-1024, where 1024 = max).
    pub capacity: u32,
}

/// Energy model for a CPU type (all CPUs of the same type share a model).
#[derive(Debug, Clone)]
pub struct EnergyModel {
    /// Name of the CPU type (e.g., "performance", "efficiency").
    pub name: &'static str,
    /// Operating Performance Points, sorted by frequency ascending.
    pub opps: &'static [Opp],
    /// Static (leakage) power in milliwatts (always consumed when powered on).
    pub static_power_mw: u32,
}

/// Default energy model for a symmetric x86_64 system.
///
/// Based on typical Intel Core i7 power characteristics.
pub static DEFAULT_ENERGY_MODEL: EnergyModel = EnergyModel {
    name: "x86_64-symmetric",
    opps: &[
        Opp { freq_mhz:  800, power_mw:  5_000, capacity: 200 },
        Opp { freq_mhz: 1000, power_mw:  8_000, capacity: 256 },
        Opp { freq_mhz: 1500, power_mw: 15_000, capacity: 384 },
        Opp { freq_mhz: 2000, power_mw: 25_000, capacity: 512 },
        Opp { freq_mhz: 2500, power_mw: 35_000, capacity: 640 },
        Opp { freq_mhz: 3000, power_mw: 50_000, capacity: 768 },
        Opp { freq_mhz: 3500, power_mw: 65_000, capacity: 896 },
        Opp { freq_mhz: 4000, power_mw: 85_000, capacity: 1024 },
    ],
    static_power_mw: 2_000,
};

/// Energy model for a hypothetical big.LITTLE-style heterogeneous system.
/// Efficiency cores consume less power at lower capacity.
pub static EFFICIENCY_CORE_MODEL: EnergyModel = EnergyModel {
    name: "efficiency-core",
    opps: &[
        Opp { freq_mhz:  600, power_mw:  1_500, capacity: 128 },
        Opp { freq_mhz:  800, power_mw:  2_500, capacity: 200 },
        Opp { freq_mhz: 1200, power_mw:  5_000, capacity: 300 },
        Opp { freq_mhz: 1800, power_mw: 10_000, capacity: 450 },
        Opp { freq_mhz: 2200, power_mw: 15_000, capacity: 550 },
    ],
    static_power_mw: 500,
};

/// Estimate the energy cost of running a task with `task_util` utilisation
/// on a CPU with the given energy model, at the OPP that provides enough
/// capacity.
///
/// Returns energy in micro-joules per scheduling period.
///
/// Algorithm:
/// 1. Find the lowest OPP whose capacity ≥ current_util + task_util.
/// 2. Compute: E = power_at_opp × (task_util / capacity).
pub fn estimate_task_energy(
    model: &EnergyModel,
    current_util: u32,
    task_util: u32,
) -> u32 {
    let total_util = current_util.saturating_add(task_util);

    // Find the lowest OPP that can handle the total utilisation.
    let opp = model.opps.iter()
        .find(|o| o.capacity >= total_util)
        .unwrap_or_else(|| model.opps.last().unwrap());

    // Energy cost of the task: proportional to its share of the OPP's capacity.
    let dynamic_energy = if opp.capacity > 0 {
        (opp.power_mw as u64 * task_util as u64 / opp.capacity as u64) as u32
    } else {
        0
    };

    // Add proportional share of static power.
    let static_energy = if opp.capacity > 0 {
        (model.static_power_mw as u64 * task_util as u64 / opp.capacity as u64) as u32
    } else {
        0
    };

    dynamic_energy + static_energy
}

/// Compare the energy cost of placing a task on two different CPUs.
///
/// Returns `true` if `cpu_a` is more energy-efficient for this task.
pub fn is_more_efficient(
    model_a: &EnergyModel,
    util_a: u32,
    model_b: &EnergyModel,
    util_b: u32,
    task_util: u32,
) -> bool {
    let energy_a = estimate_task_energy(model_a, util_a, task_util);
    let energy_b = estimate_task_energy(model_b, util_b, task_util);
    energy_a <= energy_b
}

/// Per-CPU energy tracking.
pub struct EnergyTracker {
    /// Cumulative energy in microjoules.
    pub total_uj: AtomicU32,
    /// Energy consumed in the last measurement window.
    pub window_uj: AtomicU32,
    /// Measurement window counter.
    pub window_count: AtomicU32,
}

impl EnergyTracker {
    pub const fn new() -> Self {
        EnergyTracker {
            total_uj: AtomicU32::new(0),
            window_uj: AtomicU32::new(0),
            window_count: AtomicU32::new(0),
        }
    }

    /// Add energy consumed.
    pub fn add(&self, uj: u32) {
        self.total_uj.fetch_add(uj, Ordering::Relaxed);
        self.window_uj.fetch_add(uj, Ordering::Relaxed);
    }

    /// Reset the measurement window.
    pub fn reset_window(&self) -> u32 {
        self.window_count.fetch_add(1, Ordering::Relaxed);
        self.window_uj.swap(0, Ordering::Relaxed)
    }
}
