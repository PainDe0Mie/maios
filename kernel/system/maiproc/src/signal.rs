//! Signaux : files, envoi, dispositions par défaut, arrêt/reprise, livraison (cadre `rt_sigframe`
//! de l'ABI x86_64 de Linux) et `rt_sigreturn`.
//!
//! ## Chemin de retour
//! Chaque retour en mode utilisateur passe par [`return_hook`] (crochet de `uentry`, interruptions
//! coupées). Le cas courant coûte une lecture atomique : le mot `work` du fil n'est non nul que
//! si un signal *peut* être livrable, si le groupe se termine ou si un appel système doit être
//! relancé. Le travail se fait interruptions actives (copies vers la pile utilisateur).
//!
//! ## Livraison
//! Un signal dirigé vers un processus attend dans sa file partagée et un fil qui ne le bloque pas
//! est prévenu (interruption de son cœur par IPI s'il tourne en mode utilisateur, déblocage s'il
//! dort dans une attente interruptible). Un signal synchrone (faute) ne peut être ni bloqué ni
//! ignoré : il revient à l'action par défaut si c'est le cas, comme `force_sig_info` de Linux.
//!
//! ## Cadre
//! `[pretcode][ucontext][siginfo]` sous la zone rouge (ou sur la pile alternative), état vectoriel
//! complet au format XSAVE en dessous (aligné sur 64, en-tête logiciel `fx_sw_bytes` et marqueur
//! final, comme `copy_fpstate_to_sigframe`) ; la taille vient d'`AT_MINSIGSTKSZ` (auxv). Le
//! gestionnaire démarre avec un état vectoriel initial.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use core::sync::atomic::Ordering;
use uentry::{UserRegs, FORCE_IRET_VECTOR};

use crate::{current, exit_current, signal_status, Process, Thread, WORK_EXIT, WORK_SIGNAL};

pub const SIGHUP: i32 = 1;
pub const SIGINT: i32 = 2;
pub const SIGQUIT: i32 = 3;
pub const SIGILL: i32 = 4;
pub const SIGTRAP: i32 = 5;
pub const SIGABRT: i32 = 6;
pub const SIGBUS: i32 = 7;
pub const SIGFPE: i32 = 8;
pub const SIGKILL: i32 = 9;
pub const SIGUSR1: i32 = 10;
pub const SIGSEGV: i32 = 11;
pub const SIGUSR2: i32 = 12;
pub const SIGPIPE: i32 = 13;
pub const SIGALRM: i32 = 14;
pub const SIGTERM: i32 = 15;
pub const SIGCHLD: i32 = 17;
pub const SIGCONT: i32 = 18;
pub const SIGSTOP: i32 = 19;
pub const SIGTSTP: i32 = 20;
pub const SIGTTIN: i32 = 21;
pub const SIGTTOU: i32 = 22;
pub const SIGSYS: i32 = 31;

pub const SIG_DFL: u64 = 0;
pub const SIG_IGN: u64 = 1;

pub const SA_NOCLDSTOP: u64 = 0x1;
pub const SA_NOCLDWAIT: u64 = 0x2;
pub const SA_SIGINFO: u64 = 0x4;
pub const SA_RESTORER: u64 = 0x0400_0000;
pub const SA_ONSTACK: u64 = 0x0800_0000;
pub const SA_RESTART: u64 = 0x1000_0000;
pub const SA_NODEFER: u64 = 0x4000_0000;
pub const SA_RESETHAND: u64 = 0x8000_0000;
/// Interne (jamais vu de l'utilisateur) : gestionnaire installé par un appel i386, qui reçoit
/// un cadre i386 (comme `SA_IA32_ABI` de Linux).
pub const SA_IA32_ABI: u64 = 0x0200_0000;

/// `si_code`.
pub const SI_USER: i32 = 0;
pub const SI_KERNEL: i32 = 0x80;
pub const SI_TKILL: i32 = -6;
pub const SEGV_MAPERR: i32 = 1;
pub const SEGV_ACCERR: i32 = 2;
pub const CLD_EXITED: i32 = 1;
pub const CLD_KILLED: i32 = 2;
pub const CLD_STOPPED: i32 = 5;
pub const CLD_CONTINUED: i32 = 6;

/// Codes internes de relance (jamais vus par l'utilisateur).
pub const ERESTARTSYS: i64 = 512;
pub const ERESTARTNOINTR: i64 = 513;
pub const ERESTARTNOHAND: i64 = 514;
pub const ERESTART_RESTARTBLOCK: i64 = 516;
const EINTR: i64 = 4;
const NR_RESTART_SYSCALL: u64 = 219;
const NR_RESTART_SYSCALL32: u64 = 0;

#[inline]
pub const fn sigbit(sig: i32) -> u64 { 1u64 << (sig - 1) }
const UNBLOCKABLE: u64 = sigbit(SIGKILL) | sigbit(SIGSTOP);
const STOP_SIGNALS: u64 = sigbit(SIGSTOP) | sigbit(SIGTSTP) | sigbit(SIGTTIN) | sigbit(SIGTTOU);
/// Signaux synchrones (fautes) : livrés en premier.
const SYNCHRONOUS: u64 = sigbit(SIGSEGV) | sigbit(SIGBUS) | sigbit(SIGILL) | sigbit(SIGTRAP) | sigbit(SIGFPE) | sigbit(SIGSYS);

/// Retire de `mask` ce qu'aucun masque ne peut bloquer.
pub const fn sanitize_mask(mask: u64) -> u64 { mask & !UNBLOCKABLE }

/// Disposition d'un signal (`struct sigaction` du noyau, ordre de l'ABI x86_64).
#[derive(Clone, Copy, Default, Debug)]
pub struct SigAction {
    pub handler: u64,
    pub flags: u64,
    pub restorer: u64,
    pub mask: u64,
}

/// Table des dispositions (partagée par les fils d'un processus).
pub struct SigHand {
    pub actions: sync_irq::IrqSafeMutex<[SigAction; 64]>,
}

impl SigHand {
    pub fn new() -> Arc<SigHand> {
        Arc::new(SigHand { actions: sync_irq::IrqSafeMutex::new([SigAction::default(); 64]) })
    }

    /// Copie (`fork`) ; `exec` : les gestionnaires redeviennent l'action par défaut, les signaux
    /// ignorés le restent.
    pub fn copy(&self, for_exec: bool) -> Arc<SigHand> {
        let mut a = *self.actions.lock();
        if for_exec {
            for s in a.iter_mut() {
                if s.handler != SIG_IGN {
                    *s = SigAction::default();
                } else {
                    s.flags = 0;
                    s.mask = 0;
                }
            }
        }
        Arc::new(SigHand { actions: sync_irq::IrqSafeMutex::new(a) })
    }
}

/// `siginfo_t` (128 octets).
#[derive(Clone, Copy)]
pub struct SigInfo {
    pub bytes: [u8; 128],
}

impl SigInfo {
    pub fn new(signo: i32, code: i32) -> SigInfo {
        let mut s = SigInfo { bytes: [0; 128] };
        s.set_i32(0, signo);
        s.set_i32(8, code);
        s
    }

    /// Envoi par le processus `from` à `to` (`kill`, `tkill`, `SIGPIPE`…) : `si_pid` vu de
    /// l'espace pid de `to` (0 s'il ne voit pas l'envoyeur), `si_uid` = uid réel de l'envoyeur
    /// vu de l'espace utilisateur de `to` (`prepare_kill_siginfo` + `send_signal_locked`).
    pub fn from_process(signo: i32, code: i32, from: &Process, to: &Process) -> SigInfo {
        let mut s = SigInfo::new(signo, code);
        s.set_i32(16, to.vnr(from.pid) as i32);
        let uid = from.creds.lock().ruid;
        let ns = to.creds.lock().user_ns.clone();
        s.set_i32(20, ns.from_kuid_munged(uid) as i32);
        s
    }

    /// `SIGCHLD` de `child` pour `parent` : pid et uid vus de celui-ci (`do_notify_parent`).
    pub fn child_of(code: i32, child: &Process, parent: &Process, status: i32, utime_ticks: u64) -> SigInfo {
        let mut s = SigInfo::child(code, parent.vnr(child.pid), status, utime_ticks);
        let uid = child.creds.lock().ruid;
        let ns = parent.creds.lock().user_ns.clone();
        s.set_i32(20, ns.from_kuid_munged(uid) as i32);
        s
    }

    /// Faute à l'adresse `addr`.
    pub fn fault(signo: i32, code: i32, addr: u64) -> SigInfo {
        let mut s = SigInfo::new(signo, code);
        s.set_u64(16, addr);
        s
    }

    /// `SIGCHLD`.
    pub fn child(code: i32, pid: usize, status: i32, utime_ticks: u64) -> SigInfo {
        let mut s = SigInfo::new(SIGCHLD, code);
        s.set_i32(16, pid as i32);
        s.set_i32(20, 0);
        s.set_i32(24, status);
        s.set_u64(32, utime_ticks);
        s
    }

    pub fn signo(&self) -> i32 { i32::from_le_bytes(self.bytes[0..4].try_into().unwrap()) }

    pub fn set_i32(&mut self, off: usize, v: i32) { self.bytes[off..off + 4].copy_from_slice(&v.to_le_bytes()); }

    pub fn set_u64(&mut self, off: usize, v: u64) { self.bytes[off..off + 8].copy_from_slice(&v.to_le_bytes()); }

    pub fn get_i32(&self, off: usize) -> i32 { i32::from_le_bytes(self.bytes[off..off + 4].try_into().unwrap()) }

    pub fn get_u64(&self, off: usize) -> u64 { u64::from_le_bytes(self.bytes[off..off + 8].try_into().unwrap()) }
}

/// Signaux en attente (d'un fil ou d'un processus). Un signal standard (< 32) n'est mémorisé
/// qu'une fois ; les signaux temps réel s'empilent avec leur `siginfo`.
#[derive(Default)]
pub struct SigQueue {
    pub mask: u64,
    infos: VecDeque<SigInfo>,
}

impl SigQueue {
    pub fn new() -> SigQueue { SigQueue { mask: 0, infos: VecDeque::new() } }

    /// Met `info` en file ; `false` si rien n'a été ajouté (signal standard déjà en attente,
    /// file des signaux temps réel pleine).
    pub fn push(&mut self, info: SigInfo) -> bool {
        let sig = info.signo();
        if sig < 32 && self.mask & sigbit(sig) != 0 {
            return false;
        }
        if self.infos.len() >= 1024 && sig >= 32 {
            return false; // RLIMIT_SIGPENDING
        }
        self.infos.push_back(info);
        self.mask |= sigbit(sig);
        true
    }

    /// Retire le premier signal de `allowed` : synchrones d'abord, puis le plus petit numéro.
    pub fn take(&mut self, allowed: u64) -> Option<SigInfo> {
        let avail = self.mask & allowed;
        if avail == 0 {
            return None;
        }
        let pick = if avail & SYNCHRONOUS != 0 { avail & SYNCHRONOUS } else { avail };
        let sig = pick.trailing_zeros() as i32 + 1;
        let pos = self.infos.iter().position(|i| i.signo() == sig)?;
        let info = self.infos.remove(pos)?;
        if !self.infos.iter().any(|i| i.signo() == sig) {
            self.mask &= !sigbit(sig);
        }
        Some(info)
    }

    pub fn discard(&mut self, sigs: u64) {
        if self.mask & sigs == 0 {
            return;
        }
        self.infos.retain(|i| sigs & sigbit(i.signo()) == 0);
        self.mask &= !sigs;
    }
}

/// Action par défaut.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Default_ {
    Term,
    Core,
    Ignore,
    Stop,
    Continue,
}

fn default_action(sig: i32) -> Default_ {
    match sig {
        SIGCHLD | 23 | 28 => Default_::Ignore, // SIGCHLD, SIGURG, SIGWINCH
        SIGCONT => Default_::Continue,
        SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU => Default_::Stop,
        SIGQUIT | SIGILL | SIGTRAP | SIGABRT | SIGBUS | SIGFPE | SIGSEGV | 24 | 25 | SIGSYS => Default_::Core,
        _ => Default_::Term,
    }
}

/// Vrai si `sig` serait ignoré par `p` à l'arrivée (ni gestionnaire, ni effet par défaut).
pub(crate) fn ignored(p: &Process, sig: i32) -> bool {
    if sig == SIGKILL || sig == SIGSTOP {
        return false;
    }
    let act = p.sigaction(sig);
    act.handler == SIG_IGN || (act.handler == SIG_DFL && default_action(sig) == Default_::Ignore)
}

// --- Envoi -----------------------------------------------------------------------------------

/// Effets immédiats d'un signal sur le processus (avant la mise en file) : `SIGCONT` reprend un
/// processus arrêté et annule les arrêts en attente ; un signal d'arrêt annule `SIGCONT` ;
/// `SIGKILL` reprend aussi (pour mourir).
fn prepare(p: &Arc<Process>, sig: i32) {
    if STOP_SIGNALS & sigbit(sig) != 0 {
        p.discard_pending(sigbit(SIGCONT));
    }
    if sig == SIGCONT || sig == SIGKILL {
        if sig == SIGCONT {
            p.discard_pending(STOP_SIGNALS);
        }
        let resumed = {
            let mut st = p.stop.lock();
            let was = st.stopped;
            st.stopped = false;
            if was && sig == SIGCONT {
                st.report_cont = true;
                st.report_stop = false;
            }
            was
        };
        if resumed {
            p.stop_wait.wake_all();
            if sig == SIGCONT {
                crate::notify_parent(p, CLD_CONTINUED, SIGCONT);
            }
        }
    }
}

/// Signal produit par le noyau (`SEND_SIG_PRIV`) : `si_code` positif (`SI_KERNEL`, `CLD_*`,
/// `POLL_*`…). Les codes négatifs ou nuls viennent d'un processus (`SI_USER`, `SI_QUEUE`,
/// `SI_TKILL`, `SI_TIMER`…).
fn kernel_sent(info: &SigInfo) -> bool {
    info.get_i32(8) > 0
}

/// L'init d'un espace pid ne reçoit que les signaux qu'il gère, sauf `SIGKILL`/`SIGSTOP`
/// « forcés » : venus du noyau ou d'un processus d'un espace ancêtre (`sig_task_ignored` avec
/// `SIGNAL_UNKILLABLE`). Un `SIGTERM` sans gestionnaire est donc ignoré, même de l'hôte.
fn unkillable_drops(p: &Process, sig: i32, force: bool) -> bool {
    p.unkillable.load(Ordering::Acquire) && p.sigaction(sig).handler == SIG_DFL && !(force && (sig == SIGKILL || sig == SIGSTOP))
}

/// Envoie `info` au fil `t` (signal dirigé : `tkill`, faute). `force` : faute synchrone, ni
/// bloquable ni ignorable. Rend `true` si `info` a été mis en file.
pub fn send_to_thread(t: &Arc<Thread>, info: SigInfo, force: bool) -> bool {
    let ns_force = force || kernel_sent(&info);
    send_thread_inner(t, info, force, ns_force)
}

/// Comme [`send_to_thread`] pour un signal envoyé par un processus (`tgkill`,
/// `rt_tgsigqueueinfo`) ; `from_outside` : l'envoyeur est invisible dans l'espace pid de la
/// cible (espace ancêtre).
pub fn send_to_thread_as(t: &Arc<Thread>, info: SigInfo, from_outside: bool) -> bool {
    send_thread_inner(t, info, false, from_outside || kernel_sent(&info))
}

fn send_thread_inner(t: &Arc<Thread>, info: SigInfo, force: bool, ns_force: bool) -> bool {
    let p = &t.process;
    let sig = info.signo();
    if p.group_exit.load(Ordering::Acquire) {
        return false;
    }
    // Une faute synchrone tue même l'init d'un espace (`force_sig_info_to_task`).
    if !force && sig == SIGKILL && unkillable_drops(p, sig, ns_force) {
        return false;
    }
    prepare(p, sig);
    if !force && unkillable_drops(p, sig, ns_force) {
        return false;
    }
    if force {
        let bit = sigbit(sig);
        let blocked = t.sigmask.load(Ordering::Acquire) & bit != 0;
        let sh = p.sighand();
        let mut acts = sh.actions.lock();
        let a = &mut acts[sig as usize - 1];
        if blocked || a.handler == SIG_IGN {
            *a = SigAction::default();
            t.sigmask.fetch_and(!bit, Ordering::AcqRel);
        }
    } else if ignored(p, sig) && t.sigmask.load(Ordering::Acquire) & sigbit(sig) == 0 {
        return false;
    }
    let queued = t.pending.lock().push(info);
    if sig == SIGKILL {
        crate::group_exit(p, signal_status(SIGKILL), None);
    }
    t.kick(WORK_SIGNAL);
    p.signal_wait.wake_all();
    queued
}

/// Envoie `info` au processus `p` (`kill`) : mis dans sa file partagée, un fil qui ne le bloque
/// pas est prévenu. Rend `true` si `info` a été mis en file.
pub fn send_to_process(p: &Arc<Process>, info: SigInfo) -> bool {
    let force = kernel_sent(&info);
    send_process_inner(p, info, force)
}

/// Comme [`send_to_process`] pour un signal envoyé par un processus (`kill`,
/// `rt_sigqueueinfo`, `pidfd_send_signal`, `PR_SET_PDEATHSIG`) ; `from_outside` : l'envoyeur
/// est invisible dans l'espace pid de la cible.
pub fn send_to_process_as(p: &Arc<Process>, info: SigInfo, from_outside: bool) -> bool {
    let force = from_outside || kernel_sent(&info);
    send_process_inner(p, info, force)
}

fn send_process_inner(p: &Arc<Process>, info: SigInfo, force: bool) -> bool {
    let sig = info.signo();
    if p.group_exit.load(Ordering::Acquire) || p.is_zombie() {
        return false;
    }
    if sig == SIGKILL && unkillable_drops(p, sig, force) {
        return false;
    }
    prepare(p, sig);
    if sig == SIGKILL {
        crate::group_exit(p, signal_status(SIGKILL), None);
        return false;
    }
    if unkillable_drops(p, sig, force) {
        return false;
    }
    if ignored(p, sig) {
        // Gardé seulement si tous les fils le bloquent (le gestionnaire peut changer d'ici là).
        let all_block = p.for_each_thread_until(|t| t.sigmask.load(Ordering::Acquire) & sigbit(sig) == 0).is_none();
        if !all_block {
            return false;
        }
    }
    let queued = p.shared_pending.lock().push(info);
    // Un fil qui ne le bloque pas ; à défaut, personne (il attend un déblocage).
    let bit = sigbit(sig);
    let target = p.for_each_thread_until(|t| t.sigmask.load(Ordering::Acquire) & bit == 0);
    if let Some(t) = target {
        t.kick(WORK_SIGNAL);
    }
    p.signal_wait.wake_all();
    queued
}

/// Vrai si `t` a un signal livrable (ou doit se terminer).
pub fn deliverable(t: &Thread) -> bool {
    if t.work.load(Ordering::Acquire) & WORK_EXIT != 0 || t.process.group_exit.load(Ordering::Acquire) {
        return true;
    }
    let allowed = !t.sigmask.load(Ordering::Acquire);
    t.pending.lock().mask & allowed != 0 || t.process.shared_pending.lock().mask & allowed != 0
}

/// Après un changement de masque : prévient le fil s'il a désormais quelque chose à livrer.
pub fn recalc(t: &Thread) {
    if deliverable(t) {
        t.work.fetch_or(WORK_SIGNAL, Ordering::AcqRel);
    }
}

fn dequeue(t: &Thread) -> Option<SigInfo> {
    dequeue_from(t, !t.sigmask.load(Ordering::Acquire))
}

/// Retire un signal de `set` (même bloqué) : `rt_sigtimedwait`. Un signal de minuteur POSIX
/// reçoit là son dépassement (`si_overrun`), hors des verrous des files.
pub fn dequeue_from(t: &Thread, set: u64) -> Option<SigInfo> {
    let taken = t.pending.lock().take(set);
    let mut info = taken.or_else(|| t.process.shared_pending.lock().take(set))?;
    crate::ptimer::on_dequeue(&t.process, &mut info);
    Some(info)
}

// --- Retour vers l'utilisateur ------------------------------------------------------------------

#[inline]
fn is_restart(rax: u64) -> bool {
    let r = -(rax as i64);
    matches!(r, ERESTARTSYS | ERESTARTNOINTR | ERESTARTNOHAND | ERESTART_RESTARTBLOCK)
}

/// Crochet de `uentry` : juste avant chaque retour en mode utilisateur (interruptions coupées).
pub(crate) fn return_hook(regs: &mut UserRegs) {
    let t = current(regs) as *const Thread;
    // SAFETY : voir `current`.
    let work = unsafe { (*t).work.load(Ordering::Acquire) };
    let restart = uentry::is_syscall_vector(regs.vector) && is_restart(regs.rax);
    if work == 0 && !restart {
        return;
    }
    // SAFETY : retour vers l'utilisateur sur la pile noyau du fil ; rien n'est tenu.
    unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
    do_work(t, regs);
}

/// Relance (ou `EINTR`) d'un appel système interrompu. `handler` : un gestionnaire va s'exécuter.
fn fix_restart(t: &Thread, regs: &mut UserRegs, handler: Option<&SigAction>) {
    if !uentry::is_syscall_vector(regs.vector) || !is_restart(regs.rax) {
        return;
    }
    let code = -(regs.rax as i64);
    let restart = match (code, handler) {
        (ERESTARTNOINTR, _) => true,
        (ERESTARTSYS, Some(a)) => a.flags & SA_RESTART != 0,
        (_, Some(_)) => false,
        (_, None) => true,
    };
    if !restart {
        regs.rax = (-EINTR) as u64;
        t.restart.lock().take();
        return;
    }
    let compat = uentry::is_syscall32_vector(regs.vector);
    if code == ERESTART_RESTARTBLOCK {
        regs.rax = if compat { NR_RESTART_SYSCALL32 } else { NR_RESTART_SYSCALL };
    } else {
        regs.rax = t.last_syscall.load(Ordering::Relaxed);
    }
    regs.rip -= 2; // longueur de `syscall`, `int 0x80` (vDSO : `syscall` suit `int 0x80`)
    if regs.vector == uentry::SYSCALL32_VECTOR {
        // Relance par `int 0x80` (pas de retour `sysretl` vers une instruction rejouée).
        regs.vector = uentry::INT80_VECTOR;
    }
}

/// Plantage (signal « core » fatal) : d'où vient le signal (faute, `kill` d'un autre,
/// `raise`), et la dernière faute du fil, adresses traduites en `fichier+décalage` (un
/// gestionnaire qui relance le signal par `tgkill` masque sinon la faute d'origine).
fn crash_report(t: &Thread, sig: i32, info: &SigInfo, regs: &UserRegs) {
    let p = &t.process;
    let mm = p.mm();
    let at = |addr: u64| -> String {
        let mut s = String::new();
        if let Some(mm) = &mm {
            uvm::for_each_vma(mm, |v| {
                if s.is_empty() && (v.start..v.end).contains(&(addr as usize)) {
                    let base = match &v.backing {
                        uvm::Backing::File { offset, .. } => *offset,
                        uvm::Backing::Shared { pgoff, .. } => pgoff * 4096,
                        uvm::Backing::Anon => 0,
                    };
                    s = alloc::format!("{}+{:#x}", v.name.as_str(), base + (addr as usize - v.start));
                }
            });
        }
        if s.is_empty() { alloc::format!("{:#x}", addr) } else { s }
    };
    log::warn!(
        "maiproc : pid {} fil {} : signal {} fatal (si_code {}, pid/adresse {:#x}) a {} (rsp {:#x}, dernier appel {})",
        p.pid, t.tid(), sig, info.get_i32(8), info.get_u64(16), at(regs.rip), regs.rsp,
        t.last_syscall.load(Ordering::Relaxed)
    );
    if let Some((vector, err, addr, rip, fs)) = *t.last_fault.lock() {
        log::warn!(
            "maiproc : pid {} fil {} : derniere faute : vecteur {} code {:#x} adresse {:#x} a {} (fs_base {:#x})",
            p.pid, t.tid(), vector, err, addr, at(rip), fs
        );
    }
}

fn do_work(tp: *const Thread, regs: &mut UserRegs) {
    // SAFETY : le fil vit tant que sa tâche peut entrer dans le noyau.
    let t = unsafe { &*tp };
    loop {
        let p = &t.process;
        if t.work.load(Ordering::Acquire) & WORK_EXIT != 0 || p.group_exit.load(Ordering::Acquire) {
            let status = p.exit_status.load(Ordering::Acquire);
            exit_current(tp, status);
        }
        if p.stop.lock().stopped {
            do_stop(t);
            continue;
        }
        t.work.fetch_and(!WORK_SIGNAL, Ordering::AcqRel);
        let Some(info) = dequeue(t) else {
            break;
        };
        let sig = info.signo();
        let act = p.sigaction(sig);
        if act.handler == SIG_IGN && sig != SIGKILL {
            continue;
        }
        if act.handler == SIG_DFL || sig == SIGKILL {
            match default_action(sig) {
                Default_::Ignore | Default_::Continue => continue,
                Default_::Stop => {
                    begin_group_stop(p, sig);
                    continue;
                }
                Default_::Term | Default_::Core => {
                    if default_action(sig) == Default_::Core && sig != SIGKILL {
                        crash_report(t, sig, &info, regs);
                    }
                    crate::group_exit(p, signal_status(sig), Some(t.tid()));
                    exit_current(tp, p.exit_status.load(Ordering::Acquire));
                }
            }
        }
        // Gestionnaire utilisateur : un cadre par retour (les suivants au retour de celui-ci).
        fix_restart(t, regs, Some(&act));
        match setup_frame(t, regs, &info, &act) {
            Ok(()) => {
                if act.flags & SA_RESETHAND != 0 {
                    p.sighand().actions.lock()[sig as usize - 1] = SigAction::default();
                }
                recalc(t);
                return;
            }
            Err(()) => {
                log::warn!("maiproc : pid {} cadre du signal {} impossible a ecrire -> SIGSEGV", p.pid, sig);
                crate::group_exit(p, signal_status(SIGSEGV), Some(t.tid()));
                exit_current(tp, p.exit_status.load(Ordering::Acquire));
            }
        }
    }
    // Aucun gestionnaire : relance de l'appel interrompu, masque de `sigsuspend` rétabli.
    fix_restart(t, regs, None);
    if let Some(m) = t.saved_sigmask.lock().take() {
        t.sigmask.store(m, Ordering::Release);
    }
    recalc(t);
}

/// Début d'un arrêt de groupe (`SIGSTOP`, `SIGTSTP`… par défaut).
fn begin_group_stop(p: &Arc<Process>, sig: i32) {
    {
        let mut st = p.stop.lock();
        if st.stopped {
            return;
        }
        st.stopped = true;
        st.code = sig;
        st.report_stop = true;
        st.report_cont = false;
    }
    p.for_each_thread_until(|t| {
        t.kick(WORK_SIGNAL);
        false
    });
    crate::notify_parent(p, CLD_STOPPED, sig);
}

/// Fil arrêté : attend `SIGCONT` (ou `SIGKILL`, fin du groupe).
fn do_stop(t: &Thread) {
    let p = &t.process;
    let _ = crate::wait::wait_event(&[&p.stop_wait], None, None, || {
        let resumed = !p.stop.lock().stopped;
        let dying = p.group_exit.load(Ordering::Acquire) || t.work.load(Ordering::Acquire) & WORK_EXIT != 0;
        (resumed || dying).then_some(())
    });
}

// --- Cadre de signal ---------------------------------------------------------------------------

const FRAME_SIZE: usize = 8 + UC_SIZE + 128;
const UC_SIZE: usize = 304;
const UC_FP_XSTATE: u64 = 1;
const UC_SIGCONTEXT_SS: u64 = 2;
const UC_STRICT_RESTORE_SS: u64 = 4;
const FP_XSTATE_MAGIC1: u32 = 0x4650_5853;
const FP_XSTATE_MAGIC2: u32 = 0x4650_5845;
const SS_ONSTACK: i32 = 1;
const SS_DISABLE: i32 = 2;
const RFLAGS_TF: u64 = 1 << 8;
const RFLAGS_DF: u64 = 1 << 10;
const RFLAGS_RF: u64 = 1 << 16;

/// Pile alternative (`sigaltstack`).
#[derive(Clone, Copy, Default)]
pub struct AltStack {
    pub sp: u64,
    pub size: u64,
    pub flags: i32,
}

impl AltStack {
    pub fn enabled(&self) -> bool { self.flags & SS_DISABLE == 0 && self.size != 0 }

    pub fn contains(&self, sp: u64) -> bool { self.enabled() && sp > self.sp && sp - self.sp <= self.size }

    /// `ss_flags` vus par l'utilisateur pour la pile `sp`.
    pub fn user_flags(&self, sp: u64) -> i32 {
        if !self.enabled() { SS_DISABLE } else if self.contains(sp) { SS_ONSTACK } else { 0 }
    }

    pub fn disabled() -> AltStack { AltStack { sp: 0, size: 0, flags: SS_DISABLE } }
}

/// Taille de l'état vectoriel dans un cadre.
pub fn fpstate_size() -> usize {
    if uentry::uses_xsave() { uentry::xsave_area_size() + 4 } else { 512 }
}

fn user_fpu() -> Option<&'static uentry::FpuState> {
    let task = task::get_my_current_task()?;
    let ctx = task.user.get()? as *const task_struct::UserContext;
    // SAFETY : le contexte vit autant que la tâche, qui est la tâche courante.
    Some(unsafe { &(*ctx).fpu })
}

/// Copie des registres vectoriels courants (interruptions coupées le temps de la sauvegarde).
fn save_fpu_bytes() -> alloc::vec::Vec<u8> {
    let Some(fpu) = user_fpu() else { return vec![0u8; 512] };
    let held = irq_safety::hold_interrupts();
    // SAFETY : interruptions coupées, état du fil courant, chargé dans les registres.
    let out = unsafe {
        fpu.save();
        fpu.bytes_mut().to_vec()
    };
    drop(held);
    out
}

/// Charge `bytes` (ou l'état initial) dans les registres vectoriels du fil courant.
fn load_fpu_bytes(bytes: Option<&[u8]>) {
    let Some(fpu) = user_fpu() else { return };
    let held = irq_safety::hold_interrupts();
    // SAFETY : interruptions coupées ; zone du fil courant, rechargée aussitôt.
    unsafe {
        match bytes {
            Some(b) => {
                let area = fpu.bytes_mut();
                let n = b.len().min(area.len());
                area[..n].copy_from_slice(&b[..n]);
            }
            None => fpu.reset(),
        }
        fpu.restore();
    }
    drop(held);
}

/// Remet l'état vectoriel du fil courant à l'état initial (`execve`).
pub fn reset_fpu() {
    load_fpu_bytes(None);
}

/// État vectoriel courant tel qu'un cadre de signal le porte : image XSAVE avec son en-tête
/// logiciel (`fx_sw_bytes`) et le marqueur final, ou FXSAVE seule ([`fpstate_size`] octets).
pub(crate) fn save_fpu_frame() -> alloc::vec::Vec<u8> {
    let mut fp = save_fpu_bytes();
    if uentry::uses_xsave() {
        let size = uentry::xsave_area_size();
        fp.resize(size + 4, 0);
        fp[464..468].copy_from_slice(&FP_XSTATE_MAGIC1.to_le_bytes());
        fp[468..472].copy_from_slice(&((size + 4) as u32).to_le_bytes());
        fp[472..480].copy_from_slice(&uentry::xcr0().to_le_bytes());
        fp[480..484].copy_from_slice(&(size as u32).to_le_bytes());
        fp[size..size + 4].copy_from_slice(&FP_XSTATE_MAGIC2.to_le_bytes());
    } else {
        fp.resize(512, 0);
    }
    fp
}

/// Recharge un état vectoriel lu dans un cadre (assaini), ou l'état initial.
pub(crate) fn restore_fpu_frame(bytes: Option<&[u8]>) {
    match bytes {
        Some(b) => {
            let mut b = b.to_vec();
            sanitize_fpstate(&mut b);
            load_fpu_bytes(Some(&b));
        }
        None => load_fpu_bytes(None),
    }
}

fn put_u64(b: &mut [u8], off: usize, v: u64) { b[off..off + 8].copy_from_slice(&v.to_le_bytes()); }
fn get_u64(b: &[u8], off: usize) -> u64 { u64::from_le_bytes(b[off..off + 8].try_into().unwrap()) }

fn setup_frame(t: &Thread, regs: &mut UserRegs, info: &SigInfo, act: &SigAction) -> Result<(), ()> {
    if act.flags & SA_IA32_ABI != 0 {
        return crate::signal32::setup_frame(t, regs, info, act);
    }
    if act.flags & SA_RESTORER == 0 || act.restorer == 0 {
        return Err(()); // pas de trampoline de retour : pas de vDSO pour en fournir un
    }
    let mm = t.mm();
    let alt = *t.altstack.lock();
    let mut sp = regs.rsp;
    if act.flags & SA_ONSTACK != 0 && alt.enabled() && !alt.contains(sp) {
        sp = alt.sp + alt.size;
    } else {
        sp = sp.wrapping_sub(128); // zone rouge
    }
    let fp_size = fpstate_size() as u64;
    sp = (sp.wrapping_sub(fp_size)) & !63;
    let fp_addr = sp;
    sp = ((sp.wrapping_sub(FRAME_SIZE as u64)) & !15).wrapping_sub(8);
    let frame = sp;
    if alt.contains(regs.rsp) && !alt.contains(frame + 1) && act.flags & SA_ONSTACK != 0 {
        return Err(()); // débordement de la pile alternative
    }

    // État vectoriel.
    let fp = save_fpu_frame();
    mm.copy_to_user(fp_addr as usize, &fp).map_err(|_| ())?;

    // ucontext + sigcontext.
    let old_mask = t.saved_sigmask.lock().take().unwrap_or_else(|| t.sigmask.load(Ordering::Acquire));
    let mut f = [0u8; FRAME_SIZE];
    put_u64(&mut f, 0, act.restorer);
    let uc = 8;
    put_u64(&mut f, uc, UC_FP_XSTATE | UC_SIGCONTEXT_SS | UC_STRICT_RESTORE_SS);
    put_u64(&mut f, uc + 8, 0);
    put_u64(&mut f, uc + 16, alt.sp);
    f[uc + 24..uc + 28].copy_from_slice(&alt.user_flags(regs.rsp).to_le_bytes());
    put_u64(&mut f, uc + 32, alt.size);
    let mc = uc + 40;
    let gpr = [
        regs.r8, regs.r9, regs.r10, regs.r11, regs.r12, regs.r13, regs.r14, regs.r15,
        regs.rdi, regs.rsi, regs.rbp, regs.rbx, regs.rdx, regs.rax, regs.rcx, regs.rsp,
        regs.rip, regs.rflags,
    ];
    for (i, v) in gpr.iter().enumerate() {
        put_u64(&mut f, mc + i * 8, *v);
    }
    f[mc + 144..mc + 146].copy_from_slice(&(uentry::USER_CS as u16).to_le_bytes());
    f[mc + 150..mc + 152].copy_from_slice(&(uentry::USER_SS as u16).to_le_bytes());
    let (trapno, err, cr2) = t.fault_info.lock().take().unwrap_or((0, 0, 0));
    put_u64(&mut f, mc + 152, err);
    put_u64(&mut f, mc + 160, trapno);
    put_u64(&mut f, mc + 168, old_mask);
    put_u64(&mut f, mc + 176, cr2);
    put_u64(&mut f, mc + 184, fp_addr);
    put_u64(&mut f, uc + 296, old_mask);
    f[8 + UC_SIZE..].copy_from_slice(&info.bytes);
    mm.copy_to_user(frame as usize, &f).map_err(|_| ())?;

    // Masque pendant le gestionnaire.
    let sig = info.signo();
    let mut mask = old_mask | act.mask;
    if act.flags & SA_NODEFER == 0 {
        mask |= sigbit(sig);
    }
    t.sigmask.store(sanitize_mask(mask), Ordering::Release);

    regs.rdi = sig as u64;
    regs.rsi = frame + 8 + UC_SIZE as u64;
    regs.rdx = frame + 8;
    regs.rax = 0;
    regs.rsp = frame;
    regs.rip = act.handler;
    regs.rflags &= !(RFLAGS_TF | RFLAGS_DF | RFLAGS_RF);
    regs.vector = FORCE_IRET_VECTOR;
    load_fpu_bytes(None);
    Ok(())
}

/// Cadre de signal illisible ou invalide.
#[derive(Debug)]
pub struct BadFrame;

/// `rt_sigreturn` : rétablit le contexte enregistré par [`setup_frame`]. `Err` : cadre illisible
/// ou invalide (le fil reçoit alors `SIGSEGV`).
pub fn sigreturn(t: &Arc<Thread>, regs: &mut UserRegs) -> Result<(), BadFrame> {
    let mm = t.mm();
    let frame = regs.rsp.wrapping_sub(8);
    let mut uc = [0u8; UC_SIZE];
    mm.copy_from_user(&mut uc, frame as usize + 8).map_err(|_| BadFrame)?;
    let mc = 40;
    let g = |i: usize| get_u64(&uc, mc + i * 8);
    let rip = g(16);
    let rsp = g(15);
    let limit = kernel_config::memory::USER_SPACE_END as u64;
    if rip >= limit || rsp >= limit {
        return Err(BadFrame);
    }
    // État vectoriel d'abord (lecture qui peut échouer avant toute modification).
    let fp_addr = get_u64(&uc, mc + 184);
    let fp = if fp_addr != 0 {
        let mut b = vec![0u8; fpstate_size()];
        mm.copy_from_user(&mut b, fp_addr as usize).map_err(|_| BadFrame)?;
        sanitize_fpstate(&mut b);
        Some(b)
    } else {
        None
    };
    regs.r8 = g(0);
    regs.r9 = g(1);
    regs.r10 = g(2);
    regs.r11 = g(3);
    regs.r12 = g(4);
    regs.r13 = g(5);
    regs.r14 = g(6);
    regs.r15 = g(7);
    regs.rdi = g(8);
    regs.rsi = g(9);
    regs.rbp = g(10);
    regs.rbx = g(11);
    regs.rdx = g(12);
    regs.rax = g(13);
    regs.rcx = g(14);
    regs.rsp = rsp;
    regs.rip = rip;
    regs.rflags = g(17); // assaini par `uentry` au retour (bits utilisateur seulement)
    regs.vector = FORCE_IRET_VECTOR;
    let mask = get_u64(&uc, 296);
    t.sigmask.store(sanitize_mask(mask), Ordering::Release);
    // Pile alternative enregistrée (erreurs ignorées, comme `restore_altstack`).
    let ss_sp = get_u64(&uc, 16);
    let ss_flags = i32::from_le_bytes(uc[24..28].try_into().unwrap());
    let ss_size = get_u64(&uc, 32);
    {
        let mut alt = t.altstack.lock();
        if !alt.contains(rsp) {
            *alt = if ss_flags & SS_DISABLE != 0 { AltStack::disabled() } else { AltStack { sp: ss_sp, size: ss_size, flags: 0 } };
        }
    }
    load_fpu_bytes(fp.as_deref());
    recalc(t);
    Ok(())
}

/// Rend sûr un état vectoriel venu de l'utilisateur avant `xrstor` (qui ferait #GP en mode
/// noyau sur un en-tête invalide).
fn sanitize_fpstate(b: &mut [u8]) {
    // MXCSR : bits réservés à zéro.
    let mxcsr = u32::from_le_bytes(b[24..28].try_into().unwrap()) & 0xFFFF;
    b[24..28].copy_from_slice(&mxcsr.to_le_bytes());
    if !uentry::uses_xsave() || b.len() < 576 {
        return;
    }
    let magic = u32::from_le_bytes(b[464..468].try_into().unwrap());
    let xcr0 = uentry::xcr0();
    let mut bv = get_u64(b, 512);
    if magic != FP_XSTATE_MAGIC1 {
        // Cadre FXSAVE seul : x87 et SSE.
        bv &= 0b11;
        let size = b.len();
        b[576..size].fill(0);
    }
    bv &= xcr0;
    put_u64(b, 512, bv);
    put_u64(b, 520, 0); // XCOMP_BV : format standard
    b[528..576].fill(0);
}
