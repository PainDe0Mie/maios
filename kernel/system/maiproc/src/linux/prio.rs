//! Priorités : `getpriority`/`setpriority` (nice, appliqué à MaiSched) et
//! `ioprio_get`/`ioprio_set` (priorité d'entrées-sorties), avec les règles de Linux
//! (`kernel/sys.c`, `block/ioprio.c`).
//!
//! Le nice est propre à chaque fil (`PRIO_PROCESS` désigne un identifiant de fil) ; `PRIO_PGRP`
//! touche tous les fils d'un groupe, `PRIO_USER` tous ceux d'un utilisateur (identité réelle).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use super::errno::*;
use super::SysResult;
use crate::{Process, Thread};

const PRIO_PROCESS: u64 = 0;
const PRIO_PGRP: u64 = 1;
const PRIO_USER: u64 = 2;

const IOPRIO_WHO_PROCESS: u64 = 1;
const IOPRIO_WHO_PGRP: u64 = 2;
const IOPRIO_WHO_USER: u64 = 3;
const IOPRIO_CLASS_SHIFT: u32 = 13;
const IOPRIO_CLASS_NONE: u32 = 0;
const IOPRIO_CLASS_RT: u32 = 1;
const IOPRIO_CLASS_BE: u32 = 2;
const IOPRIO_CLASS_IDLE: u32 = 3;
const IOPRIO_NR_LEVELS: u32 = 8;

/// Fils désignés par `(which, who)` : `who` = 0 désigne l'appelant (son fil, son groupe, son
/// identité réelle). `None` : `which` inconnu.
fn targets(t: &Thread, which: u64, who: u64, process: u64, pgrp: u64, user: u64) -> Option<Vec<Arc<Thread>>> {
    let who = who as u32 as usize;
    let all_threads = |pred: &dyn Fn(&Process) -> bool| {
        let mut v = Vec::new();
        // Seulement les processus visibles dans l'espace pid de l'appelant.
        for p in crate::processes_in(&t.process.pid_ns).into_iter().filter(|p| !p.is_zombie() && pred(p)) {
            p.for_each_thread_until(|th| {
                v.push(th.clone());
                false
            });
        }
        v
    };
    if which == process {
        let th = if who == 0 { crate::current_thread_arc(t) } else { crate::find_thread_v(t, who) };
        Some(th.filter(|th| !th.process.is_zombie()).into_iter().collect())
    } else if which == pgrp {
        let g = if who == 0 { t.process.pgid.load(Ordering::Acquire) } else { t.process.pid_ns.global_of(who).unwrap_or(usize::MAX) };
        Some(all_threads(&|p| p.pgid.load(Ordering::Acquire) == g))
    } else if which == user {
        // uid de l'espace utilisateur de l'appelant → identité globale.
        let u = if who == 0 {
            t.process.creds.lock().ruid
        } else {
            let ns = t.process.creds.lock().user_ns.clone();
            ns.make_kuid(who as u32)?
        };
        Some(all_threads(&|p| p.creds.lock().ruid == u))
    } else {
        None
    }
}

fn nice_of(th: &Thread) -> i32 {
    th.task().map_or(0, |task| task.sched.nice.load(Ordering::Relaxed) as i32)
}

/// L'appelant peut-il changer la priorité de `th` ? (Linux `set_one_prio_perm` /
/// `set_task_ioprio` : root, ou identité réelle ou effective de l'appelant = identité réelle
/// (ou effective, pour le nice) de la cible.)
fn may_change(t: &Thread, th: &Thread, nice: bool) -> bool {
    let (me_r, me_e, me) = {
        let c = t.process.creds.lock();
        (c.ruid, c.euid, c.clone())
    };
    let (r, e, other_ns) = {
        let c = th.process.creds.lock();
        (c.ruid, c.euid, c.user_ns.clone())
    };
    if crate::userns::ns_capable(&me, &other_ns, crate::userns::CAP_SYS_NICE) {
        return true;
    }
    if nice { me_e == r || me_e == e } else { r == me_e || r == me_r }
}

/// `getpriority(which, who)` : `20 − nice` du fil le plus prioritaire (1 à 40, jamais négatif :
/// la glibc retranche 20).
pub fn getpriority(t: &Thread, which: u64, who: u64) -> SysResult {
    let list = targets(t, which, who, PRIO_PROCESS, PRIO_PGRP, PRIO_USER).ok_or(EINVAL)?;
    list.iter().map(|th| 20 - nice_of(th)).max().map(|v| v as i64).ok_or(ESRCH)
}

/// `setpriority(which, who, nice)` : nice borné à [−20, 19] ; baisser le nice (monter en
/// priorité) demande root (`EACCES`), toucher un fil d'un autre utilisateur aussi (`EPERM`).
pub fn setpriority(t: &Thread, which: u64, who: u64, nice: u64) -> SysResult {
    let nice = (nice as i32).clamp(-20, 19);
    let list = targets(t, which, who, PRIO_PROCESS, PRIO_PGRP, PRIO_USER).ok_or(EINVAL)?;
    if list.is_empty() {
        return Err(ESRCH);
    }
    let root = crate::userns::current_capable(t, crate::userns::CAP_SYS_NICE);
    let mut err = None;
    for th in &list {
        if !may_change(t, th, true) {
            err = Some(EPERM);
            continue;
        }
        if nice < nice_of(th) && !root {
            err = Some(EACCES);
            continue;
        }
        if let Some(task) = th.task() {
            task::scheduler::set_priority(&task, (nice + 20) as u8);
        }
    }
    err.map_or(Ok(0), Err)
}

/// Priorité d'E/S d'un fil telle que `ioprio_get` la rend : sans priorité posée, 0
/// (`IOPRIO_CLASS_NONE` : l'ordonnanceur d'E/S la déduit du nice), comme Linux 6 (mesuré sur
/// 6.18).
fn ioprio_of(th: &Thread) -> u32 {
    th.ioprio.load(Ordering::Relaxed)
}

/// `ioprio_get(which, who)` : pour un groupe ou un utilisateur, la meilleure (la plus petite).
pub fn ioprio_get(t: &Thread, which: u64, who: u64) -> SysResult {
    let list = targets(t, which, who, IOPRIO_WHO_PROCESS, IOPRIO_WHO_PGRP, IOPRIO_WHO_USER).ok_or(EINVAL)?;
    list.iter().map(|th| ioprio_of(th)).min().map(|v| v as i64).ok_or(ESRCH)
}

/// `ioprio_set(which, who, ioprio)` : classes temps réel (root), au mieux (niveaux 0 à 7),
/// inactive, ou aucune (niveau 0 : revient à la priorité déduite du nice).
pub fn ioprio_set(t: &Thread, which: u64, who: u64, ioprio: u64) -> SysResult {
    let ioprio = ioprio as u32;
    let class = ioprio >> IOPRIO_CLASS_SHIFT & 7;
    let level = ioprio & (IOPRIO_NR_LEVELS - 1);
    match class {
        IOPRIO_CLASS_RT => {
            if !crate::userns::current_capable(t, crate::userns::CAP_SYS_NICE) {
                return Err(EPERM);
            }
        }
        IOPRIO_CLASS_BE | IOPRIO_CLASS_IDLE => {}
        IOPRIO_CLASS_NONE if level == 0 => {}
        _ => return Err(EINVAL),
    }
    let list = targets(t, which, who, IOPRIO_WHO_PROCESS, IOPRIO_WHO_PGRP, IOPRIO_WHO_USER).ok_or(EINVAL)?;
    if list.is_empty() {
        return Err(ESRCH);
    }
    let mut err = None;
    for th in &list {
        if !may_change(t, th, false) {
            err = Some(EPERM);
            continue;
        }
        th.ioprio.store(ioprio & 0xffff, Ordering::Relaxed);
    }
    err.map_or(Ok(0), Err)
}
