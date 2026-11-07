//! MaiProc — processus en mode utilisateur (anneau 3).
//!
//! Les applications natives de MaiOS restent dans l'espace d'adresses unique du noyau (modèle
//! Theseus). Les binaires étrangers (Linux, plus tard Windows) s'exécutent ici, en anneau 3, chacun
//! dans son propre espace d'adresses ([`uvm::Mm`]) :
//!
//! - un **fil** ([`Thread`]) est porté par une tâche noyau ordinaire (ordonnancée par MaiSched) ;
//!   elle passe en mode utilisateur par [`uentry::run_user`] et en revient par
//!   [`uentry::exit_user`] quand le fil se termine, puis finit normalement (nettoyage, `join`) ;
//! - le fil courant se retrouve sans recherche : son adresse est posée juste au-dessus du cadre
//!   d'entrée, au sommet de sa pile noyau ([`uentry::thread_slot`]) ;
//! - aucune donnée utilisateur n'est jamais lue par pointeur direct : tout passe par les copies de
//!   [`uvm`] (validation par zone puis accès par la physmap), ce qui permet SMAP sans `stac`.
//!
//! Personnalité : [`linux`] (ABI x86_64, appels système par numéro Linux).

#![no_std]
#![cfg(target_arch = "x86_64")]

extern crate alloc;

mod elf;
pub mod fd;
mod linux;
mod stack;

use alloc::{string::String, sync::Arc, vec::Vec};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use fs_node::DirRef;
use log::warn;
use spin::Mutex;
use uentry::{FpuState, UserRegs};
use uvm::Mm;

pub use fd::FdTable;

/// Statut d'attente (format de `wait4`) : sortie normale.
pub const fn exit_status(code: i32) -> i32 { (code & 0xff) << 8 }
/// Statut d'attente : tué par le signal `sig`.
pub const fn signal_status(sig: i32) -> i32 { sig & 0x7f }

/// Disposition d'un signal (enregistrée ; la livraison arrive avec M2).
#[derive(Clone, Copy, Default, Debug)]
pub struct SigAction {
    pub handler: u64,
    pub flags: u64,
    pub restorer: u64,
    pub mask: u64,
}

/// Processus : un espace d'adresses, une table de fichiers, des fils.
pub struct Process {
    pub pid: usize,
    pub ppid: usize,
    pub mm: Arc<Mm>,
    pub files: Mutex<FdTable>,
    /// Répertoire courant et son chemin absolu.
    pub cwd: Mutex<(DirRef, String)>,
    /// Chemin de l'exécutable (`/proc/self/exe`).
    pub exe: String,
    pub umask: AtomicU32,
    pub sigactions: Mutex<[SigAction; 64]>,
    /// Fils vivants.
    pub live_threads: AtomicUsize,
    /// Statut de sortie du processus (format `wait4`), posé par le premier `exit_group`.
    pub exit_status: AtomicI32,
    /// Instant de création (horloges CPU approchées).
    pub started: time::Instant,
    /// Nom court (`comm`, `PR_SET_NAME`).
    pub comm: Mutex<[u8; 16]>,
    /// Copie les écritures sur le terminal dans le journal du noyau (tests automatiques).
    pub log_output: bool,
    /// Ligne en cours de la copie dans le journal.
    log_line: Mutex<Vec<u8>>,
}

impl Process {
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
}

/// État du premier passage d'un fil en mode utilisateur (consommé par `prepare_entry`).
struct StartState {
    regs: UserRegs,
    fpu: FpuState,
    me: Arc<Thread>,
}

/// Fil d'exécution utilisateur.
pub struct Thread {
    pub tid: usize,
    pub process: Arc<Process>,
    /// Point de retour posé par `run_user` (écrit et lu par la seule tâche du fil).
    jmp: UnsafeCell<usize>,
    pub clear_child_tid: AtomicUsize,
    pub robust_list: AtomicUsize,
    pub sigmask: AtomicU64,
    start: Mutex<Option<StartState>>,
}

// SAFETY : `jmp` n'est touché que par la tâche qui porte le fil.
unsafe impl Sync for Thread {}
unsafe impl Send for Thread {}

/// Fil courant, depuis le cadre d'une entrée en mode utilisateur.
#[inline(always)]
pub(crate) fn current(regs: &UserRegs) -> &Thread {
    // SAFETY : `prepare_entry` a posé l'adresse du fil au-dessus du cadre ; la tâche garde le fil
    // vivant tant qu'il peut entrer dans le noyau.
    unsafe { &*(*uentry::thread_slot(regs) as *const Thread) }
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
    });
}

fn syscall_hook(regs: &mut UserRegs) {
    let thread = current(regs) as *const Thread;
    // SAFETY : voir `current`.
    let outcome = linux::dispatch(unsafe { &*thread }, regs);
    match outcome {
        linux::Outcome::Return(v) => regs.rax = v as u64,
        linux::Outcome::Exit(status) => exit_current(thread, status),
    }
}

/// Signal Linux correspondant à une exception x86.
fn exception_signal(vector: u8) -> (i32, &'static str) {
    match vector {
        0 => (8, "division par zero"),
        1 | 3 => (5, "point d'arret"),
        6 => (4, "instruction illegale"),
        7 | 16 | 19 => (8, "exception flottante"),
        17 => (7, "acces non aligne"),
        13 => (11, "protection generale"),
        14 => (11, "erreur de segmentation"),
        _ => (11, "exception"),
    }
}

fn fault_hook(regs: &mut UserRegs, vector: u8, cr2: u64) {
    let thread = current(regs) as *const Thread;
    // SAFETY : voir `current`.
    let t = unsafe { &*thread };
    if vector == 14 {
        let err = regs.error_code;
        let write = err & 2 != 0;
        let exec = err & 16 != 0;
        if t.process.mm.handle_fault(cr2 as usize, write, exec).is_ok() {
            return;
        }
    }
    let (sig, what) = exception_signal(vector);
    {
        let msg = alloc::format!(
            "[maiproc] pid {} : {} (vecteur {}, adresse {:#x}, rip {:#x}, code {:#x}) -> signal {}\n",
            t.process.pid, what, vector, cr2, regs.rip, regs.error_code, sig
        );
        warn!("{}", msg.trim_end());
        if let Some(f) = t.process.files.lock().get(2) {
            f.write_console(msg.as_bytes());
        }
    }
    t.process.exit_status.compare_exchange(-1, signal_status(sig), Ordering::AcqRel, Ordering::Acquire).ok();
    exit_current(thread, signal_status(sig))
}

// --- Création ----------------------------------------------------------------------------------

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

/// Crée un processus et démarre son fil principal. Renvoie la tâche qui le porte : son
/// `join()` donne le statut de sortie au format `wait4`.
pub fn spawn(req: SpawnRequest) -> Result<task::JoinableTaskRef, String> {
    if !available() {
        return Err("mode utilisateur indisponible (FSGSBASE requis)".into());
    }
    install_hooks();

    let image = elf::Image::parse(&req.image).map_err(String::from)?;
    let layout = stack::Layout::randomized();
    let mm = Mm::new(layout.mmap_top).map_err(String::from)?;

    // Interpréteur (`PT_INTERP`) : chargé lui aussi, il reçoit la main.
    let interp = match image.interp(&req.image) {
        Some(ipath) => {
            let bytes = read_whole_file(&ipath, &req.cwd)
                .ok_or_else(|| alloc::format!("interpreteur introuvable : {}", ipath))?;
            Some(bytes)
        }
        None => None,
    };

    let main = elf::load(&mm, &image, req.image.clone(), elf::Role::Main).map_err(String::from)?;
    let interp_loaded = match &interp {
        Some(bytes) => {
            let ii = elf::Image::parse(bytes).map_err(String::from)?;
            Some(elf::load(&mm, &ii, bytes.clone(), elf::Role::Interpreter).map_err(String::from)?)
        }
        None => None,
    };
    mm.set_brk_base(main.brk_base + uvm::rand::random_pages(13));

    let entry = interp_loaded.as_ref().map_or(main.entry, |i| i.entry);
    let sp = stack::build(&mm, &layout, &req.path, &req.argv, &req.envp, &main, interp_loaded.as_ref())
        .map_err(|e| alloc::format!("pile initiale : {:?}", e))?;

    let files = FdTable::with_stdio(req.stdio.as_ref());
    let mut comm = [0u8; 16];
    let base = req.path.rsplit('/').next().unwrap_or("");
    for (d, s) in comm.iter_mut().zip(base.bytes().take(15)) { *d = s; }

    // Le PID est l'identifiant de la tâche du fil principal : connu après sa création (tâche
    // bloquée), d'où le fil passé par une case remplie avant le démarrage.
    let slot: Arc<Mutex<Option<Arc<Thread>>>> = Arc::new(Mutex::new(None));
    let name = alloc::format!("proc_{}", base);

    let mut regs = UserRegs::new_user(entry as u64, sp as u64);
    regs.rdx = 0; // pas de fonction de fin fournie par le noyau (ABI x86_64)

    let fpu = FpuState::new().ok_or_else(|| String::from("etat vectoriel : memoire insuffisante"))?;

    let child = spawn::new_task_builder(thread_main, slot.clone())
        .name(name)
        .block()
        .spawn()
        .map_err(String::from)?;

    let process = Arc::new(Process {
        pid: child.id,
        ppid: req.ppid,
        mm,
        files: Mutex::new(files),
        cwd: Mutex::new((req.cwd.clone(), req.cwd_path.clone())),
        exe: req.path.clone(),
        umask: AtomicU32::new(0o022),
        sigactions: Mutex::new([SigAction::default(); 64]),
        live_threads: AtomicUsize::new(1),
        exit_status: AtomicI32::new(-1),
        started: time::Instant::now(),
        comm: Mutex::new(comm),
        log_output: req.log_output,
        log_line: Mutex::new(Vec::new()),
    });
    let thread = Arc::new(Thread {
        tid: child.id,
        process: process.clone(),
        jmp: UnsafeCell::new(0),
        clear_child_tid: AtomicUsize::new(0),
        robust_list: AtomicUsize::new(0),
        sigmask: AtomicU64::new(0),
        start: Mutex::new(None),
    });
    *thread.start.lock() = Some(StartState { regs, fpu, me: thread.clone() });
    drop(process);
    *slot.lock() = Some(thread);

    child.unblock().map_err(|_| String::from("demarrage du fil impossible"))?;
    Ok(child)
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
    let space = thread.process.mm.space().clone();
    let task = task::get_my_current_task().expect("maiproc : pas de tache courante");
    let StartState { regs, fpu, me } = st;
    let ctx = task.user.call_once(|| task_struct::UserContext {
        space: Mutex::new(space.clone()),
        kstack_top,
        fpu,
        process: me,
    });
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

/// Fin d'un fil (de retour dans le noyau, interruptions actives).
fn finish_thread(thread: &Arc<Thread>, status: i32) -> isize {
    let p = &thread.process;
    // `clear_child_tid` : remis à zéro (le réveil futex arrive avec les fils multiples, M2).
    let ctid = thread.clear_child_tid.swap(0, Ordering::AcqRel);
    if ctid != 0 {
        let _ = p.mm.write_user::<u32>(ctid, &0);
    }
    let last = p.live_threads.fetch_sub(1, Ordering::AcqRel) == 1;
    p.exit_status.compare_exchange(-1, status, Ordering::AcqRel, Ordering::Acquire).ok();
    if last {
        // Ferme les fichiers tout de suite (tubes, consoles) ; la mémoire part avec le processus.
        p.files.lock().close_all();
        let rest = core::mem::take(&mut *p.log_line.lock());
        p.emit_log_line(&rest);
        warn!(
            "maiproc : pid {} ({}) termine : {} (RSS {} Kio)",
            p.pid, p.exe, describe_status(p.exit_status.load(Ordering::Acquire) as isize), p.mm.rss() * 4
        );
    }
    p.exit_status.load(Ordering::Acquire) as isize
}

/// Lit entièrement un fichier (exécutable, interpréteur).
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
