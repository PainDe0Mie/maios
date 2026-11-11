//! Montages et espaces de montage (`mount_namespaces(7)`), racine et dossier courant par
//! processus (`fs_struct`), résolution des chemins à travers les montages.
//!
//! Modèle de Linux (`fs/namespace.c`, `fs/namei.c`) :
//! - un **montage** ([`Mount`]) montre un nœud d'un système de fichiers (sa racine) ; il est
//!   accroché à un montage parent en un **point de montage**, chemin relatif à la racine du
//!   parent. L'arbre est global (`TREE`), clé (parent, point de montage), comme la table de
//!   hachage `mount_hashtable` ; un montage appartient à un espace ([`MntNs`]) ou à aucun
//!   (détaché par `MNT_DETACH`, encore tenu par ceux qui y sont).
//! - une **position** ([`Loc`]) = (montage, nœud, chemin dans le montage) : racine et dossier
//!   courant d'un processus ([`FsStruct`], partagé par `CLONE_FS`). Leur chemin visible se
//!   recalcule à chaque fois en remontant les montages (`d_path`) : un `pivot_root` ou un
//!   `MS_MOVE` déplace donc ce que voient les processus sans rien réécrire.
//! - la résolution ([`resolve`]) part de la racine du processus, garde la pile des dossiers
//!   traversés (`..` physique, jamais au-dessus de la racine), passe dans le montage du dessus à
//!   chaque point de montage (`follow_mount`), suit les liens symboliques (40 au plus, absolus
//!   depuis la racine du processus, refusés sur un montage `nosymfollow`). Les systèmes de
//!   fichiers produits (`proc`, `sysfs`, `devpts`) sont des montages virtuels : la suite du
//!   chemin y est rendue telle quelle ([`Target::Virtual`]) à `procfs` et `pty`.
//!
//! Appels : `mount` (nouveau tmpfs/proc/sysfs/devpts, `MS_BIND` [`MS_REC`], `MS_REMOUNT`
//! [`MS_BIND`], `MS_MOVE`, `MS_SHARED`/`MS_PRIVATE`/`MS_SLAVE`/`MS_UNBINDABLE` [`MS_REC`]),
//! `umount2` (`MNT_DETACH`, `MNT_FORCE`, `UMOUNT_NOFOLLOW`, `EBUSY` si occupé), `pivot_root`
//! (y compris `pivot_root(".", ".")`), `chroot`. Droits : `CAP_SYS_ADMIN` sur l'espace
//! utilisateur propriétaire de l'espace de montage ; dans un espace utilisateur non initial,
//! seuls les systèmes de fichiers `FS_USERNS_MOUNT` (tmpfs, devpts ; proc et sysfs avec les
//! droits sur les espaces pid et réseau), et les montages copiés d'un espace plus privilégié
//! sont **verrouillés** (`lock_mnt_tree` : drapeaux figés, pas de démontage qui découvrirait ce
//! qu'ils cachent).
//!
//! Limites connues : pas encore de propagation des événements entre montages partagés (les
//! types `shared`/`slave` sont tenus et affichés) ; un démontage dans un espace ne cache pas un
//! système de fichiers inséré dans l'arbre global du VFS (`/tmp`, `/run`, `/dev/shm`), qui
//! reste visible par le montage racine.

use alloc::{
    collections::BTreeMap,
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use fs_node::{DirRef, FileOrDir, FsNode, MetaChange};

use crate::linux::errno::*;
use crate::userns::{self, UserNs, CAP_SYS_ADMIN, CAP_SYS_CHROOT};
use crate::Thread;

// --- Drapeaux ----------------------------------------------------------------------------------

pub const MS_RDONLY: u64 = 1;
pub const MS_NOSUID: u64 = 2;
pub const MS_NODEV: u64 = 4;
pub const MS_NOEXEC: u64 = 8;
pub const MS_REMOUNT: u64 = 32;
pub const MS_NOSYMFOLLOW: u64 = 256;
pub const MS_NOATIME: u64 = 1024;
pub const MS_NODIRATIME: u64 = 2048;
pub const MS_BIND: u64 = 4096;
pub const MS_MOVE: u64 = 8192;
pub const MS_REC: u64 = 16384;
pub const MS_SILENT: u64 = 32768;
pub const MS_UNBINDABLE: u64 = 1 << 17;
pub const MS_PRIVATE: u64 = 1 << 18;
pub const MS_SLAVE: u64 = 1 << 19;
pub const MS_SHARED: u64 = 1 << 20;
pub const MS_RELATIME: u64 = 1 << 21;
pub const MS_STRICTATIME: u64 = 1 << 24;
pub const MS_LAZYTIME: u64 = 1 << 25;
const MS_MGC_VAL: u64 = 0xC0ED_0000;
const MS_MGC_MSK: u64 = 0xffff_0000;
const MS_PROPAGATION: u64 = MS_SHARED | MS_PRIVATE | MS_SLAVE | MS_UNBINDABLE;

/// Drapeaux d'un montage (`mnt_flags`).
pub const MNT_NOSUID: u32 = 0x01;
pub const MNT_NODEV: u32 = 0x02;
pub const MNT_NOEXEC: u32 = 0x04;
pub const MNT_NOATIME: u32 = 0x08;
pub const MNT_NODIRATIME: u32 = 0x10;
pub const MNT_RELATIME: u32 = 0x20;
pub const MNT_READONLY: u32 = 0x40;
pub const MNT_NOSYMFOLLOW: u32 = 0x80;
const MNT_USER_SETTABLE: u32 = 0xff;
const MNT_ATIME_MASK: u32 = MNT_NOATIME | MNT_NODIRATIME | MNT_RELATIME;
pub const MNT_LOCK_ATIME: u32 = 1 << 16;
pub const MNT_LOCK_READONLY: u32 = 1 << 17;
pub const MNT_LOCK_NODEV: u32 = 1 << 18;
pub const MNT_LOCK_NOSUID: u32 = 1 << 19;
pub const MNT_LOCK_NOEXEC: u32 = 1 << 20;
/// Le démonter découvrirait ce qu'il cache à un espace moins privilégié.
pub const MNT_LOCKED: u32 = 1 << 21;

const MNT_FORCE: u64 = 1;
const MNT_DETACH: u64 = 2;
const MNT_EXPIRE: u64 = 4;
const UMOUNT_NOFOLLOW: u64 = 8;

// --- Systèmes de fichiers et montages ---------------------------------------------------------------

/// Systèmes de fichiers produits à la lecture : la suite du chemin va à `procfs` ou `pty`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VirtFs {
    Proc,
    Sys,
    Devpts,
}

impl VirtFs {
    /// Préfixe des chemins canoniques que connaissent `procfs` et `pty`.
    pub fn prefix(self) -> &'static str {
        match self {
            VirtFs::Proc => "/proc",
            VirtFs::Sys => "/sys",
            VirtFs::Devpts => "/dev/pts",
        }
    }
}

/// Instance de système de fichiers (`super_block`).
pub struct Super {
    pub fstype: String,
    pub source: String,
    /// Mineur de `st_dev` affiché par `mountinfo` (`0:N`).
    pub minor: u32,
    /// Options propres (`size=…,mode=…`), sans `rw`/`ro`.
    pub opts: spin::Mutex<String>,
    pub ro: AtomicBool,
    /// Monté depuis un espace utilisateur non initial : ses nœuds de périphérique ne s'ouvrent
    /// pas (`SB_I_NODEV`).
    pub nodev: bool,
    pub virt: Option<VirtFs>,
    /// `proc` : espace pid dont il montre les processus (celui du processus qui l'a monté,
    /// `proc_fs_info->pid_ns`) ; `None` : l'espace initial.
    pub pid_ns: Option<Arc<crate::pidns::PidNs>>,
    /// `devpts` : son instance (numéros d'esclaves et options propres) ; `None` : l'initiale.
    pub pts: Option<Arc<crate::pty::PtsFs>>,
}

static NEXT_MINOR: AtomicU32 = AtomicU32::new(20);

impl Super {
    pub fn new(fstype: &str, source: &str, opts: &str, virt: Option<VirtFs>, nodev: bool) -> Arc<Super> {
        Arc::new(Super {
            fstype: fstype.into(),
            source: source.into(),
            minor: NEXT_MINOR.fetch_add(1, Ordering::Relaxed),
            opts: spin::Mutex::new(opts.into()),
            ro: AtomicBool::new(false),
            nodev,
            virt,
            pid_ns: None,
            pts: None,
        })
    }

    /// Superbloc d'un `devpts` portant l'instance `fs`.
    pub fn new_devpts(source: &str, nodev: bool, fs: Arc<crate::pty::PtsFs>) -> Arc<Super> {
        let mut sb = Super::new("devpts", source, &fs.opts_text(), Some(VirtFs::Devpts), nodev);
        Arc::get_mut(&mut sb).expect("superbloc neuf").pts = Some(fs);
        sb
    }

    /// Instance de ce `devpts`.
    pub fn devpts(&self) -> Arc<crate::pty::PtsFs> {
        self.pts.clone().unwrap_or_else(crate::pty::init_pts)
    }

    /// Superbloc d'un `proc` lié à l'espace pid `ns`.
    pub fn new_proc(source: &str, nodev: bool, ns: Arc<crate::pidns::PidNs>) -> Arc<Super> {
        let mut sb = Super::new("proc", source, "", Some(VirtFs::Proc), nodev);
        Arc::get_mut(&mut sb).expect("superbloc neuf").pid_ns = Some(ns);
        sb
    }

    /// Espace pid que montre ce `proc`.
    pub fn proc_pid_ns(&self) -> Arc<crate::pidns::PidNs> {
        self.pid_ns.clone().unwrap_or_else(crate::pidns::init_pid_ns)
    }
}

/// Ce que montre un montage à sa racine.
#[derive(Clone)]
pub enum MountRoot {
    Node(FileOrDir),
    Virtual(VirtFs),
}

pub struct Mount {
    pub id: u32,
    pub sb: Arc<Super>,
    pub root: MountRoot,
    /// Chemin de la racine dans son système de fichiers (`/` sauf montage lié d'un sous-dossier).
    pub root_path: String,
    pub flags: AtomicU32,
    /// Espace de montage (numéro d'inode), 0 : détaché.
    pub ns: AtomicU64,
    /// Nombre de montages accrochés (lecture sans verrou dans la résolution).
    nchild: AtomicU32,
}

static NEXT_MNT_ID: AtomicU32 = AtomicU32::new(20);
static NEXT_GROUP: AtomicU32 = AtomicU32::new(1);

impl Mount {
    fn new(sb: Arc<Super>, root: MountRoot, root_path: String, flags: u32, ns: u64) -> Arc<Mount> {
        Arc::new(Mount {
            id: NEXT_MNT_ID.fetch_add(1, Ordering::Relaxed),
            sb,
            root,
            root_path,
            flags: AtomicU32::new(flags),
            ns: AtomicU64::new(ns),
            nchild: AtomicU32::new(0),
        })
    }

    pub fn flags(&self) -> u32 {
        self.flags.load(Ordering::Acquire)
    }

    /// Écritures refusées (`EROFS`) : montage ou système de fichiers en lecture seule.
    pub fn readonly(&self) -> bool {
        self.flags() & MNT_READONLY != 0 || self.sb.ro.load(Ordering::Acquire)
    }

    pub fn nosuid(&self) -> bool {
        self.flags() & MNT_NOSUID != 0
    }

    pub fn noexec(&self) -> bool {
        self.flags() & MNT_NOEXEC != 0
    }

    /// Nœuds de périphérique interdits (`may_open_dev`).
    pub fn nodev(&self) -> bool {
        self.flags() & MNT_NODEV != 0 || self.sb.nodev
    }
}

/// Place d'un montage dans l'arbre.
struct TNode {
    mnt: Arc<Mount>,
    parent: Option<u32>,
    mountpoint: String,
    /// Point de montage (relatif à la racine de ce montage) → montages empilés (le dernier
    /// au-dessus).
    children: BTreeMap<String, Vec<u32>>,
    /// Groupe de pairs (`shared:N`), 0 : aucun.
    shared: u32,
    /// Groupe maître (`master:N`), 0 : aucun.
    master: u32,
    unbindable: bool,
}

struct Tree {
    nodes: BTreeMap<u32, TNode>,
}

static TREE: sync_preemption::RwLock<Tree> = sync_preemption::RwLock::new(Tree { nodes: BTreeMap::new() });

impl Tree {
    fn attach(&mut self, child: &Arc<Mount>, parent: u32, mountpoint: &str) {
        if let Some(n) = self.nodes.get_mut(&child.id) {
            n.parent = Some(parent);
            n.mountpoint = mountpoint.into();
        }
        if let Some(p) = self.nodes.get_mut(&parent) {
            p.children.entry(mountpoint.into()).or_default().push(child.id);
            p.mnt.nchild.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn detach(&mut self, id: u32) {
        let Some((parent, mp)) = self.nodes.get(&id).and_then(|n| Some((n.parent?, n.mountpoint.clone()))) else { return };
        if let Some(p) = self.nodes.get_mut(&parent) {
            if let Some(v) = p.children.get_mut(&mp) {
                v.retain(|&c| c != id);
                if v.is_empty() {
                    p.children.remove(&mp);
                }
                p.mnt.nchild.fetch_sub(1, Ordering::AcqRel);
            }
        }
        if let Some(n) = self.nodes.get_mut(&id) {
            n.parent = None;
        }
    }

    fn insert(&mut self, mnt: Arc<Mount>) {
        self.nodes.insert(mnt.id, TNode { mnt, parent: None, mountpoint: String::from("/"), children: BTreeMap::new(), shared: 0, master: 0, unbindable: false });
    }

    /// Montages de la sous-arborescence de `id` (lui compris), en profondeur.
    fn subtree(&self, id: u32) -> Vec<u32> {
        let mut out = alloc::vec![id];
        let mut i = 0;
        while i < out.len() {
            if let Some(n) = self.nodes.get(&out[i]) {
                for v in n.children.values() {
                    out.extend(v.iter().copied());
                }
            }
            i += 1;
        }
        out
    }

    /// Montage du dessus en `(parent, inner)`, puis ceux empilés sur sa racine.
    fn top_at(&self, parent: u32, inner: &str) -> Option<Arc<Mount>> {
        let mut m = self.nodes.get(&parent)?.children.get(inner)?.last().copied()?;
        loop {
            match self.nodes.get(&m).and_then(|n| n.children.get("/")).and_then(|v| v.last().copied()) {
                Some(up) => m = up,
                None => return self.nodes.get(&m).map(|n| n.mnt.clone()),
            }
        }
    }

    /// Chemin de `(mnt, inner)` vu depuis la racine `root` (`d_path`) ; `None` hors d'atteinte.
    fn path_of(&self, mnt: u32, inner: &str, root: &Loc) -> Option<String> {
        let mut m = mnt;
        let mut p = String::from(inner);
        loop {
            if m == root.mnt.id {
                return strip_under(&p, &root.inner);
            }
            let n = self.nodes.get(&m)?;
            let parent = n.parent?;
            p = join(&n.mountpoint, &p);
            m = parent;
        }
    }
}

/// `p` relatif à `base` (tous deux absolus) si `p` est `base` ou dessous.
fn strip_under(p: &str, base: &str) -> Option<String> {
    if base == "/" {
        return Some(String::from(p));
    }
    if p == base {
        return Some(String::from("/"));
    }
    p.strip_prefix(base).filter(|r| r.starts_with('/')).map(String::from)
}

fn join(base: &str, rest: &str) -> String {
    if rest == "/" || rest.is_empty() {
        return String::from(base);
    }
    if base == "/" {
        return String::from(rest);
    }
    let mut s = String::from(base);
    if !rest.starts_with('/') {
        s.push('/');
    }
    s.push_str(rest);
    s
}

fn push_comp(base: &str, comp: &str) -> String {
    let mut s = String::from(base);
    if !s.ends_with('/') {
        s.push('/');
    }
    s.push_str(comp);
    s
}

// --- Espaces de montage ---------------------------------------------------------------------------

pub struct MntNs {
    pub ino: u64,
    pub user_ns: Arc<UserNs>,
    root: spin::Mutex<Arc<Mount>>,
}

impl MntNs {
    pub fn root(&self) -> Arc<Mount> {
        self.root.lock().clone()
    }

    /// Montages de l'espace, dans l'ordre de leur création.
    fn mounts(&self) -> Vec<u32> {
        let tree = TREE.read();
        let mut v: Vec<u32> = tree.subtree(self.root().id).into_iter().filter(|id| tree.nodes.get(id).is_some_and(|n| n.mnt.ns.load(Ordering::Acquire) == self.ino)).collect();
        v.sort_unstable();
        v
    }
}

static INIT_NS: spin::Once<Arc<MntNs>> = spin::Once::new();

// --- Positions et fs_struct -----------------------------------------------------------------------

/// Nœud d'une position.
#[derive(Clone)]
pub enum LocNode {
    Dir(DirRef),
    File(fs_node::FileRef),
    Virtual(VirtFs),
}

/// Position dans l'arbre des montages (`struct path`).
#[derive(Clone)]
pub struct Loc {
    pub mnt: Arc<Mount>,
    pub node: LocNode,
    /// Chemin dans le montage, depuis sa racine.
    pub inner: String,
}

impl Loc {
    fn of_mount(m: &Arc<Mount>) -> Loc {
        let node = match &m.root {
            MountRoot::Node(FileOrDir::Dir(d)) => LocNode::Dir(d.clone()),
            MountRoot::Node(FileOrDir::File(f)) => LocNode::File(f.clone()),
            MountRoot::Virtual(v) => LocNode::Virtual(*v),
        };
        Loc { mnt: m.clone(), node, inner: String::from("/") }
    }

    fn same(&self, other: &Loc) -> bool {
        self.mnt.id == other.mnt.id && self.inner == other.inner
    }
}

/// Racine et dossier courant d'un processus (`struct fs_struct`).
#[derive(Clone)]
pub struct FsStruct {
    pub root: Loc,
    pub cwd: Loc,
}

impl FsStruct {
    /// Chemin visible du dossier courant ; `(unreachable)…` s'il est hors de la racine
    /// (`getcwd` de Linux après un `chroot` sans `chdir`).
    pub fn cwd_path(&self) -> String {
        view_path(&self.cwd, &self.root).unwrap_or_else(|| format!("(unreachable){}", self.cwd.inner))
    }

    /// Le processus est-il enfermé (`current_chrooted`) : sa racine n'est pas celle de son
    /// espace de montage.
    pub fn chrooted(&self, ns: &MntNs) -> bool {
        self.root.mnt.id != ns.root().id || self.root.inner != "/"
    }
}

/// `fs_struct` d'un processus : partagé par `CLONE_FS`, remplaçable par `unshare(CLONE_FS)`
/// sans toucher aux processus qui le partageaient.
pub struct FsRef(spin::Mutex<Arc<spin::Mutex<FsStruct>>>);

impl FsRef {
    pub fn new(fs: FsStruct) -> FsRef {
        FsRef(spin::Mutex::new(Arc::new(spin::Mutex::new(fs))))
    }

    fn inner(&self) -> Arc<spin::Mutex<FsStruct>> {
        self.0.lock().clone()
    }

    /// Même `fs_struct` (`CLONE_FS`).
    pub fn share(&self) -> FsRef {
        FsRef(spin::Mutex::new(self.inner()))
    }

    pub fn shared(&self) -> bool {
        Arc::strong_count(&self.0.lock()) > 1
    }

    /// Copie du contenu.
    pub fn get(&self) -> FsStruct {
        self.inner().lock().clone()
    }

    pub fn root(&self) -> Loc {
        self.inner().lock().root.clone()
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut FsStruct) -> R) -> R {
        let inner = self.inner();
        let mut g = inner.lock();
        f(&mut g)
    }

    /// Remplace le contenu pour tous ceux qui le partagent.
    pub fn set(&self, fs: FsStruct) {
        self.with(|f| *f = fs);
    }

    /// `fs_struct` propre à ce processus désormais (`unshare_fs`).
    pub fn unshare(&self, fs: FsStruct) {
        *self.0.lock() = Arc::new(spin::Mutex::new(fs));
    }
}

/// Chemin visible de `loc` depuis `root`.
pub fn view_path(loc: &Loc, root: &Loc) -> Option<String> {
    TREE.read().path_of(loc.mnt.id, &loc.inner, root)
}

// --- Démarrage ----------------------------------------------------------------------------------------

fn flags_text(f: u32) -> String {
    let mut s = String::from(if f & MNT_READONLY != 0 { "ro" } else { "rw" });
    for (bit, name) in [(MNT_NOSUID, "nosuid"), (MNT_NODEV, "nodev"), (MNT_NOEXEC, "noexec"), (MNT_NOATIME, "noatime"), (MNT_NODIRATIME, "nodiratime"), (MNT_RELATIME, "relatime"), (MNT_NOSYMFOLLOW, "nosymfollow")] {
        if f & bit != 0 {
            s.push(',');
            s.push_str(name);
        }
    }
    s
}

/// Espace de montage initial, créé au premier besoin : racine du VFS, `/proc`, `/sys`.
fn make_init_ns() -> Arc<MntNs> {
    let ns_ino = userns::INIT_MNT_INO;
    let root_sb = Super::new("rootfs", "rootfs", "", None, false);
    let root = Mount::new(root_sb, MountRoot::Node(FileOrDir::Dir(root::get_root().clone())), String::from("/"), MNT_RELATIME, ns_ino);
    let ns = Arc::new(MntNs { ino: ns_ino, user_ns: userns::init_user_ns(), root: spin::Mutex::new(root.clone()) });
    let mut tree = TREE.write();
    tree.insert(root.clone());
    let nsd = MNT_NOSUID | MNT_NODEV;
    for (mp, fstype, v) in [("/proc", "proc", VirtFs::Proc), ("/sys", "sysfs", VirtFs::Sys)] {
        let m = Mount::new(Super::new(fstype, fstype, "", Some(v), false), MountRoot::Virtual(v), String::from("/"), nsd | MNT_NOEXEC | MNT_RELATIME, ns_ino);
        tree.insert(m.clone());
        tree.attach(&m, root.id, mp);
    }
    ns
}

/// Accroche à `parent` (espace initial) un montage en `mp`, s'il n'y en a pas déjà un.
fn add_init_mount(parent: &Arc<Mount>, mp: &str, sb: Arc<Super>, r: MountRoot, flags: u32) -> Option<Arc<Mount>> {
    let mut tree = TREE.write();
    if let Some(m) = tree.top_at(parent.id, mp) {
        return Some(m);
    }
    let m = Mount::new(sb, r, String::from("/"), flags, userns::INIT_MNT_INO);
    tree.insert(m.clone());
    tree.attach(&m, parent.id, mp);
    Some(m)
}

/// Inscrit les montages de démarrage dans l'espace initial : `/dev` (tmpfs de périphériques),
/// `/dev/pts`, `/dev/shm`, `/tmp`. Appelé par `mount_sysroot` une fois ces dossiers créés.
pub fn init(dev: Option<DirRef>, shm: Option<DirRef>, tmp: Option<DirRef>) {
    let root = init_ns().root();
    let nsd = MNT_NOSUID | MNT_NODEV;
    if let Some(d) = dev {
        if let Some(devm) = add_init_mount(&root, "/dev", Super::new("devtmpfs", "udev", "mode=755", None, false), MountRoot::Node(FileOrDir::Dir(d)), MNT_NOSUID | MNT_RELATIME) {
            add_init_mount(&devm, "/pts", Super::new_devpts("devpts", false, crate::pty::init_pts()), MountRoot::Virtual(VirtFs::Devpts), MNT_NOSUID | MNT_NOEXEC | MNT_RELATIME);
            if let Some(s) = shm {
                add_init_mount(&devm, "/shm", Super::new("tmpfs", "tmpfs", "", None, false), MountRoot::Node(FileOrDir::Dir(s)), nsd | MNT_RELATIME);
            }
        }
    }
    if let Some(t) = tmp {
        add_init_mount(&root, "/tmp", Super::new("tmpfs", "tmpfs", "", None, false), MountRoot::Node(FileOrDir::Dir(t)), nsd | MNT_RELATIME);
    }
}

/// Inscrit un système de fichiers inséré plus tard dans le VFS global (`/run`) comme montage de
/// l'espace initial, accroché à la racine.
pub fn register_root_mount(mountpoint: &str, dir: DirRef, fstype: &str, flags: u32) {
    let root = init_ns().root();
    add_init_mount(&root, mountpoint, Super::new(fstype, fstype, "", None, false), MountRoot::Node(FileOrDir::Dir(dir)), flags);
}

/// Racine de l'espace initial (services du noyau).
pub fn init_root() -> Loc {
    Loc::of_mount(&init_ns().root())
}

pub fn init_ns() -> Arc<MntNs> {
    INIT_NS.call_once(make_init_ns).clone()
}

/// `fs_struct` d'un processus lancé par une tâche native : racine de l'espace initial, dossier
/// courant `cwd` (chemin absolu), ou la racine s'il est introuvable.
pub fn init_fs(cwd: &str) -> FsStruct {
    let root = Loc::of_mount(&init_ns().root());
    let cwd = resolve(&root, cwd, true).ok().and_then(|f| f.loc()).filter(|l| !matches!(l.node, LocNode::File(_))).unwrap_or_else(|| root.clone());
    FsStruct { root, cwd }
}

// --- Résolution -----------------------------------------------------------------------------------

pub enum Target {
    Node(FileOrDir),
    /// Chemin dans un système de fichiers produit, absolu depuis sa racine.
    Virtual(VirtFs, String),
}

pub struct Found {
    pub target: Target,
    pub mnt: Arc<Mount>,
    /// Chemin dans le montage.
    pub inner: String,
    /// Chemin visible (liens résolus), depuis la racine du processus.
    pub path: String,
}

impl Found {
    /// Position correspondante (`None` pour un fichier d'un système produit).
    pub fn loc(&self) -> Option<Loc> {
        let node = match &self.target {
            Target::Node(FileOrDir::Dir(d)) => LocNode::Dir(d.clone()),
            Target::Node(FileOrDir::File(f)) => LocNode::File(f.clone()),
            Target::Virtual(v, _) => LocNode::Virtual(*v),
        };
        Some(Loc { mnt: self.mnt.clone(), node, inner: self.inner.clone() })
    }

    /// Chemin canonique d'un nœud produit (`/proc/…`, `/sys/…`, `/dev/pts/…`).
    pub fn virtual_path(&self) -> Option<String> {
        match &self.target {
            Target::Virtual(v, sub) => Some(if sub == "/" { String::from(v.prefix()) } else { format!("{}{}", v.prefix(), sub) }),
            _ => None,
        }
    }

    /// Le résultat est la racine de son montage.
    pub fn is_mount_root(&self) -> bool {
        self.inner == "/"
    }
}

struct Entry {
    node: LocNode,
    name: String,
    mnt: Arc<Mount>,
    inner: String,
}

impl Entry {
    fn of(loc: &Loc) -> Entry {
        Entry { node: loc.node.clone(), name: String::new(), mnt: loc.mnt.clone(), inner: loc.inner.clone() }
    }
}

/// Montage du dessus accroché en `(parent, inner)`.
fn mount_at(parent: &Arc<Mount>, inner: &str) -> Option<Arc<Mount>> {
    if parent.nchild.load(Ordering::Acquire) == 0 {
        return None;
    }
    TREE.read().top_at(parent.id, inner)
}

fn canonical(stack: &[Entry]) -> String {
    let mut p = String::new();
    for e in stack.iter().skip(1) {
        p.push('/');
        p.push_str(&e.name);
    }
    if p.is_empty() {
        p.push('/');
    }
    p
}

fn sub_of(mnt: &Mount, inner: &str) -> String {
    join(&mnt.root_path, inner)
}

fn found_of(stack: &[Entry]) -> Found {
    let top = stack.last().unwrap();
    let target = match &top.node {
        LocNode::Dir(d) => Target::Node(FileOrDir::Dir(d.clone())),
        LocNode::File(f) => Target::Node(FileOrDir::File(f.clone())),
        LocNode::Virtual(v) => Target::Virtual(*v, sub_of(&top.mnt, &top.inner)),
    };
    Found { target, mnt: top.mnt.clone(), inner: top.inner.clone(), path: canonical(stack) }
}

/// Règles de résolution d'`openat2` (`RESOLVE_*`, `struct open_how`), comme `LOOKUP_*` de
/// `fs/namei.c`. Par défaut : aucune restriction (chemin ordinaire).
#[derive(Clone, Copy, Default)]
pub struct Scope {
    /// `RESOLVE_NO_XDEV` : tout passage d'un montage à un autre → `EXDEV`.
    pub no_xdev: bool,
    /// `RESOLVE_NO_MAGICLINKS` : liens magiques de `/proc` → `ELOOP` (vérifié par l'appelant,
    /// [`crate::procfs::links_in`]).
    pub no_magiclinks: bool,
    /// `RESOLVE_NO_SYMLINKS` : tout lien suivi → `ELOOP`.
    pub no_symlinks: bool,
    /// `RESOLVE_BENEATH` : ne jamais sortir du dossier de départ (`..` au départ, chemin ou lien
    /// absolu → `EXDEV`).
    pub beneath: bool,
    /// `RESOLVE_IN_ROOT` : le dossier de départ tient lieu de racine (`..` y reste, chemins et
    /// liens absolus en partent), comme un `chroot` le temps de la résolution.
    pub in_root: bool,
}

impl Scope {
    /// Résolution bornée au dossier de départ (`LOOKUP_IS_SCOPED`).
    pub fn scoped(&self) -> bool {
        self.beneath || self.in_root
    }
}

/// Résultat d'une résolution qui admet un dernier composant absent (`O_CREAT`).
pub enum Resolved {
    Found(Found),
    /// Tout le chemin existe sauf le dernier nom : `parent` est son dossier (liens résolus).
    Missing { parent: Found, name: String },
}

fn comps_of(p: &str) -> Vec<String> {
    p.split('/').rev().filter(|c| !c.is_empty() && *c != ".").map(String::from).collect()
}

/// Cœur de la résolution (`link_path_walk` + `walk_component`) : consomme `pending` (composants
/// en ordre inverse) à partir de la pile `stack`. `floor` : profondeur sous laquelle `..` ne
/// descend pas (1 = racine du processus ; dossier de départ pour une résolution bornée). Rend
/// `Some(nom)` si `allow_missing` et que seul le dernier composant manque.
fn walk_stack(stack: &mut Vec<Entry>, mut pending: Vec<String>, follow_last: bool, floor: usize, s: &Scope, allow_missing: bool) -> Result<Option<String>, i64> {
    let mut links = 0usize;
    while let Some(comp) = pending.pop() {
        if comp == ".." {
            if stack.len() > floor {
                let from = stack.pop().unwrap().mnt;
                if s.no_xdev && !Arc::ptr_eq(&from, &stack.last().unwrap().mnt) {
                    return Err(EXDEV);
                }
            } else if s.beneath {
                return Err(EXDEV);
            }
            continue;
        }
        let last = pending.is_empty();
        let top = stack.last().unwrap();
        let inner = push_comp(&top.inner, &comp);
        // Point de montage : on passe dans le montage du dessus.
        if let Some(m) = mount_at(&top.mnt, &inner) {
            if s.no_xdev {
                return Err(EXDEV);
            }
            let mut e = Entry::of(&Loc::of_mount(&m));
            e.name = comp;
            if let LocNode::File(_) = e.node {
                if !last {
                    return Err(ENOTDIR);
                }
            }
            stack.push(e);
            continue;
        }
        match top.node.clone() {
            LocNode::Virtual(v) => {
                // Système produit : la suite du chemin est rendue telle quelle.
                stack.push(Entry { node: LocNode::Virtual(v), name: comp, mnt: top.mnt.clone(), inner });
            }
            LocNode::File(_) => return Err(ENOTDIR),
            LocNode::Dir(dir) => {
                let got = dir.lock().get_with_link(&comp);
                let (child, link) = match got {
                    Some(c) => c,
                    None if last && allow_missing => return Ok(Some(comp)),
                    None => return Err(ENOENT),
                };
                if let (FileOrDir::File(_), Some(target)) = (&child, link) {
                    if !last || follow_last {
                        if top.mnt.flags() & MNT_NOSYMFOLLOW != 0 || s.no_symlinks {
                            return Err(ELOOP);
                        }
                        links += 1;
                        if links > path::resolve::MAX_SYMLINKS {
                            return Err(ELOOP);
                        }
                        if target.starts_with('/') {
                            // Lien absolu : à la racine du processus, ou à celle de la
                            // résolution (`IN_ROOT`) ; interdit sous `BENEATH`.
                            if s.beneath {
                                return Err(EXDEV);
                            }
                            let from = top.mnt.clone();
                            stack.truncate(if s.in_root { floor } else { 1 });
                            if s.no_xdev && !Arc::ptr_eq(&from, &stack.last().unwrap().mnt) {
                                return Err(EXDEV);
                            }
                        }
                        pending.extend(comps_of(&target));
                        continue;
                    }
                }
                let mnt = top.mnt.clone();
                match child {
                    FileOrDir::Dir(d) => stack.push(Entry { node: LocNode::Dir(d), name: comp, mnt, inner }),
                    FileOrDir::File(f) => {
                        if !last {
                            return Err(ENOTDIR);
                        }
                        stack.push(Entry { node: LocNode::File(f), name: comp, mnt, inner });
                    }
                }
            }
        }
    }
    Ok(None)
}

/// Résout `abs` (chemin absolu vu par le processus) depuis sa racine `root` ; le dernier
/// composant n'est suivi (lien) que si `follow_last`.
pub fn resolve(root: &Loc, abs: &str, follow_last: bool) -> Result<Found, i64> {
    let mut stack: Vec<Entry> = alloc::vec![Entry::of(root)];
    walk_stack(&mut stack, comps_of(abs), follow_last, 1, &Scope::default(), false)?;
    Ok(found_of(&stack))
}

/// Résolution d'`openat2` : `path` tel que donné (`..` physique), depuis le dossier `base`
/// (chemin visible absolu du descripteur de départ), selon `s`. Les chemins rendus
/// (`Found::path`) partent toujours de la racine du processus.
pub fn resolve_scoped(root: &Loc, base: &str, path: &str, follow_last: bool, s: Scope, allow_missing: bool) -> Result<Resolved, i64> {
    let mut stack: Vec<Entry> = alloc::vec![Entry::of(root)];
    walk_stack(&mut stack, comps_of(base), true, 1, &Scope::default(), false)?;
    if let LocNode::File(_) = stack.last().unwrap().node {
        return Err(ENOTDIR);
    }
    let floor = if s.scoped() { stack.len() } else { 1 };
    if path.starts_with('/') {
        if s.beneath {
            return Err(EXDEV);
        }
        let from = stack.last().unwrap().mnt.clone();
        stack.truncate(floor);
        if s.no_xdev && !Arc::ptr_eq(&from, &stack.last().unwrap().mnt) {
            return Err(EXDEV);
        }
    }
    match walk_stack(&mut stack, comps_of(path), follow_last, floor, &s, allow_missing)? {
        None => Ok(Resolved::Found(found_of(&stack))),
        Some(name) => Ok(Resolved::Missing { parent: found_of(&stack), name }),
    }
}

/// Comme Linux pour `umount` (`LOOKUP_MOUNTPOINT`) : passe dans les montages empilés sur le
/// résultat, même pour `.`.
fn follow_top(f: Found) -> Found {
    let mut f = f;
    loop {
        let up = if f.inner == "/" { mount_at(&f.mnt, "/") } else { mount_at(&f.mnt, &f.inner) };
        let Some(m) = up else { return f };
        let loc = Loc::of_mount(&m);
        let target = match &loc.node {
            LocNode::Dir(d) => Target::Node(FileOrDir::Dir(d.clone())),
            LocNode::File(x) => Target::Node(FileOrDir::File(x.clone())),
            LocNode::Virtual(v) => Target::Virtual(*v, m.root_path.clone()),
        };
        f = Found { target, mnt: m, inner: String::from("/"), path: f.path };
    }
}

// --- Contexte du processus --------------------------------------------------------------------------

fn fs_of(t: &Thread) -> FsStruct {
    t.process.fs.get()
}

fn ns_of(t: &Thread) -> Arc<MntNs> {
    crate::ns::of(&t.process).mnt.clone()
}

/// Résolution d'un chemin visible du processus.
pub fn lookup(t: &Thread, abs: &str, follow: bool) -> Result<Found, i64> {
    let root = t.process.fs.root();
    resolve(&root, abs, follow)
}

/// Résolution des appels de montage (`user_path_at(LOOKUP_FOLLOW)`) : comme [`lookup`], en
/// suivant aussi les liens magiques de `/proc` traversés (`exe`, `cwd`, `root`, `fd/<n>`) —
/// bubblewrap monte `/proc/self/fd/<n>` d'un `O_PATH` ouvert par `openat2`. Un lien vers un
/// objet sans chemin (`ns/<x>`, tube) reste un nœud de `/proc`.
pub fn lookup_magic(t: &Thread, abs: &str, follow: bool) -> Result<Found, i64> {
    let mut abs = String::from(abs);
    for _ in 0..8 {
        let f = lookup(t, &abs, follow)?;
        let Target::Virtual(VirtFs::Proc, _) = &f.target else { return Ok(f) };
        let c = f.virtual_path().unwrap_or_default();
        let (_, magic) = crate::procfs::links_in(&c, follow);
        let Some(link) = magic else { return Ok(f) };
        let ns = f.mnt.sb.proc_pid_ns();
        let Some(target) = crate::procfs::magic_target(t, &ns, &link) else { return Ok(f) };
        let rest = &c[link.len()..];
        abs = if rest.is_empty() { target } else { format!("{}{}", target.trim_end_matches('/'), rest) };
    }
    Err(ELOOP)
}

fn may_mount(t: &Thread, ns: &MntNs) -> Result<(), i64> {
    if userns::ns_capable(&t.process.creds.lock(), &ns.user_ns, CAP_SYS_ADMIN) {
        Ok(())
    } else {
        Err(EPERM)
    }
}

fn check_mnt(m: &Mount, ns: &MntNs) -> Result<(), i64> {
    if m.ns.load(Ordering::Acquire) == ns.ino {
        Ok(())
    } else {
        Err(EINVAL)
    }
}

fn read_str(t: &Thread, ptr: u64, max: usize) -> Result<Option<String>, i64> {
    if ptr == 0 {
        return Ok(None);
    }
    let raw = t.mm().read_cstr(ptr as usize, max).map_err(crate::linux::fault)?;
    Ok(Some(String::from_utf8_lossy(&raw).into_owned()))
}

/// Drapeaux de montage (`MS_*`) → drapeaux d'un montage (`MNT_*`), heure d'accès comprise
/// (`relatime` par défaut).
fn mnt_flags_of(ms: u64) -> u32 {
    let mut f = 0;
    if ms & MS_RDONLY != 0 {
        f |= MNT_READONLY;
    }
    if ms & MS_NOSUID != 0 {
        f |= MNT_NOSUID;
    }
    if ms & MS_NODEV != 0 {
        f |= MNT_NODEV;
    }
    if ms & MS_NOEXEC != 0 {
        f |= MNT_NOEXEC;
    }
    if ms & MS_NOSYMFOLLOW != 0 {
        f |= MNT_NOSYMFOLLOW;
    }
    if ms & MS_NOATIME != 0 {
        f |= MNT_NOATIME;
    } else if ms & MS_STRICTATIME == 0 {
        f |= MNT_RELATIME;
    }
    if ms & MS_NODIRATIME != 0 {
        f |= MNT_NODIRATIME;
    }
    f
}

// --- mount(2) -----------------------------------------------------------------------------------------

pub fn sys_mount(t: &Thread, source: u64, target: u64, fstype: u64, flags: u64, data: u64) -> Result<i64, i64> {
    let mut flags = flags;
    if flags & MS_MGC_MSK == MS_MGC_VAL {
        flags &= !MS_MGC_MSK;
    }
    let target = crate::linux::user_path(t, target)?;
    let source = read_str(t, source, 4096)?;
    let fstype = read_str(t, fstype, 256)?;
    let data = read_str(t, data, 4096)?.unwrap_or_default();
    let abs = crate::linux::fs::absolute(t, crate::linux::fs::AT_FDCWD, &target)?;
    let tgt = lookup_magic(t, &abs, true)?;
    let ns = ns_of(t);
    may_mount(t, &ns)?;
    if flags & MS_REMOUNT != 0 && flags & MS_BIND != 0 {
        reconfigure(&ns, &tgt, flags)
    } else if flags & MS_REMOUNT != 0 {
        remount(t, &ns, &tgt, flags, &data)
    } else if flags & MS_BIND != 0 {
        let src = source.ok_or(EFAULT)?;
        let sabs = crate::linux::fs::absolute(t, crate::linux::fs::AT_FDCWD, &src)?;
        let s = lookup_magic(t, &sabs, true)?;
        loopback(t, &ns, &tgt, &s, flags & MS_REC != 0)
    } else if flags & MS_PROPAGATION != 0 {
        change_type(&ns, &tgt, flags)
    } else if flags & MS_MOVE != 0 {
        let src = source.ok_or(EFAULT)?;
        let sabs = crate::linux::fs::absolute(t, crate::linux::fs::AT_FDCWD, &src)?;
        let s = lookup_magic(t, &sabs, true)?;
        move_mount(t, &ns, &s, &tgt)
    } else {
        new_mount(t, &ns, &tgt, fstype.as_deref().ok_or(EINVAL)?, source.as_deref().unwrap_or("none"), flags, &data)
    }
    .map(|_| 0)
}

/// `MS_REMOUNT | MS_BIND` (`do_reconfigure_mnt`) : drapeaux propres au montage seulement.
fn reconfigure(ns: &MntNs, tgt: &Found, flags: u64) -> Result<(), i64> {
    check_mnt(&tgt.mnt, ns)?;
    if !tgt.is_mount_root() {
        return Err(EINVAL);
    }
    let new = mnt_flags_of(flags);
    change_mnt_flags(&tgt.mnt, new)
}

/// Applique les drapeaux réglables `new` en respectant les verrous (`can_change_locked_flags`).
fn change_mnt_flags(m: &Mount, new: u32) -> Result<(), i64> {
    let old = m.flags();
    let locked = |lock: u32, bit: u32| old & lock != 0 && (old & bit) != (new & bit);
    if locked(MNT_LOCK_READONLY, MNT_READONLY) && new & MNT_READONLY == 0
        || locked(MNT_LOCK_NODEV, MNT_NODEV) && new & MNT_NODEV == 0
        || locked(MNT_LOCK_NOSUID, MNT_NOSUID) && new & MNT_NOSUID == 0
        || locked(MNT_LOCK_NOEXEC, MNT_NOEXEC) && new & MNT_NOEXEC == 0
        || (old & MNT_LOCK_ATIME != 0 && (old & MNT_ATIME_MASK) != (new & MNT_ATIME_MASK))
    {
        return Err(EPERM);
    }
    m.flags.store((old & !MNT_USER_SETTABLE) | (new & MNT_USER_SETTABLE), Ordering::Release);
    Ok(())
}

/// `MS_REMOUNT` (`do_remount`) : drapeaux du montage et lecture seule du système de fichiers.
fn remount(t: &Thread, ns: &MntNs, tgt: &Found, flags: u64, data: &str) -> Result<(), i64> {
    check_mnt(&tgt.mnt, ns)?;
    if !tgt.is_mount_root() {
        return Err(EINVAL);
    }
    let m = &tgt.mnt;
    // Un système de fichiers en lecture seule par nature (ext4 de MaiSS) ne passe pas en écriture.
    let fs_ro = match &m.root {
        MountRoot::Node(n) => n.read_only(),
        MountRoot::Virtual(_) => false,
    };
    if flags & MS_RDONLY == 0 && fs_ro {
        return Err(EROFS);
    }
    // devpts relit ses options (`devpts_reconfigure`) avant tout changement.
    if m.sb.fstype == "devpts" {
        let creds = t.process.creds.lock().clone();
        let fs = m.sb.devpts();
        fs.remount(data, &creds)?;
        *m.sb.opts.lock() = fs.opts_text();
    }
    change_mnt_flags(m, mnt_flags_of(flags))?;
    m.sb.ro.store(flags & MS_RDONLY != 0, Ordering::Release);
    if !data.is_empty() && m.sb.fstype == "tmpfs" {
        *m.sb.opts.lock() = tmpfs_opts_text(data);
    }
    Ok(())
}

/// Le montage `id` cache-t-il des montages verrouillés sous `inner` (`__has_locked_children`) ?
fn has_locked_children(tree: &Tree, id: u32, inner: &str) -> bool {
    let Some(n) = tree.nodes.get(&id) else { return false };
    n.children.iter().any(|(mp, v)| strip_under(mp, inner).is_some() && v.iter().any(|c| tree.nodes.get(c).is_some_and(|x| x.mnt.flags() & MNT_LOCKED != 0)))
}

/// Copie du montage `src` montrant `root` (chemin `inner` dans `src`), et si `rec` de tous ceux
/// accrochés dessous (`copy_tree`). Rend l'identifiant de la copie de tête ; les copies
/// appartiennent à l'espace `ns_ino` et ne sont pas encore accrochées (la tête).
fn copy_tree(tree: &mut Tree, src: u32, root: MountRoot, inner: &str, rec: bool, ns_ino: u64, copy_unbindable: bool, map: &mut BTreeMap<u32, Arc<Mount>>) -> Result<u32, i64> {
    let n = tree.nodes.get(&src).ok_or(EINVAL)?;
    let m = n.mnt.clone();
    let (shared, master) = (n.shared, n.master);
    let unbind = n.unbindable;
    let head = Mount::new(m.sb.clone(), root, join(&m.root_path, inner), m.flags() & !MNT_LOCKED, ns_ino);
    tree.insert(head.clone());
    if let Some(h) = tree.nodes.get_mut(&head.id) {
        h.shared = shared;
        h.master = master;
        h.unbindable = unbind && copy_unbindable;
    }
    map.insert(src, head.clone());
    if !rec {
        return Ok(head.id);
    }
    let kids: Vec<(String, u32)> = tree.nodes.get(&src).map(|n| n.children.iter().flat_map(|(mp, v)| v.iter().map(move |c| (mp.clone(), *c))).collect()).unwrap_or_default();
    for (mp, c) in kids {
        let Some(rel) = strip_under(&mp, inner) else { continue };
        let Some(cn) = tree.nodes.get(&c) else { continue };
        if cn.unbindable && !copy_unbindable {
            continue;
        }
        let croot = cn.mnt.root.clone();
        let locked = cn.mnt.flags() & MNT_LOCKED;
        let copy = copy_tree(tree, c, croot, "/", true, ns_ino, copy_unbindable, map)?;
        let cm = tree.nodes.get(&copy).map(|x| x.mnt.clone()).ok_or(EINVAL)?;
        cm.flags.fetch_or(locked, Ordering::AcqRel);
        tree.attach(&cm, head.id, &rel);
    }
    Ok(head.id)
}

fn root_of(f: &Found) -> Result<MountRoot, i64> {
    Ok(match &f.target {
        Target::Node(n) => MountRoot::Node(n.clone()),
        Target::Virtual(v, _) => MountRoot::Virtual(*v),
    })
}

/// Dossier ou non (`d_is_dir`) : pour un nœud produit, son vrai type (`/dev/pts/0`,
/// `/proc/self/status` sont des fichiers ; bubblewrap lie le terminal de l'hôte sur
/// `/dev/console`).
fn is_dir(t: &Thread, f: &Found) -> bool {
    match &f.target {
        Target::Node(n) => !matches!(n, FileOrDir::File(_)),
        Target::Virtual(..) => crate::linux::fs::virtual_is_dir(t, f),
    }
}

/// `MS_BIND` (`do_loopback`).
fn loopback(t: &Thread, ns: &MntNs, tgt: &Found, src: &Found, rec: bool) -> Result<(), i64> {
    check_mnt(&tgt.mnt, ns)?;
    if is_dir(t, tgt) != is_dir(t, src) {
        return Err(ENOTDIR);
    }
    let mut tree = TREE.write();
    let sn = tree.nodes.get(&src.mnt.id).ok_or(EINVAL)?;
    if sn.unbindable {
        return Err(EINVAL);
    }
    if src.mnt.ns.load(Ordering::Acquire) != ns.ino {
        return Err(EINVAL);
    }
    if !rec && has_locked_children(&tree, src.mnt.id, &src.inner) {
        return Err(EINVAL);
    }
    // Le montage virtuel garde le sous-chemin dans sa racine (`root_path`).
    let root = root_of(src)?;
    let mut map = BTreeMap::new();
    let head = copy_tree(&mut tree, src.mnt.id, root, &src.inner, rec, ns.ino, false, &mut map)?;
    let hm = tree.nodes.get(&head).map(|n| n.mnt.clone()).ok_or(EINVAL)?;
    // Une copie de montage partagé rejoint son groupe de pairs (`CL_MAKE_SHARED` si la cible
    // est partagée).
    let tshared = tree.nodes.get(&tgt.mnt.id).map(|n| n.shared).unwrap_or(0);
    if tshared != 0 {
        for m in map.values() {
            if let Some(x) = tree.nodes.get_mut(&m.id) {
                if x.shared == 0 {
                    x.shared = NEXT_GROUP.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
    tree.attach(&hm, tgt.mnt.id, &tgt.inner);
    Ok(())
}

/// `MS_SHARED` / `MS_PRIVATE` / `MS_SLAVE` / `MS_UNBINDABLE` (`do_change_type`).
fn change_type(ns: &MntNs, tgt: &Found, flags: u64) -> Result<(), i64> {
    check_mnt(&tgt.mnt, ns)?;
    if !tgt.is_mount_root() {
        return Err(EINVAL);
    }
    let ty = flags & MS_PROPAGATION;
    if ty.count_ones() != 1 {
        return Err(EINVAL);
    }
    let mut tree = TREE.write();
    let ids = if flags & MS_REC != 0 { tree.subtree(tgt.mnt.id) } else { alloc::vec![tgt.mnt.id] };
    for id in ids {
        let Some(n) = tree.nodes.get_mut(&id) else { continue };
        match ty {
            MS_SHARED => {
                if n.shared == 0 {
                    n.shared = NEXT_GROUP.fetch_add(1, Ordering::Relaxed);
                }
                n.unbindable = false;
            }
            MS_SLAVE => {
                // Un montage partagé devient esclave de son groupe ; sans maître, privé.
                if n.shared != 0 {
                    n.master = n.shared;
                    n.shared = 0;
                }
                n.unbindable = false;
            }
            MS_PRIVATE => {
                n.shared = 0;
                n.master = 0;
                n.unbindable = false;
            }
            _ => {
                n.shared = 0;
                n.master = 0;
                n.unbindable = true;
            }
        }
    }
    Ok(())
}

/// `MS_MOVE` (`do_move_mount`).
fn move_mount(t: &Thread, ns: &MntNs, src: &Found, tgt: &Found) -> Result<(), i64> {
    check_mnt(&src.mnt, ns)?;
    check_mnt(&tgt.mnt, ns)?;
    if !src.is_mount_root() || src.mnt.flags() & MNT_LOCKED != 0 {
        return Err(EINVAL);
    }
    if src.mnt.id == ns.root().id {
        return Err(EINVAL);
    }
    if is_dir(t, tgt) != is_dir(t, src) {
        return Err(ENOTDIR);
    }
    let mut tree = TREE.write();
    // Pas de déplacement sous soi-même.
    if tree.subtree(src.mnt.id).contains(&tgt.mnt.id) {
        return Err(ELOOP);
    }
    // Un montage dont le parent est partagé ne se déplace pas (propagation non prise en charge).
    let parent_shared = tree.nodes.get(&src.mnt.id).and_then(|n| n.parent).and_then(|p| tree.nodes.get(&p)).is_some_and(|p| p.shared != 0);
    if parent_shared {
        return Err(EINVAL);
    }
    tree.detach(src.mnt.id);
    tree.attach(&src.mnt, tgt.mnt.id, &tgt.inner);
    Ok(())
}

fn tmpfs_opts_text(data: &str) -> String {
    data.split(',').filter(|o| o.starts_with("size=") || o.starts_with("mode=") || o.starts_with("uid=") || o.starts_with("gid=") || o.starts_with("nr_inodes=")).collect::<Vec<_>>().join(",")
}

fn parse_size(v: &str) -> Option<usize> {
    let total = frame_allocator::stats().total_frames * 4096;
    let (num, mul) = match v.as_bytes().last()? {
        b'k' | b'K' => (&v[..v.len() - 1], 1024),
        b'm' | b'M' => (&v[..v.len() - 1], 1 << 20),
        b'g' | b'G' => (&v[..v.len() - 1], 1 << 30),
        b'%' => return v[..v.len() - 1].parse::<usize>().ok().map(|p| total / 100 * p),
        _ => (v, 1),
    };
    num.parse::<usize>().ok().map(|n| n * mul)
}

/// Nouveau montage d'un système de fichiers (`do_new_mount`).
fn new_mount(t: &Thread, ns: &MntNs, tgt: &Found, fstype: &str, source: &str, flags: u64, data: &str) -> Result<(), i64> {
    check_mnt(&tgt.mnt, ns)?;
    if !is_dir(t, tgt) {
        return Err(ENOTDIR);
    }
    let creds = t.process.creds.lock().clone();
    let userns_mount = !creds.user_ns.is_init();
    let init = userns::init_user_ns();
    let (sb, root, opts_ok) = match fstype {
        "tmpfs" | "ramfs" => {
            let mut perm = 0o1777u16;
            let (mut uid, mut gid) = (creds.fsuid, creds.fsgid);
            let mut limit = None;
            for o in data.split(',').filter(|o| !o.is_empty()) {
                let (k, v) = o.split_once('=').unwrap_or((o, ""));
                match k {
                    "mode" => perm = u16::from_str_radix(v, 8).map_err(|_| EINVAL)? & 0o7777,
                    "uid" => uid = creds.user_ns.make_kuid(v.parse().map_err(|_| EINVAL)?).ok_or(EINVAL)?,
                    "gid" => gid = creds.user_ns.make_kgid(v.parse().map_err(|_| EINVAL)?).ok_or(EINVAL)?,
                    "size" => limit = Some(parse_size(v).ok_or(EINVAL)?),
                    "nr_inodes" | "nr_blocks" | "huge" | "mpol" | "inode32" | "inode64" | "noswap" | "quota" | "usrquota" | "grpquota" => {}
                    _ => return Err(EINVAL),
                }
            }
            let dir = tmpfs::new_instance(fstype, perm, limit);
            let _ = FileOrDir::Dir(dir.clone()).set_metadata(MetaChange::Owner { uid: Some(uid), gid: Some(gid) });
            let sb = Super::new(fstype, source, &tmpfs_opts_text(data), None, userns_mount);
            (sb, MountRoot::Node(FileOrDir::Dir(dir)), true)
        }
        "proc" => {
            // Le propriétaire de l'espace pid actif doit l'accorder ; le nouveau `proc` montre
            // cet espace (`proc_fill_super`).
            let pid_ns = t.process.pid_ns.clone();
            if !userns::ns_capable(&creds, &pid_ns.user_ns, CAP_SYS_ADMIN) {
                return Err(EPERM);
            }
            (Super::new_proc(source, userns_mount, pid_ns), MountRoot::Virtual(VirtFs::Proc), true)
        }
        "sysfs" => {
            if !userns::ns_capable(&creds, &crate::ns::of(&t.process).net_user_ns(), CAP_SYS_ADMIN) {
                return Err(EPERM);
            }
            (Super::new("sysfs", source, "", Some(VirtFs::Sys), userns_mount), MountRoot::Virtual(VirtFs::Sys), true)
        }
        // Chaque montage est une nouvelle instance (Linux ≥ 4.7) ; permis dans un espace
        // utilisateur (`FS_USERNS_MOUNT`).
        "devpts" => (Super::new_devpts(source, userns_mount, crate::pty::PtsFs::new(data, &creds)?), MountRoot::Virtual(VirtFs::Devpts), true),
        _ => {
            // Systèmes sur périphérique : root de l'espace initial seulement, et aucun pilote
            // de bloc montable par `mount` ici.
            if !userns::ns_capable(&creds, &init, CAP_SYS_ADMIN) {
                return Err(EPERM);
            }
            return Err(ENODEV);
        }
    };
    let _ = opts_ok;
    let mut mflags = mnt_flags_of(flags);
    if flags & MS_RDONLY != 0 {
        sb.ro.store(true, Ordering::Release);
    }
    // Montage depuis un espace utilisateur non initial : `nodev` implicite (`SB_I_NODEV`).
    if userns_mount {
        mflags |= 0;
    }
    let m = Mount::new(sb, root, String::from("/"), mflags, ns.ino);
    let mut tree = TREE.write();
    tree.insert(m.clone());
    tree.attach(&m, tgt.mnt.id, &tgt.inner);
    Ok(())
}

// --- umount2(2) ----------------------------------------------------------------------------------------

pub fn sys_umount2(t: &Thread, target: u64, flags: u64) -> Result<i64, i64> {
    if flags & !(MNT_FORCE | MNT_DETACH | MNT_EXPIRE | UMOUNT_NOFOLLOW) != 0 {
        return Err(EINVAL);
    }
    if flags & MNT_EXPIRE != 0 && flags & (MNT_FORCE | MNT_DETACH) != 0 {
        return Err(EINVAL);
    }
    let path = crate::linux::user_path(t, target)?;
    let abs = crate::linux::fs::absolute(t, crate::linux::fs::AT_FDCWD, &path)?;
    let f = follow_top(lookup_magic(t, &abs, flags & UMOUNT_NOFOLLOW == 0)?);
    let ns = ns_of(t);
    may_mount(t, &ns)?;
    if !f.is_mount_root() {
        return Err(EINVAL);
    }
    check_mnt(&f.mnt, &ns)?;
    let m = f.mnt.clone();
    drop(f);
    if m.flags() & MNT_LOCKED != 0 {
        return Err(EINVAL);
    }
    if flags & MNT_FORCE != 0 && !userns::capable(&t.process.creds.lock(), CAP_SYS_ADMIN) {
        return Err(EPERM);
    }
    if flags & MNT_EXPIRE != 0 {
        return Err(EAGAIN);
    }
    let mut tree = TREE.write();
    let is_root = m.id == ns.root().id;
    if flags & MNT_DETACH == 0 {
        // Occupé : montages accrochés, ou position tenue (racine, dossier courant) ailleurs que
        // par l'arbre et ici.
        let has_children = m.nchild.load(Ordering::Acquire) != 0;
        if is_root || has_children || Arc::strong_count(&m) > 2 {
            return Err(EBUSY);
        }
    } else if is_root {
        return Err(EINVAL);
    }
    let ids = tree.subtree(m.id);
    tree.detach(m.id);
    for id in ids {
        if let Some(n) = tree.nodes.remove(&id) {
            n.mnt.ns.store(0, Ordering::Release);
            // Les montages enfants restent liés entre eux par leurs positions, plus par l'arbre.
        }
    }
    Ok(0)
}

// --- pivot_root(2), chroot(2) -----------------------------------------------------------------------

/// Remplace dans tous les `fs_struct` la racine et le dossier courant égaux à `old` par `new`
/// (`chroot_fs_refs`).
fn chroot_fs_refs(old: &Loc, new: &Loc) {
    for p in crate::all_processes() {
        p.fs.with(|fs| {
            if fs.root.same(old) {
                fs.root = new.clone();
            }
            if fs.cwd.same(old) {
                fs.cwd = new.clone();
            }
        });
    }
}

pub fn sys_pivot_root(t: &Thread, new_root: u64, put_old: u64) -> Result<i64, i64> {
    let ns = ns_of(t);
    may_mount(t, &ns)?;
    let new_abs = crate::linux::fs::absolute(t, crate::linux::fs::AT_FDCWD, &crate::linux::user_path(t, new_root)?)?;
    let old_abs = crate::linux::fs::absolute(t, crate::linux::fs::AT_FDCWD, &crate::linux::user_path(t, put_old)?)?;
    let new = lookup_magic(t, &new_abs, true)?;
    let old = lookup_magic(t, &old_abs, true)?;
    if !is_dir(t, &new) || !is_dir(t, &old) {
        return Err(ENOTDIR);
    }
    let root = fs_of(t).root;
    check_mnt(&new.mnt, &ns)?;
    check_mnt(&old.mnt, &ns)?;
    check_mnt(&root.mnt, &ns)?;
    if new.mnt.flags() & MNT_LOCKED != 0 && false {
        return Err(EINVAL);
    }
    // `new_root` est la racine d'un montage autre que celui de la racine actuelle, qui doit
    // elle-même être une racine de montage.
    if new.mnt.id == root.mnt.id || old.mnt.id == root.mnt.id && old.inner == root.inner {
        return Err(EBUSY);
    }
    if !new.is_mount_root() || root.inner != "/" {
        return Err(EINVAL);
    }
    let mut tree = TREE.write();
    // `new_root` sous la racine, `put_old` sous `new_root`.
    if tree.path_of(new.mnt.id, &new.inner, &root).is_none() {
        return Err(EINVAL);
    }
    let new_loc = Loc::of_mount(&new.mnt);
    if tree.path_of(old.mnt.id, &old.inner, &new_loc).is_none() {
        return Err(EINVAL);
    }
    // Ni le parent de `new_root` ni celui de la racine ne doivent être partagés.
    let parent_shared = |id: u32| tree.nodes.get(&id).and_then(|n| n.parent).and_then(|p| tree.nodes.get(&p)).is_some_and(|p| p.shared != 0);
    if parent_shared(new.mnt.id) || parent_shared(root.mnt.id) {
        return Err(EINVAL);
    }
    let (root_parent, root_mp) = match tree.nodes.get(&root.mnt.id) {
        Some(n) => (n.parent, n.mountpoint.clone()),
        None => return Err(EINVAL),
    };
    // Le verrou passe de l'ancienne racine à la nouvelle.
    if root.mnt.flags() & MNT_LOCKED != 0 {
        new.mnt.flags.fetch_or(MNT_LOCKED, Ordering::AcqRel);
        root.mnt.flags.fetch_and(!MNT_LOCKED, Ordering::AcqRel);
    }
    tree.detach(new.mnt.id);
    tree.detach(root.mnt.id);
    match root_parent {
        Some(rp) => tree.attach(&new.mnt, rp, &root_mp),
        None => *ns.root.lock() = new.mnt.clone(),
    }
    // `put_old` : sa position a été calculée avant le détachement (même montage, même chemin).
    tree.attach(&root.mnt, old.mnt.id, &old.inner);
    drop(tree);
    chroot_fs_refs(&root, &new_loc);
    Ok(0)
}

pub fn sys_chroot(t: &Thread, path: u64) -> Result<i64, i64> {
    let abs = crate::linux::fs::absolute(t, crate::linux::fs::AT_FDCWD, &crate::linux::user_path(t, path)?)?;
    let f = lookup_magic(t, &abs, true)?;
    if !is_dir(t, &f) {
        return Err(ENOTDIR);
    }
    if let Target::Node(n) = &f.target {
        crate::linux::fs::check_access(t, n, crate::creds::MAY_EXEC)?;
    }
    if !userns::current_ns_capable(t, CAP_SYS_CHROOT) {
        return Err(EPERM);
    }
    let loc = f.loc().ok_or(ENOTDIR)?;
    t.process.fs.with(|fs| fs.root = loc);
    Ok(0)
}

// --- Espaces : copie (unshare, clone) ---------------------------------------------------------------

/// Copie l'espace `old` pour `CLONE_NEWNS` (`copy_mnt_ns`) ; `fs` (celui du processus qui
/// change d'espace) est reporté sur les copies. Depuis un espace utilisateur moins privilégié,
/// les copies sont verrouillées et les montages partagés deviennent esclaves.
pub fn copy_ns(old: &Arc<MntNs>, user_ns: &Arc<UserNs>, fs: &mut FsStruct) -> Result<Arc<MntNs>, i64> {
    let ino = userns::alloc_ns_ino();
    let mut tree = TREE.write();
    let root = old.root();
    let mut map = BTreeMap::new();
    let head = copy_tree(&mut tree, root.id, root.root.clone(), "/", true, ino, true, &mut map)?;
    let head_m = tree.nodes.get(&head).map(|n| n.mnt.clone()).ok_or(EINVAL)?;
    let less_privileged = !Arc::ptr_eq(user_ns, &old.user_ns);
    for (old_id, m) in map.iter() {
        // Les copies venues d'un montage hors de l'espace (détaché) ne sont pas reprises.
        let _ = old_id;
        if less_privileged {
            let mut f = m.flags() | MNT_LOCK_ATIME;
            for (bit, lock) in [(MNT_READONLY, MNT_LOCK_READONLY), (MNT_NODEV, MNT_LOCK_NODEV), (MNT_NOSUID, MNT_LOCK_NOSUID), (MNT_NOEXEC, MNT_LOCK_NOEXEC)] {
                if f & bit != 0 {
                    f |= lock;
                }
            }
            f |= MNT_LOCKED;
            m.flags.store(f, Ordering::Release);
            if let Some(n) = tree.nodes.get_mut(&m.id) {
                if n.shared != 0 {
                    n.master = n.shared;
                    n.shared = 0;
                }
            }
        }
    }
    drop(tree);
    let ns = Arc::new(MntNs { ino, user_ns: user_ns.clone(), root: spin::Mutex::new(head_m) });
    // Racine et dossier courant : mêmes chemins dans les copies.
    for loc in [&mut fs.root, &mut fs.cwd] {
        if let Some(m) = map.get(&loc.mnt.id) {
            loc.mnt = m.clone();
        }
    }
    Ok(ns)
}

/// `setns(CLONE_NEWNS)` : racine et dossier courant à la racine de l'espace rejoint.
pub fn enter_ns(ns: &Arc<MntNs>, fs: &mut FsStruct) {
    let root = Loc::of_mount(&ns.root());
    fs.root = root.clone();
    fs.cwd = root;
}

// --- /proc/<pid>/mountinfo, mounts ------------------------------------------------------------------

fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            ' ' => o.push_str("\\040"),
            '\t' => o.push_str("\\011"),
            '\n' => o.push_str("\\012"),
            '\\' => o.push_str("\\134"),
            c => o.push(c),
        }
    }
    o
}

fn sb_text(sb: &Super) -> String {
    let mut s = String::from(if sb.ro.load(Ordering::Acquire) { "ro" } else { "rw" });
    let o = sb.opts.lock();
    if !o.is_empty() {
        s.push(',');
        s.push_str(&o);
    }
    s
}

/// `/proc/<pid>/mountinfo` du processus `p` (son espace, vu de sa racine).
pub fn mountinfo(p: &crate::Process) -> Vec<u8> {
    let ns = crate::ns::of(p).mnt.clone();
    let root = p.fs.root();
    let ids = ns.mounts();
    let tree = TREE.read();
    let mut out = String::new();
    for id in ids {
        let Some(n) = tree.nodes.get(&id) else { continue };
        let Some(mp) = tree.path_of(id, "/", &root) else { continue };
        let parent = n.parent.unwrap_or(id);
        let mut opt = String::new();
        if n.shared != 0 {
            opt.push_str(&format!(" shared:{}", n.shared));
        }
        if n.master != 0 {
            opt.push_str(&format!(" master:{}", n.master));
        }
        if n.unbindable {
            opt.push_str(" unbindable");
        }
        let m = &n.mnt;
        out.push_str(&format!("{} {} 0:{} {} {} {}{} - {} {} {}\n", id, parent, m.sb.minor, escape(&m.root_path), escape(&mp), flags_text(m.flags()), opt, m.sb.fstype, escape(&m.sb.source), sb_text(&m.sb)));
    }
    out.into_bytes()
}

/// `/proc/<pid>/mounts` (et `/proc/mounts`, `/etc/mtab`) : format de `fstab`.
pub fn mounts(p: &crate::Process) -> Vec<u8> {
    let ns = crate::ns::of(p).mnt.clone();
    let root = p.fs.root();
    let ids = ns.mounts();
    let tree = TREE.read();
    let mut out = String::new();
    for id in ids {
        let Some(n) = tree.nodes.get(&id) else { continue };
        let Some(mp) = tree.path_of(id, "/", &root) else { continue };
        let m = &n.mnt;
        let mut opts = flags_text(m.flags() | if m.sb.ro.load(Ordering::Acquire) { MNT_READONLY } else { 0 });
        let so = m.sb.opts.lock().clone();
        if !so.is_empty() {
            opts.push(',');
            opts.push_str(&so);
        }
        out.push_str(&format!("{} {} {} {} 0 0\n", escape(&m.sb.source), escape(&mp), m.sb.fstype, opts));
    }
    out.into_bytes()
}

/// Identifiant du montage d'un chemin (`fdinfo` : `mnt_id`, `statx` : `STATX_MNT_ID`).
pub fn mnt_id_of(t: &Thread, abs: &str) -> u32 {
    lookup(t, abs, true).map(|f| f.mnt.id).unwrap_or(0)
}

/// Nom de chemin de `loc` pour un affichage (`/proc/<pid>/root`, `cwd`) vu depuis `viewer`.
pub fn display_path(loc: &Loc, viewer_root: &Loc) -> String {
    view_path(loc, viewer_root).unwrap_or_else(|| loc.inner.to_string())
}
