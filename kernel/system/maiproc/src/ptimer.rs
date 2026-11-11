//! Minuteurs POSIX par processus : `timer_create`, `timer_settime`, `timer_gettime`,
//! `timer_getoverrun`, `timer_delete` (Linux `kernel/time/posix-timers.c`).
//!
//! * Horloges murales (`CLOCK_REALTIME`, `MONOTONIC`, `BOOTTIME`, `TAI`, alarmes pour root) :
//!   armées sur les minuteurs du noyau (`sleep::future`, appelés depuis l'interruption de
//!   l'horloge) ; un réarmement change la génération, ce qui rend caduc le réveil précédent.
//! * Horloges de temps processeur (`CLOCK_PROCESS_CPUTIME_ID`, `CLOCK_THREAD_CPUTIME_ID`,
//!   `clockid_t` négatifs) : vérifiées par la tâche de [`crate::cputimer`], qui dort jusqu'au
//!   plus tôt possible de l'échéance.
//! * Notification : signal au processus (`SIGEV_SIGNAL`, `SIGEV_THREAD` que la glibc traduit),
//!   à un fil (`SIGEV_THREAD_ID`) ou rien (`SIGEV_NONE`). `siginfo` : `SI_TIMER`, identifiant,
//!   dépassement, valeur.
//! * Dépassement (`si_overrun`, `timer_getoverrun`) : comme Linux, un minuteur n'a qu'un signal
//!   en file ; les échéances écoulées pendant qu'il attend sont comptées, et leur nombre est
//!   posé dans le `siginfo` au moment où le signal est retiré de la file ([`on_dequeue`]).
//!
//! Les minuteurs ne passent pas `fork` et disparaissent à `execve` (comme Linux).

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use core::time::Duration;
use sync_irq::IrqSafeMutex;
use time::Instant;

use crate::linux::errno::*;
use crate::linux::sys::{cpu_clock, instant_from_clock, CpuClock};

use crate::linux::{fault, SysResult};
use crate::signal::{self, SigInfo};
use crate::{Process, Thread};

/// `si_code` d'un signal de minuteur POSIX.
pub const SI_TIMER: i32 = -2;
const SIGALRM: i32 = 14;
const SIGRTMAX: i32 = 64;
const SIGEV_SIGNAL: i32 = 0;
const SIGEV_NONE: i32 = 1;
const SIGEV_THREAD: i32 = 2;
const SIGEV_THREAD_ID: i32 = 4;
const TIMER_ABSTIME: u64 = 1;
/// `DELAYTIMER_MAX` : plafond du compteur de dépassement.
const DELAYTIMER_MAX: u32 = i32::MAX as u32;
/// Minuteurs par processus (Linux : borné par `RLIMIT_SIGPENDING`) ; au-delà, `EAGAIN`.
const MAX_TIMERS: usize = 32768;

enum Clock {
    /// Horloge murale ; `realtime` : une échéance absolue se lit sur l'heure du jour.
    Wall { realtime: bool },
    /// Temps processeur d'un processus ou d'un fil (références faibles : le processus tient
    /// ses minuteurs).
    CpuProcess(Weak<Process>),
    CpuThread(Weak<Thread>),
}

impl Clock {
    /// Temps processeur consommé (ns) ; `None` : processus ou fil disparu.
    fn cpu_ns(&self) -> Option<u64> {
        match self {
            Clock::Wall { .. } => None,
            Clock::CpuProcess(p) => p.upgrade().map(|p| p.cpu_ns()),
            Clock::CpuThread(t) => t.upgrade().map(|t| t.cpu_ns()),
        }
    }
}

enum Target {
    Process,
    Thread(Weak<Thread>),
}

struct State {
    gen: u64,
    /// Horloge murale : prochaine échéance (horloge monotone du noyau).
    deadline: Option<Instant>,
    interval: Duration,
    /// Horloge de temps processeur : échéance et période en ns (0 : désarmé).
    cpu_expires: u64,
    cpu_interval: u64,
}

pub struct PosixTimer {
    id: i32,
    clock: Clock,
    /// `None` : `SIGEV_NONE`.
    signo: Option<i32>,
    value: u64,
    target: Target,
    process: Weak<Process>,
    state: IrqSafeMutex<State>,
    /// Le signal de ce minuteur est en file (pas encore retiré par le fil).
    queued: AtomicBool,
    /// Échéances écoulées pendant que le signal attendait.
    overrun: AtomicU32,
    /// Dépassement du dernier signal remis (`timer_getoverrun`).
    overrun_last: AtomicI32,
}

/// Minuteurs d'un processus.
#[derive(Default)]
pub struct Timers {
    map: BTreeMap<i32, Arc<PosixTimer>>,
    next: i32,
}

impl Timers {
    /// `execve` : tous les minuteurs disparaissent (leurs réveils en vol deviennent caducs).
    pub fn clear(&mut self) {
        for t in self.map.values() {
            t.disarm();
        }
        self.map.clear();
        self.next = 0;
    }
}

struct WallWaker {
    timer: Weak<PosixTimer>,
    gen: u64,
}

impl alloc::task::Wake for WallWaker {
    fn wake(self: Arc<Self>) {
        if let Some(t) = self.timer.upgrade() {
            t.fire_wall(self.gen);
        }
    }
}

impl PosixTimer {
    fn disarm(&self) {
        let mut s = self.state.lock();
        s.gen += 1;
        s.deadline = None;
        s.cpu_expires = 0;
    }

    fn arm_wall(self: &Arc<Self>, gen: u64, at: Instant) {
        let w = Arc::new(WallWaker { timer: Arc::downgrade(self), gen });
        let now = Instant::now();
        let after = if at > now { at - now } else { Duration::ZERO };
        sleep::future::sleep(after, core::task::Waker::from(w));
    }

    /// Échéance d'une horloge murale (depuis l'interruption de l'horloge).
    fn fire_wall(self: &Arc<Self>, gen: u64) {
        let (rearm, extra) = {
            let mut s = self.state.lock();
            let Some(d) = s.deadline else { return };
            if s.gen != gen {
                return;
            }
            let now = Instant::now();
            if now < d {
                // Réveil en avance (granularité de l'horloge) : réarmé sans compter.
                (Some(d), None)
            } else if s.interval.is_zero() {
                s.deadline = None;
                (None, Some(0))
            } else {
                // Périodes manquées comptées d'un coup (`hrtimer_forward`).
                let late = now.duration_since(d).as_nanos();
                let n = (late / s.interval.as_nanos()) as u64;
                let next = d + Duration::from_nanos((s.interval.as_nanos() as u64).saturating_mul(n + 1));
                s.deadline = Some(next);
                (Some(next), Some(n))
            }
        };
        if let Some(at) = rearm {
            self.arm_wall(gen, at);
        }
        if let Some(extra) = extra {
            self.deliver(extra);
        }
    }

    /// Une échéance (plus `extra` manquées) : signal, ou dépassement si le précédent attend.
    fn deliver(&self, extra: u64) {
        let Some(signo) = self.signo else { return };
        let add = |n: u64| {
            let _ = self.overrun.fetch_update(Ordering::AcqRel, Ordering::Acquire, |o| {
                Some((o as u64).saturating_add(n).min(DELAYTIMER_MAX as u64) as u32)
            });
        };
        if self.queued.swap(true, Ordering::AcqRel) {
            add(1 + extra);
            return;
        }
        add(extra);
        let Some(p) = self.process.upgrade() else { return };
        let mut info = SigInfo::new(signo, SI_TIMER);
        info.set_i32(16, self.id);
        info.set_u64(24, self.value);
        let sent = match &self.target {
            Target::Thread(w) => match w.upgrade() {
                Some(th) => signal::send_to_thread(&th, info, false),
                None => signal::send_to_process(&p, info),
            },
            Target::Process => signal::send_to_process(&p, info),
        };
        if !sent {
            // Ignoré, ou un signal standard du même numéro attend déjà : rien n'est en file.
            self.queued.store(false, Ordering::Release);
        }
    }

    /// `(reste, intervalle)` ; reste nul = désarmé.
    fn current(&self) -> (Duration, Duration) {
        let s = self.state.lock();
        match &self.clock {
            Clock::Wall { .. } => {
                let left = s.deadline.map_or(Duration::ZERO, |d| {
                    let now = Instant::now();
                    if d > now { d - now } else { Duration::from_nanos(1) }
                });
                (left, s.interval)
            }
            Clock::CpuProcess(_) | Clock::CpuThread(_) => {
                if s.cpu_expires == 0 {
                    return (Duration::ZERO, Duration::from_nanos(s.cpu_interval));
                }
                let left = s.cpu_expires.saturating_sub(self.clock.cpu_ns().unwrap_or(0)).max(1);
                (Duration::from_nanos(left), Duration::from_nanos(s.cpu_interval))
            }
        }
    }
}

/// Minuteur de temps processeur vérifié par la tâche de `cputimer` : délai mural (ns) avant la
/// prochaine vérification utile, `None` s'il n'est plus armé.
pub(crate) fn check_cpu(t: &Arc<PosixTimer>) -> Option<u64> {
    if t.process.upgrade().map_or(true, |p| p.is_zombie()) {
        return None;
    }
    let now = t.clock.cpu_ns()?;
    let fired = {
        let mut s = t.state.lock();
        if s.cpu_expires == 0 {
            return None;
        }
        if now < s.cpu_expires {
            None
        } else if s.cpu_interval == 0 {
            s.cpu_expires = 0;
            Some(0)
        } else {
            let missed = (now - s.cpu_expires) / s.cpu_interval;
            s.cpu_expires += (missed + 1) * s.cpu_interval;
            Some(missed)
        }
    };
    if let Some(extra) = fired {
        t.deliver(extra);
    }
    let s = t.state.lock();
    if s.cpu_expires == 0 {
        return None;
    }
    let k = match &t.clock {
        Clock::CpuProcess(p) => p.upgrade().map_or(1, |p| crate::cputimer::parallelism(&p)),
        _ => 1,
    };
    Some(s.cpu_expires.saturating_sub(now) / k)
}

/// Le minuteur de temps processeur est-il armé ?
pub(crate) fn cpu_armed(t: &PosixTimer) -> bool {
    t.state.lock().cpu_expires != 0
}

/// Un signal de minuteur quitte la file : son dépassement est posé dans le `siginfo` et le
/// minuteur peut de nouveau mettre un signal en file.
pub(crate) fn on_dequeue(p: &Process, info: &mut SigInfo) {
    if info.get_i32(8) != SI_TIMER {
        return;
    }
    let id = info.get_i32(16);
    let Some(t) = p.ptimers.lock().map.get(&id).cloned() else { return };
    let ov = t.overrun.swap(0, Ordering::AcqRel).min(DELAYTIMER_MAX) as i32;
    t.overrun_last.store(ov, Ordering::Release);
    t.queued.store(false, Ordering::Release);
    info.set_i32(20, ov);
}

fn timer_of(t: &Thread, id: u64) -> Result<Arc<PosixTimer>, i64> {
    t.process.ptimers.lock().map.get(&(id as i32)).cloned().ok_or(EINVAL)
}

/// `timer_create(clockid, sevp, timerid)`.
pub fn timer_create(t: &Thread, clk: u64, sevp: u64, idp: u64) -> SysResult {
    let clock = match cpu_clock(t, clk)? {
        Some(CpuClock::Process(p)) => Clock::CpuProcess(Arc::downgrade(&p)),
        Some(CpuClock::Thread(th)) => Clock::CpuThread(Arc::downgrade(&th)),
        None => match clk as u32 as i32 {
            0 | 11 => Clock::Wall { realtime: true },
            1 | 7 => Clock::Wall { realtime: false },
            // Alarmes (réveil du système) : `CAP_WAKE_ALARM`.
            8 | 9 => {
                if t.process.creds.lock().euid != 0 {
                    return Err(EPERM);
                }
                Clock::Wall { realtime: clk == 8 }
            }
            // Horloges sans minuteur (`MONOTONIC_RAW`, `*_COARSE`) : `EOPNOTSUPP`, comme Linux.
            4..=6 => return Err(EOPNOTSUPP),
            _ => return Err(EINVAL),
        },
    };
    // `sigevent` : valeur (8), numéro (4), mode (4), fil (4) ; `compat_sigevent` d'i386 :
    // valeur (4), numéro, mode, fil (4 chacun).
    let (value, signo, notify, tid) = if sevp == 0 {
        (None, SIGALRM, SIGEV_SIGNAL, 0)
    } else if crate::linux::compat::active(t) {
        let raw = t.mm().read_user::<[u32; 4]>(sevp as usize).map_err(fault)?;
        (Some(raw[0] as u64), raw[1] as i32, raw[2] as i32, raw[3] as i32)
    } else {
        let raw = t.mm().read_user::<[u8; 20]>(sevp as usize).map_err(fault)?;
        let i = |o: usize| i32::from_le_bytes(raw[o..o + 4].try_into().unwrap());
        (Some(u64::from_le_bytes(raw[0..8].try_into().unwrap())), i(8), i(12), i(16))
    };
    let target = match notify {
        SIGEV_SIGNAL | SIGEV_THREAD | SIGEV_NONE => Target::Process,
        SIGEV_THREAD_ID => {
            let th = crate::find_thread_v(t, tid as usize).filter(|th| Arc::ptr_eq(&th.process, &t.process)).ok_or(EINVAL)?;
            Target::Thread(Arc::downgrade(&th))
        }
        _ => return Err(EINVAL),
    };
    if notify != SIGEV_NONE && !(1..=SIGRTMAX).contains(&signo) {
        return Err(EINVAL);
    }
    let timer = {
        let mut tm = t.process.ptimers.lock();
        if tm.map.len() >= MAX_TIMERS {
            return Err(EAGAIN);
        }
        let mut id = tm.next;
        while tm.map.contains_key(&id) {
            id = id.checked_add(1).unwrap_or(0);
        }
        tm.next = id.checked_add(1).unwrap_or(0);
        let timer = Arc::new(PosixTimer {
            id,
            clock,
            signo: (notify != SIGEV_NONE).then_some(signo),
            // Sans `sigevent`, la valeur est l'identifiant du minuteur (Linux).
            value: value.unwrap_or(id as u64),
            target,
            process: Arc::downgrade(&t.process),
            state: IrqSafeMutex::new(State { gen: 0, deadline: None, interval: Duration::ZERO, cpu_expires: 0, cpu_interval: 0 }),
            queued: AtomicBool::new(false),
            overrun: AtomicU32::new(0),
            overrun_last: AtomicI32::new(0),
        });
        tm.map.insert(id, timer.clone());
        timer
    };
    if let Err(e) = t.mm().write_user::<i32>(idp as usize, &timer.id) {
        t.process.ptimers.lock().map.remove(&timer.id);
        return Err(fault(e));
    }
    Ok(0)
}

fn read_itimerspec(t: &Thread, p: u64) -> Result<(Duration, Duration), i64> {
    // `itimerspec` i386 « time32 » (`timer_settime`, `timerfd_settime`) : 4 × 32 bits.
    let v = crate::linux::compat::get_itimerspec(t, p)?;
    let ts = |s: i64, ns: i64| {
        if s < 0 || !(0..1_000_000_000).contains(&ns) {
            Err(EINVAL)
        } else {
            Ok(Duration::new(s as u64, ns as u32))
        }
    };
    Ok((ts(v[0], v[1])?, ts(v[2], v[3])?))
}

fn write_itimerspec(t: &Thread, p: u64, left: Duration, iv: Duration) -> Result<(), i64> {
    let v = [iv.as_secs() as i64, iv.subsec_nanos() as i64, left.as_secs() as i64, left.subsec_nanos() as i64];
    crate::linux::compat::put_itimerspec(t, p, v)
}

/// `timer_settime(timerid, flags, new, old)`.
pub fn timer_settime(t: &Thread, id: u64, flags: u64, new: u64, old: u64) -> SysResult {
    let timer = timer_of(t, id)?;
    if new == 0 {
        return Err(EINVAL);
    }
    let (interval, value) = read_itimerspec(t, new)?;
    if old != 0 {
        let (left, iv) = timer.current();
        write_itimerspec(t, old, left, iv)?;
    }
    let abs = flags & TIMER_ABSTIME != 0;
    timer.overrun.store(0, Ordering::Release);
    timer.overrun_last.store(0, Ordering::Release);
    match &timer.clock {
        Clock::Wall { realtime } => {
            let arm = {
                let mut s = timer.state.lock();
                s.gen += 1;
                s.interval = interval;
                s.deadline = if value.is_zero() {
                    None
                } else if abs {
                    Some(instant_from_clock(value, *realtime))
                } else {
                    Some(Instant::now() + value)
                };
                s.deadline.map(|d| (s.gen, d))
            };
            if let Some((gen, at)) = arm {
                timer.arm_wall(gen, at);
            }
        }
        Clock::CpuProcess(_) | Clock::CpuThread(_) => {
            let now = timer.clock.cpu_ns().ok_or(EINVAL)?;
            let armed = {
                let mut s = timer.state.lock();
                s.gen += 1;
                s.cpu_interval = interval.as_nanos() as u64;
                s.cpu_expires = if value.is_zero() {
                    0
                } else if abs {
                    (value.as_nanos() as u64).max(1)
                } else {
                    now + value.as_nanos() as u64
                };
                s.cpu_expires != 0
            };
            if armed {
                crate::cputimer::arm_posix(&timer);
            }
        }
    }
    Ok(0)
}

/// `timer_gettime(timerid, cur)`.
pub fn timer_gettime(t: &Thread, id: u64, cur: u64) -> SysResult {
    let timer = timer_of(t, id)?;
    let (left, iv) = timer.current();
    write_itimerspec(t, cur, left, iv)?;
    Ok(0)
}

/// `timer_getoverrun(timerid)`.
pub fn timer_getoverrun(t: &Thread, id: u64) -> SysResult {
    Ok(timer_of(t, id)?.overrun_last.load(Ordering::Acquire) as i64)
}

/// `timer_delete(timerid)`.
pub fn timer_delete(t: &Thread, id: u64) -> SysResult {
    let timer = t.process.ptimers.lock().map.remove(&(id as i32)).ok_or(EINVAL)?;
    timer.disarm();
    Ok(0)
}
