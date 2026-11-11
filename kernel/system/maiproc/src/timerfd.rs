//! `timerfd` : minuteurs lus comme des fichiers (nombre d'échéances écoulées), surveillables par
//! `epoll`. Armés sur les minuteurs du noyau (`sleep::future`, appelés depuis l'interruption de
//! l'horloge) ; un réarmement change la génération, ce qui rend caduc le réveil précédent.

use alloc::{format, string::String, sync::{Arc, Weak}};
use core::time::Duration;
use sync_irq::IrqSafeMutex;
use time::Instant;

use crate::fd::{Node, OpenFile, O_NONBLOCK, O_RDWR};
use crate::linux::errno::*;
use crate::linux::SysResult;
use crate::object::{ev, new_ino, object_any, wait_errno, FileObject};
use crate::wait::{wait_event, WaitQueue};
use crate::Thread;

const CLOCK_REALTIME: u64 = 0;
const CLOCK_MONOTONIC: u64 = 1;
const CLOCK_BOOTTIME: u64 = 7;
const CLOCK_REALTIME_ALARM: u64 = 8;
const CLOCK_BOOTTIME_ALARM: u64 = 9;

const TFD_NONBLOCK: u64 = 0x800;
const TFD_CLOEXEC: u64 = 0x80000;
const TFD_TIMER_ABSTIME: u64 = 1;
const TFD_TIMER_CANCEL_ON_SET: u64 = 2;

struct State {
    gen: u64,
    /// Prochaine échéance (horloge monotone du noyau).
    deadline: Option<Instant>,
    interval: Duration,
    /// Échéances écoulées non lues.
    ticks: u64,
}

pub struct TimerFd {
    realtime: bool,
    state: IrqSafeMutex<State>,
    wq: WaitQueue,
    ino: u64,
}

struct TimerWaker {
    timer: Weak<TimerFd>,
    gen: u64,
}

impl alloc::task::Wake for TimerWaker {
    fn wake(self: Arc<Self>) {
        if let Some(t) = self.timer.upgrade() {
            t.fire(self.gen);
        }
    }
}

impl TimerFd {
    fn arm(self: &Arc<Self>, gen: u64, at: Instant) {
        let w = Arc::new(TimerWaker { timer: Arc::downgrade(self), gen });
        let now = Instant::now();
        let after = if at > now { at - now } else { Duration::ZERO };
        sleep::future::sleep(after, core::task::Waker::from(w));
    }

    /// Échéance (depuis l'interruption de l'horloge).
    fn fire(self: &Arc<Self>, gen: u64) {
        let rearm = {
            let mut s = self.state.lock();
            let Some(d) = s.deadline else { return };
            if s.gen != gen {
                return;
            }
            let now = Instant::now();
            if now < d {
                // Réveil en avance (granularité de l'horloge) : on réarme sans compter.
                Some(d)
            } else if s.interval.is_zero() {
                s.ticks += 1;
                s.deadline = None;
                None
            } else {
                // Échéances manquées comptées d'un coup (comme `hrtimer_forward`).
                let late = now.duration_since(d).as_nanos();
                let n = 1 + (late / s.interval.as_nanos()) as u64;
                s.ticks = s.ticks.saturating_add(n);
                let next = d + Duration::from_nanos((s.interval.as_nanos() as u64).saturating_mul(n));
                s.deadline = Some(next);
                Some(next)
            }
        };
        if let Some(at) = rearm {
            self.arm(gen, at);
        }
        self.wq.wake_all();
    }

    /// `(reste, intervalle)`.
    fn current(&self) -> (Duration, Duration) {
        let s = self.state.lock();
        let left = s.deadline.map_or(Duration::ZERO, |d| {
            let now = Instant::now();
            if d > now { d - now } else { Duration::from_nanos(1) }
        });
        (left, s.interval)
    }
}

impl FileObject for TimerFd {
    object_any!();

    fn link_name(&self) -> String {
        String::from("anon_inode:[timerfd]")
    }

    fn stat(&self) -> (u32, u64) {
        (0o600, self.ino)
    }

    fn read(&self, t: &Thread, kb: &mut [u8], nonblock: bool) -> Result<usize, i64> {
        if kb.len() < 8 {
            return Err(EINVAL);
        }
        let n = wait_event(&[&self.wq], None, Some(t), || {
            let mut s = self.state.lock();
            if s.ticks > 0 {
                return Some(Ok(core::mem::take(&mut s.ticks)));
            }
            nonblock.then_some(Err(EAGAIN))
        })
        .map_err(wait_errno)??;
        kb[..8].copy_from_slice(&n.to_le_bytes());
        Ok(8)
    }

    fn poll(&self, _t: Option<&Thread>) -> u32 {
        if self.state.lock().ticks > 0 { ev::READABLE } else { 0 }
    }

    fn queues<'a>(&'a self, cb: &mut dyn FnMut(&'a WaitQueue)) {
        cb(&self.wq);
    }

    fn fdinfo(&self) -> String {
        let (left, iv) = self.current();
        let ticks = self.state.lock().ticks;
        format!(
            "clockid: {}\nticks: {}\nit_value: ({}, {})\nit_interval: ({}, {})\n",
            if self.realtime { 0 } else { 1 },
            ticks,
            left.as_secs(),
            left.subsec_nanos(),
            iv.as_secs(),
            iv.subsec_nanos()
        )
    }
}

pub fn timerfd_create(t: &Thread, clock: u64, flags: u64) -> SysResult {
    if flags & !(TFD_NONBLOCK | TFD_CLOEXEC) != 0 {
        return Err(EINVAL);
    }
    let realtime = match clock {
        CLOCK_REALTIME | CLOCK_REALTIME_ALARM => true,
        CLOCK_MONOTONIC | CLOCK_BOOTTIME | CLOCK_BOOTTIME_ALARM => false,
        _ => return Err(EINVAL),
    };
    let obj = Arc::new(TimerFd {
        realtime,
        state: IrqSafeMutex::new(State { gen: 0, deadline: None, interval: Duration::ZERO, ticks: 0 }),
        wq: WaitQueue::new(),
        ino: new_ino(),
    });
    let status = O_RDWR | if flags & TFD_NONBLOCK != 0 { O_NONBLOCK } else { 0 };
    let file = OpenFile::new(Node::Object(obj), status);
    t.process.files.lock().alloc(file, 0, flags & TFD_CLOEXEC != 0).map(|fd| fd as i64).ok_or(EMFILE)
}

fn timer_of(t: &Thread, fd: u64) -> Result<Arc<TimerFd>, i64> {
    let f = crate::linux::fs::get_fd(t, fd)?;
    let o = f.object().ok_or(EINVAL)?;
    crate::object::downcast_arc::<TimerFd>(o).ok_or(EINVAL)
}

/// Lit un `itimerspec` : (intervalle, valeur).
fn read_itimerspec(t: &Thread, p: u64) -> Result<(Duration, Duration), i64> {
    // `itimerspec` i386 « time32 » (`timer_settime`, `timerfd_settime`) : 4 × 32 bits.
    let v = crate::linux::compat::get_itimerspec(t, p)?;
    let d = |s: i64, ns: i64| -> Result<Duration, i64> {
        if s < 0 || !(0..1_000_000_000).contains(&ns) {
            return Err(EINVAL);
        }
        Ok(Duration::new(s as u64, ns as u32))
    };
    Ok((d(v[0], v[1])?, d(v[2], v[3])?))
}

fn write_itimerspec(t: &Thread, p: u64, left: Duration, iv: Duration) -> Result<(), i64> {
    let v: [i64; 4] = [iv.as_secs() as i64, iv.subsec_nanos() as i64, left.as_secs() as i64, left.subsec_nanos() as i64];
    crate::linux::compat::put_itimerspec(t, p, v)
}

pub fn timerfd_settime(t: &Thread, fd: u64, flags: u64, new: u64, old: u64) -> SysResult {
    if flags & !(TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET) != 0 {
        return Err(EINVAL);
    }
    let tm = timer_of(t, fd)?;
    let (interval, value) = read_itimerspec(t, new)?;
    if old != 0 {
        let (l, i) = tm.current();
        write_itimerspec(t, old, l, i)?;
    }
    let deadline = if value.is_zero() {
        None
    } else if flags & TFD_TIMER_ABSTIME != 0 {
        Some(crate::linux::sys::instant_from_clock(value, tm.realtime))
    } else {
        Some(Instant::now() + value)
    };
    let gen = {
        let mut s = tm.state.lock();
        s.gen += 1;
        s.deadline = deadline;
        s.interval = interval;
        s.ticks = 0;
        s.gen
    };
    if let Some(d) = deadline {
        tm.arm(gen, d);
    }
    Ok(0)
}

pub fn timerfd_gettime(t: &Thread, fd: u64, cur: u64) -> SysResult {
    let tm = timer_of(t, fd)?;
    let (l, i) = tm.current();
    write_itimerspec(t, cur, l, i)?;
    Ok(0)
}
