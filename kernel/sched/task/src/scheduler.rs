//! MaiSched, partie noyau : files par cœur, `schedule()`, réveil, tick, équilibrage.
//!
//! Les algorithmes (EEVDF, temps réel) sont dans la crate `maisched`, sans dépendance et testée
//! sur l'hôte. Ce module s'occupe de la concurrence entre cœurs.
//!
//! ## Invariants
//! * Une entité appartient à la file du cœur `task.sched.cpu` et n'est lue ou modifiée que sous
//!   le verrou de cette file (verrou par cœur, interruptions coupées ; aucun verrou global).
//! * La file détient une référence forte sur chaque tâche `on_rq` (`entity.owner`, issu de
//!   `Arc::into_raw`) : une tâche prête ne peut pas être perdue ni libérée.
//! * `running_on_cpu` est posé avant le changement de contexte et effacé *après*, par le cœur
//!   d'arrivée (`finish_task_switch`). Le réveil attend cet effacement avant de remettre une tâche
//!   en file ailleurs, et le vol ignore les tâches encore sur un CPU (Linux `on_cpu`).
//! * `block()` ne retire pas la tâche courante : `schedule()` relit son état sous le verrou et la
//!   retire ou la garde. Un réveil concurrent, qui vérifie `on_rq` sous le même verrou, est donc
//!   toujours vu : pas de réveil perdu.
//!
//! Le plan, les mesures et les références sont dans `.claude/plans/03-maisched-ordonnanceur.md`.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::cell::UnsafeCell;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering::{AcqRel, Acquire, Relaxed, Release}};

use cpu::CpuId;
use irq_safety::{hold_interrupts, HeldInterrupts};
use maisched::{cpumask::AtomicCpuMask, flags, topology::CpuTopology, Class, Dequeued, Entity, FairRq, RtRq};
use task_struct::{RunState, Task};

use crate::TaskRef;

/// Nombre maximal de cœurs gérés.
pub const MAX_CPUS: usize = 256;

/// Période (en ticks) du tirage de charge entre cœurs occupés.
const BALANCE_TICKS: u64 = 8;

// ---------------------------------------------------------------------------
// Horloge : TSC converti en ns par multiplication (pas de division sur le chemin chaud)
// ---------------------------------------------------------------------------

/// `ns = cycles × TSC_MULT >> 32`. 0 tant que l'étalonnage n'a pas eu lieu (on suppose 1 cycle = 1 ns).
static TSC_MULT: AtomicU64 = AtomicU64::new(0);

/// Étalonnage de l'horloge de l'ordonnanceur : nombre de cycles du compteur par milliseconde.
pub fn set_clock_ticks_per_ms(ticks_per_ms: u64) {
    if ticks_per_ms > 0 {
        TSC_MULT.store((1_000_000u64 << 32) / ticks_per_ms, Relaxed);
    }
}

#[inline(always)]
fn read_counter() -> u64 {
    #[cfg(target_arch = "x86_64")]
    // SAFETY : lecture du TSC, sans effet de bord.
    unsafe { core::arch::x86_64::_rdtsc() }
    #[cfg(target_arch = "aarch64")]
    {
        let v: u64;
        // SAFETY : lecture du compteur virtuel générique.
        unsafe { core::arch::asm!("mrs {}, cntvct_el0", out(reg) v) };
        v
    }
}

/// Horloge monotone de l'ordonnanceur, en ns.
#[inline]
pub fn clock_ns() -> u64 {
    let m = TSC_MULT.load(Relaxed);
    let c = read_counter();
    if m == 0 { c } else { ((c as u128 * m as u128) >> 32) as u64 }
}

// ---------------------------------------------------------------------------
// Statistiques par cœur
// ---------------------------------------------------------------------------

/// Compteurs d'un cœur (lecture sans verrou).
pub struct CpuCounters {
    pub ticks: AtomicU64,
    pub switches: AtomicU64,
    pub wakeups: AtomicU64,
    pub remote_wakeups: AtomicU64,
    pub migrations: AtomicU64,
    pub tick_preemptions: AtomicU64,
    pub wakeup_preemptions: AtomicU64,
    pub idle_entries: AtomicU64,
    pub wake_lat_sum_ns: AtomicU64,
    pub wake_lat_count: AtomicU64,
    pub wake_lat_max_ns: AtomicU64,
}

impl CpuCounters {
    const fn new() -> CpuCounters {
        CpuCounters {
            ticks: AtomicU64::new(0),
            switches: AtomicU64::new(0),
            wakeups: AtomicU64::new(0),
            remote_wakeups: AtomicU64::new(0),
            migrations: AtomicU64::new(0),
            tick_preemptions: AtomicU64::new(0),
            wakeup_preemptions: AtomicU64::new(0),
            idle_entries: AtomicU64::new(0),
            wake_lat_sum_ns: AtomicU64::new(0),
            wake_lat_count: AtomicU64::new(0),
            wake_lat_max_ns: AtomicU64::new(0),
        }
    }
}

/// Copie des statistiques d'un cœur.
#[derive(Clone, Copy, Debug, Default)]
pub struct SchedStats {
    pub cpu: usize,
    pub nr_running: u32,
    pub idle: bool,
    pub ticks: u64,
    pub switches: u64,
    pub wakeups: u64,
    pub remote_wakeups: u64,
    pub migrations: u64,
    pub tick_preemptions: u64,
    pub wakeup_preemptions: u64,
    pub idle_entries: u64,
    pub wake_lat_avg_ns: u64,
    pub wake_lat_max_ns: u64,
}

// ---------------------------------------------------------------------------
// Files par cœur
// ---------------------------------------------------------------------------

struct RqInner {
    fair: FairRq,
    rt: RtRq,
    /// Tâche inactive du cœur (jamais dans une file).
    idle: Option<TaskRef>,
    /// Tâche en cours d'exécution selon l'ordonnanceur (pointeur ; la référence est `owner`).
    curr: *const Task,
    last_tick_ns: u64,
    ticks: u64,
}

/// File d'un cœur, alignée sur une ligne de cache : les champs publiés (`nr_running`…) sont lus par
/// les autres cœurs sans verrou.
#[repr(C, align(64))]
pub struct Rq {
    locked: AtomicBool,
    online: AtomicBool,
    need_resched: AtomicBool,
    nr_running: AtomicU32,
    inner: UnsafeCell<RqInner>,
    counters: CpuCounters,
}

// SAFETY : `inner` n'est accédé que sous `locked`.
unsafe impl Sync for Rq {}

impl Rq {
    const fn new() -> Rq {
        Rq {
            locked: AtomicBool::new(false),
            online: AtomicBool::new(false),
            need_resched: AtomicBool::new(false),
            nr_running: AtomicU32::new(0),
            inner: UnsafeCell::new(RqInner {
                fair: FairRq::new(),
                rt: RtRq::new(),
                idle: None,
                curr: ptr::null(),
                last_tick_ns: 0,
                ticks: 0,
            }),
            counters: CpuCounters::new(),
        }
    }

    #[inline]
    fn raw_lock(&self) {
        while self.locked.compare_exchange_weak(false, true, Acquire, Relaxed).is_err() {
            while self.locked.load(Relaxed) {
                core::hint::spin_loop();
            }
        }
    }

    #[inline]
    fn raw_unlock(&self) {
        self.locked.store(false, Release);
    }

    /// Prend le verrou, interruptions coupées.
    #[inline]
    fn lock(&self) -> RqGuard<'_> {
        let irq = hold_interrupts();
        self.raw_lock();
        RqGuard { rq: self, _irq: irq }
    }

    #[inline]
    fn publish(&self, g: &RqInner) {
        self.nr_running.store(g.fair.nr_runnable() + g.rt.nr_runnable(), Release);
    }
}

struct RqGuard<'a> {
    rq: &'a Rq,
    _irq: HeldInterrupts,
}

impl Drop for RqGuard<'_> {
    fn drop(&mut self) {
        // Déverrouille avant que `_irq` ne rétablisse les interruptions.
        self.rq.raw_unlock();
    }
}

impl core::ops::Deref for RqGuard<'_> {
    type Target = RqInner;
    fn deref(&self) -> &RqInner {
        // SAFETY : verrou tenu.
        unsafe { &*self.rq.inner.get() }
    }
}

impl core::ops::DerefMut for RqGuard<'_> {
    fn deref_mut(&mut self) -> &mut RqInner {
        // SAFETY : verrou tenu.
        unsafe { &mut *self.rq.inner.get() }
    }
}

/// Deux files verrouillées dans l'ordre des indices (pas d'interblocage entre voleurs).
struct DoubleGuard<'a> {
    a: &'a Rq,
    b: &'a Rq,
    _irq: HeldInterrupts,
}

impl<'a> DoubleGuard<'a> {
    fn lock(x: usize, y: usize) -> DoubleGuard<'a> {
        debug_assert_ne!(x, y);
        let irq = hold_interrupts();
        let (lo, hi) = if x < y { (x, y) } else { (y, x) };
        RQS[lo].raw_lock();
        RQS[hi].raw_lock();
        DoubleGuard { a: &RQS[x], b: &RQS[y], _irq: irq }
    }
    #[allow(clippy::mut_from_ref)]
    fn first(&self) -> &mut RqInner {
        // SAFETY : verrou tenu.
        unsafe { &mut *self.a.inner.get() }
    }
    #[allow(clippy::mut_from_ref)]
    fn second(&self) -> &mut RqInner {
        // SAFETY : verrou tenu.
        unsafe { &mut *self.b.inner.get() }
    }
}

impl Drop for DoubleGuard<'_> {
    fn drop(&mut self) {
        self.a.raw_unlock();
        self.b.raw_unlock();
    }
}

#[allow(clippy::declare_interior_mutable_const)]
const RQ_INIT: Rq = Rq::new();
static RQS: [Rq; MAX_CPUS] = [RQ_INIT; MAX_CPUS];

/// Cœurs dont la tâche courante est la tâche inactive.
static IDLE_CPUS: AtomicCpuMask = AtomicCpuMask::new();
/// Nombre de cœurs gérés.
static NR_CPUS: AtomicUsize = AtomicUsize::new(1);
/// Topologie (fuitée volontairement : elle vit autant que le noyau).
static TOPOLOGY: AtomicPtr<CpuTopology> = AtomicPtr::new(ptr::null_mut());
/// Envoi de l'IPI de replanification (enregistré par la crate `scheduler`).
static RESCHED_IPI: AtomicUsize = AtomicUsize::new(0);

/// Fixe le nombre de cœurs gérés.
pub fn set_cpu_count(n: usize) {
    NR_CPUS.store(n.clamp(1, MAX_CPUS), Release);
}

/// Installe la topologie des caches (ordres de vol et de placement).
pub fn set_topology(topology: CpuTopology) {
    let old = TOPOLOGY.swap(Box::into_raw(Box::new(topology)), AcqRel);
    // L'ancienne peut encore être lue par un autre cœur : on la laisse fuir (une fois au boot).
    let _ = old;
}

fn topology() -> Option<&'static CpuTopology> {
    let p = TOPOLOGY.load(Acquire);
    // SAFETY : jamais libérée.
    unsafe { p.as_ref() }
}

/// Enregistre la fonction qui envoie l'IPI de replanification à un cœur.
pub fn set_resched_ipi(f: fn(usize)) {
    RESCHED_IPI.store(f as usize, Release);
}

/// Demande au cœur `cpu` de replanifier dès que possible (IPI, qui réveille aussi un `hlt`).
fn kick(cpu: usize) {
    let p = RESCHED_IPI.load(Acquire);
    if p != 0 {
        // SAFETY : seule une `fn(usize)` est enregistrée.
        let f: fn(usize) = unsafe { core::mem::transmute(p) };
        f(cpu);
    }
}

#[inline]
fn nr_cpus() -> usize {
    NR_CPUS.load(Relaxed)
}

#[inline]
fn this_cpu() -> usize {
    (cpu::current_cpu().value() as usize).min(MAX_CPUS - 1)
}

// ---------------------------------------------------------------------------
// Entités et références
// ---------------------------------------------------------------------------

#[inline(always)]
fn ent(t: &Task) -> *mut Entity {
    t.sched.entity_ptr()
}

/// La file prend une référence forte sur la tâche.
unsafe fn hold_ref(t: &TaskRef, e: *mut Entity) {
    debug_assert_eq!((*e).owner, 0);
    (*e).owner = Arc::into_raw(t.0.clone()) as usize;
}

/// La file rend sa référence.
unsafe fn take_ref(e: *mut Entity) -> TaskRef {
    let p = (*e).owner as *const Task;
    (*e).owner = 0;
    TaskRef(Arc::from_raw(p))
}

/// Nouvelle référence vers la tâche d'une entité en file.
unsafe fn clone_ref(e: *mut Entity) -> TaskRef {
    let p = (*e).owner as *const Task;
    Arc::increment_strong_count(p);
    TaskRef(Arc::from_raw(p))
}

unsafe fn task_of<'a>(e: *mut Entity) -> &'a Task {
    &*((*e).owner as *const Task)
}

fn pinned_cpu(t: &Task) -> Option<usize> {
    Option::<CpuId>::from(t.pinned_core.load()).map(|c| c.value() as usize)
}

fn allowed_on(t: &Task, cpu: usize) -> bool {
    pinned_cpu(t).map_or(true, |p| p == cpu)
}

/// Met une entité dans la file de sa classe.
unsafe fn enqueue_entity(g: &mut RqInner, e: *mut Entity, fl: u32, now: u64) {
    match (*e).class {
        Class::Fair { .. } => g.fair.enqueue(e, fl, now),
        Class::Rt { .. } => g.rt.enqueue(e),
        Class::Idle => {}
    }
}

/// Retire une entité en attente (pas la courante) de la file de sa classe.
unsafe fn dequeue_queued(g: &mut RqInner, e: *mut Entity, now: u64) {
    match (*e).class {
        Class::Fair { .. } => g.fair.dequeue_queued(e, now),
        Class::Rt { .. } => g.rt.dequeue_queued(e),
        Class::Idle => {}
    }
}

/// La tâche qui vient d'entrer (`e`) doit-elle préempter la courante ?
unsafe fn check_preempt(g: &mut RqInner, e: *mut Entity, now: u64) -> bool {
    let curr = g.curr;
    if curr.is_null() || (*curr).is_an_idle_task {
        return true;
    }
    let ce = ent(&*curr);
    match ((*e).class, (*ce).class) {
        (Class::Rt { .. }, Class::Rt { .. }) => g.rt.should_preempt(e),
        (Class::Rt { .. }, _) => true,
        (Class::Fair { .. }, Class::Fair { .. }) => g.fair.should_preempt(e, now),
        _ => false,
    }
}

/// Applique un changement de classe en attente à une entité qui n'est pas la courante.
unsafe fn apply_class(e: *mut Entity, class: Class) {
    (*e).class = class;
    (*e).recompute_slice();
}

// ---------------------------------------------------------------------------
// schedule()
// ---------------------------------------------------------------------------

/// Cède le CPU (abandon volontaire, comme `sched_yield`) : la tâche courante, si elle reste prête,
/// repasse derrière les autres. Si elle s'est bloquée ou est sortie, elle quitte la file.
///
/// Renvoie `true` si une autre tâche a été choisie.
#[doc(alias("yield"))]
pub fn schedule() -> bool {
    schedule_inner(true)
}

/// Replanification demandée par l'ordonnanceur (fin de tranche, préemption au réveil) : la tâche
/// courante garde sa place.
pub fn schedule_preempt() -> bool {
    schedule_inner(false)
}

fn schedule_inner(yielded: bool) -> bool {
    let preemption_guard = preemption::hold_preemption();
    if !preemption_guard.preemption_was_enabled() {
        return false;
    }
    let cpu_id = preemption_guard.cpu_id();
    let cpu = cpu_id.value() as usize;
    if cpu >= MAX_CPUS {
        return false;
    }
    let Some(prev) = crate::get_my_current_task() else { return false };
    let rq = &RQS[cpu];

    // Interruptions coupées jusqu'à la fin du changement de contexte.
    let irq = hold_interrupts();
    let mut g = rq.lock();
    rq.need_resched.store(false, Relaxed);
    let now = clock_ns();
    // SAFETY : verrou de la file tenu pour toutes les manipulations d'entités ci-dessous.
    unsafe { put_prev_task(&mut g, &prev, now, yielded, cpu) };
    let mut next = unsafe { pick_next_task(&mut g, now) };
    if next.is_none() {
        drop(g);
        newidle_balance(cpu);
        g = rq.lock();
        next = unsafe { pick_next_task(&mut g, clock_ns()) };
    }
    let next = match next {
        Some(n) => n,
        None => match g.idle.clone() {
            Some(i) => i,
            None => {
                // Pas encore de tâche inactive (tout début de l'amorçage) : on continue.
                g.curr = Arc::as_ptr(&prev.0);
                return false;
            }
        },
    };
    g.curr = Arc::as_ptr(&next.0);
    rq.publish(&g);
    if next.is_an_idle_task {
        IDLE_CPUS.set(cpu);
    } else {
        IDLE_CPUS.clear(cpu);
        let ts = next.sched.wake_ts.swap(0, Relaxed);
        if ts != 0 {
            let lat = now.saturating_sub(ts);
            rq.counters.wake_lat_sum_ns.fetch_add(lat, Relaxed);
            rq.counters.wake_lat_count.fetch_add(1, Relaxed);
            rq.counters.wake_lat_max_ns.fetch_max(lat, Relaxed);
        }
    }
    if Arc::ptr_eq(&next.0, &prev.0) {
        return false;
    }
    rq.counters.switches.fetch_add(1, Relaxed);
    drop(g);

    let zombie = crate::switch_to(prev, next, cpu_id);
    // Nous sommes de retour (plus tard) sur la pile de la tâche qui avait appelé schedule().
    drop(irq);
    drop(zombie);
    drop(preemption_guard);
    true
}

/// Remet la tâche précédente dans sa file, ou l'en retire si elle s'est bloquée ou est sortie.
unsafe fn put_prev_task(g: &mut RqInner, prev: &TaskRef, now: u64, yielded: bool, cpu: usize) {
    if prev.is_an_idle_task {
        return;
    }
    let e = ent(prev);
    let runnable = prev.runstate.load() == RunState::Runnable;
    if g.curr != Arc::as_ptr(&prev.0) {
        // La tâche tournait sans être connue de cette file (tâche d'amorçage pas encore adoptée).
        if runnable && !(*e).on_rq {
            prev.sched.cpu.store(cpu, Release);
            hold_ref(prev, e);
            enqueue_entity(g, e, flags::INITIAL, now);
        }
        return;
    }
    g.curr = ptr::null();
    if !(*e).on_rq {
        return;
    }
    let pending = prev.sched.pending_class.take();
    match (*e).class {
        Class::Fair { .. } => {
            if runnable && pending.is_none() {
                g.fair.put_prev(e, now, yielded);
                return;
            }
            let sleep = prev.runstate.load() == RunState::Blocked;
            if g.fair.dequeue_curr(e, now, sleep && pending.is_none()) == Dequeued::Delayed {
                return;
            }
        }
        Class::Rt { .. } => {
            if runnable && pending.is_none() {
                let rr_expired = (*e).rr_left == maisched::RR_TIMESLICE_NS;
                g.rt.put_prev(e, yielded || rr_expired);
                return;
            }
            g.rt.dequeue_curr(e);
        }
        Class::Idle => return,
    }
    match pending {
        // Changement de classe : la tâche reste en file, dans sa nouvelle classe.
        Some(class) if runnable => {
            apply_class(e, class);
            enqueue_entity(g, e, 0, now);
        }
        _ => {
            if let Some(class) = pending {
                apply_class(e, class);
            }
            drop(take_ref(e));
        }
    }
}

/// Choisit la prochaine tâche : temps réel, puis EEVDF. `None` si rien n'est prêt.
unsafe fn pick_next_task(g: &mut RqInner, now: u64) -> Option<TaskRef> {
    loop {
        let mut e = g.rt.pick_next();
        if e.is_null() {
            e = g.fair.pick_next(now, |gone| drop(take_ref(gone)));
        }
        if e.is_null() {
            return None;
        }
        let t = clone_ref(e);
        if let RunState::Exited(_) = t.runstate.load() {
            // Tuée pendant qu'elle attendait : on la sort de la file.
            match (*e).class {
                Class::Fair { .. } => { g.fair.dequeue_curr(e, now, false); }
                Class::Rt { .. } => g.rt.dequeue_curr(e),
                Class::Idle => {}
            }
            drop(take_ref(e));
            continue;
        }
        return Some(t);
    }
}

// ---------------------------------------------------------------------------
// Réveil et mise en file
// ---------------------------------------------------------------------------

/// Choix du cœur d'une tâche qui devient prête (Linux `select_task_rq_fair`, Windows « processeur
/// idéal », Nest : on garde les cœurs déjà chauds).
fn select_cpu(t: &Task, prev: usize, this: usize, initial: bool) -> usize {
    let n = nr_cpus();
    if let Some(p) = pinned_cpu(t) {
        return if p < n { p } else { this };
    }
    let online = |c: usize| c < n && RQS[c].online.load(Relaxed);
    // 1. Le cœur précédent s'il est libre : caches encore chauds.
    if !initial && online(prev) && IDLE_CPUS.test_and_clear(prev) {
        return prev;
    }
    // 2. Un cœur libre, du plus proche au plus lointain (SMT, LLC, NUMA).
    let origin = if online(prev) { prev } else { this };
    if let Some(topo) = topology() {
        for &c in topo.steal_order(origin) {
            if online(c) && IDLE_CPUS.test_and_clear(c) {
                return c;
            }
        }
    } else {
        for c in 0..n {
            if online(c) && IDLE_CPUS.test_and_clear(c) {
                return c;
            }
        }
    }
    // 3. Aucun libre : le moins chargé, à égalité le précédent (cache), puis le courant.
    let mut best = if online(origin) { origin } else { this };
    let mut best_nr = RQS[best].nr_running.load(Relaxed);
    for c in 0..n {
        if online(c) {
            let nr = RQS[c].nr_running.load(Relaxed);
            if nr < best_nr {
                best = c;
                best_nr = nr;
            }
        }
    }
    best
}

/// Rend une tâche prête (réveil, ou première mise en file d'une tâche neuve).
///
/// À appeler après que l'état est passé à `Runnable` (`unblock()` le fait par CAS, une seule fois
/// par réveil). `force_cpu` impose le cœur (`add_task_to`).
fn wake_up(task: &TaskRef, force_cpu: Option<usize>) {
    // Seule une tâche prête entre en file (une tâche bloquée y entrera à son `unblock()`).
    if task.runstate.load() != RunState::Runnable {
        return;
    }
    let e = ent(task);
    let this = this_cpu();
    let _irq = hold_interrupts();

    // 1. Encore en file (différée, ou pas encore sortie par schedule()) : rien à déplacer.
    loop {
        let c = task.sched.cpu.load(Acquire);
        let rq = &RQS[c.min(MAX_CPUS - 1)];
        let mut g = rq.lock();
        if task.sched.cpu.load(Acquire) != c {
            continue;
        }
        // SAFETY : verrou de la file propriétaire tenu.
        unsafe {
            if !(*e).on_rq {
                break;
            }
            if (*e).delayed {
                let now = clock_ns();
                enqueue_entity(&mut g, e, flags::WAKEUP, now);
                task.sched.wake_ts.store(now, Relaxed);
                rq.publish(&g);
                let preempt = check_preempt(&mut g, e, now);
                drop(g);
                rq.counters.wakeups.fetch_add(1, Relaxed);
                if preempt {
                    rq.need_resched.store(true, Release);
                    kick(c);
                }
            }
        }
        return;
    }

    // 2. Attendre qu'elle ait fini de quitter son ancien CPU (fenêtre du changement de contexte).
    while task.is_running() {
        core::hint::spin_loop();
    }

    // 3. Choisir un cœur et mettre en file.
    let prev = task.sched.cpu.load(Acquire);
    // SAFETY : la tâche n'est dans aucune file ni sur aucun CPU : personne d'autre ne lit l'entité.
    let initial = unsafe { (*e).sum_exec == 0 && (*e).deadline == 0 };
    let target = force_cpu.filter(|&c| c < nr_cpus()).unwrap_or_else(|| select_cpu(task, prev, this, initial));
    let rq = &RQS[target];
    let mut g = rq.lock();
    // SAFETY : verrou de la file cible tenu.
    let preempt = unsafe {
        if (*e).on_rq {
            return; // mise en file par un autre chemin entre-temps
        }
        if let Some(class) = task.sched.pending_class.take() {
            apply_class(e, class);
        }
        task.sched.cpu.store(target, Release);
        let now = clock_ns();
        hold_ref(task, e);
        enqueue_entity(&mut g, e, if initial { flags::INITIAL } else { flags::WAKEUP }, now);
        if !initial {
            task.sched.wake_ts.store(now, Relaxed);
        }
        rq.publish(&g);
        check_preempt(&mut g, e, now)
    };
    drop(g);
    rq.counters.wakeups.fetch_add(1, Relaxed);
    if target != this {
        rq.counters.remote_wakeups.fetch_add(1, Relaxed);
    }
    if preempt {
        rq.need_resched.store(true, Release);
        rq.counters.wakeup_preemptions.fetch_add(1, Relaxed);
        kick(target);
    }
}

/// Adopte comme tâche courante du cœur `cpu` une tâche qui y tourne déjà (tâche d'amorçage).
fn adopt_current(cpu: usize, task: &TaskRef) {
    let rq = &RQS[cpu];
    let mut g = rq.lock();
    let e = ent(task);
    // SAFETY : verrou tenu.
    unsafe {
        if (*e).on_rq {
            return;
        }
        let now = clock_ns();
        task.sched.cpu.store(cpu, Release);
        hold_ref(task, e);
        (*e).class = Class::Fair { batch: false };
        g.fair.enqueue(e, flags::INITIAL, now);
        if g.curr.is_null() {
            g.fair.take_as_curr(e, now);
            g.curr = Arc::as_ptr(&task.0);
        }
        rq.publish(&g);
    }
}

fn is_current(task: &TaskRef) -> bool {
    crate::get_my_current_task().map_or(false, |c| Arc::ptr_eq(&c.0, &task.0))
}

/// Ajoute une tâche prête au cœur le plus approprié.
pub fn add_task(task: TaskRef) {
    wake_up(&task, None);
}

/// Ajoute une tâche prête au cœur donné.
pub fn add_task_to(cpu_id: CpuId, task: TaskRef) {
    let cpu = cpu_id.value() as usize;
    if cpu < MAX_CPUS && is_current(&task) {
        adopt_current(cpu, &task);
    } else {
        wake_up(&task, Some(cpu));
    }
}

/// Ajoute une tâche prête au cœur courant.
pub fn add_task_to_current(task: TaskRef) {
    add_task_to(cpu::current_cpu(), task);
}

/// Retire de sa file une tâche qui n'est plus prête (bloquée, tuée). Sans effet sur une tâche
/// prête. La tâche courante n'est pas retirée ici : `schedule()` s'en charge.
pub fn remove_task(task: &TaskRef) -> bool {
    if is_current(task) {
        return true;
    }
    let e = ent(task);
    let _irq = hold_interrupts();
    loop {
        let c = task.sched.cpu.load(Acquire).min(MAX_CPUS - 1);
        let rq = &RQS[c];
        let mut g = rq.lock();
        if task.sched.cpu.load(Acquire) != c {
            continue;
        }
        // SAFETY : verrou de la file propriétaire tenu.
        unsafe {
            if !(*e).on_rq || task.runstate.load() == RunState::Runnable {
                return true;
            }
            if g.curr == Arc::as_ptr(&task.0) {
                // Elle tourne sur un autre cœur : ce cœur la retirera en replanifiant.
                drop(g);
                rq.need_resched.store(true, Release);
                kick(c);
                return true;
            }
            dequeue_queued(&mut g, e, clock_ns());
            drop(take_ref(e));
            rq.publish(&g);
        }
        return true;
    }
}

pub fn remove_task_from(task: &TaskRef, _cpu_id: CpuId) -> bool {
    remove_task(task)
}

pub fn remove_task_from_current(task: &TaskRef) -> bool {
    remove_task(task)
}

/// Enregistre la tâche inactive d'un cœur et le déclare en ligne.
pub fn register_idle_task(cpu_id: CpuId, task: TaskRef) {
    let cpu = cpu_id.value() as usize;
    if cpu >= MAX_CPUS {
        return;
    }
    // SAFETY : la tâche inactive n'est jamais dans une file ; personne d'autre ne la manipule.
    unsafe { (*ent(&task)).class = Class::Idle };
    task.sched.cpu.store(cpu, Release);
    let rq = &RQS[cpu];
    let mut g = rq.lock();
    g.idle = Some(task);
    rq.online.store(true, Release);
    drop(g);
    if cpu >= nr_cpus() {
        NR_CPUS.fetch_max(cpu + 1, AcqRel);
    }
}

// ---------------------------------------------------------------------------
// Tick, préemption, attente inactive
// ---------------------------------------------------------------------------

/// Tick d'ordonnancement du cœur courant (interruption du timer). Renvoie `true` s'il faut
/// replanifier en sortant de l'interruption.
pub fn scheduler_tick() -> bool {
    let cpu = this_cpu();
    let rq = &RQS[cpu];
    let mut g = rq.lock();
    let now = clock_ns();
    let elapsed = now.saturating_sub(g.last_tick_ns);
    g.last_tick_ns = now;
    g.ticks += 1;
    let ticks = g.ticks;
    rq.counters.ticks.fetch_add(1, Relaxed);
    let curr = g.curr;
    let resched = if curr.is_null() {
        false
    } else {
        // SAFETY : verrou tenu ; la courante est en file (référence `owner`).
        unsafe {
            if (*curr).is_an_idle_task {
                g.fair.nr_runnable() + g.rt.nr_runnable() > 0
            } else {
                match (*ent(&*curr)).class {
                    Class::Fair { .. } => {
                        g.fair.update_curr(now) || g.fair.tick_should_resched() || g.rt.nr_runnable() > 0
                    }
                    Class::Rt { .. } => g.rt.tick(elapsed),
                    Class::Idle => false,
                }
            }
        }
    };
    drop(g);
    if resched {
        rq.need_resched.store(true, Release);
        rq.counters.tick_preemptions.fetch_add(1, Relaxed);
    }
    if ticks % BALANCE_TICKS == cpu as u64 % BALANCE_TICKS {
        periodic_balance(cpu);
    }
    rq.need_resched.load(Acquire)
}

/// Point de préemption différée : appelé par `preemption` quand le compteur de préemption repasse
/// à 0. Si un tick ou un réveil a demandé une replanification pendant la section non préemptible,
/// on la fait maintenant (seulement interruptions actives : jamais au milieu d'une section qui les
/// a coupées).
pub fn preemption_point() {
    if RQS[this_cpu()].need_resched.load(Acquire) && irq_safety::interrupts_enabled() {
        schedule_preempt();
    }
}

/// Faut-il replanifier sur ce cœur ?
pub fn need_resched() -> bool {
    RQS[this_cpu()].need_resched.load(Acquire)
}

/// Attente de la tâche inactive : endort le cœur jusqu'à la prochaine interruption, sauf si du
/// travail est arrivé entre-temps. `sti; hlt` est atomique (fenêtre d'une instruction), donc un
/// réveil (IPI) arrivé après la vérification ne peut pas être manqué.
pub fn idle_wait() {
    let rq = &RQS[this_cpu()];
    irq_safety::disable_interrupts();
    if rq.nr_running.load(Acquire) > 0 || rq.need_resched.load(Acquire) {
        irq_safety::enable_interrupts();
        return;
    }
    rq.counters.idle_entries.fetch_add(1, Relaxed);
    #[cfg(target_arch = "x86_64")]
    // SAFETY : réactive les interruptions et attend la suivante.
    unsafe { core::arch::asm!("sti", "hlt", options(nomem, nostack)) };
    #[cfg(target_arch = "aarch64")]
    {
        irq_safety::enable_interrupts();
        // SAFETY : attente d'interruption.
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
    }
}

// ---------------------------------------------------------------------------
// Équilibrage
// ---------------------------------------------------------------------------

/// Vole une tâche prête (pas en cours d'exécution, autorisée sur `dst`) à `src`. Le retard EEVDF
/// est conservé à travers la migration, l'équité aussi.
fn steal_from(src: usize, dst: usize) -> bool {
    if src == dst {
        return false;
    }
    let dg = DoubleGuard::lock(src, dst);
    let (gs, gd) = (dg.first(), dg.second());
    let now = clock_ns();
    let filter = |e: *mut Entity| unsafe {
        let t = task_of(e);
        Option::<CpuId>::from(t.running_on_cpu.load()).is_none() && allowed_on(t, dst)
    };
    // SAFETY : les deux verrous sont tenus.
    unsafe {
        let mut e = gs.fair.steal_candidate(filter);
        if !e.is_null() {
            gs.fair.detach(e, now);
        } else {
            e = gs.rt.steal_candidate(filter);
            if e.is_null() {
                return false;
            }
            gs.rt.dequeue_queued(e);
        }
        task_of(e).sched.cpu.store(dst, Release);
        enqueue_entity(gd, e, flags::MIGRATED, now);
        RQS[src].publish(gs);
        RQS[dst].publish(gd);
        if check_preempt(gd, e, now) {
            RQS[dst].need_resched.store(true, Release);
        }
    }
    drop(dg);
    RQS[dst].counters.migrations.fetch_add(1, Relaxed);
    true
}

/// Le cœur `cpu` va passer à vide : il vole une tâche, du voisin le plus proche au plus lointain
/// (« Decade of Wasted Cores » : un cœur ne doit jamais rester inactif quand un autre a une file).
fn newidle_balance(cpu: usize) -> bool {
    let n = nr_cpus();
    let busy = |c: usize| c < n && RQS[c].nr_running.load(Relaxed) >= 2;
    if let Some(topo) = topology() {
        for &c in topo.steal_order(cpu) {
            if busy(c) && steal_from(c, cpu) {
                return true;
            }
        }
        false
    } else {
        (0..n).filter(|&c| c != cpu).any(|c| busy(c) && steal_from(c, cpu))
    }
}

/// Tirage périodique : un cœur nettement moins chargé que le plus chargé lui prend une tâche.
fn periodic_balance(cpu: usize) {
    let n = nr_cpus();
    let mine = RQS[cpu].nr_running.load(Relaxed);
    let mut busiest = cpu;
    let mut most = mine;
    for c in 0..n {
        let nr = RQS[c].nr_running.load(Relaxed);
        if nr > most {
            most = nr;
            busiest = c;
        }
    }
    if busiest != cpu && most >= mine + 2 {
        steal_from(busiest, cpu);
    }
}

// ---------------------------------------------------------------------------
// Priorités et classes
// ---------------------------------------------------------------------------

/// Exécute `f` sous le verrou de la file qui possède la tâche.
fn with_task_rq<R>(task: &TaskRef, f: impl FnOnce(&mut RqInner, *mut Entity, u64) -> R) -> R {
    let _irq = hold_interrupts();
    loop {
        let c = task.sched.cpu.load(Acquire).min(MAX_CPUS - 1);
        let rq = &RQS[c];
        let mut g = rq.lock();
        if task.sched.cpu.load(Acquire) != c {
            continue;
        }
        let r = f(&mut g, ent(task), clock_ns());
        rq.publish(&g);
        return r;
    }
}

/// Priorité (nice + 20, de 0 à 39) d'une tâche.
pub fn priority(task: &TaskRef) -> Option<u8> {
    Some((task.sched.nice.load(Relaxed) + 20) as u8)
}

/// Fixe la priorité (nice + 20) d'une tâche, sans fausser son retard EEVDF.
pub fn set_priority(task: &TaskRef, priority: u8) -> bool {
    let nice = (priority.min(39) as i8) - 20;
    task.sched.nice.store(nice, Relaxed);
    let w = maisched::weight_of_nice(nice);
    with_task_rq(task, |g, e, now| unsafe {
        if (*e).on_rq && (*e).is_fair() {
            g.fair.reweight(e, w, now);
        } else {
            (*e).set_weight(w);
        }
    });
    true
}

/// Change la classe d'ordonnancement d'une tâche (temps partagé, fond, temps réel).
pub fn set_class(task: &TaskRef, class: Class) {
    if matches!(class, Class::Idle) {
        return;
    }
    let resched_cpu = with_task_rq(task, |g, e, now| unsafe {
        if !(*e).on_rq {
            apply_class(e, class);
            return None;
        }
        if g.curr == Arc::as_ptr(&task.0) {
            task.sched.pending_class.store(Some(class));
            return Some(task.sched.cpu.load(Relaxed));
        }
        dequeue_queued(g, e, now);
        apply_class(e, class);
        enqueue_entity(g, e, 0, now);
        None
    });
    if let Some(c) = resched_cpu {
        RQS[c].need_resched.store(true, Release);
        kick(c);
    }
}

/// Charge d'un cœur (tâches prêtes, courante comprise).
pub fn busyness(cpu_id: CpuId) -> Option<usize> {
    let c = cpu_id.value() as usize;
    (c < MAX_CPUS).then(|| RQS[c].nr_running.load(Relaxed) as usize)
}

/// Statistiques de chaque cœur en ligne.
pub fn stats() -> Vec<SchedStats> {
    (0..nr_cpus())
        .filter(|&c| RQS[c].online.load(Relaxed))
        .map(|c| {
            let k = &RQS[c].counters;
            let cnt = k.wake_lat_count.load(Relaxed).max(1);
            SchedStats {
                cpu: c,
                nr_running: RQS[c].nr_running.load(Relaxed),
                idle: IDLE_CPUS.test(c),
                ticks: k.ticks.load(Relaxed),
                switches: k.switches.load(Relaxed),
                wakeups: k.wakeups.load(Relaxed),
                remote_wakeups: k.remote_wakeups.load(Relaxed),
                migrations: k.migrations.load(Relaxed),
                tick_preemptions: k.tick_preemptions.load(Relaxed),
                wakeup_preemptions: k.wakeup_preemptions.load(Relaxed),
                idle_entries: k.idle_entries.load(Relaxed),
                wake_lat_avg_ns: k.wake_lat_sum_ns.load(Relaxed) / cnt,
                wake_lat_max_ns: k.wake_lat_max_ns.load(Relaxed),
            }
        })
        .collect()
}

/// Ancienne API : sans effet, MaiSched gère toutes les politiques.
pub fn set_policy<T: Scheduler>(_cpu_id: CpuId, _scheduler: T) {}

/// Élève temporairement la priorité de `task` à celle de la tâche courante (héritage de
/// priorité) ; le garde rétablit l'ancienne.
pub fn inherit_priority(task: &TaskRef) -> PriorityInheritanceGuard<'_> {
    let current_priority = super::with_current_task(priority).ok().flatten();
    let other_priority = priority(task);
    let inner = match (current_priority, other_priority) {
        // Priorité = nice + 20 : plus petit = plus prioritaire.
        (Some(cur), Some(other)) if cur < other => {
            set_priority(task, cur);
            Some((task, other))
        }
        _ => None,
    };
    PriorityInheritanceGuard { inner }
}

/// Rétablit la priorité d'une tâche à la destruction.
pub struct PriorityInheritanceGuard<'a> {
    inner: Option<(&'a TaskRef, u8)>,
}

impl Drop for PriorityInheritanceGuard<'_> {
    fn drop(&mut self) {
        if let Some((task, priority)) = self.inner {
            set_priority(task, priority);
        }
    }
}

/// Tâches en cours d'exécution sur chaque cœur (diagnostic).
pub fn tasks() -> Vec<(CpuId, Vec<TaskRef>)> {
    crate::all_tasks()
        .into_iter()
        .filter_map(|(_, t)| Option::<CpuId>::from(t.running_on_cpu.load()).map(|c| (c, t)))
        .fold(Vec::new(), |mut acc: Vec<(CpuId, Vec<TaskRef>)>, (c, t)| {
            match acc.iter_mut().find(|(k, _)| *k == c) {
                Some((_, v)) => v.push(t),
                None => acc.push((c, alloc::vec![t])),
            }
            acc
        })
}

// ---------------------------------------------------------------------------
// Traits historiques (API Theseus)
// ---------------------------------------------------------------------------

/// Ordonnanceur historique de Theseus (conservé pour compatibilité ; MaiSched ne l'utilise pas).
pub trait Scheduler: Send + Sync + 'static {
    fn next(&mut self) -> TaskRef;
    fn add(&mut self, task: TaskRef);
    fn busyness(&self) -> usize;
    fn remove(&mut self, task: &TaskRef) -> bool;
    fn as_priority_scheduler(&mut self) -> Option<&mut dyn PriorityScheduler>;
    fn drain(&mut self) -> Box<dyn Iterator<Item = TaskRef> + '_>;
    fn tasks(&self) -> Vec<TaskRef>;
}

pub trait PriorityScheduler {
    fn set_priority(&mut self, task: &TaskRef, priority: u8) -> bool;
    fn priority(&mut self, task: &TaskRef) -> Option<u8>;
}
