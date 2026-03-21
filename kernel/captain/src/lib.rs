//! The main initialization routine and setup logic of MaiOS.
//!
//! The `captain` steers the ship — it initializes all subsystems in the
//! correct order, wires data between them, and hands off to the first
//! application.
//!
//! ## Initialization order (MaiOS)
//!
//!  1. TSC calibration (must be early, before interrupts)
//!  2. ACPI / device_manager early init
//!  3. Interrupt controller (APIC/GIC)
//!  4. IDT / exception handlers (x86_64)
//!  5. CPU registration
//!  6. **MKS phase-1**: single-CPU init (scheduler must exist before first task)
//!  7. Bootstrap task creation (spawn::init)
//!  8. Full exception handlers
//!  9. AP (secondary CPU) bringup
//! 10. **MKS phase-2**: expand to all CPUs + topology + TSC calibration
//! 11. TLB shootdown, per-core heaps, PAT
//! 12. Window manager / device drivers
//! 13. Syscall subsystem (MEB — Linux + Windows ABI)
//! 14. Swap init
//! 15. First application

#![no_std]

extern crate memory_swap;
extern crate alloc;
extern crate mod_mgmt;
extern crate environment;
extern crate storage_manager;
#[cfg(target_arch = "x86_64")]
extern crate syscall;

use log::{error, info, warn};
use memory::{EarlyIdentityMappedPages, MmiRef, PhysicalAddress};
use irq_safety::enable_interrupts;
use stack::Stack;
use no_drop::NoDrop;
use alloc::vec::Vec;

#[cfg(target_arch = "x86_64")]
use {
    core::ops::DerefMut,
    kernel_config::memory::KERNEL_STACK_SIZE_IN_PAGES,
};

// ---------------------------------------------------------------------------
// Log mirroring callbacks
// ---------------------------------------------------------------------------

#[cfg(all(mirror_log_to_vga, target_arch = "x86_64"))]
mod mirror_log_callbacks {
    pub(crate) fn mirror_to_early_vga(args: core::fmt::Arguments) {
        early_printer::println!("{}", args);
    }
    pub(crate) fn mirror_to_terminal(args: core::fmt::Arguments) {
        app_io::println!("{}", args);
    }
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Items that must be held alive through init and dropped at the end.
pub struct DropAfterInit {
    pub identity_mappings: NoDrop<EarlyIdentityMappedPages>,
}
impl DropAfterInit {
    fn drop_all(self) {
        drop(self.identity_mappings.into_inner());
    }
}

pub use multicore_bringup::MulticoreBringupInfo;

// ---------------------------------------------------------------------------
// Swap init
// ---------------------------------------------------------------------------

fn init_swap() {
    if let Some(device) = storage_manager::storage_devices().next() {
        let swap_mb = {
            let locked = device.lock();
            (locked.size_in_blocks() / 2048).saturating_sub(1)
        };
        memory_swap::init(device, swap_mb);
        warn!("Swap initialized: {} MB", swap_mb);
    } else {
        warn!("Swap: no storage device found, running without swap");
    }
}

// ---------------------------------------------------------------------------
// TSC calibration helpers
// ---------------------------------------------------------------------------

/// Nanoseconds per TSC tick, computed from `tsc::get_tsc_period()`.
/// Used to calibrate MKS's elapsed-time measurement.
#[cfg(target_arch = "x86_64")]
#[allow(dead_code)]
fn tsc_delta_per_ms() -> Option<u64> {
    // tsc::get_tsc_period() returns a Period (femtoseconds per TSC tick).
    // tsc_ticks_per_ms = 1_000_000_000_000 / period_fs (1ms = 10^12 fs).
    let period_fs: u64 = tsc::get_tsc_period()?.into();
    if period_fs == 0 {
        return None;
    }
    // tsc ticks per millisecond = 1_000_000_000_000 fs/ms / period_fs
    Some(1_000_000_000_000u64 / period_fs)
}

// ---------------------------------------------------------------------------
// CPU topology discovery
// ---------------------------------------------------------------------------

/// Build the MKS CPU topology from APIC / ACPI data.
///
/// We query ACPI for the MADT to find core / package relationships.
/// Falls back to a uniform topology if ACPI data is unavailable.
#[cfg(target_arch = "x86_64")]
fn discover_cpu_topology(cpu_count: usize) -> mks::topology::CpuTopology {
    use mks::topology::{CpuTopology, CpuInfo};

    // Attempt to build topology from ACPI MADT processor info.
    // `device_manager` exposes an iterator of CpuTopologyEntry if ACPI init
    // succeeded. If not, we fall back to a uniform single-socket layout.
    let entries: Vec<CpuInfo> = (0..cpu_count)
        .map(|logical_id| {
            // Try to get real topology from ACPI.
            #[cfg(feature = "acpi_topology")]
            if let Some(info) = device_manager::cpu_topology_entry(logical_id) {
                return CpuInfo {
                    logical_id,
                    core_id: info.core_id,
                    package_id: info.package_id,
                    numa_node: info.numa_node,
                    l3_group: info.l3_cache_id,
                };
            }
            // Fallback: assume HT pairs (even/odd = same core), single socket.
            CpuInfo {
                logical_id,
                core_id: (logical_id / 2) as u32,
                package_id: 0,
                numa_node: 0,
                l3_group: 0,
            }
        })
        .collect();

    if entries.len() > 1 {
        warn!(
            "MKS: topology discovered — {} CPUs, {} unique cores",
            entries.len(),
            {
                let mut cores: Vec<u32> = entries.iter().map(|e| e.core_id).collect();
                cores.dedup();
                cores.len()
            }
        );
    }

    CpuTopology::from_cpus(entries)
}

#[cfg(target_arch = "aarch64")]
fn discover_cpu_topology(cpu_count: usize) -> mks::topology::CpuTopology {
    mks::topology::CpuTopology::uniform(cpu_count)
}

// ---------------------------------------------------------------------------
// Main init
// ---------------------------------------------------------------------------

/// Initialize MaiOS. Called from `nano_core` after memory setup.
///
/// # Arguments
/// * `kernel_mmi_ref`   — kernel memory management info (page table, heap).
/// * `bsp_initial_stack`— the stack currently in use; must not be dropped
///                         during init.
/// * `drop_after_init`  — identity-mapped pages; dropped at end of init.
/// * `multicore_info`   — data needed to wake secondary CPUs.
/// * `rsdp_address`     — RSDP pointer from bootloader (x86_64).
#[cfg_attr(target_arch = "aarch64", allow(unreachable_code, unused_variables))]
pub fn init(
    kernel_mmi_ref: MmiRef,
    bsp_initial_stack: NoDrop<Stack>,
    drop_after_init: DropAfterInit,
    multicore_info: MulticoreBringupInfo,
    rsdp_address: Option<PhysicalAddress>,
) -> Result<(), &'static str> {

    // =========================================================================
    // Step 1 — Early log mirroring to VGA (real hardware debugging)
    // =========================================================================
    #[cfg(all(mirror_log_to_vga, target_arch = "x86_64"))] {
        logger::set_log_mirror_function(mirror_log_callbacks::mirror_to_early_vga);
    }

    // =========================================================================
    // Step 2 — TSC calibration (x86_64)
    //
    // Must happen early, before interrupts, to get the most accurate reading.
    // The result feeds both the `time` crate and MKS's elapsed-ns computation.
    // =========================================================================
    #[cfg(target_arch = "x86_64")]
    let tsc_ticks_per_ms: Option<u64> = {
        if let Some(period) = tsc::get_tsc_period() {
            time::register_clock_source::<tsc::Tsc>(period);
            let ticks_per_ms = 1_000_000_000_000u64.checked_div(period.into());
            if let Some(t) = ticks_per_ms {
                warn!("TSC calibrated: {} ticks/ms (~{} MHz)", t, t / 1_000);
            } else {
                warn!("TSC period is zero — skipping TSC clock registration");
            }
            ticks_per_ms
        } else {
            warn!("TSC: could not determine TSC period");
            None
        }
    };

    // =========================================================================
    // Step 3 — ACPI / early device manager (x86_64)
    //
    // Discovers CPU count, NUMA topology, IOMMU, HPET, etc.
    // Must happen before interrupt_controller::init because APIC addresses
    // come from ACPI MADT.
    // =========================================================================
    #[cfg(target_arch = "x86_64")]
    device_manager::early_init(rsdp_address, kernel_mmi_ref.lock().deref_mut())?;

    // =========================================================================
    // Step 4 — Interrupt controller (APIC on x86_64, GIC on aarch64)
    // =========================================================================
    interrupt_controller::init(&kernel_mmi_ref)?;

    // =========================================================================
    // Step 5 — IDT + early exception handlers (x86_64)
    //
    // We set up the IDT with two stacks:
    //   - double_fault_stack: separate stack so a stack overflow doesn't
    //     silently triple-fault.
    //   - privilege_stack: used on privilege level changes (ring 3 → 0).
    // =========================================================================
    #[cfg(target_arch = "x86_64")]
    let idt = {
        let (double_fault_stack, privilege_stack) = {
            let mut kernel_mmi = kernel_mmi_ref.lock();
            (
                stack::alloc_stack(
                    KERNEL_STACK_SIZE_IN_PAGES,
                    &mut kernel_mmi.page_table,
                ).ok_or("captain: could not allocate double fault stack")?,
                stack::alloc_stack(
                    1,
                    &mut kernel_mmi.page_table,
                ).ok_or("captain: could not allocate privilege stack")?,
            )
        };
        interrupts::init(
            double_fault_stack.top_unusable(),
            privilege_stack.top_unusable(),
        )?
    };

    #[cfg(target_arch = "aarch64")] {
        interrupts::init()?;
        irq_safety::enable_fast_interrupts();
        cpu::register_cpu(true)?;
    }

    // =========================================================================
    // Step 6 — CPU identification
    // =========================================================================
    let bsp_id = cpu::bootstrap_cpu()
        .ok_or("captain: couldn't get bootstrap CPU ID")?;
    cls_allocator::reload_current_cpu();

    // =========================================================================
    // Step 7 — MKS phase-1: single-CPU init
    //
    // We must have *some* scheduler active before spawn::init, because
    // spawn creates the bootstrap task and immediately needs to be able
    // to context-switch to/from it.
    //
    // We start with a single-CPU scheduler here; after AP bringup we call
    // mks_expand() to register all secondary CPUs.
    // =========================================================================
    scheduler::init_single_cpu()
        .map_err(|e| { error!("MKS phase-1 init failed: {}", e); e })?;
    warn!("MKS phase-1: single-CPU EEVDF scheduler active on BSP (CPU {})", bsp_id);

    // =========================================================================
    // Step 8 — Kernel namespace + bootstrap task
    //
    // spawn::init creates the initial Task from the current execution context
    // (the bootstrap stack passed in from nano_core).
    // =========================================================================
    let kernel_namespace = mod_mgmt::get_initial_kernel_namespace()
        .ok_or("captain: couldn't get initial kernel namespace")?
        .clone();
    let kernel_env = environment::get_default_environment()
        .ok_or("captain: couldn't get default kernel environment")?;

    let bootstrap_task = spawn::init(
        kernel_mmi_ref.clone(),
        bsp_id,
        bsp_initial_stack,
        kernel_namespace,
        kernel_env,
    )?;
    warn!("Bootstrap task created: {:?}", bootstrap_task);

    // =========================================================================
    // Step 9 — Full exception handlers (x86_64)
    //
    // Now that we have a task subsystem, we can install richer handlers
    // (e.g., page fault handler that knows about tasks and can kill the
    // faulting task instead of always halting).
    // =========================================================================
    #[cfg(target_arch = "x86_64")]
    exceptions_full::init(idt);

    // =========================================================================
    // Step 10 — AP (secondary CPU) bringup
    //
    // Wakes all secondary CPUs, gives each one its own bootstrap stack,
    // and waits until every AP has checked in with the scheduler.
    // =========================================================================
    let ap_count = multicore_bringup::handle_ap_cores(
        &kernel_mmi_ref,
        multicore_info,
    )?;
    let cpu_count = ap_count + 1;
    warn!("All {} APs online — {} total CPUs", ap_count, cpu_count);

    // =========================================================================
    // Step 11 — MKS phase-2: expand to all CPUs + calibrate TSC
    //
    // Now that we know the final CPU count and have ACPI topology data,
    // we reinitialize MKS with:
    //   a) The full per-CPU run queues (one per logical CPU).
    //   b) The real CPU topology (for cache-aware work stealing).
    //   c) Calibrated TSC for accurate elapsed-time measurement.
    //
    // This call is idempotent: it reuses any tasks already enqueued on the
    // BSP's run queue (the bootstrap task) by migrating them into the new
    // multi-CPU scheduler.
    // =========================================================================
    {
        let topology = discover_cpu_topology(cpu_count as usize);
        scheduler::expand_to_all_cpus(cpu_count as usize, topology)
            .map_err(|e| { error!("MKS phase-2 expand failed: {}", e); e })?;
        warn!(
            "MKS phase-2: expanded to {} CPUs, work-stealing active",
            cpu_count
        );

        // Calibrate TSC → ns conversion for tick accounting.
        #[cfg(target_arch = "x86_64")]
        if let Some(ticks_per_ms) = tsc_ticks_per_ms {
            scheduler::calibrate_tsc(ticks_per_ms);
            warn!("MKS: TSC calibration applied ({} ticks/ms)", ticks_per_ms);
        } else {
            warn!("MKS: TSC not calibrated — using approximate 3 GHz default");
        }
    }

    // =========================================================================
    // Step 12 — Framebuffer / log mirror switch (x86_64)
    //
    // After AP bringup the graphics mode switches from text-VGA to the
    // graphical framebuffer. Mirror log output to the new terminal.
    // =========================================================================
    #[cfg(all(mirror_log_to_vga, target_arch = "x86_64"))] {
        logger::set_log_mirror_function(mirror_log_callbacks::mirror_to_terminal);
    }

    // =========================================================================
    // Step 13 — TLB shootdown
    //
    // Requires Local APICs on all CPUs to be running (step 10 must be done).
    // TLB shootdowns are needed whenever we modify page tables in shared
    // address spaces.
    // =========================================================================
    tlb_shootdown::init();
    warn!("TLB shootdown initialized");

    // =========================================================================
    // Step 14 — Per-core heaps (x86_64)
    //
    // Replaces the single global heap with per-CPU heaps.
    // Reduces contention on alloc/free dramatically on multicore systems.
    // Based on: TCMalloc / jemalloc per-thread caches.
    // =========================================================================
    #[cfg(target_arch = "x86_64")] {
        multiple_heaps::switch_to_multiple_heaps()?;
        warn!("Per-core heaps initialized ({} heaps)", cpu_count);
    }

    // =========================================================================
    // Step 15 — Page Attribute Table (x86_64)
    //
    // Enables write-combining for graphics memory (MGI framebuffer).
    // Must be called on every CPU but only needs to succeed on at least one.
    // =========================================================================
    #[cfg(target_arch = "x86_64")]
    if page_attribute_table::init().is_err() {
        error!("PAT not supported on this CPU — write-combining disabled for MGI");
    } else {
        warn!("PAT initialized — write-combining enabled for MGI framebuffer");
    }

    // =========================================================================
    // Step 16 — Window manager + input devices (x86_64)
    //
    // Initializes MGI (Mai Graphics Infrastructure) compositor and the
    // keyboard/mouse input subsystem.
    // =========================================================================
    #[cfg(target_arch = "x86_64")]
    match window_manager::init() {
        Ok((key_producer, mouse_producer)) => {
            device_manager::init(key_producer, mouse_producer)?;
            warn!("MGI window manager and input devices initialized");
        }
        Err(e) => {
            error!("Window manager init failed (expected in --nographic): {}", e);
        }
    }

    #[cfg(target_arch = "aarch64")]
    device_manager::init()?;

    // =========================================================================
    // Step 17 — Task filesystem (/proc-like virtual filesystem)
    //
    // Makes task / scheduler info accessible as virtual files.
    // e.g., /task/<id>/state, /task/<id>/sched_policy, etc.
    // =========================================================================
    task_fs::init()?;
    warn!("Task filesystem initialized");

    // =========================================================================
    // Step 18 — SIMD personality (conditional compile feature)
    // =========================================================================
    #[cfg(simd_personality)] {
        #[cfg(simd_personality_sse)]
        let simd_ext = task::SimdExt::SSE;
        #[cfg(simd_personality_avx)]
        let simd_ext = task::SimdExt::AVX;
        warn!("SIMD personality enabled ({:?})", simd_ext);
        spawn::new_task_builder(simd_personality::setup_simd_personality, simd_ext)
            .name(alloc::format!("setup_simd_personality_{:?}", simd_ext))
            .spawn()?;
    }

    // =========================================================================
    // Step 19 — Swap
    //
    // Initializes the swap subsystem using the first available block device.
    // Swap allows the memory manager to evict cold pages to disk when RAM
    // is under pressure (part of MVA — Mai Virtual Allocator).
    // =========================================================================
    warn!("Initializing swap...");
    init_swap();

    // =========================================================================
    // Step 20 — Syscall subsystem (MEB — Mai Execution Bridge)
    //
    // Enables SYSCALL/SYSRET on x86_64 via the LSTAR/STAR/SFMASK MSRs.
    // The syscall handler in the `syscall` crate dispatches to the correct
    // personality (Linux, Windows NT, or MaiOS native) based on the task's
    // `abi` field set at ELF/PE load time.
    // =========================================================================
    #[cfg(target_arch = "x86_64")]
    match syscall::init() {
        Ok(()) => warn!("MEB syscall subsystem initialized (Linux + Windows NT + MaiOS)"),
        Err(e) => error!("MEB syscall init failed: {} — userspace binaries will not run", e),
    }

    // =========================================================================
    // Step 21 — MKS scheduler stats daemon
    //
    // Spawns a low-priority background task that periodically logs
    // per-CPU scheduler statistics (context switches, wakeup latency, etc.).
    // Uses the SCHED_BATCH class so it never interferes with real work.
    // =========================================================================
    #[cfg(feature = "mks_stats_daemon")]
    spawn_mks_stats_daemon()?;

    // =========================================================================
    // Step 22 — Console connection detection
    // =========================================================================
    console::start_connection_detection()?;

    // =========================================================================
    // Step 23 — First application
    //
    // Spawns the first userspace application (shell, desktop, etc.)
    // as configured in `first_application`.
    // =========================================================================
    first_application::start()?;

    // =========================================================================
    // Finalization — drop locals, kill bootstrap task, enable interrupts
    //
    // ORDER MATTERS. See comments on each step.
    // =========================================================================
    warn!(
        "captain::init(): initialization done! \
         BSP CPU {} going idle. Enabling interrupts...",
        bsp_id
    );

    // 1. Drop kernel_mmi_ref — we no longer need exclusive access.
    drop(kernel_mmi_ref);

    // 2. Drop identity-mapped pages — APs are done booting.
    drop_after_init.drop_all();

    // 3. Mark all bootstrap tasks (BSP + APs) as finished.
    // spawn::cleanup_bootstrap_tasks(cpu_count)?;

    // // 4. Mark THIS bootstrap task as finished.
    // bootstrap_task.finish();

    // // 5. Enable interrupts — from this point, other tasks can be scheduled.
    // enable_interrupts();

    // // ****************************************************
    // // NOTE: nothing below here is guaranteed to run again!
    // // ****************************************************

    // // Yield to the scheduler. The bootstrap task is dead; it will not be
    // // rescheduled. The idle task for this CPU takes over.
    // scheduler::schedule();

    spawn::cleanup_bootstrap_tasks(cpu_count)?;
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    for _ in 0..100_000 { core::hint::spin_loop(); }
    bootstrap_task.finish();
    enable_interrupts();
    scheduler::schedule();

    // Should never reach here.
    loop {
        error!(
            "BUG: captain::init(): bootstrap task rescheduled after death! \
             CPU {} halting.",
            bsp_id
        );
        #[cfg(target_arch = "x86_64")]
        unsafe { core::arch::asm!("hlt") };
    }
}

// ---------------------------------------------------------------------------
// MKS stats daemon (optional feature)
// ---------------------------------------------------------------------------

/// Spawn a low-priority periodic task that logs MKS statistics.
/// Only compiled when the `mks_stats_daemon` feature is enabled.
#[cfg(feature = "mks_stats_daemon")]
fn spawn_mks_stats_daemon() -> Result<(), &'static str> {
    use core::sync::atomic::{AtomicBool, Ordering};

    warn!("MKS: spawning stats daemon (SCHED_BATCH, nice +19)");

    let task = spawn::new_task_builder(move |_: ()| -> isize {
        loop {
            // Sleep for 5 seconds between dumps.
            sleep::sleep(core::time::Duration::from_secs(5));
            scheduler::dump_stats();
        }
    }, ())
    .name(alloc::string::String::from("mks_stats_daemon"))
    .spawn()?;

    // Assign batch scheduling: runs only when nothing else is runnable.
    {
        use task_struct::SchedClass;
        task.write().sched.policy = SchedClass::Batch;
        task.write().sched.nice = 19;
        task.write().sched.update_weight();
    }

    warn!("MKS stats daemon spawned (task id={})", task.read().id);
    Ok(())
}