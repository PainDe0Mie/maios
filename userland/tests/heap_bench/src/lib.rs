//! `heap_bench` : banc de mesure du tas noyau (MaiMM, phase M2).
//!
//! Tests inspirés de mimalloc-bench (Leijen et al.) et des bancs de snmalloc, pour comparer
//! l'ancien tas (`multiple_heaps` + `slabmalloc`) et `kalloc` sur la même machine :
//! * `lat` : latences p50/p99 d'allocation et de libération par taille (16 o à 256 Kio), en lots
//!   de 128 objets libérés dans un ordre mélangé (pas seulement le même bloc réutilisé) ;
//! * `churn` : 4096 objets vivants de tailles aléatoires (8 o à 2 Kio, loi log-uniforme), chacun
//!   remplacé au hasard, comme `alloc-test` ;
//! * `xfree` : un cœur alloue, un autre libère (`xmalloc-test`) : mesure le coût des libérations
//!   distantes, le point faible des tas par cœur à verrou ;
//! * `scale` : 1, 2, 4… N cœurs qui font chacun du churn local (débit total, accélération) ;
//! * `rss` : 32 Mio de petits objets alloués puis libérés : mémoire physique consommée au pic et
//!   *rendue* ensuite (un tas qui ne rend jamais rien garde tout).
//!
//! Résultats écrits dans le journal noyau sous la forme `[heap_bench] test=... clé=valeur`,
//! relevés sur le port série par `scripts/maios_bench.py --tests heap`.

#![no_std]
#[macro_use] extern crate app_io;
extern crate alloc;

use alloc::alloc::{alloc, dealloc, Layout};
use alloc::{format, string::String, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::{Acquire, Relaxed, Release}};

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

/// Cycles par microseconde, mesurés sur 20 ms contre l'horloge monotone.
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
    mean: u64,
}

fn summarize(clock: &Clock, samples: &mut [u64]) -> Summary {
    samples.sort_unstable();
    let n = samples.len().max(1);
    let pick = |q: usize| samples.get((samples.len() * q / 100).min(samples.len().saturating_sub(1))).copied().unwrap_or(0);
    let sum: u64 = samples.iter().sum();
    Summary { p50: clock.ns(pick(50)), p99: clock.ns(pick(99)), mean: clock.ns(sum / n as u64) }
}

/// Générateur xorshift64* : rapide, reproductible, suffisant pour tirer des tailles.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    /// Taille log-uniforme entre 8 et 2048 octets (la distribution typique d'un noyau : beaucoup
    /// de petits objets, quelques tampons).
    fn size(&mut self) -> usize {
        let shift = 3 + self.below(9); // 8 .. 2048
        let base = 1usize << shift;
        base + self.below(base)
    }
}

fn layout(size: usize) -> Layout {
    Layout::from_size_align(size, 8).unwrap()
}

// ---------------------------------------------------------------------------
// lat
// ---------------------------------------------------------------------------

fn bench_lat(clock: &Clock, iters: usize) -> Result<(), &'static str> {
    const BATCH: usize = 128;
    let sizes = [16usize, 64, 256, 1024, 4096, 16384, 65536, 262144];
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for &size in &sizes {
        // Les grandes tailles passent par le mappage de pages : moins d'itérations.
        let rounds = (if size >= 65536 { iters / 10 } else { iters } / BATCH).max(4);
        let lay = layout(size);
        let mut a_samples = Vec::with_capacity(rounds * BATCH);
        let mut f_samples = Vec::with_capacity(rounds * BATCH);
        let mut ptrs = vec![core::ptr::null_mut::<u8>(); BATCH];
        let mut order: Vec<usize> = (0..BATCH).collect();
        for _ in 0..rounds {
            for p in ptrs.iter_mut() {
                let t0 = cycles();
                // SAFETY: taille non nulle, libéré plus bas avec la même disposition.
                let q = unsafe { alloc(lay) };
                let t1 = cycles();
                if q.is_null() {
                    return Err("plus de memoire (lat)");
                }
                // On touche l'objet : un tas qui rend des pages non mappées serait pénalisé ici.
                unsafe { q.write_volatile(1) };
                a_samples.push(t1 - t0);
                *p = q;
            }
            for i in (1..BATCH).rev() {
                order.swap(i, rng.below(i + 1));
            }
            for &i in &order {
                let t0 = cycles();
                // SAFETY: alloué ci-dessus avec `lay`.
                unsafe { dealloc(ptrs[i], lay) };
                let t1 = cycles();
                f_samples.push(t1 - t0);
            }
        }
        let a = summarize(clock, &mut a_samples);
        let f = summarize(clock, &mut f_samples);
        println!("  lat   {size:>7} o  alloc p50 {:>6} ns p99 {:>7} ns | free p50 {:>6} ns p99 {:>7} ns", a.p50, a.p99, f.p50, f.p99);
        log::warn!("[heap_bench] test=lat size={size} op=alloc p50_ns={} p99_ns={} mean_ns={}", a.p50, a.p99, a.mean);
        log::warn!("[heap_bench] test=lat size={size} op=free p50_ns={} p99_ns={} mean_ns={}", f.p50, f.p99, f.mean);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// churn (et le travail de chaque cœur de `scale`)
// ---------------------------------------------------------------------------

/// Remplace `ops` fois un objet tiré au hasard parmi `slots`. Rend le nombre d'opérations
/// (allocations + libérations), ou `None` si la mémoire manque.
fn churn(slots: usize, ops: usize, seed: u64) -> Option<u64> {
    let mut rng = Rng(seed | 1);
    let mut live: Vec<(*mut u8, usize)> = vec![(core::ptr::null_mut(), 0); slots];
    let mut done = 0u64;
    for _ in 0..ops {
        let i = rng.below(slots);
        let (p, s) = live[i];
        if !p.is_null() {
            // SAFETY: alloué plus bas avec la taille `s`.
            unsafe { dealloc(p, layout(s)) };
            done += 1;
        }
        let s = rng.size();
        // SAFETY: taille non nulle.
        let q = unsafe { alloc(layout(s)) };
        if q.is_null() {
            return None;
        }
        unsafe { q.write_volatile(s as u8) };
        live[i] = (q, s);
        done += 1;
    }
    for (p, s) in live {
        if !p.is_null() {
            unsafe { dealloc(p, layout(s)) };
            done += 1;
        }
    }
    Some(done)
}

fn bench_churn(iters: usize) -> Result<(), &'static str> {
    let ops = iters * 10;
    let t0 = time::Instant::now();
    let done = churn(4096, ops, 0xD1B5_4A32_D192_ED03).ok_or("plus de memoire (churn)")?;
    let us = t0.elapsed().as_micros().max(1) as u64;
    let kops = done * 1000 / us;
    println!("  churn  4096 objets vivants, {done} ops : {kops} kops/s");
    log::warn!("[heap_bench] test=churn kops={kops}");
    Ok(())
}

// ---------------------------------------------------------------------------
// xfree : allocation sur un cœur, libération sur un autre
// ---------------------------------------------------------------------------

const RING: usize = 1024;

struct Ring {
    slots: Vec<AtomicUsize>,
    head: AtomicUsize,
    tail: AtomicUsize,
    done: AtomicBool,
    failed: AtomicBool,
}

/// Tailles 16 à 512 octets, codées dans les 3 bits de poids faible du pointeur (aligné sur 8).
const XSIZES: [usize; 6] = [16, 32, 64, 128, 256, 512];

fn xproducer(arg: (Arc<Ring>, usize)) -> isize {
    let (ring, n) = arg;
    for i in 0..n {
        let k = i % XSIZES.len();
        // SAFETY: taille non nulle ; libéré par le consommateur avec la même disposition.
        let p = unsafe { alloc(layout(XSIZES[k])) };
        if p.is_null() {
            ring.failed.store(true, Relaxed);
            break;
        }
        unsafe { p.write_volatile(k as u8) };
        let h = ring.head.load(Relaxed);
        while h.wrapping_sub(ring.tail.load(Acquire)) >= RING {
            core::hint::spin_loop();
        }
        ring.slots[h % RING].store(p as usize | k, Relaxed);
        ring.head.store(h + 1, Release);
    }
    ring.done.store(true, Release);
    0
}

fn xconsumer(ring: Arc<Ring>) -> isize {
    loop {
        let t = ring.tail.load(Relaxed);
        if t == ring.head.load(Acquire) {
            if ring.done.load(Acquire) && t == ring.head.load(Acquire) {
                break;
            }
            core::hint::spin_loop();
            continue;
        }
        let v = ring.slots[t % RING].load(Relaxed);
        let k = v & 7;
        // SAFETY: pointeur alloué par le producteur avec `XSIZES[k]`.
        unsafe { dealloc((v & !7) as *mut u8, layout(XSIZES[k])) };
        ring.tail.store(t + 1, Release);
    }
    0
}

fn bench_xfree(iters: usize) -> Result<(), &'static str> {
    let cpus: Vec<cpu::CpuId> = cpu::cpus().collect();
    if cpus.len() < 2 {
        println!("  xfree  ignore : un seul coeur");
        return Ok(());
    }
    let n = iters * 10;
    let ring = Arc::new(Ring {
        slots: (0..RING).map(|_| AtomicUsize::new(0)).collect(),
        head: AtomicUsize::new(0),
        tail: AtomicUsize::new(0),
        done: AtomicBool::new(false),
        failed: AtomicBool::new(false),
    });
    let t0 = time::Instant::now();
    let c = spawn::new_task_builder(xconsumer, ring.clone())
        .name(String::from("heap_bench_xfree_c"))
        .pin_on_cpu(cpus[1])
        .spawn()?;
    let p = spawn::new_task_builder(xproducer, (ring.clone(), n))
        .name(String::from("heap_bench_xfree_p"))
        .pin_on_cpu(cpus[0])
        .spawn()?;
    p.join()?;
    c.join()?;
    let us = t0.elapsed().as_micros().max(1) as u64;
    if ring.failed.load(Relaxed) {
        return Err("plus de memoire (xfree)");
    }
    let kops = (2 * n as u64) * 1000 / us;
    println!("  xfree  coeur {} alloue, coeur {} libere : {kops} kops/s", cpus[0].value(), cpus[1].value());
    log::warn!("[heap_bench] test=xfree kops={kops}");
    Ok(())
}

// ---------------------------------------------------------------------------
// scale
// ---------------------------------------------------------------------------

struct Shared {
    ready: AtomicUsize,
    go: AtomicBool,
    ops: AtomicU64,
    failed: AtomicBool,
}

fn scale_worker(arg: (Arc<Shared>, usize, u64)) -> isize {
    let (shared, ops, seed) = arg;
    shared.ready.fetch_add(1, Release);
    while !shared.go.load(Acquire) {
        core::hint::spin_loop();
    }
    match churn(512, ops, seed) {
        Some(d) => {
            shared.ops.fetch_add(d, Relaxed);
        }
        None => shared.failed.store(true, Relaxed),
    }
    0
}

fn bench_scale(iters: usize) -> Result<(), &'static str> {
    let cpus: Vec<cpu::CpuId> = cpu::cpus().collect();
    let mut counts = Vec::new();
    let mut k = 1;
    while k < cpus.len() {
        counts.push(k);
        k *= 2;
    }
    counts.push(cpus.len());
    let mut base = 0u64;
    for &k in &counts {
        let shared = Arc::new(Shared {
            ready: AtomicUsize::new(0),
            go: AtomicBool::new(false),
            ops: AtomicU64::new(0),
            failed: AtomicBool::new(false),
        });
        let mut tasks = Vec::new();
        for (j, &cpu) in cpus.iter().take(k).enumerate() {
            let arg = (shared.clone(), iters * 4, 0x1234_5678 + j as u64 * 0x9E37_79B9);
            tasks.push(
                spawn::new_task_builder(scale_worker, arg)
                    .name(format!("heap_bench_scale_{}", cpu.value()))
                    .pin_on_cpu(cpu)
                    .spawn()?,
            );
        }
        while shared.ready.load(Acquire) < k {
            core::hint::spin_loop();
        }
        let t0 = time::Instant::now();
        shared.go.store(true, Release);
        for t in &tasks {
            t.join()?;
        }
        let us = t0.elapsed().as_micros().max(1) as u64;
        if shared.failed.load(Relaxed) {
            return Err("plus de memoire (scale)");
        }
        let kops = shared.ops.load(Relaxed) * 1000 / us;
        if k == 1 {
            base = kops.max(1);
        }
        let sp = kops * 100 / base.max(1);
        println!("  scale  {k:>2} coeur(s)  {kops:>8} kops/s   acceleration x{}.{:02}", sp / 100, sp % 100);
        log::warn!("[heap_bench] test=scale cpus={k} kops={kops} speedup_x100={sp}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// rss : mémoire consommée et rendue
// ---------------------------------------------------------------------------

fn free_kib() -> usize {
    frame_allocator::stats().free_frames * 4
}

fn bench_rss() -> Result<(), &'static str> {
    const PAYLOAD: usize = 32 << 20;
    let before = free_kib();
    let mut rng = Rng(0xA076_1D64_78BD_642F);
    let mut ptrs: Vec<(*mut u8, usize)> = Vec::with_capacity(PAYLOAD / 64);
    let mut total = 0usize;
    while total < PAYLOAD {
        let s = 16 + rng.below(1008); // 16 .. 1024 o
        // SAFETY: taille non nulle.
        let p = unsafe { alloc(layout(s)) };
        if p.is_null() {
            break;
        }
        unsafe { p.write_volatile(1) };
        ptrs.push((p, s));
        total += s;
    }
    let peak = before.saturating_sub(free_kib());
    // Libération dans un ordre mélangé (fragmentation réaliste).
    for i in (1..ptrs.len()).rev() {
        let j = rng.below(i + 1);
        ptrs.swap(i, j);
    }
    for &(p, s) in &ptrs {
        unsafe { dealloc(p, layout(s)) };
    }
    drop(ptrs);
    // Laisse aux mécanismes différés (tâche idle, collecte) le temps d'agir.
    let t0 = time::Instant::now();
    while t0.elapsed().as_millis() < 100 {
        core::hint::spin_loop();
    }
    let retained = before.saturating_sub(free_kib());
    let payload_kib = total / 1024;
    let overhead_pct = if payload_kib > 0 { peak.saturating_sub(payload_kib) * 100 / payload_kib } else { 0 };
    println!("  rss    {payload_kib} Kio utiles : pic {peak} Kio (+{overhead_pct} %), garde apres liberation {retained} Kio");
    log::warn!("[heap_bench] test=rss payload_kib={payload_kib} peak_kib={peak} overhead_pct={overhead_pct} retained_kib={retained}");
    Ok(())
}

// ---------------------------------------------------------------------------

fn run(iters: usize, which: &str) -> Result<(), &'static str> {
    let clock = Clock { cycles_per_us: calibrate() };
    println!("heap_bench : {iters} iterations, TSC {} MHz", clock.cycles_per_us);
    log::warn!("[heap_bench] start iters={iters} tsc_mhz={}", clock.cycles_per_us);
    let all = which == "all";
    if all || which == "lat" {
        bench_lat(&clock, iters)?;
    }
    if all || which == "churn" {
        bench_churn(iters)?;
    }
    if all || which == "xfree" {
        bench_xfree(iters)?;
    }
    if all || which == "scale" {
        bench_scale(iters)?;
    }
    if all || which == "rss" {
        bench_rss()?;
    }
    log::warn!("[heap_bench] done");
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
                    println!("heap_bench : -n attend un entier positif");
                    log::error!("[heap_bench] error=argument");
                    return 1;
                }
            },
            "lat" | "churn" | "xfree" | "scale" | "rss" | "all" => which = arg.clone(),
            "-h" | "--help" => {
                println!("Usage : heap_bench [-n ITERATIONS] [lat|churn|xfree|scale|rss|all]");
                return 0;
            }
            other => {
                println!("heap_bench : argument inconnu '{other}'");
                return 1;
            }
        }
    }
    match run(iters, &which) {
        Ok(()) => 0,
        Err(e) => {
            println!("heap_bench : erreur : {e}");
            log::error!("[heap_bench] error={e}");
            -1
        }
    }
}
