//! This crate contains the basic `Task` structure.
#![no_std]
#![feature(panic_info_message)]
#![feature(negative_impls)]
#![allow(clippy::type_complexity)]

extern crate alloc;

pub use maisched;

use core::{
    any::Any,
    fmt,
    hash::{Hash, Hasher},
    cell::UnsafeCell,
    sync::atomic::{AtomicI8, AtomicU64, AtomicU8, AtomicUsize, Ordering},
    task::Waker,
};

// ---------------------------------------------------------------------------
// Ordonnancement (MaiSched)
// ---------------------------------------------------------------------------

/// Politique d'ordonnancement demandée pour une tâche (API historique, convertie en
/// [`maisched::Class`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedClass {
    /// Tâche inactive d'un cœur.
    Idle,
    /// Tâche de fond (`SCHED_BATCH`) : tranche longue, ne préempte pas au réveil.
    Batch,
    /// Temps partagé EEVDF.
    Normal,
    /// Tâche de calcul GPU (ordonnancée comme `Normal` côté CPU).
    Compute,
    /// Temps réel tourniquet ; le paramètre (tranche en ns) est ignoré : `maisched::RR_TIMESLICE_NS`.
    RoundRobin(u64),
    /// Temps réel FIFO.
    Fifo,
    /// `SCHED_DEADLINE` (EDF + CBS) : prévu en phase S3, ordonnancé d'ici là comme FIFO de
    /// priorité maximale.
    Deadline { period_ns: u64, runtime_ns: u64 },
}

impl Default for SchedClass {
    fn default() -> Self { SchedClass::Normal }
}

impl SchedClass {
    /// Classe MaiSched correspondante, avec la priorité temps réel `rt_prio` (1..99).
    pub fn to_class(self, rt_prio: u8) -> maisched::Class {
        let prio = rt_prio.clamp(1, 99);
        match self {
            SchedClass::Idle => maisched::Class::Idle,
            SchedClass::Batch => maisched::Class::Fair { batch: true },
            SchedClass::Normal | SchedClass::Compute => maisched::Class::Fair { batch: false },
            SchedClass::RoundRobin(_) => maisched::Class::Rt { prio, rr: true },
            SchedClass::Fifo => maisched::Class::Rt { prio, rr: false },
            SchedClass::Deadline { .. } => maisched::Class::Rt { prio: 99, rr: false },
        }
    }
}

/// État d'ordonnancement d'une tâche.
///
/// L'entité MaiSched (`vruntime`, échéance, chaînage intrusif…) n'est lue et modifiée que sous
/// le verrou de la file du cœur `cpu` (voir `task::scheduler`). Les champs atomiques peuvent être
/// lus sans verrou.
pub struct SchedEntity {
    entity: UnsafeCell<maisched::Entity>,
    /// Cœur dont la file possède (ou a possédé en dernier) l'entité.
    pub cpu: AtomicUsize,
    /// Valeur de nice courante [-20, 19].
    pub nice: AtomicI8,
    /// Horodatage (ns) du dernier réveil, pour mesurer la latence réveil -> exécution.
    pub wake_ts: AtomicU64,
    /// Changement de classe en attente, appliqué par l'ordonnanceur au prochain passage.
    pub pending_class: AtomicClass,
    /// Cœurs permis (`sched_setaffinity`), hérités à la création d'un fil.
    pub cpus_allowed: CpuMask,
    /// Diagnostic : dernière prise d'un cœur (`switch_to`) et dernière libération
    /// (`finish_task_switch`), codées par `task::trace_word` (cœur, autre tâche, numéro d'ordre).
    pub trace_on: AtomicU64,
    pub trace_off: AtomicU64,
}

/// Nombre de cœurs que décrit un [`CpuMask`] (celui de `task::scheduler::MAX_CPUS`).
pub const CPU_MASK_BITS: usize = 256;
const CPU_MASK_WORDS: usize = CPU_MASK_BITS / 64;

/// Ensemble de cœurs permis, lu sans verrou par l'ordonnanceur (aussi depuis un réveil venu
/// d'une interruption).
///
/// `restricted` faux = tous les cœurs (cas courant : un seul chargement atomique). Un lecteur
/// peut voir un masque à moitié écrit pendant `set` : l'ordonnanceur se corrige au passage
/// suivant (une tâche sur un cœur qu'elle n'a plus le droit d'utiliser est migrée quand elle le
/// rend), ce qui n'exige donc pas de *seqlock*.
pub struct CpuMask {
    words: [AtomicU64; CPU_MASK_WORDS],
    restricted: core::sync::atomic::AtomicBool,
}

impl CpuMask {
    pub const fn all() -> CpuMask {
        const FULL: AtomicU64 = AtomicU64::new(u64::MAX);
        CpuMask { words: [FULL; CPU_MASK_WORDS], restricted: core::sync::atomic::AtomicBool::new(false) }
    }

    /// Le cœur `cpu` est-il permis ?
    #[inline]
    pub fn allows(&self, cpu: usize) -> bool {
        if !self.restricted.load(Ordering::Acquire) {
            return true;
        }
        cpu < CPU_MASK_BITS && self.words[cpu / 64].load(Ordering::Relaxed) & (1 << (cpu % 64)) != 0
    }

    pub fn is_restricted(&self) -> bool {
        self.restricted.load(Ordering::Acquire)
    }

    /// Copie du masque (tous les bits posés s'il n'est pas restreint).
    pub fn get(&self) -> [u64; CPU_MASK_WORDS] {
        if !self.is_restricted() {
            return [u64::MAX; CPU_MASK_WORDS];
        }
        core::array::from_fn(|i| self.words[i].load(Ordering::Relaxed))
    }

    /// Pose le masque ; `ncpus` : nombre de cœurs du système (un masque qui les couvre tous
    /// redevient « non restreint »).
    pub fn set(&self, mask: &[u64; CPU_MASK_WORDS], ncpus: usize) {
        let all = (0..ncpus.min(CPU_MASK_BITS)).all(|c| mask[c / 64] & (1 << (c % 64)) != 0);
        if all {
            self.restricted.store(false, Ordering::Release);
        }
        for (w, &m) in self.words.iter().zip(mask.iter()) {
            w.store(if all { u64::MAX } else { m }, Ordering::Relaxed);
        }
        if !all {
            self.restricted.store(true, Ordering::Release);
        }
    }

    /// Recopie le masque d'une autre tâche (création d'un fil, `fork`).
    pub fn copy_from(&self, other: &CpuMask) {
        let r = other.is_restricted();
        for (w, o) in self.words.iter().zip(other.words.iter()) {
            w.store(o.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        self.restricted.store(r, Ordering::Release);
    }
}

/// `Option<maisched::Class>` dans un `AtomicU32` : toujours sans verrou.
///
/// `AtomicCell<Option<Class>>` (3 octets) passait par les verrous globaux de crossbeam : un
/// `take()` dans un réveil venu d'une interruption tournait pour toujours si le code interrompu
/// tenait le même verrou (voir [`AtomicRunState`]).
pub struct AtomicClass(core::sync::atomic::AtomicU32);

impl AtomicClass {
    const NONE: u32 = 0;

    pub const fn new() -> AtomicClass {
        AtomicClass(core::sync::atomic::AtomicU32::new(Self::NONE))
    }

    fn encode(c: Option<maisched::Class>) -> u32 {
        match c {
            None => Self::NONE,
            Some(maisched::Class::Fair { batch }) => 1 | (batch as u32) << 8,
            Some(maisched::Class::Rt { prio, rr }) => 2 | (prio as u32) << 8 | (rr as u32) << 16,
            Some(maisched::Class::Idle) => 3,
        }
    }

    fn decode(v: u32) -> Option<maisched::Class> {
        match v & 0xFF {
            1 => Some(maisched::Class::Fair { batch: v >> 8 & 1 != 0 }),
            2 => Some(maisched::Class::Rt { prio: (v >> 8) as u8, rr: v >> 16 & 1 != 0 }),
            3 => Some(maisched::Class::Idle),
            _ => None,
        }
    }

    pub fn load(&self) -> Option<maisched::Class> {
        Self::decode(self.0.load(core::sync::atomic::Ordering::Acquire))
    }

    pub fn store(&self, c: Option<maisched::Class>) {
        self.0.store(Self::encode(c), core::sync::atomic::Ordering::Release)
    }

    pub fn take(&self) -> Option<maisched::Class> {
        Self::decode(self.0.swap(Self::NONE, core::sync::atomic::Ordering::AcqRel))
    }
}

impl Default for AtomicClass {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY : l'entité n'est accédée que sous le verrou de la file qui la possède.
unsafe impl Sync for SchedEntity {}
unsafe impl Send for SchedEntity {}

impl SchedEntity {
    pub fn new(id: usize) -> SchedEntity {
        SchedEntity {
            entity: UnsafeCell::new(maisched::Entity::new(id as u64, maisched::Class::Fair { batch: false })),
            cpu: AtomicUsize::new(0),
            nice: AtomicI8::new(0),
            wake_ts: AtomicU64::new(0),
            pending_class: AtomicClass::new(),
            cpus_allowed: CpuMask::all(),
            trace_on: AtomicU64::new(0),
            trace_off: AtomicU64::new(0),
        }
    }

    /// Pointeur vers l'entité, à ne déréférencer que sous le verrou de la file propriétaire
    /// (ou avant que la tâche ne soit visible par l'ordonnanceur).
    #[inline(always)]
    pub fn entity_ptr(&self) -> *mut maisched::Entity {
        self.entity.get()
    }

    /// Accès exclusif à l'entité pendant la construction de la tâche (avant `Arc`).
    pub fn entity_mut(&mut self) -> &mut maisched::Entity {
        self.entity.get_mut()
    }

    /// Lecture non synchronisée d'informations de diagnostic (peut être légèrement périmée).
    pub fn snapshot(&self) -> SchedSnapshot {
        // SAFETY : lectures de champs entiers, au pire périmées ; aucune référence conservée.
        let e = unsafe { &*self.entity.get() };
        SchedSnapshot {
            class: e.class,
            nice: self.nice.load(Ordering::Relaxed),
            weight: e.weight,
            slice_ns: e.slice,
            sum_exec_ns: e.sum_exec,
            burst_ewma_ns: e.burst_ewma,
            on_rq: e.on_rq,
            cpu: self.cpu.load(Ordering::Relaxed),
        }
    }
}

/// Copie lisible de l'état d'ordonnancement (pour `ps`, `top`, le gestionnaire de tâches).
#[derive(Clone, Copy, Debug)]
pub struct SchedSnapshot {
    pub class: maisched::Class,
    pub nice: i8,
    pub weight: u32,
    pub slice_ns: u64,
    pub sum_exec_ns: u64,
    pub burst_ewma_ns: u64,
    pub on_rq: bool,
    pub cpu: usize,
}

use alloc::{
    boxed::Box,
    string::String,
    sync::Arc,
    vec::Vec,
};
use cpu::{OptionalCpuId}; // On garde juste OptionalCpuId
use crossbeam_utils::atomic::AtomicCell;
use sync_irq::IrqSafeMutex;
use memory::MmiRef;
use stack::Stack;
use mod_mgmt::{AppCrateRef, CrateNamespace, TlsDataImage};
use environment::Environment;
use spin::Mutex;

#[cfg(target_arch = "x86_64")]
use memory::paging::address_space::AddressSpace;

/// MaiProc : état d'une tâche qui exécute un fil en mode utilisateur (anneau 3).
///
/// Posé une seule fois, par la tâche elle-même, juste avant son premier passage en mode
/// utilisateur ; lu par le changement de contexte (espace d'adresses, pile noyau, état vectoriel).
#[cfg(target_arch = "x86_64")]
pub struct UserContext {
    /// Espace d'adresses courant du fil (`execve` le remplace).
    pub space: spin::Mutex<Arc<AddressSpace>>,
    /// Sommet de la pile noyau de la tâche (RSP0, pile de `syscall`), aligné sur 16 octets.
    pub kstack_top: usize,
    /// Registres vectoriels du fil, sauvés quand il quitte le cœur.
    pub fpu: uentry::FpuState,
    /// Sélecteurs de segment et entrées TLS de la GDT (fils 32 bits), échangés au changement
    /// de contexte.
    pub seg: uentry::SegState,
    /// Fil utilisateur porté par la tâche (type défini par la couche processus : `maiproc::Thread`).
    pub process: Arc<dyn Any + Send + Sync>,
}

/// The generic type of a callback that will be invoked when a task panics.
pub type KillHandler = Box<dyn Fn(&Task, KillReason) + Send + Sync + 'static>;

/// A structure that contains the contextual execution states of a thread.
pub struct Task {
    /// The unique ID of this task.
    pub id: usize,
    /// The name of this task.
    pub name: String,
    /// The definition of this task's runstate (Running, Blocked, etc).
    pub runstate: AtomicRunState,
    /// The CPU that this task is currently running on.
    pub running_on_cpu: AtomicCell<OptionalCpuId>,
    /// The stack of this task.
    pub kstack: Stack,
    /// The memory management info (page table) for this task (kernel default).
    pub mmi: MmiRef,
    /// The namespace of crates/symbols that this task has access to.
    pub namespace: Arc<CrateNamespace>,
    /// The environment variables for this task.
    pub env: Arc<Mutex<Environment>>,
    /// The specific application crate that this task is running.
    pub app_crate: Option<Arc<AppCrateRef>>,
    /// The Thread-Local Storage (TLS) area for this task.
    pub tls_area: TlsDataImage,
    /// The saved stack pointer (used for context switching).
    pub saved_sp: usize,
    /// A custom kill handler.
    pub kill_handler: IrqSafeMutex<Option<KillHandler>>,
    /// Whether this task is pinned to a specific core.
    pub pinned_core: AtomicCell<OptionalCpuId>,
    /// User-defined context data.
    pub context_data: IrqSafeMutex<Option<Box<dyn Any + Send + Sync>>>,
    /// Wakers for async tasks.
    pub wakers: IrqSafeMutex<Vec<Waker>>,
    /// Tâches bloquées dans `join()` sur celle-ci, réveillées à sa sortie (plus d'attente active).
    pub exit_waiters: IrqSafeMutex<Vec<Arc<Task>>>,
    
    /// Syscall ABI mode: 0 = Native, 1 = Linux, 2 = Windows.
    /// Stored as AtomicU8 for lock-free access from the syscall hot path.
    pub exec_mode: AtomicU8,

    /// MaiProc : présent si la tâche exécute un fil utilisateur.
    #[cfg(target_arch = "x86_64")]
    pub user: spin::Once<UserContext>,

    pub is_an_idle_task: bool,

    pub restart_info: Option<RestartInfo>,

    /// État d'ordonnancement MaiSched.
    pub sched: SchedEntity,
}

pub struct RestartInfo {
    pub argument: alloc::boxed::Box<dyn core::any::Any + Send + Sync + 'static>,
    pub func:     alloc::boxed::Box<dyn core::any::Any + Send + Sync + 'static>,
}

impl Task {
    /// Creates a new Task structure.
    pub fn new(
        id: usize,
        name: String,
        kstack: Stack,
        mmi: MmiRef,
        namespace: Arc<CrateNamespace>,
        env: Arc<Mutex<Environment>>,
        app_crate: Option<Arc<AppCrateRef>>,
        tls_area: TlsDataImage,
    ) -> Task {
        Task {
            id,
            name,
            runstate: AtomicRunState::new(RunState::Runnable),
            running_on_cpu: AtomicCell::new(None.into()),
            kstack,
            mmi,
            namespace,
            env,
            app_crate,
            tls_area,
            saved_sp: 0,
            kill_handler: IrqSafeMutex::new(None),
            pinned_core: AtomicCell::new(None.into()),
            context_data: IrqSafeMutex::new(None),
            wakers: IrqSafeMutex::new(Vec::new()),
            exit_waiters: IrqSafeMutex::new(Vec::new()),
            exec_mode: AtomicU8::new(0), // 0 = Native
            #[cfg(target_arch = "x86_64")]
            user: spin::Once::new(),
            is_an_idle_task: false,
            restart_info: None,
            sched: SchedEntity::new(id),
        }
    }

    pub fn block_initing_task(&mut self) -> Result<(), RunState> {
        match self.runstate.compare_exchange(RunState::Initing, RunState::Blocked) {
            Ok(_) => Ok(()),
            Err(s) => Err(s),
        }
    }

    pub fn make_inited_task_runnable(&mut self) -> Result<(), RunState> {
        match self.runstate.compare_exchange(RunState::Initing, RunState::Runnable) {
            Ok(_) => Ok(()),
            Err(s) => Err(s),
        }
    }

    pub fn get_namespace(&self) -> &Arc<CrateNamespace> { &self.namespace }
    pub fn get_environment(&self) -> &Arc<Mutex<Environment>> { &self.env }
    pub fn get_app_crate(&self) -> Option<&Arc<AppCrateRef>> { self.app_crate.as_ref() }
}

impl PartialEq for Task {
    fn eq(&self, other: &Task) -> bool { self.id == other.id }
}
impl Eq for Task { }
impl Hash for Task {
    fn hash<H: Hasher>(&self, state: &mut H) { self.id.hash(state); }
}
impl fmt::Debug for Task {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Task {{ id: {}, name: '{}' }}", self.id, self.name)
    }
}
impl fmt::Display for Task {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}{{{}}}", self.name, self.id)
    }
}

/// État d'une tâche, atomique et **sans verrou** pour les états qui changent sans cesse.
///
/// `RunState` contient la valeur de sortie (16 octets) : `AtomicCell<RunState>` de crossbeam le
/// protégeait par un verrou tournant global (tableau de verrous indexé par l'adresse). Une
/// interruption qui réveillait une tâche (`unblock()` d'une fin NVMe) pendant que le code
/// interrompu, sur le même cœur, était au milieu de son propre `block()` attendait ce verrou pour
/// toujours : le cœur se figeait, interruptions coupées (vu avec `nmi_backtrace` sous `blkbench`).
///
/// Ici l'état tient dans un `AtomicU8` ; seule la valeur de sortie, écrite une fois à la fin de
/// la tâche, est sous un verrou qui coupe les interruptions (un détenteur ne peut donc pas être
/// interrompu sur son cœur).
pub struct AtomicRunState {
    state: core::sync::atomic::AtomicU8,
    exit: IrqSafeMutex<Option<ExitValue>>,
}

const RS_INITING: u8 = 0;
const RS_RUNNABLE: u8 = 1;
const RS_BLOCKED: u8 = 2;
const RS_EXITED: u8 = 3;

impl AtomicRunState {
    pub fn new(s: RunState) -> AtomicRunState {
        let a = AtomicRunState { state: core::sync::atomic::AtomicU8::new(RS_INITING), exit: IrqSafeMutex::new(None) };
        a.store(s);
        a
    }

    fn code(s: RunState) -> u8 {
        match s {
            RunState::Initing => RS_INITING,
            RunState::Runnable => RS_RUNNABLE,
            RunState::Blocked => RS_BLOCKED,
            RunState::Exited(_) => RS_EXITED,
        }
    }

    fn decode(&self, c: u8) -> RunState {
        match c {
            RS_INITING => RunState::Initing,
            RS_RUNNABLE => RunState::Runnable,
            RS_BLOCKED => RunState::Blocked,
            _ => RunState::Exited(self.exit.lock().unwrap_or(ExitValue::Completed(0))),
        }
    }

    pub fn load(&self) -> RunState {
        self.decode(self.state.load(core::sync::atomic::Ordering::Acquire))
    }

    pub fn store(&self, s: RunState) {
        use core::sync::atomic::Ordering::Release;
        if let RunState::Exited(v) = s {
            let mut g = self.exit.lock();
            *g = Some(v);
            self.state.store(RS_EXITED, Release);
        } else {
            self.state.store(Self::code(s), Release);
        }
    }

    /// Comme `AtomicCell::compare_exchange` : `Ok(ancien)` si l'état valait `current`,
    /// `Err(état actuel)` sinon.
    pub fn compare_exchange(&self, current: RunState, new: RunState) -> Result<RunState, RunState> {
        use core::sync::atomic::Ordering::{AcqRel, Acquire};
        let (cc, nc) = (Self::code(current), Self::code(new));
        if cc != RS_EXITED && nc != RS_EXITED {
            // Chemin des réveils et des blocages : un seul `cmpxchg`, sans verrou.
            return self.state.compare_exchange(cc, nc, AcqRel, Acquire).map(|_| current).map_err(|c| self.decode(c));
        }
        // Sortie (ou état de sortie comparé) : valeur et état changent ensemble sous le verrou.
        let mut g = self.exit.lock();
        let cur = self.state.load(Acquire);
        let cur_state = if cur == RS_EXITED { RunState::Exited(g.unwrap_or(ExitValue::Completed(0))) } else { self.decode(cur) };
        if cur_state != current {
            return Err(cur_state);
        }
        if let RunState::Exited(v) = new {
            *g = Some(v);
        }
        self.state.store(nc, core::sync::atomic::Ordering::Release);
        Ok(current)
    }
}

impl core::fmt::Debug for AtomicRunState {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        write!(f, "{:?}", self.load())
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RunState {
    Initing,
    Runnable,
    Blocked,
    Exited(ExitValue),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ExitValue {
    Completed(isize),
    Killed(KillReason),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum KillReason {
    Requested,
    Panic,
    Exception(u8),
}

pub enum InheritedStates<'t> {
    FromTask(&'t Task),
    Custom {
        mmi: MmiRef,
        namespace: Arc<CrateNamespace>,
        env: Arc<Mutex<Environment>>,
        app_crate: Option<Arc<AppCrateRef>>,
    }
}
