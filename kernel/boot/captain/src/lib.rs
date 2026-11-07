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
extern crate disk_mount;
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

#[cfg(all(mirror_log_to_vga, target_arch = "x86_64"))]
mod mirror_log_callbacks {
    pub(crate) fn mirror_to_early_vga(args: core::fmt::Arguments) {
        early_printer::println!("{}", args);
    }
    pub(crate) fn mirror_to_terminal(args: core::fmt::Arguments) {
        app_io::println!("{}", args);
    }
}

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
#[cfg(target_arch = "x86_64")]
pub use multicore_bringup::LimineAp;

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

/// Nanoseconds per TSC tick, computed from `tsc::get_tsc_period()`.
/// Used to calibrate MKS's elapsed-time measurement.
#[cfg(target_arch = "x86_64")]
#[allow(dead_code)]
fn tsc_delta_per_ms() -> Option<u64> {
    let period_fs: u64 = tsc::get_tsc_period()?.into();
    if period_fs == 0 {
        return None;
    }
    Some(1_000_000_000_000u64 / period_fs)
}

/// Build the MaiSched CPU topology from APIC / ACPI data.
///
/// We query ACPI for the MADT to find core / package relationships.
/// Falls back to a uniform topology if ACPI data is unavailable.
#[cfg(target_arch = "x86_64")]
fn discover_cpu_topology(cpu_count: usize) -> maisched::topology::CpuTopology {
    use maisched::topology::{CpuTopology, CpuInfo};

    let entries: Vec<CpuInfo> = (0..cpu_count)
        .map(|logical_id| {
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
            "MaiSched: topology discovered — {} CPUs, {} unique cores",
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
fn discover_cpu_topology(cpu_count: usize) -> maisched::topology::CpuTopology {
    maisched::topology::CpuTopology::uniform(cpu_count)
}

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

    #[cfg(all(mirror_log_to_vga, target_arch = "x86_64"))] {
        logger::set_log_mirror_function(mirror_log_callbacks::mirror_to_early_vga);
    }

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

    #[cfg(target_arch = "x86_64")]
    device_manager::early_init(rsdp_address, kernel_mmi_ref.lock().deref_mut())?;

    interrupt_controller::init(&kernel_mmi_ref)?;

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

    let bsp_id = cpu::bootstrap_cpu()
        .ok_or("captain: couldn't get bootstrap CPU ID")?;
    cls_allocator::reload_current_cpu();

    scheduler::init_single_cpu()
        .map_err(|e| { error!("MKS phase-1 init failed: {}", e); e })?;
    warn!("MKS phase-1: single-CPU EEVDF scheduler active on BSP (CPU {})", bsp_id);

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

    #[cfg(target_arch = "x86_64")]
    exceptions_full::init(idt);

    // MaiProc : IDT des talons d'entrée (mode utilisateur) à la place de l'IDT historique ;
    // les cœurs secondaires la chargent en démarrant.
    #[cfg(target_arch = "x86_64")]
    interrupts::install_entry_stubs();

    #[cfg(target_arch = "x86_64")]
    let total_cpus_expected = {
        let acpi_tables = acpi::get_acpi_tables().lock();
        let total = match madt::Madt::get(&acpi_tables) {
            Some(madt) => {
                let mut count: u32 = 0;
                for entry in madt.iter() {
                    let flags = match entry {
                        madt::MadtEntry::LocalApic(e) => e.flags,
                        madt::MadtEntry::LocalX2Apic(e) => e.flags,
                        _ => continue,
                    };
                    if flags & 0x1 == 0x1 {
                        count += 1;
                    }
                }
                count
            }
            None => {
                warn!("MADT not found — assuming single CPU");
                1
            }
        };
        total as usize
    };
    #[cfg(not(target_arch = "x86_64"))]
    let total_cpus_expected: usize = 1;

    {
        let topology = discover_cpu_topology(total_cpus_expected);
        scheduler::expand_to_all_cpus(total_cpus_expected, topology)
            .map_err(|e| { error!("MKS phase-2 pre-expand failed: {}", e); e })?;
        info!(
            "MKS phase-2: pre-expanded to {} CPUs (from MADT), work-stealing active",
            total_cpus_expected
        );
    }

    let ap_count = multicore_bringup::handle_ap_cores(
        &kernel_mmi_ref,
        multicore_info,
    )?;
    let cpu_count = ap_count + 1;
    warn!("All {} APs online — {} total CPUs", ap_count, cpu_count);

    {
        #[cfg(target_arch = "x86_64")]
        if let Some(ticks_per_ms) = tsc_ticks_per_ms {
            scheduler::calibrate_tsc(ticks_per_ms);
            warn!("MaiSched: TSC calibration applied ({} ticks/ms)", ticks_per_ms);
        } else {
            warn!("MaiSched: TSC not calibrated — scheduler clock uses raw cycles");
        }
    }

    #[cfg(all(mirror_log_to_vga, target_arch = "x86_64"))] {
        logger::set_log_mirror_function(mirror_log_callbacks::mirror_to_terminal);
    }

    tlb_shootdown::init();
    warn!("TLB shootdown initialized");

    #[cfg(target_arch = "x86_64")] {
        heap::enable_kalloc(cpu_count as usize)?;
        warn!("MaiMM : kalloc actif ({} tas par coeur, segments de 2 Mio)", cpu_count);
    }

    #[cfg(target_arch = "x86_64")]
    if page_attribute_table::init().is_err() {
        error!("PAT not supported on this CPU — write-combining disabled for MGI");
    } else {
        warn!("PAT initialized — write-combining enabled for MGI framebuffer");
    }

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

    task_fs::init()?;
    warn!("Task filesystem initialized");

    warn!("Initializing swap...");
    init_swap();

    {
        let mounted = disk_mount::init();
        if mounted > 0 {
            disk_mount::create_system_dirs();
            disk_mount::install_system_files();
            info!("Persistent filesystem: {} volume(s) mounted and installed", mounted);
        }
    }

    #[cfg(target_arch = "x86_64")]
    match syscall::init() {
        Ok(()) => warn!("MEB syscall subsystem initialized (Linux + Windows NT + MaiOS)"),
        Err(e) => error!("MEB syscall init failed: {} — userspace binaries will not run", e),
    }


    #[cfg(feature = "mks_stats_daemon")]
    spawn_mks_stats_daemon()?;

    console::start_connection_detection()?;

    first_application::start()?;

    warn!(
        "captain::init(): initialization done! \
         BSP CPU {} going idle. Enabling interrupts...",
        bsp_id
    );

    drop(kernel_mmi_ref);

    drop_after_init.drop_all();

    spawn::cleanup_bootstrap_tasks(cpu_count)?;
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    for _ in 0..100_000 { core::hint::spin_loop(); }
    bootstrap_task.finish();
    enable_interrupts();
    scheduler::schedule();

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

/// Spawn a low-priority periodic task that logs MKS statistics.
/// Only compiled when the `mks_stats_daemon` feature is enabled.
#[cfg(feature = "mks_stats_daemon")]
fn spawn_mks_stats_daemon() -> Result<(), &'static str> {
    use core::sync::atomic::{AtomicBool, Ordering};

    warn!("MKS: spawning stats daemon (SCHED_BATCH, nice +19)");

    let task = spawn::new_task_builder(move |_: ()| -> isize {
        loop {
            sleep::sleep(core::time::Duration::from_secs(5));
            scheduler::dump_stats();
        }
    }, ())
    .name(alloc::string::String::from("mks_stats_daemon"))
    .spawn()?;

    scheduler::set_task_policy(&task, scheduler::SchedClass::Batch, 0);
    task::scheduler::set_priority(&task, 39);

    warn!("MaiSched stats daemon spawned (task id={})", task.id);
    Ok(())
}