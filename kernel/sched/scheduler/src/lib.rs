//! Point d'entrée de l'ordonnanceur MaiSched pour le reste du noyau.
//!
//! * Branche l'interruption du timer local sur `task::scheduler::scheduler_tick()` (imputation,
//!   fin de tranche, tourniquet temps réel, tirage de charge) puis replanifie en sortie
//!   d'interruption si besoin.
//! * Alloue le vecteur de l'**IPI de replanification** : un réveil qui doit préempter la tâche d'un
//!   autre cœur (ou sortir un cœur de `hlt`) le lui envoie, au lieu d'attendre son prochain tick.
//! * Étalonne l'horloge de l'ordonnanceur (TSC) et installe la topologie des caches.
//!
//! Les algorithmes sont dans `maisched`, la concurrence entre cœurs dans `task::scheduler`.
//! L'API historique de Theseus (`schedule`, `set_priority`, `priority`, `inherit_priority`) est
//! conservée.

#![no_std]
#![cfg_attr(target_arch = "x86_64", feature(abi_x86_interrupt))]

extern crate alloc;

#[cfg(target_arch = "x86_64")]
use core::convert::TryFrom;
use core::sync::atomic::{AtomicU8, Ordering};
use interrupts::{self, CPU_LOCAL_TIMER_IRQ, interrupt_handler, eoi, EoiBehaviour};
use maisched::topology::CpuTopology;

pub use task::scheduler::{schedule, schedule_preempt, stats, SchedStats};
pub use task_struct::SchedClass;

// ---------------------------------------------------------------------------
// API historique (tâche courante)
// ---------------------------------------------------------------------------

/// Fixe le nice [-20, 19] de la tâche courante.
pub fn set_priority(nice: i8) {
    let _ = task::with_current_task(|t| task::scheduler::set_priority(t, (nice.clamp(-20, 19) + 20) as u8));
}

/// Nice de la tâche courante.
pub fn priority() -> i8 {
    task::with_current_task(|t| t.sched.nice.load(Ordering::Relaxed)).unwrap_or(0)
}

/// La tâche `child` hérite du nice et de la classe de `parent`.
pub fn inherit_priority(child: &task::TaskRef, parent: &task::TaskRef) {
    if let Some(p) = task::scheduler::priority(parent) {
        task::scheduler::set_priority(child, p);
    }
    let class = parent.sched.snapshot().class;
    if !matches!(class, maisched::Class::Idle) {
        task::scheduler::set_class(child, class);
    }
}

/// Change la politique d'une tâche (temps partagé, fond, temps réel) ; `rt_prio` sert aux classes
/// temps réel (1..99).
pub fn set_task_policy(task: &task::TaskRef, policy: SchedClass, rt_prio: u8) {
    task::scheduler::set_class(task, policy.to_class(rt_prio));
}

// ---------------------------------------------------------------------------
// Initialisation
// ---------------------------------------------------------------------------

/// Vecteur de l'IPI de replanification (0 : pas encore alloué).
static RESCHED_VECTOR: AtomicU8 = AtomicU8::new(0);

/// Initialise MaiSched et enregistre ses interruptions. À appeler une fois au démarrage.
pub fn init(num_cpus: usize, topology: CpuTopology) -> Result<(), &'static str> {
    task::scheduler::set_cpu_count(num_cpus);
    task::scheduler::set_topology(topology);
    preemption::set_preempt_hook(task::scheduler::preemption_point);

    #[cfg(target_arch = "x86_64")]
    {
        interrupts::register_interrupt(CPU_LOCAL_TIMER_IRQ, timer_tick_handler).map_err(|_handler| {
            log::error!("MaiSched: timer IRQ {} already registered to {:#X}", CPU_LOCAL_TIMER_IRQ, _handler);
            "MaiSched: CPU-local timer IRQ already registered"
        })?;
        let vector = interrupts::register_msi_interrupt(resched_ipi_handler)?;
        RESCHED_VECTOR.store(vector, Ordering::Release);
        task::scheduler::set_resched_ipi(send_resched_ipi);
    }

    #[cfg(target_arch = "aarch64")]
    {
        interrupts::setup_timer_interrupt(timer_tick_handler)?;
        generic_timer_aarch64::enable_timer_interrupt(true);
        let freq: u64;
        // SAFETY : lecture de la fréquence du compteur générique.
        unsafe { core::arch::asm!("mrs {}, cntfrq_el0", out(reg) freq) };
        task::scheduler::set_clock_ticks_per_ms(freq / 1000);
    }

    log::info!("MaiSched: {} CPU(s), EEVDF + temps réel, réveil par IPI", num_cpus);
    Ok(())
}

/// Phase 2 : tous les cœurs sont démarrés, on installe la vraie topologie.
pub fn expand_to_all_cpus(num_cpus: usize, topology: CpuTopology) -> Result<(), &'static str> {
    task::scheduler::set_cpu_count(num_cpus);
    task::scheduler::set_topology(topology);
    log::info!("MaiSched: {} CPU(s) avec topologie", num_cpus);
    Ok(())
}

/// Initialisation à un seul cœur (tout début du démarrage).
pub fn init_single_cpu() -> Result<(), &'static str> {
    init(1, CpuTopology::uniform(1))
}

/// Étalonnage de l'horloge de l'ordonnanceur : cycles TSC mesurés pendant une milliseconde.
///
/// La valeur fournie (issue du PIT) est contre-vérifiée sur l'horloge monotone du noyau (HPET si
/// présent) : un écart de plus de 10 % signale un PIT défaillant, et c'est alors la mesure
/// monotone qui est retenue. Une horloge fausse fausse toutes les tranches de temps.
pub fn calibrate_tsc(tsc_delta_per_ms: u64) {
    let chosen = match measure_tsc_per_ms() {
        Some(m) if tsc_delta_per_ms == 0 || m.abs_diff(tsc_delta_per_ms) * 10 > m => {
            log::warn!(
                "MaiSched: TSC fourni {} cycles/ms, mesuré {} cycles/ms sur l'horloge monotone : mesure retenue",
                tsc_delta_per_ms, m
            );
            m
        }
        _ => tsc_delta_per_ms,
    };
    task::scheduler::set_clock_ticks_per_ms(chosen);
    log::warn!("MaiSched: horloge TSC {} cycles/ms (~{} MHz)", chosen, chosen / 1000);
}

/// Cycles TSC par milliseconde, mesurés sur 5 ms de l'horloge monotone.
fn measure_tsc_per_ms() -> Option<u64> {
    #[cfg(target_arch = "x86_64")]
    {
        let t0 = time::Instant::now();
        // SAFETY : lecture du TSC.
        let c0 = unsafe { core::arch::x86_64::_rdtsc() };
        while t0.elapsed().as_micros() < 5_000 {
            core::hint::spin_loop();
        }
        let us = t0.elapsed().as_micros() as u64;
        // SAFETY : lecture du TSC.
        let c1 = unsafe { core::arch::x86_64::_rdtsc() };
        if us == 0 {
            return None;
        }
        Some((c1 - c0) * 1000 / us)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        None
    }
}

// ---------------------------------------------------------------------------
// Interruptions
// ---------------------------------------------------------------------------

#[cfg(target_arch = "x86_64")]
fn send_resched_ipi(cpu: usize) {
    let vector = RESCHED_VECTOR.load(Ordering::Acquire);
    if vector == 0 {
        return;
    }
    let Some(my_apic) = apic::get_my_apic() else { return };
    let dest = if cpu == cpu::current_cpu().value() as usize {
        apic::LapicIpiDestination::Me
    } else {
        match apic::ApicId::try_from(cpu as u32) {
            Ok(id) => apic::LapicIpiDestination::One(id),
            Err(_) => return,
        }
    };
    my_apic.write().send_ipi(vector, dest);
}

#[cfg(target_arch = "x86_64")]
interrupt_handler!(resched_ipi_handler, _, _stack_frame, {
    eoi(RESCHED_VECTOR.load(Ordering::Relaxed));
    if task::scheduler::need_resched() {
        task::scheduler::schedule_preempt();
    }
    EoiBehaviour::HandlerSentEoi
});

interrupt_handler!(timer_tick_handler, _, _stack_frame, {
    #[cfg(target_arch = "aarch64")]
    generic_timer_aarch64::set_next_timer_interrupt(get_timeslice_ticks());

    let resched = task::scheduler::scheduler_tick();

    // Réveil des tâches dont le délai de sommeil est écoulé. La liste est globale : un seul cœur
    // la consulte par fenêtre de 1 ms (l'horloge monotone est souvent le HPET, une lecture MMIO
    // coûteuse, et sous virtualisation une sortie de VM), et seulement s'il y a des dormeurs.
    if sleep::has_sleepers() && claim_sleep_check() {
        sleep::unblock_sleeping_tasks();
    }

    // Pompe audio sur le cœur 0 seulement (try_lock + remplissage DMA éventuel).
    if cpu::current_cpu().value() == 0 {
        audio_mixer::pump_hardware();
    }

    // EOI avant un éventuel changement de contexte : on ne revient pas tout de suite ici.
    eoi(CPU_LOCAL_TIMER_IRQ);

    if resched || task::scheduler::need_resched() {
        task::scheduler::schedule_preempt();
    }

    EoiBehaviour::HandlerSentEoi
});

/// Prochain instant (horloge MaiSched, ns) où un cœur peut consulter la liste des dormeurs.
static NEXT_SLEEP_CHECK_NS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Un seul cœur gagne le droit de consulter la liste des dormeurs par fenêtre de 1 ms.
fn claim_sleep_check() -> bool {
    let now = task::scheduler::clock_ns();
    let next = NEXT_SLEEP_CHECK_NS.load(Ordering::Relaxed);
    now >= next
        && NEXT_SLEEP_CHECK_NS
            .compare_exchange(next, now + 1_000_000, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
}

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

// ---------------------------------------------------------------------------
// Diagnostic
// ---------------------------------------------------------------------------

/// Écrit les statistiques de chaque cœur dans le journal.
pub fn dump_stats() {
    for s in stats() {
        log::warn!(
            "MaiSched CPU{}: prets={} switches={} reveils={} (distants {}) migrations={} preempt tick={} reveil={} latence reveil moy={}us max={}us",
            s.cpu, s.nr_running, s.switches, s.wakeups, s.remote_wakeups, s.migrations,
            s.tick_preemptions, s.wakeup_preemptions, s.wake_lat_avg_ns / 1000, s.wake_lat_max_ns / 1000,
        );
    }
}
