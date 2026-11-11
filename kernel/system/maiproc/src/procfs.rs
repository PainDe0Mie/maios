//! `/proc` et une partie de `/sys`, produits à la lecture (aucun fichier stocké).
//!
//! Arborescence de Linux, dans les formats que lisent `ps`, `top`, `htop`, la glibc
//! (`sysconf(_SC_NPROCESSORS_ONLN)`) et les environnements d'exécution (Go, JVM, Node) :
//! - `/proc/<pid>/` : `stat`, `status`, `statm`, `cmdline`, `environ`, `comm`, `maps`, `exe`,
//!   `cwd`, `root`, `fd/`, `fdinfo/`, `task/<tid>/`, `limits`, `cgroup`, `oom_score_adj` ;
//! - `/proc/self` et `/proc/thread-self` (liens), `/proc/stat`, `/proc/loadavg`, `/proc/meminfo`,
//!   `/proc/cpuinfo`, `/proc/uptime`, `/proc/version`, `/proc/mounts`, `/proc/filesystems`,
//!   `/proc/sys/kernel/*` ;
//! - `/sys/devices/system/cpu/{online,possible,present}`.
//!
//! Les liens (`exe`, `cwd`, `fd/<n>`) ne sont pas des liens du VFS : [`canonical`] les suit dans
//! les chemins, [`readlink`] les lit, et un `fd/<n>` ouvert rend la description de fichier ouverte
//! du processus visé (comme le « magic link » de Linux).
//!
//! Chaque montage `proc` montre un espace pid (`ns`, celui de son superbloc) : les numéros des
//! dossiers, `self`, `thread-self` et ceux des fichiers (`stat`, `status`, `task/`) sont ceux de
//! cet espace ; un processus qui n'y a pas de numéro n'y apparaît pas.

use alloc::{format, string::String, sync::Arc, vec::Vec};
use core::sync::atomic::Ordering;

use crate::fd::{Node, OpenFile};
use spin::Mutex;
use crate::pidns::PidNs;
use crate::{find_process, Process, Thread};

pub const DT_DIR: u8 = 4;
pub const DT_REG: u8 = 8;
pub const DT_LNK: u8 = 10;

/// Nœud de `/proc`.
pub enum ProcNode {
    File(Vec<u8>),
    Dir(Vec<(String, u8)>),
    /// Lien vers une description ouverte (`/proc/<pid>/fd/<n>`).
    Fd(Arc<OpenFile>),
    /// Fichier inscriptible (`oom_score_adj`).
    Node(Node),
}

/// Chemin sous `/proc` ou `/sys` ?
pub fn handles(abs: &str) -> bool {
    abs == "/proc" || abs.starts_with("/proc/") || abs == "/sys" || abs.starts_with("/sys/")
}

/// Remplace `/proc/self`, `/proc/thread-self` et les liens `exe`, `cwd`, `root` d'un processus
/// par leur cible (au plus 8 fois).
pub fn canonical(t: &Thread, ns: &PidNs, abs: &str) -> String {
    let mut p = String::from(abs);
    // `self` d'un processus invisible dans cet espace ne mène nulle part (`ENOENT`).
    let me = ns.nr_of(t.process.pid);
    let me_t = ns.nr_of(t.tid());
    for _ in 0..8 {
        let next = if let Some(rest) = p.strip_prefix("/proc/self") {
            (me != 0 && (rest.is_empty() || rest.starts_with('/'))).then(|| format!("/proc/{}{}", me, rest))
        } else if let Some(rest) = p.strip_prefix("/proc/thread-self") {
            (me_t != 0 && (rest.is_empty() || rest.starts_with('/'))).then(|| format!("/proc/{}/task/{}{}", me, me_t, rest))
        } else {
            follow_process_link(ns, &p)
        };
        match next {
            Some(n) => p = n,
            None => break,
        }
    }
    p
}

/// Liens de `/proc` que traverse `c` (chemin sous `/proc`, tel que résolu jusqu'au montage,
/// pas encore passé par [`canonical`]) ; le dernier composant n'est traversé que si
/// `follow_last`. Rend (un lien ordinaire `self`/`thread-self` est traversé, préfixe du premier
/// lien magique traversé : `exe`, `cwd`, `root`, `fd/<n>`, `ns/<x>`, `map_files/<x>` d'un
/// processus ou d'un fil). Pour `openat2` (`RESOLVE_NO_SYMLINKS`, `RESOLVE_NO_MAGICLINKS`).
pub fn links_in(c: &str, follow_last: bool) -> (bool, Option<String>) {
    let parts: Vec<&str> = match c.strip_prefix("/proc") {
        Some(r) if r.is_empty() || r.starts_with('/') => r.split('/').filter(|s| !s.is_empty()).collect(),
        _ => return (false, None),
    };
    let crossed = |i: usize| i + 1 < parts.len() || follow_last;
    let num = |i: usize| parts.get(i).map_or(false, |s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()));
    let mut symlink = false;
    let mut i = match parts.first() {
        Some(&"self") | Some(&"thread-self") => {
            symlink = crossed(0);
            1
        }
        Some(_) if num(0) => 1,
        _ => return (false, None),
    };
    if parts[0] != "thread-self" && parts.get(i) == Some(&"task") && num(i + 1) {
        i += 2;
    }
    let end = match parts.get(i) {
        Some(&"exe") | Some(&"cwd") | Some(&"root") => i + 1,
        Some(&"fd") | Some(&"ns") | Some(&"map_files") if parts.len() > i + 1 => i + 2,
        _ => return (symlink, None),
    };
    if !crossed(end - 1) {
        return (symlink, None);
    }
    (symlink, Some(format!("/proc/{}", parts[..end].join("/"))))
}

/// Chemin désigné par le lien magique `link` (préfixe rendu par [`links_in`]) : `exe`, `cwd`,
/// `root`, ou `fd/<n>` d'un fichier ou d'un dossier. `None` pour un objet sans chemin.
pub fn magic_target(t: &Thread, ns: &PidNs, link: &str) -> Option<String> {
    let c = canonical_dirs(t, ns, link);
    let parts: Vec<&str> = c.strip_prefix("/proc/")?.split('/').collect();
    let p = process_in(ns, parts.first()?.parse().ok()?)?;
    match parts.as_slice() {
        [.., "exe"] => Some(p.exe.lock().clone()),
        [.., "cwd"] => Some(p.cwd_path()),
        [.., "root"] => Some(proc_root(&p)),
        [.., "fd", n] => {
            let f = p.files.lock().get(n.parse().ok()?)?;
            match &f.node {
                Node::File { path, .. } | Node::Dir { path, .. } | Node::Generated { path, .. } | Node::VirtDir { path, .. } => Some(path.clone()),
                Node::Object(o) => o.as_any().downcast_ref::<crate::linux::fs::PathRef>().map(|r| String::from(r.path())),
                _ => None,
            }
        }
        _ => None,
    }
}

/// `/proc/<pid>/{exe,cwd,root}[/reste]` → cible.
fn follow_process_link(ns: &PidNs, p: &str) -> Option<String> {
    let rest = p.strip_prefix("/proc/")?;
    let mut parts = rest.splitn(3, '/');
    let pid: usize = parts.next()?.parse().ok()?;
    let what = parts.next()?;
    let tail = parts.next();
    let proc_ = process_in(ns, pid)?;
    let target = match what {
        "exe" if tail.is_none() => proc_.exe.lock().clone(),
        "cwd" => proc_.cwd_path(),
        "root" => proc_root(&proc_),
        _ => return None,
    };
    Some(match tail {
        Some(t) => format!("{}/{}", target.trim_end_matches('/'), t),
        None => target,
    })
}

/// Cible d'un lien de `/proc` (`readlink`).
pub fn readlink(t: &Thread, ns: &PidNs, abs: &str) -> Option<String> {
    if abs == "/proc/self" {
        return match ns.nr_of(t.process.pid) {
            0 => None,
            n => Some(format!("{}", n)),
        };
    }
    if abs == "/proc/thread-self" {
        return match (ns.nr_of(t.process.pid), ns.nr_of(t.tid())) {
            (0, _) | (_, 0) => None,
            (p, th) => Some(format!("{}/task/{}", p, th)),
        };
    }
    let abs = canonical_dirs(t, ns, abs);
    let rest = abs.strip_prefix("/proc/")?;
    let parts: Vec<&str> = rest.split('/').collect();
    let pid: usize = parts.first()?.parse().ok()?;
    let p = process_in(ns, pid)?;
    match parts.as_slice() {
        [_, "exe"] => Some(p.exe.lock().clone()),
        [_, "cwd"] => Some(p.cwd_path()),
        [_, "root"] => Some(proc_root(&p)),
        [_, "fd", n] => {
            let fd: usize = n.parse().ok()?;
            let f = p.files.lock().get(fd)?;
            Some(link_name(&f))
        }
        [_, "ns", ns] => crate::ns::ns_of(&p, ns).map(|r| crate::ns::link_text(&r)),
        _ => None,
    }
}

/// Racine d'un processus vue de celle du processus courant (`/proc/<pid>/root`).
fn proc_root(p: &Process) -> String {
    let viewer = crate::current_process().map(|c| c.fs.root());
    match viewer {
        Some(v) => crate::mount::display_path(&p.fs.root(), &v),
        None => String::from("/"),
    }
}

/// Comme [`canonical`] mais sans suivre le dernier composant (pour `readlink`).
pub(crate) fn canonical_dirs(t: &Thread, ns: &PidNs, abs: &str) -> String {
    match abs.rsplit_once('/') {
        Some((dir, last)) if !dir.is_empty() => format!("{}/{}", canonical(t, ns, dir), last),
        _ => String::from(abs),
    }
}

/// Processus de numéro `nr` dans l'espace `ns` (dossier `/proc/<nr>`).
fn process_in(ns: &PidNs, nr: usize) -> Option<Arc<Process>> {
    ns.global_of(nr).and_then(find_process)
}

/// Numéros (`NSpid:`…) d'un pid vus depuis `ns` jusqu'à son propre espace.
fn ns_list(r: Option<Arc<crate::pidns::Pid>>, ns: &PidNs) -> String {
    match r {
        Some(r) => {
            let v = r.numbers_from(ns);
            if v.is_empty() {
                String::from("0")
            } else {
                v.iter().map(|n| format!("{}", n)).collect::<Vec<_>>().join("\t")
            }
        }
        None => String::from("0"),
    }
}

/// Nom d'une description ouverte (`/proc/<pid>/fd/<n>`).
pub fn link_name(f: &OpenFile) -> String {
    match &f.node {
        Node::File { path, .. } | Node::Dir { path, .. } | Node::Generated { path, .. } | Node::VirtDir { path, .. } => path.clone(),
        Node::Console { .. } => String::from("/dev/pts/0"),
        Node::Null => String::from("/dev/null"),
        Node::Zero => String::from("/dev/zero"),
        Node::Random => String::from("/dev/urandom"),
        Node::Pipe(end) => format!("pipe:[{}]", end.pipe.ino),
        Node::Object(o) => o.link_name(),
    }
}

/// `/proc/<pid>/oom_score_adj`, une ouverture : lu et écrit comme sous Linux (−1000 à 1000 ;
/// baisser la valeur, ou toucher un processus d'un autre utilisateur, demande root).
struct OomAdj {
    p: alloc::sync::Weak<Process>,
    pos: Mutex<usize>,
}

impl crate::object::FileObject for OomAdj {
    crate::object::object_any!();

    fn link_name(&self) -> String {
        self.p.upgrade().map_or(String::from("/proc/oom_score_adj"), |p| format!("/proc/{}/oom_score_adj", p.pid))
    }

    fn stat(&self) -> (u32, u64) {
        (0o100644, 0x6d61_6902)
    }

    fn read(&self, _t: &Thread, kb: &mut [u8], _nonblock: bool) -> Result<usize, i64> {
        let p = self.p.upgrade().ok_or(crate::linux::errno::ESRCH)?;
        let text = format!("{}\n", p.oom_score_adj.load(Ordering::Relaxed));
        let mut pos = self.pos.lock();
        let rest = text.as_bytes().get(*pos..).unwrap_or(&[]);
        let n = rest.len().min(kb.len());
        kb[..n].copy_from_slice(&rest[..n]);
        *pos += n;
        Ok(n)
    }

    fn write(&self, t: &Thread, kb: &[u8], _nonblock: bool) -> Result<usize, i64> {
        use crate::linux::errno::{EACCES, EINVAL, ESRCH};
        let p = self.p.upgrade().ok_or(ESRCH)?;
        let v: i32 = core::str::from_utf8(kb).ok().and_then(|s| s.trim().parse().ok()).ok_or(EINVAL)?;
        if !(-1000..=1000).contains(&v) {
            return Err(EINVAL);
        }
        // Un verrou à la fois : `p` est souvent l'appelant lui-même (verrou non réentrant).
        let (euid, privileged) = {
            let c = t.process.creds.lock();
            (c.euid, crate::userns::capable(&c, crate::userns::CAP_SYS_RESOURCE))
        };
        let other = p.creds.lock().ruid;
        if !privileged && (euid != other || v < p.oom_score_adj.load(Ordering::Relaxed)) {
            return Err(EACCES);
        }
        p.oom_score_adj.store(v, Ordering::Relaxed);
        Ok(kb.len())
    }

    fn poll(&self, _t: Option<&Thread>) -> u32 {
        crate::object::ev::READABLE | crate::object::ev::WRITABLE
    }

    fn queues<'a>(&'a self, _cb: &mut dyn FnMut(&'a crate::wait::WaitQueue)) {}
}

/// `/proc/sys/kernel/ns_last_pid` : dernier numéro attribué dans l'espace pid de l'appelant ;
/// une écriture fixe le point de départ du prochain (`CAP_SYS_ADMIN` ou
/// `CAP_CHECKPOINT_RESTORE` dans l'espace utilisateur propriétaire).
struct NsLastPid {
    ns: Arc<PidNs>,
    pos: Mutex<usize>,
}

impl crate::object::FileObject for NsLastPid {
    crate::object::object_any!();

    fn link_name(&self) -> String {
        String::from("/proc/sys/kernel/ns_last_pid")
    }

    fn stat(&self) -> (u32, u64) {
        (0o100666, 0x6d61_6903)
    }

    fn read(&self, _t: &Thread, kb: &mut [u8], _nonblock: bool) -> Result<usize, i64> {
        let text = format!("{}\n", self.ns.last_pid());
        let mut pos = self.pos.lock();
        let rest = text.as_bytes().get(*pos..).unwrap_or(&[]);
        let n = rest.len().min(kb.len());
        kb[..n].copy_from_slice(&rest[..n]);
        *pos += n;
        Ok(n)
    }

    fn write(&self, t: &Thread, kb: &[u8], _nonblock: bool) -> Result<usize, i64> {
        use crate::linux::errno::{EINVAL, EPERM};
        let c = t.process.creds.lock().clone();
        if !crate::userns::ns_capable(&c, &self.ns.user_ns, crate::userns::CAP_SYS_ADMIN)
            && !crate::userns::ns_capable(&c, &self.ns.user_ns, crate::userns::CAP_CHECKPOINT_RESTORE)
        {
            return Err(EPERM);
        }
        let v: u32 = core::str::from_utf8(kb).ok().and_then(|s| s.trim().parse().ok()).ok_or(EINVAL)?;
        self.ns.set_last_pid(v)?;
        Ok(kb.len())
    }

    fn poll(&self, _t: Option<&Thread>) -> u32 {
        crate::object::ev::READABLE | crate::object::ev::WRITABLE
    }

    fn queues<'a>(&'a self, _cb: &mut dyn FnMut(&'a crate::wait::WaitQueue)) {}
}

/// Nœud de `abs` (déjà passé par [`canonical`]).
pub fn lookup(t: &Thread, ns: &PidNs, abs: &str) -> Option<ProcNode> {
    let abs = abs.trim_end_matches('/');
    if let Some(rest) = abs.strip_prefix("/sys") {
        return sys(rest);
    }
    let rest = abs.strip_prefix("/proc")?;
    let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    let file = |v: Vec<u8>| Some(ProcNode::File(v));
    match parts.as_slice() {
        [] => {
            let mut e: Vec<(String, u8)> = [
                ("self", DT_LNK),
                ("thread-self", DT_LNK),
                ("meminfo", DT_REG),
                ("cpuinfo", DT_REG),
                ("stat", DT_REG),
                ("loadavg", DT_REG),
                ("uptime", DT_REG),
                ("version", DT_REG),
                ("mounts", DT_REG),
                ("filesystems", DT_REG),
                ("cmdline", DT_REG),
                ("sys", DT_DIR),
                ("sysvipc", DT_DIR),
                ("net", DT_DIR),
            ]
            .iter()
            .map(|(n, d)| (String::from(*n), *d))
            .collect();
            for p in crate::processes_in(ns) {
                e.push((format!("{}", ns.nr_of(p.pid)), DT_DIR));
            }
            Some(ProcNode::Dir(e))
        }
        ["meminfo"] => file(meminfo()),
        ["cpuinfo"] => file(cpuinfo()),
        ["stat"] => file(stat()),
        ["loadavg"] => file(loadavg(ns)),
        ["uptime"] => file(uptime()),
        ["version"] => file(format!("Linux version {} (maios@maios) (rustc) #1 SMP PREEMPT MaiOS\n", crate::linux::sys::RELEASE).into_bytes()),
        ["mounts"] => file(crate::mount::mounts(&t.process)),
        ["filesystems"] => file(b"nodev\tsysfs\nnodev\ttmpfs\nnodev\tproc\nnodev\tdevtmpfs\nnodev\tdevpts\nnodev\tramfs\n\text4\n\tvfat\n".to_vec()),
        ["cmdline"] => file(b"maios\n".to_vec()),
        ["sys", rest @ ..] => sysctl(t, rest),
        ["sysvipc"] => Some(ProcNode::Dir(alloc::vec![(String::from("shm"), DT_REG)])),
        ["sysvipc", "shm"] => file(crate::ipc::proc_shm(t).into_bytes()),
        ["net", rest @ ..] => procnet(&crate::netns::of(&t.process), rest),
        [pid, rest @ ..] => {
            let pid: usize = pid.parse().ok()?;
            let p = process_in(ns, pid)?;
            process_node(t, ns, &p, rest)
        }
    }
}

fn process_node(t: &Thread, ns: &PidNs, p: &Arc<Process>, rest: &[&str]) -> Option<ProcNode> {
    let file = |v: Vec<u8>| Some(ProcNode::File(v));
    match rest {
        [] => Some(ProcNode::Dir(
            [
                ("stat", DT_REG),
                ("status", DT_REG),
                ("statm", DT_REG),
                ("cmdline", DT_REG),
                ("environ", DT_REG),
                ("comm", DT_REG),
                ("maps", DT_REG),
                ("limits", DT_REG),
                ("cgroup", DT_REG),
                ("oom_score_adj", DT_REG),
                ("mounts", DT_REG),
                ("mountinfo", DT_REG),
                ("uid_map", DT_REG),
                ("gid_map", DT_REG),
                ("setgroups", DT_REG),
                ("ns", DT_DIR),
                ("exe", DT_LNK),
                ("cwd", DT_LNK),
                ("root", DT_LNK),
                ("fd", DT_DIR),
                ("fdinfo", DT_DIR),
                ("task", DT_DIR),
            ]
            .iter()
            .map(|(n, d)| (String::from(*n), *d))
            .collect(),
        )),
        ["stat"] => file(proc_stat(ns, p, None)),
        ["status"] => file(status(ns, p, None)),
        ["statm"] => file(statm(p)),
        ["cmdline"] => file(user_range(p, 0)),
        ["environ"] => file(user_range(p, 2)),
        ["comm"] => file(format!("{}\n", p.comm_string()).into_bytes()),
        ["maps"] => file(maps(p)),
        ["mounts"] => file(crate::mount::mounts(p)),
        ["mountinfo"] => file(crate::mount::mountinfo(p)),
        ["uid_map"] | ["gid_map"] | ["setgroups"] => {
            use crate::userns::{MapKind, NsMapFile, NsMapWhat};
            let what = match rest[0] {
                "uid_map" => NsMapWhat::Map(MapKind::Uid),
                "gid_map" => NsMapWhat::Map(MapKind::Gid),
                _ => NsMapWhat::Setgroups,
            };
            let ns = p.creds.lock().user_ns.clone();
            let opener = t.process.creds.lock().clone();
            Some(ProcNode::Node(Node::Object(Arc::new(NsMapFile { ns, what, pid: p.pid, opener, pos: Mutex::new(0) }))))
        }
        ["ns"] => Some(ProcNode::Dir(crate::ns::NS_NAMES.iter().map(|n| (String::from(*n), DT_LNK)).collect())),
        ["ns", name] => crate::ns::open_ns_file(p, name).map(ProcNode::Node),
        ["limits"] => file(limits()),
        ["cgroup"] => file(b"0::/\n".to_vec()),
        ["oom_score_adj"] => Some(ProcNode::Node(Node::Object(Arc::new(OomAdj { p: Arc::downgrade(p), pos: Mutex::new(0) })))),
        // Ancienne échelle (−17 à 15), lecture seule ici.
        ["oom_adj"] => file(format!("{}\n", p.oom_score_adj.load(Ordering::Relaxed) * 17 / 1000).into_bytes()),
        ["oom_score"] => {
            let adj = p.oom_score_adj.load(Ordering::Relaxed).max(-1000) as i64;
            file(format!("{}\n", (1000 + adj).clamp(0, 2000) / 2).into_bytes())
        }
        ["fd"] => Some(ProcNode::Dir(p.files.lock().iter().map(|(n, _)| (format!("{}", n), DT_LNK)).collect())),
        ["fdinfo"] => Some(ProcNode::Dir(p.files.lock().iter().map(|(n, _)| (format!("{}", n), DT_REG)).collect())),
        ["fd", n] => {
            let fd: usize = n.parse().ok()?;
            p.files.lock().get(fd).map(ProcNode::Fd)
        }
        ["fdinfo", n] => {
            let fd: usize = n.parse().ok()?;
            let f = p.files.lock().get(fd)?;
            let cloexec = p.files.lock().cloexec(fd).unwrap_or(false);
            let flags = f.flags() | if cloexec { 0o2000000 } else { 0 };
            let extra = f.object().map(|o| o.fdinfo()).unwrap_or_default();
            file(format!("pos:\t{}\nflags:\t0{:o}\nmnt_id:\t1\nino:\t0\n{}", *f.pos.lock(), flags, extra).into_bytes())
        }
        ["task"] => Some(ProcNode::Dir(
            p.threads().iter().map(|th| ns.nr_of(th.tid())).filter(|&n| n != 0).map(|n| (format!("{}", n), DT_DIR)).collect(),
        )),
        ["task", tid, more @ ..] => {
            let tid: usize = tid.parse().ok()?;
            let g = ns.global_of(tid)?;
            let th = p.threads().into_iter().find(|x| x.tid() == g)?;
            match more {
                [] => Some(ProcNode::Dir(
                    [("stat", DT_REG), ("status", DT_REG), ("comm", DT_REG)].iter().map(|(n, d)| (String::from(*n), *d)).collect(),
                )),
                ["stat"] => file(proc_stat(ns, p, Some(&th))),
                ["status"] => file(status(ns, p, Some(&th))),
                ["comm"] => file(format!("{}\n", th.comm_string()).into_bytes()),
                _ => None,
            }
        }
        _ => {
            let _ = t;
            None
        }
    }
}

// --- Contenus -------------------------------------------------------------------------------

const HZ: u64 = 100;

fn ticks(ns: u64) -> u64 {
    ns / (1_000_000_000 / HZ)
}

fn uptime_ns() -> u64 {
    time::Instant::now().duration_since(time::Instant::ZERO).as_nanos() as u64
}

/// État d'un fil : `R` en cours ou prêt, `S` endormi.
fn thread_state(th: &Thread) -> char {
    match th.task() {
        Some(k) if k.is_running() || k.sched.snapshot().on_rq => 'R',
        Some(_) => 'S',
        None => 'Z',
    }
}

fn process_state(p: &Process, th: Option<&Thread>) -> char {
    if p.is_zombie() {
        return 'Z';
    }
    if p.is_stopped() {
        return 'T';
    }
    match th {
        Some(t) => thread_state(t),
        None => {
            if p.threads().iter().any(|t| thread_state(t) == 'R') { 'R' } else { 'S' }
        }
    }
}

fn state_name(c: char) -> &'static str {
    match c {
        'R' => "R (running)",
        'S' => "S (sleeping)",
        'T' => "T (stopped)",
        'Z' => "Z (zombie)",
        _ => "D (disk sleep)",
    }
}

/// (taille virtuelle en octets, pages résidentes).
fn vm_sizes(p: &Process) -> (usize, usize) {
    match p.mm() {
        Some(mm) => {
            let mut size = 0;
            uvm::for_each_vma(&mm, |v| size += v.end - v.start);
            (size, mm.rss())
        }
        None => (0, 0),
    }
}

fn sigset_of(p: &Process, th: Option<&Thread>) -> (u64, u64, u64, u64, u64) {
    let shared = p.shared_pending.lock().mask;
    let (pend, blocked) = match th {
        Some(t) => (t.pending.lock().mask, t.sigmask.load(Ordering::Relaxed)),
        None => p.threads().first().map_or((0, 0), |t| (t.pending.lock().mask, t.sigmask.load(Ordering::Relaxed))),
    };
    let (mut ign, mut cgt) = (0u64, 0u64);
    for sig in 1..=64 {
        let a = p.sigaction(sig);
        let bit = 1u64 << (sig - 1);
        if a.handler == crate::signal::SIG_IGN {
            ign |= bit;
        } else if a.handler != crate::signal::SIG_DFL {
            cgt |= bit;
        }
    }
    (pend, shared, blocked, ign, cgt)
}

fn proc_stat(ns: &PidNs, p: &Process, th: Option<&Thread>) -> Vec<u8> {
    let state = process_state(p, th);
    let (vsize, rss) = vm_sizes(p);
    let cpu = th.map_or_else(|| p.cpu_ns(), |t| t.cpu_ns());
    let threads = p.threads();
    let nthreads = threads.len().max(1);
    let processor = th.or(threads.first().map(|a| &**a)).and_then(|t| t.task()).map_or(0, |k| k.sched.snapshot().cpu);
    let nice = th.or(threads.first().map(|a| &**a)).and_then(|t| t.task()).map_or(0, |k| k.sched.snapshot().nice);
    let start = ticks(p.started.duration_since(time::Instant::ZERO).as_nanos() as u64);
    let (sig_pend, sig_shared, blocked, ign, cgt) = sigset_of(p, th);
    let a = *p.args.lock();
    let pid = ns.nr_of(th.map_or(p.pid, |t| t.tid()));
    let pgid = ns.nr_of(p.pgid.load(Ordering::Relaxed));
    format!(
        "{} ({}) {} {} {} {} 34816 {} 4194560 0 0 0 0 {} 0 {} {} {} {} {} 0 {} {} {} 18446744073709551615 0 0 0 0 0 {} {} {} {} 0 0 0 {} {} 0 0 0 0 0 0 0 0 {} {} {} {} 0\n",
        pid,
        th.map_or_else(|| p.comm_string(), |t| t.comm_string()),
        state,
        ns.nr_of(p.ppid.load(Ordering::Relaxed)),
        pgid,
        ns.nr_of(p.sid.load(Ordering::Relaxed)),
        pgid,
        ticks(cpu),
        ticks(p.children_ns.load(Ordering::Relaxed)),
        0,
        20 + nice as i64,
        nice,
        nthreads,
        start,
        vsize,
        rss,
        (sig_pend | sig_shared) & 0x7fff_ffff,
        blocked & 0x7fff_ffff,
        ign & 0x7fff_ffff,
        cgt & 0x7fff_ffff,
        p.exit_signal.load(Ordering::Relaxed),
        processor,
        a[0],
        a[1],
        a[2],
        a[3],
    )
    .into_bytes()
}

fn status(ns: &PidNs, p: &Process, th: Option<&Thread>) -> Vec<u8> {
    let state = process_state(p, th);
    let (vsize, rss) = vm_sizes(p);
    let (pend, shared, blocked, ign, cgt) = sigset_of(p, th);
    let pid = ns.nr_of(th.map_or(p.pid, |t| t.tid()));
    // Parent invisible (hors de l'espace) : 0, comme `task_ppid_nr_ns`.
    let ppid = ns.nr_of(p.ppid.load(Ordering::Relaxed));
    let nspid = ns_list(th.map_or_else(|| p.pid_ref(), |t| t.pid_ref.lock().clone()), ns);
    let n = cpu::cpu_count() as usize;
    let caps = p.creds.lock().clone();
    // Identités vues de l'espace utilisateur du lecteur.
    let viewer = crate::current_process().map_or_else(crate::userns::init_user_ns, |c| c.creds.lock().user_ns.clone());
    format!(
        "Name:\t{}\nUmask:\t{:04o}\nState:\t{}\nTgid:\t{}\nNgid:\t0\nPid:\t{}\nPPid:\t{}\nTracerPid:\t0\n\
         Uid:\t{}\nGid:\t{}\nFDSize:\t{}\nGroups:\t{}\nNStgid:\t{}\nNSpid:\t{}\nNSpgid:\t{}\nNSsid:\t{}\n\
         VmPeak:\t{:8} kB\nVmSize:\t{:8} kB\nVmLck:\t       0 kB\nVmHWM:\t{:8} kB\nVmRSS:\t{:8} kB\nRssAnon:\t{:8} kB\n\
         RssFile:\t       0 kB\nRssShmem:\t       0 kB\nVmSwap:\t       0 kB\nThreads:\t{}\nSigQ:\t0/63432\n\
         SigPnd:\t{:016x}\nShdPnd:\t{:016x}\nSigBlk:\t{:016x}\nSigIgn:\t{:016x}\nSigCgt:\t{:016x}\n\
         CapInh:\t{:016x}\nCapPrm:\t{:016x}\nCapEff:\t{:016x}\nCapBnd:\t{:016x}\n\
         CapAmb:\t{:016x}\nNoNewPrivs:\t{}\nSeccomp:\t0\nCpus_allowed:\t{:x}\nCpus_allowed_list:\t0-{}\n\
         Mems_allowed:\t1\nMems_allowed_list:\t0\nvoluntary_ctxt_switches:\t0\nnonvoluntary_ctxt_switches:\t0\n",
        th.map_or_else(|| p.comm_string(), |t| t.comm_string()),
        p.umask.load(Ordering::Relaxed),
        state_name(state),
        ns.nr_of(p.pid),
        pid,
        ppid,
        {
            let c = p.creds.lock();
            let u = |k| viewer.from_kuid_munged(k);
            format!("{}\t{}\t{}\t{}", u(c.ruid), u(c.euid), u(c.suid), u(c.fsuid))
        },
        {
            let c = p.creds.lock();
            let g = |k| viewer.from_kgid_munged(k);
            format!("{}\t{}\t{}\t{}", g(c.rgid), g(c.egid), g(c.sgid), g(c.fsgid))
        },
        crate::fd::NOFILE,
        {
            let c = p.creds.lock();
            c.groups.iter().map(|&g| format!("{} ", viewer.from_kgid_munged(g))).collect::<String>()
        },
        ns_list(p.pid_ref(), ns),
        nspid,
        ns_list(p.pgrp_ref(), ns),
        ns_list(p.session_ref(), ns),
        vsize / 1024,
        vsize / 1024,
        rss * 4,
        rss * 4,
        rss * 4,
        p.threads().len().max(1),
        pend,
        shared,
        blocked,
        ign,
        cgt,
        caps.cap_inh,
        caps.cap_prm,
        caps.cap_eff,
        caps.cap_bnd,
        caps.cap_amb,
        p.no_new_privs.load(Ordering::Acquire) as u8,
        if n >= 64 { u64::MAX } else { (1u64 << n) - 1 },
        n.saturating_sub(1),
    )
    .into_bytes()
}

fn statm(p: &Process) -> Vec<u8> {
    let (vsize, rss) = vm_sizes(p);
    format!("{} {} 0 0 0 {} 0\n", vsize / 4096, rss, rss).into_bytes()
}

/// Octets `[args[i], args[i+1])` de la mémoire du processus (`cmdline`, `environ`).
fn user_range(p: &Process, i: usize) -> Vec<u8> {
    let a = *p.args.lock();
    let (start, end) = (a[i], a[i + 1]);
    let Some(mm) = p.mm() else { return Vec::new() };
    if end <= start || end - start > crate::exec::MAX_ARGS_BYTES {
        return Vec::new();
    }
    let mut v = alloc::vec![0u8; end - start];
    if mm.copy_from_user(&mut v, start).is_err() {
        return Vec::new();
    }
    v
}

/// `/proc/<pid>/maps`.
pub fn maps(p: &Process) -> Vec<u8> {
    let mut out = String::new();
    let exe = p.exe.lock().clone();
    let Some(mm) = p.mm() else { return Vec::new() };
    uvm::for_each_vma(&mm, |v| {
        let r = if v.prot.read() { 'r' } else { '-' };
        let w = if v.prot.write() { 'w' } else { '-' };
        let x = if v.prot.exec() { 'x' } else { '-' };
        let name = match v.name.as_str() {
            "exe" => exe.as_str(),
            n => n,
        };
        let off = match &v.backing {
            uvm::Backing::File { offset, .. } => *offset,
            uvm::Backing::Anon => 0,
            uvm::Backing::Shared { pgoff, .. } => pgoff * 4096,
        };
        let sp = if v.is_shared() && name != "[vvar]" && name != "[vdso]" { 's' } else { 'p' };
        let name = if v.is_shared() && name == "[anon]" { "/dev/zero (deleted)" } else { name };
        let name = if name == "[anon]" { "" } else { name };
        out.push_str(&format!("{:012x}-{:012x} {}{}{}{} {:08x} 00:00 0          {}\n", v.start, v.end, r, w, x, sp, off, name));
    });
    out.into_bytes()
}

fn meminfo() -> Vec<u8> {
    let s = frame_allocator::stats();
    let kb = |frames: usize| frames * 4;
    let (_, cached) = crate::pagecache::stats();
    format!(
        "MemTotal:       {:8} kB\nMemFree:        {:8} kB\nMemAvailable:   {:8} kB\nBuffers:               0 kB\n\
         Cached:         {:8} kB\nSwapCached:            0 kB\nActive:                0 kB\nInactive:              0 kB\n\
         SwapTotal:             0 kB\nSwapFree:              0 kB\nDirty:                 0 kB\nShmem:                 0 kB\n\
         Hugepagesize:       2048 kB\n",
        kb(s.total_frames),
        kb(s.free_frames),
        kb(s.free_frames + cached),
        kb(cached)
    )
    .into_bytes()
}

fn cpuinfo() -> Vec<u8> {
    let mut brand = [0u8; 48];
    // SAFETY : CPUID étendu (marque) disponible sur tout x86_64.
    let (f1, f7) = unsafe {
        for (i, leaf) in (0x8000_0002u32..=0x8000_0004).enumerate() {
            let r = core::arch::x86_64::__cpuid(leaf);
            for (j, w) in [r.eax, r.ebx, r.ecx, r.edx].iter().enumerate() {
                brand[i * 16 + j * 4..i * 16 + j * 4 + 4].copy_from_slice(&w.to_le_bytes());
            }
        }
        (core::arch::x86_64::__cpuid(1), core::arch::x86_64::__cpuid_count(7, 0))
    };
    let end = brand.iter().position(|&b| b == 0).unwrap_or(48);
    let name = core::str::from_utf8(&brand[..end]).unwrap_or("x86_64").trim();
    // Drapeaux usuels lus par les programmes (détection AVX, etc.).
    let mut flags: Vec<&str> = alloc::vec!["fpu", "tsc", "msr", "pae", "cx8", "apic", "sep", "cmov", "pat", "clflush", "mmx", "fxsr", "sse", "sse2", "ht", "syscall", "nx", "lm", "constant_tsc", "nonstop_tsc"];
    let bits: [(u32, u32, &str); 12] = [
        (f1.ecx, 0, "pni"),
        (f1.ecx, 9, "ssse3"),
        (f1.ecx, 12, "fma"),
        (f1.ecx, 13, "cx16"),
        (f1.ecx, 19, "sse4_1"),
        (f1.ecx, 20, "sse4_2"),
        (f1.ecx, 22, "movbe"),
        (f1.ecx, 23, "popcnt"),
        (f1.ecx, 25, "aes"),
        (f1.ecx, 26, "xsave"),
        (f1.ecx, 28, "avx"),
        (f1.ecx, 30, "rdrand"),
    ];
    for (reg, b, n) in bits {
        if reg & (1 << b) != 0 {
            flags.push(n);
        }
    }
    let bits7: [(u32, &str); 8] = [(0, "fsgsbase"), (3, "bmi1"), (5, "avx2"), (8, "bmi2"), (9, "erms"), (16, "avx512f"), (18, "rdseed"), (29, "sha_ni")];
    for (b, n) in bits7 {
        if f7.ebx & (1 << b) != 0 {
            flags.push(n);
        }
    }
    let flags = flags.join(" ");
    let n = cpu::cpu_count();
    let mut out = String::new();
    for c in 0..n {
        out.push_str(&format!(
            "processor\t: {}\nvendor_id\t: GenuineIntel\nmodel name\t: {}\ncpu MHz\t\t: 3000.000\ncache size\t: 8192 KB\n\
             physical id\t: 0\nsiblings\t: {}\ncore id\t\t: {}\ncpu cores\t: {}\napicid\t\t: {}\nfpu\t\t: yes\n\
             cpuid level\t: 22\nflags\t\t: {}\nbogomips\t: 6000.00\nclflush size\t: 64\ncache_alignment\t: 64\n\
             address sizes\t: 46 bits physical, 48 bits virtual\n\n",
            c, name, n, c, n, c, flags
        ));
    }
    out.into_bytes()
}

fn stat() -> Vec<u8> {
    let st = task::scheduler::stats();
    let mut out = String::new();
    let (mut busy_all, mut idle_all, mut ctxt) = (0u64, 0u64, 0u64);
    let mut lines = String::new();
    for s in &st {
        let idle = ticks(s.idle_ns);
        let busy = ticks(s.now_ns).saturating_sub(idle);
        busy_all += busy;
        idle_all += idle;
        ctxt += s.switches;
        lines.push_str(&format!("cpu{} {} 0 0 {} 0 0 0 0 0 0\n", s.cpu, busy, idle));
    }
    out.push_str(&format!("cpu  {} 0 0 {} 0 0 0 0 0 0\n", busy_all, idle_all));
    out.push_str(&lines);
    let running: u32 = st.iter().map(|s| s.nr_running).sum();
    let (epoch, _) = crate::linux::sys::realtime();
    let boot = epoch - (uptime_ns() / 1_000_000_000) as i64;
    out.push_str(&format!(
        "intr 0\nctxt {}\nbtime {}\nprocesses {}\nprocs_running {}\nprocs_blocked 0\nsoftirq 0 0 0 0 0 0 0 0 0 0 0\n",
        ctxt,
        boot,
        crate::all_processes().len(),
        running.max(1)
    ));
    out.into_bytes()
}

fn loadavg(ns: &PidNs) -> Vec<u8> {
    let st = task::scheduler::stats();
    let running: u32 = st.iter().map(|s| s.nr_running).sum();
    let l = crate::loadavg::get();
    // Dernier numéro attribué dans l'espace du `proc` (`idr_get_cursor - 1`).
    let last = ns.last_pid();
    format!(
        "{}.{:02} {}.{:02} {}.{:02} {}/{} {}\n",
        l[0] / 100,
        l[0] % 100,
        l[1] / 100,
        l[1] % 100,
        l[2] / 100,
        l[2] % 100,
        running.max(1),
        task::all_tasks().len(),
        last
    )
    .into_bytes()
}

fn uptime() -> Vec<u8> {
    let ns = uptime_ns();
    let idle: u64 = task::scheduler::stats().iter().map(|s| s.idle_ns).sum();
    format!("{}.{:02} {}.{:02}\n", ns / 1_000_000_000, (ns / 10_000_000) % 100, idle / 1_000_000_000, (idle / 10_000_000) % 100).into_bytes()
}


fn limits() -> Vec<u8> {
    format!(
        "Limit                     Soft Limit           Hard Limit           Units     \n\
         Max cpu time              unlimited            unlimited            seconds   \n\
         Max file size             unlimited            unlimited            bytes     \n\
         Max data size             unlimited            unlimited            bytes     \n\
         Max stack size            8388608              unlimited            bytes     \n\
         Max core file size        0                    unlimited            bytes     \n\
         Max resident set          unlimited            unlimited            bytes     \n\
         Max processes             63432                63432                processes \n\
         Max open files            {:<20} {:<20} files     \n\
         Max locked memory         8388608              8388608              bytes     \n\
         Max address space         unlimited            unlimited            bytes     \n\
         Max pending signals       63432                63432                signals   \n",
        crate::fd::NOFILE,
        crate::fd::NOFILE
    )
    .into_bytes()
}

/// `/proc/net/...` : sockets Internet (`netstat`, `ss`) et interfaces.
/// Espace réseau de l'appelant (`/proc/net` → `/proc/self/net`).
fn procnet(net: &crate::netns::NetNs, rest: &[&str]) -> Option<ProcNode> {
    use crate::inet::{bound_sockets, interfaces};
    let file = |s: String| Some(ProcNode::File(s.into_bytes()));
    let sockets = |proto: i32, v6: bool| {
        let tcp = proto == crate::inet::IPPROTO_TCP;
        let mut out = String::from(if v6 {
            "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
"
        } else {
            "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
"
        });
        let want = if v6 { crate::socket::AF_INET6 } else { crate::socket::AF_INET };
        for (i, s) in bound_sockets(net, proto).iter().filter(|s| s.family() == want).enumerate() {
            let (l, r, rx) = s.proc_info();
            let st = if tcp { s.tcp_state() } else { 7 };
            out += &format!(
                "{:4}: {} {} {:02X} {:08X}:{:08X} 00:00000000 00000000     0        0 {} 1 0000000000000000 20 4 30 10 -1
",
                i,
                l.proc_hex(!v6),
                r.proc_hex(!v6),
                st,
                0,
                rx,
                s.ino()
            );
        }
        out
    };
    match rest {
        [] => Some(ProcNode::Dir(
            ["tcp", "tcp6", "udp", "udp6", "udplite", "udplite6", "dev", "route", "if_inet6"].iter().map(|n| (String::from(*n), DT_REG)).collect(),
        )),
        ["tcp"] => file(sockets(crate::inet::IPPROTO_TCP, false)),
        ["tcp6"] => file(sockets(crate::inet::IPPROTO_TCP, true)),
        ["udp"] => file(sockets(crate::inet::IPPROTO_UDP, false)),
        ["udp6"] => file(sockets(crate::inet::IPPROTO_UDP, true)),
        ["udplite"] => file(sockets(crate::inet::IPPROTO_UDPLITE, false)),
        ["udplite6"] => file(sockets(crate::inet::IPPROTO_UDPLITE, true)),
        ["dev"] => {
            let mut out = String::from(
                "Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
",
            );
            for i in interfaces(net) {
                let [rb, rp, tb, tp, td] = i.stats;
                out += &format!(
                    "{:>6}: {:>8} {:>7}    0    0    0     0          0         0 {:>8} {:>7}    0 {:>4}    0     0       0          0
",
                    i.name, rb, rp, tb, tp, td
                );
            }
            file(out)
        }
        ["route"] => {
            let mut out = String::from("Iface	Destination	Gateway 	Flags	RefCnt	Use	Metric	Mask		MTU	Window	IRTT
");
            for i in interfaces(net).iter().filter(|i| i.index > 1) {
                if let (Some(gw), Some(_)) = (i.gw4, i.v4) {
                    // Route par défaut : RTF_UP | RTF_GATEWAY.
                    out += &format!(
                        "{}	00000000	{:08X}	0003	0	0	0	00000000	0	0	0
",
                        i.name,
                        u32::from_le_bytes(gw)
                    );
                }
                if let Some((ip, p)) = i.v4 {
                    let m: u32 = if p == 0 { 0 } else { u32::MAX << (32 - p as u32) };
                    let net = u32::from_be_bytes(ip) & m;
                    out += &format!(
                        "{}	{:08X}	00000000	0001	0	0	0	{:08X}	0	0	0
",
                        i.name,
                        u32::from_le_bytes(net.to_be_bytes()),
                        u32::from_le_bytes(m.to_be_bytes())
                    );
                }
            }
            file(out)
        }
        ["if_inet6"] => file(if net.lo().local6([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]) {
            String::from("00000000000000000000000000000001 01 80 10 80       lo
")
        } else {
            String::new()
        }),
        _ => None,
    }
}

/// `/proc/sys/...`.
fn sysctl(t: &Thread, rest: &[&str]) -> Option<ProcNode> {
    let file = |s: String| Some(ProcNode::File(s.into_bytes()));
    match rest {
        [] => Some(ProcNode::Dir(alloc::vec![(String::from("kernel"), DT_DIR), (String::from("vm"), DT_DIR), (String::from("fs"), DT_DIR), (String::from("user"), DT_DIR)])),
        // Espaces de noms : limites par utilisateur (`/proc/sys/user`), espaces utilisateur
        // permis sans privilège (réglage des noyaux Debian que lisent bwrap et Steam).
        ["user", name] if name.starts_with("max_") && name.ends_with("_namespaces") => file(String::from("63432\n")),
        ["user"] => Some(ProcNode::Dir(
            ["max_cgroup_namespaces", "max_ipc_namespaces", "max_mnt_namespaces", "max_net_namespaces", "max_pid_namespaces", "max_time_namespaces", "max_user_namespaces", "max_uts_namespaces"]
                .iter()
                .map(|n| (String::from(*n), DT_REG))
                .collect(),
        )),
        ["kernel", "unprivileged_userns_clone"] => file(String::from("1\n")),
        ["kernel", "osrelease"] => file(format!("{}\n", crate::linux::sys::RELEASE)),
        ["kernel", "ostype"] => file(String::from("Linux\n")),
        ["kernel", "hostname"] => file(format!("{}\n", String::from_utf8_lossy(&crate::linux::sys::uts_name(t, 0)))),
        ["kernel", "domainname"] => file(format!("{}\n", String::from_utf8_lossy(&crate::linux::sys::uts_name(t, 1)))),
        ["kernel", "pid_max"] => file(String::from("4194304\n")),
        // Espace pid de celui qui ouvre (`pid_ns_ctl_handler`) ; inscriptible pour CRIU.
        ["kernel", "ns_last_pid"] => Some(ProcNode::Node(Node::Object(Arc::new(NsLastPid { ns: t.process.pid_ns.clone(), pos: Mutex::new(0) })))),
        ["kernel", "threads-max"] => file(String::from("126864\n")),
        ["kernel", "ngroups_max"] => file(String::from("65536\n")),
        ["kernel", "cap_last_cap"] => file(String::from("40\n")),
        ["kernel", "overflowuid"] | ["kernel", "overflowgid"] => file(String::from("65534\n")),
        // Limites des IPC System V (valeurs par défaut de Linux, `ipc.rs`).
        ["kernel", "shmmax"] | ["kernel", "shmall"] => file(format!("{}
", u64::MAX - (1 << 24))),
        ["kernel", "shmmni"] => file(String::from("4096
")),
        ["kernel", "random", "boot_id"] => file(boot_id()),
        ["kernel", "random", "uuid"] => file(uuid(uvm::rand::random_u64(), uvm::rand::random_u64())),
        ["kernel", "random", "entropy_avail"] => file(String::from("256\n")),
        ["kernel", "yama", "ptrace_scope"] => file(String::from("1\n")),
        ["vm", "overcommit_memory"] => file(String::from("0\n")),
        ["vm", "max_map_count"] => file(String::from("65530\n")),
        ["fs", "file-max"] => file(String::from("9223372036854775807\n")),
        ["fs", "nr_open"] => file(format!("{}\n", crate::fd::NOFILE)),
        ["fs", "pipe-max-size"] => file(String::from("1048576\n")),
        _ => None,
    }
}

fn uuid(a: u64, b: u64) -> String {
    let a = (a & !0xf000) | 0x4000; // version 4
    let b = (b & !(0xc000 << 48)) | (0x8000 << 48); // variante RFC 4122
    format!("{:08x}-{:04x}-{:04x}-{:04x}-{:012x}\n", a >> 32, (a >> 16) & 0xffff, a & 0xffff, b >> 48, b & 0xffff_ffff_ffff)
}

fn boot_id() -> String {
    static ID: spin::Once<(u64, u64)> = spin::Once::new();
    let &(a, b) = ID.call_once(|| (uvm::rand::random_u64(), uvm::rand::random_u64()));
    uuid(a, b)
}

/// `/sys/...` (topologie des processeurs).
fn sys(rest: &str) -> Option<ProcNode> {
    let n = cpu::cpu_count() as usize;
    let range = if n > 1 { format!("0-{}\n", n - 1) } else { String::from("0\n") };
    let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    match parts.as_slice() {
        [] => Some(ProcNode::Dir(alloc::vec![(String::from("devices"), DT_DIR)])),
        ["devices"] => Some(ProcNode::Dir(alloc::vec![(String::from("system"), DT_DIR)])),
        ["devices", "system"] => Some(ProcNode::Dir(alloc::vec![(String::from("cpu"), DT_DIR)])),
        ["devices", "system", "cpu"] => {
            let mut e: Vec<(String, u8)> = ["online", "possible", "present", "kernel_max"].iter().map(|s| (String::from(*s), DT_REG)).collect();
            for c in 0..n {
                e.push((format!("cpu{}", c), DT_DIR));
            }
            Some(ProcNode::Dir(e))
        }
        ["devices", "system", "cpu", "online" | "possible" | "present"] => Some(ProcNode::File(range.into_bytes())),
        ["devices", "system", "cpu", "kernel_max"] => Some(ProcNode::File(b"8191\n".to_vec())),
        ["devices", "system", "cpu", c] if c.starts_with("cpu") => {
            let i: usize = c[3..].parse().ok()?;
            (i < n).then(|| ProcNode::Dir(alloc::vec![(String::from("online"), DT_REG)]))
        }
        ["devices", "system", "cpu", c, "online"] if c.starts_with("cpu") => Some(ProcNode::File(b"1\n".to_vec())),
        _ => None,
    }
}
