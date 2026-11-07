//! Appels système de processus, signaux, temps et informations système.

use super::errno::*;
use super::{fault, Outcome, SysResult};
use crate::{exit_status, signal_status, SigAction, Thread};
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use log::warn;
use uentry::UserRegs;

/// Version annoncée par `uname` (les libc lisent la version du noyau Linux attendue).
pub const RELEASE: &str = "6.12.0-maios";

// --- Fin, signaux ---------------------------------------------------------------------------

pub fn exit(t: &Thread, code: i32, _group: bool) -> Outcome {
    let status = exit_status(code);
    t.process.exit_status.compare_exchange(-1, status, Ordering::AcqRel, Ordering::Acquire).ok();
    Outcome::Exit(t.process.exit_status.load(Ordering::Acquire))
}

const SIGKILL: i32 = 9;
const SIGSTOP: i32 = 19;
const SIG_IGN: u64 = 1;

/// Action par défaut « ignorer » (SIGCHLD, SIGCONT, SIGURG, SIGWINCH) ; arrêt (SIGSTOP…) : pas de
/// contrôle de tâches encore, ignoré aussi.
fn default_ignored(sig: i32) -> bool {
    matches!(sig, 17 | 18 | 23 | 28 | 19 | 20 | 21 | 22)
}

/// Envoie `sig` au processus courant : `Some(statut)` s'il doit se terminer.
fn deliver_self(t: &Thread, sig: i32) -> Result<Option<i32>, i64> {
    if !(0..=64).contains(&sig) {
        return Err(EINVAL);
    }
    if sig == 0 {
        return Ok(None);
    }
    let act = t.process.sigactions.lock()[sig as usize - 1];
    if sig != SIGKILL && sig != SIGSTOP {
        if act.handler == SIG_IGN || (act.handler == 0 && default_ignored(sig)) {
            return Ok(None);
        }
        if act.handler > SIG_IGN {
            // Livraison aux gestionnaires utilisateur : cadre de signal et `rt_sigreturn` (M2).
            warn!("maiproc : pid {} signal {} avec gestionnaire : action par defaut appliquee (livraison a venir)", t.process.pid, sig);
        }
    }
    if default_ignored(sig) && sig != SIGSTOP {
        return Ok(None);
    }
    let status = signal_status(sig);
    t.process.exit_status.compare_exchange(-1, status, Ordering::AcqRel, Ordering::Acquire).ok();
    Ok(Some(status))
}

pub fn kill(t: &Thread, pid: i64, sig: i32) -> Result<Option<i32>, i64> {
    let me = t.process.pid as i64;
    if pid == me || pid == 0 || pid == -1 || pid == -me {
        deliver_self(t, sig)
    } else {
        Err(ESRCH)
    }
}

pub fn tkill(t: &Thread, tid: i64, sig: i32) -> Result<Option<i32>, i64> {
    if tid == t.tid as i64 {
        deliver_self(t, sig)
    } else {
        Err(ESRCH)
    }
}

pub fn rt_sigaction(t: &Thread, sig: u64, act: u64, oact: u64, size: u64) -> SysResult {
    if size != 8 || sig == 0 || sig > 64 || ((sig as i32 == SIGKILL || sig as i32 == SIGSTOP) && act != 0) {
        return Err(EINVAL);
    }
    let mm = &t.process.mm;
    let new = if act != 0 {
        let raw = mm.read_user::<[u64; 4]>(act as usize).map_err(fault)?;
        Some(SigAction { handler: raw[0], flags: raw[1], restorer: raw[2], mask: raw[3] })
    } else {
        None
    };
    let old = {
        let mut table = t.process.sigactions.lock();
        let old = table[sig as usize - 1];
        if let Some(n) = new {
            table[sig as usize - 1] = n;
        }
        old
    };
    if oact != 0 {
        mm.write_user(oact as usize, &[old.handler, old.flags, old.restorer, old.mask]).map_err(fault)?;
    }
    Ok(0)
}

pub fn rt_sigprocmask(t: &Thread, how: u64, set: u64, oset: u64, size: u64) -> SysResult {
    if size != 8 {
        return Err(EINVAL);
    }
    let mm = &t.process.mm;
    let old = t.sigmask.load(Ordering::Acquire);
    if set != 0 {
        let s = mm.read_user::<u64>(set as usize).map_err(fault)?;
        let unblockable = (1u64 << (SIGKILL - 1)) | (1u64 << (SIGSTOP - 1));
        let new = match how {
            0 => old | s,
            1 => old & !s,
            2 => s,
            _ => return Err(EINVAL),
        } & !unblockable;
        t.sigmask.store(new, Ordering::Release);
    }
    if oset != 0 {
        mm.write_user(oset as usize, &old).map_err(fault)?;
    }
    Ok(0)
}

pub fn rt_sigpending(t: &Thread, set: u64, size: u64) -> SysResult {
    if size != 8 {
        return Err(EINVAL);
    }
    t.process.mm.write_user::<u64>(set as usize, &0).map_err(fault)?;
    Ok(0)
}

// --- Identité, limites, contrôle -----------------------------------------------------------------

pub fn getresid(t: &Thread, r: u64, e: u64, s: u64) -> SysResult {
    for p in [r, e, s] {
        t.process.mm.write_user::<u32>(p as usize, &0).map_err(fault)?;
    }
    Ok(0)
}

pub fn prctl(t: &Thread, option: u64, arg2: u64) -> SysResult {
    const PR_SET_PDEATHSIG: u64 = 1;
    const PR_GET_DUMPABLE: u64 = 3;
    const PR_SET_DUMPABLE: u64 = 4;
    const PR_SET_NAME: u64 = 15;
    const PR_GET_NAME: u64 = 16;
    const PR_CAPBSET_READ: u64 = 23;
    const PR_SET_NO_NEW_PRIVS: u64 = 38;
    const PR_GET_NO_NEW_PRIVS: u64 = 39;
    const PR_SET_VMA: u64 = 0x5356_4d41;
    let mm = &t.process.mm;
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
            *t.process.comm.lock() = comm;
            Ok(0)
        }
        PR_GET_NAME => {
            let comm = *t.process.comm.lock();
            mm.copy_to_user(arg2 as usize, &comm).map_err(fault)?;
            Ok(0)
        }
        PR_GET_DUMPABLE => Ok(1),
        PR_SET_DUMPABLE | PR_SET_PDEATHSIG | PR_SET_NO_NEW_PRIVS | PR_SET_VMA => Ok(0),
        PR_GET_NO_NEW_PRIVS => Ok(0),
        PR_CAPBSET_READ => Ok(1),
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
        ARCH_GET_FS => t.process.mm.write_user(addr as usize, &regs.fs_base).map(|_| 0).map_err(fault),
        ARCH_GET_GS => t.process.mm.write_user(addr as usize, &regs.gs_base).map(|_| 0).map_err(fault),
        ARCH_GET_CPUID => Ok(1),
        ARCH_SET_CPUID => Ok(0),
        _ => Err(EINVAL),
    }
}

pub fn getrlimit(t: &Thread, pid: u64, resource: u64, new: u64, old: u64) -> SysResult {
    const INF: u64 = u64::MAX;
    if pid != 0 && pid as usize != t.process.pid {
        return Err(ESRCH);
    }
    let lim: [u64; 2] = match resource {
        3 => [8 << 20, INF],                                 // RLIMIT_STACK
        4 => [0, INF],                                       // RLIMIT_CORE
        6 => [32768, 32768],                                 // RLIMIT_NPROC
        7 => [crate::fd::NOFILE as u64, crate::fd::NOFILE as u64], // RLIMIT_NOFILE
        8 => [8 << 20, 8 << 20],                             // RLIMIT_MEMLOCK
        0..=15 => [INF, INF],
        _ => return Err(EINVAL),
    };
    let _ = new; // les limites sont fixes pour l'instant
    if old != 0 {
        t.process.mm.write_user(old as usize, &lim).map_err(fault)?;
    }
    Ok(0)
}

fn cpu_time_ns() -> u64 {
    task::with_current_task(|tk| tk.sched.snapshot().sum_exec_ns).unwrap_or(0)
}

pub fn getrusage(t: &Thread, _who: u64, usage: u64) -> SysResult {
    let ns = cpu_time_ns();
    let mut ru = [0u64; 18];
    ru[0] = ns / 1_000_000_000;
    ru[1] = (ns % 1_000_000_000) / 1000;
    ru[4] = (t.process.mm.rss() * 4) as u64; // ru_maxrss (Kio), approché par la RSS
    t.process.mm.write_user(usage as usize, &ru).map_err(fault)?;
    Ok(0)
}

pub fn times(t: &Thread, buf: u64) -> SysResult {
    let ticks = |ns: u64| ns / 10_000_000; // 100 Hz
    if buf != 0 {
        let tms: [u64; 4] = [ticks(cpu_time_ns()), 0, 0, 0];
        t.process.mm.write_user(buf as usize, &tms).map_err(fault)?;
    }
    Ok(ticks(monotonic_ns()) as i64)
}

pub fn sched_getaffinity(t: &Thread, len: u64, mask: u64) -> SysResult {
    let n = cpu::cpu_count() as usize;
    let bytes = ((n + 63) / 64) * 8;
    if (len as usize) < bytes {
        return Err(EINVAL);
    }
    let mut m = alloc::vec![0u8; bytes];
    for c in 0..n {
        m[c / 8] |= 1 << (c % 8);
    }
    t.process.mm.copy_to_user(mask as usize, &m).map_err(fault)?;
    Ok(bytes as i64)
}

// --- Temps -----------------------------------------------------------------------------------

fn monotonic_ns() -> u64 {
    time::Instant::now().duration_since(time::Instant::ZERO).as_nanos() as u64
}

static BOOT_EPOCH: AtomicI64 = AtomicI64::new(0);
static BOOT_EPOCH_SET: AtomicBool = AtomicBool::new(false);

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let (y, m) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    365 * y + y / 4 - y / 100 + y / 400 + (m * 306 + 5) / 10 + (d - 1) - 719_468
}

/// Instant Unix du démarrage (RTC lue une fois).
fn boot_epoch() -> i64 {
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
    let ns = monotonic_ns();
    (boot_epoch() + (ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as i64)
}

fn clock_now(clk: u64) -> Result<(i64, i64), i64> {
    match clk {
        0 | 5 | 8 | 11 => Ok(realtime()),
        1 | 4 | 6 | 7 | 9 => {
            let ns = monotonic_ns();
            Ok(((ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as i64))
        }
        2 | 3 => {
            let ns = cpu_time_ns();
            Ok(((ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as i64))
        }
        _ => Err(EINVAL),
    }
}

pub fn clock_gettime(t: &Thread, clk: u64, tp: u64) -> SysResult {
    let (s, ns) = clock_now(clk)?;
    t.process.mm.write_user(tp as usize, &[s, ns]).map_err(fault)?;
    Ok(0)
}

pub fn clock_getres(t: &Thread, clk: u64, tp: u64) -> SysResult {
    clock_now(clk)?;
    if tp != 0 {
        let res: [i64; 2] = if matches!(clk, 5 | 6) { [0, 4_000_000] } else { [0, 1] };
        t.process.mm.write_user(tp as usize, &res).map_err(fault)?;
    }
    Ok(0)
}

pub fn gettimeofday(t: &Thread, tv: u64) -> SysResult {
    if tv != 0 {
        let (s, ns) = realtime();
        t.process.mm.write_user(tv as usize, &[s, ns / 1000]).map_err(fault)?;
    }
    Ok(0)
}

pub fn time(t: &Thread, tloc: u64) -> SysResult {
    let (s, _) = realtime();
    if tloc != 0 {
        t.process.mm.write_user(tloc as usize, &s).map_err(fault)?;
    }
    Ok(s)
}

fn read_timespec(t: &Thread, p: u64) -> Result<core::time::Duration, i64> {
    let ts = t.process.mm.read_user::<[i64; 2]>(p as usize).map_err(fault)?;
    if ts[0] < 0 || !(0..1_000_000_000).contains(&ts[1]) {
        return Err(EINVAL);
    }
    Ok(core::time::Duration::new(ts[0] as u64, ts[1] as u32))
}

pub fn nanosleep(t: &Thread, req: u64, rem: u64) -> SysResult {
    let d = read_timespec(t, req)?;
    let _ = sleep::sleep(d);
    if rem != 0 {
        t.process.mm.write_user(rem as usize, &[0i64, 0]).map_err(fault)?;
    }
    Ok(0)
}

pub fn clock_nanosleep(t: &Thread, clk: u64, flags: u64, req: u64, rem: u64) -> SysResult {
    const TIMER_ABSTIME: u64 = 1;
    let d = read_timespec(t, req)?;
    let wait = if flags & TIMER_ABSTIME != 0 {
        let (s, ns) = clock_now(clk)?;
        let now = core::time::Duration::new(s as u64, ns as u32);
        d.saturating_sub(now)
    } else {
        clock_now(clk)?;
        d
    };
    let _ = sleep::sleep(wait);
    if rem != 0 && flags & TIMER_ABSTIME == 0 {
        t.process.mm.write_user(rem as usize, &[0i64, 0]).map_err(fault)?;
    }
    Ok(0)
}

// --- Système -------------------------------------------------------------------------------

pub fn uname(t: &Thread, buf: u64) -> SysResult {
    let mut u = [0u8; 65 * 6];
    let fields = ["Linux", "maios", RELEASE, "#1 SMP PREEMPT MaiOS", "x86_64", "(none)"];
    for (i, f) in fields.iter().enumerate() {
        u[i * 65..i * 65 + f.len()].copy_from_slice(f.as_bytes());
    }
    t.process.mm.copy_to_user(buf as usize, &u).map_err(fault)?;
    Ok(0)
}

pub fn sysinfo(t: &Thread, info: u64) -> SysResult {
    let s = frame_allocator::stats();
    let mut b = [0u8; 112];
    let put = |b: &mut [u8; 112], off: usize, v: u64, n: usize| b[off..off + n].copy_from_slice(&v.to_le_bytes()[..n]);
    put(&mut b, 0, monotonic_ns() / 1_000_000_000, 8);
    put(&mut b, 32, (s.total_frames * 4096) as u64, 8);
    put(&mut b, 40, (s.free_frames * 4096) as u64, 8);
    put(&mut b, 80, 1, 2);
    put(&mut b, 104, 1, 4);
    t.process.mm.copy_to_user(info as usize, &b).map_err(fault)?;
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
        t.process.mm.copy_to_user(buf as usize + done, &kb[..n]).map_err(fault)?;
        done += n;
    }
    Ok(len as i64)
}

/// `futex` : un seul fil par processus pour l'instant (M1). `WAIT` vérifie la valeur et attend
/// (délai ou changement), `WAKE` ne réveille personne. Les files d'attente arrivent avec les fils
/// multiples (M2).
pub fn futex(t: &Thread, uaddr: u64, op: u64, val: u64, timeout: u64) -> SysResult {
    const FUTEX_WAIT: u64 = 0;
    const FUTEX_WAKE: u64 = 1;
    const FUTEX_WAIT_BITSET: u64 = 9;
    const FUTEX_WAKE_BITSET: u64 = 10;
    const ETIMEDOUT: i64 = 110;
    let mm = &t.process.mm;
    match op & 0x7f {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            let cur = mm.read_user::<u32>(uaddr as usize).map_err(fault)?;
            if cur != val as u32 {
                return Err(EAGAIN);
            }
            let limit = if timeout != 0 { Some(read_timespec(t, timeout)?) } else { None };
            let start = time::Instant::now();
            loop {
                let _ = sleep::sleep(core::time::Duration::from_millis(1));
                if mm.read_user::<u32>(uaddr as usize).map_err(fault)? != val as u32 {
                    return Ok(0);
                }
                if let Some(l) = limit {
                    if time::Instant::now().duration_since(start) >= l {
                        return Err(ETIMEDOUT);
                    }
                }
            }
        }
        FUTEX_WAKE | FUTEX_WAKE_BITSET => Ok(0),
        _ => Err(ENOSYS),
    }
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
