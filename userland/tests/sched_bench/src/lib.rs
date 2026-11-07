//! `sched_bench` : banc de mesure de l'ordonnanceur (MaiSched, phase S0).
//!
//! Chaque test cible une propriété mesurée dans la littérature sur les ordonnanceurs :
//! * `yield`   : coût d'un `schedule()` sans autre tâche prête (chemin rapide) ;
//! * `switch`  : changement de contexte entre deux tâches épinglées sur un même cœur (ping-pong
//!   de `yield`, la mesure classique de lmbench `lat_ctx`) ;
//! * `wake`    : latence de réveil inter-cœurs, de `unblock()` au moment où la tâche réveillée
//!   s'exécute (équivalent de `perf bench sched pipe` / schbench) ;
//! * `preempt` : une tâche qui dort 1 ms en boucle face à une tâche qui calcule sans jamais céder,
//!   sur le même cœur ; on mesure le retard au réveil. Sans préemption temporelle, la dormeuse
//!   attend que la calculatrice ait fini ;
//! * `fair`    : trois tâches de calcul sur un cœur (nice 0, 0 et 5) ; parts de CPU observées
//!   contre les parts théoriques (poids 1024/1024/335) ;
//! * `balance` : 2 × N tâches de calcul non épinglées ; indice d'équité de Jain sur le travail
//!   accompli (1000 = parfait). Mesure l'équilibrage de charge (« Decade of Wasted Cores »,
//!   EuroSys 2016) ;
//! * `spawn`   : création + attente de fin d'une tâche vide.
//!
//! Les résultats sont aussi écrits dans le journal noyau (`[sched_bench] test=... clé=valeur`) pour
//! le harnais `scripts/maios_bench.py`. Chaque attente a un délai maximal : un ordonnanceur
//! défaillant est signalé (`status=stuck`), jamais un blocage du banc.

#![no_std]
#[macro_use] extern crate app_io;
extern crate alloc;

use alloc::{format, string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::{Acquire, Relaxed, Release, SeqCst}};
use core::time::Duration;
use task::{RunState, TaskRef};

#[inline(always)]
fn tsc() -> u64 {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: lecture du TSC, sans effet de bord.
    unsafe { core::arch::x86_64::_rdtsc() }
    #[cfg(not(target_arch = "x86_64"))]
    { time::Instant::now().elapsed().as_nanos() as u64 }
}

/// Conversion cycles TSC -> ns, étalonnée contre l'horloge monotone du noyau.
#[derive(Clone, Copy)]
struct Clock {
    cycles_per_us: u64,
}

impl Clock {
    fn calibrate() -> Clock {
        let t0 = time::Instant::now();
        let c0 = tsc();
        while t0.elapsed().as_micros() < 20_000 {
            core::hint::spin_loop();
        }
        let us = t0.elapsed().as_micros().max(1) as u64;
        Clock { cycles_per_us: ((tsc() - c0) / us).max(1) }
    }
    fn ns(&self, cycles: u64) -> u64 {
        cycles.saturating_mul(1000) / self.cycles_per_us
    }
    fn cycles(&self, ns: u64) -> u64 {
        ns * self.cycles_per_us / 1000
    }
}

struct Summary {
    p50: u64,
    p99: u64,
    max: u64,
    mean: u64,
}

fn summarize(samples: &mut [u64]) -> Summary {
    if samples.is_empty() {
        return Summary { p50: 0, p99: 0, max: 0, mean: 0 };
    }
    samples.sort_unstable();
    let n = samples.len();
    let pick = |q: usize| samples[(n * q / 100).min(n - 1)];
    Summary { p50: pick(50), p99: pick(99), max: samples[n - 1], mean: samples.iter().sum::<u64>() / n as u64 }
}

fn report_latency(test: &str, s: &Summary, n: usize) {
    println!("  {test:<8} p50 {:>8} ns   p99 {:>8} ns   max {:>9} ns   moy {:>8} ns  (n={n})", s.p50, s.p99, s.max, s.mean);
    log::warn!("[sched_bench] test={test} p50_ns={} p99_ns={} max_ns={} mean_ns={} n={n}", s.p50, s.p99, s.max, s.mean);
}

fn report_stuck(test: &str, detail: &str) {
    println!("  {test:<8} BLOQUE : {detail}");
    log::warn!("[sched_bench] test={test} status=stuck detail={}", detail.replace(' ', "_"));
}

/// Attend `cond` au plus `ms` millisecondes en cédant le processeur. Renvoie `false` au délai.
fn wait_until(ms: u64, mut cond: impl FnMut() -> bool) -> bool {
    let t0 = time::Instant::now();
    while !cond() {
        if t0.elapsed().as_millis() as u64 >= ms {
            return false;
        }
        task::schedule();
    }
    true
}

fn join_all(tasks: &[task::JoinableTaskRef], ms: u64) -> bool {
    wait_until(ms, || tasks.iter().all(|t| t.has_exited()))
}

/// Cœur cible des tests sur un seul cœur : le dernier, ou un autre que le nôtre.
fn target_cpu() -> cpu::CpuId {
    let me = cpu::current_cpu();
    let all: Vec<cpu::CpuId> = cpu::cpus().collect();
    all.iter().rev().copied().find(|c| *c != me).unwrap_or(me)
}

// ---------------------------------------------------------------------------
// yield : schedule() sans concurrent
// ---------------------------------------------------------------------------

fn bench_yield(clock: &Clock, iters: usize) {
    let mut s = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t0 = tsc();
        task::schedule();
        s.push(clock.ns(tsc() - t0));
    }
    report_latency("yield", &summarize(&mut s), iters);
}

// ---------------------------------------------------------------------------
// switch : ping-pong de yield entre deux tâches épinglées sur le même cœur
// ---------------------------------------------------------------------------

struct SwitchShared {
    ready: AtomicUsize,
    go: AtomicBool,
    turns: AtomicU64,
    end: AtomicU64,
}

fn switch_worker(arg: (Arc<SwitchShared>, usize)) -> isize {
    let (sh, iters) = arg;
    sh.ready.fetch_add(1, SeqCst);
    while !sh.go.load(Acquire) {
        task::schedule();
    }
    for _ in 0..iters {
        task::schedule();
        sh.turns.fetch_add(1, Relaxed);
    }
    sh.end.fetch_max(tsc(), Relaxed);
    0
}

fn bench_switch(clock: &Clock, iters: usize) -> Result<(), &'static str> {
    let cpu = target_cpu();
    let sh = Arc::new(SwitchShared { ready: AtomicUsize::new(0), go: AtomicBool::new(false), turns: AtomicU64::new(0), end: AtomicU64::new(0) });
    let mut tasks = Vec::new();
    for i in 0..2 {
        tasks.push(spawn::new_task_builder(switch_worker, (sh.clone(), iters)).name(format!("sb_switch_{i}")).pin_on_cpu(cpu).spawn()?);
    }
    if !wait_until(5_000, || sh.ready.load(SeqCst) == 2) {
        sh.go.store(true, Release);
        report_stuck("switch", &format!("{}/2 started", sh.ready.load(SeqCst)));
        return Ok(());
    }
    let t0 = tsc();
    sh.go.store(true, Release);
    if !join_all(&tasks, 60_000) {
        report_stuck("switch", "workers did not finish");
        return Ok(());
    }
    let turns = sh.turns.load(Relaxed).max(1);
    let ns = clock.ns(sh.end.load(Relaxed).saturating_sub(t0)) / turns;
    println!("  switch   {ns:>8} ns par changement de contexte ({turns} yields, coeur {cpu})");
    log::warn!("[sched_bench] test=switch ns_per_switch={ns} n={turns}");
    Ok(())
}

// ---------------------------------------------------------------------------
// wake : latence de réveil inter-cœurs
// ---------------------------------------------------------------------------

struct WakeShared {
    me: spin_slot::Slot,
    stamp: AtomicU64,
    acked: AtomicUsize,
    lat: alloc::boxed::Box<[AtomicU64]>,
    quit: AtomicBool,
}

/// Petit emplacement pour publier la `TaskRef` de la tâche réveillée au thread principal.
mod spin_slot {
    use task::TaskRef;
    pub struct Slot(spin_lock::Lock<Option<TaskRef>>);
    impl Slot {
        pub fn new() -> Slot { Slot(spin_lock::Lock::new(None)) }
        pub fn set(&self, t: TaskRef) { *self.0.lock() = Some(t); }
        pub fn get(&self) -> Option<TaskRef> { self.0.lock().clone() }
    }
    /// Verrou tournant minimal (pas de dépendance supplémentaire pour l'application).
    mod spin_lock {
        use core::cell::UnsafeCell;
        use core::sync::atomic::{AtomicBool, Ordering};
        pub struct Lock<T> { held: AtomicBool, val: UnsafeCell<T> }
        unsafe impl<T: Send> Sync for Lock<T> {}
        pub struct Guard<'a, T> { l: &'a Lock<T> }
        impl<T> Lock<T> {
            pub const fn new(v: T) -> Self { Lock { held: AtomicBool::new(false), val: UnsafeCell::new(v) } }
            pub fn lock(&self) -> Guard<'_, T> {
                while self.held.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
                    core::hint::spin_loop();
                }
                Guard { l: self }
            }
        }
        impl<T> core::ops::Deref for Guard<'_, T> { type Target = T; fn deref(&self) -> &T { unsafe { &*self.l.val.get() } } }
        impl<T> core::ops::DerefMut for Guard<'_, T> { fn deref_mut(&mut self) -> &mut T { unsafe { &mut *self.l.val.get() } } }
        impl<T> Drop for Guard<'_, T> { fn drop(&mut self) { self.l.held.store(false, Ordering::Release); } }
    }
}

fn wake_worker(sh: Arc<WakeShared>) -> isize {
    let me = task::get_my_current_task().expect("wake_worker: pas de tâche courante");
    sh.me.set(me.clone());
    let mut i = 0;
    loop {
        // Se bloquer, puis céder : l'ordonnanceur nous retire de la file.
        let _ = me.block();
        task::schedule();
        let now = tsc();
        if sh.quit.load(Acquire) {
            break;
        }
        if i < sh.lat.len() {
            sh.lat[i].store(now.saturating_sub(sh.stamp.load(Acquire)), Relaxed);
        }
        i += 1;
        sh.acked.store(i, Release);
    }
    0
}

fn bench_wake(clock: &Clock, iters: usize) -> Result<(), &'static str> {
    let cpu = target_cpu();
    let lat: Vec<AtomicU64> = (0..iters).map(|_| AtomicU64::new(0)).collect();
    let sh = Arc::new(WakeShared {
        me: spin_slot::Slot::new(),
        stamp: AtomicU64::new(0),
        acked: AtomicUsize::new(0),
        lat: lat.into_boxed_slice(),
        quit: AtomicBool::new(false),
    });
    let t = spawn::new_task_builder(wake_worker, sh.clone()).name(String::from("sb_wake")).pin_on_cpu(cpu).spawn()?;
    if !wait_until(5_000, || sh.me.get().is_some()) {
        report_stuck("wake", "wakee did not start");
        return Ok(());
    }
    let wakee: TaskRef = sh.me.get().unwrap();
    let mut done = 0;
    for i in 0..iters {
        // Attendre que la tâche soit réellement endormie (bloquée et plus en cours d'exécution).
        if !wait_until(2_000, || wakee.runstate.load() == RunState::Blocked && !wakee.is_running()) {
            break;
        }
        sh.stamp.store(tsc(), Release);
        let _ = wakee.unblock();
        if !wait_until(2_000, || sh.acked.load(Acquire) > i) {
            break;
        }
        done = i + 1;
    }
    sh.quit.store(true, Release);
    let _ = wait_until(2_000, || wakee.runstate.load() == RunState::Blocked);
    let _ = wakee.unblock();
    let _ = join_all(core::slice::from_ref(&t), 5_000);
    if done < iters {
        report_stuck("wake", &format!("{done}/{iters} wakeups"));
        if done == 0 {
            return Ok(());
        }
    }
    let mut s: Vec<u64> = sh.lat[..done].iter().map(|a| clock.ns(a.load(Relaxed))).collect();
    report_latency("wake", &summarize(&mut s), done);
    Ok(())
}

// ---------------------------------------------------------------------------
// preempt : dormeuse 1 ms contre calculatrice sur le même cœur
// ---------------------------------------------------------------------------

struct PreemptShared {
    started: AtomicBool,
    spin_until: AtomicU64,
    oversleep: alloc::boxed::Box<[AtomicU64]>,
    done: AtomicUsize,
}

fn sleeper(arg: (Arc<PreemptShared>, Clock)) -> isize {
    let (sh, clock) = arg;
    sh.started.store(true, Release);
    for i in 0..sh.oversleep.len() {
        let t0 = tsc();
        let _ = sleep::sleep(Duration::from_millis(1));
        let slept = clock.ns(tsc() - t0);
        sh.oversleep[i].store(slept.saturating_sub(1_000_000), Relaxed);
        sh.done.store(i + 1, Release);
    }
    0
}

fn spinner_until(end: u64) -> isize {
    let mut x = 0u64;
    while tsc() < end {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        core::hint::black_box(x);
    }
    0
}

fn bench_preempt(clock: &Clock, iters: usize) -> Result<(), &'static str> {
    let cpu = target_cpu();
    let sh = Arc::new(PreemptShared {
        started: AtomicBool::new(false),
        spin_until: AtomicU64::new(0),
        oversleep: (0..iters).map(|_| AtomicU64::new(0)).collect::<Vec<_>>().into_boxed_slice(),
        done: AtomicUsize::new(0),
    });
    let s = spawn::new_task_builder(sleeper, (sh.clone(), *clock)).name(String::from("sb_sleeper")).pin_on_cpu(cpu).spawn()?;
    if !wait_until(5_000, || sh.started.load(Acquire)) {
        report_stuck("preempt", "sleeper did not start");
        return Ok(());
    }
    // La calculatrice tourne pendant toute la durée nominale du test (+ marge).
    let end = tsc() + clock.cycles((iters as u64 * 2 + 200) * 1_000_000);
    sh.spin_until.store(end, Relaxed);
    let h = spawn::new_task_builder(spinner_until, end).name(String::from("sb_hog")).pin_on_cpu(cpu).spawn()?;
    let ok = join_all(core::slice::from_ref(&s), 30_000);
    let _ = join_all(core::slice::from_ref(&h), 30_000);
    let done = sh.done.load(Acquire);
    if !ok || done < iters {
        report_stuck("preempt", &format!("{done}/{iters} sleeps"));
    }
    if done == 0 {
        return Ok(());
    }
    let mut v: Vec<u64> = sh.oversleep[..done].iter().map(|a| a.load(Relaxed)).collect();
    report_latency("preempt", &summarize(&mut v), done);
    Ok(())
}

// ---------------------------------------------------------------------------
// fair : parts de CPU sur un cœur (nice 0, 0, 5)
// ---------------------------------------------------------------------------

struct FairShared {
    start: AtomicU64,
    end: AtomicU64,
    work: [AtomicU64; 3],
}

fn fair_worker(arg: (Arc<FairShared>, usize)) -> isize {
    let (sh, idx) = arg;
    while tsc() < sh.start.load(Acquire) {
        task::schedule();
    }
    let end = sh.end.load(Relaxed);
    let mut n = 0u64;
    let mut x = idx as u64;
    loop {
        for _ in 0..64 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        core::hint::black_box(x);
        n += 1;
        if n & 63 == 0 && tsc() >= end {
            break;
        }
    }
    sh.work[idx].store(n, Relaxed);
    0
}

fn bench_fair(clock: &Clock, ms: u64) -> Result<(), &'static str> {
    let cpu = target_cpu();
    let start = tsc() + clock.cycles(50_000_000);
    let sh = Arc::new(FairShared {
        start: AtomicU64::new(start),
        end: AtomicU64::new(start + clock.cycles(ms * 1_000_000)),
        work: [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)],
    });
    let nices: [i8; 3] = [0, 0, 5];
    let mut tasks = Vec::new();
    for (i, nice) in nices.iter().enumerate() {
        let t = spawn::new_task_builder(fair_worker, (sh.clone(), i)).name(format!("sb_fair_{i}")).pin_on_cpu(cpu).spawn()?;
        task::scheduler::set_priority(&t, (*nice + 20) as u8);
        tasks.push(t);
    }
    if !join_all(&tasks, ms + 30_000) {
        report_stuck("fair", "workers did not finish");
        return Ok(());
    }
    let w: Vec<u64> = sh.work.iter().map(|a| a.load(Relaxed)).collect();
    let total: u64 = w.iter().sum::<u64>().max(1);
    // Parts théoriques (poids Linux) : 1024, 1024, 335 -> 42.98 %, 42.98 %, 14.06 %.
    let expect = [4298u64, 4298, 1406];
    let mut err_max = 0u64;
    let mut shares = [0u64; 3];
    for i in 0..3 {
        shares[i] = w[i] * 10_000 / total;
        err_max = err_max.max(shares[i].abs_diff(expect[i]));
    }
    println!("  fair     parts {}.{:02}% / {}.{:02}% / {}.{:02}%  (attendu 42.98/42.98/14.06)  ecart max {}.{:02} pts",
        shares[0] / 100, shares[0] % 100, shares[1] / 100, shares[1] % 100, shares[2] / 100, shares[2] % 100, err_max / 100, err_max % 100);
    log::warn!("[sched_bench] test=fair share0_bp={} share1_bp={} share2_bp={} err_bp={}", shares[0], shares[1], shares[2], err_max);
    Ok(())
}

// ---------------------------------------------------------------------------
// balance : 2 x N tâches non épinglées, indice de Jain
// ---------------------------------------------------------------------------

struct BalanceShared {
    start: AtomicU64,
    end: AtomicU64,
    work: alloc::boxed::Box<[AtomicU64]>,
}

fn balance_worker(arg: (Arc<BalanceShared>, usize)) -> isize {
    let (sh, idx) = arg;
    while tsc() < sh.start.load(Acquire) {
        task::schedule();
    }
    let end = sh.end.load(Relaxed);
    let mut n = 0u64;
    let mut x = idx as u64;
    loop {
        for _ in 0..64 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        core::hint::black_box(x);
        n += 1;
        if n & 63 == 0 && tsc() >= end {
            break;
        }
    }
    sh.work[idx].store(n, Relaxed);
    0
}

fn bench_balance(clock: &Clock, ms: u64) -> Result<(), &'static str> {
    let ncpu = cpu::cpu_count() as usize;
    let k = 2 * ncpu;
    let start = tsc() + clock.cycles(50_000_000);
    let sh = Arc::new(BalanceShared {
        start: AtomicU64::new(start),
        end: AtomicU64::new(start + clock.cycles(ms * 1_000_000)),
        work: (0..k).map(|_| AtomicU64::new(0)).collect::<Vec<_>>().into_boxed_slice(),
    });
    let mut tasks = Vec::new();
    for i in 0..k {
        tasks.push(spawn::new_task_builder(balance_worker, (sh.clone(), i)).name(format!("sb_bal_{i}")).spawn()?);
    }
    // Le thread principal dort pour ne pas fausser la mesure.
    let _ = sleep::sleep(Duration::from_millis(ms + 60));
    if !join_all(&tasks, ms + 30_000) {
        report_stuck("balance", "workers did not finish");
        return Ok(());
    }
    let w: Vec<u64> = sh.work.iter().map(|a| a.load(Relaxed)).collect();
    // Indice de Jain : (somme)^2 / (n * somme des carrés), x1000. En u128 pour éviter les débordements.
    let sum: u128 = w.iter().map(|&x| x as u128).sum();
    let sq: u128 = w.iter().map(|&x| (x as u128) * (x as u128)).sum();
    let jain = if sq == 0 { 0 } else { (sum * sum * 1000 / (k as u128 * sq)) as u64 };
    let min = *w.iter().min().unwrap_or(&0);
    let max = *w.iter().max().unwrap_or(&1).max(&1);
    let min_pct = min * 100 / max;
    println!("  balance  {k} taches / {ncpu} coeurs : indice de Jain {}.{:03}, la moins servie a {min_pct} % de la plus servie", jain / 1000, jain % 1000);
    log::warn!("[sched_bench] test=balance tasks={k} jain_x1000={jain} min_pct={min_pct}");
    Ok(())
}

// ---------------------------------------------------------------------------
// spawn : création + fin d'une tâche vide
// ---------------------------------------------------------------------------

fn nop(_: ()) -> isize { 0 }

fn bench_spawn(clock: &Clock, iters: usize) -> Result<(), &'static str> {
    let mut s = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t0 = tsc();
        let t = spawn::new_task_builder(nop, ()).name(String::from("sb_nop")).spawn()?;
        if !join_all(core::slice::from_ref(&t), 5_000) {
            report_stuck("spawn", "task did not exit");
            return Ok(());
        }
        s.push(clock.ns(tsc() - t0));
    }
    report_latency("spawn", &summarize(&mut s), iters);
    Ok(())
}

/// Fréquence réelle du tick d'ordonnancement, par cœur (doit valoir 1 / CONFIG_TIMESLICE_PERIOD).
fn bench_tick(clock: &Clock) {
    let before = task::scheduler::stats();
    let t0 = tsc();
    let _ = sleep::sleep(Duration::from_millis(300));
    let ns = clock.ns(tsc() - t0).max(1);
    let after = task::scheduler::stats();
    let mut hz = Vec::new();
    for a in &after {
        if let Some(b) = before.iter().find(|b| b.cpu == a.cpu) {
            hz.push((a.ticks - b.ticks) * 1_000_000_000 / ns);
        }
    }
    let avg = hz.iter().sum::<u64>() / (hz.len().max(1) as u64);
    println!("  tick     {avg} Hz par coeur (attendu 500 Hz) : {hz:?}");
    log::warn!("[sched_bench] test=tick hz={avg}");
}

/// `top` : temps CPU consommé par chaque tâche pendant `ms` millisecondes (diagnostic).
fn bench_top(ms: u64) {
    let snap = || -> Vec<(usize, String, u64, usize)> {
        task::all_tasks()
            .into_iter()
            .map(|(id, t)| {
                let s = t.sched.snapshot();
                (id, t.name.clone(), s.sum_exec_ns, s.cpu)
            })
            .collect()
    };
    let before = snap();
    let _ = sleep::sleep(Duration::from_millis(ms));
    let after = snap();
    let mut rows: Vec<(u64, usize, String, usize)> = after
        .iter()
        .map(|(id, name, exec, cpu)| {
            let b = before.iter().find(|r| r.0 == *id).map(|r| r.2).unwrap_or(0);
            (exec.saturating_sub(b), *id, name.clone(), *cpu)
        })
        .collect();
    rows.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    println!("  top      temps CPU sur {ms} ms :");
    for (ns, id, name, cpu) in rows.iter().take(12) {
        let pm = ns * 1000 / (ms * 1_000_000).max(1);
        println!("    {:>5}.{} %  cpu{cpu}  [{id}] {name}", pm / 10, pm % 10);
        log::warn!("[sched_bench] top task={} id={id} cpu={cpu} permille={pm}", name.replace(' ', "_"));
    }
}

fn run(which: &str, iters: usize) -> Result<(), &'static str> {
    let clock = Clock::calibrate();
    let ncpu = cpu::cpu_count();
    println!("sched_bench : {ncpu} coeurs, TSC {} MHz", clock.cycles_per_us);
    log::warn!("[sched_bench] start cpus={ncpu} tsc_mhz={}", clock.cycles_per_us);
    if which == "top" {
        bench_top(iters as u64);
        log::warn!("[sched_bench] done");
        return Ok(());
    }
    bench_tick(&clock);
    let all = which == "all";
    if all || which == "yield" { bench_yield(&clock, iters); }
    if all || which == "switch" { bench_switch(&clock, iters)?; }
    if all || which == "wake" { bench_wake(&clock, (iters / 20).clamp(50, 2_000))?; }
    if all || which == "preempt" { bench_preempt(&clock, 50)?; }
    if all || which == "fair" { bench_fair(&clock, 600)?; }
    if all || which == "balance" { bench_balance(&clock, 600)?; }
    if all || which == "spawn" { bench_spawn(&clock, 100)?; }
    log::warn!("[sched_bench] done");
    Ok(())
}

pub fn main(args: Vec<String>) -> isize {
    let mut iters = 10_000usize;
    let mut which = String::from("all");
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-n" => match it.next().and_then(|v| v.parse().ok()) {
                Some(n) if n > 0 => iters = n,
                _ => {
                    println!("sched_bench : -n attend un entier positif");
                    return 1;
                }
            },
            "yield" | "switch" | "wake" | "preempt" | "fair" | "balance" | "spawn" | "all" | "top" => which = arg.clone(),
            "-h" | "--help" => {
                println!("Usage : sched_bench [-n ITERATIONS] [yield|switch|wake|preempt|fair|balance|spawn|all] | sched_bench -n MS top");
                return 0;
            }
            other => {
                println!("sched_bench : argument inconnu '{other}'");
                return 1;
            }
        }
    }
    match run(&which, iters) {
        Ok(()) => 0,
        Err(e) => {
            println!("sched_bench : erreur : {e}");
            log::error!("[sched_bench] error={e}");
            -1
        }
    }
}
