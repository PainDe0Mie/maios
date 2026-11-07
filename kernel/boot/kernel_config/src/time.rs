/// The chosen interrupt frequency (in Hertz) of the PIT clock.
///
/// 1000 Hz = 1ms resolution for `sleep()` and timekeeping.
/// Higher values improve sleep precision but increase interrupt overhead.
/// Values above 1000 Hz are rarely useful on real hardware.
pub const CONFIG_PIT_FREQUENCY_HZ: u32 = 1000;

/// The chosen interrupt frequency (in Hertz) of the RTC.
///
/// Valid values are powers of 2 from 2 Hz to 8192 Hz.
/// Used for real-time clock events; 128 Hz is a reasonable default.
/// See [`change_rtc_frequency()`](rtc/).
pub const CONFIG_RTC_FREQUENCY_HZ: usize = 128;

/// The preemptive scheduling timeslice, in microseconds.
///
/// ## Impact on UI fluidity
/// This is the single most important tuning knob for perceived responsiveness:
/// - **4 ms (4000 µs)** — matches Linux at HZ=250; good balance of throughput vs latency.
///   Recommended for a graphical OS targeting smooth 60 fps rendering.
/// - **8 ms (8000 µs)** — Linux default at HZ=125; acceptable for server workloads,
///   but noticeably laggy for interactive UI (was the previous Theseus default).
/// - **1 ms (1000 µs)** — very low latency, but high context-switch overhead;
///   only useful if most tasks are very short-lived (e.g. real-time audio).
///
/// ## Relationship to framerate
/// At 4 ms, the scheduler interrupts ~250 times per second.
/// A rendering task scheduled every tick can theoretically hit 250 fps.
/// In practice, aim for a dedicated rendering task with a 16 ms (60 fps)
/// or 8 ms (120 fps) deadline that yields the CPU between frames.
///
/// ## Tradeoff
/// Smaller timeslice → more responsive UI, more context-switch overhead.
/// Larger timeslice → higher CPU throughput, less interactive feel.
pub const CONFIG_TIMESLICE_PERIOD_MICROSECONDS: u32 = 2_000; // 2ms — meilleur pour UI

/// The heartbeat log period, in milliseconds.
///
/// Used by the kernel to emit periodic "still alive" diagnostics.
/// 10 seconds is a reasonable default; lower it during development
/// if you want more frequent liveness checks.
pub const CONFIG_HEARTBEAT_PERIOD_MS: usize = 10_000;
