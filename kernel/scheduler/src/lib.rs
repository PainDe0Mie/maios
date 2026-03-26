//! MaiOS Scheduler crate — MKS integration layer.
//!
//! This crate is the **entry point** that the rest of the kernel uses.
//! It wires the hardware timer interrupt to MKS's tick handler,
//! and exposes the `schedule()` function (voluntary yield).
//!
//! It replaces the original Theseus scheduler crate while keeping
//! the same public API (`schedule`, `set_priority`, `priority`,
//! `inherit_priority`) so existing callers don't need to change.
//!
//! ## How context switching works
//!
//!  1. Timer fires → `timer_tick_handler` → `mks::get().tick(cpu, elapsed)`
//!     → sets `need_resched = true` on the run queue if preemption warranted.
//!  2. On return from the interrupt (or at any `schedule()` call site),
//!     the kernel checks `need_resched`.
//!  3. `schedule()` → `pick_next()` → context_switch assembly.
//!
//! ## Elapsed time measurement
//!
//! On x86_64 we use the TSC delta between ticks (calibrated at boot).
//! On aarch64 we use the virtual counter (CNTVCT_EL0).

#![no_std]
#![cfg_attr(target_arch = "x86_64", feature(abi_x86_interrupt))]
#![feature(thread_local)]

extern crate alloc;

use interrupts::{self, CPU_LOCAL_TIMER_IRQ, interrupt_handler, eoi, EoiBehaviour};
use core::sync::atomic::{AtomicU64, Ordering};
use mks::topology::CpuTopology;

// ---------------------------------------------------------------------------
// Re-exports for backward compatibility with existing MaiOS callers
// ---------------------------------------------------------------------------

pub use task::scheduler::schedule;

/// Set the nice value for the current task (maps to MKS nice).
pub fn set_priority(nice: i8) {
    let _ = task::with_current_task(|t| {
        // SAFETY: `t` is the current task on this CPU. No other CPU can
        // concurrently mutate our own sched fields while we are running.
        // We go through Arc::as_ptr → *mut to avoid &T→&mut T UB.
        let ptr = alloc::sync::Arc::as_ptr(&t.0) as *mut task_struct::Task;
        unsafe { (*ptr).set_nice(nice) };
    });
}

/// Get the nice value of the current task.
pub fn priority() -> i8 {
    // task::TaskRef: Deref<Target=Task> — direct field read, no lock needed.
    task::with_current_task(|t| t.sched.nice).unwrap_or(0)
}

/// Inherit scheduling parameters from a parent task.
///
/// NOTE: `task::TaskRef` wraps `Arc<Task>` (no RwLock). We use unsafe
/// interior mutation, safe because the caller owns the only reference
/// to the child at spawn time.
pub fn inherit_priority(child: &task::TaskRef, parent: &task::TaskRef) {
    let parent_nice = parent.sched.nice;
    let parent_policy = parent.sched.policy;
    // SAFETY: child is freshly created, no one else has a reference yet.
    let ptr = alloc::sync::Arc::as_ptr(&child.0) as *mut task_struct::Task;
    unsafe {
        (*ptr).sched.nice = parent_nice;
        (*ptr).sched.policy = parent_policy;
        (*ptr).update_weight();
    }
}

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

/// Initializes MKS and registers the hardware timer interrupt handler.
///
/// Call this once during boot, after `multicore_bringup` has enumerated CPUs.
///
/// # Arguments
/// * `num_cpus`  — number of logical CPUs online.
/// * `topology`  — CPU topology from ACPI/CPUID. Pass `CpuTopology::uniform(n)`
///                 if ACPI is not yet available.
pub fn init(num_cpus: usize, topology: CpuTopology) -> Result<(), &'static str> {
    // Initialize the global MKS instance.
    mks::init(num_cpus, topology);
    log::info!("MKS: initialized with {} CPUs, EEVDF + RT + Deadline + work-stealing", num_cpus);

    // Register MKS as the scheduling backend for task::scheduler.
    // This replaces the old global SCHEDULERS lock with lock-free per-CPU dispatch.
    task::scheduler::register_backend(
        |task| mks::get().enqueue(task),
        |cpu, task| {
            let rqs = mks::get().rqs.read();
            if let Some(rq) = rqs.get(cpu) {
                rq.enqueue(task);
            }
        },
        |task| mks::get().dequeue(task),
        |cpu| mks::get().pick_next(cpu),
        |cpu| {
            let rqs = mks::get().rqs.read();
            rqs.get(cpu).map(|rq| rq.load()).unwrap_or(0)
        },
        |cpu, task| {
            let rqs = mks::get().rqs.read();
            if let Some(rq) = rqs.get(cpu) {
                rq.idle_rq.lock().set_idle_task(task);
            }
        },
    );

    // Register put_prev: re-enqueue after yield/preemption (no vruntime reset).
    task::scheduler::register_put_prev(|cpu, task| {
        let rqs = mks::get().rqs.read();
        if let Some(rq) = rqs.get(cpu) {
            rq.put_prev(task);
        }
    });

    // Register the timer interrupt for preemptive scheduling.
    #[cfg(target_arch = "x86_64")]
    {
        interrupts::register_interrupt(
            CPU_LOCAL_TIMER_IRQ,
            timer_tick_handler,
        ).map_err(|_handler| {
            log::error!("MKS: timer IRQ {} already registered to {:#X}",
                CPU_LOCAL_TIMER_IRQ, _handler);
            "MKS: CPU-local timer IRQ already registered"
        })?;
    }

    #[cfg(target_arch = "aarch64")]
    {
        interrupts::setup_timer_interrupt(timer_tick_handler)?;
        generic_timer_aarch64::enable_timer_interrupt(true);
    }

    Ok(())
}

/// Phase-2 expansion: grow MKS to `num_cpus` with a real topology.
///
/// Called from `captain` after all APs are online.
/// Extends the existing single-CPU MKS instance with new per-CPU run queues.
pub fn expand_to_all_cpus(num_cpus: usize, topology: CpuTopology) -> Result<(), &'static str> {
    mks::expand(num_cpus, topology);
    log::info!("MKS: expanded to {} CPUs with real topology", num_cpus);
    Ok(())
}

/// Simplified init: single CPU, uniform topology. For early boot / QEMU.
pub fn init_single_cpu() -> Result<(), &'static str> {
    init(1, CpuTopology::uniform(1))
}

// ---------------------------------------------------------------------------
// TSC-based elapsed time measurement (x86_64)
// ---------------------------------------------------------------------------

/// TSC value at the last timer tick, per CPU.
///
/// Each CPU maintains its own TSC snapshot so that `elapsed_ns_tsc()` computes
/// the correct delta for *this* CPU's timer interval.  Without per-CPU storage
/// multiple CPUs would swap the same global, producing near-zero deltas on all
/// CPUs except the one that happened to swap last — corrupting vruntime
/// accounting and causing scheduling chaos under SMP.
#[cls::cpu_local]
static LAST_TICK_TSC: u64 = 0;

/// Nanoseconds per TSC tick, calibrated at boot (approximate default: 1/3GHz).
/// Updated by `calibrate_tsc()` if called.
static TSC_NS_PER_TICK_RECIP: AtomicU64 = AtomicU64::new(3); // divide by 3 ≈ 3GHz

/// Read the current TSC.
#[inline(always)]
fn rdtsc() -> u64 {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        let lo: u32; let hi: u32;
        core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi);
        ((hi as u64) << 32) | (lo as u64)
    }
    #[cfg(not(target_arch = "x86_64"))]
    { 0u64 }
}

/// Compute elapsed nanoseconds since the last tick using TSC.
///
/// Uses per-CPU `LAST_TICK_TSC` so each CPU computes its own delta correctly.
/// Called from the timer interrupt handler where preemption is already disabled.
fn elapsed_ns_tsc() -> u64 {
    let now = rdtsc();
    let prev = LAST_TICK_TSC.load();
    LAST_TICK_TSC.set(now);
    if prev == 0 { return 1_000_000; } // first tick: assume 1ms
    let delta_tsc = now.saturating_sub(prev);
    // ns = tsc_delta / (tsc_freq_ghz) ≈ delta / 3
    delta_tsc / TSC_NS_PER_TICK_RECIP.load(Ordering::Relaxed).max(1)
}

/// Calibrate TSC frequency against the PIT or HPET.
/// Call from `captain` after the PIT/HPET is initialized.
///
/// `tsc_delta_per_ms`: TSC ticks measured during one millisecond.
pub fn calibrate_tsc(tsc_delta_per_ms: u64) {
    if tsc_delta_per_ms > 0 {
        // recip = tsc_delta / 1_000_000 (to get TSC ticks per ns).
        // We store the divisor: ns = tsc_delta / recip.
        let recip = tsc_delta_per_ms / 1_000;
        TSC_NS_PER_TICK_RECIP.store(recip.max(1), Ordering::Relaxed);
        log::info!("MKS: TSC calibrated: {} TSC ticks/ns (≈ {} MHz)",
            recip, recip * 1000);
    }
}

// ---------------------------------------------------------------------------
// Timer interrupt handler
// ---------------------------------------------------------------------------

interrupt_handler!(timer_tick_handler, _, _stack_frame, {
    #[cfg(target_arch = "aarch64")]
    generic_timer_aarch64::set_next_timer_interrupt(get_timeslice_ticks());

    // Measure elapsed time since last tick.
    let elapsed_ns = {
        #[cfg(target_arch = "x86_64")]
        { elapsed_ns_tsc() }
        #[cfg(target_arch = "aarch64")]
        { get_timeslice_period_ns() }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        { 1_000_000u64 }
    };

    // Determine which CPU we are on.
    let cpu_id = cpu::current_cpu().value() as usize;

    // Tick MKS: updates vruntime, checks deadlines, sets need_resched.
    mks::get().tick(cpu_id, elapsed_ns);

    // Unblock any sleeping tasks whose timers have expired.
    sleep::unblock_sleeping_tasks();

    // Pump audio hardware on CPU 0 only (avoids redundant work on other CPUs).
    // This is lightweight: try_lock + check mixer + maybe refill DMA.
    if cpu_id == 0 {
        audio_mixer::pump_hardware();
    }

    // Acknowledge the interrupt BEFORE the potential context switch.
    // (If we switch tasks here we must never return to the IRQ handler.)
    eoi(CPU_LOCAL_TIMER_IRQ);

    // Perform preemptive context switch if needed.
    // rqs is behind IrqSafeRwLock; IRQs are masked while the lock is held.
    let need_resched = {
        let rqs = mks::get().rqs.read();
        rqs.get(cpu_id)
            .map(|rq| rq.need_resched.load(core::sync::atomic::Ordering::Acquire))
            .unwrap_or(false)
    };

    if need_resched {
        schedule();
    }

    EoiBehaviour::HandlerSentEoi
});

// ---------------------------------------------------------------------------
// AArch64 timer helpers
// ---------------------------------------------------------------------------

#[cfg(target_arch = "aarch64")]
fn get_timeslice_ticks() -> u64 {
    use kernel_config::time::CONFIG_TIMESLICE_PERIOD_MICROSECONDS;
    static CACHE: spin::Once<u64> = spin::Once::new();
    *CACHE.call_once(|| {
        let femtosecs = (CONFIG_TIMESLICE_PERIOD_MICROSECONDS as u64) * 1_000_000_000;
        let period_fs = generic_timer_aarch64::timer_period_femtoseconds();
        femtosecs / period_fs
    })
}

#[cfg(target_arch = "aarch64")]
fn get_timeslice_period_ns() -> u64 {
    use kernel_config::time::CONFIG_TIMESLICE_PERIOD_MICROSECONDS;
    (CONFIG_TIMESLICE_PERIOD_MICROSECONDS as u64) * 1_000
}

// ---------------------------------------------------------------------------
// Public API extensions (MKS-specific, not in original Theseus scheduler)
// ---------------------------------------------------------------------------

/// Dump scheduler statistics for all CPUs to the kernel log.
pub fn dump_stats() {
    let sched = mks::get();
    let nc = sched.num_cpus.load(Ordering::Relaxed);
    let rqs = sched.rqs.read();
    for cpu_id in 0..nc {
        if let Some(rq) = rqs.get(cpu_id) {
            let stats = rq.stats.lock();
            log::info!(
                "MKS CPU{}: switches={}, avg_latency={}µs, max_latency={}µs, \
                 stolen_in={}, stolen_out={}, dl_misses={}",
                cpu_id,
                stats.context_switches,
                stats.avg_wakeup_latency_ns / 1_000,
                stats.max_wakeup_latency_ns / 1_000,
                stats.tasks_stolen_in,
                stats.tasks_stolen_out,
                stats.deadline_misses,
            );
        }
    }
    log::info!(
        "MKS global: total_switches={}, tasks={}",
        sched.context_switches.load(Ordering::Relaxed),
        sched.global_task_count.load(Ordering::Relaxed),
    );
}
