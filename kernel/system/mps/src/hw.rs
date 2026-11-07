//! Hardware abstraction for power management MSRs and registers.
//!
//! All hardware-specific reads and writes are centralised here to provide
//! a clean abstraction boundary. This module handles:
//!
//! - Intel IA32_PERF_CTL / IA32_PERF_STATUS (P-state control)
//! - Intel IA32_THERM_STATUS (thermal sensor)
//! - Intel MSR_RAPL_POWER_UNIT / MSR_PKG_ENERGY_STATUS (RAPL energy metering)
//! - Intel IA32_HWP_REQUEST (Hardware P-states)
//! - AMD MSR_PSTATE_* (AMD P-state control)
//! - MWAIT hint values for C-states
//!
//! In Phase 1, these are stubs that track state in software.
//! In Phase 2, they will use `rdmsr`/`wrmsr` instructions.

/// x86 MSR addresses for power management.
pub mod msr {
    /// Intel: current P-state request.
    pub const IA32_PERF_CTL: u32 = 0x199;
    /// Intel: current P-state status.
    pub const IA32_PERF_STATUS: u32 = 0x198;
    /// Intel: thermal status and interrupt.
    pub const IA32_THERM_STATUS: u32 = 0x19C;
    /// Intel: package thermal status.
    pub const IA32_PACKAGE_THERM_STATUS: u32 = 0x1B1;
    /// Intel: temperature target (TjMax).
    pub const MSR_TEMPERATURE_TARGET: u32 = 0x1A2;
    /// Intel: RAPL power unit.
    pub const MSR_RAPL_POWER_UNIT: u32 = 0x606;
    /// Intel: package energy status.
    pub const MSR_PKG_ENERGY_STATUS: u32 = 0x611;
    /// Intel: PP0 (core) energy status.
    pub const MSR_PP0_ENERGY_STATUS: u32 = 0x639;
    /// Intel: package power limit.
    pub const MSR_PKG_POWER_LIMIT: u32 = 0x610;
    /// Intel: package power info (TDP).
    pub const MSR_PKG_POWER_INFO: u32 = 0x614;
    /// Intel: HWP request.
    pub const IA32_HWP_REQUEST: u32 = 0x774;
    /// Intel: HWP capabilities.
    pub const IA32_HWP_CAPABILITIES: u32 = 0x771;
    /// Intel: MPERF (actual performance counter).
    pub const IA32_MPERF: u32 = 0xE7;
    /// Intel: APERF (requested performance counter).
    pub const IA32_APERF: u32 = 0xE8;
}

/// MWAIT hint values for C-state entry (Intel).
///
/// The hint is passed in EAX to the MWAIT instruction.
/// Format: bits [7:4] = sub C-state, bits [3:0] = C-state.
pub mod mwait_hints {
    pub const C1: u32  = 0x00;
    pub const C1E: u32 = 0x01;
    pub const C3: u32  = 0x10;
    pub const C6: u32  = 0x20;
    pub const C7: u32  = 0x30;
}

/// Read an MSR (stub — returns 0 in Phase 1).
///
/// In Phase 2, this will use the `rdmsr` instruction:
/// ```ignore
/// unsafe {
///     let (lo, hi): (u32, u32);
///     asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi);
///     ((hi as u64) << 32) | (lo as u64)
/// }
/// ```
#[inline]
pub fn read_msr(_msr: u32) -> u64 {
    // Phase 1 stub.
    0
}

/// Write an MSR (stub — no-op in Phase 1).
///
/// In Phase 2, this will use the `wrmsr` instruction.
#[inline]
pub fn write_msr(_msr: u32, _value: u64) {
    // Phase 1 stub.
}

/// Read the CPU temperature from the Digital Thermal Sensor.
///
/// Returns temperature in °C.
///
/// On Intel: TjMax - digital_readout from IA32_THERM_STATUS[22:16].
/// Default TjMax = 100°C.
pub fn read_cpu_temperature() -> u32 {
    let _therm_status = read_msr(msr::IA32_THERM_STATUS);
    // Phase 1 stub: return a safe default.
    // Real implementation:
    //   let digital_readout = (therm_status >> 16) & 0x7F;
    //   let tj_max = read_tj_max();
    //   tj_max - digital_readout as u32
    45 // 45°C default
}

/// Read TjMax (maximum junction temperature) from MSR.
pub fn read_tj_max() -> u32 {
    let _target = read_msr(msr::MSR_TEMPERATURE_TARGET);
    // Real: (target >> 16) & 0xFF
    100 // default 100°C
}

/// Read package energy consumption from RAPL.
///
/// Returns energy in microjoules since last reset.
pub fn read_package_energy_uj() -> u64 {
    let _raw = read_msr(msr::MSR_PKG_ENERGY_STATUS);
    // Real: raw * energy_unit_uj (from MSR_RAPL_POWER_UNIT)
    0
}

/// Read core energy consumption from RAPL.
pub fn read_core_energy_uj() -> u64 {
    let _raw = read_msr(msr::MSR_PP0_ENERGY_STATUS);
    0
}

/// Set the P-state via IA32_PERF_CTL.
///
/// `ratio` is the target frequency ratio (e.g., 40 for 4.0 GHz on 100 MHz bus).
pub fn set_pstate_ratio(ratio: u8) {
    let value = (ratio as u64) << 8;
    write_msr(msr::IA32_PERF_CTL, value);
}

/// Read the current P-state from IA32_PERF_STATUS.
pub fn read_pstate_ratio() -> u8 {
    let status = read_msr(msr::IA32_PERF_STATUS);
    ((status >> 8) & 0xFF) as u8
}

/// Set HWP request (min perf, max perf, desired perf, EPP).
pub fn set_hwp_request(min: u8, max: u8, desired: u8, epp: u8) {
    let value = (epp as u64) << 24
        | (desired as u64) << 16
        | (max as u64) << 8
        | (min as u64);
    write_msr(msr::IA32_HWP_REQUEST, value);
}

/// Read MPERF/APERF ratio to determine actual vs. requested frequency.
///
/// Returns (aperf, mperf). The ratio aperf/mperf × base_freq = actual_freq.
pub fn read_perf_counters() -> (u64, u64) {
    let aperf = read_msr(msr::IA32_APERF);
    let mperf = read_msr(msr::IA32_MPERF);
    (aperf, mperf)
}

/// Enter a C-state using MWAIT.
///
/// Requires MONITOR/MWAIT support (CPUID.01H:ECX.MONITOR[bit 3]).
pub fn enter_cstate(_hint: u32) {
    // Phase 1 stub.
    // Real implementation:
    // unsafe {
    //     asm!("monitor", in("eax") addr, in("ecx") 0, in("edx") 0);
    //     asm!("mwait", in("eax") hint, in("ecx") 0);
    // }
}

/// Check if Intel HWP (Hardware P-states) is supported.
pub fn hwp_supported() -> bool {
    // CPUID.06H:EAX.HWP[bit 7]
    // Phase 1 stub.
    false
}

/// Check if Intel RAPL is supported.
pub fn rapl_supported() -> bool {
    // Check if MSR_RAPL_POWER_UNIT is readable.
    // Phase 1 stub.
    false
}
