//! Appels système de processus, signaux, temps et informations système.

use super::errno::*;
use super::{fault, Restart, SysResult};
use crate::wait::{wait_event, WaitError};
use crate::Thread;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use log::warn;
use uentry::UserRegs;

/// Version annoncée par `uname` (les libc lisent la version du noyau Linux attendue).
pub const RELEASE: &str = "6.12.0-maios";

// --- Identité, limites, contrôle -----------------------------------------------------------------

/// Appels d'identité : `get*id`, `set*id`, `setfs*id`, `getres*id`, `getgroups`, `setgroups`
/// (règles dans `crate::creds`).
/// Un changement d'identité efface `PR_SET_PDEATHSIG` du fil (`commit_creds`) : uid ou gid
/// effectif ou de système de fichiers différent, ou capacités qui ne sont plus un
/// sous-ensemble des anciennes.
pub(crate) fn clears_pdeath(old: &crate::creds::Creds, new: &crate::creds::Creds) -> bool {
    old.euid != new.euid
        || old.egid != new.egid
        || old.fsuid != new.fsuid
        || old.fsgid != new.fsgid
        || new.cap_prm & !old.cap_prm != 0
        || !alloc::sync::Arc::ptr_eq(&old.user_ns, &new.user_ns)
}

pub fn identity(t: &Thread, nr: u64, a0: u64, a1: u64, a2: u64) -> SysResult {
    let before = t.process.creds.lock().clone();
    let r = identity_inner(t, nr, a0, a1, a2);
    if clears_pdeath(&before, &t.process.creds.lock()) {
        t.pdeath_signal.store(0, Ordering::Relaxed);
    }
    r
}

fn identity_inner(t: &Thread, nr: u64, a0: u64, a1: u64, a2: u64) -> SysResult {
    use crate::creds::KEEP;
    let (x, y, z) = (a0 as u32, a1 as u32, a2 as u32);
    let creds = &t.process.creds;
    let done = |r: Result<(), i64>| r.map(|_| 0);
    // Identités de l'espace utilisateur de l'appelant ↔ identités globales du noyau.
    let ns = creds.lock().user_ns.clone();
    let up = |k: u32| ns.from_kuid_munged(k) as i64;
    let upg = |k: u32| ns.from_kgid_munged(k) as i64;
    let du = |v: u32| -> Result<u32, i64> { if v == KEEP { Ok(KEEP) } else { ns.make_kuid(v).ok_or(EINVAL) } };
    let dg = |v: u32| -> Result<u32, i64> { if v == KEEP { Ok(KEEP) } else { ns.make_kgid(v).ok_or(EINVAL) } };
    match nr {
        102 => Ok(up(creds.lock().ruid)),
        104 => Ok(upg(creds.lock().rgid)),
        107 => Ok(up(creds.lock().euid)),
        108 => Ok(upg(creds.lock().egid)),
        105 => done(creds.lock().setuid(if x == KEEP { KEEP } else { du(x)? })),
        106 => done(creds.lock().setgid(if x == KEEP { KEEP } else { dg(x)? })),
        113 => done(creds.lock().setreuid(du(x)?, du(y)?)),
        114 => done(creds.lock().setregid(dg(x)?, dg(y)?)),
        117 => done(creds.lock().setresuid(du(x)?, du(y)?, du(z)?)),
        119 => done(creds.lock().setresgid(dg(x)?, dg(y)?, dg(z)?)),
        // `setfsuid` rend toujours l'ancien ; une identité sans projection ne change rien.
        122 => Ok(up(creds.lock().setfsuid(ns.make_kuid(x).unwrap_or(KEEP)))),
        123 => Ok(upg(creds.lock().setfsgid(ns.make_kgid(x).unwrap_or(KEEP)))),
        118 | 120 => {
            let c = creds.lock().clone();
            let v = if nr == 118 { [c.ruid, c.euid, c.suid].map(|k| up(k) as u32) } else { [c.rgid, c.egid, c.sgid].map(|k| upg(k) as u32) };
            for (p, v) in [a0, a1, a2].into_iter().zip(v) {
                t.mm().write_user::<u32>(p as usize, &v).map_err(fault)?;
            }
            Ok(0)
        }
        115 => {
            let groups: alloc::vec::Vec<u32> = creds.lock().groups.iter().map(|&g| upg(g) as u32).collect();
            let size = a0 as i32;
            if size < 0 {
                return Err(EINVAL);
            }
            if size == 0 {
                return Ok(groups.len() as i64);
            }
            if (size as usize) < groups.len() {
                return Err(EINVAL);
            }
            let raw: alloc::vec::Vec<u8> = groups.iter().flat_map(|g| g.to_le_bytes()).collect();
            t.mm().copy_to_user(a1 as usize, &raw).map_err(fault)?;
            Ok(groups.len() as i64)
        }
        116 => {
            let n = a0 as i32;
            if n < 0 || n as usize > crate::creds::NGROUPS_MAX {
                return Err(EINVAL);
            }
            let mut raw = alloc::vec![0u8; n as usize * 4];
            if n > 0 {
                t.mm().copy_from_user(&mut raw, a1 as usize).map_err(fault)?;
            }
            let groups = raw.chunks(4).map(|c| ns.make_kgid(u32::from_le_bytes([c[0], c[1], c[2], c[3]])).ok_or(EINVAL)).collect::<Result<alloc::vec::Vec<u32>, i64>>()?;
            done(creds.lock().setgroups(groups))
        }
        _ => Err(ENOSYS),
    }
}

pub fn prctl(t: &Thread, option: u64, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> SysResult {
    use crate::userns::{bit, cap_valid, CAP_SETPCAP};
    const PR_GET_KEEPCAPS: u64 = 7;
    const PR_SET_KEEPCAPS: u64 = 8;
    const PR_CAPBSET_DROP: u64 = 24;
    const PR_GET_SECUREBITS: u64 = 27;
    const PR_SET_SECUREBITS: u64 = 28;
    const PR_CAP_AMBIENT: u64 = 47;
    // Capacités, `no_new_privs`, bits de sécurité (`cap_task_prctl`).
    let creds = &t.process.creds;
    match option {
        PR_GET_KEEPCAPS => return Ok((creds.lock().securebits >> crate::creds::SECURE_KEEP_CAPS & 1) as i64),
        PR_SET_KEEPCAPS => {
            if arg2 > 1 || arg3 | arg4 | arg5 != 0 {
                return Err(EINVAL);
            }
            let mut c = creds.lock();
            if c.securebits & (1 << 5) != 0 {
                return Err(EPERM); // SECURE_KEEP_CAPS_LOCKED
            }
            if arg2 == 1 {
                c.securebits |= 1 << crate::creds::SECURE_KEEP_CAPS;
            } else {
                c.securebits &= !(1 << crate::creds::SECURE_KEEP_CAPS);
            }
            return Ok(0);
        }
        PR_GET_SECUREBITS => return Ok(creds.lock().securebits as i64),
        PR_SET_SECUREBITS => {
            let mut c = creds.lock();
            // Bits verrouillés (impairs) : on ne revient pas en arrière.
            let locked = c.securebits & 0x2a;
            let changed = (c.securebits ^ arg2 as u32) & 0x3f;
            if arg2 >> 6 != 0 || changed & (locked >> 1) != 0 || (c.securebits & !(arg2 as u32)) & locked != 0 || c.cap_eff & bit(CAP_SETPCAP) == 0 {
                return Err(EPERM);
            }
            c.securebits = arg2 as u32;
            return Ok(0);
        }
        23 /* PR_CAPBSET_READ */ => {
            if !cap_valid(arg2 as u32) {
                return Err(EINVAL);
            }
            return Ok((creds.lock().cap_bnd & bit(arg2 as u32) != 0) as i64);
        }
        PR_CAPBSET_DROP => {
            if !cap_valid(arg2 as u32) {
                return Err(EINVAL);
            }
            let mut c = creds.lock();
            if c.cap_eff & bit(CAP_SETPCAP) == 0 {
                return Err(EPERM);
            }
            c.cap_bnd &= !bit(arg2 as u32);
            return Ok(0);
        }
        PR_CAP_AMBIENT => {
            const IS_SET: u64 = 1;
            const RAISE: u64 = 2;
            const LOWER: u64 = 3;
            const CLEAR_ALL: u64 = 4;
            let mut c = creds.lock();
            if arg2 == CLEAR_ALL {
                if arg3 | arg4 | arg5 != 0 {
                    return Err(EINVAL);
                }
                c.cap_amb = 0;
                return Ok(0);
            }
            if arg4 | arg5 != 0 || !cap_valid(arg3 as u32) {
                return Err(EINVAL);
            }
            let b = bit(arg3 as u32);
            return match arg2 {
                IS_SET => Ok((c.cap_amb & b != 0) as i64),
                RAISE => {
                    // Permise et héritable, et `SECBIT_NO_CAP_AMBIENT_RAISE` absent.
                    if c.cap_prm & b == 0 || c.cap_inh & b == 0 || c.securebits & (1 << 6) != 0 {
                        return Err(EPERM);
                    }
                    c.cap_amb |= b;
                    Ok(0)
                }
                LOWER => {
                    c.cap_amb &= !b;
                    Ok(0)
                }
                _ => Err(EINVAL),
            };
        }
        38 /* PR_SET_NO_NEW_PRIVS */ => {
            if arg2 != 1 || arg3 | arg4 | arg5 != 0 {
                return Err(EINVAL);
            }
            t.process.no_new_privs.store(true, Ordering::Release);
            return Ok(0);
        }
        39 /* PR_GET_NO_NEW_PRIVS */ => {
            if arg2 | arg3 | arg4 | arg5 != 0 {
                return Err(EINVAL);
            }
            return Ok(t.process.no_new_privs.load(Ordering::Acquire) as i64);
        }
        _ => {}
    }
    const PR_SET_PDEATHSIG: u64 = 1;
    const PR_GET_PDEATHSIG: u64 = 2;
    const PR_SET_CHILD_SUBREAPER: u64 = 36;
    const PR_GET_CHILD_SUBREAPER: u64 = 37;
    const PR_GET_DUMPABLE: u64 = 3;
    const PR_SET_DUMPABLE: u64 = 4;
    const PR_SET_NAME: u64 = 15;
    const PR_GET_NAME: u64 = 16;
    const PR_SET_VMA: u64 = 0x5356_4d41;
    let mm = t.mm();
    match option {
        PR_SET_NAME => {
            let mut raw = [0u8; 16];
            let name = mm.read_cstr(arg2 as usize, 16).or_else(|_| {
                mm.copy_from_user(&mut raw[..15], arg2 as usize).map(|_| raw[..15].to_vec())
            }).map_err(fault)?;
            let mut comm = [0u8; 16];
            for (d, s) in comm.iter_mut().zip(name.iter().take(15)) {
                *d = *s;
            }
            // Propre au fil (Firefox nomme ses fils « Socket Thread »…) ; celui du fil chef
            // nomme aussi le processus (`/proc/<pid>/comm`, `ps`).
            *t.comm.lock() = comm;
            if t.tid() == t.process.pid {
                *t.process.comm.lock() = comm;
            }
            Ok(0)
        }
        PR_GET_NAME => {
            let comm = *t.comm.lock();
            mm.copy_to_user(arg2 as usize, &comm).map_err(fault)?;
            Ok(0)
        }
        PR_GET_DUMPABLE => Ok(1),
        // Signal reçu par ce fil à la fin de son parent (`lib::forget_original_parent`).
        PR_SET_PDEATHSIG => {
            if arg2 > 64 {
                return Err(EINVAL);
            }
            t.pdeath_signal.store(arg2 as i32, Ordering::Relaxed);
            Ok(0)
        }
        PR_GET_PDEATHSIG => {
            mm.write_user::<i32>(arg2 as usize, &t.pdeath_signal.load(Ordering::Relaxed)).map_err(fault)?;
            Ok(0)
        }
        // *Subreaper* : les orphelins de la descendance lui reviennent au lieu de l'init ; toute
        // la descendance actuelle le sait (`propagate_has_child_subreaper`).
        PR_SET_CHILD_SUBREAPER => {
            let p = &t.process;
            p.child_subreaper.store(arg2 != 0, Ordering::Relaxed);
            if arg2 != 0 {
                let mut stack = p.children();
                while let Some(c) = stack.pop() {
                    if c.has_child_subreaper.swap(true, Ordering::Relaxed) {
                        continue; // déjà propagé sous lui
                    }
                    stack.extend(c.children());
                }
            }
            Ok(0)
        }
        PR_GET_CHILD_SUBREAPER => {
            let v = t.process.child_subreaper.load(Ordering::Relaxed) as i32;
            mm.write_user::<i32>(arg2 as usize, &v).map_err(fault)?;
            Ok(0)
        }
        PR_SET_DUMPABLE | PR_SET_VMA => Ok(0),
        _ => Err(EINVAL),
    }
}

pub fn arch_prctl(t: &Thread, regs: &mut UserRegs, code: u64, addr: u64) -> SysResult {
    const ARCH_SET_GS: u64 = 0x1001;
    const ARCH_SET_FS: u64 = 0x1002;
    const ARCH_GET_FS: u64 = 0x1003;
    const ARCH_GET_GS: u64 = 0x1004;
    const ARCH_GET_CPUID: u64 = 0x1011;
    const ARCH_SET_CPUID: u64 = 0x1012;
    // Une base non canonique ferait fauter `wrfsbase` au retour : refusée ici.
    let canonical = addr < kernel_config::memory::USER_SPACE_END as u64;
    match code {
        ARCH_SET_FS => {
            if !canonical {
                return Err(EPERM);
            }
            regs.fs_base = addr;
            Ok(0)
        }
        ARCH_SET_GS => {
            if !canonical {
                return Err(EPERM);
            }
            regs.gs_base = addr;
            Ok(0)
        }
        ARCH_GET_FS => t.mm().write_user(addr as usize, &regs.fs_base).map(|_| 0).map_err(fault),
        ARCH_GET_GS => t.mm().write_user(addr as usize, &regs.gs_base).map(|_| 0).map_err(fault),
        ARCH_GET_CPUID => Ok(1),
        ARCH_SET_CPUID => Ok(0),
        _ => Err(EINVAL),
    }
}

pub fn getrlimit(t: &Thread, pid: u64, resource: u64, new: u64, old: u64) -> SysResult {
    let lim = rlimit_of(t, pid, resource)?;
    let _ = new; // les limites sont fixes pour l'instant
    if old != 0 {
        t.mm().write_user(old as usize, &lim).map_err(fault)?;
    }
    Ok(0)
}

/// Limites (souple, dure) de `resource` du processus `pid` (0 : soi), `u64::MAX` = infini.
pub(crate) fn rlimit_of(t: &Thread, pid: u64, resource: u64) -> Result<[u64; 2], i64> {
    const INF: u64 = u64::MAX;
    if pid != 0 && pid as usize != t.process.vpid {
        return Err(ESRCH);
    }
    Ok(match resource {
        3 => [8 << 20, INF],                                 // RLIMIT_STACK
        4 => [0, INF],                                       // RLIMIT_CORE
        6 => [32768, 32768],                                 // RLIMIT_NPROC
        7 => [crate::fd::NOFILE as u64, crate::fd::NOFILE as u64], // RLIMIT_NOFILE
        8 => [8 << 20, 8 << 20],                             // RLIMIT_MEMLOCK
        0..=15 => [INF, INF],
        _ => return Err(EINVAL),
    })
}

fn cpu_time_ns() -> u64 {
    task::with_current_task(|tk| tk.sched.snapshot().sum_exec_ns).unwrap_or(0)
}

/// `get_robust_list(pid, head_ptr, len_ptr)`.
pub fn get_robust_list(t: &Thread, pid: u64, head: u64, len: u64) -> SysResult {
    let th = if pid == 0 { crate::current_thread_arc(t) } else { crate::find_thread_v(t, pid as u32 as usize) }.ok_or(ESRCH)?;
    t.mm().write_user::<u64>(head as usize, &(th.robust_list.load(Ordering::Acquire) as u64)).map_err(fault)?;
    t.mm().write_user::<u64>(len as usize, &24).map_err(fault)?;
    Ok(0)
}

/// Fil désigné par `pid` (un TID ; 0 = l'appelant) des appels `sched_*affinity`.
fn affinity_target(t: &Thread, pid: u64) -> Result<Arc<Thread>, i64> {
    let pid = pid as u32 as i32;
    if pid < 0 {
        return Err(ESRCH);
    }
    if pid == 0 { crate::current_thread_arc(t) } else { crate::find_thread_v(t, pid as usize) }.ok_or(ESRCH)
}

/// Taille d'un masque de cœurs vu par Linux (`cpumask_size()` : `nr_cpu_ids` en mots de 64 bits).
fn cpumask_bytes() -> usize {
    (cpu::cpu_count() as usize).div_ceil(64) * 8
}

/// `sched_getaffinity(pid, len, mask)` : cœurs permis du fil (ceux de MaiSched, `set_affinity`).
pub fn sched_getaffinity(t: &Thread, pid: u64, len: u64, mask: u64) -> SysResult {
    let bytes = cpumask_bytes();
    if (len as usize) < bytes || len & 7 != 0 {
        return Err(EINVAL);
    }
    let th = affinity_target(t, pid)?;
    let task = th.task().ok_or(ESRCH)?;
    let set = task::scheduler::affinity(&task);
    let mut m = alloc::vec![0u8; bytes];
    for (i, b) in m.iter_mut().enumerate() {
        *b = (set[i / 8] >> ((i % 8) * 8)) as u8;
    }
    t.mm().copy_to_user(mask as usize, &m).map_err(fault)?;
    Ok(bytes as i64)
}

/// `sched_setaffinity(pid, len, mask)` : restreint le fil aux cœurs du masque (bits au-delà des
/// cœurs du système ignorés ; `EINVAL` si aucun cœur en ligne n'y figure). Un fil d'un autre
/// utilisateur demande les privilèges (`CAP_SYS_NICE`). Appliqué par MaiSched, qui migre le fil
/// s'il tourne ou attend sur un cœur retiré.
pub fn sched_setaffinity(t: &Thread, pid: u64, len: u64, mask: u64) -> SysResult {
    let th = affinity_target(t, pid)?;
    {
        let me = t.process.creds.lock();
        if !Arc::ptr_eq(&th.process, &t.process) {
            let other = th.process.creds.lock();
            if me.euid != other.euid && me.euid != other.ruid && !crate::userns::ns_capable(&me, &other.user_ns, crate::userns::CAP_SYS_NICE) {
                return Err(EPERM);
            }
        }
    }
    let n = (len as usize).min(task_struct::CPU_MASK_BITS / 8);
    let mut raw = [0u8; task_struct::CPU_MASK_BITS / 8];
    t.mm().copy_from_user(&mut raw[..n], mask as usize).map_err(fault)?;
    let mut set: task::scheduler::CpuSet = [0; task_struct::CPU_MASK_BITS / 64];
    for (i, b) in raw.iter().enumerate() {
        set[i / 8] |= (*b as u64) << ((i % 8) * 8);
    }
    let task = th.task().ok_or(ESRCH)?;
    if !task::scheduler::set_affinity(&task, &set) {
        return Err(EINVAL);
    }
    if Arc::ptr_eq(&th, &crate::current_thread_arc(t).ok_or(ESRCH)?) {
        // Le fil appelant n'a plus le droit de tourner ici : il part tout de suite.
        task::scheduler::schedule();
    }
    Ok(0)
}

/// `sched_setparam`, `sched_getparam`, `sched_setscheduler`, `sched_getscheduler`,
/// `sched_get_priority_max/min`, `sched_rr_get_interval`. Les fils Linux sont ordonnancés par
/// l'EEVDF de MaiSched (`SCHED_OTHER`) ; `SCHED_BATCH` et `SCHED_IDLE` sont acceptés comme
/// indications. Les classes temps réel (`SCHED_FIFO`, `SCHED_RR`) ne sont pas encore reliées à
/// la classe temps réel de MaiSched : refusées (`EPERM`) plutôt que prétendues appliquées.
pub fn sched_policy(t: &Thread, nr: u64, a0: u64, a1: u64, a2: u64) -> SysResult {
    const SCHED_OTHER: u64 = 0;
    const SCHED_FIFO: u64 = 1;
    const SCHED_RR: u64 = 2;
    const SCHED_BATCH: u64 = 3;
    const SCHED_IDLE: u64 = 5;
    const SCHED_RESET_ON_FORK: u64 = 0x4000_0000;
    let pid = a0 as i32;
    let normal = |p: u64| matches!(p, SCHED_OTHER | SCHED_BATCH | SCHED_IDLE);
    let realtime = |p: u64| matches!(p, SCHED_FIFO | SCHED_RR);
    match nr {
        146 | 147 => {
            let p = a0;
            if realtime(p) {
                Ok(if nr == 146 { 99 } else { 1 })
            } else if normal(p) {
                Ok(0)
            } else {
                Err(EINVAL)
            }
        }
        _ if pid < 0 => Err(EINVAL),
        145 => Ok(SCHED_OTHER as i64),
        143 => {
            if a1 == 0 {
                return Err(EINVAL);
            }
            t.mm().write_user::<i32>(a1 as usize, &0).map_err(fault)?;
            Ok(0)
        }
        142 | 144 => {
            let (policy, param) = if nr == 142 { (SCHED_OTHER, a1) } else { (a1 & !SCHED_RESET_ON_FORK, a2) };
            if param == 0 {
                return Err(EINVAL);
            }
            let prio = t.mm().read_user::<i32>(param as usize).map_err(fault)?;
            if realtime(policy) {
                return if (1..=99).contains(&prio) { Err(EPERM) } else { Err(EINVAL) };
            }
            if !normal(policy) || prio != 0 {
                return Err(EINVAL);
            }
            Ok(0)
        }
        148 => {
            // Tranche de base de MaiSched (EEVDF, tranches adaptatives) : 3 ms.
            let mut raw = [0u8; 16];
            raw[8..].copy_from_slice(&3_000_000i64.to_le_bytes());
            t.mm().copy_to_user(a1 as usize, &raw).map_err(fault)?;
            Ok(0)
        }
        _ => Err(ENOSYS),
    }
}

// --- Temps -----------------------------------------------------------------------------------

fn monotonic_ns() -> u64 {
    time::Instant::now().duration_since(time::Instant::ZERO).as_nanos() as u64
}

static BOOT_EPOCH: AtomicI64 = AtomicI64::new(0);
/// Nanosecondes de l'instant du démarrage (posées par `clock_settime`).
static BOOT_EPOCH_NSEC: AtomicI64 = AtomicI64::new(0);
static BOOT_EPOCH_SET: AtomicBool = AtomicBool::new(false);

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let (y, m) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    365 * y + y / 4 - y / 100 + y / 400 + (m * 306 + 5) / 10 + (d - 1) - 719_468
}

/// Instant Unix du démarrage (RTC lue une fois).
pub(crate) fn boot_epoch() -> i64 {
    if BOOT_EPOCH_SET.load(Ordering::Acquire) {
        return BOOT_EPOCH.load(Ordering::Relaxed);
    }
    let r = rtc::read_rtc();
    let year = if r.years < 70 { 2000 + r.years as i64 } else { 1900 + r.years as i64 };
    let now = days_from_civil(year, r.months as i64, r.days as i64) * 86_400
        + r.hours as i64 * 3600
        + r.minutes as i64 * 60
        + r.seconds as i64;
    let epoch = now - (monotonic_ns() / 1_000_000_000) as i64;
    BOOT_EPOCH.store(epoch, Ordering::Relaxed);
    BOOT_EPOCH_SET.store(true, Ordering::Release);
    epoch
}

/// Heure réelle `(secondes, nanosecondes)`.
pub fn realtime() -> (i64, i64) {
    let ns = monotonic_ns() as i128 + boot_epoch() as i128 * 1_000_000_000 + BOOT_EPOCH_NSEC.load(Ordering::Acquire) as i128;
    (ns.div_euclid(1_000_000_000) as i64, ns.rem_euclid(1_000_000_000) as i64)
}

/// Instant du démarrage : (secondes, nanosecondes).
pub(crate) fn epoch_parts() -> (i64, i64) {
    (boot_epoch(), BOOT_EPOCH_NSEC.load(Ordering::Acquire))
}

/// Règle l'heure : l'instant du démarrage devient `target − temps monotone`.
pub(crate) fn set_realtime(sec: i64, nsec: i64) {
    boot_epoch();
    let total = sec as i128 * 1_000_000_000 + nsec as i128 - monotonic_ns() as i128;
    let (s, n) = (total.div_euclid(1_000_000_000) as i64, total.rem_euclid(1_000_000_000) as i64);
    BOOT_EPOCH.store(s, Ordering::Release);
    BOOT_EPOCH_NSEC.store(n, Ordering::Release);
    crate::vdso::set_epoch(s, n);
}

/// `clock_settime(clk, ts)` : seule `CLOCK_REALTIME` se règle (`EINVAL` pour les autres,
/// comme Linux), et seulement par un processus privilégié (`EPERM`).
pub fn clock_settime(t: &Thread, clk: u64, ts: u64) -> SysResult {
    if clk != 0 {
        return Err(EINVAL);
    }
    let [sec, nsec] = super::compat::get_timespec(t, ts)?;
    if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
        return Err(EINVAL);
    }
    if !crate::userns::current_capable(t, crate::userns::CAP_SYS_TIME) {
        return Err(EPERM);
    }
    set_realtime(sec, nsec);
    Ok(0)
}

/// `settimeofday(tv, tz)` : `tv` nul ne change rien (le fuseau est ignoré, comme Linux).
pub fn settimeofday(t: &Thread, tv: u64) -> SysResult {
    if tv == 0 {
        return Ok(0);
    }
    let [sec, usec] = super::compat::get_timespec(t, tv)?;
    if sec < 0 || !(0..1_000_000).contains(&usec) {
        return Err(EINVAL);
    }
    if !crate::userns::current_capable(t, crate::userns::CAP_SYS_TIME) {
        return Err(EPERM);
    }
    set_realtime(sec, usec * 1000);
    Ok(0)
}

/// Horloge de temps processeur : d'un processus ou d'un fil.
pub(crate) enum CpuClock {
    Process(Arc<crate::Process>),
    Thread(Arc<Thread>),
}

impl CpuClock {
    pub(crate) fn ns(&self) -> u64 {
        match self {
            CpuClock::Process(p) => p.cpu_ns(),
            CpuClock::Thread(th) => th.cpu_ns(),
        }
    }
}

/// Horloges CPU : `CLOCK_PROCESS_CPUTIME_ID` (2), `CLOCK_THREAD_CPUTIME_ID` (3), et les
/// `clockid_t` négatifs de `clock_getcpuclockid`/`pthread_getcpuclockid` : `(~pid << 3) | type`,
/// bit 2 = horloge d'un fil, type 0 `PROF`, 1 `VIRT`, 2 `SCHED`, 3 = horloge d'un descripteur
/// (non prise en charge). Un fil d'un autre processus est refusé (`EINVAL`, comme Linux).
/// `None` : horloge ordinaire.
pub(crate) fn cpu_clock(t: &Thread, clk: u64) -> Result<Option<CpuClock>, i64> {
    let c = clk as u32 as i32;
    match c {
        2 => return Ok(Some(CpuClock::Process(t.process.clone()))),
        3 => return Ok(Some(CpuClock::Thread(crate::current_thread_arc(t).ok_or(EINVAL)?))),
        c if c >= 0 => return Ok(None),
        _ => {}
    }
    if c & 3 == 3 {
        return Err(EINVAL);
    }
    let id = !(c >> 3) as usize;
    if c & 4 != 0 {
        let th = if id == 0 { crate::current_thread_arc(t) } else { crate::find_thread_v(t, id) }.ok_or(EINVAL)?;
        if !Arc::ptr_eq(&th.process, &t.process) {
            return Err(EINVAL);
        }
        Ok(Some(CpuClock::Thread(th)))
    } else {
        let p = if id == 0 { Some(t.process.clone()) } else { crate::find_process_v(t, id) }.ok_or(EINVAL)?;
        Ok(Some(CpuClock::Process(p)))
    }
}

fn split_ns(ns: u64) -> (i64, i64) {
    ((ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as i64)
}

fn clock_now(clk: u64) -> Result<(i64, i64), i64> {
    match clk {
        0 | 5 | 8 | 11 => Ok(realtime()),
        1 | 4 | 6 | 7 | 9 => {
            let ns = monotonic_ns();
            Ok(((ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as i64))
        }
        2 | 3 => Ok(split_ns(cpu_time_ns())),
        _ => Err(EINVAL),
    }
}

/// `getcpu(cpu, node, cache)` : cœur courant (identifiant APIC, comme `sched_getaffinity`).
pub fn getcpu(t: &Thread, cpu: u64, node: u64) -> SysResult {
    let c = cpu::current_cpu().value();
    if cpu != 0 {
        t.mm().write_user::<u32>(cpu as usize, &c).map_err(fault)?;
    }
    if node != 0 {
        t.mm().write_user::<u32>(node as usize, &0).map_err(fault)?;
    }
    Ok(0)
}

pub fn clock_gettime(t: &Thread, clk: u64, tp: u64) -> SysResult {
    let (s, ns) = match cpu_clock(t, clk)? {
        Some(c) => split_ns(c.ns()),
        None => clock_now(clk)?,
    };
    super::compat::put_timespec(t, tp, s, ns)?;
    Ok(0)
}

pub fn clock_getres(t: &Thread, clk: u64, tp: u64) -> SysResult {
    if cpu_clock(t, clk)?.is_none() {
        clock_now(clk)?;
    }
    if tp != 0 {
        let res: [i64; 2] = if matches!(clk, 5 | 6) { [0, 4_000_000] } else { [0, 1] };
        super::compat::put_timespec(t, tp, res[0], res[1])?;
    }
    Ok(0)
}

pub fn gettimeofday(t: &Thread, tv: u64) -> SysResult {
    if tv != 0 {
        let (s, ns) = realtime();
        super::compat::put_timespec(t, tv, s, ns / 1000)?;
    }
    Ok(0)
}

pub fn time(t: &Thread, tloc: u64) -> SysResult {
    let (s, _) = realtime();
    if tloc != 0 {
        if super::compat::active(t) {
            t.mm().write_user(tloc as usize, &(s as i32)).map_err(fault)?;
        } else {
            t.mm().write_user(tloc as usize, &s).map_err(fault)?;
        }
    }
    Ok(s)
}

fn read_timespec(t: &Thread, p: u64) -> Result<core::time::Duration, i64> {
    let ts = super::compat::get_timespec(t, p)?;
    if ts[0] < 0 || !(0..1_000_000_000).contains(&ts[1]) {
        return Err(EINVAL);
    }
    Ok(core::time::Duration::new(ts[0] as u64, ts[1] as u32))
}

/// Échéance monotone d'un instant absolu `abs` de l'horloge réelle (`realtime`) ou monotone.
pub fn instant_from_clock(abs: core::time::Duration, realtime: bool) -> time::Instant {
    let now_mono = time::Instant::now();
    let (s, ns) = if realtime { self::realtime() } else {
        let n = monotonic_ns();
        ((n / 1_000_000_000) as i64, (n % 1_000_000_000) as i64)
    };
    let now = core::time::Duration::new(s.max(0) as u64, ns as u32);
    now_mono + abs.saturating_sub(now)
}

/// Dort jusqu'à `deadline` ; interrompu par un signal : reste écrit dans `rem` et suite notée
/// pour `restart_syscall`.
fn sleep_until(t: &Thread, deadline: time::Instant, rem: u64, restartable: bool) -> SysResult {
    match wait_event::<()>(&[], Some(deadline), Some(t), || (time::Instant::now() >= deadline).then_some(())) {
        Ok(()) | Err(WaitError::TimedOut) => Ok(0),
        Err(WaitError::Interrupted) => {
            if !restartable {
                return Err(crate::signal::ERESTARTNOHAND);
            }
            if rem != 0 {
                let now = time::Instant::now();
                let left = if deadline > now { deadline - now } else { core::time::Duration::ZERO };
                super::compat::put_timespec(t, rem, left.as_secs() as i64, left.subsec_nanos() as i64)?;
            }
            *t.restart.lock() = Some(Restart::Sleep { deadline, rem });
            Err(crate::signal::ERESTART_RESTARTBLOCK)
        }
    }
}

pub fn nanosleep(t: &Thread, req: u64, rem: u64) -> SysResult {
    let d = read_timespec(t, req)?;
    sleep_until(t, time::Instant::now() + d, rem, true)
}

pub fn clock_nanosleep(t: &Thread, clk: u64, flags: u64, req: u64, rem: u64) -> SysResult {
    const TIMER_ABSTIME: u64 = 1;
    let d = read_timespec(t, req)?;
    match cpu_clock(t, clk)? {
        // Dormir sur sa propre horloge de fil n'a pas de sens (elle n'avance pas) : `EINVAL`.
        Some(CpuClock::Thread(_)) => return Err(EINVAL),
        Some(CpuClock::Process(p)) => {
            let now = p.cpu_ns();
            let target = if flags & TIMER_ABSTIME != 0 { d.as_nanos() as u64 } else { now + d.as_nanos() as u64 };
            return cpu_sleep(t, &p, target, if flags & TIMER_ABSTIME != 0 { 0 } else { rem });
        }
        None => {
            clock_now(clk)?;
        }
    }
    if flags & TIMER_ABSTIME != 0 {
        let deadline = instant_from_clock(d, matches!(clk, 0 | 8 | 11));
        sleep_until(t, deadline, 0, false)
    } else {
        sleep_until(t, time::Instant::now() + d, rem, true)
    }
}

/// Sommeil jusqu'à ce que le processus `p` ait consommé `target` ns de temps processeur : réveils
/// au plus tôt possible (le temps processeur avance au plus de `k` ns par ns, `k` = fils
/// utilisables en parallèle), comme les minuteurs de `crate::cputimer`.
fn cpu_sleep(t: &Thread, p: &Arc<crate::Process>, target: u64, rem: u64) -> SysResult {
    loop {
        let now = p.cpu_ns();
        if now >= target {
            return Ok(0);
        }
        let wall = ((target - now) / crate::cputimer::parallelism(p)).clamp(100_000, 1_000_000_000);
        let deadline = time::Instant::now() + core::time::Duration::from_nanos(wall);
        if let Err(WaitError::Interrupted) = wait_event::<()>(&[], Some(deadline), Some(t), || None) {
            if rem != 0 {
                let (s, ns) = split_ns(target.saturating_sub(p.cpu_ns()));
                super::compat::put_timespec(t, rem, s, ns)?;
            }
            return Err(EINTR);
        }
    }
}

/// `restart_syscall` : reprend un sommeil interrompu là où il en était.
pub fn restart_syscall(t: &Thread) -> SysResult {
    let r = t.restart.lock().take();
    match r {
        Some(Restart::Sleep { deadline, rem }) => sleep_until(t, deadline, rem, true),
        None => Err(EINTR),
    }
}

// --- Système -------------------------------------------------------------------------------

/// Nom d'hôte (0) ou de domaine (1) de l'espace UTS de l'appelant.
pub(crate) fn uts_name(t: &Thread, which: usize) -> alloc::vec::Vec<u8> {
    crate::ns::uts_name(&t.process, which)
}

/// `sethostname(name, len)` (`which` 0) et `setdomainname` (1) : 64 octets au plus, réservés
/// aux processus privilégiés (`CAP_SYS_ADMIN`).
pub fn set_uts_name(t: &Thread, which: usize, name: u64, len: u64) -> SysResult {
    if len > 64 {
        return Err(EINVAL);
    }
    let uts = crate::ns::of(&t.process).uts.clone();
    if !crate::userns::ns_capable(&t.process.creds.lock(), &uts.user_ns, crate::userns::CAP_SYS_ADMIN) {
        return Err(EPERM);
    }
    let mut v = alloc::vec![0u8; len as usize];
    t.mm().copy_from_user(&mut v, name as usize).map_err(fault)?;
    uts.names.lock()[which] = Some(v);
    Ok(0)
}

pub fn uname(t: &Thread, buf: u64) -> SysResult {
    let mut u = [0u8; 65 * 6];
    let (host, domain) = (uts_name(t, 0), uts_name(t, 1));
    let fields: [&[u8]; 6] = [b"Linux", &host, RELEASE.as_bytes(), b"#1 SMP PREEMPT MaiOS", b"x86_64", &domain];
    for (i, f) in fields.iter().enumerate() {
        let n = f.len().min(64);
        u[i * 65..i * 65 + n].copy_from_slice(&f[..n]);
    }
    t.mm().copy_to_user(buf as usize, &u).map_err(fault)?;
    Ok(0)
}

pub fn sysinfo(t: &Thread, info: u64) -> SysResult {
    let s = frame_allocator::stats();
    let mut b = [0u8; 112];
    let put = |b: &mut [u8; 112], off: usize, v: u64, n: usize| b[off..off + n].copy_from_slice(&v.to_le_bytes()[..n]);
    put(&mut b, 0, monotonic_ns() / 1_000_000_000, 8);
    let l = crate::loadavg::get();
    for (i, v) in l.iter().enumerate() {
        put(&mut b, 8 + i * 8, v * 65536 / 100, 8);
    }
    put(&mut b, 32, (s.total_frames * 4096) as u64, 8);
    put(&mut b, 40, (s.free_frames * 4096) as u64, 8);
    put(&mut b, 80, 1, 2);
    put(&mut b, 104, 1, 4);
    if super::compat::active(t) {
        return sysinfo32(t, info, &b);
    }
    t.mm().copy_to_user(info as usize, &b).map_err(fault)?;
    Ok(0)
}

/// `struct compat_sysinfo` (64 octets) depuis la disposition 64 bits, comme
/// `compat_sys_sysinfo` : des tailles de mémoire qui débordent 32 bits sont ramenées en
/// augmentant `mem_unit`.
fn sysinfo32(t: &Thread, info: u64, b: &[u8; 112]) -> SysResult {
    let q = |off: usize| u64::from_le_bytes(b[off..off + 8].try_into().unwrap());
    // totalram, freeram, sharedram, bufferram, totalswap, freeswap, totalhigh, freehigh.
    let mut mem = [q(32), q(40), q(48), q(56), q(64), q(72), q(88), q(96)];
    let mut unit = u32::from_le_bytes(b[104..108].try_into().unwrap()) as u64;
    let mut shift = 0;
    while mem.iter().any(|&m| m >> shift > u32::MAX as u64) {
        shift += 1;
    }
    if shift > 0 {
        mem.iter_mut().for_each(|m| *m >>= shift);
        unit <<= shift;
    }
    let mut o = [0u8; 64];
    let put = |o: &mut [u8; 64], off: usize, v: u64, n: usize| o[off..off + n].copy_from_slice(&v.to_le_bytes()[..n]);
    put(&mut o, 0, q(0), 4);
    for i in 0..3 {
        put(&mut o, 4 + i * 4, q(8 + i * 8), 4);
    }
    for (i, &m) in mem[..6].iter().enumerate() {
        put(&mut o, 16 + i * 4, m, 4);
    }
    o[40..42].copy_from_slice(&b[80..82]); // procs
    put(&mut o, 44, mem[6], 4);
    put(&mut o, 48, mem[7], 4);
    put(&mut o, 52, unit, 4);
    t.mm().copy_to_user(info as usize, &o).map_err(fault)?;
    Ok(0)
}

pub fn getrandom(t: &Thread, buf: u64, len: u64, _flags: u64) -> SysResult {
    let len = (len as usize).min(32 << 20);
    let mut kb = alloc::vec![0u8; len.min(4096)];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(kb.len());
        for c in kb[..n].chunks_mut(8) {
            let v = uvm::rand::random_u64().to_le_bytes();
            c.copy_from_slice(&v[..c.len()]);
        }
        t.mm().copy_to_user(buf as usize + done, &kb[..n]).map_err(fault)?;
        done += n;
    }
    Ok(len as i64)
}

/// Appel inconnu : signalé une fois par numéro.
pub fn unknown(t: &Thread, nr: u64) {
    #[allow(clippy::declare_interior_mutable_const)]
    const ZERO: AtomicU64 = AtomicU64::new(0);
    static SEEN: [AtomicU64; 8] = [ZERO; 8];
    let (w, b) = ((nr as usize / 64) % 8, nr % 64);
    if nr < 512 && SEEN[w].fetch_or(1 << b, Ordering::Relaxed) & (1 << b) != 0 {
        return;
    }
    warn!("maiproc : pid {} appel systeme Linux {} non pris en charge (ENOSYS)", t.process.pid, nr);
}

// --- Capacités ----------------------------------------------------------------------------------

const CAP_V1: u32 = 0x1998_0330;
const CAP_V2: u32 = 0x2007_1026;
const CAP_V3: u32 = 0x2008_0522;

/// En-tête de `capget`/`capset` : version (mots de données : 1 ou 2) et processus visé.
fn cap_header(t: &Thread, hdr: u64, data: u64) -> Result<(usize, i32), i64> {
    let h: [u32; 2] = t.mm().read_user(hdr as usize).map_err(fault)?;
    let words = match h[0] {
        CAP_V1 => 1,
        CAP_V2 | CAP_V3 => 2,
        _ => {
            // Version inconnue : on écrit la nôtre (sondage de libcap).
            t.mm().write_user::<u32>(hdr as usize, &CAP_V3).map_err(fault)?;
            return Err(if data == 0 { 0 } else { EINVAL });
        }
    };
    Ok((words, h[1] as i32))
}

/// `capget(hdr, data)`.
pub fn capget(t: &Thread, hdr: u64, data: u64) -> SysResult {
    let (words, pid) = match cap_header(t, hdr, data) {
        Ok(v) => v,
        Err(0) => return Ok(0),
        Err(e) => return Err(e),
    };
    if pid < 0 {
        return Err(EINVAL);
    }
    let c = if pid == 0 || pid as usize == t.process.vpid {
        t.process.creds.lock().clone()
    } else {
        crate::find_thread_v(t, pid as usize).ok_or(ESRCH)?.process.creds.lock().clone()
    };
    if data == 0 {
        return Ok(0);
    }
    let mut raw = [0u32; 6];
    for w in 0..words {
        raw[w * 3] = (c.cap_eff >> (32 * w)) as u32;
        raw[w * 3 + 1] = (c.cap_prm >> (32 * w)) as u32;
        raw[w * 3 + 2] = (c.cap_inh >> (32 * w)) as u32;
    }
    let bytes: alloc::vec::Vec<u8> = raw[..words * 3].iter().flat_map(|v| v.to_le_bytes()).collect();
    t.mm().copy_to_user(data as usize, &bytes).map_err(fault)?;
    Ok(0)
}

/// `capset(hdr, data)` : seulement sur soi, règles de `cap_capset`.
pub fn capset(t: &Thread, hdr: u64, data: u64) -> SysResult {
    use crate::userns::{bit, CAP_FULL, CAP_SETPCAP};
    let (words, pid) = match cap_header(t, hdr, data) {
        Ok(v) => v,
        Err(0) => return Err(EINVAL),
        Err(e) => return Err(e),
    };
    if pid != 0 && pid as usize != t.process.vpid {
        return Err(EPERM);
    }
    let mut raw = [0u8; 24];
    t.mm().copy_from_user(&mut raw[..words * 12], data as usize).map_err(fault)?;
    let word = |i: usize| u32::from_le_bytes(raw[i * 4..i * 4 + 4].try_into().unwrap()) as u64;
    let (mut eff, mut prm, mut inh) = (0u64, 0u64, 0u64);
    for w in 0..words {
        eff |= word(w * 3) << (32 * w);
        prm |= word(w * 3 + 1) << (32 * w);
        inh |= word(w * 3 + 2) << (32 * w);
    }
    let (eff, prm, inh) = (eff & CAP_FULL, prm & CAP_FULL, inh & CAP_FULL);
    let mut c = t.process.creds.lock();
    let subset = |a: u64, b: u64| a & !b == 0;
    if !subset(inh, c.cap_inh | c.cap_prm) && c.cap_eff & bit(CAP_SETPCAP) == 0 {
        return Err(EPERM);
    }
    if !subset(inh, c.cap_inh | c.cap_bnd) || !subset(prm, c.cap_prm) || !subset(eff, prm) {
        return Err(EPERM);
    }
    c.cap_eff = eff;
    c.cap_prm = prm;
    c.cap_inh = inh;
    c.cap_amb &= prm & inh;
    Ok(0)
}
