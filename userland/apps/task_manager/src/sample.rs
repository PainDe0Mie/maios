//! Échantillonnage du système, une fois par période : cœurs, tâches, processus, mémoire.
//!
//! Méthodes reprises des outils de référence :
//! - **Utilisation d'un cœur** : 1 − Δ(temps inactif) / Δ(temps), à partir du temps passé dans
//!   la tâche inactive de chaque cœur (comptabilité de l'ordonnanceur, comme la ligne `idle` de
//!   `/proc/stat`), et non d'un simple instantané « tourne / ne tourne pas ».
//! - **CPU d'une tâche ou d'un processus** : Δ(temps d'exécution) / (Δt × cœurs), comme `top`
//!   en mode « Solaris » et le gestionnaire des tâches de Windows (part de la machine).
//! - **Charge moyenne** : moyennes exponentielles sur 1, 5 et 15 minutes du nombre de tâches
//!   prêtes, en virgule fixe 11 bits, comme `calc_load()` de Linux.
//! - **Pression** : part du temps où au moins une tâche prête attendait un cœur, moyennée sur
//!   10, 60 et 300 s, comme la ligne `some` de PSI (`/proc/pressure/cpu`, Linux 4.20,
//!   J. Weiner).
//! - **Utilisation du GPU** : Δ(temps où l'appareil avait une commande en cours) / Δt, comme le
//!   temps actif d'un moteur dans le gestionnaire des tâches de Windows (WDDM) ou les clés
//!   `drm-engine-*` de `fdinfo` sous Linux. Sans GPU, le graphe montre la composition (temps
//!   processeur de MaiCompositor).

use alloc::collections::BTreeMap;
use alloc::string::String;
#[cfg(target_arch = "x86_64")]
use alloc::sync::Arc;
use alloc::vec::Vec;

use mai_ui::kit::History;
use task_struct::RunState;

/// Points d'historique des graphes (une minute à 2 échantillons par seconde, deux à 1/s).
pub const HIST: usize = 120;

/// Virgule fixe des moyennes exponentielles (comme `FSHIFT` de Linux).
const FSHIFT: u32 = 11;
const FIXED_1: u64 = 1 << FSHIFT;

/// e^(−1/τ) pour une seconde, en virgule fixe 11 bits.
const EXP_1M: u64 = 2014; // τ = 60 s
const EXP_5M: u64 = 2041; // τ = 300 s
const EXP_15M: u64 = 2046; // τ = 900 s
const EXP_10S: u64 = 1853; // τ = 10 s

/// Facteur pour `secs` secondes : e^(−secs/τ) = (e^(−1/τ))^secs.
fn decay(e: u64, secs: u32) -> u64 {
    let mut r = FIXED_1;
    for _ in 0..secs.max(1) {
        r = (r * e) >> FSHIFT;
    }
    r
}

/// Moyenne exponentielle en virgule fixe : `avg = avg·e + v·(1−e)`.
fn ewma(avg: u64, v: u64, e: u64) -> u64 { (avg * e + v * (FIXED_1 - e)) >> FSHIFT }

/// Cœur.
#[derive(Clone, Default)]
pub struct Core {
    pub id: usize,
    /// Utilisation sur la dernière période (‰).
    pub busy_pm: u32,
    /// Pression sur la dernière période (‰).
    pub pressure_pm: u32,
    pub nr_running: u32,
    /// Nombre moyen de tâches prêtes sur la période (centièmes).
    pub runnable_x100: u64,
    pub switches_ps: u64,
    pub wakeups_ps: u64,
    pub remote_wakeups_ps: u64,
    pub migrations_ps: u64,
    pub tick_preempt_ps: u64,
    pub wake_preempt_ps: u64,
    pub wake_lat_avg_ns: u64,
    pub wake_lat_max_ns: u64,
    pub heap_allocs_ps: u64,
    pub hist: History<HIST>,
}

/// Genre d'une ligne de la liste des processus.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// Processus Linux (anneau 3).
    Process,
    /// Fil d'un processus Linux.
    Thread,
    /// Application MaiOS (crate d'application chargée).
    App,
    /// Tâche du noyau.
    Kernel,
    /// Tâche inactive d'un cœur.
    Idle,
}

/// État affiché.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Running,
    Ready,
    Sleeping,
    Stopped,
    Starting,
    Exited,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Running => "En cours",
            State::Ready => "Prêt",
            State::Sleeping => "En attente",
            State::Stopped => "Suspendu",
            State::Starting => "Démarrage",
            State::Exited => "Terminé",
        }
    }
}

/// Entité de la liste (processus, fil ou tâche).
#[derive(Clone)]
pub struct Entry {
    pub kind: Kind,
    /// PID d'un processus, TID d'un fil, identifiant d'une tâche.
    pub id: usize,
    /// Tâche de l'ordonnanceur (fil, application, tâche du noyau).
    pub task_id: Option<usize>,
    /// Processus parent (processus Linux), processus propriétaire (fil).
    pub parent: Option<usize>,
    pub name: String,
    /// Chemin de l'exécutable ou de la crate.
    pub path: String,
    pub state: State,
    /// Part de la machine sur la dernière période (‰).
    pub cpu_pm: u32,
    /// Temps processeur total (ns).
    pub cpu_ns: u64,
    /// Mémoire résidente (processus) ou pile du noyau (tâches), en octets.
    pub mem: u64,
    pub threads: usize,
    pub core: Option<usize>,
    pub nice: i8,
    pub class: &'static str,
    pub weight: u32,
    pub slice_ns: u64,
    pub burst_ns: u64,
    pub pinned: Option<u32>,
    pub pgid: usize,
    pub sid: usize,
    /// Âge du processus (ns).
    pub age_ns: u64,
}

/// Mémoire.
#[derive(Clone, Copy, Default)]
pub struct Mem {
    pub total: u64,
    pub free: u64,
    pub zero_pool: u64,
    pub huge_free: u64,
    pub reserved_free: u64,
    pub allocator: &'static str,
    pub allocs_ps: u64,
    pub frees_ps: u64,
    pub failures: u64,
    pub zeroed_hits: u64,
    pub zeroed_misses: u64,
    /// Tas du noyau.
    pub heap_segments: usize,
    pub heap_pool: usize,
    pub heap_large: usize,
    pub heap_large_bytes: u64,
    pub heap_allocs_ps: u64,
    pub heap_remote_ps: u64,
    /// Somme des mémoires résidentes des processus.
    pub user_rss: u64,
}

impl Mem {
    pub fn used(&self) -> u64 { self.total.saturating_sub(self.free) }
    pub fn used_pm(&self) -> u32 { if self.total == 0 { 0 } else { (self.used() * 1000 / self.total) as u32 } }
}

/// Affichage : GPU (s'il y en a un) et MaiCompositor, débits sur la dernière période.
#[derive(Clone, Default)]
pub struct Gpu {
    /// Identité et compteurs cumulés du GPU (`None` : framebuffer du chargeur).
    pub report: Option<mgi::GpuReport>,
    /// Résolution de l'écran.
    pub screen: (usize, usize),
    /// Utilisation de l'appareil (‰).
    pub busy_pm: u32,
    pub presents_ps: u64,
    pub transfer_bps: u64,
    pub commands_ps: u64,
    pub interrupts_ps: u64,
    pub cursor_moves_ps: u64,
    /// Latence « soumission → fence » moyenne des présentations de la période.
    pub present_lat_ns: u64,
    /// Images composées par seconde, temps moyen d'une composition, part d'un cœur (‰).
    pub frames_ps: u64,
    pub compose_avg_ns: u64,
    pub compose_pm: u32,
    pub pixels_ps: u64,
    /// Tracés du curseur logiciel par seconde (0 quand le plan matériel sert).
    pub sw_cursor_ps: u64,
}

impl Gpu {
    /// Valeur du graphe principal : utilisation du GPU, ou composition sans GPU.
    pub fn main_pm(&self) -> u32 {
        if self.report.is_some() { self.busy_pm } else { self.compose_pm }
    }
}

/// Compteurs cumulés de l'affichage au dernier échantillon.
#[derive(Default)]
struct GpuPrev {
    busy_ns: u64,
    presents: u64,
    transfer_bytes: u64,
    commands: u64,
    interrupts: u64,
    cursor_moves: u64,
    lat_total_ns: u64,
    lat_count: u64,
    frames: u64,
    compose_ns: u64,
    pixels: u64,
    sw_cursor: u64,
}

/// Compteurs d'un cœur au dernier échantillon : inactivité, pression, horloge, compteurs
/// d'événements, allocations du tas, intégrale des tâches prêtes.
type CpuPrev = (u64, u64, u64, [u64; 6], u64, u64);

/// Valeurs précédentes servant aux différences.
#[derive(Default)]
struct Prev {
    now_ns: u64,
    cpu: BTreeMap<usize, CpuPrev>,
    task_exec: BTreeMap<usize, u64>,
    proc_exec: BTreeMap<usize, u64>,
    frame_allocs: u64,
    frame_frees: u64,
    heap_allocs: u64,
    heap_remote: u64,
    gpu: GpuPrev,
}

/// Échantillonneur.
pub struct Sampler {
    prev: Prev,
    pub cores: Vec<Core>,
    pub entries: Vec<Entry>,
    pub mem: Mem,
    /// Utilisation totale (‰) et historiques.
    pub busy_pm: u32,
    pub hist_cpu: History<HIST>,
    pub hist_mem: History<HIST>,
    pub hist_pressure: History<HIST>,
    pub hist_heap: History<HIST>,
    /// Affichage et historiques (GPU ou composition, composition seule).
    pub gpu: Gpu,
    pub hist_gpu: History<HIST>,
    pub hist_compose: History<HIST>,
    /// Charge moyenne 1/5/15 min (virgule fixe 11 bits).
    pub load: [u64; 3],
    /// Pression `some` 10 s / 60 s / 300 s (‰ en virgule fixe 11 bits).
    pub psi: [u64; 3],
    pub pressure_pm: u32,
    pub switches_ps: u64,
    pub wakeups_ps: u64,
    pub migrations_ps: u64,
    pub nr_tasks: usize,
    pub nr_processes: usize,
    pub nr_threads: usize,
    pub uptime_ns: u64,
    /// Période d'échantillonnage (s).
    pub period_s: u32,
    pub samples: u64,
}

fn class_name(c: maisched::Class) -> &'static str {
    match c {
        maisched::Class::Fair { batch: false } => "EEVDF",
        maisched::Class::Fair { batch: true } => "Fond",
        maisched::Class::Rt { rr: true, .. } => "RT tourniquet",
        maisched::Class::Rt { rr: false, .. } => "RT FIFO",
        maisched::Class::Idle => "Inactive",
    }
}

fn delta_per_s(now: u64, before: u64, dt_ns: u64) -> u64 {
    // Arrondi au plus proche : une période d'échantillonnage un peu plus longue qu'une seconde
    // ne doit pas afficher 0 présentation/s à côté d'un débit d'un plein écran par seconde.
    if dt_ns == 0 { 0 } else { ((now.saturating_sub(before) as u128 * 1_000_000_000 + dt_ns as u128 / 2) / dt_ns as u128) as u64 }
}

impl Sampler {
    pub fn new() -> Sampler {
        let mut s = Sampler {
            prev: Prev::default(),
            cores: Vec::new(),
            entries: Vec::new(),
            mem: Mem::default(),
            busy_pm: 0,
            hist_cpu: History::default(),
            hist_mem: History::default(),
            hist_pressure: History::default(),
            hist_heap: History::default(),
            gpu: Gpu::default(),
            hist_gpu: History::default(),
            hist_compose: History::default(),
            load: [0; 3],
            psi: [0; 3],
            pressure_pm: 0,
            switches_ps: 0,
            wakeups_ps: 0,
            migrations_ps: 0,
            nr_tasks: 0,
            nr_processes: 0,
            nr_threads: 0,
            uptime_ns: 0,
            period_s: 1,
            samples: 0,
        };
        // Premier passage : il ne fait que poser les références des différences.
        s.sample();
        s
    }

    /// Prend un échantillon.
    pub fn sample(&mut self) {
        let stats = task::scheduler::stats();
        let now = stats.first().map(|s| s.now_ns).unwrap_or_else(task::scheduler::clock_ns);
        let dt = now.saturating_sub(self.prev.now_ns);
        let first = self.prev.now_ns == 0;
        let ncpu = stats.len().max(1) as u64;
        self.uptime_ns = now;

        // ── Cœurs ────────────────────────────────────────────────
        let mut busy_sum = 0u64;
        let mut pressure_sum = 0u64;
        // Charge moyenne : tâches prêtes moyennées sur la période (×100), à partir de l'intégrale
        // tenue par le tick de l'ordonnanceur (comme `calc_load` de Linux, mais pondérée dans le
        // temps au lieu d'un instantané toutes les 5 s).
        let mut runnable_x100 = 0u64;
        let (mut sw, mut wk, mut mg) = (0u64, 0u64, 0u64);
        let mut cores = Vec::with_capacity(stats.len());
        for st in &stats {
            let mut core = self.cores.iter().find(|c| c.id == st.cpu).cloned().unwrap_or_else(|| Core { id: st.cpu, ..Core::default() });
            let counters = [st.switches, st.wakeups, st.remote_wakeups, st.migrations, st.tick_preemptions, st.wakeup_preemptions];
            let heap_allocs = heap::cpu_stats(st.cpu).map_or(0, |h| h.allocs);
            if let Some(&(p_idle, p_cont, p_now, p_cnt, p_heap, p_run)) = self.prev.cpu.get(&st.cpu) {
                let d = st.now_ns.saturating_sub(p_now).max(1);
                let idle = st.idle_ns.saturating_sub(p_idle).min(d);
                core.busy_pm = (1000 - idle * 1000 / d) as u32;
                core.pressure_pm = (st.contended_ns.saturating_sub(p_cont).min(d) * 1000 / d) as u32;
                core.switches_ps = delta_per_s(counters[0], p_cnt[0], d);
                core.wakeups_ps = delta_per_s(counters[1], p_cnt[1], d);
                core.remote_wakeups_ps = delta_per_s(counters[2], p_cnt[2], d);
                core.migrations_ps = delta_per_s(counters[3], p_cnt[3], d);
                core.tick_preempt_ps = delta_per_s(counters[4], p_cnt[4], d);
                core.wake_preempt_ps = delta_per_s(counters[5], p_cnt[5], d);
                core.heap_allocs_ps = delta_per_s(heap_allocs, p_heap, d);
                core.runnable_x100 = (st.runnable_ns.saturating_sub(p_run) as u128 * 100 / d as u128) as u64;
            }
            core.nr_running = st.nr_running;
            core.wake_lat_avg_ns = st.wake_lat_avg_ns;
            core.wake_lat_max_ns = st.wake_lat_max_ns;
            if !first {
                core.hist.push(core.busy_pm);
            }
            busy_sum += core.busy_pm as u64;
            pressure_sum += core.pressure_pm as u64;
            runnable_x100 += core.runnable_x100;
            sw += core.switches_ps;
            wk += core.wakeups_ps;
            mg += core.migrations_ps;
            self.prev.cpu.insert(st.cpu, (st.idle_ns, st.contended_ns, st.now_ns, counters, heap_allocs, st.runnable_ns));
            cores.push(core);
        }
        self.cores = cores;
        self.busy_pm = (busy_sum / ncpu) as u32;
        self.pressure_pm = (pressure_sum / ncpu) as u32;
        self.switches_ps = sw;
        self.wakeups_ps = wk;
        self.migrations_ps = mg;

        // ── Charge moyenne et pression ──────────────────────────
        if !first {
            let secs = ((dt + 500_000_000) / 1_000_000_000).max(1) as u32;
            let n = runnable_x100 * FIXED_1 / 100;
            if self.samples == 0 {
                self.load = [n; 3];
                self.psi = [self.pressure_pm as u64 * FIXED_1; 3];
            } else {
                self.load[0] = ewma(self.load[0], n, decay(EXP_1M, secs));
                self.load[1] = ewma(self.load[1], n, decay(EXP_5M, secs));
                self.load[2] = ewma(self.load[2], n, decay(EXP_15M, secs));
                let p = self.pressure_pm as u64 * FIXED_1;
                self.psi[0] = ewma(self.psi[0], p, decay(EXP_10S, secs));
                self.psi[1] = ewma(self.psi[1], p, decay(EXP_1M, secs));
                self.psi[2] = ewma(self.psi[2], p, decay(EXP_5M, secs));
            }
            self.hist_cpu.push(self.busy_pm);
            self.hist_pressure.push(self.pressure_pm);
            self.samples += 1;
        }

        // ── Processus Linux ─────────────────────────────────────
        let mut entries = Vec::new();
        let mut thread_tasks: BTreeMap<usize, ()> = BTreeMap::new();
        let mut user_rss = 0u64;
        let mut new_proc_exec = BTreeMap::new();
        let mut new_task_exec = BTreeMap::new();
        let machine_ns = dt.max(1) as u128 * ncpu as u128;
        let pm_of = |d: u64| -> u32 { if first { 0 } else { ((d as u128 * 1000 / machine_ns) as u32).min(1000) } };
        #[cfg(target_arch = "x86_64")]
        {
            let procs = maiproc::all_processes();
            self.nr_processes = procs.len();
            self.nr_threads = 0;
            for p in &procs {
                collect_process(p, now, &mut entries, &mut thread_tasks, &mut user_rss, &self.prev, &mut new_proc_exec, &mut new_task_exec, &pm_of);
                self.nr_threads += p.live_threads.load(core::sync::atomic::Ordering::Relaxed);
            }
        }

        // ── Tâches de l'ordonnanceur ────────────────────────────
        let tasks = task::all_tasks();
        self.nr_tasks = tasks.len();
        for (id, t) in &tasks {
            if thread_tasks.contains_key(id) {
                continue;
            }
            let snap = t.sched.snapshot();
            let exec = snap.sum_exec_ns;
            let before = self.prev.task_exec.get(id).copied().unwrap_or(exec);
            new_task_exec.insert(*id, exec);
            let core = Option::<cpu::CpuId>::from(t.running_on_cpu.load()).map(|c| c.value() as usize);
            let kind = if t.is_an_idle_task {
                Kind::Idle
            } else if t.app_crate.is_some() {
                Kind::App
            } else {
                Kind::Kernel
            };
            let path = t.app_crate.as_ref().map(|a| alloc::format!("{}", a.lock_as_ref().crate_name)).unwrap_or_default();
            let mut cpu_pm = pm_of(exec.saturating_sub(before));
            if kind == Kind::Idle {
                // Le temps de la tâche inactive n'est pas compté par l'ordonnanceur comme du
                // travail : on affiche l'inactivité mesurée du cœur.
                let c = snap.cpu;
                cpu_pm = self.cores.iter().find(|x| x.id == c).map_or(0, |x| (1000 - x.busy_pm) / ncpu as u32);
            }
            entries.push(Entry {
                kind,
                id: *id,
                task_id: Some(*id),
                parent: None,
                name: if t.is_an_idle_task {
                    alloc::format!("Inactivité du cœur {}", snap.cpu)
                } else if kind == Kind::App {
                    app_name(&t.name)
                } else {
                    t.name.clone()
                },
                path,
                state: task_state(t.runstate.load(), core.is_some(), snap.on_rq),
                cpu_pm,
                cpu_ns: exec,
                mem: t.kstack.size_in_bytes() as u64,
                threads: 1,
                core: core.or(Some(snap.cpu)),
                nice: snap.nice,
                class: class_name(snap.class),
                weight: snap.weight,
                slice_ns: snap.slice_ns,
                burst_ns: snap.burst_ewma_ns,
                pinned: Option::<cpu::CpuId>::from(t.pinned_core.load()).map(|c| c.value()),
                pgid: 0,
                sid: 0,
                age_ns: 0,
            });
        }
        self.prev.task_exec = new_task_exec;
        self.prev.proc_exec = new_proc_exec;
        self.entries = entries;

        // ── Mémoire ─────────────────────────────────────────────
        let fa = frame_allocator::stats();
        let hs = heap::stats();
        let mut mem = Mem {
            total: fa.total_frames as u64 * 4096,
            free: fa.free_frames as u64 * 4096,
            zero_pool: fa.zeroed_pool_frames as u64 * 4096,
            huge_free: fa.free_huge_2m as u64 * 2 * 1024 * 1024,
            reserved_free: fa.free_reserved_frames as u64 * 4096,
            allocator: fa.allocator,
            failures: fa.events.failures,
            zeroed_hits: fa.events.zeroed_hits,
            zeroed_misses: fa.events.zeroed_misses,
            heap_segments: hs.segments_mapped,
            heap_pool: hs.kalloc.pool,
            heap_large: hs.large_count,
            heap_large_bytes: hs.large_bytes as u64,
            user_rss,
            ..Mem::default()
        };
        if !first {
            mem.allocs_ps = delta_per_s(fa.events.allocations, self.prev.frame_allocs, dt);
            mem.frees_ps = delta_per_s(fa.events.deallocations, self.prev.frame_frees, dt);
            mem.heap_allocs_ps = delta_per_s(hs.kalloc.allocs, self.prev.heap_allocs, dt);
            mem.heap_remote_ps = delta_per_s(hs.kalloc.remote_frees, self.prev.heap_remote, dt);
            self.hist_mem.push(mem.used_pm());
            self.hist_heap.push(((hs.segments_mapped as u64 * 2 * 1024 * 1024 + hs.large_bytes as u64) / 1024) as u32);
        }
        self.prev.frame_allocs = fa.events.allocations;
        self.prev.frame_frees = fa.events.deallocations;
        self.prev.heap_allocs = hs.kalloc.allocs;
        self.prev.heap_remote = hs.kalloc.remote_frees;
        self.mem = mem;

        // ── Affichage ───────────────────────────────────────────
        self.sample_gpu(first, dt);
        self.prev.now_ns = now;
    }

    fn sample_gpu(&mut self, first: bool, dt: u64) {
        let report = window_manager::gpu_report();
        let wm = window_manager::stats();
        let p = &self.prev.gpu;
        let mut g = Gpu { screen: window_manager::screen_size(), ..Gpu::default() };
        let d = |now: u64, before: u64| delta_per_s(now, before, dt);
        if !first && dt > 0 {
            if let Some(r) = &report {
                g.busy_pm = (r.busy_ns.saturating_sub(p.busy_ns).min(dt) * 1000 / dt) as u32;
                g.presents_ps = d(r.presents, p.presents);
                g.transfer_bps = d(r.transfer_bytes, p.transfer_bytes);
                g.commands_ps = d(r.commands, p.commands);
                g.interrupts_ps = d(r.interrupts, p.interrupts);
                g.cursor_moves_ps = d(r.cursor_moves, p.cursor_moves);
                let n = r.present_lat_count.saturating_sub(p.lat_count);
                if n > 0 {
                    g.present_lat_ns = r.present_lat_total_ns.saturating_sub(p.lat_total_ns) / n;
                }
            }
            let frames = wm.frames.saturating_sub(p.frames);
            let compose = wm.compose_ns.saturating_sub(p.compose_ns);
            g.frames_ps = d(wm.frames, p.frames);
            g.compose_avg_ns = if frames > 0 { compose / frames } else { 0 };
            g.compose_pm = (compose.min(dt) * 1000 / dt) as u32;
            g.pixels_ps = d(wm.pixels, p.pixels);
            g.sw_cursor_ps = d(wm.sw_cursor_draws, p.sw_cursor);
        }
        let mut np = GpuPrev {
            frames: wm.frames,
            compose_ns: wm.compose_ns,
            pixels: wm.pixels,
            sw_cursor: wm.sw_cursor_draws,
            ..GpuPrev::default()
        };
        if let Some(r) = &report {
            np.busy_ns = r.busy_ns;
            np.presents = r.presents;
            np.transfer_bytes = r.transfer_bytes;
            np.commands = r.commands;
            np.interrupts = r.interrupts;
            np.cursor_moves = r.cursor_moves;
            np.lat_total_ns = r.present_lat_total_ns;
            np.lat_count = r.present_lat_count;
        }
        self.prev.gpu = np;
        g.report = report;
        if !first {
            self.hist_gpu.push(g.main_pm());
            self.hist_compose.push(g.compose_pm);
        }
        self.gpu = g;
    }

    /// Charge moyenne `i` (0 : 1 min…) en centièmes.
    pub fn load_x100(&self, i: usize) -> u64 { (self.load[i] * 100) >> FSHIFT }

    /// Pression `i` (0 : 10 s…) en ‰.
    pub fn psi_pm(&self, i: usize) -> u32 { (self.psi[i] >> FSHIFT) as u32 }
}

/// Nom lisible d'une tâche d'application : `first_/namespaces/_applications/explorer-af94…`
/// → `explorer`, `task_manager-c529dd59c52a6f51` → `task_manager`.
fn app_name(raw: &str) -> String {
    let base = raw.rsplit('/').next().unwrap_or(raw);
    let base = base.strip_suffix(".o").unwrap_or(base);
    match base.rsplit_once('-') {
        Some((stem, hash)) if hash.len() >= 8 && hash.chars().all(|c| c.is_ascii_hexdigit()) => String::from(stem),
        _ => String::from(base),
    }
}

fn task_state(rs: RunState, on_cpu: bool, on_rq: bool) -> State {
    match rs {
        RunState::Runnable if on_cpu => State::Running,
        RunState::Runnable => State::Ready,
        RunState::Blocked if on_rq && on_cpu => State::Running,
        RunState::Blocked => State::Sleeping,
        RunState::Initing => State::Starting,
        RunState::Exited(_) => State::Exited,
    }
}

#[cfg(target_arch = "x86_64")]
#[allow(clippy::too_many_arguments)]
fn collect_process(
    p: &Arc<maiproc::Process>,
    _now: u64,
    entries: &mut Vec<Entry>,
    thread_tasks: &mut BTreeMap<usize, ()>,
    user_rss: &mut u64,
    prev: &Prev,
    new_proc_exec: &mut BTreeMap<usize, u64>,
    new_task_exec: &mut BTreeMap<usize, u64>,
    pm_of: &dyn Fn(u64) -> u32,
) {
    use core::sync::atomic::Ordering::Relaxed;
    let exec = p.cpu_ns();
    let before = prev.proc_exec.get(&p.pid).copied().unwrap_or(exec);
    new_proc_exec.insert(p.pid, exec);
    let rss = p.mm().map_or(0, |m| m.rss() as u64 * 4096);
    *user_rss += rss;
    let threads = p.threads();
    let mut any_running = false;
    let mut any_ready = false;
    let mut thread_entries = Vec::with_capacity(threads.len());
    for th in &threads {
        let Some(t) = th.task() else { continue };
        thread_tasks.insert(t.id, ());
        let snap = t.sched.snapshot();
        let texec = snap.sum_exec_ns;
        let tbefore = prev.task_exec.get(&t.id).copied().unwrap_or(texec);
        new_task_exec.insert(t.id, texec);
        let core = Option::<cpu::CpuId>::from(t.running_on_cpu.load()).map(|c| c.value() as usize);
        let st = task_state(t.runstate.load(), core.is_some(), snap.on_rq);
        any_running |= st == State::Running;
        any_ready |= st == State::Ready;
        thread_entries.push(Entry {
            kind: Kind::Thread,
            id: th.tid(),
            task_id: Some(t.id),
            parent: Some(p.pid),
            name: t.name.clone(),
            path: String::new(),
            state: if p.is_stopped() { State::Stopped } else { st },
            cpu_pm: pm_of(texec.saturating_sub(tbefore)),
            cpu_ns: texec,
            mem: 0,
            threads: 1,
            core: core.or(Some(snap.cpu)),
            nice: snap.nice,
            class: class_name(snap.class),
            weight: snap.weight,
            slice_ns: snap.slice_ns,
            burst_ns: snap.burst_ewma_ns,
            pinned: Option::<cpu::CpuId>::from(t.pinned_core.load()).map(|c| c.value()),
            pgid: 0,
            sid: 0,
            age_ns: 0,
        });
    }
    let state = if p.is_stopped() {
        State::Stopped
    } else if any_running {
        State::Running
    } else if any_ready {
        State::Ready
    } else {
        State::Sleeping
    };
    let first_task = thread_entries.first().map(|e| (e.nice, e.class, e.weight, e.slice_ns, e.burst_ns, e.core));
    let (nice, class, weight, slice_ns, burst_ns, core) = first_task.unwrap_or((0, "EEVDF", 0, 0, 0, None));
    let ppid = p.ppid.load(Relaxed);
    entries.push(Entry {
        kind: Kind::Process,
        id: p.pid,
        task_id: thread_entries.first().and_then(|e| e.task_id),
        parent: (ppid != 0).then_some(ppid),
        name: p.comm_string(),
        path: p.exe.lock().clone(),
        state,
        cpu_pm: pm_of(exec.saturating_sub(before)),
        cpu_ns: exec,
        mem: rss,
        threads: threads.len(),
        core,
        nice,
        class,
        weight,
        slice_ns,
        burst_ns,
        pinned: None,
        pgid: p.pgid.load(Relaxed),
        sid: p.sid.load(Relaxed),
        age_ns: p.started.elapsed().as_nanos() as u64,
    });
    entries.extend(thread_entries);
}
