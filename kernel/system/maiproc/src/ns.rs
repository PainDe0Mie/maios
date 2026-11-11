//! Espaces de noms d'un processus (`nsproxy`) : `unshare(2)`, `setns(2)`, `clone(CLONE_NEW*)`
//! et `/proc/<pid>/ns/*` (`namespaces(7)`).
//!
//! Pris en charge : utilisateur ([`crate::userns`]), montage ([`crate::mount`]), UTS (nom
//! d'hôte et de domaine), IPC System V ([`crate::ipc`] : une table de segments par espace) et
//! cgroup (tout processus est à la racine de l'unique hiérarchie : `/proc/self/cgroup` vaut
//! `0::/` dans chaque espace, ce qui est exactement ce qu'un nouvel espace cgroup montre).
//! PID ([`crate::pidns`] : `unshare(CLONE_NEWPID)` ne change que l'espace des **enfants**,
//! `pid_for_children`, comme Linux). Réseau ([`crate::netns`] : `lo` seul, éteint).
//! L'espace temps ne l'est pas encore : `EINVAL`, comme un noyau compilé sans lui
//! (`CONFIG_TIME_NS=n`), plutôt qu'une isolation feinte.

use alloc::{format, string::String, sync::Arc, vec::Vec};
use core::sync::atomic::Ordering;

use crate::ipc::IpcNs;
use crate::linux::errno::*;
use crate::mount::{FsStruct, MntNs};
use crate::netns::NetNs;
use crate::pidns::PidNs;
use crate::userns::{self, UserNs, CAP_SYS_ADMIN, CAP_SYS_CHROOT};
use crate::{Process, Thread};

pub const CLONE_NEWTIME: u64 = 0x0000_0080;
pub const CLONE_VM: u64 = 0x0000_0100;
pub const CLONE_FS: u64 = 0x0000_0200;
pub const CLONE_FILES: u64 = 0x0000_0400;
pub const CLONE_SIGHAND: u64 = 0x0000_0800;
pub const CLONE_THREAD: u64 = 0x0001_0000;
pub const CLONE_NEWNS: u64 = 0x0002_0000;
pub const CLONE_SYSVSEM: u64 = 0x0004_0000;
pub const CLONE_NEWCGROUP: u64 = 0x0200_0000;
pub const CLONE_NEWUTS: u64 = 0x0400_0000;
pub const CLONE_NEWIPC: u64 = 0x0800_0000;
pub const CLONE_NEWUSER: u64 = 0x1000_0000;
pub const CLONE_NEWPID: u64 = 0x2000_0000;
pub const CLONE_NEWNET: u64 = 0x4000_0000;
/// Espaces que MaiProc ne sait pas encore séparer.
const UNSUPPORTED: u64 = CLONE_NEWTIME;
pub const NS_FLAGS: u64 = CLONE_NEWNS | CLONE_NEWCGROUP | CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWUSER | CLONE_NEWPID | CLONE_NEWNET | CLONE_NEWTIME;

/// Espace sans état propre (cgroup ; temps initial).
pub struct SimpleNs {
    pub ino: u64,
    pub user_ns: Arc<UserNs>,
}

/// Espace UTS : nom d'hôte et de domaine NIS (`None` : valeur par défaut).
pub struct UtsNs {
    pub ino: u64,
    pub user_ns: Arc<UserNs>,
    pub names: spin::Mutex<[Option<Vec<u8>>; 2]>,
}

#[derive(Clone)]
pub struct Nsproxy {
    pub mnt: Arc<MntNs>,
    pub uts: Arc<UtsNs>,
    pub ipc: Arc<IpcNs>,
    pub cgroup: Arc<SimpleNs>,
    /// Espace pid des futurs enfants (`pid_ns_for_children`) ; celui du processus lui-même est
    /// [`crate::Process::pid_ns`].
    pub pid_for_children: Arc<PidNs>,
    pub net: Arc<NetNs>,
    pub time: Arc<SimpleNs>,
}

impl Nsproxy {
    /// Propriétaire de l'espace réseau (droit de monter `sysfs`).
    pub fn net_user_ns(&self) -> Arc<UserNs> {
        self.net.user_ns.clone()
    }
}

static INIT: spin::Once<Arc<Nsproxy>> = spin::Once::new();

/// Espaces initiaux.
pub fn init_nsproxy() -> Arc<Nsproxy> {
    INIT.call_once(|| {
        let user = userns::init_user_ns();
        let simple = |ino| Arc::new(SimpleNs { ino, user_ns: user.clone() });
        Arc::new(Nsproxy {
            mnt: crate::mount::init_ns(),
            uts: Arc::new(UtsNs { ino: userns::INIT_UTS_INO, user_ns: user.clone(), names: spin::Mutex::new([None, None]) }),
            ipc: IpcNs::new(userns::INIT_IPC_INO, user.clone()),
            cgroup: simple(userns::INIT_CGROUP_INO),
            pid_for_children: crate::pidns::init_pid_ns(),
            net: crate::netns::init_net(),
            time: simple(userns::INIT_TIME_INO),
        })
    })
    .clone()
}

/// Espaces du processus.
pub fn of(p: &Process) -> Arc<Nsproxy> {
    p.nsproxy.lock().clone()
}

/// Nom d'hôte (0) ou de domaine (1) de l'espace UTS du processus.
pub fn uts_name(p: &Process, which: usize) -> Vec<u8> {
    let ns = of(p);
    let names = ns.uts.names.lock();
    match &names[which] {
        Some(v) => v.clone(),
        None => (if which == 0 { "maios" } else { "(none)" }).as_bytes().to_vec(),
    }
}

/// Nouveaux espaces de `flags` pour des identités `creds` (`create_new_namespaces`) ; `fs` est
/// reporté dans le nouvel espace de montage.
fn create(old: &Nsproxy, active_pid: &Arc<PidNs>, flags: u64, creds: &crate::creds::Creds, fs: &mut FsStruct) -> Result<Nsproxy, i64> {
    if flags & UNSUPPORTED != 0 {
        return Err(EINVAL);
    }
    let mut n = old.clone();
    let others = flags & (CLONE_NEWNS | CLONE_NEWCGROUP | CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWPID | CLONE_NEWNET);
    if others != 0 && !userns::ns_capable(creds, &creds.user_ns, CAP_SYS_ADMIN) {
        return Err(EPERM);
    }
    if flags & CLONE_NEWNS != 0 {
        n.mnt = crate::mount::copy_ns(&old.mnt, &creds.user_ns, fs)?;
    }
    if flags & CLONE_NEWUTS != 0 {
        let names = old.uts.names.lock().clone();
        n.uts = Arc::new(UtsNs { ino: userns::alloc_ns_ino(), user_ns: creds.user_ns.clone(), names: spin::Mutex::new(names) });
    }
    if flags & CLONE_NEWIPC != 0 {
        n.ipc = IpcNs::new(userns::alloc_ns_ino(), creds.user_ns.clone());
    }
    if flags & CLONE_NEWCGROUP != 0 {
        n.cgroup = Arc::new(SimpleNs { ino: userns::alloc_ns_ino(), user_ns: creds.user_ns.clone() });
    }
    if flags & CLONE_NEWPID != 0 {
        // Un seul nouvel espace avant le premier enfant (`copy_pid_ns`).
        if !Arc::ptr_eq(&old.pid_for_children, active_pid) {
            return Err(EINVAL);
        }
        n.pid_for_children = PidNs::new_child(&old.pid_for_children, &creds.user_ns)?;
    }
    if flags & CLONE_NEWNET != 0 {
        n.net = NetNs::new(&creds.user_ns);
    }
    Ok(n)
}

/// Espaces, identités et `fs_struct` d'un enfant de `clone` (`copy_namespaces`, `copy_creds`).
/// `fs` : celui de l'enfant (déjà copié sans `CLONE_FS`).
pub fn for_clone(p: &Process, flags: u64, creds: &mut crate::creds::Creds, fs: &mut FsStruct) -> Result<Arc<Nsproxy>, i64> {
    let old = of(p);
    if flags & NS_FLAGS == 0 {
        return Ok(old);
    }
    if flags & CLONE_NEWUSER != 0 {
        if flags & (CLONE_THREAD | CLONE_FS) != 0 {
            return Err(EINVAL);
        }
        if fs.chrooted(&old.mnt) {
            return Err(EPERM);
        }
        let ns = UserNs::new_child(&creds.user_ns, creds)?;
        creds.enter_user_ns(ns);
    }
    if flags & CLONE_NEWNS != 0 && flags & CLONE_FS != 0 {
        return Err(EINVAL);
    }
    Ok(Arc::new(create(&old, &p.pid_ns, flags, creds, fs)?))
}

/// `unshare(flags)`.
pub fn unshare(t: &Thread, flags: u64) -> Result<i64, i64> {
    const ALLOWED: u64 = CLONE_THREAD | CLONE_FS | CLONE_NEWNS | CLONE_SIGHAND | CLONE_VM | CLONE_FILES | CLONE_SYSVSEM | NS_FLAGS;
    if flags & !ALLOWED != 0 {
        return Err(EINVAL);
    }
    let mut flags = flags;
    if flags & CLONE_NEWUSER != 0 {
        flags |= CLONE_THREAD | CLONE_FS;
    }
    if flags & CLONE_VM != 0 {
        flags |= CLONE_SIGHAND;
    }
    if flags & CLONE_SIGHAND != 0 {
        flags |= CLONE_THREAD;
    }
    if flags & CLONE_NEWNS != 0 {
        flags |= CLONE_FS;
    }
    let p = &t.process;
    // Partager moins de choses qu'un fil : seulement si le processus est seul.
    if flags & (CLONE_THREAD | CLONE_SIGHAND | CLONE_VM) != 0 && p.live_threads.load(Ordering::Acquire) > 1 {
        return Err(EINVAL);
    }
    if flags & UNSUPPORTED != 0 {
        return Err(EINVAL);
    }
    let old = of(p);
    let mut fs = p.fs.get();
    let mut creds = p.creds.lock().clone();
    if flags & CLONE_NEWUSER != 0 {
        if fs.chrooted(&old.mnt) {
            return Err(EPERM);
        }
        let ns = UserNs::new_child(&creds.user_ns, &creds)?;
        creds.enter_user_ns(ns);
    }
    let new_ns = if flags & NS_FLAGS & !CLONE_NEWUSER != 0 { Some(create(&old, &p.pid_ns, flags, &creds, &mut fs)?) } else { None };
    // --- Validation faite : on applique. ---
    if flags & CLONE_FILES != 0 {
        let copy = p.files.lock().clone_table();
        *p.files.lock() = copy;
    }
    if flags & CLONE_NEWUSER != 0 {
        *p.creds.lock() = creds;
    }
    if flags & CLONE_FS != 0 {
        // `fs_struct` propre au processus (copie s'il était partagé), puis nouvelle racine.
        p.unshare_fs(fs);
    }
    if let Some(n) = new_ns {
        *p.nsproxy.lock() = Arc::new(n);
    }
    Ok(0)
}

// --- /proc/<pid>/ns/* ------------------------------------------------------------------------------

#[derive(Clone)]
pub enum NsRef {
    User(Arc<UserNs>),
    Mnt(Arc<MntNs>),
    Uts(Arc<UtsNs>),
    Ipc(Arc<IpcNs>),
    Cgroup(Arc<SimpleNs>),
    Pid(Arc<PidNs>),
    Net(Arc<NetNs>),
    Time(Arc<SimpleNs>),
}

impl NsRef {
    pub fn ino(&self) -> u64 {
        match self {
            NsRef::User(n) => n.ino,
            NsRef::Mnt(n) => n.ino,
            NsRef::Uts(n) => n.ino,
            NsRef::Ipc(n) => n.ino,
            NsRef::Pid(n) => n.ino,
            NsRef::Net(n) => n.ino,
            NsRef::Cgroup(n) | NsRef::Time(n) => n.ino,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            NsRef::User(_) => "user",
            NsRef::Mnt(_) => "mnt",
            NsRef::Uts(_) => "uts",
            NsRef::Ipc(_) => "ipc",
            NsRef::Cgroup(_) => "cgroup",
            NsRef::Pid(_) => "pid",
            NsRef::Net(_) => "net",
            NsRef::Time(_) => "time",
        }
    }

    /// Drapeau `CLONE_NEW*` du type.
    pub fn flag(&self) -> u64 {
        match self {
            NsRef::User(_) => CLONE_NEWUSER,
            NsRef::Mnt(_) => CLONE_NEWNS,
            NsRef::Uts(_) => CLONE_NEWUTS,
            NsRef::Ipc(_) => CLONE_NEWIPC,
            NsRef::Cgroup(_) => CLONE_NEWCGROUP,
            NsRef::Pid(_) => CLONE_NEWPID,
            NsRef::Net(_) => CLONE_NEWNET,
            NsRef::Time(_) => CLONE_NEWTIME,
        }
    }

    fn owner(&self) -> Arc<UserNs> {
        match self {
            NsRef::User(n) => n.parent.clone().unwrap_or_else(|| n.clone()),
            NsRef::Mnt(n) => n.user_ns.clone(),
            NsRef::Uts(n) => n.user_ns.clone(),
            NsRef::Ipc(n) => n.user_ns.clone(),
            NsRef::Pid(n) => n.user_ns.clone(),
            NsRef::Net(n) => n.user_ns.clone(),
            NsRef::Cgroup(n) | NsRef::Time(n) => n.user_ns.clone(),
        }
    }
}

/// Noms des entrées de `/proc/<pid>/ns/`.
pub const NS_NAMES: [&str; 10] = ["cgroup", "ipc", "mnt", "net", "pid", "pid_for_children", "time", "time_for_children", "user", "uts"];

/// Espace `name` du processus `p`.
pub fn ns_of(p: &Process, name: &str) -> Option<NsRef> {
    let n = of(p);
    Some(match name {
        "user" => NsRef::User(p.creds.lock().user_ns.clone()),
        "mnt" => NsRef::Mnt(n.mnt.clone()),
        "uts" => NsRef::Uts(n.uts.clone()),
        "ipc" => NsRef::Ipc(n.ipc.clone()),
        "cgroup" => NsRef::Cgroup(n.cgroup.clone()),
        "pid" => NsRef::Pid(p.pid_ns.clone()),
        "pid_for_children" => NsRef::Pid(n.pid_for_children.clone()),
        "net" => NsRef::Net(n.net.clone()),
        "time" | "time_for_children" => NsRef::Time(n.time.clone()),
        _ => return None,
    })
}

/// Cible du lien `/proc/<pid>/ns/<name>` : `mnt:[4026531841]`.
pub fn link_text(r: &NsRef) -> String {
    format!("{}:[{}]", r.name(), r.ino())
}

/// Description ouverte d'un espace (`nsfs`) : `setns`, `ioctl(NS_GET_*)`.
pub struct NsFile {
    pub ns: NsRef,
    pub pid: usize,
}

const NS_GET_USERNS: u64 = 0xb701;
const NS_GET_PARENT: u64 = 0xb702;
const NS_GET_NSTYPE: u64 = 0xb703;
const NS_GET_OWNER_UID: u64 = 0xb704;
/// `_IOR(NSIO, 0x6..0x9, int)` : traduction d'un pid entre l'espace du descripteur et celui de
/// l'appelant (Linux 6.11).
const NS_GET_PID_FROM_PIDNS: u64 = 0x8004_b706;
const NS_GET_TGID_FROM_PIDNS: u64 = 0x8004_b707;
const NS_GET_PID_IN_PIDNS: u64 = 0x8004_b708;
const NS_GET_TGID_IN_PIDNS: u64 = 0x8004_b709;

impl crate::object::FileObject for NsFile {
    crate::object::object_any!();

    fn link_name(&self) -> String {
        link_text(&self.ns)
    }

    /// Mode `S_IFREG | 0444` comme `nsfs`, inode = numéro de l'espace.
    fn stat(&self) -> (u32, u64) {
        (0o100444, self.ns.ino())
    }

    fn read(&self, _t: &Thread, _kb: &mut [u8], _nonblock: bool) -> Result<usize, i64> {
        Err(EINVAL)
    }

    fn write(&self, _t: &Thread, _kb: &[u8], _nonblock: bool) -> Result<usize, i64> {
        Err(EINVAL)
    }

    fn poll(&self, _t: Option<&Thread>) -> u32 {
        0
    }

    fn queues<'a>(&'a self, _cb: &mut dyn FnMut(&'a crate::wait::WaitQueue)) {}

    fn ioctl(&self, t: &Thread, req: u64, arg: u64) -> crate::linux::SysResult {
        match req {
            NS_GET_NSTYPE => Ok(self.ns.flag() as i64),
            NS_GET_OWNER_UID => {
                let NsRef::User(u) = &self.ns else { return Err(EINVAL) };
                let me = t.process.creds.lock().user_ns.clone();
                let uid = me.from_kuid_munged(u.owner);
                t.mm().write_user::<u32>(arg as usize, &uid).map_err(crate::linux::fault)?;
                Ok(0)
            }
            NS_GET_PID_FROM_PIDNS | NS_GET_TGID_FROM_PIDNS | NS_GET_PID_IN_PIDNS | NS_GET_TGID_IN_PIDNS => {
                let NsRef::Pid(ns) = &self.ns else { return Err(EINVAL) };
                let me = &t.process.pid_ns;
                let (from, to) = if req == NS_GET_PID_FROM_PIDNS || req == NS_GET_TGID_FROM_PIDNS { (&**ns, &**me) } else { (&**me, &**ns) };
                let g = from.global_of(arg as u32 as usize).ok_or(ESRCH)?;
                let g = if req == NS_GET_TGID_FROM_PIDNS || req == NS_GET_TGID_IN_PIDNS {
                    crate::find_thread(g).map(|th| th.process.pid).or_else(|| crate::find_process(g).map(|p| p.pid)).ok_or(ESRCH)?
                } else {
                    g
                };
                match to.nr_of(g) {
                    0 => Err(ESRCH),
                    n => Ok(n as i64),
                }
            }
            NS_GET_PARENT if matches!(self.ns, NsRef::Pid(_)) => {
                // `pidns_get_parent` : un parent que l'appelant voit (son espace ou un
                // descendant).
                let NsRef::Pid(ns) = &self.ns else { return Err(EINVAL) };
                let parent = ns.parent.clone().ok_or(EPERM)?;
                if !t.process.pid_ns.is_ancestor_of(&parent) {
                    return Err(EPERM);
                }
                let f = crate::fd::OpenFile::new(crate::fd::Node::Object(Arc::new(NsFile { ns: NsRef::Pid(parent), pid: self.pid })), 0);
                t.process.files.lock().alloc(f, 0, true).map(|fd| fd as i64).ok_or(EMFILE)
            }
            NS_GET_USERNS | NS_GET_PARENT => {
                let target = if req == NS_GET_USERNS {
                    if let NsRef::User(_) = self.ns {
                        return Err(EINVAL);
                    }
                    self.ns.owner()
                } else {
                    match &self.ns {
                        NsRef::User(u) => u.parent.clone().ok_or(EPERM)?,
                        _ => return Err(EINVAL),
                    }
                };
                // Seulement un espace que l'appelant voit (son espace ou un descendant).
                let me = t.process.creds.lock().user_ns.clone();
                if !me.is_ancestor_of(&target) {
                    return Err(EPERM);
                }
                let f = crate::fd::OpenFile::new(crate::fd::Node::Object(Arc::new(NsFile { ns: NsRef::User(target), pid: self.pid })), 0);
                t.process.files.lock().alloc(f, 0, true).map(|fd| fd as i64).ok_or(EMFILE)
            }
            _ => Err(ENOTTY),
        }
    }
}

/// Ouvre `/proc/<pid>/ns/<name>`.
pub fn open_ns_file(p: &Arc<Process>, name: &str) -> Option<crate::fd::Node> {
    let ns = ns_of(p, name)?;
    Some(crate::fd::Node::Object(Arc::new(NsFile { ns, pid: p.pid })))
}

/// `setns(fd, nstype)`.
pub fn setns(t: &Thread, fd: u64, nstype: u64) -> Result<i64, i64> {
    let f = crate::linux::fs::get_fd(t, fd)?;
    let obj = f.object().ok_or(EINVAL)?;
    let nsf = obj.as_any().downcast_ref::<NsFile>().ok_or(EINVAL)?;
    let target = nsf.ns.clone();
    if nstype != 0 && nstype != target.flag() {
        return Err(EINVAL);
    }
    let p = &t.process;
    let creds = p.creds.lock().clone();
    let me_cap = |cap| userns::ns_capable(&creds, &creds.user_ns, cap);
    let old = of(p);
    match target {
        NsRef::User(ns) => {
            // `userns_install`.
            if Arc::ptr_eq(&ns, &creds.user_ns) {
                return Err(EINVAL);
            }
            if p.live_threads.load(Ordering::Acquire) > 1 {
                return Err(EINVAL);
            }
            if p.fs.with(|f| f.chrooted(&old.mnt)) {
                return Err(EPERM);
            }
            if !userns::ns_capable(&creds, &ns, CAP_SYS_ADMIN) {
                return Err(EPERM);
            }
            let mut c = creds;
            c.enter_user_ns(ns);
            *p.creds.lock() = c;
        }
        NsRef::Mnt(ns) => {
            // `mntns_install`.
            if !userns::ns_capable(&creds, &ns.user_ns, CAP_SYS_ADMIN) || !me_cap(CAP_SYS_CHROOT) || !me_cap(CAP_SYS_ADMIN) {
                return Err(EPERM);
            }
            if p.fs_shared() {
                return Err(EINVAL);
            }
            p.fs.with(|fs| crate::mount::enter_ns(&ns, fs));
            let mut n = (*old).clone();
            n.mnt = ns;
            *p.nsproxy.lock() = Arc::new(n);
        }
        NsRef::Uts(ns) => {
            if !userns::ns_capable(&creds, &ns.user_ns, CAP_SYS_ADMIN) || !me_cap(CAP_SYS_ADMIN) {
                return Err(EPERM);
            }
            let mut n = (*old).clone();
            n.uts = ns;
            *p.nsproxy.lock() = Arc::new(n);
        }
        NsRef::Ipc(ns) => {
            if !userns::ns_capable(&creds, &ns.user_ns, CAP_SYS_ADMIN) || !me_cap(CAP_SYS_ADMIN) {
                return Err(EPERM);
            }
            let mut n = (*old).clone();
            n.ipc = ns;
            *p.nsproxy.lock() = Arc::new(n);
        }
        NsRef::Cgroup(ns) => {
            if !userns::ns_capable(&creds, &ns.user_ns, CAP_SYS_ADMIN) || !me_cap(CAP_SYS_ADMIN) {
                return Err(EPERM);
            }
            let mut n = (*old).clone();
            n.cgroup = ns;
            *p.nsproxy.lock() = Arc::new(n);
        }
        NsRef::Pid(ns) => {
            // `pidns_install` : seulement l'espace actif ou l'un de ses descendants ; ne change
            // que l'espace des futurs enfants.
            if !userns::ns_capable(&creds, &ns.user_ns, CAP_SYS_ADMIN) || !me_cap(CAP_SYS_ADMIN) {
                return Err(EPERM);
            }
            if !p.pid_ns.is_ancestor_of(&ns) {
                return Err(EINVAL);
            }
            let mut n = (*old).clone();
            n.pid_for_children = ns;
            *p.nsproxy.lock() = Arc::new(n);
        }
        NsRef::Net(ns) => {
            // `netns_install` : les sockets déjà ouvertes restent dans leur espace.
            if !userns::ns_capable(&creds, &ns.user_ns, CAP_SYS_ADMIN) || !me_cap(CAP_SYS_ADMIN) {
                return Err(EPERM);
            }
            let mut n = (*old).clone();
            n.net = ns;
            *p.nsproxy.lock() = Arc::new(n);
        }
        // Un seul espace temps : rejoindre l'espace actuel ne change rien.
        NsRef::Time(ns) => {
            if !userns::ns_capable(&creds, &ns.user_ns, CAP_SYS_ADMIN) {
                return Err(EPERM);
            }
        }
    }
    Ok(0)
}
