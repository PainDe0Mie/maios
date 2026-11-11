//! MaiProc — processus en mode utilisateur (anneau 3).
//!
//! Les applications natives de MaiOS restent dans l'espace d'adresses unique du noyau (modèle
//! Theseus). Les binaires étrangers (Linux, plus tard Windows) s'exécutent ici, en anneau 3, chacun
//! dans son propre espace d'adresses ([`uvm::Mm`]) :
//!
//! - un **fil** ([`Thread`]) est porté par une tâche noyau ordinaire (ordonnancée par MaiSched) ;
//!   elle passe en mode utilisateur par [`uentry::run_user`] et en revient par
//!   [`uentry::exit_user`] quand le fil se termine, puis finit normalement (nettoyage) ;
//! - le fil courant se retrouve sans recherche : son adresse est posée juste au-dessus du cadre
//!   d'entrée, au sommet de sa pile noyau ([`uentry::thread_slot`]) ;
//! - un **processus** ([`Process`]) regroupe des fils, un espace d'adresses, une table de
//!   descripteurs, des dispositions de signaux ; il a un parent, des enfants, un groupe et une
//!   session, et devient **zombie** à sa fin jusqu'à ce que son parent recueille son statut ;
//! - aucune donnée utilisateur n'est jamais lue par pointeur direct : tout passe par les copies de
//!   [`uvm`] (validation par zone puis accès par la physmap), ce qui permet SMAP sans `stac`.
//!
//! `fork` partage les tables de pages ([`uvm::Mm::fork`], On-Demand-Fork), `clone` crée des fils
//! qui partagent l'espace, `execve` remplace l'image en place ([`exec`]). Signaux : [`signal`] ;
//! attentes : [`wait`] ; futex : [`futex`] ; tubes : [`pipe`].
//!
//! Personnalité : [`linux`] (ABI x86_64, appels système par numéro Linux).

#![no_std]
#![cfg(target_arch = "x86_64")]
#![feature(naked_functions)]

extern crate alloc;

mod elf;
pub mod epoll;
pub mod eventfd;
pub mod exec;
pub mod creds;
pub mod userns;
pub mod mount;
pub mod ns;
pub mod netns;
pub mod pidns;
pub mod fd;
mod reclock;
pub(crate) mod cputimer;
pub(crate) mod ptimer;
pub(crate) mod inotify;
mod fifo;
pub(crate) mod pty;
mod sysstats;
mod console;
mod netlink;
mod oom;
mod flock;
pub mod ipc;
mod seals;
pub mod strace;
pub mod futex;
pub mod loadavg;
pub mod object;
pub mod pagecache;
pub mod pidfd;
pub mod poll;
pub mod procfs;
pub mod signalfd;
pub mod socket;
pub mod inet;
pub mod uring;
pub mod netpoll;
pub mod portsel;
pub mod timerfd;
pub mod unix;
pub mod vdso;
mod linux;
pub mod pipe;
pub mod signal;
mod signal32;
mod stack;
pub mod wait;

use alloc::collections::BTreeMap;
use alloc::{string::String, sync::Arc, sync::Weak, vec::Vec};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use fs_node::DirRef;
use log::warn;
use spin::Mutex;
use sync_irq::IrqSafeMutex;
use uentry::{FpuState, UserRegs};
use uvm::Mm;

pub use fd::FdTable;
pub use signal::{SigAction, SigHand, SigInfo, SigQueue};
use wait::{Completion, WaitQueue};

/// Statut d'attente (format de `wait4`) : sortie normale.
pub const fn exit_status(code: i32) -> i32 { (code & 0xff) << 8 }
/// Statut d'attente : tué par le signal `sig`.
pub const fn signal_status(sig: i32) -> i32 { sig & 0x7f }

/// Mot `work` d'un fil : un signal peut être livrable.
pub const WORK_SIGNAL: u32 = 1;
/// Mot `work` : le fil doit se terminer (fin du groupe, `execve` d'un autre fil).
pub const WORK_EXIT: u32 = 2;

const STATE_RUNNING: u8 = 0;
const STATE_ZOMBIE: u8 = 1;
const STATE_REAPED: u8 = 2;

/// État d'arrêt d'un processus (contrôle des tâches) et événements à rapporter au parent.
#[derive(Default)]
pub struct StopState {
    pub stopped: bool,
    pub code: i32,
    /// Arrêt pas encore rapporté par `wait4(WUNTRACED)`.
    pub report_stop: bool,
    /// Reprise pas encore rapportée par `wait4(WCONTINUED)`.
    pub report_cont: bool,
}

/// Minuteur `ITIMER_REAL` (`alarm`, `setitimer`).
#[derive(Default)]
pub struct RealTimer {
    /// Génération : un réveil d'une génération ancienne est ignoré (minuteur réarmé ou annulé).
    pub gen: u64,
    pub deadline: Option<time::Instant>,
    pub interval: core::time::Duration,
}

/// Processus : un espace d'adresses, une table de fichiers, des fils.
pub struct Process {
    /// Numéro **global** (identifiant de la tâche du fil chef). Toute valeur rendue à
    /// l'utilisateur passe par [`Process::vnr`] de l'observateur (espaces pid, [`pidns`]).
    pub pid: usize,
    /// Espace pid du processus (`task_active_pid_ns`), fixé à la création.
    pub pid_ns: Arc<pidns::PidNs>,
    /// Numéro du processus dans son propre espace (`getpid`).
    pub vpid: usize,
    /// Attaches aux numéros : pid, groupe, session (`pid_links`) ; rendues au recueil.
    pids: Mutex<ProcPids>,
    /// Parent (vide : parent natif ou processus orphelin).
    pub parent: Mutex<Weak<Process>>,
    /// Numéros globaux du parent, du groupe et de la session.
    pub ppid: AtomicUsize,
    pub pgid: AtomicUsize,
    pub sid: AtomicUsize,
    /// Init d'un espace pid descendant (`SIGNAL_UNKILLABLE`) : seuls les signaux qu'il gère lui
    /// parviennent de son espace ; sa fin tue l'espace.
    pub unkillable: AtomicBool,
    /// `PR_SET_CHILD_SUBREAPER` : recueille les orphelins de sa descendance.
    pub child_subreaper: AtomicBool,
    /// Un ancêtre (dans le même espace pid) est *subreaper* (hérité par `fork`).
    pub has_child_subreaper: AtomicBool,
    /// Le parent est une tâche native (`run`) : il recueille le statut par [`ProcessHandle`].
    native_parent: bool,
    /// `/proc/<pid>/oom_score_adj` (−1000 : jamais tué, 1000 : tué en premier), hérité par
    /// `fork` ; pèse dans le choix de la victime de [`oom`].
    pub oom_score_adj: AtomicI32,
    mm: Mutex<Option<Arc<Mm>>>,
    pub files: Arc<Mutex<FdTable>>,
    /// Racine et dossier courant (`fs_struct`, partagé avec `CLONE_FS`).
    pub fs: mount::FsRef,
    /// Espaces de noms (montage, UTS, IPC, cgroup… ; l'espace utilisateur est dans `creds`).
    pub nsproxy: Mutex<Arc<ns::Nsproxy>>,
    /// `PR_SET_NO_NEW_PRIVS` : `execve` n'accorde plus aucun privilège ; hérité, irréversible.
    pub no_new_privs: AtomicBool,
    /// Chemin de l'exécutable (`/proc/self/exe`).
    pub exe: Mutex<String>,
    pub umask: AtomicU32,
    /// Identités (uid, gid, groupes), voir `creds`.
    pub creds: Mutex<creds::Creds>,
    /// Terminal de contrôle (pseudo-terminal) de la session, hérité par `fork`, quitté par
    /// `setsid`.
    pub ctty: Mutex<Option<Weak<pty::Pty>>>,
    sighand: IrqSafeMutex<Arc<SigHand>>,
    /// Fils du processus.
    threads: IrqSafeMutex<Vec<Weak<Thread>>>,
    pub live_threads: AtomicUsize,
    /// Statut de sortie (format `wait4`) ; -1 tant qu'il n'est pas fixé.
    pub exit_status: AtomicI32,
    /// Le groupe de fils se termine (`exit_group`, signal fatal).
    pub group_exit: AtomicBool,
    state: AtomicU8,
    /// Signal envoyé au parent à la fin (`SIGCHLD` en général ; 0 : aucun). Remis à `SIGCHLD`
    /// quand un orphelin change de parent (`reparent_leader`).
    pub exit_signal: AtomicI32,
    children: Mutex<Vec<Arc<Process>>>,
    /// Les fils de ce processus y attendent leurs enfants (`wait4`).
    pub child_wait: WaitQueue,
    /// Fin du processus (parent natif) et fin de fils (`execve`).
    pub exit_wait: WaitQueue,
    pub shared_pending: IrqSafeMutex<SigQueue>,
    /// Réveillée à chaque signal mis en file (fil ou processus) : lecteurs de `signalfd`.
    pub signal_wait: WaitQueue,
    pub stop: IrqSafeMutex<StopState>,
    pub stop_wait: WaitQueue,
    pub real_timer: IrqSafeMutex<RealTimer>,
    /// `ITIMER_VIRTUAL` et `ITIMER_PROF` (`cputimer`) ; pas hérités par `fork`, gardés par `execve`.
    pub cpu_timers: Mutex<[cputimer::CpuTimer; 2]>,
    /// Minuteurs POSIX (`timer_create`) : ni hérités par `fork`, ni gardés par `execve`.
    pub(crate) ptimers: IrqSafeMutex<ptimer::Timers>,
    /// Temps processeur des fils terminés, et des enfants recueillis (ns).
    pub dead_threads_ns: AtomicU64,
    pub children_ns: AtomicU64,
    /// Instant de création.
    pub started: time::Instant,
    /// Instants (ns, `sysstats::now_ns`) de la demande de fin et du passage à l'état zombie.
    exit_req_ns: AtomicU64,
    exit_tear_ns: AtomicU64,
    zombie_ns: AtomicU64,
    /// Nom court (`comm`, `PR_SET_NAME`).
    pub comm: Mutex<[u8; 16]>,
    /// `[début, fin)` des arguments puis de l'environnement dans la mémoire du processus.
    pub args: Mutex<[usize; 4]>,
    /// Copie les écritures sur le terminal dans le journal du noyau (tests automatiques).
    pub log_output: bool,
    log_line: Mutex<Vec<u8>>,
}

/// Attaches d'un processus aux numéros de pid (`pid_links` de Linux).
#[derive(Default)]
struct ProcPids {
    pid: Option<Arc<pidns::Pid>>,
    pgrp: Option<Arc<pidns::Pid>>,
    session: Option<Arc<pidns::Pid>>,
}

impl Process {
    /// Numéro de `global` vu de ce processus (0 : invisible), `pid_nr_ns`.
    pub fn vnr(&self, global: usize) -> usize {
        self.pid_ns.nr_of(global)
    }

    /// Numéro du processus `p` vu de celui-ci.
    pub fn vnr_of(&self, p: &Process) -> usize {
        self.pid_ns.nr_of(p.pid)
    }

    pub(crate) fn pid_ref(&self) -> Option<Arc<pidns::Pid>> {
        self.pids.lock().pid.clone()
    }

    pub(crate) fn pgrp_ref(&self) -> Option<Arc<pidns::Pid>> {
        self.pids.lock().pgrp.clone()
    }

    pub(crate) fn session_ref(&self) -> Option<Arc<pidns::Pid>> {
        self.pids.lock().session.clone()
    }

    /// Change de groupe (`change_pid(PIDTYPE_PGID)`).
    pub(crate) fn set_pgrp(&self, g: Arc<pidns::Pid>) {
        self.pgid.store(g.nr, Ordering::Relaxed);
        self.pids.lock().pgrp = Some(g);
    }

    /// Change de session (`setsid`).
    pub(crate) fn set_session(&self, s: Arc<pidns::Pid>) {
        self.sid.store(s.nr, Ordering::Relaxed);
        self.pids.lock().session = Some(s);
    }

    /// `fs_struct` partagé avec un autre processus (`CLONE_FS`).
    pub fn fs_shared(&self) -> bool {
        self.fs.shared()
    }

    /// Donne au processus son propre `fs_struct` (`unshare(CLONE_FS)`), avec ce contenu.
    pub fn unshare_fs(&self, fs: mount::FsStruct) {
        self.fs.unshare(fs);
    }

    /// Chemin visible du dossier courant.
    pub fn cwd_path(&self) -> String {
        self.fs.with(|f| f.cwd_path())
    }

    /// Copie dans le journal, ligne par ligne (une ligne peut arriver en plusieurs écritures).
    pub(crate) fn log_bytes(&self, bytes: &[u8]) {
        let mut line = self.log_line.lock();
        for &b in bytes {
            if b == b'\n' {
                self.emit_log_line(&line);
                line.clear();
            } else {
                line.push(b);
            }
        }
    }

    fn emit_log_line(&self, line: &[u8]) {
        if !line.is_empty() {
            warn!("[pid {}] {}", self.pid, String::from_utf8_lossy(line));
        }
    }

    /// Mémoire du processus (`None` une fois terminé).
    pub fn mm(&self) -> Option<Arc<Mm>> { self.mm.lock().clone() }

    pub fn sighand(&self) -> Arc<SigHand> { self.sighand.lock().clone() }

    pub fn sigaction(&self, sig: i32) -> SigAction {
        if !(1..=64).contains(&sig) {
            return SigAction::default();
        }
        self.sighand().actions.lock()[sig as usize - 1]
    }

    pub fn is_zombie(&self) -> bool { self.state.load(Ordering::Acquire) != STATE_RUNNING }

    /// Instant du passage à l'état zombie (0 : inconnu, statistiques éteintes à ce moment-là).
    pub(crate) fn zombie_ns(&self) -> u64 { self.zombie_ns.load(Ordering::Relaxed) }

    /// Premier fil vivant pour lequel `f` est vrai (`f` peut aussi servir de simple parcours).
    pub fn for_each_thread_until(&self, mut f: impl FnMut(&Arc<Thread>) -> bool) -> Option<Arc<Thread>> {
        let list: Vec<Arc<Thread>> = self.threads.lock().iter().filter_map(|w| w.upgrade()).collect();
        list.into_iter().find(|t| f(t))
    }

    /// Retire `sigs` de toutes les files (processus et fils).
    pub fn discard_pending(&self, sigs: u64) {
        self.shared_pending.lock().discard(sigs);
        self.for_each_thread_until(|t| {
            t.pending.lock().discard(sigs);
            false
        });
    }

    /// Enfants (copie).
    pub fn children(&self) -> Vec<Arc<Process>> { self.children.lock().clone() }

    /// Temps processeur consommé par les fils vivants et terminés (ns).
    pub fn cpu_ns(&self) -> u64 {
        let mut ns = self.dead_threads_ns.load(Ordering::Relaxed);
        self.for_each_thread_until(|t| {
            ns += t.cpu_ns();
            false
        });
        ns
    }
}

/// État du premier passage d'un fil en mode utilisateur (consommé par `prepare_entry`).
struct StartState {
    regs: UserRegs,
    fpu: FpuState,
    seg: uentry::SegState,
    me: Arc<Thread>,
}

/// Contexte utilisateur de la tâche courante (état vectoriel, segments).
pub(crate) fn user_context() -> Option<&'static task_struct::UserContext> {
    let task = task::get_my_current_task()?;
    let ctx = task.user.get()? as *const task_struct::UserContext;
    // SAFETY : le contexte vit autant que la tâche, qui est la tâche courante (elle ne peut
    // pas se terminer pendant qu'elle exécute ce code).
    Some(unsafe { &*ctx })
}

/// Fil d'exécution utilisateur.
pub struct Thread {
    /// Numéro global (identifiant de la tâche).
    tid: AtomicUsize,
    /// Numéro du fil dans l'espace pid de son processus (`gettid`, mot des futex).
    vtid: AtomicUsize,
    /// Attache au numéro du fil, rendue à sa fin (celle du chef est aussi celle du processus).
    pid_ref: Mutex<Option<Arc<pidns::Pid>>>,
    /// `PR_SET_PDEATHSIG` : signal reçu à la fin du parent ; 0 à `fork`, effacé par un
    /// `execve` set-ID ou un changement d'identité.
    pub pdeath_signal: AtomicI32,
    pub process: Arc<Process>,
    /// Mémoire vue par ce fil (copie de celle du processus, changée par `execve`) ; lue par le seul
    /// fil, sans verrou.
    mm: UnsafeCell<Option<Arc<Mm>>>,
    /// Point de retour posé par `run_user` (écrit et lu par la seule tâche du fil).
    jmp: UnsafeCell<usize>,
    task: spin::Once<task::WeakTaskRef>,
    pub clear_child_tid: AtomicUsize,
    pub robust_list: AtomicUsize,
    pub sigmask: AtomicU64,
    pub pending: IrqSafeMutex<SigQueue>,
    /// Travail à faire avant le retour en mode utilisateur ([`WORK_SIGNAL`], [`WORK_EXIT`]).
    pub work: AtomicU32,
    /// Masque à rétablir après `sigsuspend`/`ppoll` (s'il n'est pas consommé par un cadre).
    pub saved_sigmask: IrqSafeMutex<Option<u64>>,
    pub altstack: IrqSafeMutex<signal::AltStack>,
    /// Dernière faute (vecteur, code d'erreur, CR2) pour le cadre du signal synchrone.
    pub fault_info: IrqSafeMutex<Option<(u64, u64, u64)>>,
    /// Dernière faute qui a donné un signal (vecteur, code d'erreur, adresse, `rip`, base FS),
    /// gardée pour le
    /// diagnostic d'un plantage : un gestionnaire qui relance le signal (`tgkill`) la masque.
    pub last_fault: IrqSafeMutex<Option<(u64, u64, u64, u64, u64)>>,
    /// Numéro du dernier appel système (relance après signal).
    pub last_syscall: AtomicU64,
    /// L'appel système en cours vient de la table i386 (`in_ia32_syscall` de Linux) : les
    /// structures échangées avec l'utilisateur ont alors la disposition 32 bits.
    pub in_compat: AtomicBool,
    /// Liste des verrous robustes au format 32 bits (`compat_robust_list` de Linux).
    pub robust_compat: AtomicBool,
    /// L'appel i386 en cours prend des `timespec`/`timeval` 32 bits (appels « time32 »).
    pub time32: AtomicBool,
    /// Nom court du fil (`PR_SET_NAME`, `/proc/<pid>/task/<tid>/comm`) : propre à chaque fil
    /// comme sous Linux ; celui du fil chef est aussi le nom du processus.
    pub comm: Mutex<[u8; 16]>,
    /// Priorité d'entrées-sorties (`ioprio_set`, classe << 13 | niveau ; 0 = aucune : déduite
    /// du nice, comme Linux). Héritée par `fork` et `clone`.
    pub ioprio: AtomicU32,
    /// Suite d'un appel interrompu (`restart_syscall`).
    pub(crate) restart: IrqSafeMutex<Option<linux::Restart>>,
    /// `vfork` : complété à l'`execve` ou à la fin de l'enfant.
    pub vfork_done: Mutex<Option<Arc<Completion>>>,
    start: Mutex<Option<StartState>>,
}

// SAFETY : `jmp` et `mm` ne sont touchés que par la tâche qui porte le fil.
unsafe impl Sync for Thread {}
unsafe impl Send for Thread {}

impl Thread {
    /// Nom court du fil.
    pub fn comm_string(&self) -> String {
        let c = *self.comm.lock();
        let n = c.iter().position(|&b| b == 0).unwrap_or(c.len());
        String::from_utf8_lossy(&c[..n]).into_owned()
    }

    pub fn tid(&self) -> usize { self.tid.load(Ordering::Relaxed) }

    /// Numéro du fil dans l'espace pid de son processus.
    pub fn vtid(&self) -> usize { self.vtid.load(Ordering::Relaxed) }

    /// Mémoire du fil courant.
    pub fn mm(&self) -> &Mm {
        // SAFETY : seul le fil lit ou remplace son `mm`, jamais pendant qu'une référence vit
        // (`execve` le remplace en fin d'appel, la fin du fil après le dernier appel).
        unsafe { (*self.mm.get()).as_deref().expect("maiproc : fil sans memoire") }
    }

    fn mm_arc(&self) -> Arc<Mm> {
        // SAFETY : voir `mm`.
        unsafe { (*self.mm.get()).clone().expect("maiproc : fil sans memoire") }
    }

    /// Remplace la mémoire du fil (`execve`) ; renvoie l'ancienne.
    ///
    /// # Safety
    /// Appelé par le fil lui-même, sans référence vivante sur l'ancienne mémoire.
    unsafe fn replace_mm(&self, mm: Option<Arc<Mm>>) -> Option<Arc<Mm>> {
        core::mem::replace(&mut *self.mm.get(), mm)
    }

    pub fn task(&self) -> Option<task::TaskRef> { self.task.get().and_then(|w| w.upgrade()) }

    /// Temps processeur du fil (ns).
    pub fn cpu_ns(&self) -> u64 { self.task().map_or(0, |t| t.sched.snapshot().sum_exec_ns) }

    /// Signale du travail au fil et le fait entrer dans le noyau : déblocage s'il dort dans une
    /// attente, IPI s'il tourne en mode utilisateur sur un autre cœur.
    pub fn kick(&self, work: u32) {
        self.work.fetch_or(work, Ordering::AcqRel);
        if let Some(task) = self.task() {
            let _ = task.unblock();
            if let Some(cpu) = Option::<cpu::CpuId>::from(task.running_on_cpu.load()) {
                if cpu != cpu::current_cpu() {
                    task::scheduler::kick_cpu(cpu.value() as usize);
                }
            }
        }
    }

    /// Rattache le fil à la tâche courante (travailleurs d'io_uring : [`io_worker_thread`]).
    pub(crate) fn bind_current_task(&self) {
        if let Some(task) = task::get_my_current_task() {
            self.task.call_once(|| task.downgrade());
        }
    }

    /// Un signal est livrable, ou le fil doit se terminer (attentes interruptibles).
    pub fn signal_pending(&self) -> bool { signal::deliverable(self) }
}

// --- Tables globales -----------------------------------------------------------------------------

static PROCESSES: Mutex<BTreeMap<usize, Weak<Process>>> = Mutex::new(BTreeMap::new());
static THREADS: Mutex<BTreeMap<usize, Weak<Thread>>> = Mutex::new(BTreeMap::new());

/// Processus de PID `pid` (vivant ou zombie non recueilli).
pub fn find_process(pid: usize) -> Option<Arc<Process>> {
    PROCESSES.lock().get(&pid).and_then(|w| w.upgrade())
}

/// Fil de TID `tid`.
pub fn find_thread(tid: usize) -> Option<Arc<Thread>> {
    THREADS.lock().get(&tid).and_then(|w| w.upgrade())
}

/// Processus de numéro `nr` dans l'espace pid de `t` (`find_task_by_vpid`) : un pid venu de
/// l'utilisateur passe toujours par ici, jamais par [`find_process`].
pub fn find_process_v(t: &Thread, nr: usize) -> Option<Arc<Process>> {
    t.process.pid_ns.global_of(nr).and_then(find_process)
}

/// Fil de numéro `nr` dans l'espace pid de `t`.
pub fn find_thread_v(t: &Thread, nr: usize) -> Option<Arc<Thread>> {
    t.process.pid_ns.global_of(nr).and_then(find_thread)
}

/// Processus vivants que voit l'espace pid `ns`.
pub fn processes_in(ns: &pidns::PidNs) -> Vec<Arc<Process>> {
    if ns.level == 0 {
        return all_processes();
    }
    ns.numbers().into_iter().filter_map(|(_, g)| find_process(g)).filter(|p| !p.is_zombie()).collect()
}

/// Envoie le signal `sig` au processus `pid` depuis le noyau (gestionnaire des tâches) :
/// `SIGTERM`, `SIGKILL`, `SIGSTOP`/`SIGCONT`… avec la même sémantique qu'un `kill(2)`.
pub fn signal_process(pid: usize, sig: i32) -> Result<(), &'static str> {
    if !(1..=64).contains(&sig) {
        return Err("signal invalide");
    }
    let p = find_process(pid).ok_or("processus introuvable")?;
    if p.is_zombie() {
        return Err("processus deja termine");
    }
    signal::send_to_process(&p, SigInfo::new(sig, signal::SI_KERNEL));
    Ok(())
}

impl Process {
    /// Le processus est arrêté (`SIGSTOP`, `SIGTSTP`…).
    pub fn is_stopped(&self) -> bool { self.stop.lock().stopped }

    /// Fils vivants (copie).
    pub fn threads(&self) -> Vec<Arc<Thread>> { self.threads.lock().iter().filter_map(|w| w.upgrade()).collect() }

    /// Nom court (`comm`).
    pub fn comm_string(&self) -> String {
        let c = *self.comm.lock();
        let n = c.iter().position(|&b| b == 0).unwrap_or(c.len());
        String::from_utf8_lossy(&c[..n]).into_owned()
    }
}

/// Tous les processus vivants.
pub fn all_processes() -> Vec<Arc<Process>> {
    PROCESSES.lock().values().filter_map(|w| w.upgrade()).filter(|p| !p.is_zombie()).collect()
}

/// Fil courant, depuis le cadre d'une entrée en mode utilisateur.
#[inline(always)]
pub(crate) fn current(regs: &UserRegs) -> &Thread {
    // SAFETY : `prepare_entry` a posé l'adresse du fil au-dessus du cadre ; la tâche garde le fil
    // vivant tant qu'il peut entrer dans le noyau.
    unsafe { &*(*uentry::thread_slot(regs) as *const Thread) }
}

/// Processus de la tâche courante (lectures de `/proc` sans fil sous la main).
pub(crate) fn current_process() -> Option<Arc<Process>> {
    let id = task::get_my_current_task()?.id;
    find_thread(id).map(|t| t.process.clone())
}

/// `Arc` du fil `t` (toujours un fil vivant, venu de [`current`]).
pub(crate) fn current_thread_arc(t: &Thread) -> Option<Arc<Thread>> {
    let ptr = t as *const Thread;
    // SAFETY : tout `Thread` vit dans un `Arc` (créé par `new_thread`), que la tâche garde.
    unsafe {
        Arc::increment_strong_count(ptr);
        Some(Arc::from_raw(ptr))
    }
}

/// Termine le fil courant (après que l'appelant a libéré tout ce qu'il possède sur la pile).
fn exit_current(thread: *const Thread, status: i32) -> ! {
    // SAFETY : `jmp` a été posé par `run_user` pour ce fil, sur cette pile.
    unsafe { uentry::exit_user(*(*thread).jmp.get(), status as u32 as u64) }
}

// --- Crochets d'entrée ----------------------------------------------------------------------------

static HOOKS: spin::Once<()> = spin::Once::new();

fn install_hooks() {
    HOOKS.call_once(|| {
        uentry::set_syscall_hook(syscall_hook);
        uentry::set_fault_hook(fault_hook);
        uentry::set_return_hook(signal::return_hook);
        mount_sysroot();
    });
}

/// Racine des fichiers Linux livrés avec MaiOS (`extra_files/sysroot` : chargeur dynamique,
/// glibc…).
const SYSROOT: &str = "/extra_files/sysroot";

/// Rend visibles à la racine les dossiers de [`SYSROOT`] (`/lib64`, `/lib`, `/usr`, `/etc`…),
/// là où les binaires Linux les cherchent (`PT_INTERP = /lib64/ld-linux-x86-64.so.2`). Comme un
/// montage par liaison : le même nœud est inscrit dans deux dossiers, sans copie ; un dossier qui
/// existe déjà est fusionné entrée par entrée.
fn mount_sysroot() {
    // `/tmp` et `/dev/shm` (FHS) : volumes tmpfs (fichiers creux, vrais inodes, quota), en
    // `1777` comme sous Linux. `/dev/shm` porte `shm_open` et les sémaphores nommés de la glibc
    // (`sem_open`, dont dépend `multiprocessing` de Python).
    tmpfs::set_clock(|| {
        let (sec, nsec) = linux::sys::realtime();
        fs_node::Timespec { sec, nsec: nsec as u32 }
    });
    let root = root::get_root().clone();
    if root.lock().get("tmp").is_none() {
        if let Err(e) = tmpfs::mount("tmp", &root, 0o1777, None) {
            log::error!("maiproc : tmpfs sur /tmp impossible : {e}");
        }
    }
    let existing = root.lock().get("dev");
    let dev = match existing {
        Some(fs_node::FileOrDir::Dir(d)) => Some(d),
        Some(_) => None,
        // `/dev` : tmpfs de périphériques (`devtmpfs`), 1 Mio.
        None => tmpfs::mount("dev", &root, 0o755, Some(1 << 20)).ok(),
    };
    let mut shm = None;
    if let Some(dev) = &dev {
        populate_dev(dev);
        let existing = dev.lock().get("shm");
        shm = match existing {
            Some(fs_node::FileOrDir::Dir(d)) => Some(d),
            _ => match tmpfs::mount("shm", dev, 0o1777, None) {
                Ok(d) => Some(d),
                Err(e) => {
                    log::error!("maiproc : tmpfs sur /dev/shm impossible : {e}");
                    None
                }
            },
        };
    }
    let tmp = match root.lock().get("tmp") {
        Some(fs_node::FileOrDir::Dir(d)) => Some(d),
        _ => None,
    };
    mount::init(dev, shm, tmp);
    let p: &path::Path = SYSROOT.as_ref();
    let Some(fs_node::FileOrDir::Dir(src)) = p.get(root::get_root()) else { return };
    let root = root::get_root().clone();
    let n = merge_dir(&src, &root);
    if n > 0 {
        log::info!("maiproc : {} entrees de {} visibles a la racine", n, SYSROOT);
    }
}

/// Nœuds de `/dev` (ceux d'un `devtmpfs` de Linux) : périphériques caractère, liens vers
/// `/proc/self/fd`, point de montage de `devpts`. Un `/dev` sans nœuds spéciaux (VFS en mémoire)
/// est laissé tel quel.
fn populate_dev(dev: &DirRef) {
    use fs_node::NodeKind;
    const NODES: [(&str, u16, (u32, u32)); 8] = [
        ("null", 0o666, (1, 3)),
        ("zero", 0o666, (1, 5)),
        ("full", 0o666, (1, 7)),
        ("random", 0o666, (1, 8)),
        ("urandom", 0o666, (1, 9)),
        ("tty", 0o666, (5, 0)),
        ("console", 0o600, (5, 1)),
        ("ptmx", 0o666, (5, 2)),
    ];
    for (name, perm, rdev) in NODES {
        if dev.lock().get(name).is_some() {
            continue;
        }
        let made = dev.lock().create_special(name, NodeKind::CharDev, perm, rdev);
        if made.is_none() {
            return;
        }
    }
    for (name, target) in [("fd", "/proc/self/fd"), ("stdin", "/proc/self/fd/0"), ("stdout", "/proc/self/fd/1"), ("stderr", "/proc/self/fd/2")] {
        if dev.lock().get(name).is_none() {
            let _ = dev.lock().create_symlink(name, target);
        }
    }
    if dev.lock().get("pts").is_none() {
        let _ = dev.lock().create_dir("pts", 0o755);
    }
}

/// Dossier d'exécution de l'utilisateur `uid` (`XDG_RUNTIME_DIR` : `/run/user/<uid>`, `0700`),
/// créé au besoin, avec `/run` en tmpfs (`0755`) comme sous Linux. Renvoie son chemin.
pub fn runtime_dir(uid: u32) -> Result<alloc::string::String, &'static str> {
    use fs_node::FileOrDir;
    fn subdir(parent: &DirRef, name: &str, perm: u16) -> Result<DirRef, &'static str> {
        // Le verrou du parent est relâché avant toute création (voir les règles du VFS).
        let existing = parent.lock().get(name);
        match existing {
            Some(FileOrDir::Dir(d)) => Ok(d),
            Some(FileOrDir::File(_)) => Err("un fichier occupe le chemin du dossier"),
            None => {
                let created = parent.lock().create_dir(name, perm);
                match created {
                    Some(r) => r,
                    None => vfs_node::VFSDirectory::create(alloc::string::String::from(name), parent),
                }
            }
        }
    }
    let root = root::get_root().clone();
    let existing = root.lock().get("run");
    let run = match existing {
        Some(FileOrDir::Dir(d)) => d,
        Some(FileOrDir::File(_)) => return Err("/run n'est pas un dossier"),
        None => tmpfs::mount("run", &root, 0o755, None)?,
    };
    mount::register_root_mount("/run", run.clone(), "tmpfs", mount::MNT_NOSUID | mount::MNT_NODEV | mount::MNT_RELATIME);
    let user = subdir(&run, "user", 0o755)?;
    let name = alloc::format!("{uid}");
    subdir(&user, &name, 0o700)?;
    Ok(alloc::format!("/run/user/{uid}"))
}

/// Inscrit dans `dst` les entrées de `src` absentes ; fusionne les sous-dossiers communs.
/// Aucun verrou de dossier n'est tenu pendant l'accès à un enfant.
fn merge_dir(src: &DirRef, dst: &DirRef) -> usize {
    let names = src.lock().list();
    let mut added = 0;
    for name in names {
        let Some(node) = src.lock().get(&name) else { continue };
        let existing = dst.lock().get(&name);
        match (node, existing) {
            (n, None) => {
                if dst.lock().insert(n).is_ok() {
                    added += 1;
                }
            }
            (fs_node::FileOrDir::Dir(s), Some(fs_node::FileOrDir::Dir(d))) => added += merge_dir(&s, &d),
            _ => {}
        }
    }
    added
}

fn syscall_hook(regs: &mut UserRegs) {
    let thread = current(regs) as *const Thread;
    // SAFETY : voir `current`.
    let t = unsafe { &*thread };
    let compat = uentry::is_syscall32_vector(regs.vector);
    let outcome = if compat {
        regs.rax &= 0xFFFF_FFFF;
        t.last_syscall.store(regs.rax, Ordering::Relaxed);
        t.in_compat.store(true, Ordering::Relaxed);
        let o = linux::compat::dispatch(t, regs);
        t.in_compat.store(false, Ordering::Relaxed);
        o
    } else {
        t.last_syscall.store(regs.rax, Ordering::Relaxed);
        linux::dispatch(t, regs)
    };
    match outcome {
        linux::Outcome::Return(v) => regs.rax = v as u64,
        linux::Outcome::Keep => {}
        linux::Outcome::Exit(status) => exit_current(thread, status),
    }
}

/// Signal Linux, `si_code` et description d'une exception x86.
fn exception_signal(vector: u8) -> (i32, i32, &'static str) {
    match vector {
        0 => (signal::SIGFPE, 1, "division par zero"),  // FPE_INTDIV
        1 | 3 => (signal::SIGTRAP, 1, "point d'arret"), // TRAP_BRKPT
        6 => (signal::SIGILL, 2, "instruction illegale"), // ILL_ILLOPN
        7 | 16 | 19 => (signal::SIGFPE, 7, "exception flottante"), // FPE_FLTINV
        17 => (signal::SIGBUS, 1, "acces non aligne"),  // BUS_ADRALN
        13 => (signal::SIGSEGV, signal::SI_KERNEL, "protection generale"),
        14 => (signal::SIGSEGV, signal::SEGV_MAPERR, "erreur de segmentation"),
        _ => (signal::SIGSEGV, signal::SI_KERNEL, "exception"),
    }
}

fn fault_hook(regs: &mut UserRegs, vector: u8, cr2: u64) {
    // Faute d'un fil utilisateur : traitée interruptions actives (la mémoire du processus peut
    // demander une invalidation sur d'autres cœurs).
    // SAFETY : sur la pile noyau du fil, rien n'est tenu.
    unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
    let t = current(regs);
    let mut code_override = None;
    if vector == 14 {
        let err = regs.error_code;
        let start = if sysstats::enabled() { sysstats::now_ns() } else { 0 };
        let mut r = t.mm().handle_fault(cr2 as usize, err & 2 != 0, err & 16 != 0);
        // Plus de frame : le tueur de mémoire épuisée libère de la place, puis on rejoue la
        // faute (au plus quelques fois) ; si c'est nous la victime, notre `SIGKILL` nous attend
        // au retour vers l'espace utilisateur.
        let mut tries = 0;
        while matches!(r, Err(uvm::VmError::NoMemory)) && tries < 4 {
            if !oom::out_of_memory(&t) {
                return;
            }
            tries += 1;
            r = t.mm().handle_fault(cr2 as usize, err & 2 != 0, err & 16 != 0);
        }
        if start != 0 {
            sysstats::record_fault(start);
        }
        match r {
            Ok(()) => return,
            Err(uvm::VmError::Access) => code_override = Some(signal::SEGV_ACCERR),
            Err(_) => {}
        }
    }
    let (sig, mut code, what) = exception_signal(vector);
    if let Some(c) = code_override {
        code = c;
    }
    *t.fault_info.lock() = Some((vector as u64, regs.error_code, cr2));
    *t.last_fault.lock() = Some((vector as u64, regs.error_code, if vector == 14 { cr2 } else { 0 }, regs.rip, regs.fs_base));
    if t.process.sigaction(sig).handler == signal::SIG_DFL {
        let msg = alloc::format!(
            "[maiproc] pid {} : {} (vecteur {}, adresse {:#x}, rip {:#x}, code {:#x}) -> signal {}\n",
            t.process.pid, what, vector, cr2, regs.rip, regs.error_code, sig
        );
        warn!("{}", msg.trim_end());
        if let Some(f) = t.process.files.lock().get(2) {
            f.write_console(msg.as_bytes());
        }
    }
    let addr = if vector == 14 { cr2 } else { regs.rip };
    if let Some(me) = current_thread_arc(t) {
        signal::send_to_thread(&me, SigInfo::fault(sig, code, addr), true);
    }
    // Le signal est livré au retour (`signal::return_hook`).
}

// --- Création -----------------------------------------------------------------------------------

/// Demande de création d'un processus Linux.
pub struct SpawnRequest {
    /// Chemin absolu de l'exécutable.
    pub path: String,
    /// Contenu du fichier ELF.
    pub image: Arc<[u8]>,
    pub argv: Vec<String>,
    pub envp: Vec<String>,
    pub cwd: DirRef,
    pub cwd_path: String,
    /// Entrées/sorties standard (celles du terminal de l'appelant).
    pub stdio: Option<app_io::IoStreams>,
    /// Identifiant du parent (tâche appelante).
    pub ppid: usize,
    /// Copie la sortie du processus dans le journal du noyau.
    pub log_output: bool,
}

/// Les processus en mode utilisateur sont-ils possibles sur cette machine ?
pub fn available() -> bool {
    uentry::user_mode_available()
}

/// Processus créé par une tâche native : son statut se recueille par [`ProcessHandle::wait`].
pub struct ProcessHandle {
    process: Arc<Process>,
}

impl ProcessHandle {
    pub fn pid(&self) -> usize { self.process.pid }

    /// Attend la fin du processus et renvoie son statut (format `wait4`).
    pub fn wait(&self) -> i32 {
        let p = &self.process;
        let _ = wait::wait_event(&[&p.exit_wait], None, None, || p.is_zombie().then_some(()));
        reap(p);
        p.exit_status.load(Ordering::Acquire)
    }
}

/// Tâche noyau créée bloquée, en attente de son fil.
struct NewTask {
    task: task::JoinableTaskRef,
    slot: Arc<Mutex<Option<Arc<Thread>>>>,
}

fn new_task(name: String) -> Result<NewTask, &'static str> {
    let slot: Arc<Mutex<Option<Arc<Thread>>>> = Arc::new(Mutex::new(None));
    let task = spawn::new_task_builder(thread_main, slot.clone()).name(name).block().spawn()?;
    // Les cœurs permis (`sched_setaffinity`) passent du créateur au fil neuf (`clone`, `fork`).
    let _ = task::with_current_task(|cur| task.sched.cpus_allowed.copy_from(&cur.sched.cpus_allowed));
    Ok(NewTask { task, slot })
}

/// Fil neuf (pas encore démarré).
fn new_thread(tid: usize, pid: Option<Arc<pidns::Pid>>, process: Arc<Process>, mm: Arc<Mm>, sigmask: u64, altstack: signal::AltStack) -> Arc<Thread> {
    let comm = *process.comm.lock();
    Arc::new(Thread {
        tid: AtomicUsize::new(tid),
        vtid: AtomicUsize::new(pid.as_ref().map_or(tid, |p| p.vnr())),
        pid_ref: Mutex::new(pid),
        pdeath_signal: AtomicI32::new(0),
        process,
        mm: UnsafeCell::new(Some(mm)),
        jmp: UnsafeCell::new(0),
        task: spin::Once::new(),
        clear_child_tid: AtomicUsize::new(0),
        robust_list: AtomicUsize::new(0),
        sigmask: AtomicU64::new(sigmask),
        pending: IrqSafeMutex::new(SigQueue::new()),
        work: AtomicU32::new(0),
        saved_sigmask: IrqSafeMutex::new(None),
        altstack: IrqSafeMutex::new(altstack),
        fault_info: IrqSafeMutex::new(None),
        last_fault: IrqSafeMutex::new(None),
        last_syscall: AtomicU64::new(0),
        in_compat: AtomicBool::new(false),
        robust_compat: AtomicBool::new(false),
        time32: AtomicBool::new(false),
        comm: Mutex::new(comm),
        ioprio: AtomicU32::new(0),
        restart: IrqSafeMutex::new(None),
        vfork_done: Mutex::new(None),
        start: Mutex::new(None),
    })
}

/// Fil interne d'un processus pour les travailleurs d'io_uring (les `PF_IO_WORKER` de Linux) :
/// il porte la mémoire, les descripteurs et l'identité du processus pour exécuter ses
/// opérations, mais n'entre jamais en mode utilisateur et n'est pas inscrit parmi ses fils
/// (aucun signal du processus ne lui est adressé ; tous sont masqués). La fin du processus ou
/// `WORK_EXIT` interrompt ses attentes.
pub(crate) fn io_worker_thread(process: Arc<Process>, mm: Arc<Mm>) -> Arc<Thread> {
    new_thread(0, None, process, mm, !0, signal::AltStack::default())
}

/// Démarre `thread` sur la tâche `nt` avec les registres `regs`, l'état vectoriel `fpu` et les
/// segments `seg`.
fn launch(nt: NewTask, thread: Arc<Thread>, regs: UserRegs, fpu: FpuState, seg: uentry::SegState) -> Result<(), &'static str> {
    thread.task.call_once(|| nt.task.downgrade());
    *thread.start.lock() = Some(StartState { regs, fpu, seg, me: thread.clone() });
    // La case d'abord : dès son inscription, un signal peut débloquer la tâche.
    *nt.slot.lock() = Some(thread.clone());
    THREADS.lock().insert(thread.tid(), Arc::downgrade(&thread));
    thread.process.threads.lock().push(Arc::downgrade(&thread));
    drop(thread);
    nt.task.unblock().map_err(|_| "demarrage du fil impossible")?;
    // La tâche est orpheline : elle se libère seule à sa fin (le statut passe par le processus).
    drop(nt.task);
    Ok(())
}

/// Description d'un nouveau processus (création initiale ou `fork`).
struct ProcessSpec {
    /// Numéros du processus (celui de son fil chef), de son groupe et de sa session.
    pid: Arc<pidns::Pid>,
    pgrp: Arc<pidns::Pid>,
    session: Arc<pidns::Pid>,
    parent: Option<Arc<Process>>,
    ppid: usize,
    /// Un ancêtre est *subreaper* (`has_child_subreaper`).
    has_subreaper: bool,
    mm: Arc<Mm>,
    files: Arc<Mutex<FdTable>>,
    fs: mount::FsRef,
    nsproxy: Arc<ns::Nsproxy>,
    no_new_privs: bool,
    exe: String,
    umask: u32,
    creds: creds::Creds,
    ctty: Option<Weak<pty::Pty>>,
    sighand: Arc<SigHand>,
    exit_signal: i32,
    comm: [u8; 16],
    log_output: bool,
    /// Créé par une tâche native qui recueillera le statut.
    native_parent: bool,
}

fn new_process(s: ProcessSpec) -> Arc<Process> {
    let native_parent = s.native_parent;
    let pid_ns = s.pid.ns.clone();
    let vpid = s.pid.vnr();
    // Premier processus d'un espace descendant : son init (`is_child_reaper`).
    let init = pid_ns.level > 0 && vpid == 1;
    let p = Arc::new(Process {
        pid: s.pid.nr,
        pid_ns,
        vpid,
        parent: Mutex::new(s.parent.as_ref().map_or_else(Weak::new, Arc::downgrade)),
        ppid: AtomicUsize::new(s.ppid),
        pgid: AtomicUsize::new(s.pgrp.nr),
        sid: AtomicUsize::new(s.session.nr),
        pids: Mutex::new(ProcPids { pid: Some(s.pid), pgrp: Some(s.pgrp), session: Some(s.session) }),
        unkillable: AtomicBool::new(init),
        child_subreaper: AtomicBool::new(false),
        has_child_subreaper: AtomicBool::new(s.has_subreaper),
        native_parent,
        oom_score_adj: AtomicI32::new(0),
        mm: Mutex::new(Some(s.mm)),
        files: s.files,
        fs: s.fs,
        nsproxy: Mutex::new(s.nsproxy),
        no_new_privs: AtomicBool::new(s.no_new_privs),
        exe: Mutex::new(s.exe),
        umask: AtomicU32::new(s.umask),
        creds: Mutex::new(s.creds),
        ctty: Mutex::new(s.ctty),
        sighand: IrqSafeMutex::new(s.sighand),
        threads: IrqSafeMutex::new(Vec::new()),
        live_threads: AtomicUsize::new(1),
        exit_status: AtomicI32::new(-1),
        group_exit: AtomicBool::new(false),
        state: AtomicU8::new(STATE_RUNNING),
        exit_signal: AtomicI32::new(s.exit_signal),
        children: Mutex::new(Vec::new()),
        child_wait: WaitQueue::new(),
        exit_wait: WaitQueue::new(),
        shared_pending: IrqSafeMutex::new(SigQueue::new()),
        signal_wait: WaitQueue::new(),
        stop: IrqSafeMutex::new(StopState::default()),
        stop_wait: WaitQueue::new(),
        real_timer: IrqSafeMutex::new(RealTimer::default()),
        cpu_timers: Mutex::new([cputimer::CpuTimer::default(); 2]),
        ptimers: IrqSafeMutex::new(ptimer::Timers::default()),
        dead_threads_ns: AtomicU64::new(0),
        children_ns: AtomicU64::new(0),
        started: time::Instant::now(),
        exit_req_ns: AtomicU64::new(0),
        exit_tear_ns: AtomicU64::new(0),
        zombie_ns: AtomicU64::new(0),
        comm: Mutex::new(s.comm),
        args: Mutex::new([0; 4]),
        log_output: s.log_output,
        log_line: Mutex::new(Vec::new()),
    });
    if init {
        p.pid_ns.set_reaper(&p);
    }
    PROCESSES.lock().insert(p.pid, Arc::downgrade(&p));
    if let Some(parent) = &s.parent {
        parent.children.lock().push(p.clone());
    }
    p
}

/// Crée un processus et démarre son fil principal.
pub fn spawn(req: SpawnRequest) -> Result<ProcessHandle, String> {
    if !available() {
        return Err("mode utilisateur indisponible (FSGSBASE requis)".into());
    }
    install_hooks();
    let fs = mount::init_fs(&req.cwd_path);
    let prep = exec::prepare(&req.path, req.image.clone(), &req.argv, &req.envp, &fs.root)?;
    let exe = exe_path(&fs.root, &prep.path);
    let files = FdTable::with_stdio(req.stdio.as_ref());
    let base = req.path.rsplit('/').next().unwrap_or("");
    let nt = new_task(alloc::format!("proc_{}", base)).map_err(String::from)?;
    let pid = nt.task.id;
    let pid_ref = pidns::Pid::alloc(pid, &pidns::init_pid_ns(), &[]).map_err(|_| String::from("numero de processus indisponible"))?;
    let process = new_process(ProcessSpec {
        pid: pid_ref.clone(),
        pgrp: pid_ref.clone(),
        session: pid_ref.clone(),
        parent: None,
        ppid: req.ppid,
        has_subreaper: false,
        mm: prep.mm.clone(),
        files: Arc::new(Mutex::new(files)),
        fs: mount::FsRef::new(fs),
        nsproxy: ns::init_nsproxy(),
        no_new_privs: false,
        exe,
        umask: 0o022,
        creds: creds::Creds::root(),
        ctty: None,
        sighand: SigHand::new(),
        exit_signal: signal::SIGCHLD,
        comm: prep.comm,
        log_output: req.log_output,
        native_parent: true,
    });
    *process.args.lock() = prep.args;
    let thread = new_thread(pid, Some(pid_ref), process.clone(), prep.mm.clone(), 0, signal::AltStack::disabled());
    // Tous les registres à zéro : `rdx`/`edx` nul = pas de fonction de fin fournie par le noyau.
    let (regs, seg) = if prep.compat {
        (UserRegs::new_user32(prep.entry as u32, prep.sp as u32), uentry::SegState::compat())
    } else {
        (UserRegs::new_user(prep.entry as u64, prep.sp as u64), uentry::SegState::new())
    };
    let fpu = FpuState::new().ok_or_else(|| String::from("etat vectoriel : memoire insuffisante"))?;
    launch(nt, thread, regs, fpu, seg).map_err(String::from)?;
    Ok(ProcessHandle { process })
}

/// Paramètres d'un `clone`.
pub(crate) struct CloneSpec {
    pub flags: u64,
    pub exit_signal: i32,
    pub stack: u64,
    pub tls: u64,
    pub child_tid: u64,
    /// `CLONE_PARENT_SETTID` : écrit dans la mémoire du parent **avant** que le fil neuf ne
    /// tourne (comme `kernel_clone` avant `wake_up_new_task`). glibc y range le `tid` du TCB
    /// (`pd->tid`) : un fil qui démarrait avant l'écriture voyait 0, et
    /// `pthread_rwlock_wrlock` prenait un verrou libre pour le sien (`EDEADLK`).
    pub parent_tid: u64,
    /// `CLONE_SETTLS` d'un appel i386 : entrée TLS de la GDT (indice 0..3, descripteur) lue
    /// dans le `user_desc` (au lieu d'une base `fs`).
    pub tls32: Option<(usize, u64)>,
    /// `clone3` : numéros voulus, de l'espace pid de l'enfant vers ses ancêtres (`set_tid`).
    pub set_tid: Vec<u32>,
}

/// Résultat d'un `clone`.
pub(crate) struct Cloned {
    /// Numéro de l'enfant vu du créateur (valeur de retour de `clone`).
    pub nr: usize,
    /// `CLONE_VFORK` : complété quand l'enfant quitte l'espace partagé.
    pub vfork: Option<Arc<Completion>>,
    pub process: Arc<Process>,
}

pub(crate) const CLONE_VM: u64 = 0x100;
pub(crate) const CLONE_PARENT_SETTID: u64 = 0x10_0000;
pub(crate) const CLONE_FS: u64 = 0x200;
pub(crate) const CLONE_FILES: u64 = 0x400;

/// La table des descripteurs de `p` est partagée avec un autre processus (`CLONE_FILES` sans
/// `CLONE_THREAD`) : seules ces `clone` en gardent une référence durable.
pub(crate) fn shares_files(p: &Process) -> bool {
    Arc::strong_count(&p.files) > 1
}
pub(crate) const CLONE_SIGHAND: u64 = 0x800;
pub(crate) const CLONE_VFORK: u64 = 0x4000;
pub(crate) const CLONE_PARENT: u64 = 0x8000;
pub(crate) const CLONE_THREAD: u64 = 0x10000;
pub(crate) const CLONE_SETTLS: u64 = 0x80000;
pub(crate) const CLONE_CHILD_CLEARTID: u64 = 0x20_0000;
pub(crate) const CLONE_CHILD_SETTID: u64 = 0x100_0000;

/// `fork`/`vfork`/`clone` : crée le fil (et le processus) enfant, renvoie son identifiant.
/// Pour `CLONE_VFORK`, l'appelant attend ensuite [`Thread::vfork_done`] de l'enfant.
pub(crate) fn clone_thread(t: &Thread, regs: &UserRegs, spec: &CloneSpec) -> Result<Cloned, i64> {
    use linux::errno::*;
    let f = spec.flags;
    let p = &t.process;
    // L'init d'un espace pid ne crée pas de frère (`CLONE_PARENT`) : l'arbre resterait sans
    // racine (`copy_process`).
    if f & CLONE_PARENT != 0 && p.unkillable.load(Ordering::Acquire) {
        return Err(EINVAL);
    }
    let mm = if f & CLONE_VM != 0 { t.mm_arc() } else { t.mm().fork().map_err(linux::fault)? };
    // Registres de l'enfant : ceux de l'appel, `rax` = 0 (l'enfant « revient » de `clone`).
    let mut cregs = regs.clone();
    cregs.rax = 0;
    if spec.stack != 0 {
        cregs.rsp = spec.stack;
    }
    if f & CLONE_SETTLS != 0 && spec.tls32.is_none() {
        if spec.tls >= kernel_config::memory::USER_SPACE_END as u64 {
            return Err(EPERM);
        }
        cregs.fs_base = spec.tls;
    }
    // Segments : ceux du fil courant (sélecteurs vivants, entrées TLS), puis l'entrée TLS du
    // `user_desc` d'un `clone` i386 ; les bases des sélecteurs qui la désignent suivent.
    let seg = match user_context() {
        Some(ctx) => ctx.seg.duplicate_current(),
        None => uentry::SegState::new(),
    };
    if let (true, Some((idx, desc))) = (f & CLONE_SETTLS != 0, spec.tls32) {
        let (fs, gs) = uentry::set_tls(&seg, idx, desc, false);
        if let Some(b) = fs {
            cregs.fs_base = b;
        }
        if let Some(b) = gs {
            cregs.gs_base = b;
        }
    }
    // État vectoriel : celui des registres en ce moment (sauvé puis copié).
    let fpu = {
        let held = irq_safety::hold_interrupts();
        let task = task::get_my_current_task().ok_or(EINVAL)?;
        let ctx = task.user.get().ok_or(EINVAL)?;
        // SAFETY : interruptions coupées, état du fil courant chargé dans les registres.
        unsafe { ctx.fpu.save() };
        let copy = FpuState::clone_from_state(&ctx.fpu);
        drop(held);
        copy.ok_or(ENOMEM)?
    };
    let base = {
        let c = *p.comm.lock();
        let n = c.iter().position(|&b| b == 0).unwrap_or(16);
        String::from_utf8_lossy(&c[..n]).into_owned()
    };
    // Identités, espaces et `fs_struct` de l'enfant (`copy_creds`, `copy_namespaces`).
    let mut child_ctx = if f & CLONE_THREAD != 0 {
        // Un fil reste dans l'espace pid de son processus : refusé si `unshare(CLONE_NEWPID)`
        // a déjà désigné un autre espace pour les enfants.
        if f & ns::NS_FLAGS != 0 || !Arc::ptr_eq(&ns::of(p).pid_for_children, &p.pid_ns) {
            return Err(EINVAL);
        }
        None
    } else {
        let mut creds = p.creds.lock().clone();
        let mut fs = p.fs.get();
        let nsp = ns::for_clone(p, f, &mut creds, &mut fs)?;
        Some((creds, fs, nsp))
    };
    // Espace pid de l'enfant ; `set_tid` exige le droit de restaurer dans chaque espace visé
    // (`checkpoint_restore_ns_capable`).
    let child_pid_ns = match &child_ctx {
        Some((_, _, nsp)) => nsp.pid_for_children.clone(),
        None => p.pid_ns.clone(),
    };
    // L'init de cet espace est mort : plus personne n'y entre (`ENOMEM`, comme `alloc_pid`).
    if !child_pid_ns.accepting() {
        return Err(ENOMEM);
    }
    if !spec.set_tid.is_empty() {
        let creds = p.creds.lock().clone();
        let mut ns = Some(child_pid_ns.clone());
        for _ in 0..spec.set_tid.len() {
            let Some(n) = ns else { return Err(EINVAL) };
            if !userns::ns_capable(&creds, &n.user_ns, userns::CAP_SYS_ADMIN) && !userns::ns_capable(&creds, &n.user_ns, userns::CAP_CHECKPOINT_RESTORE) {
                return Err(EPERM);
            }
            ns = n.parent.clone();
        }
    }
    let nt = new_task(alloc::format!("{}_{}", if f & CLONE_THREAD != 0 { "fil" } else { "proc" }, base)).map_err(|_| ENOMEM)?;
    let tid = nt.task.id;
    let pid_ref = pidns::Pid::alloc(tid, &child_pid_ns, &spec.set_tid)?;
    if let Some(me) = t.task() {
        scheduler_inherit(&nt.task, &me);
    }
    // Pile alternative : héritée par `fork`, pas par un fil qui partage l'espace sans `vfork`.
    let alt = if f & CLONE_VM != 0 && f & CLONE_VFORK == 0 { signal::AltStack::disabled() } else { *t.altstack.lock() };
    let process = if f & CLONE_THREAD != 0 {
        p.live_threads.fetch_add(1, Ordering::AcqRel);
        p.clone()
    } else {
        let parent = if f & CLONE_PARENT != 0 { p.parent.lock().upgrade() } else { Some(p.clone()) };
        let ppid = if f & CLONE_PARENT != 0 { p.ppid.load(Ordering::Relaxed) } else { p.pid };
        let files = if f & CLONE_FILES != 0 { p.files.clone() } else { Arc::new(Mutex::new(p.files.lock().clone_table())) };
        let (child_creds, child_fs, child_ns) = child_ctx.take().ok_or(EINVAL)?;
        let fs = if f & CLONE_FS != 0 { p.fs.share() } else { mount::FsRef::new(child_fs) };
        let sighand = if f & CLONE_SIGHAND != 0 { p.sighand() } else { p.sighand().copy(false) };
        let child = new_process(ProcessSpec {
            pid: pid_ref.clone(),
            // Groupe et session du créateur (ils peuvent être hors de l'espace de l'enfant :
            // `getpgrp` y rend alors 0).
            pgrp: p.pgrp_ref().unwrap_or_else(|| pid_ref.clone()),
            session: p.session_ref().unwrap_or_else(|| pid_ref.clone()),
            parent,
            ppid,
            has_subreaper: p.has_child_subreaper.load(Ordering::Relaxed) || p.child_subreaper.load(Ordering::Relaxed),
            mm: mm.clone(),
            files,
            fs,
            nsproxy: child_ns,
            no_new_privs: p.no_new_privs.load(Ordering::Acquire),
            exe: p.exe.lock().clone(),
            umask: p.umask.load(Ordering::Relaxed),
            creds: child_creds,
            ctty: p.ctty.lock().clone(),
            sighand,
            exit_signal: spec.exit_signal,
            // L'enfant d'un `fork` prend le nom du fil qui l'a créé.
            comm: *t.comm.lock(),
            log_output: p.log_output,
            native_parent: false,
        });
        *child.args.lock() = *p.args.lock();
        child.oom_score_adj.store(p.oom_score_adj.load(Ordering::Relaxed), Ordering::Relaxed);
        child
    };
    let child_process = process.clone();
    let thread = new_thread(tid, Some(pid_ref), process, mm.clone(), t.sigmask.load(Ordering::Acquire), alt);
    *thread.comm.lock() = *t.comm.lock(); // un fil neuf hérite du nom de son créateur
    thread.ioprio.store(t.ioprio.load(Ordering::Relaxed), Ordering::Relaxed);
    thread.robust_compat.store(t.robust_compat.load(Ordering::Relaxed), Ordering::Relaxed);
    // Numéro vu du créateur (retour de `clone`, `CLONE_PARENT_SETTID`) et vu de l'enfant
    // (`CLONE_CHILD_SETTID`, comme `schedule_tail`) : différents si l'enfant ouvre un espace pid.
    let nr = p.vnr(tid);
    if f & CLONE_CHILD_SETTID != 0 && spec.child_tid != 0 {
        let _ = mm.write_user::<u32>(spec.child_tid as usize, &(thread.vtid() as u32));
    }
    if f & CLONE_CHILD_CLEARTID != 0 {
        thread.clear_child_tid.store(spec.child_tid as usize, Ordering::Release);
    }
    if f & CLONE_PARENT_SETTID != 0 && spec.parent_tid != 0 {
        let _ = t.mm().write_user::<u32>(spec.parent_tid as usize, &(nr as u32));
    }
    let done = if f & CLONE_VFORK != 0 {
        let c = Completion::new();
        *thread.vfork_done.lock() = Some(c.clone());
        Some(c)
    } else {
        None
    };
    drop(mm);
    launch(nt, thread, cregs, fpu, seg).map_err(|_| EAGAIN)?;
    Ok(Cloned { nr, vfork: done, process: child_process })
}

/// Fin d'`execve` (après le point de non-retour, le fil est seul) : nouvel espace chargé,
/// descripteurs `O_CLOEXEC` fermés, gestionnaires remis à l'action par défaut, état vectoriel
/// initial, registres de départ du nouveau programme.
pub(crate) fn finish_exec(t: &Thread, prep: exec::Prepared, regs: &mut UserRegs) {
    let p = &t.process;
    // Les mécanismes liés à l'ancienne mémoire s'achèvent avec elle (listes robustes, `ctid`).
    futex::exit_thread(t, t.mm());
    let new = prep.mm.clone();
    *p.mm.lock() = Some(new.clone());
    if let Some(task) = task::get_my_current_task() {
        if let Some(ctx) = task.user.get() {
            *ctx.space.lock() = new.space().clone();
        }
    }
    {
        let _held = irq_safety::hold_interrupts();
        memory::paging::address_space::switch_to(new.space());
    }
    // SAFETY : appelé par le fil lui-même ; aucune référence sur l'ancienne mémoire ne vit ici.
    let old = unsafe { t.replace_mm(Some(new)) };
    drop(old);
    let closed = p.files.lock().close_cloexec();
    for f in &closed {
        reclock::on_fd_close(p.pid, f);
    }
    drop(closed);
    let sh = p.sighand().copy(true);
    *p.sighand.lock() = sh;
    *t.altstack.lock() = signal::AltStack::disabled();
    *t.saved_sigmask.lock() = None;
    p.ptimers.lock().clear();
    t.restart.lock().take();
    let root = p.fs.root();
    *p.exe.lock() = exe_path(&root, &prep.path);
    *p.comm.lock() = prep.comm;
    *t.comm.lock() = prep.comm;
    *p.args.lock() = prep.args;
    if let Some(c) = t.vfork_done.lock().take() {
        c.complete();
    }
    signal::reset_fpu();
    // Segments du nouveau programme (plus d'entrée TLS ; `ds`/`es` de données en 32 bits).
    if let Some(ctx) = user_context() {
        ctx.seg.exec_reset(prep.compat);
    }
    t.robust_compat.store(false, Ordering::Relaxed);
    *regs = if prep.compat {
        UserRegs::new_user32(prep.entry as u32, prep.sp as u32)
    } else {
        UserRegs::new_user(prep.entry as u64, prep.sp as u64)
    };
}

/// Un fil secondaire qui exécute prend l'identifiant du chef de groupe.
pub(crate) fn retid(t: &Thread, new: usize) {
    // Le numéro du chef (tenu par le processus) devient celui du fil ; l'ancien est rendu
    // (`exchange_tids`).
    let leader = t.process.pid_ref();
    t.vtid.store(t.process.vpid, Ordering::Relaxed);
    let old_ref = core::mem::replace(&mut *t.pid_ref.lock(), leader);
    drop(old_ref);
    let old = t.tid.swap(new, Ordering::AcqRel);
    let mut th = THREADS.lock();
    if let Some(w) = th.remove(&old) {
        th.insert(new, w);
    }
}

fn scheduler_inherit(child: &task::TaskRef, parent: &task::TaskRef) {
    if let Some(prio) = task::scheduler::priority(parent) {
        task::scheduler::set_priority(child, prio);
    }
}

/// Corps de la tâche qui porte un fil.
fn thread_main(slot: Arc<Mutex<Option<Arc<Thread>>>>) -> isize {
    let Some(thread) = slot.lock().take() else {
        warn!("maiproc : fil absent au demarrage");
        return -1;
    };
    drop(slot);
    // SAFETY : `prepare_entry` respecte le contrat ; `jmp` vit dans `thread`, gardé ici.
    let raw = unsafe { uentry::run_user(prepare_entry, Arc::as_ptr(&thread) as usize, thread.jmp.get()) };
    let status = raw as u32 as i32;
    finish_thread(&thread, status)
}

/// Premier passage : contexte utilisateur de la tâche, espace d'adresses, état vectoriel, cadre.
extern "C" fn prepare_entry(arg: usize, kstack_top: usize) -> usize {
    // SAFETY : `arg` pointe sur le fil, gardé vivant par `thread_main`.
    let thread = unsafe { &*(arg as *const Thread) };
    let st = thread.start.lock().take().expect("maiproc : fil deja demarre");
    let space = thread.mm().space().clone();
    let task = task::get_my_current_task().expect("maiproc : pas de tache courante");
    let StartState { mut regs, fpu, seg, me } = st;
    if sysstats::enabled() && thread.tid() == thread.process.pid {
        sysstats::record_phase(sysstats::PH_START, thread.process.started.elapsed().as_nanos() as u64);
    }
    let ctx = task.user.call_once(|| task_struct::UserContext {
        space: spin::Mutex::new(space.clone()),
        kstack_top,
        fpu,
        seg,
        process: me,
    });
    // Sélecteurs et entrées TLS du fil : la commutation qui a amené cette tâche ne les
    // connaissait pas encore. Un `fs`/`gs` non nul prend la base de son descripteur.
    let (want, _) = ctx.seg.snapshot(false);
    let (loaded, fs_base, gs_base) = uentry::load_segments(&ctx.seg, want);
    if loaded.fs & !3 != 0 {
        regs.fs_base = fs_base;
    }
    if loaded.gs & !3 != 0 {
        regs.gs_base = gs_base;
    }
    uentry::set_kernel_stack(kstack_top);
    memory::paging::address_space::switch_to(&space);
    // SAFETY : interruptions coupées, sur le cœur qui exécute le fil.
    unsafe { uentry::begin_user_fpu(&ctx.fpu) };
    let frame = uentry::frame_at(kstack_top);
    // SAFETY : la zone `[frame, kstack_top + 16)` est réservée par `run_user`.
    unsafe {
        frame.write(regs);
        *(kstack_top as *mut usize) = arg;
    }
    drop(task);
    frame as usize
}

/// Fin du groupe de fils de `p` avec `status` (le premier statut fixé l'emporte) : tous les fils
/// sauf `except` sont prévenus.
pub(crate) fn group_exit(p: &Process, status: i32, except: Option<usize>) {
    p.exit_status.compare_exchange(-1, status, Ordering::AcqRel, Ordering::Acquire).ok();
    if p.group_exit.swap(true, Ordering::AcqRel) {
        return;
    }
    p.exit_req_ns.store(sysstats::now_ns(), Ordering::Relaxed);
    // Un processus arrêté doit reprendre pour mourir.
    p.stop.lock().stopped = false;
    p.stop_wait.wake_all();
    p.for_each_thread_until(|t| {
        if Some(t.tid()) != except {
            t.kick(WORK_EXIT);
        }
        false
    });
}

/// Fin d'un fil (de retour dans le noyau, interruptions actives).
fn finish_thread(thread: &Arc<Thread>, status: i32) -> isize {
    let p = &thread.process;
    if sysstats::enabled() {
        let req = p.exit_req_ns.load(Ordering::Relaxed);
        if req != 0 && p.live_threads.load(Ordering::Relaxed) == 1 {
            let now = sysstats::now_ns();
            sysstats::record_phase(sysstats::PH_EXIT_RET, now.saturating_sub(req));
            p.exit_tear_ns.store(now, Ordering::Relaxed);
        }
    }
    // SAFETY : le fil ne s'exécute plus ; sa mémoire est encore là pour les derniers accès.
    let mm = unsafe { thread.replace_mm(None) };
    if let Some(mm) = &mm {
        futex::exit_thread(thread, mm);
    }
    if let Some(c) = thread.vfork_done.lock().take() {
        c.complete();
    }
    p.dead_threads_ns.fetch_add(thread.cpu_ns(), Ordering::Relaxed);
    THREADS.lock().remove(&thread.tid());
    // Le numéro du fil est rendu (celui du chef reste tenu par le processus jusqu'au recueil).
    drop(thread.pid_ref.lock().take());
    p.threads.lock().retain(|w| w.upgrade().map_or(false, |x| !Arc::ptr_eq(&x, thread)));
    let last = p.live_threads.fetch_sub(1, Ordering::AcqRel) == 1;
    if last {
        if !p.group_exit.load(Ordering::Acquire) {
            p.exit_status.compare_exchange(-1, status, Ordering::AcqRel, Ordering::Acquire).ok();
        }
        drop(mm);
        process_exit(p);
    } else {
        drop(mm);
    }
    p.exit_wait.wake_all();
    p.exit_status.load(Ordering::Acquire) as isize
}

/// Dernier fil terminé : ressources rendues, enfants confiés, parent prévenu, zombie.
fn process_exit(p: &Arc<Process>) {
    if Arc::strong_count(&p.files) == 1 {
        p.files.lock().close_all();
    }
    reclock::on_exit(p.pid);
    // Sortie d'un chef de session : son terminal de contrôle est rendu (SIGHUP au premier plan).
    if p.sid.load(Ordering::Relaxed) == p.pid {
        pty::disassociate(p);
    }
    let mm = p.mm.lock().take();
    let rss = mm.as_ref().map_or(0, |m| m.rss());
    drop(mm);
    p.real_timer.lock().gen += 1;
    // L'init d'un espace pid emporte tout l'espace avec lui.
    if p.unkillable.load(Ordering::Acquire) {
        zap_pid_ns(p);
    }
    let orphans = core::mem::take(&mut *p.children.lock());
    if !orphans.is_empty() {
        forget_original_parent(p, orphans);
    }
    let status = p.exit_status.load(Ordering::Acquire);
    let log_start = sysstats::now_ns();
    if sysstats::enabled() {
        let tear = p.exit_tear_ns.load(Ordering::Relaxed);
        if tear != 0 {
            sysstats::record_phase(sysstats::PH_EXIT_TEAR, log_start.saturating_sub(tear));
        }
    }
    if p.log_output {
        let rest = core::mem::take(&mut *p.log_line.lock());
        p.emit_log_line(&rest);
        warn!(
            "maiproc : pid {} ({}) termine : {} (RSS {} Kio)",
            p.pid, p.exe.lock().as_str(), describe_status(status as isize), rss * 4
        );
    }
    if sysstats::enabled() {
        let now = sysstats::now_ns();
        sysstats::record_phase(sysstats::PH_EXIT_LOG, now.saturating_sub(log_start));
        let req = p.exit_req_ns.load(Ordering::Relaxed);
        if req != 0 {
            sysstats::record_phase(sysstats::PH_EXIT, now.saturating_sub(req));
        }
        sysstats::record_phase(sysstats::PH_LIFE, p.started.elapsed().as_nanos() as u64);
        sysstats::record_phase(sysstats::PH_CPU, p.cpu_ns());
        p.zombie_ns.store(now, Ordering::Relaxed);
    }
    p.state.store(STATE_ZOMBIE, Ordering::Release);
    let parent = p.parent.lock().upgrade();
    match parent {
        Some(parent) => {
            if do_notify_parent(p, &parent) {
                reap(p);
            }
            parent.child_wait.wake_all();
        }
        None if !p.native_parent => reap(p),
        None => {}
    }
    p.exit_wait.wake_all();
}

/// Prévient `parent` de la fin de `p` (`do_notify_parent`) ; vrai si `p` doit être recueilli
/// tout de suite (`SIGCHLD` ignoré ou `SA_NOCLDWAIT`, pour un enfant à `SIGCHLD` seulement).
fn do_notify_parent(p: &Process, parent: &Arc<Process>) -> bool {
    let status = p.exit_status.load(Ordering::Acquire);
    let act = parent.sigaction(signal::SIGCHLD);
    let mut sig = p.exit_signal.load(Ordering::Relaxed);
    let mut auto_reap = false;
    if sig == signal::SIGCHLD && (act.handler == signal::SIG_IGN || act.flags & signal::SA_NOCLDWAIT != 0) {
        auto_reap = true;
        if act.handler == signal::SIG_IGN {
            sig = 0;
        }
    }
    if (1..=64).contains(&sig) {
        let (code, st) = if status & 0x7f == 0 {
            (signal::CLD_EXITED, (status >> 8) & 0xff)
        } else {
            (signal::CLD_KILLED, status & 0x7f)
        };
        let mut info = SigInfo::child_of(code, p, parent, st, p.cpu_ns() / 10_000_000);
        info.set_i32(0, sig);
        signal::send_to_process(parent, info);
    }
    auto_reap
}

/// Nouveau parent des orphelins de `father` (`find_new_reaper`) : le plus proche ancêtre
/// *subreaper* vivant dans le même espace pid, sinon l'init de l'espace. `None` dans l'espace
/// initial sans *subreaper* : le noyau les recueille lui-même (MaiOS n'a pas de processus 1).
fn find_new_reaper(father: &Arc<Process>) -> Option<Arc<Process>> {
    let alive = |r: &Arc<Process>| !r.is_zombie() && !r.group_exit.load(Ordering::Acquire);
    if father.has_child_subreaper.load(Ordering::Relaxed) {
        let mut cur = father.parent.lock().upgrade();
        while let Some(r) = cur {
            // Jamais hors de l'espace pid du parent qui meurt (un parent injecté par `setns`).
            if !Arc::ptr_eq(&r.pid_ns, &father.pid_ns) {
                break;
            }
            if r.child_subreaper.load(Ordering::Relaxed) && alive(&r) {
                return Some(r);
            }
            cur = r.parent.lock().upgrade();
        }
    }
    father.pid_ns.reaper().filter(|r| !Arc::ptr_eq(r, father))
}

/// Confie les enfants de `father` à leur nouveau parent (`forget_original_parent`) et envoie
/// les `PR_SET_PDEATHSIG` demandés. Linux l'envoie à la fin du **fil** créateur ; MaiProc à la
/// fin du processus parent, ce que les programmes attendent (bwrap, le `reaper` de Steam).
fn forget_original_parent(father: &Arc<Process>, orphans: Vec<Arc<Process>>) {
    let reaper = find_new_reaper(father);
    for c in &orphans {
        for th in c.threads() {
            let sig = th.pdeath_signal.load(Ordering::Relaxed);
            if (1..=64).contains(&sig) {
                // Envoyé par le parent (`SEND_SIG_NOINFO`) : forcé s'il est hors de l'espace
                // de l'enfant — c'est ainsi que l'init d'un bac à sable meurt avec son moniteur.
                let force = !c.pid_ns.sees(father.pid);
                signal::send_to_process_as(c, SigInfo::from_process(sig, signal::SI_USER, father, c), force);
            }
        }
        match &reaper {
            Some(r) => {
                *c.parent.lock() = Arc::downgrade(r);
                c.ppid.store(r.pid, Ordering::Relaxed);
                c.exit_signal.store(signal::SIGCHLD, Ordering::Relaxed);
                r.children.lock().push(c.clone());
                if c.is_zombie() && do_notify_parent(c, r) {
                    reap(c);
                }
            }
            None => {
                // Plus de parent : les zombies sont recueillis tout de suite, les autres le
                // seront à leur fin.
                *c.parent.lock() = Weak::new();
                c.ppid.store(1, Ordering::Relaxed);
                if c.is_zombie() {
                    reap(c);
                }
            }
        }
    }
    if let Some(r) = reaper {
        r.child_wait.wake_all();
    }
}

/// Fin de l'init d'un espace pid (`zap_pid_ns_processes`) : l'espace ne prend plus personne,
/// chaque processus qui y a un numéro (descendants compris) reçoit `SIGKILL`, puis l'init
/// attend leur fin — en recueillant ses propres enfants, les autres l'étant par leur parent.
fn zap_pid_ns(p: &Arc<Process>) {
    let ns = p.pid_ns.clone();
    ns.disable();
    // Ses enfants se recueillent seuls (Linux met `SIGCHLD` à `SIG_IGN`).
    p.sighand().actions.lock()[signal::SIGCHLD as usize - 1].handler = signal::SIG_IGN;
    for (_, g) in ns.numbers() {
        if g == p.pid {
            continue;
        }
        if let Some(q) = find_process(g) {
            signal::send_to_process(&q, SigInfo::new(signal::SIGKILL, signal::SI_KERNEL));
        }
    }
    let others_left = || ns.numbers().iter().any(|&(_, g)| g != p.pid && (find_process(g).is_some() || find_thread(g).is_some()));
    loop {
        for c in p.children() {
            if c.is_zombie() {
                reap(&c);
            }
        }
        if !others_left() {
            break;
        }
        let _ = wait::wait_event(&[&ns.wait, &p.child_wait], None, None, || {
            (p.children().iter().any(|c| c.is_zombie()) || !others_left()).then_some(())
        });
    }
}

/// Prévient le parent d'un arrêt ou d'une reprise (`SIGCHLD` sauf `SA_NOCLDSTOP`).
pub(crate) fn notify_parent(p: &Process, code: i32, sig: i32) {
    let Some(parent) = p.parent.lock().upgrade() else { return };
    let act = parent.sigaction(signal::SIGCHLD);
    if act.flags & signal::SA_NOCLDSTOP == 0 && act.handler != signal::SIG_IGN {
        signal::send_to_process(&parent, SigInfo::child_of(code, p, &parent, sig, 0));
    }
    parent.child_wait.wake_all();
}

/// Recueille un zombie : il quitte la table des processus et la liste de son parent, et rend
/// ses numéros (pid, groupe, session : ceux qui n'ont plus d'attache sont libérés).
pub(crate) fn reap(p: &Arc<Process>) {
    if p.state.swap(STATE_REAPED, Ordering::AcqRel) == STATE_REAPED {
        return;
    }
    PROCESSES.lock().remove(&p.pid);
    if let Some(parent) = p.parent.lock().upgrade() {
        parent.children_ns.fetch_add(p.cpu_ns() + p.children_ns.load(Ordering::Relaxed), Ordering::Relaxed);
        parent.children.lock().retain(|c| !Arc::ptr_eq(c, p));
    }
    let pids = core::mem::take(&mut *p.pids.lock());
    drop(pids);
    // L'init d'un espace qui meurt attend ce recueil (même si un numéro reste tenu).
    let mut ns = Some(&p.pid_ns);
    while let Some(n) = ns {
        if n.level == 0 {
            break;
        }
        n.wait.wake_all();
        ns = n.parent.as_ref();
    }
}

/// Lit entièrement un fichier (exécutable, interpréteur).
/// Chemin de `/proc/<pid>/exe` : le fichier exécuté, liens symboliques résolus comme Linux
/// (`/usr/bin/firefox-esr` → `/usr/lib/firefox-esr/firefox-esr` : Firefox y cherche `libxul.so`).
pub(crate) fn exe_path(root: &mount::Loc, path: &str) -> String {
    mount::resolve(root, path, true).map(|f| f.path).unwrap_or_else(|_| String::from(path))
}

pub fn read_whole_file(path: &str, cwd: &DirRef) -> Option<Arc<[u8]>> {
    let p: &path::Path = path.as_ref();
    let file = p.get_file(cwd)?;
    let mut locked = file.lock();
    let len = locked.len();
    let mut buf = alloc::vec![0u8; len];
    let n = locked.read_at(&mut buf, 0).ok()?;
    buf.truncate(n);
    Some(Arc::from(buf.into_boxed_slice()))
}

/// Décrit un statut de sortie (`wait4`) pour l'affichage.
pub fn describe_status(status: isize) -> String {
    let s = status as i32;
    if s < 0 {
        alloc::format!("statut inconnu ({})", s)
    } else if s & 0x7f == 0 {
        alloc::format!("code {}", (s >> 8) & 0xff)
    } else {
        alloc::format!("tue par le signal {}", s & 0x7f)
    }
}
