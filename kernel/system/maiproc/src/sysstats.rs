//! Statistiques des appels système et des fautes de page (`/proc/maios/syscalls`), l'équivalent
//! de `perf trace -s` / `strace -c` pour tout le système.
//!
//! Éteintes par défaut (une lecture atomique par appel). `echo on > /proc/maios/syscalls`
//! les allume, `off` les éteint, `reset` les remet à zéro ; la lecture donne, par appel, le
//! nombre, le temps total, moyen et maximal (temps mural entre l'entrée et la sortie de
//! l'appel, attentes comprises), triés par temps total, puis les fautes de page des processus.
//!
//! Compteurs par cœur (pas de ligne de cache disputée entre cœurs) : chaque cœur n'écrit que
//! les siens, la lecture les additionne.

use alloc::{format, string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use sync_preemption::PreemptionSafeMutex as Mutex;

use crate::linux::errno::*;
use crate::object::{object_any, FileObject};
use crate::wait::WaitQueue;
use crate::Thread;

static ENABLED: AtomicBool = AtomicBool::new(false);

const NR_MAX: usize = 512;
/// Rangée des fautes de page.
const FAULT: usize = NR_MAX;
/// Phases de la vie d'un processus (mesurées pour tous les processus Linux) : du `fork` à la
/// première instruction de l'enfant, de l'`exit` à l'état zombie, de l'état zombie au `wait4` qui
/// le recueille, vie entière, temps CPU consommé sur cette vie.
pub const PH_START: usize = NR_MAX + 1;
pub const PH_EXIT: usize = NR_MAX + 2;
pub const PH_REAP: usize = NR_MAX + 3;
pub const PH_LIFE: usize = NR_MAX + 4;
pub const PH_CPU: usize = NR_MAX + 5;
/// Détail de la sortie : de la demande au début de `finish_thread` (retour du fil vers sa tâche),
/// démontage (fichiers, mémoire, enfants), ligne de journal du `run -s`.
pub const PH_EXIT_RET: usize = NR_MAX + 6;
pub const PH_EXIT_TEAR: usize = NR_MAX + 7;
pub const PH_EXIT_LOG: usize = NR_MAX + 8;
const ROWS: usize = NR_MAX + 9;
const CPUS: usize = 16;

struct Row {
    count: AtomicU64,
    ns: AtomicU64,
    max: AtomicU64,
}

#[allow(clippy::declare_interior_mutable_const)]
const ROW: Row = Row { count: AtomicU64::new(0), ns: AtomicU64::new(0), max: AtomicU64::new(0) };
#[allow(clippy::declare_interior_mutable_const)]
const CPU_ROWS: [Row; ROWS] = [ROW; ROWS];
static STATS: [[Row; ROWS]; CPUS] = [CPU_ROWS; CPUS];

#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

#[inline]
pub fn now_ns() -> u64 {
    time::Instant::now().duration_since(time::Instant::ZERO).as_nanos() as u64
}

fn record_row(row: usize, ns: u64) {
    let cpu = (cpu::current_cpu().value() as usize) % CPUS;
    let r = &STATS[cpu][row];
    // Un seul écrivain par cœur (préemption possible entre deux opérations : au pire une
    // mesure attribuée au mauvais cœur, jamais perdue, car les opérations sont atomiques).
    r.count.fetch_add(1, Ordering::Relaxed);
    r.ns.fetch_add(ns, Ordering::Relaxed);
    r.max.fetch_max(ns, Ordering::Relaxed);
}

/// Fin d'un appel système `nr` commencé à `start` (ns).
#[inline]
pub fn record_syscall(nr: u64, start: u64) {
    record_row((nr as usize).min(NR_MAX - 1), now_ns().saturating_sub(start));
}

/// Phase `row` (`PH_*`) de durée `ns`.
#[inline]
pub fn record_phase(row: usize, ns: u64) {
    if enabled() {
        record_row(row, ns);
    }
}

/// Fin d'une faute de page commencée à `start`.
#[inline]
pub fn record_fault(start: u64) {
    record_row(FAULT, now_ns().saturating_sub(start));
}

fn reset() {
    for cpu in STATS.iter() {
        for r in cpu.iter() {
            r.count.store(0, Ordering::Relaxed);
            r.ns.store(0, Ordering::Relaxed);
            r.max.store(0, Ordering::Relaxed);
        }
    }
}

/// Noms des appels les plus courants (x86-64).
fn name(nr: usize) -> &'static str {
    match nr {
        0 => "read", 1 => "write", 2 => "open", 3 => "close", 4 => "stat", 5 => "fstat", 6 => "lstat",
        7 => "poll", 8 => "lseek", 9 => "mmap", 10 => "mprotect", 11 => "munmap", 12 => "brk",
        13 => "rt_sigaction", 14 => "rt_sigprocmask", 15 => "rt_sigreturn", 16 => "ioctl",
        17 => "pread64", 18 => "pwrite64", 19 => "readv", 20 => "writev", 21 => "access", 22 => "pipe",
        23 => "select", 24 => "sched_yield", 25 => "mremap", 28 => "madvise", 32 => "dup", 33 => "dup2",
        35 => "nanosleep", 39 => "getpid", 41 => "socket", 42 => "connect", 43 => "accept",
        44 => "sendto", 45 => "recvfrom", 56 => "clone", 57 => "fork", 58 => "vfork", 59 => "execve",
        60 => "exit", 61 => "wait4", 62 => "kill", 63 => "uname", 72 => "fcntl", 78 => "getdents",
        79 => "getcwd", 80 => "chdir", 87 => "unlink", 89 => "readlink", 96 => "gettimeofday",
        97 => "getrlimit", 102 => "getuid", 104 => "getgid", 107 => "geteuid", 108 => "getegid",
        110 => "getppid", 158 => "arch_prctl", 186 => "gettid", 202 => "futex", 217 => "getdents64",
        218 => "set_tid_address", 228 => "clock_gettime", 230 => "clock_nanosleep", 231 => "exit_group",
        232 => "epoll_wait", 233 => "epoll_ctl", 257 => "openat", 262 => "newfstatat", 263 => "unlinkat",
        267 => "readlinkat", 270 => "pselect6", 271 => "ppoll", 273 => "set_robust_list",
        281 => "epoll_pwait", 290 => "eventfd2", 291 => "epoll_create1", 292 => "dup3", 293 => "pipe2",
        302 => "prlimit64", 318 => "getrandom", 332 => "statx", 334 => "rseq", 435 => "clone3",
        436 => "close_range", 439 => "faccessat2",
        _ => "",
    }
}

/// Rapport (texte).
fn report() -> String {
    struct Line {
        row: usize,
        count: u64,
        ns: u64,
        max: u64,
    }
    let mut lines: Vec<Line> = (0..ROWS)
        .map(|row| {
            let (mut count, mut ns, mut max) = (0, 0, 0);
            for cpu in STATS.iter() {
                count += cpu[row].count.load(Ordering::Relaxed);
                ns += cpu[row].ns.load(Ordering::Relaxed);
                max = max.max(cpu[row].max.load(Ordering::Relaxed));
            }
            Line { row, count, ns, max }
        })
        .filter(|l| l.count > 0)
        .collect();
    lines.sort_by(|a, b| b.ns.cmp(&a.ns));
    let mut out = format!("# maios : appels systeme ({})\n", if enabled() { "on" } else { "off" });
    out.push_str("#  nr nom                  appels     total_us   moyen_ns     max_ns\n");
    for l in &lines {
        let label = match l.row {
            FAULT => String::from("[faute de page]"),
            PH_START => String::from("[fork->1er pas]"),
            PH_EXIT => String::from("[exit->zombie]"),
            PH_REAP => String::from("[zombie->wait4]"),
            PH_LIFE => String::from("[vie entiere]"),
            PH_CPU => String::from("[cpu de la vie]"),
            PH_EXIT_RET => String::from("[exit:retour fil]"),
            PH_EXIT_TEAR => String::from("[exit:demontage]"),
            PH_EXIT_LOG => String::from("[exit:journal]"),
            r => String::from(name(r)),
        };
        let nr = if l.row >= NR_MAX { String::from("-") } else { format!("{}", l.row) };
        out.push_str(&format!(
            "{:>5} {:<18} {:>8} {:>12} {:>10} {:>10}\n",
            nr,
            label,
            l.count,
            l.ns / 1000,
            l.ns / l.count.max(1),
            l.max
        ));
    }
    out
}

/// `/proc/maios/syscalls`, une ouverture.
struct StatsFile {
    /// Rapport figé à la première lecture, et position.
    snap: Mutex<Option<(Vec<u8>, usize)>>,
}

impl FileObject for StatsFile {
    object_any!();

    fn link_name(&self) -> String {
        String::from("/proc/maios/syscalls")
    }

    fn stat(&self) -> (u32, u64) {
        (0o100644, 0x6d61_6900)
    }

    fn read(&self, _t: &Thread, kb: &mut [u8], _nonblock: bool) -> Result<usize, i64> {
        let mut g = self.snap.lock();
        let (data, pos) = g.get_or_insert_with(|| (report().into_bytes(), 0));
        let n = kb.len().min(data.len() - *pos);
        kb[..n].copy_from_slice(&data[*pos..*pos + n]);
        *pos += n;
        Ok(n)
    }

    fn write(&self, t: &Thread, kb: &[u8], _nonblock: bool) -> Result<usize, i64> {
        if t.process.creds.lock().euid != 0 {
            return Err(EPERM);
        }
        match core::str::from_utf8(kb).map(|s| s.trim()) {
            Ok("on") | Ok("1") => ENABLED.store(true, Ordering::Relaxed),
            Ok("off") | Ok("0") => ENABLED.store(false, Ordering::Relaxed),
            Ok("reset") => reset(),
            _ => return Err(EINVAL),
        }
        Ok(kb.len())
    }

    fn poll(&self, _t: Option<&Thread>) -> u32 {
        crate::object::ev::READABLE | crate::object::ev::WRITABLE
    }

    fn queues<'a>(&'a self, _cb: &mut dyn FnMut(&'a WaitQueue)) {}
}

/// Nœud de `/proc/maios/syscalls` (une nouvelle ouverture à chaque appel).
pub fn proc_node(abs: &str) -> Option<crate::fd::Node> {
    (abs == "/proc/maios/syscalls").then(|| crate::fd::Node::Object(Arc::new(StatsFile { snap: Mutex::new(None) })))
}
