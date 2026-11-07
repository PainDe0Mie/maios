//! `mm_bench` : banc de mesure de l'allocateur de frames physiques (MaiMM, phase M0).
//!
//! Mesures reproductibles, comparables entre l'allocateur historique (liste + verrou) et LLFree :
//! * `single` : allocation puis libération d'une frame, latences p50/p99/max séparées ;
//! * `burst` : 256 frames allouées d'affilée puis toutes libérées, comme une rafale de défauts
//!   de page ;
//! * `contig` : plages contiguës de 8, 64 et 512 frames (512 frames = une page de 2 Mio) ;
//! * `scale` : 1, 2, 4… N cœurs qui allouent et libèrent en parallèle (débit total).
//!
//! Chaque résultat est aussi écrit dans le journal noyau sous la forme
//! `[mm_bench] clé=valeur ...`, ce qui permet au harnais `scripts/maios_bench.py` de le relever
//! sur le port série.

#![no_std]
#[macro_use] extern crate app_io;
extern crate alloc;

use alloc::{format, string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::{Acquire, Relaxed, Release}};
use frame_allocator::{allocate_frames, AllocatedFrames};

/// Compteur de cycles. Sur x86_64 c'est le TSC, converti en ns grâce à un étalonnage contre
/// l'horloge monotone du noyau.
#[inline(always)]
fn cycles() -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: lecture du TSC, sans effet de bord.
        unsafe { core::arch::x86_64::_rdtsc() }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        time::Instant::now().elapsed().as_nanos() as u64
    }
}

/// Cycles par microseconde, mesurés sur 20 ms.
fn calibrate() -> u64 {
    let t0 = time::Instant::now();
    let c0 = cycles();
    while t0.elapsed().as_micros() < 20_000 {
        core::hint::spin_loop();
    }
    let c1 = cycles();
    let us = t0.elapsed().as_micros().max(1) as u64;
    ((c1 - c0) / us).max(1)
}

struct Clock {
    cycles_per_us: u64,
}

impl Clock {
    fn ns(&self, cycles: u64) -> u64 {
        cycles * 1000 / self.cycles_per_us
    }
}

struct Summary {
    p50: u64,
    p99: u64,
    max: u64,
    mean: u64,
}

fn summarize(clock: &Clock, samples: &mut [u64]) -> Summary {
    samples.sort_unstable();
    let n = samples.len().max(1);
    let pick = |q: usize| samples.get((samples.len() * q / 100).min(samples.len().saturating_sub(1))).copied().unwrap_or(0);
    let sum: u64 = samples.iter().sum();
    Summary {
        p50: clock.ns(pick(50)),
        p99: clock.ns(pick(99)),
        max: clock.ns(samples.last().copied().unwrap_or(0)),
        mean: clock.ns(sum / n as u64),
    }
}

fn report(test: &str, op: &str, s: &Summary) {
    println!("  {test:<8} {op:<10} p50 {:>7} ns   p99 {:>7} ns   max {:>8} ns   moy {:>7} ns", s.p50, s.p99, s.max, s.mean);
    log::warn!("[mm_bench] test={test} op={op} p50_ns={} p99_ns={} max_ns={} mean_ns={}", s.p50, s.p99, s.max, s.mean);
}

/// Une frame allouée puis libérée immédiatement, `iters` fois.
fn bench_single(clock: &Clock, iters: usize) -> Result<(), &'static str> {
    let mut alloc_s = Vec::with_capacity(iters);
    let mut free_s = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t0 = cycles();
        let af = allocate_frames(1).ok_or("plus de mémoire")?;
        let t1 = cycles();
        drop(af);
        let t2 = cycles();
        alloc_s.push(t1 - t0);
        free_s.push(t2 - t1);
    }
    report("single", "alloc", &summarize(clock, &mut alloc_s));
    report("single", "free", &summarize(clock, &mut free_s));
    Ok(())
}

/// Rafales : 256 allocations d'une frame, puis 256 libérations.
fn bench_burst(clock: &Clock, iters: usize) -> Result<(), &'static str> {
    const BURST: usize = 256;
    let rounds = (iters / BURST).max(1);
    let mut alloc_s = Vec::with_capacity(rounds * BURST);
    let mut free_s = Vec::with_capacity(rounds * BURST);
    let mut held: Vec<AllocatedFrames> = Vec::with_capacity(BURST);
    for _ in 0..rounds {
        for _ in 0..BURST {
            let t0 = cycles();
            let af = allocate_frames(1).ok_or("plus de mémoire")?;
            alloc_s.push(cycles() - t0);
            held.push(af);
        }
        while let Some(af) = held.pop() {
            let t0 = cycles();
            drop(af);
            free_s.push(cycles() - t0);
        }
    }
    report("burst", "alloc", &summarize(clock, &mut alloc_s));
    report("burst", "free", &summarize(clock, &mut free_s));
    Ok(())
}

/// Plages contiguës.
fn bench_contig(clock: &Clock, iters: usize) -> Result<(), &'static str> {
    for size in [8usize, 64, 512] {
        let n = (iters / 10).max(100);
        let mut alloc_s = Vec::with_capacity(n);
        let mut free_s = Vec::with_capacity(n);
        for _ in 0..n {
            let t0 = cycles();
            let af = allocate_frames(size).ok_or("plus de mémoire contiguë")?;
            let t1 = cycles();
            drop(af);
            alloc_s.push(t1 - t0);
            free_s.push(cycles() - t1);
        }
        report("contig", &format!("alloc{size}"), &summarize(clock, &mut alloc_s));
        report("contig", &format!("free{size}"), &summarize(clock, &mut free_s));
    }
    Ok(())
}

struct Shared {
    ready: AtomicUsize,
    go: AtomicBool,
    ops: AtomicU64,
    failed: AtomicBool,
}

struct WorkerArg {
    shared: Arc<Shared>,
    iters: usize,
}

/// Tâche épinglée sur un cœur : fenêtre glissante de 16 frames (alloue, libère la plus ancienne).
fn scale_worker(arg: WorkerArg) -> isize {
    let mut window: Vec<AllocatedFrames> = Vec::with_capacity(16);
    arg.shared.ready.fetch_add(1, Release);
    while !arg.shared.go.load(Acquire) {
        core::hint::spin_loop();
    }
    for i in 0..arg.iters {
        match allocate_frames(1) {
            Some(af) => {
                if window.len() == 16 {
                    window.swap_remove(i % 16);
                }
                window.push(af);
            }
            None => {
                arg.shared.failed.store(true, Relaxed);
                break;
            }
        }
    }
    drop(window);
    arg.shared.ops.fetch_add(arg.iters as u64 * 2, Relaxed);
    0
}

/// Débit total (alloc + free) avec 1, 2, 4… N cœurs.
fn bench_scale(iters: usize) -> Result<(), &'static str> {
    let cpus: Vec<cpu::CpuId> = cpu::cpus().collect();
    let mut counts = Vec::new();
    let mut k = 1;
    while k < cpus.len() {
        counts.push(k);
        k *= 2;
    }
    counts.push(cpus.len());
    let mut base_rate = 0u64;
    for &k in &counts {
        let shared = Arc::new(Shared {
            ready: AtomicUsize::new(0),
            go: AtomicBool::new(false),
            ops: AtomicU64::new(0),
            failed: AtomicBool::new(false),
        });
        let mut tasks = Vec::new();
        for &cpu in cpus.iter().take(k) {
            let arg = WorkerArg { shared: shared.clone(), iters };
            tasks.push(
                spawn::new_task_builder(scale_worker, arg)
                    .name(format!("mm_bench_scale_{}", cpu.value()))
                    .pin_on_cpu(cpu)
                    .spawn()?,
            );
        }
        // Une tâche épinglée qui ne démarre jamais révèle un problème d'ordonnancement sur ce
        // cœur : on le signale au lieu de bloquer le banc.
        let wait = time::Instant::now();
        while shared.ready.load(Acquire) < k && wait.elapsed().as_millis() < 5_000 {
            core::hint::spin_loop();
        }
        let t0 = time::Instant::now();
        shared.go.store(true, Release);
        let all_started = shared.ready.load(Acquire) == k;
        while !tasks.iter().all(|t| t.has_exited()) && t0.elapsed().as_millis() < 60_000 {
            core::hint::spin_loop();
        }
        if !all_started || !tasks.iter().all(|t| t.has_exited()) {
            let started = shared.ready.load(Acquire);
            println!("  scale    {k:>2} coeur(s)  BLOQUE : {started}/{k} taches epinglees ont demarre");
            log::warn!("[mm_bench] test=scale cpus={k} status=stuck started={started}");
            break;
        }
        for t in &tasks {
            t.join()?;
        }
        let us = t0.elapsed().as_micros().max(1) as u64;
        if shared.failed.load(Relaxed) {
            return Err("plus de memoire pendant le test de montee en charge");
        }
        let ops = shared.ops.load(Relaxed);
        let kops = ops * 1000 / us; // opérations par milliseconde = kops/s
        if k == 1 {
            base_rate = kops.max(1);
        }
        let speedup_x100 = kops * 100 / base_rate.max(1);
        println!("  scale    {k:>2} coeur(s)  {kops:>8} kops/s   acceleration x{}.{:02}", speedup_x100 / 100, speedup_x100 % 100);
        log::warn!("[mm_bench] test=scale cpus={k} kops={kops} speedup_x100={speedup_x100}");
    }
    Ok(())
}

fn run(iters: usize, which: &str) -> Result<(), &'static str> {
    let clock = Clock { cycles_per_us: calibrate() };
    let before = frame_allocator::stats();
    // Sorties écran en ASCII : la police du terminal MaiOS n'a pas encore les accents.
    println!("mm_bench : allocateur '{}', {} iterations, TSC {} MHz", before.allocator, iters, clock.cycles_per_us);
    log::warn!("[mm_bench] start allocator={} iters={} tsc_mhz={}", before.allocator, iters, clock.cycles_per_us);
    if which == "all" || which == "single" {
        bench_single(&clock, iters)?;
    }
    if which == "all" || which == "burst" {
        bench_burst(&clock, iters)?;
    }
    if which == "all" || which == "contig" {
        bench_contig(&clock, iters)?;
    }
    if which == "all" || which == "scale" {
        bench_scale(iters)?;
    }
    let after = frame_allocator::stats();
    let leaked = before.free_frames as isize - after.free_frames as isize;
    println!("  frames libres : {} avant, {} apres (ecart {leaked})", before.free_frames, after.free_frames);
    log::warn!("[mm_bench] done free_before={} free_after={} leak={leaked}", before.free_frames, after.free_frames);
    Ok(())
}

pub fn main(args: Vec<String>) -> isize {
    let mut iters = 20_000usize;
    let mut which = String::from("all");
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-n" => match it.next().and_then(|v| v.parse().ok()) {
                Some(n) if n > 0 => iters = n,
                _ => {
                    println!("mm_bench : -n attend un entier positif");
                    log::error!("[mm_bench] error=argument");
                    return 1;
                }
            },
            "single" | "burst" | "contig" | "scale" | "all" => which = arg.clone(),
            "-h" | "--help" => {
                println!("Usage : mm_bench [-n ITERATIONS] [single|burst|contig|scale|all]");
                return 0;
            }
            other => {
                println!("mm_bench : argument inconnu '{other}'");
                return 1;
            }
        }
    }
    match run(iters, &which) {
        Ok(()) => 0,
        Err(e) => {
            println!("mm_bench : erreur : {e}");
            log::error!("[mm_bench] error={e}");
            -1
        }
    }
}
