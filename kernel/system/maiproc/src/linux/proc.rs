//! Appels système de processus : création (`clone`, `fork`, `vfork`, `clone3`), `execve`, attente
//! des enfants, signaux, groupes et sessions, minuteur réel.

use alloc::{string::String, sync::Arc, sync::Weak, vec::Vec};
use core::sync::atomic::Ordering;
use uentry::UserRegs;

use super::errno::*;
use super::{fault, user_path, Outcome, SysResult};
use crate::signal::{self, sigbit, SigAction, SigInfo, SIGKILL, SIGSTOP};
use crate::wait::{wait_event, WaitError};
use crate::{exit_status, find_process_v, find_thread_v, CloneSpec, Process, Thread};

const ECHILD_: i64 = ECHILD;

// --- Création ----------------------------------------------------------------------------------

const CLONE_PIDFD: u64 = 0x1000;

/// Arguments communs de `clone`, `clone3`, `fork`, `vfork`.
#[derive(Default)]
struct CloneArgs {
    flags: u64,
    exit_signal: i32,
    stack: u64,
    tls: u64,
    parent_tid: u64,
    child_tid: u64,
    /// `CLONE_PIDFD` : où écrire le descripteur (`parent_tid` pour `clone`, champ `pidfd` de
    /// `clone3`).
    pidfd: u64,
    set_tid: Vec<u32>,
}

fn do_clone(t: &Thread, regs: &UserRegs, a: CloneArgs) -> SysResult {
    use crate::{CLONE_SIGHAND, CLONE_THREAD, CLONE_VM};
    let CloneArgs { flags, exit_signal, stack, tls, parent_tid, child_tid, pidfd, set_tid } = a;
    // Les espaces de noms (`CLONE_NEW*`) sont validés par `ns::for_clone`.
    if (flags & CLONE_THREAD != 0 && flags & CLONE_SIGHAND == 0) || (flags & CLONE_SIGHAND != 0 && flags & CLONE_VM == 0) {
        return Err(EINVAL);
    }
    // Un pidfd désigne un processus, pas un fil (`CLONE_PIDFD | CLONE_THREAD` refusé).
    if flags & CLONE_PIDFD != 0 && flags & CLONE_THREAD != 0 {
        return Err(EINVAL);
    }
    if !(0..=64).contains(&exit_signal) {
        return Err(EINVAL);
    }
    // Appel i386 : `tls` désigne un `user_desc` (entrée TLS de la GDT), comme `set_new_tls`.
    let tls32 = if flags & crate::CLONE_SETTLS != 0 && super::compat::active(t) {
        Some(super::compat::clone_tls(t, tls)?)
    } else {
        None
    };
    let spec = CloneSpec { flags, exit_signal: if flags & CLONE_THREAD != 0 { 0 } else { exit_signal }, stack, tls, child_tid, parent_tid, tls32, set_tid };
    let c = crate::clone_thread(t, regs, &spec)?;
    if flags & CLONE_PIDFD != 0 {
        let fd = crate::pidfd::install(t, c.process.clone(), 0)?;
        t.mm().write_user::<i32>(pidfd as usize, &(fd as i32)).map_err(fault)?;
    }
    if let Some(done) = c.vfork {
        // Le parent attend que l'enfant ait quitté l'espace partagé (`execve` ou fin).
        done.wait(None);
    }
    Ok(c.nr as i64)
}

/// `clone(flags, newsp, parent_tid, child_tid, tls)` (ordre x86_64).
pub fn clone(t: &Thread, regs: &UserRegs, flags: u64, newsp: u64, ptid: u64, ctid: u64, tls: u64) -> SysResult {
    // `clone` range le pidfd là où irait `parent_tid` : les deux ensemble sont refusés.
    if flags & CLONE_PIDFD != 0 && flags & crate::CLONE_PARENT_SETTID != 0 {
        return Err(EINVAL);
    }
    let a = CloneArgs { flags: flags & !0xff, exit_signal: (flags & 0xff) as i32, stack: newsp, tls, parent_tid: ptid, child_tid: ctid, pidfd: ptid, set_tid: Vec::new() };
    do_clone(t, regs, a)
}

pub fn fork(t: &Thread, regs: &UserRegs) -> SysResult {
    do_clone(t, regs, CloneArgs { exit_signal: signal::SIGCHLD, ..Default::default() })
}

pub fn vfork(t: &Thread, regs: &UserRegs) -> SysResult {
    do_clone(t, regs, CloneArgs { flags: crate::CLONE_VM | crate::CLONE_VFORK, exit_signal: signal::SIGCHLD, ..Default::default() })
}

/// `clone3(struct clone_args *, size)`.
pub fn clone3(t: &Thread, regs: &UserRegs, args: u64, size: u64) -> SysResult {
    if !(64..=4096).contains(&size) {
        return Err(EINVAL);
    }
    let mut raw = [0u8; 88];
    let n = (size as usize).min(raw.len());
    t.mm().copy_from_user(&mut raw[..n], args as usize).map_err(fault)?;
    let g = |i: usize| u64::from_le_bytes(raw[i * 8..i * 8 + 8].try_into().unwrap());
    let (flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls) = (g(0), g(1), g(2), g(3), g(4), g(5), g(6), g(7));
    let (set_tid_ptr, set_tid_size) = if n > 64 { (g(8), g(9)) } else { (0, 0) };
    // `set_tid` : numéros voulus de l'enfant, de son espace pid vers ses ancêtres (CRIU).
    if set_tid_size > crate::pidns::MAX_PID_NS_LEVEL as u64 || (set_tid_size == 0) != (set_tid_ptr == 0) {
        return Err(EINVAL);
    }
    let mut set_tid = Vec::new();
    for i in 0..set_tid_size as usize {
        let v = t.mm().read_user::<i32>(set_tid_ptr as usize + i * 4).map_err(fault)?;
        if v <= 0 {
            return Err(EINVAL);
        }
        set_tid.push(v as u32);
    }
    if exit_signal > 64 || flags & 0xff != 0 {
        return Err(EINVAL);
    }
    let sp = if stack != 0 { stack.checked_add(stack_size).ok_or(EINVAL)? } else { 0 };
    do_clone(t, regs, CloneArgs { flags, exit_signal: exit_signal as i32, stack: sp, tls, parent_tid, child_tid, pidfd, set_tid })
}

// --- execve ----------------------------------------------------------------------------------

/// Lit un tableau de chaînes terminé par un pointeur nul (`argv`, `envp`).
fn read_strings(t: &Thread, ptr: u64, total: &mut usize) -> Result<Vec<String>, i64> {
    let mut out = Vec::new();
    if ptr == 0 {
        return Ok(out);
    }
    let mm = t.mm();
    // Tableau de pointeurs de 4 octets pour un appel i386 (`compat_ptr`).
    let compat = super::compat::active(t);
    for i in 0..32768usize {
        let p = if compat {
            mm.read_user::<u32>(ptr as usize + i * 4).map_err(fault)? as u64
        } else {
            mm.read_user::<u64>(ptr as usize + i * 8).map_err(fault)?
        };
        if p == 0 {
            return Ok(out);
        }
        let s = mm.read_cstr(p as usize, 128 * 1024).map_err(|e| match e {
            uvm::VmError::Invalid => E2BIG,
            e => fault(e),
        })?;
        *total += s.len() + 1 + 8;
        if *total > crate::exec::MAX_ARGS_BYTES {
            return Err(E2BIG);
        }
        // Octets quelconques (variables d'environnement non UTF-8) : sans perte (PEP 383).
        out.push(fs_node::rawname::decode(&s));
    }
    Err(E2BIG)
}

/// Les autres fils du processus se terminent avant qu'`execve` ne remplace l'image.
fn de_thread(t: &Thread) -> Result<(), ()> {
    let p = &t.process;
    if p.live_threads.load(Ordering::Acquire) > 1 {
        let me = t.tid();
        p.for_each_thread_until(|o| {
            if o.tid() != me {
                o.kick(crate::WORK_EXIT);
            }
            false
        });
        let _ = wait_event(&[&p.exit_wait], None, None, || {
            (p.live_threads.load(Ordering::Acquire) <= 1 || p.group_exit.load(Ordering::Acquire)).then_some(())
        });
    }
    if p.group_exit.load(Ordering::Acquire) {
        return Err(());
    }
    // Un fil secondaire qui exécute prend l'identité du chef de groupe.
    if t.tid() != p.pid {
        crate::retid(t, p.pid);
    }
    Ok(())
}

/// `execve` / `execveat`.
pub fn execve(t: &Thread, regs: &mut UserRegs, dirfd: i64, path_ptr: u64, argv_ptr: u64, envp_ptr: u64, flags: u64) -> Result<Outcome, i64> {
    const AT_EMPTY_PATH: u64 = 0x1000;
    let path = user_path(t, path_ptr)?;
    let abs = if path.is_empty() && flags & AT_EMPTY_PATH != 0 {
        super::fs::fd_path(t, dirfd as u64)?
    } else {
        super::fs::absolute(t, dirfd, &path)?
    };
    let mut total = 0;
    let mut argv = read_strings(t, argv_ptr, &mut total)?;
    let envp = read_strings(t, envp_ptr, &mut total)?;
    if argv.is_empty() {
        argv.push(abs.clone());
    }
    let root = t.process.fs.root();
    let (elf_path, image, argv) = crate::exec::resolve(&abs, argv, &root)?;
    // Droit d'exécuter (montage `noexec` : `EACCES`), puis bits set-user-ID / set-group-ID du
    // programme finalement chargé (l'interpréteur d'un script `#!`, comme Linux), ignorés sur
    // un montage `nosuid` ou sous `no_new_privs`.
    let (node, mnt) = super::fs::walk_mnt(t, &abs, true)?;
    if mnt.noexec() {
        return Err(EACCES);
    }
    if let Some(m) = fs_node::FsNode::metadata(&node) {
        if !t.process.creds.lock().may_access(&m, crate::creds::MAY_EXEC, false) {
            return Err(EACCES);
        }
    }
    let (elf_node, elf_mnt) = if elf_path == abs { (node, mnt) } else { super::fs::walk_mnt(t, &elf_path, true)? };
    if elf_mnt.noexec() {
        return Err(EACCES);
    }
    let meta = fs_node::FsNode::metadata(&elf_node);
    let nosuid = elf_mnt.nosuid() || t.process.no_new_privs.load(Ordering::Acquire);
    let prep = match crate::exec::prepare(&elf_path, image, &argv, &envp, &root) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("maiproc : pid {} execve({}) : {}", t.process.pid, elf_path, e);
            return Err(if e.contains("memoire") { ENOMEM } else { ENOEXEC_ });
        }
    };

    // --- Point de non-retour ---
    if de_thread(t).is_err() {
        drop(prep);
        return Ok(Outcome::Exit(t.process.exit_status.load(Ordering::Acquire)));
    }
    let file = meta.map(|m| crate::creds::ExecFile { perm: m.perm, uid: m.uid, gid: m.gid });
    let before = t.process.creds.lock().clone();
    let nnp = t.process.no_new_privs.load(Ordering::Acquire);
    t.process.creds.lock().on_exec(file, nosuid, nnp);
    // Un programme set-ID (ou qui gagne des capacités) perd `PR_SET_PDEATHSIG`.
    if super::sys::clears_pdeath(&before, &t.process.creds.lock()) {
        t.pdeath_signal.store(0, Ordering::Relaxed);
    }
    crate::finish_exec(t, prep, regs);
    Ok(Outcome::Keep)
}

const ENOEXEC_: i64 = 8;

// --- Attente des enfants -------------------------------------------------------------------------

const WNOHANG: u64 = 1;
const WUNTRACED: u64 = 2;
const WEXITED: u64 = 4;
const WCONTINUED: u64 = 8;
const WNOWAIT: u64 = 0x0100_0000;

enum Which {
    Any,
    Pid(usize),
    Pgid(usize),
}

struct Waited {
    /// Numéro de l'enfant vu de l'appelant.
    pid: usize,
    /// uid réel de l'enfant vu de l'appelant (`si_uid` de `waitid`).
    uid: u32,
    /// Statut au format `wait4`.
    status: i32,
    /// `si_code` et `si_status` pour `waitid`.
    code: i32,
    si_status: i32,
    cpu_ns: u64,
}

fn matches(c: &Process, which: &Which) -> bool {
    match which {
        Which::Any => true,
        Which::Pid(p) => c.pid == *p,
        Which::Pgid(g) => c.pgid.load(Ordering::Relaxed) == *g,
    }
}

fn do_wait(t: &Thread, which: Which, options: u64) -> Result<Option<Waited>, i64> {
    let p = &t.process;
    let uns = p.creds.lock().user_ns.clone();
    let uid_of = |c: &Process| uns.from_kuid_munged(c.creds.lock().ruid);
    let r = wait_event(&[&p.child_wait], None, Some(t), || {
        let children = p.children();
        let mut any = false;
        for c in children.iter().filter(|c| matches(c, &which)) {
            any = true;
            if c.is_zombie() && options & WEXITED != 0 {
                let status = c.exit_status.load(Ordering::Acquire);
                let (code, si) = if status & 0x7f == 0 {
                    (signal::CLD_EXITED, (status >> 8) & 0xff)
                } else {
                    (signal::CLD_KILLED, status & 0x7f)
                };
                let w = Waited { pid: p.vnr(c.pid), uid: uid_of(c), status, code, si_status: si, cpu_ns: c.cpu_ns() };
                if options & WNOWAIT == 0 {
                    let z = c.zombie_ns();
                    if z != 0 {
                        crate::sysstats::record_phase(crate::sysstats::PH_REAP, crate::sysstats::now_ns().saturating_sub(z));
                    }
                    crate::reap(c);
                }
                return Some(Ok(Some(w)));
            }
            let mut st = c.stop.lock();
            if options & WUNTRACED != 0 && st.stopped && st.report_stop {
                if options & WNOWAIT == 0 {
                    st.report_stop = false;
                }
                let w = Waited { pid: p.vnr(c.pid), uid: uid_of(c), status: (st.code << 8) | 0x7f, code: signal::CLD_STOPPED, si_status: st.code, cpu_ns: 0 };
                return Some(Ok(Some(w)));
            }
            if options & WCONTINUED != 0 && st.report_cont {
                if options & WNOWAIT == 0 {
                    st.report_cont = false;
                }
                let w = Waited { pid: p.vnr(c.pid), uid: uid_of(c), status: 0xffff, code: signal::CLD_CONTINUED, si_status: signal::SIGCONT, cpu_ns: 0 };
                return Some(Ok(Some(w)));
            }
        }
        if !any {
            return Some(Err(ECHILD_));
        }
        if options & WNOHANG != 0 {
            return Some(Ok(None));
        }
        None
    });
    match r {
        Ok(v) => v,
        Err(WaitError::Interrupted) => Err(signal::ERESTARTSYS),
        Err(WaitError::TimedOut) => Ok(None),
    }
}

fn write_rusage(t: &Thread, ptr: u64, ns: u64) -> Result<(), i64> {
    if ptr == 0 {
        return Ok(());
    }
    let mut ru = [0u64; 18];
    ru[0] = ns / 1_000_000_000;
    ru[1] = (ns % 1_000_000_000) / 1000;
    put_rusage(t, ptr, &ru)
}

/// Écrit une `struct rusage` (18 mots) ; `struct compat_rusage` (`timeval` et `long` de 4
/// octets) pour un appel i386.
fn put_rusage(t: &Thread, ptr: u64, ru: &[u64; 18]) -> Result<(), i64> {
    if super::compat::active(t) {
        let ru32: [u32; 18] = core::array::from_fn(|i| ru[i] as u32);
        return t.mm().write_user(ptr as usize, &ru32).map_err(fault);
    }
    t.mm().write_user(ptr as usize, ru).map_err(fault)
}

/// `wait4(pid, status, options, rusage)`.
pub fn wait4(t: &Thread, pid: i64, status: u64, options: u64, rusage: u64) -> SysResult {
    if options & !(WNOHANG | WUNTRACED | WCONTINUED | 0xE000_0000) != 0 {
        return Err(EINVAL);
    }
    // Numéros de l'appelant → globaux ; un numéro inconnu ne désigne aucun enfant (`ECHILD`).
    let g = |nr: usize| t.process.pid_ns.global_of(nr).unwrap_or(usize::MAX);
    let which = match pid {
        -1 => Which::Any,
        0 => Which::Pgid(t.process.pgid.load(Ordering::Relaxed)),
        p if p < -1 => Which::Pgid(g((-p) as usize)),
        p => Which::Pid(g(p as usize)),
    };
    match do_wait(t, which, options | WEXITED)? {
        None => Ok(0),
        Some(w) => {
            if status != 0 {
                t.mm().write_user::<i32>(status as usize, &w.status).map_err(fault)?;
            }
            write_rusage(t, rusage, w.cpu_ns)?;
            Ok(w.pid as i64)
        }
    }
}

/// `waitid(idtype, id, infop, options, rusage)`.
pub fn waitid(t: &Thread, idtype: u64, id: u64, infop: u64, options: u64, rusage: u64) -> SysResult {
    let g = |nr: u64| t.process.pid_ns.global_of(nr as u32 as usize).unwrap_or(usize::MAX);
    let which = match idtype {
        0 => Which::Any,
        1 => Which::Pid(g(id)),
        2 => Which::Pgid(if id == 0 { t.process.pgid.load(Ordering::Relaxed) } else { g(id) }),
        3 => Which::Pid(crate::pidfd::process_of(t, id)?.pid), // P_PIDFD
        _ => return Err(EINVAL),
    };
    if options & (WEXITED | WUNTRACED | WCONTINUED) == 0 {
        return Err(EINVAL);
    }
    let r = do_wait(t, which, options)?;
    if infop != 0 {
        let info = match &r {
            Some(w) => {
                let mut i = SigInfo::child(w.code, w.pid, w.si_status, w.cpu_ns / 10_000_000);
                i.set_i32(20, w.uid as i32);
                i
            }
            None => SigInfo { bytes: [0; 128] },
        };
        super::compat::put_siginfo(t, infop, &info)?;
    }
    write_rusage(t, rusage, r.as_ref().map_or(0, |w| w.cpu_ns))?;
    Ok(0)
}

// --- Fin -------------------------------------------------------------------------------------

pub fn exit(code: i32) -> Outcome {
    Outcome::Exit(exit_status(code))
}

pub fn exit_group(t: &Thread, code: i32) -> Outcome {
    crate::group_exit(&t.process, exit_status(code), Some(t.tid()));
    Outcome::Exit(t.process.exit_status.load(Ordering::Acquire))
}

// --- Envoi de signaux ----------------------------------------------------------------------------

fn check_sig(sig: i32) -> Result<(), i64> {
    if (0..=64).contains(&sig) { Ok(()) } else { Err(EINVAL) }
}

/// Droit d'envoyer `sig` à `target` (`check_kill_permission`) : même utilisateur (réel ou
/// effectif de l'envoyeur contre réel ou sauvegardé de la cible), `CAP_KILL` dans l'espace
/// utilisateur de la cible, ou `SIGCONT` dans la même session.
pub(crate) fn kill_permitted(me: &Process, target: &Process, sig: i32) -> bool {
    if core::ptr::eq(me, target) {
        return true;
    }
    let c = me.creds.lock().clone();
    let (truid, tsuid, tns) = {
        let tc = target.creds.lock();
        (tc.ruid, tc.suid, tc.user_ns.clone())
    };
    if c.euid == tsuid || c.euid == truid || c.ruid == tsuid || c.ruid == truid {
        return true;
    }
    if crate::userns::ns_capable(&c, &tns, crate::userns::CAP_KILL) {
        return true;
    }
    sig == signal::SIGCONT && me.sid.load(Ordering::Relaxed) == target.sid.load(Ordering::Relaxed)
}

/// Envoi d'un signal de processus à processus, droits vérifiés ; `sig` 0 ne fait que vérifier.
fn send_checked(me: &Process, target: &Arc<Process>, sig: i32, code: i32) -> Result<(), i64> {
    if !kill_permitted(me, target, sig) {
        return Err(EPERM);
    }
    if sig != 0 {
        // Venu d'un espace pid ancêtre : `SIGKILL`/`SIGSTOP` atteignent l'init de la cible.
        let outside = !target.pid_ns.sees(me.pid);
        signal::send_to_process_as(target, SigInfo::from_process(sig, code, me, target), outside);
    }
    Ok(())
}

pub fn kill(t: &Thread, pid: i64, sig: i32) -> SysResult {
    check_sig(sig)?;
    let me = &t.process;
    let ns = &me.pid_ns;
    match pid {
        p if p > 0 => {
            let target = find_process_v(t, p as usize).filter(|q| !q.is_zombie()).ok_or(ESRCH)?;
            send_checked(me, &target, sig, signal::SI_USER).map(|_| 0)
        }
        -1 => {
            // Tous les processus visibles sauf soi et l'init de l'espace (`kill_something_info`).
            let (mut count, mut ret) = (0, Ok(0));
            for p in crate::processes_in(ns) {
                if Arc::ptr_eq(&p, me) || ns.nr_of(p.pid) <= 1 {
                    continue;
                }
                count += 1;
                if let Err(e) = send_checked(me, &p, sig, signal::SI_USER) {
                    if e != EPERM {
                        ret = Err(e);
                    }
                }
            }
            if count == 0 { Err(ESRCH) } else { ret }
        }
        p => {
            // Groupe : réussi si au moins un membre l'a reçu (`__kill_pgrp_info`).
            let pgid = if p == 0 { me.pgid.load(Ordering::Relaxed) } else { ns.global_of((-p) as usize).ok_or(ESRCH)? };
            let (mut success, mut ret) = (false, Err(ESRCH));
            for q in crate::all_processes() {
                if q.pgid.load(Ordering::Relaxed) == pgid {
                    match send_checked(me, &q, sig, signal::SI_USER) {
                        Ok(()) => success = true,
                        Err(e) => ret = Err(e),
                    }
                }
            }
            if success { Ok(0) } else { ret }
        }
    }
}

pub fn tgkill(t: &Thread, tgid: i64, tid: i64, sig: i32) -> SysResult {
    check_sig(sig)?;
    if tid <= 0 || (tgid != -1 && tgid <= 0) {
        return Err(EINVAL);
    }
    let target = find_thread_v(t, tid as usize).ok_or(ESRCH)?;
    if tgid > 0 && t.process.vnr(target.process.pid) != tgid as usize {
        return Err(ESRCH);
    }
    let me = &t.process;
    if !kill_permitted(me, &target.process, sig) {
        return Err(EPERM);
    }
    if sig != 0 {
        let outside = !target.process.pid_ns.sees(me.pid);
        signal::send_to_thread_as(&target, SigInfo::from_process(sig, signal::SI_TKILL, me, &target.process), outside);
    }
    Ok(0)
}

pub fn rt_sigqueueinfo(t: &Thread, tgid: i64, tid: Option<i64>, sig: i32, uinfo: u64) -> SysResult {
    check_sig(sig)?;
    let mut info = SigInfo { bytes: [0; 128] };
    t.mm().copy_from_user(&mut info.bytes, uinfo as usize).map_err(fault)?;
    if super::compat::active(t) {
        // `compat_siginfo` (union à 12) : remis au format 64 bits.
        info = crate::signal32::from_compat(&info.bytes);
    }
    let code = i32::from_le_bytes(info.bytes[8..12].try_into().unwrap());
    // Un code positif (ou SI_TKILL) ferait passer le signal pour un envoi du noyau.
    if (code >= 0 || code == signal::SI_TKILL) && tgid as usize != t.process.vpid {
        return Err(EPERM);
    }
    info.set_i32(0, sig);
    let me = &t.process;
    match tid {
        Some(tid) => {
            let th = find_thread_v(t, tid as usize).ok_or(ESRCH)?;
            if me.vnr(th.process.pid) != tgid as usize {
                return Err(ESRCH);
            }
            if !kill_permitted(me, &th.process, sig) {
                return Err(EPERM);
            }
            if sig != 0 {
                let outside = !th.process.pid_ns.sees(me.pid);
                signal::send_to_thread_as(&th, info, outside);
            }
        }
        None => {
            let p = find_process_v(t, tgid as usize).ok_or(ESRCH)?;
            if !kill_permitted(me, &p, sig) {
                return Err(EPERM);
            }
            if sig != 0 {
                let outside = !p.pid_ns.sees(me.pid);
                signal::send_to_process_as(&p, info, outside);
            }
        }
    }
    Ok(0)
}

// --- Dispositions et masques ----------------------------------------------------------------

pub fn rt_sigaction(t: &Thread, sig: u64, act: u64, oact: u64, size: u64) -> SysResult {
    if size != 8 || sig == 0 || sig > 64 || ((sig as i32 == SIGKILL || sig as i32 == SIGSTOP) && act != 0) {
        return Err(EINVAL);
    }
    let mm = t.mm();
    let compat = super::compat::active(t);
    let new = if act != 0 {
        if compat {
            // `struct compat_sigaction` : gestionnaire, drapeaux, restaurateur (4 octets), masque
            // (8). Le gestionnaire installé par un appel i386 reçoit un cadre i386 (`SA_IA32_ABI`).
            let raw = mm.read_user::<[u32; 5]>(act as usize).map_err(fault)?;
            let mask = raw[3] as u64 | (raw[4] as u64) << 32;
            Some(SigAction { handler: raw[0] as u64, flags: raw[1] as u64 | signal::SA_IA32_ABI, restorer: raw[2] as u64, mask: signal::sanitize_mask(mask) })
        } else {
            let raw = mm.read_user::<[u64; 4]>(act as usize).map_err(fault)?;
            Some(SigAction { handler: raw[0], flags: raw[1] & !signal::SA_IA32_ABI, restorer: raw[2], mask: signal::sanitize_mask(raw[3]) })
        }
    } else {
        None
    };
    let sh = t.process.sighand();
    let old = {
        let mut table = sh.actions.lock();
        let old = table[sig as usize - 1];
        if let Some(n) = new {
            table[sig as usize - 1] = n;
        }
        old
    };
    // POSIX : un signal qui devient ignoré est retiré des files.
    if let Some(n) = new {
        let s = sig as i32;
        let now_ignored = n.handler == signal::SIG_IGN || (n.handler == signal::SIG_DFL && matches!(s, 17 | 23 | 28));
        if now_ignored {
            t.process.discard_pending(sigbit(s));
        }
    }
    if oact != 0 {
        let flags = old.flags & !signal::SA_IA32_ABI;
        if compat {
            let raw = [old.handler as u32, flags as u32, old.restorer as u32, old.mask as u32, (old.mask >> 32) as u32];
            mm.write_user(oact as usize, &raw).map_err(fault)?;
        } else {
            mm.write_user(oact as usize, &[old.handler, flags, old.restorer, old.mask]).map_err(fault)?;
        }
    }
    Ok(0)
}

pub fn rt_sigprocmask(t: &Thread, how: u64, set: u64, oset: u64, size: u64) -> SysResult {
    if size != 8 {
        return Err(EINVAL);
    }
    let mm = t.mm();
    let old = t.sigmask.load(Ordering::Acquire);
    if set != 0 {
        let s = mm.read_user::<u64>(set as usize).map_err(fault)?;
        let new = match how {
            0 => old | s,
            1 => old & !s,
            2 => s,
            _ => return Err(EINVAL),
        };
        t.sigmask.store(signal::sanitize_mask(new), Ordering::Release);
        signal::recalc(t);
    }
    if oset != 0 {
        mm.write_user(oset as usize, &old).map_err(fault)?;
    }
    Ok(0)
}

pub fn rt_sigpending(t: &Thread, set: u64, size: u64) -> SysResult {
    if size > 8 {
        return Err(EINVAL);
    }
    let pending = (t.pending.lock().mask | t.process.shared_pending.lock().mask) & t.sigmask.load(Ordering::Acquire);
    t.mm().write_user::<u64>(set as usize, &pending).map_err(fault)?;
    Ok(0)
}

/// Attend un signal livrable (`sigsuspend`, `pause`) : toujours interrompu.
fn wait_for_signal(t: &Thread) -> SysResult {
    let _ = wait_event::<()>(&[], None, Some(t), || None);
    Err(signal::ERESTARTNOHAND)
}

pub fn rt_sigsuspend(t: &Thread, mask: u64, size: u64) -> SysResult {
    if size != 8 {
        return Err(EINVAL);
    }
    let m = t.mm().read_user::<u64>(mask as usize).map_err(fault)?;
    let old = t.sigmask.swap(signal::sanitize_mask(m), Ordering::AcqRel);
    *t.saved_sigmask.lock() = Some(old);
    signal::recalc(t);
    wait_for_signal(t)
}

pub fn pause(t: &Thread) -> SysResult {
    wait_for_signal(t)
}

/// `rt_sigtimedwait(set, info, timeout, size)`.
pub fn rt_sigtimedwait(t: &Thread, set: u64, info: u64, timeout: u64, size: u64) -> SysResult {
    if size != 8 {
        return Err(EINVAL);
    }
    let s = signal::sanitize_mask(t.mm().read_user::<u64>(set as usize).map_err(fault)?);
    let deadline = super::futex_deadline(t, timeout, false)?;
    // Les signaux attendus sont débloqués le temps de l'attente : ils visent alors ce fil.
    let old = t.sigmask.fetch_and(!s, Ordering::AcqRel);
    let r = wait_event(&[], deadline, Some(t), || signal::dequeue_from(t, s));
    t.sigmask.store(old, Ordering::Release);
    signal::recalc(t);
    match r {
        Ok(i) => {
            if info != 0 {
                super::compat::put_siginfo(t, info, &i)?;
            }
            Ok(i.signo() as i64)
        }
        Err(WaitError::TimedOut) => Err(EAGAIN),
        Err(WaitError::Interrupted) => Err(EINTR),
    }
}

/// `sigaltstack(ss, old_ss)`.
pub fn sigaltstack(t: &Thread, regs: &UserRegs, ss: u64, old: u64) -> SysResult {
    const SS_ONSTACK: i32 = 1;
    const SS_DISABLE: i32 = 2;
    const SS_AUTODISARM: i32 = 1 << 31;
    const MINSIGSTKSZ: u64 = 2048;
    let mm = t.mm();
    let cur = *t.altstack.lock();
    // `stack_t` : { sp, flags, size } ; 12 octets pour un appel i386 (`compat_stack_t`).
    let compat = super::compat::active(t);
    if old != 0 {
        if compat {
            let raw = [cur.sp as u32, cur.user_flags(regs.rsp) as u32, cur.size as u32];
            mm.write_user(old as usize, &raw).map_err(fault)?;
        } else {
            let mut b = [0u8; 24];
            b[0..8].copy_from_slice(&cur.sp.to_le_bytes());
            b[8..12].copy_from_slice(&cur.user_flags(regs.rsp).to_le_bytes());
            b[16..24].copy_from_slice(&cur.size.to_le_bytes());
            mm.copy_to_user(old as usize, &b).map_err(fault)?;
        }
    }
    if ss != 0 {
        if cur.contains(regs.rsp) {
            return Err(EPERM);
        }
        let (sp, flags, size) = if compat {
            let raw = mm.read_user::<[u32; 3]>(ss as usize).map_err(fault)?;
            (raw[0] as u64, raw[1] as i32, raw[2] as u64)
        } else {
            let mut b = [0u8; 24];
            mm.copy_from_user(&mut b, ss as usize).map_err(fault)?;
            (
                u64::from_le_bytes(b[0..8].try_into().unwrap()),
                i32::from_le_bytes(b[8..12].try_into().unwrap()),
                u64::from_le_bytes(b[16..24].try_into().unwrap()),
            )
        };
        let new = if flags & SS_DISABLE != 0 {
            signal::AltStack::disabled()
        } else {
            if flags & !(SS_ONSTACK | SS_AUTODISARM) != 0 {
                return Err(EINVAL);
            }
            if size < MINSIGSTKSZ {
                return Err(ENOMEM);
            }
            signal::AltStack { sp, size, flags: 0 }
        };
        *t.altstack.lock() = new;
    }
    Ok(0)
}

pub fn rt_sigreturn(t: &Thread, regs: &mut UserRegs) -> Outcome {
    let Some(me) = crate::current_thread_arc(t) else { return Outcome::Return(-EINVAL) };
    if signal::sigreturn(&me, regs).is_err() {
        log::warn!("maiproc : pid {} rt_sigreturn : cadre invalide -> SIGSEGV", t.process.pid);
        signal::send_to_thread(&me, SigInfo::new(signal::SIGSEGV, signal::SI_KERNEL), true);
    }
    drop(me);
    Outcome::Keep
}

// --- Groupes, sessions ---------------------------------------------------------------------------

fn process_or_self(t: &Thread, pid: u64) -> Result<Arc<Process>, i64> {
    if pid == 0 || pid as usize == t.process.vpid {
        return Ok(t.process.clone());
    }
    find_process_v(t, pid as u32 as usize).filter(|p| !p.is_zombie()).ok_or(ESRCH)
}

pub fn setpgid(t: &Thread, pid: u64, pgid: u64) -> SysResult {
    if (pgid as i64) < 0 {
        return Err(EINVAL);
    }
    let target = process_or_self(t, pid)?;
    let me = &t.process;
    let is_child = target.parent.lock().upgrade().map_or(false, |p| Arc::ptr_eq(&p, me));
    if !Arc::ptr_eq(&target, me) && !is_child {
        return Err(ESRCH);
    }
    let sid = me.sid.load(Ordering::Relaxed);
    if target.sid.load(Ordering::Relaxed) != sid {
        return Err(EPERM);
    }
    if target.sid.load(Ordering::Relaxed) == target.pid {
        return Err(EPERM); // chef de session
    }
    // Nouveau groupe : celui du processus lui-même, ou un groupe existant de la même session
    // (on s'attache alors à son numéro).
    let group = if pgid == 0 || me.pid_ns.global_of(pgid as usize) == Some(target.pid) {
        target.pid_ref().ok_or(ESRCH)?
    } else {
        let g = me.pid_ns.global_of(pgid as usize).ok_or(EPERM)?;
        crate::all_processes()
            .into_iter()
            .find(|q| q.pgid.load(Ordering::Relaxed) == g && q.sid.load(Ordering::Relaxed) == sid)
            .and_then(|q| q.pgrp_ref())
            .ok_or(EPERM)?
    };
    target.set_pgrp(group);
    Ok(0)
}

pub fn getpgid(t: &Thread, pid: u64) -> SysResult {
    let p = process_or_self(t, pid)?;
    Ok(t.process.vnr(p.pgid.load(Ordering::Relaxed)) as i64)
}

pub fn setsid(t: &Thread) -> SysResult {
    let p = &t.process;
    // Refusé au chef d'un groupe, et si un groupe porte déjà ce numéro (`ksys_setsid`).
    if p.pgid.load(Ordering::Relaxed) == p.pid || crate::all_processes().iter().any(|q| q.pgid.load(Ordering::Relaxed) == p.pid) {
        return Err(EPERM);
    }
    let me = p.pid_ref().ok_or(EPERM)?;
    p.set_session(me.clone());
    p.set_pgrp(me);
    *p.ctty.lock() = None;
    Ok(p.vpid as i64)
}

pub fn getsid(t: &Thread, pid: u64) -> SysResult {
    let p = process_or_self(t, pid)?;
    Ok(t.process.vnr(p.sid.load(Ordering::Relaxed)) as i64)
}

// --- Minuteur réel (alarm, setitimer) -------------------------------------------------------------

struct AlarmWaker {
    process: Weak<Process>,
    gen: u64,
}

impl alloc::task::Wake for AlarmWaker {
    fn wake(self: Arc<Self>) {
        fire(&self);
    }
}

fn arm(p: &Arc<Process>, gen: u64, after: core::time::Duration) {
    let w = Arc::new(AlarmWaker { process: Arc::downgrade(p), gen });
    sleep::future::sleep(after, core::task::Waker::from(w));
}

/// Échéance du minuteur (appelé depuis l'interruption du timer).
fn fire(w: &AlarmWaker) {
    let Some(p) = w.process.upgrade() else { return };
    if p.is_zombie() {
        return;
    }
    let rearm = {
        let mut rt = p.real_timer.lock();
        if rt.gen != w.gen || rt.deadline.is_none() {
            return;
        }
        if rt.interval.is_zero() {
            rt.deadline = None;
            None
        } else {
            rt.deadline = rt.deadline.map(|d| d + rt.interval);
            Some(rt.interval)
        }
    };
    signal::send_to_process(&p, SigInfo::new(signal::SIGALRM, signal::SI_KERNEL));
    if let Some(iv) = rearm {
        arm(&p, w.gen, iv);
    }
}

/// Reste avant l'échéance et intervalle du minuteur réel.
fn real_timer_state(p: &Process) -> (core::time::Duration, core::time::Duration) {
    let rt = p.real_timer.lock();
    let left = rt.deadline.map_or(core::time::Duration::ZERO, |d| {
        let now = time::Instant::now();
        if d > now { d - now } else { core::time::Duration::from_micros(1) }
    });
    (left, rt.interval)
}

fn set_real_timer(p: &Arc<Process>, value: core::time::Duration, interval: core::time::Duration) {
    let gen = {
        let mut rt = p.real_timer.lock();
        rt.gen += 1;
        rt.interval = interval;
        rt.deadline = if value.is_zero() { None } else { Some(time::Instant::now() + value) };
        rt.gen
    };
    if !value.is_zero() {
        arm(p, gen, value);
    }
}

pub fn alarm(t: &Thread, secs: u64) -> SysResult {
    let (left, _) = real_timer_state(&t.process);
    set_real_timer(&t.process, core::time::Duration::from_secs(secs), core::time::Duration::ZERO);
    let mut s = left.as_secs();
    if left.subsec_nanos() >= 500_000_000 || (s == 0 && !left.is_zero()) {
        s += 1;
    }
    Ok(s as i64)
}

fn timeval(d: core::time::Duration) -> [i64; 2] {
    [d.as_secs() as i64, d.subsec_micros() as i64]
}

/// `timeval` lu (32 bits pour `setitimer` i386, `compat_itimerval`).
fn read_timeval(t: &Thread, p: u64) -> Result<core::time::Duration, i64> {
    let v = super::compat::get_timespec(t, p)?;
    if v[0] < 0 || !(0..1_000_000).contains(&v[1]) {
        return Err(EINVAL);
    }
    Ok(core::time::Duration::new(v[0] as u64, v[1] as u32 * 1000))
}

pub fn getitimer(t: &Thread, which: u64, cur: u64) -> SysResult {
    let v = match which {
        0 => {
            let (left, iv) = real_timer_state(&t.process);
            [timeval(iv), timeval(left)]
        }
        1 | 2 => {
            let (left, iv) = crate::cputimer::get(&t.process, which as usize - 1);
            [timeval(core::time::Duration::from_nanos(iv)), timeval(core::time::Duration::from_nanos(left))]
        }
        _ => return Err(EINVAL),
    };
    super::compat::put_itimerspec(t, cur, [v[0][0], v[0][1], v[1][0], v[1][1]])?;
    Ok(0)
}

pub fn setitimer(t: &Thread, which: u64, new: u64, old: u64) -> SysResult {
    if which > 2 {
        return Err(EINVAL);
    }
    if old != 0 {
        getitimer(t, which, old)?;
    }
    if new != 0 {
        let interval = read_timeval(t, new)?;
        let value = read_timeval(t, new + super::compat::timespec_size(t))?;
        if which == 0 {
            set_real_timer(&t.process, value, interval);
        } else {
            crate::cputimer::set(&t.process, which as usize - 1, value.as_nanos() as u64, interval.as_nanos() as u64);
        }
    }
    Ok(0)
}

// --- Temps processeur ----------------------------------------------------------------------------

pub fn getrusage(t: &Thread, who: i64, usage: u64) -> SysResult {
    let ns = match who {
        0 => t.process.cpu_ns(),
        -1 => t.process.children_ns.load(Ordering::Relaxed),
        1 => t.cpu_ns(),
        _ => return Err(EINVAL),
    };
    let mut ru = [0u64; 18];
    ru[0] = ns / 1_000_000_000;
    ru[1] = (ns % 1_000_000_000) / 1000;
    if who != -1 {
        ru[4] = (t.mm().rss() * 4) as u64; // ru_maxrss (Kio), approché par la RSS
    }
    put_rusage(t, usage, &ru)?;
    Ok(0)
}

pub fn times(t: &Thread, buf: u64) -> SysResult {
    let ticks = |ns: u64| ns / 10_000_000; // 100 Hz
    if buf != 0 {
        let tms: [u64; 4] = [ticks(t.process.cpu_ns()), 0, ticks(t.process.children_ns.load(Ordering::Relaxed)), 0];
        if super::compat::active(t) {
            // `struct compat_tms` : `compat_clock_t` de 4 octets.
            let tms32: [u32; 4] = core::array::from_fn(|i| tms[i] as u32);
            t.mm().write_user(buf as usize, &tms32).map_err(fault)?;
        } else {
            t.mm().write_user(buf as usize, &tms).map_err(fault)?;
        }
    }
    Ok(ticks(time::Instant::now().duration_since(time::Instant::ZERO).as_nanos() as u64) as i64)
}
