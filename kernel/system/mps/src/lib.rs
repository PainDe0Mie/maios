//! MPS — Mai Power Subsystem
//!
//! A research-backed power management subsystem for MaiOS implementing:
//!
//! 1. **CPU C-states** (idle power states)
//!    Based on: ACPI Specification 6.5, Chapter 8 — Processor Power States.
//!    Manages C0 (active), C1 (halt), C1E (enhanced halt), C3 (sleep),
//!    C6 (deep power down). Selects the optimal C-state based on predicted
//!    idle duration using the menu governor algorithm (Linux cpuidle).
//!
//! 2. **CPU P-states / DVFS** (Dynamic Voltage and Frequency Scaling)
//!    Based on: "Energy-Aware Scheduling" (ARM, 2020) + Intel SpeedStep /
//!    AMD Cool'n'Quiet / ACPI CPPC.
//!    Adjusts CPU frequency and voltage per-core to match workload demand,
//!    minimising energy while meeting performance targets.
//!
//! 3. **Energy-Aware Scheduling (EAS)** integration with MKS
//!    Based on: "Energy Aware Scheduling" (Morten Rasmussen, Linux Plumbers 2015)
//!    + "Capacity Aware Scheduling" (ARM big.LITTLE).
//!    Provides energy cost estimates so MKS can prefer power-efficient cores
//!    for light workloads and performance cores for heavy ones.
//!
//! 4. **Thermal monitoring and throttling**
//!    Based on: Linux thermal framework, Intel Digital Thermal Sensor (DTS).
//!    Monitors per-core temperatures, enforces thermal limits via progressive
//!    throttling with hysteresis to avoid oscillation.
//!
//! 5. **Power domains and budgeting**
//!    Based on: Intel RAPL (Running Average Power Limit), AMD PPT.
//!    Per-package and per-core power budgets with enforcement via P-state capping.
//!
//! ## Design invariants
//!
//! - All MSR reads/writes are centralised in the `hw` module (hardware abstraction).
//! - Governors run periodically from the timer tick, not in interrupt context.
//! - No allocations on the hot path (governor tick).
//! - Per-CPU state is indexed by CPU ID, no locking needed for local reads.

#![no_std]
#![allow(dead_code)]

extern crate alloc;

pub mod cstate;
pub mod pstate;
pub mod thermal;
pub mod energy;
pub mod governor;
pub mod hw;
pub mod budget;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicBool, Ordering};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum CPUs supported (matches MKS).
pub const MAX_CPUS: usize = 256;

/// Governor tick interval in milliseconds.
/// The power governor runs at this frequency to adjust P-states and C-state hints.
/// Based on: Linux schedutil governor (default ~10ms).
pub const GOVERNOR_TICK_MS: u64 = 10;

/// Thermal polling interval in milliseconds.
pub const THERMAL_POLL_MS: u64 = 1000;

/// Default power budget per package in milliwatts.
pub const DEFAULT_PACKAGE_POWER_MW: u32 = 65_000; // 65W TDP

/// Temperature threshold for thermal throttling (°C).
pub const THERMAL_THROTTLE_TEMP_C: u32 = 90;

/// Temperature at which throttling is released (hysteresis, °C).
pub const THERMAL_CLEAR_TEMP_C: u32 = 80;

/// Critical temperature — emergency shutdown threshold (°C).
pub const THERMAL_CRITICAL_TEMP_C: u32 = 105;

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

static MPS: spin::Once<MaiPower> = spin::Once::new();

/// Initialize the MPS subsystem.
///
/// Called once during kernel boot, after ACPI tables are parsed and
/// CPU topology is known.
pub fn init(num_cpus: usize, num_packages: usize) {
    MPS.call_once(|| MaiPower::new(num_cpus, num_packages));
}

/// Access the global MPS instance.
#[inline]
pub fn get() -> &'static MaiPower {
    MPS.get().expect("MPS: subsystem not initialized — call mps::init() first")
}

// ---------------------------------------------------------------------------
// Per-CPU power state
// ---------------------------------------------------------------------------

/// Power state of a single CPU core.
pub struct CpuPowerState {
    /// CPU ID.
    pub cpu_id: u32,
    /// Package (socket) ID this CPU belongs to.
    pub package_id: u32,
    /// Current P-state index (0 = highest frequency).
    pub current_pstate: AtomicU32,
    /// Target P-state (set by governor, applied on next tick).
    pub target_pstate: AtomicU32,
    /// Current frequency in MHz.
    pub current_freq_mhz: AtomicU32,
    /// Maximum frequency in MHz (turbo).
    pub max_freq_mhz: u32,
    /// Minimum frequency in MHz.
    pub min_freq_mhz: u32,
    /// Base frequency in MHz (non-turbo).
    pub base_freq_mhz: u32,
    /// Current C-state (0 = active).
    pub current_cstate: AtomicU32,
    /// Deepest C-state allowed for this CPU.
    pub max_cstate: u32,
    /// Current temperature in °C.
    pub temperature_c: AtomicU32,
    /// Whether this CPU is thermally throttled.
    pub throttled: AtomicBool,
    /// Cumulative energy consumed in microjoules (from RAPL or estimate).
    pub energy_uj: AtomicU64,
    /// CPU utilisation in the last governor period (0-1000, i.e. 0.0%-100.0%).
    pub utilization: AtomicU32,
    /// Energy cost coefficient for this CPU (for EAS).
    /// Higher = more power-hungry. Relative to other CPUs.
    pub energy_cost: u32,
}

impl CpuPowerState {
    fn new(cpu_id: u32, package_id: u32) -> Self {
        CpuPowerState {
            cpu_id,
            package_id,
            current_pstate: AtomicU32::new(0),
            target_pstate: AtomicU32::new(0),
            current_freq_mhz: AtomicU32::new(0),
            max_freq_mhz: 0,
            min_freq_mhz: 0,
            base_freq_mhz: 0,
            current_cstate: AtomicU32::new(0),
            max_cstate: 6, // C6 by default
            temperature_c: AtomicU32::new(0),
            throttled: AtomicBool::new(false),
            energy_uj: AtomicU64::new(0),
            utilization: AtomicU32::new(0),
            energy_cost: 100, // default cost (normalised)
        }
    }

    /// Check if this CPU is idle (C-state > 0).
    #[inline]
    pub fn is_idle(&self) -> bool {
        self.current_cstate.load(Ordering::Relaxed) > 0
    }

    /// Get current frequency as a fraction of max (0-1000).
    #[inline]
    pub fn freq_ratio(&self) -> u32 {
        if self.max_freq_mhz == 0 { return 0; }
        (self.current_freq_mhz.load(Ordering::Relaxed) as u64 * 1000
            / self.max_freq_mhz as u64) as u32
    }
}

// ---------------------------------------------------------------------------
// Package (socket) power state
// ---------------------------------------------------------------------------

/// Power state of a physical CPU package (socket).
pub struct PackagePowerState {
    /// Package ID.
    pub package_id: u32,
    /// CPUs in this package.
    pub cpu_ids: Vec<u32>,
    /// Power budget in milliwatts (from RAPL PL1 or configured).
    pub power_budget_mw: AtomicU32,
    /// Current power draw in milliwatts (estimated or from RAPL).
    pub current_power_mw: AtomicU32,
    /// Cumulative energy in microjoules.
    pub energy_uj: AtomicU64,
    /// Whether the package is power-limited (budget exceeded).
    pub power_limited: AtomicBool,
    /// Package temperature (hottest core).
    pub temperature_c: AtomicU32,
}

impl PackagePowerState {
    fn new(package_id: u32) -> Self {
        PackagePowerState {
            package_id,
            cpu_ids: Vec::new(),
            power_budget_mw: AtomicU32::new(DEFAULT_PACKAGE_POWER_MW),
            current_power_mw: AtomicU32::new(0),
            energy_uj: AtomicU64::new(0),
            power_limited: AtomicBool::new(false),
            temperature_c: AtomicU32::new(0),
        }
    }

    /// Check if the package is over its power budget.
    #[inline]
    pub fn is_over_budget(&self) -> bool {
        self.current_power_mw.load(Ordering::Relaxed)
            > self.power_budget_mw.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Main power subsystem
// ---------------------------------------------------------------------------

/// The Mai Power Subsystem.
///
/// Manages per-CPU and per-package power states, runs the power governor,
/// and provides energy cost information for the scheduler.
pub struct MaiPower {
    /// Per-CPU power states.
    pub cpus: Vec<CpuPowerState>,
    /// Per-package power states.
    pub packages: Vec<PackagePowerState>,
    /// Number of CPUs.
    pub num_cpus: AtomicU32,
    /// Number of packages.
    pub num_packages: u32,
    /// Global thermal state: any CPU thermally throttled?
    pub any_throttled: AtomicBool,
    /// Total energy consumed across all CPUs (microjoules).
    pub total_energy_uj: AtomicU64,
    /// Governor tick counter.
    pub governor_ticks: AtomicU64,
    /// Whether the power subsystem is active.
    pub active: AtomicBool,
}

impl MaiPower {
    fn new(num_cpus: usize, num_packages: usize) -> Self {
        let mut cpus = Vec::with_capacity(num_cpus);
        for i in 0..num_cpus {
            let pkg_id = (i / (num_cpus.max(1) / num_packages.max(1)).max(1)) as u32;
            cpus.push(CpuPowerState::new(i as u32, pkg_id));
        }

        let mut packages = Vec::with_capacity(num_packages);
        for p in 0..num_packages {
            let mut pkg = PackagePowerState::new(p as u32);
            for (i, cpu) in cpus.iter().enumerate() {
                if cpu.package_id == p as u32 {
                    pkg.cpu_ids.push(i as u32);
                }
            }
            packages.push(pkg);
        }

        MaiPower {
            cpus,
            packages,
            num_cpus: AtomicU32::new(num_cpus as u32),
            num_packages: num_packages as u32,
            any_throttled: AtomicBool::new(false),
            total_energy_uj: AtomicU64::new(0),
            governor_ticks: AtomicU64::new(0),
            active: AtomicBool::new(true),
        }
    }

    /// Called from the timer tick handler.
    ///
    /// Updates utilisation, runs the P-state governor, checks thermal limits.
    /// Should be called every GOVERNOR_TICK_MS milliseconds.
    pub fn tick(&self, cpu_id: usize) {
        if !self.active.load(Ordering::Relaxed) {
            return;
        }

        if cpu_id >= self.cpus.len() {
            return;
        }

        self.governor_ticks.fetch_add(1, Ordering::Relaxed);

        // Run the per-CPU governor.
        governor::tick(self, cpu_id);

        // Periodically check thermals (every THERMAL_POLL_MS / GOVERNOR_TICK_MS ticks).
        let tick_count = self.governor_ticks.load(Ordering::Relaxed);
        let thermal_interval = THERMAL_POLL_MS / GOVERNOR_TICK_MS;
        if tick_count % thermal_interval == 0 {
            thermal::check(self, cpu_id);
        }
    }

    /// Update CPU utilisation (called by MKS after each scheduling period).
    ///
    /// `util` is in the range [0, 1000] representing 0.0% to 100.0%.
    pub fn update_utilization(&self, cpu_id: usize, util: u32) {
        if let Some(cpu) = self.cpus.get(cpu_id) {
            cpu.utilization.store(util.min(1000), Ordering::Relaxed);
        }
    }

    /// Get the energy cost for placing a task on `cpu_id`.
    ///
    /// Used by MKS Energy-Aware Scheduling to prefer efficient cores.
    /// Returns a relative cost (lower = more efficient).
    pub fn energy_cost(&self, cpu_id: usize) -> u32 {
        self.cpus.get(cpu_id)
            .map(|cpu| {
                let base_cost = cpu.energy_cost;
                let freq_factor = cpu.freq_ratio();
                // Cost scales with frequency (approximately cubic: P ∝ V²·f ≈ f³).
                // Simplified to quadratic for fast computation.
                base_cost * freq_factor / 1000
            })
            .unwrap_or(u32::MAX)
    }

    /// Suggest the most energy-efficient CPU for a task with the given
    /// utilisation requirement (0-1000).
    ///
    /// This is the EAS placement hook: for light tasks, prefer the CPU
    /// that can handle the load at the lowest energy cost.
    pub fn suggest_efficient_cpu(&self, required_util: u32) -> usize {
        let mut best_cpu = 0;
        let mut best_cost = u32::MAX;

        for (i, cpu) in self.cpus.iter().enumerate() {
            if cpu.throttled.load(Ordering::Relaxed) {
                continue;
            }

            // Check if this CPU has enough remaining capacity.
            let current_util = cpu.utilization.load(Ordering::Relaxed);
            let remaining = 1000u32.saturating_sub(current_util);
            if remaining < required_util {
                continue;
            }

            let cost = self.energy_cost(i);
            if cost < best_cost {
                best_cost = cost;
                best_cpu = i;
            }
        }

        best_cpu
    }

    /// Get a snapshot of the global power state.
    pub fn snapshot(&self) -> PowerSnapshot {
        let num = self.num_cpus.load(Ordering::Relaxed) as usize;
        let mut total_power = 0u32;
        let mut max_temp = 0u32;

        for pkg in &self.packages {
            total_power += pkg.current_power_mw.load(Ordering::Relaxed);
            let temp = pkg.temperature_c.load(Ordering::Relaxed);
            if temp > max_temp { max_temp = temp; }
        }

        PowerSnapshot {
            num_cpus: num as u32,
            total_power_mw: total_power,
            max_temperature_c: max_temp,
            any_throttled: self.any_throttled.load(Ordering::Relaxed),
            total_energy_uj: self.total_energy_uj.load(Ordering::Relaxed),
            governor_ticks: self.governor_ticks.load(Ordering::Relaxed),
        }
    }
}

/// Immutable snapshot of global power state.
#[derive(Debug, Clone)]
pub struct PowerSnapshot {
    pub num_cpus: u32,
    pub total_power_mw: u32,
    pub max_temperature_c: u32,
    pub any_throttled: bool,
    pub total_energy_uj: u64,
    pub governor_ticks: u64,
}
