//! Appels système de fichiers : VFS de MaiOS, terminal, `/dev` et `/proc` produits à la volée.

use super::errno::*;
use super::{fault, user_path, SysResult};
use crate::fd::{Node, OpenFile, O_ACCMODE, O_APPEND, O_NONBLOCK, O_RDWR, O_WRONLY};
use crate::Thread;
use alloc::{format, string::String, sync::Arc, vec, vec::Vec};
use core::sync::atomic::Ordering;
use fs_node::{DirRef, FileOrDir, FileRef, FsNode, MetaChange, Metadata, NodeKind, TimeSet, Timespec};

pub const AT_FDCWD: i64 = -100;
pub const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
pub const AT_REMOVEDIR: u64 = 0x200;
pub const AT_EMPTY_PATH: u64 = 0x1000;

const O_CREAT: u64 = 0x40;
const O_EXCL: u64 = 0x80;
const O_TRUNC: u64 = 0x200;
const O_DIRECTORY: u64 = 0x10000;
const O_NOFOLLOW: u64 = 0x20000;
const O_CLOEXEC: u64 = 0x80000;
const O_PATH: u64 = 0x20_0000;

const S_IFIFO: u32 = 0o010000;
const S_IFCHR: u32 = 0o020000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;
const S_IFBLK: u32 = 0o060000;
const S_IFSOCK: u32 = 0o140000;

/// Morceau des copies noyau ↔ utilisateur.
const CHUNK: usize = 64 * 1024;

// --- Résolution des chemins ------------------------------------------------------------------

/// Normalise lexicalement `path` relatif à `base` (absolu) : `..` remonte d'un composant du
/// chemin écrit. Les liens symboliques (ext4) sont résolus ensuite par [`walk`], dont les `..`
/// *dans les cibles des liens* sont physiques ; seuls les `..` écrits par l'appelant après un
/// lien restent lexicaux.
fn normalize(base: &str, path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let prefix = if path.starts_with('/') { None } else { Some(base) };
    for comp in prefix.into_iter().chain(core::iter::once(path)).flat_map(|s| s.split('/')) {
        match comp {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    let mut out = String::from("/");
    out.push_str(&parts.join("/"));
    out
}

fn dir_path_of(t: &Thread, dirfd: i64) -> Result<String, i64> {
    if dirfd == AT_FDCWD {
        return Ok(t.process.cwd_path());
    }
    let f = get_fd(t, dirfd as u64)?;
    match &f.node {
        Node::Dir { path, .. } | Node::VirtDir { path, .. } => Ok(path.clone()),
        _ => Err(ENOTDIR),
    }
}

/// Chemin absolu normalisé de `path` vu depuis `dirfd`.
pub(crate) fn absolute(t: &Thread, dirfd: i64, path: &str) -> Result<String, i64> {
    if path.is_empty() {
        return Err(ENOENT);
    }
    if path.starts_with('/') {
        return Ok(normalize("/", path));
    }
    Ok(normalize(&dir_path_of(t, dirfd)?, path))
}

/// Résultat d'une résolution complète dans l'espace de montage du processus.
pub(crate) enum Lookup {
    /// Nœud d'un système de fichiers, avec son montage.
    Vfs(crate::mount::Found),
    /// Nœud produit (`/proc`, `/sys`).
    Special(Special),
    /// Chemin canonique `/dev/pts[/<n>]` d'un montage `devpts`, avec son instance.
    Pts(Arc<crate::pty::PtsFs>, String),
}

/// Résout `abs` (chemin de l'appelant) à travers les montages ; les liens magiques de `/proc`
/// (`cwd`, `root`, `exe`) qui sortent de `/proc` sont suivis (8 fois au plus).
pub(crate) fn lookup_any(t: &Thread, abs: &str, follow: bool) -> Result<Lookup, i64> {
    use crate::mount::{Target, VirtFs};
    let mut abs = String::from(abs);
    for _ in 0..8 {
        let f = crate::mount::lookup(t, &abs, follow)?;
        let fs = match &f.target {
            Target::Node(_) => return Ok(Lookup::Vfs(f)),
            Target::Virtual(v, _) => *v,
        };
        let c = f.virtual_path().unwrap_or_default();
        // Espace pid que montre ce `proc` (celui de son superbloc).
        let pid_ns = f.mnt.sb.proc_pid_ns();
        match fs {
            VirtFs::Devpts => return Ok(Lookup::Pts(f.mnt.sb.devpts(), c)),
            VirtFs::Sys => return special_virtual(t, &pid_ns, &c).map(Lookup::Special).ok_or(ENOENT),
            VirtFs::Proc => {
                let c = if follow { crate::procfs::canonical(t, &pid_ns, &c) } else { crate::procfs::canonical_dirs(t, &pid_ns, &c) };
                if c == "/proc" || c.starts_with("/proc/") {
                    return special_virtual(t, &pid_ns, &c).map(Lookup::Special).ok_or(ENOENT);
                }
                // Lien magique vers un chemin du processus : on recommence avec lui.
                abs = c;
            }
        }
    }
    Err(ELOOP)
}

/// Nœud de `abs` : liens symboliques suivis (le dernier composant seulement si `follow`).
pub(crate) fn walk(t: &Thread, abs: &str, follow: bool) -> Result<FileOrDir, i64> {
    walk_mnt(t, abs, follow).map(|(n, _)| n)
}

/// Comme [`walk`], avec le montage du nœud (drapeaux `ro`, `nodev`, `noexec`, `nosuid`).
pub(crate) fn walk_mnt(t: &Thread, abs: &str, follow: bool) -> Result<(FileOrDir, Arc<crate::mount::Mount>), i64> {
    let f = crate::mount::lookup(t, abs, follow)?;
    match f.target {
        crate::mount::Target::Node(n) => Ok((n, f.mnt)),
        crate::mount::Target::Virtual(..) => Err(ENOENT),
    }
}

pub(crate) fn lookup(t: &Thread, abs: &str) -> Option<FileOrDir> {
    walk(t, abs, true).ok()
}

/// Comme [`lookup`] sans suivre un lien final (`lstat`, `readlink`, `unlink`).
#[allow(dead_code)]
pub(crate) fn lookup_nofollow(t: &Thread, abs: &str) -> Option<FileOrDir> {
    walk(t, abs, false).ok()
}

/// Chemin physique de `abs` (liens résolus), pour `/proc/<pid>/exe` et `inotify`.
pub(crate) fn physical(t: &Thread, abs: &str) -> Option<String> {
    crate::mount::lookup(t, abs, true).ok().map(|f| f.path)
}

/// Parent et nom final de `abs`, pour créer, supprimer ou renommer : `EROFS` si le montage
/// du parent est en lecture seule.
pub(crate) fn split_parent(t: &Thread, abs: &str) -> Result<(DirRef, String), i64> {
    let (dir, name) = abs.rsplit_once('/').ok_or(EINVAL)?;
    if name.is_empty() {
        return Err(EEXIST); // la racine
    }
    let dir = if dir.is_empty() { "/" } else { dir };
    let f = crate::mount::lookup(t, dir, true)?;
    match f.target {
        crate::mount::Target::Node(FileOrDir::Dir(d)) => {
            if f.mnt.readonly() {
                return Err(EROFS);
            }
            Ok((d, String::from(name)))
        }
        crate::mount::Target::Node(FileOrDir::File(_)) => Err(ENOTDIR),
        crate::mount::Target::Virtual(..) => Err(EACCES),
    }
}

/// Chemin d'un descripteur (`execveat` avec `AT_EMPTY_PATH`).
pub(crate) fn fd_path(t: &Thread, fd: u64) -> Result<String, i64> {
    match &get_fd(t, fd)?.node {
        Node::File { path, .. } => Ok(path.clone()),
        Node::Dir { .. } => Err(EACCES),
        _ => Err(ENOEXEC),
    }
}

pub(crate) fn get_fd(t: &Thread, fd: u64) -> Result<Arc<OpenFile>, i64> {
    if fd > i32::MAX as u64 {
        return Err(EBADF);
    }
    t.process.files.lock().get(fd as usize).ok_or(EBADF)
}

// --- /dev et /proc ---------------------------------------------------------------------------

pub(crate) enum Special {
    Node(Node),
    #[allow(dead_code)]
    Dup(usize),
    /// Description ouverte d'un processus (`/proc/<pid>/fd/<n>`).
    File(Arc<OpenFile>),
}

fn console_of(t: &Thread) -> Option<Node> {
    let files = t.process.files.lock();
    let input = files.get(0).and_then(|f| match &f.node {
        Node::Console { input, .. } => input.clone(),
        _ => None,
    });
    let (output, discipline) = files
        .get(1)
        .and_then(|f| match &f.node {
            Node::Console { output, discipline, .. } => Some((output.clone(), discipline.clone())),
            _ => None,
        })
        .unwrap_or((None, None));
    if input.is_none() && output.is_none() {
        return None;
    }
    Some(Node::Console { input, output, discipline })
}

/// Nœud produit de chemin canonique `c` (`/proc/…`, `/sys/…`).
fn special_virtual(t: &Thread, pid_ns: &crate::pidns::PidNs, c: &str) -> Option<Special> {
    let gen = |data: Vec<u8>| Some(Special::Node(Node::Generated { data: Arc::from(data.into_boxed_slice()), path: String::from(c) }));
    if let Some(n) = crate::sysstats::proc_node(c) {
        return Some(Special::Node(n));
    }
    if let Some(n) = crate::strace::proc_node(c) {
        return Some(Special::Node(n));
    }
    if crate::procfs::handles(c) {
        return match crate::procfs::lookup(t, pid_ns, c)? {
            crate::procfs::ProcNode::File(d) => gen(d),
            crate::procfs::ProcNode::Dir(e) => Some(Special::Node(Node::VirtDir { path: String::from(c), entries: Arc::new(e) })),
            crate::procfs::ProcNode::Fd(f) => Some(Special::File(f)),
            crate::procfs::ProcNode::Node(n) => Some(Special::Node(n)),
        };
    }
    None
}

/// Le nœud produit `f` (résultat d'une résolution dans `/proc`, `/sys` ou devpts) est-il un
/// dossier ? Un nœud inconnu compte pour un dossier (sous-arbre produit à la lecture).
pub(crate) fn virtual_is_dir(t: &Thread, f: &crate::mount::Found) -> bool {
    use crate::mount::{Target, VirtFs};
    let Target::Virtual(v, sub) = &f.target else { return false };
    match v {
        VirtFs::Devpts => sub == "/",
        VirtFs::Proc | VirtFs::Sys => {
            let pid_ns = f.mnt.sb.proc_pid_ns();
            let c = crate::procfs::canonical(t, &pid_ns, &f.virtual_path().unwrap_or_default());
            matches!(special_virtual(t, &pid_ns, &c), None | Some(Special::Node(Node::VirtDir { .. })))
        }
    }
}

/// Nœud de `devpts` (`/dev/pts`, `/dev/pts/ptmx`, `/dev/pts/<n>`) de l'instance `fs`, sans
/// l'ouvrir.
fn pts_node(t: &Thread, fs: &Arc<crate::pty::PtsFs>, c: &str) -> Option<Special> {
    if c == "/dev/pts" {
        return Some(Special::Node(Node::VirtDir { path: String::from(c), entries: Arc::new(crate::pty::dir_entries(fs)) }));
    }
    match crate::pty::stat_node(fs, c) {
        Some(n) => Some(Special::Node(n)),
        // Sans pseudo-terminal 0, `/dev/pts/0` de l'instance initiale désigne la console du
        // processus.
        None if c == "/dev/pts/0" && fs.is_initial() => console_of(t).map(Special::Node),
        None => None,
    }
}

/// `/etc/mtab` quand la racine n'en a pas : la table des montages (le lien de Debian).
fn mtab(t: &Thread) -> Special {
    Special::Node(Node::Generated { data: Arc::from(crate::mount::mounts(&t.process).into_boxed_slice()), path: String::from("/etc/mtab") })
}

/// Nœud produit désigné par `abs` (chemin de l'appelant), s'il en est un.
pub(crate) fn special(t: &Thread, abs: &str) -> Option<Special> {
    match lookup_any(t, abs, true) {
        Ok(Lookup::Special(s)) => Some(s),
        Ok(Lookup::Pts(fs, c)) => pts_node(t, &fs, &c),
        Ok(Lookup::Vfs(_)) => None,
        Err(_) if abs == "/etc/mtab" => Some(mtab(t)),
        Err(_) => None,
    }
}

/// [`lookup_any`] avec le repli de `/etc/mtab`.
fn lookup_any_mtab(t: &Thread, abs: &str, follow: bool) -> Result<Lookup, i64> {
    match lookup_any(t, abs, follow) {
        Err(ENOENT) if abs == "/etc/mtab" => Ok(Lookup::Special(mtab(t))),
        r => r,
    }
}

/// Chemin absolu de `path` vu depuis `dirfd`, liens magiques de `/proc` sortants suivis
/// (`/proc/self/cwd/x` → chemin du dossier courant).
pub(crate) fn resolved(t: &Thread, dirfd: i64, path: &str) -> Result<String, i64> {
    let abs = absolute(t, dirfd, path)?;
    if let Ok(f) = crate::mount::lookup(t, &abs, true) {
        if let crate::mount::Target::Virtual(crate::mount::VirtFs::Proc, _) = f.target {
            let c = crate::procfs::canonical(t, &f.mnt.sb.proc_pid_ns(), &f.virtual_path().unwrap_or_default());
            if c != "/proc" && !c.starts_with("/proc/") {
                return Ok(c);
            }
        }
    }
    Ok(abs)
}

/// Cible d'un lien de `/proc` désigné par `abs` (chemin de l'appelant, dernier composant non
/// suivi) : `Some(None)` pour un chemin de `proc` qui n'est pas un lien, `None` hors de `proc`.
fn proc_link(t: &Thread, abs: &str) -> Option<Option<String>> {
    let f = crate::mount::lookup(t, abs, false).ok()?;
    let crate::mount::Target::Virtual(crate::mount::VirtFs::Proc, _) = f.target else { return None };
    let c = f.virtual_path().unwrap_or_default();
    Some(crate::procfs::readlink(t, &f.mnt.sb.proc_pid_ns(), &c))
}

/// Identités d'un `stat` vues de l'espace utilisateur de l'appelant.
fn view_ids(t: &Thread, s: &StatInfo) -> StatInfo {
    let ns = t.process.creds.lock().user_ns.clone();
    let mut v = s.clone();
    v.uid = ns.from_kuid_munged(s.uid);
    v.gid = ns.from_kgid_munged(s.gid);
    v
}

/// Description `O_PATH` d'un nœud de devpts (`/dev/pts/ptmx`, `/dev/pts/<n>`, console) : elle
/// désigne le nœud sans l'ouvrir (`O_PATH` n'appelle jamais le pilote : un `ptmx` n'y crée pas
/// de terminal, un esclave n'y devient pas terminal de contrôle), garde le chemin d'ouverture
/// (`/proc/self/fd/N`, que `mount` suit : bubblewrap lie ainsi le terminal de l'hôte sur
/// `/dev/console`) et rend le `stat` du nœud.
pub(crate) struct PathRef {
    path: String,
    st: StatInfo,
}

impl PathRef {
    /// Chemin d'ouverture (visible depuis la racine de l'ouvreur).
    pub(crate) fn path(&self) -> &str {
        &self.path
    }
}

impl crate::object::FileObject for PathRef {
    crate::object::object_any!();

    fn link_name(&self) -> String {
        self.path.clone()
    }

    fn stat(&self) -> (u32, u64) {
        (self.st.mode, self.st.ino)
    }

    fn stat_extra(&self) -> (u64, u32, u32) {
        (self.st.rdev, self.st.uid, self.st.gid)
    }

    fn poll(&self, _t: Option<&Thread>) -> u32 {
        0
    }

    fn queues<'a>(&'a self, _cb: &mut dyn FnMut(&'a crate::wait::WaitQueue)) {}
}

// --- Informations de fichier -------------------------------------------------------------------

#[derive(Clone)]
struct StatInfo {
    ino: u64,
    mode: u32,
    nlink: u64,
    rdev: u64,
    size: u64,
    uid: u32,
    gid: u32,
    dev: u64,
    /// Secteurs de 512 octets occupés ; `None` : déduit de la taille.
    blocks: Option<u64>,
    blksize: u32,
    /// Dates (accès, modification, changement, création) ; `None` : maintenant.
    times: Option<[fs_node::Timespec; 4]>,
}

impl StatInfo {
    const DEFAULT: StatInfo = StatInfo { ino: 0, mode: 0, nlink: 1, rdev: 0, size: 0, uid: 0, gid: 0, dev: 1, blocks: None, blksize: 4096, times: None };
}

/// `st_dev`/`st_rdev` au codage de glibc (`gnu_dev_makedev`).
fn linux_dev(major: u32, minor: u32) -> u64 {
    let (ma, mi) = (major as u64, minor as u64);
    ((ma & 0xfff) << 8) | ((ma & !0xfff) << 32) | (mi & 0xff) | ((mi & !0xff) << 12)
}

/// Informations tenues par le système de fichiers (ext4).
fn stat_meta(m: &Metadata) -> StatInfo {
    let ty = match m.kind {
        NodeKind::File => S_IFREG,
        NodeKind::Dir => S_IFDIR,
        NodeKind::Symlink => S_IFLNK,
        NodeKind::CharDev => S_IFCHR,
        NodeKind::BlockDev => S_IFBLK,
        NodeKind::Fifo => S_IFIFO,
        NodeKind::Socket => S_IFSOCK,
    };
    StatInfo {
        ino: m.ino,
        mode: ty | m.perm as u32,
        nlink: m.nlink as u64,
        rdev: if matches!(m.kind, NodeKind::CharDev | NodeKind::BlockDev) { linux_dev(m.rdev.0, m.rdev.1) } else { 0 },
        size: m.size,
        uid: m.uid,
        gid: m.gid,
        dev: m.dev,
        blocks: Some(m.blocks),
        blksize: m.blksize,
        times: Some([m.atime, m.mtime, m.ctime, m.btime.unwrap_or(m.ctime)]),
    }
}

fn ino_of(path: &str) -> u64 {
    // FNV-1a : stable pour un même chemin, replié sur 32 bits comme les inodes des
    // pseudo-systèmes de Linux (`get_next_ino`) : un programme i386 sans LFS (`stat`,
    // `readdir` de la glibc, ancien `getdents`) rend `EOVERFLOW` au-delà.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    ((h ^ (h >> 32)) as u32 | 1) as u64
}

const fn makedev(major: u64, minor: u64) -> u64 { (major << 8) | minor }

fn stat_node(node: &Node) -> StatInfo {
    match node {
        Node::Console { .. } => StatInfo { ino: 3, mode: S_IFCHR | 0o620, nlink: 1, rdev: makedev(136, 0), size: 0, ..StatInfo::DEFAULT },
        Node::Null => StatInfo { ino: 4, mode: S_IFCHR | 0o666, nlink: 1, rdev: makedev(1, 3), size: 0, ..StatInfo::DEFAULT },
        Node::Zero => StatInfo { ino: 5, mode: S_IFCHR | 0o666, nlink: 1, rdev: makedev(1, 5), size: 0, ..StatInfo::DEFAULT },
        Node::Random => StatInfo { ino: 6, mode: S_IFCHR | 0o666, nlink: 1, rdev: makedev(1, 9), size: 0, ..StatInfo::DEFAULT },
        Node::Generated { data, path } => StatInfo { ino: ino_of(path), mode: S_IFREG | 0o444, nlink: 1, rdev: 0, size: data.len() as u64, ..StatInfo::DEFAULT },
        Node::File { file, path } => {
            let f = file.lock();
            match f.metadata() {
                Some(m) => stat_meta(&m),
                None => StatInfo { ino: ino_of(path), mode: S_IFREG | 0o755, nlink: 1, rdev: 0, size: f.len() as u64, ..StatInfo::DEFAULT },
            }
        }
        Node::Dir { dir, path } => match dir.lock().metadata() {
            Some(m) => stat_meta(&m),
            None => StatInfo { ino: ino_of(path), mode: S_IFDIR | 0o755, nlink: 2, rdev: 0, size: 4096, ..StatInfo::DEFAULT },
        },
        Node::VirtDir { path, .. } => StatInfo { ino: ino_of(path), mode: S_IFDIR | 0o555, nlink: 2, rdev: 0, size: 0, ..StatInfo::DEFAULT },
        Node::Pipe(end) => match end.file.as_ref().and_then(|f| f.lock().metadata()) {
            // Tube nommé : l'inode du système de fichiers.
            Some(m) => stat_meta(&m),
            None => StatInfo { ino: end.pipe.ino, mode: S_IFIFO | 0o600, nlink: 1, rdev: 0, size: 0, ..StatInfo::DEFAULT },
        },
        Node::Object(o) => {
            let (mode, ino) = o.stat();
            let (rdev, uid, gid) = o.stat_extra();
            StatInfo { ino, mode, nlink: 1, rdev, uid, gid, size: 0, ..StatInfo::DEFAULT }
        }
    }
}

/// Identité `(st_dev, st_ino)` d'un nœud, la même que `stat` (inotify, verrous).
pub(crate) fn inode_key(node: &FileOrDir, abs: &str) -> (u64, u64) {
    let s = stat_fs(node, abs);
    (s.dev, s.ino)
}

fn stat_fs(node: &FileOrDir, abs: &str) -> StatInfo {
    if let Some(m) = node.metadata() {
        return stat_meta(&m);
    }
    match node {
        FileOrDir::File(f) => StatInfo { ino: ino_of(abs), mode: S_IFREG | 0o755, nlink: 1, rdev: 0, size: f.lock().len() as u64, ..StatInfo::DEFAULT },
        FileOrDir::Dir(_) => StatInfo { ino: ino_of(abs), mode: S_IFDIR | 0o755, nlink: 2, rdev: 0, size: 4096, ..StatInfo::DEFAULT },
    }
}

fn write_stat(t: &Thread, buf: u64, s: &StatInfo) -> SysResult {
    let s = &view_ids(t, s);
    let mut st = [0u8; 144];
    let put = |b: &mut [u8; 144], off: usize, v: u64, n: usize| b[off..off + n].copy_from_slice(&v.to_le_bytes()[..n]);
    let now = super::sys::realtime();
    put(&mut st, 0, s.dev, 8);
    put(&mut st, 8, s.ino, 8);
    put(&mut st, 16, s.nlink, 8);
    put(&mut st, 24, s.mode as u64, 4);
    put(&mut st, 28, s.uid as u64, 4);
    put(&mut st, 32, s.gid as u64, 4);
    put(&mut st, 40, s.rdev, 8);
    put(&mut st, 48, s.size, 8);
    put(&mut st, 56, s.blksize as u64, 8);
    put(&mut st, 64, s.blocks.unwrap_or((s.size + 511) / 512), 8);
    // st_atim, st_mtim, st_ctim.
    for (k, off) in [72, 88, 104].into_iter().enumerate() {
        let (sec, nsec) = match &s.times {
            Some(t) => (t[k].sec as u64, t[k].nsec as u64),
            None => (now.0 as u64, now.1 as u64),
        };
        put(&mut st, off, sec, 8);
        put(&mut st, off + 8, nsec, 8);
    }
    t.mm().copy_to_user(buf as usize, &st).map_err(fault)?;
    Ok(0)
}

fn write_statx(t: &Thread, buf: u64, s: &StatInfo) -> SysResult {
    let s = &view_ids(t, s);
    let mut sx = [0u8; 256];
    let put = |b: &mut [u8; 256], off: usize, v: u64, n: usize| b[off..off + n].copy_from_slice(&v.to_le_bytes()[..n]);
    let now = super::sys::realtime();
    // stx_mask : STATX_BASIC_STATS, plus STATX_BTIME quand le système de fichiers la tient.
    put(&mut sx, 0, if s.times.is_some() { 0xfff } else { 0x7ff }, 4);
    put(&mut sx, 4, s.blksize as u64, 4);
    put(&mut sx, 16, s.nlink, 4);
    put(&mut sx, 20, s.uid as u64, 4);
    put(&mut sx, 24, s.gid as u64, 4);
    put(&mut sx, 28, s.mode as u64, 2);
    put(&mut sx, 32, s.ino, 8);
    put(&mut sx, 40, s.size, 8);
    put(&mut sx, 48, s.blocks.unwrap_or((s.size + 511) / 512), 8);
    // stx_atime, stx_btime, stx_ctime, stx_mtime (ordre de `struct statx`).
    for (k, off) in [(0usize, 64usize), (3, 80), (2, 96), (1, 112)] {
        let (sec, nsec) = match &s.times {
            Some(t) => (t[k].sec as u64, t[k].nsec as u64),
            None => (now.0 as u64, now.1 as u64),
        };
        put(&mut sx, off, sec, 8);
        put(&mut sx, off + 8, nsec, 4);
    }
    let dev_major = |d: u64| ((d >> 8) & 0xfff) | ((d >> 32) & !0xfff);
    let dev_minor = |d: u64| (d & 0xff) | ((d >> 12) & !0xff);
    put(&mut sx, 128, dev_major(s.rdev), 4);
    put(&mut sx, 132, dev_minor(s.rdev), 4);
    put(&mut sx, 136, dev_major(s.dev), 4);
    put(&mut sx, 140, dev_minor(s.dev), 4);
    t.mm().copy_to_user(buf as usize, &sx).map_err(fault)?;
    Ok(0)
}

/// Informations sur `path` (ou sur `dirfd` avec `AT_EMPTY_PATH`).
fn stat_at(t: &Thread, dirfd: i64, path_ptr: u64, flags: u64) -> Result<StatInfo, i64> {
    let path = user_path(t, path_ptr)?;
    if path.is_empty() {
        if flags & AT_EMPTY_PATH == 0 {
            return Err(ENOENT);
        }
        if dirfd == AT_FDCWD {
            let p = t.process.cwd_path();
            return Ok(StatInfo { ino: ino_of(&p), mode: S_IFDIR | 0o755, nlink: 2, rdev: 0, size: 4096, ..StatInfo::DEFAULT });
        }
        return Ok(stat_node(&get_fd(t, dirfd as u64)?.node));
    }
    let abs = absolute(t, dirfd, &path)?;
    if flags & AT_SYMLINK_NOFOLLOW != 0 && matches!(proc_link(t, &abs), Some(Some(_))) {
        return Ok(StatInfo { ino: ino_of(&abs), mode: S_IFLNK | 0o777, nlink: 1, rdev: 0, size: 64, ..StatInfo::DEFAULT });
    }
    let follow = flags & AT_SYMLINK_NOFOLLOW == 0;
    let n = match lookup_any_mtab(t, &abs, follow)? {
        Lookup::Special(Special::Node(n)) => return Ok(stat_node(&n)),
        Lookup::Special(Special::Dup(fd)) => return Ok(stat_node(&get_fd(t, fd as u64)?.node)),
        Lookup::Special(Special::File(f)) => return Ok(stat_node(&f.node)),
        Lookup::Pts(fs, c) => {
            return match pts_node(t, &fs, &c) {
                Some(Special::Node(n)) => Ok(stat_node(&n)),
                _ => Err(ENOENT),
            }
        }
        Lookup::Vfs(f) => match f.target {
            crate::mount::Target::Node(n) => n,
            crate::mount::Target::Virtual(..) => return Err(ENOENT),
        },
    };
    let mut st = stat_fs(&n, &abs);
    if crate::unix::is_socket_node(&n) {
        st.mode = crate::object::S_IFSOCK | 0o777;
        st.size = 0;
    }
    Ok(st)
}

pub fn fstat(t: &Thread, fd: u64, buf: u64) -> SysResult {
    let f = get_fd(t, fd)?;
    write_stat(t, buf, &stat_node(&f.node))
}

/// Disposition i386 d'un `stat` : `struct stat64` (96 octets, groupée) ou `struct compat_stat`
/// (64 octets, champs de 16 et 32 bits, `EOVERFLOW` au-delà).
#[derive(Clone, Copy)]
pub(crate) enum Stat32 {
    Stat64,
    Old,
}

fn write_stat32(t: &Thread, buf: u64, s: &StatInfo, kind: Stat32) -> SysResult {
    let s = &view_ids(t, s);
    let now = super::sys::realtime();
    let times: [(u32, u32); 3] = core::array::from_fn(|k| match &s.times {
        Some(t) => (t[k].sec as u32, t[k].nsec as u32),
        None => (now.0 as u32, now.1 as u32),
    });
    let blocks = s.blocks.unwrap_or((s.size + 511) / 512);
    let old_dev = |d: u64| ((d >> 8) & 0xfff) << 8 | (d & 0xff); // `old_encode_dev`
    match kind {
        Stat32::Stat64 => {
            let mut st = [0u8; 96];
            let put = |b: &mut [u8; 96], off: usize, v: u64, n: usize| b[off..off + n].copy_from_slice(&v.to_le_bytes()[..n]);
            put(&mut st, 0, s.dev, 8);
            put(&mut st, 12, s.ino as u32 as u64, 4); // __st_ino
            put(&mut st, 16, s.mode as u64, 4);
            put(&mut st, 20, s.nlink, 4);
            put(&mut st, 24, s.uid as u64, 4);
            put(&mut st, 28, s.gid as u64, 4);
            put(&mut st, 32, s.rdev, 8);
            put(&mut st, 44, s.size, 8);
            put(&mut st, 52, s.blksize as u64, 4);
            put(&mut st, 56, blocks, 8);
            for (k, off) in [64, 72, 80].into_iter().enumerate() {
                put(&mut st, off, times[k].0 as u64, 4);
                put(&mut st, off + 4, times[k].1 as u64, 4);
            }
            put(&mut st, 88, s.ino, 8);
            t.mm().copy_to_user(buf as usize, &st).map_err(fault)?;
        }
        Stat32::Old => {
            if s.ino > u32::MAX as u64 || s.size > i32::MAX as u64 || s.nlink > u16::MAX as u64 {
                return Err(EOVERFLOW);
            }
            if s.uid > u16::MAX as u32 || s.gid > u16::MAX as u32 {
                return Err(EOVERFLOW);
            }
            let mut st = [0u8; 64];
            let put = |b: &mut [u8; 64], off: usize, v: u64, n: usize| b[off..off + n].copy_from_slice(&v.to_le_bytes()[..n]);
            put(&mut st, 0, old_dev(s.dev), 4);
            put(&mut st, 4, s.ino, 4);
            put(&mut st, 8, s.mode as u64, 2);
            put(&mut st, 10, s.nlink, 2);
            put(&mut st, 12, s.uid as u64, 2);
            put(&mut st, 14, s.gid as u64, 2);
            put(&mut st, 16, old_dev(s.rdev), 4);
            put(&mut st, 20, s.size, 4);
            put(&mut st, 24, s.blksize as u64, 4);
            put(&mut st, 28, blocks.min(u32::MAX as u64), 4);
            for (k, off) in [32, 40, 48].into_iter().enumerate() {
                put(&mut st, off, times[k].0 as u64, 4);
                put(&mut st, off + 4, times[k].1 as u64, 4);
            }
            t.mm().copy_to_user(buf as usize, &st).map_err(fault)?;
        }
    }
    Ok(0)
}

/// `fstat64` / `fstat` i386.
pub(crate) fn fstat32(t: &Thread, fd: u64, buf: u64, kind: Stat32) -> SysResult {
    let f = get_fd(t, fd)?;
    write_stat32(t, buf, &stat_node(&f.node), kind)
}

/// `stat64`, `lstat64`, `fstatat64` / `stat`, `lstat` i386.
pub(crate) fn fstatat32(t: &Thread, dirfd: i64, path: u64, buf: u64, flags: u64, kind: Stat32) -> SysResult {
    let s = stat_at(t, dirfd, path, flags)?;
    write_stat32(t, buf, &s, kind)
}

pub fn fstatat(t: &Thread, dirfd: i64, path: u64, buf: u64, flags: u64) -> SysResult {
    let s = stat_at(t, dirfd, path, flags)?;
    write_stat(t, buf, &s)
}

pub fn statx(t: &Thread, dirfd: i64, path: u64, flags: u64, _mask: u64, buf: u64) -> SysResult {
    let s = stat_at(t, dirfd, path, flags)?;
    write_statx(t, buf, &s)
}

/// Disposition de la `struct statfs` écrite : x86_64 (120 octets), `compat_statfs` i386
/// (`statfs`/`fstatfs`, mots de 32 bits, 64 octets) ou `compat_statfs64` i386
/// (`statfs64`/`fstatfs64`, compteurs de 64 bits, groupée, 84 octets).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StatfsLayout {
    Native,
    Compat,
    Compat64,
}

/// `statfs(path, buf)` : le chemin doit exister.
pub fn statfs_path(t: &Thread, path: u64, buf: u64, layout: StatfsLayout) -> SysResult {
    let abs = resolved(t, AT_FDCWD, &user_path(t, path)?)?;
    lookup_any_mtab(t, &abs, true)?;
    statfs(t, buf, layout)
}

/// `fstatfs(fd, buf)` : le descripteur doit être ouvert (`EBADF`).
pub fn fstatfs(t: &Thread, fd: u64, buf: u64, layout: StatfsLayout) -> SysResult {
    get_fd(t, fd)?;
    statfs(t, buf, layout)
}

pub fn statfs(t: &Thread, buf: u64, layout: StatfsLayout) -> SysResult {
    let s = frame_allocator::stats();
    // f_type, f_bsize, f_blocks, f_bfree, f_bavail, f_files, f_ffree, f_fsid (2 × 32 bits),
    // f_namelen, f_frsize, f_flags.
    let f: [u64; 11] = [0x0102_1994, 4096, s.total_frames as u64, s.free_frames as u64, s.free_frames as u64, 1 << 20, 1 << 20, 0, 255, 4096, 0];
    match layout {
        StatfsLayout::Native => {
            let mut b = [0u8; 120];
            for (i, v) in f.iter().enumerate() {
                b[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
            }
            t.mm().copy_to_user(buf as usize, &b).map_err(fault)?;
        }
        StatfsLayout::Compat => {
            // `put_compat_statfs` : un compteur qui ne tient pas en 32 bits → `EOVERFLOW`.
            if f[2..7].iter().any(|&v| v > u32::MAX as u64) {
                return Err(EOVERFLOW);
            }
            let mut b = [0u8; 64];
            for (i, v) in f.iter().enumerate() {
                b[i * 4 + if i > 7 { 4 } else { 0 }..][..4].copy_from_slice(&(*v as u32).to_le_bytes());
            }
            t.mm().copy_to_user(buf as usize, &b).map_err(fault)?;
        }
        StatfsLayout::Compat64 => {
            let mut b = [0u8; 84];
            b[0..4].copy_from_slice(&(f[0] as u32).to_le_bytes());
            b[4..8].copy_from_slice(&(f[1] as u32).to_le_bytes());
            for i in 0..5 {
                b[8 + i * 8..16 + i * 8].copy_from_slice(&f[2 + i].to_le_bytes());
            }
            // f_fsid (48..56) nul, puis f_namelen, f_frsize, f_flags.
            for i in 0..3 {
                b[56 + i * 4..60 + i * 4].copy_from_slice(&(f[8 + i] as u32).to_le_bytes());
            }
            t.mm().copy_to_user(buf as usize, &b).map_err(fault)?;
        }
    }
    Ok(0)
}

// --- Ouverture, fermeture, duplication -------------------------------------------------------

pub fn openat(t: &Thread, dirfd: i64, path_ptr: u64, flags: u64, mode: u64) -> SysResult {
    let path = user_path(t, path_ptr)?;
    let abs = absolute(t, dirfd, &path)?;
    open_abs(t, &abs, &path, flags, mode)
}

/// `openat2(dirfd, path, how, size)` (Linux 5.6) : `struct open_how { flags, mode, resolve }`
/// (`u64` chacun, 24 octets dans sa première version, extensible : octets en plus nuls, sinon
/// `E2BIG`, comme `copy_struct_from_user`). Contrairement à `openat`, tout drapeau inconnu est
/// refusé (`build_open_flags`). bubblewrap ≥ 0.11 ouvre ainsi les sources de ses montages
/// (`RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS`), sans repli.
pub fn openat2(t: &Thread, dirfd: i64, path_ptr: u64, how_ptr: u64, size: u64) -> SysResult {
    const OPEN_HOW_SIZE_VER0: u64 = 24;
    const O_TMPFILE_BIT: u64 = 0x40_0000; // __O_TMPFILE
    // O_ACCMODE et tous les bits de O_CREAT (0o100) à __O_TMPFILE (0o20000000) ; les bits 2 à 5
    // et au-delà de 22 ne sont pas des drapeaux d'ouverture.
    const VALID_OPEN_FLAGS: u64 = 0o37777700 | 3;
    const O_PATH_FLAGS: u64 = O_DIRECTORY | O_NOFOLLOW | O_PATH | O_CLOEXEC;
    const RESOLVE_NO_XDEV: u64 = 0x01;
    const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
    const RESOLVE_NO_SYMLINKS: u64 = 0x04;
    const RESOLVE_BENEATH: u64 = 0x08;
    const RESOLVE_IN_ROOT: u64 = 0x10;
    const RESOLVE_CACHED: u64 = 0x20;

    if size < OPEN_HOW_SIZE_VER0 {
        return Err(EINVAL);
    }
    if size > 4096 {
        return Err(E2BIG);
    }
    let mut how = alloc::vec![0u8; size as usize];
    t.mm().copy_from_user(&mut how, how_ptr as usize).map_err(fault)?;
    if how[OPEN_HOW_SIZE_VER0 as usize..].iter().any(|b| *b != 0) {
        return Err(E2BIG);
    }
    let word = |i: usize| u64::from_le_bytes(how[i * 8..i * 8 + 8].try_into().unwrap());
    let (flags, mode, resolve) = (word(0), word(1), word(2));

    if flags & !VALID_OPEN_FLAGS != 0 {
        return Err(EINVAL);
    }
    if flags & (O_CREAT | O_TMPFILE_BIT) != 0 {
        if mode & !0o7777 != 0 {
            return Err(EINVAL);
        }
    } else if mode != 0 {
        return Err(EINVAL);
    }
    if flags & O_PATH != 0 && flags & !O_PATH_FLAGS != 0 {
        return Err(EINVAL);
    }
    // Linux 6.4 : `O_DIRECTORY | O_CREAT` n'a pas de sens.
    if flags & (O_DIRECTORY | O_CREAT) == O_DIRECTORY | O_CREAT {
        return Err(EINVAL);
    }
    if flags & O_TMPFILE_BIT != 0 && (flags & O_DIRECTORY == 0 || flags as u32 & O_ACCMODE == 0) {
        return Err(EINVAL);
    }
    if resolve & !0x3f != 0 || resolve & (RESOLVE_BENEATH | RESOLVE_IN_ROOT) == RESOLVE_BENEATH | RESOLVE_IN_ROOT {
        return Err(EINVAL);
    }
    if resolve & RESOLVE_CACHED != 0 && flags & (O_TRUNC | O_CREAT | O_TMPFILE_BIT) != 0 {
        return Err(EAGAIN);
    }
    let scope = crate::mount::Scope {
        no_xdev: resolve & RESOLVE_NO_XDEV != 0,
        no_magiclinks: resolve & RESOLVE_NO_MAGICLINKS != 0,
        no_symlinks: resolve & RESOLVE_NO_SYMLINKS != 0,
        beneath: resolve & RESOLVE_BENEATH != 0,
        in_root: resolve & RESOLVE_IN_ROOT != 0,
    };

    let path = user_path(t, path_ptr)?;
    if path.is_empty() {
        return Err(ENOENT);
    }
    // Chemin absolu sans portée : le descripteur de départ n'est pas consulté (comme Linux).
    let base = if path.starts_with('/') && !scope.scoped() { String::from("/") } else { dir_path_of(t, dirfd)? };
    let follow = flags & O_NOFOLLOW == 0 && flags & (O_CREAT | O_EXCL) != O_CREAT | O_EXCL;
    let root = t.process.fs.root();
    let abs = match crate::mount::resolve_scoped(&root, &base, &path, follow, scope, flags & O_CREAT != 0)? {
        crate::mount::Resolved::Found(f) => {
            if let Some(c) = f.virtual_path() {
                check_proc_links(t, &f, &c, follow, &scope)?;
            }
            f.path
        }
        crate::mount::Resolved::Missing { parent, name } => {
            if let Some(c) = parent.virtual_path() {
                check_proc_links(t, &parent, &c, true, &scope)?;
            }
            if parent.path == "/" { format!("/{}", name) } else { format!("{}/{}", parent.path, name) }
        }
    };
    open_abs(t, &abs, &path, flags, mode)
}

/// Liens de `/proc` traversés par une résolution d'`openat2` (`pick_link`, `nd_jump_link`) :
/// lien quelconque sous `RESOLVE_NO_SYMLINKS` → `ELOOP` ; lien magique sous
/// `RESOLVE_NO_MAGICLINKS` → `ELOOP`, sous une résolution bornée → `EXDEV`, sous
/// `RESOLVE_NO_XDEV` → `EXDEV` s'il mène hors de ce `proc`.
fn check_proc_links(t: &Thread, f: &crate::mount::Found, c: &str, follow_last: bool, s: &crate::mount::Scope) -> Result<(), i64> {
    if !matches!(f.target, crate::mount::Target::Virtual(crate::mount::VirtFs::Proc, _)) {
        return Ok(());
    }
    let (symlink, magic) = crate::procfs::links_in(c, follow_last);
    if symlink && s.no_symlinks {
        return Err(ELOOP);
    }
    let Some(link) = magic else { return Ok(()) };
    if s.no_symlinks || s.no_magiclinks {
        return Err(ELOOP);
    }
    if s.scoped() {
        return Err(EXDEV);
    }
    if s.no_xdev {
        let pid_ns = f.mnt.sb.proc_pid_ns();
        let link = crate::procfs::canonical_dirs(t, &pid_ns, &link);
        let target = crate::procfs::readlink(t, &pid_ns, &link).unwrap_or_default();
        if !(target == "/proc" || target.starts_with("/proc/")) {
            return Err(EXDEV);
        }
    }
    Ok(())
}

/// Ouverture de `abs` (chemin visible, normalisé) ; `path` est le chemin donné par l'appelant
/// (barre finale).
fn open_abs(t: &Thread, abs: &str, path: &str, flags: u64, mode: u64) -> SysResult {
    let abs = String::from(abs);
    let cloexec = flags & O_CLOEXEC != 0;
    let status = (flags as u32) & (O_ACCMODE | O_APPEND | O_NONBLOCK);
    let acc = status & O_ACCMODE;
    let wants_write = acc == O_WRONLY || acc == O_RDWR;
    let mut created = false;

    let nofollow = flags & O_NOFOLLOW != 0 || (flags & O_CREAT != 0 && flags & O_EXCL != 0);
    let mut nodev = false;
    let file = match lookup_any_mtab(t, &abs, !nofollow) {
        Ok(Lookup::Pts(fs, c)) if flags & O_PATH != 0 => {
            let node = match pts_node(t, &fs, &c) {
                Some(Special::Node(n @ Node::VirtDir { .. })) => n,
                Some(Special::Node(n)) => Node::Object(Arc::new(PathRef { path: abs.clone(), st: stat_node(&n) })),
                _ => return Err(ENOENT),
            };
            let f = OpenFile::new(node, status);
            return t.process.files.lock().alloc(f, 0, cloexec).map(|fd| fd as i64).ok_or(EMFILE);
        }
        Ok(Lookup::Pts(fs, c)) => {
            let node = match crate::pty::open_pts(t, &fs, &c, flags as u32) {
                Some(r) => r?,
                None => match pts_node(t, &fs, &c) {
                    Some(Special::Node(n)) => n,
                    _ => return Err(ENOENT),
                },
            };
            let f = OpenFile::new(node, status);
            return t.process.files.lock().alloc(f, 0, cloexec).map(|fd| fd as i64).ok_or(EMFILE);
        }
        Ok(Lookup::Special(Special::Node(node))) => {
            if matches!(node, Node::Generated { .. } | Node::VirtDir { .. }) && wants_write {
                return Err(if matches!(node, Node::VirtDir { .. }) { EISDIR } else { EACCES });
            }
            if flags & O_DIRECTORY != 0 && !matches!(node, Node::VirtDir { .. }) {
                return Err(ENOTDIR);
            }
            OpenFile::new(node, status)
        }
        Ok(Lookup::Special(Special::Dup(fd))) => get_fd(t, fd as u64)?,
        Ok(Lookup::Special(Special::File(f))) => f,
        looked => {
            let walked = match looked {
                Ok(Lookup::Vfs(f)) => match f.target {
                    crate::mount::Target::Node(n) => Ok((n, f.mnt)),
                    crate::mount::Target::Virtual(..) => Err(ENOENT),
                },
                Ok(_) => Err(ENOENT),
                Err(e) => Err(e),
            };
            let node = match walked {
                Ok((n, mnt)) => {
                    nodev = mnt.nodev();
                    if wants_write && flags & O_PATH == 0 && mnt.readonly() {
                        return Err(EROFS);
                    }
                    if flags & O_CREAT != 0 && flags & O_EXCL != 0 {
                        return Err(EEXIST);
                    }
                    // `chemin/` désigne un dossier.
                    if path.ends_with('/') && !matches!(n, FileOrDir::Dir(_)) && n.read_link().is_none() {
                        return Err(ENOTDIR);
                    }
                    // `O_NOFOLLOW` sur un lien : `ELOOP` (sauf `O_PATH`, qui désigne le lien).
                    if flags & O_NOFOLLOW != 0 && flags & O_PATH == 0 && n.read_link().is_some() {
                        return Err(ELOOP);
                    }
                    if wants_write && flags & O_PATH == 0 && n.read_only() {
                        return Err(EROFS);
                    }
                    if flags & O_PATH == 0 {
                        let want = match acc {
                            O_WRONLY => crate::creds::MAY_WRITE,
                            O_RDWR => crate::creds::MAY_READ | crate::creds::MAY_WRITE,
                            _ if flags & O_TRUNC != 0 => crate::creds::MAY_READ | crate::creds::MAY_WRITE,
                            _ => crate::creds::MAY_READ,
                        };
                        check_access(t, &n, want)?;
                    }
                    n
                }
                Err(ENOENT) => {
                    if flags & O_CREAT == 0 {
                        return Err(ENOENT);
                    }
                    // `O_CREAT` ne crée pas de dossier : `nouveau/` rend `EISDIR` (comme Linux).
                    if path.ends_with('/') {
                        return Err(EISDIR);
                    }
                    let (parent, name) = split_parent(t, &abs)?;
                    if parent.lock().read_only() {
                        return Err(EROFS);
                    }
                    check_access(t, &FileOrDir::Dir(parent.clone()), crate::creds::MAY_WRITE | crate::creds::MAY_EXEC)?;
                    let f = FileOrDir::File(create_file_in(&parent, &name, creation_perm(t, mode))?);
                    created = true;
                    give_to_caller(t, f)
                }
                Err(e) => return Err(e),
            };
            match node {
                FileOrDir::Dir(dir) => {
                    if wants_write {
                        return Err(EISDIR);
                    }
                    OpenFile::new(Node::Dir { dir, path: abs.clone() }, status)
                }
                FileOrDir::File(file) => {
                    if flags & O_DIRECTORY != 0 {
                        return Err(ENOTDIR);
                    }
                    // Nœuds spéciaux (`mknod`) : tube nommé, périphérique.
                    let kind = if flags & O_PATH == 0 { file.lock().metadata().map(|m| (m.kind, m.rdev)) } else { None };
                    match kind {
                        Some((NodeKind::Fifo, _)) => {
                            let node = crate::fifo::open(t, &file, acc, status & O_NONBLOCK != 0)?;
                            let f = OpenFile::new(node, status);
                            return t.process.files.lock().alloc(f, 0, cloexec).map(|fd| fd as i64).ok_or(EMFILE);
                        }
                        Some((NodeKind::CharDev, rdev)) => {
                            // Montage `nodev`, ou système monté depuis un espace utilisateur.
                            if nodev {
                                return Err(EACCES);
                            }
                            let node = char_device(t, &abs, rdev, flags as u32)?;
                            let f = OpenFile::new(node, status);
                            return t.process.files.lock().alloc(f, 0, cloexec).map(|fd| fd as i64).ok_or(EMFILE);
                        }
                        Some((NodeKind::BlockDev, _)) => return Err(ENXIO),
                        // Socket du domaine Unix : `connect`, pas `open` (Linux : `ENXIO`).
                        Some((NodeKind::Socket, _)) => return Err(ENXIO),
                        _ => {}
                    }
                    if flags & O_TRUNC != 0 && wants_write && flags & O_PATH == 0 {
                        file.lock().set_len(0).map_err(|_| EROFS)?;
                        crate::pagecache::invalidate(&file);
                    }
                    OpenFile::new(Node::File { file, path: abs.clone() }, status)
                }
            }
        }
    };
    if crate::inotify::active() {
        if created {
            crate::inotify::created(&abs, false);
        }
        if flags & O_PATH == 0 {
            crate::inotify::open_file_event(&file, crate::inotify::IN_OPEN);
        }
    }
    t.process.files.lock().alloc(file, 0, cloexec).map(|fd| fd as i64).ok_or(EMFILE)
}

pub fn close(t: &Thread, fd: u64) -> SysResult {
    if fd > i32::MAX as u64 {
        return Err(EBADF);
    }
    let f = t.process.files.lock().take(fd as usize).ok_or(EBADF)?;
    // POSIX : fermer un descripteur d'un fichier rend tous les verrous du processus sur ce fichier.
    crate::reclock::on_fd_close(t.process.pid, &f);
    drop(f);
    Ok(0)
}

/// `close_range(first, last, flags)` (Linux 5.9) : ce que `subprocess` de Python et
/// `closefrom` de la glibc font dans un enfant avant `execve`.
/// `CLOSE_RANGE_CLOEXEC` marque au lieu de fermer. `CLOSE_RANGE_UNSHARE` demande une table
/// propre : la table d'un processus est déjà la sienne, sauf après `clone(CLONE_FILES)` sans
/// `CLONE_THREAD`, cas refusé (`EINVAL`, la glibc retombe alors sur des `close` un par un).
pub fn close_range(t: &Thread, first: u64, last: u64, flags: u64) -> SysResult {
    const CLOSE_RANGE_UNSHARE: u64 = 1 << 1;
    const CLOSE_RANGE_CLOEXEC: u64 = 1 << 2;
    let (first, last) = (first as u32 as usize, last as u32 as usize);
    if flags & !(CLOSE_RANGE_UNSHARE | CLOSE_RANGE_CLOEXEC) != 0 || first > last {
        return Err(EINVAL);
    }
    if flags & CLOSE_RANGE_UNSHARE != 0 && crate::shares_files(&t.process) {
        return Err(EINVAL);
    }
    if flags & CLOSE_RANGE_CLOEXEC != 0 {
        t.process.files.lock().set_cloexec_range(first, last);
        return Ok(0);
    }
    let taken = t.process.files.lock().take_range(first, last);
    for f in &taken {
        crate::reclock::on_fd_close(t.process.pid, f);
    }
    drop(taken);
    Ok(0)
}

/// `fadvise64(fd, offset, len, advice)` : conseils acceptés et vérifiés comme Linux (`EBADF`,
/// `ESPIPE` sur un tube, `EINVAL` pour un conseil inconnu), sans effet pour l'instant.
pub fn fadvise64(t: &Thread, fd: u64, offset: i64, len: i64, advice: u64) -> SysResult {
    let f = get_fd(t, fd)?;
    if let Node::Pipe(_) = &f.node {
        return Err(ESPIPE);
    }
    if advice > 5 || len < 0 {
        return Err(EINVAL);
    }
    // POSIX_FADV_WILLNEED : lecture anticipée (longueur nulle = jusqu'à la fin).
    if advice == 3 && offset >= 0 {
        let len = if len == 0 { usize::MAX } else { len as usize };
        prefetch(&f, offset as usize, len);
    }
    Ok(0)
}

/// `readahead(fd, offset, count)` : lit d'avance dans le cache de pages (`EBADF` si le
/// descripteur n'est pas ouvert en lecture, `EINVAL` hors fichier ordinaire, comme Linux).
pub fn readahead(t: &Thread, fd: u64, offset: i64, count: u64) -> SysResult {
    let f = get_fd(t, fd)?;
    if !f.readable() {
        return Err(EBADF);
    }
    if !matches!(&f.node, Node::File { .. }) || offset < 0 {
        return Err(EINVAL);
    }
    prefetch(&f, offset as usize, count as usize);
    Ok(0)
}

/// Lecture anticipée des fichiers immuables (ext4) dont les pages passent par le cache ; sans
/// effet ailleurs (tmpfs et memfs sont déjà en mémoire).
fn prefetch(f: &OpenFile, offset: usize, len: usize) {
    if let Node::File { file, .. } = &f.node {
        if file.lock().read_only() {
            crate::pagecache::get(file).prefetch(offset, len);
        }
    }
}

pub fn dup(t: &Thread, fd: u64) -> SysResult {
    let f = get_fd(t, fd)?;
    t.process.files.lock().alloc(f, 0, false).map(|n| n as i64).ok_or(EMFILE)
}

/// `dup2` (`legacy`) ou `dup3`.
pub fn dup3(t: &Thread, old: u64, new: u64, flags: u64, legacy: bool) -> SysResult {
    let f = get_fd(t, old)?;
    if new >= crate::fd::NOFILE as u64 {
        return Err(EBADF);
    }
    if old == new {
        return if legacy { Ok(new as i64) } else { Err(EINVAL) };
    }
    if flags & !O_CLOEXEC != 0 {
        return Err(EINVAL);
    }
    let old = t.process.files.lock().install(new as usize, f, flags & O_CLOEXEC != 0);
    if let Some(old) = old {
        crate::reclock::on_fd_close(t.process.pid, &old);
    }
    Ok(new as i64)
}

pub fn fcntl(t: &Thread, fd: u64, cmd: u64, arg: u64) -> SysResult {
    const F_DUPFD: u64 = 0;
    const F_GETFD: u64 = 1;
    const F_SETFD: u64 = 2;
    const F_GETFL: u64 = 3;
    const F_SETFL: u64 = 4;
    const F_GETLK: u64 = 5;
    const F_SETLK: u64 = 6;
    const F_SETLKW: u64 = 7;
    const F_SETOWN: u64 = 8;
    const F_GETOWN: u64 = 9;
    const F_DUPFD_CLOEXEC: u64 = 1030;
    const F_SETPIPE_SZ: u64 = 1031;
    const F_GETPIPE_SZ: u64 = 1032;
    const F_ADD_SEALS: u64 = 1033;
    const F_GET_SEALS: u64 = 1034;
    let f = get_fd(t, fd)?;
    match cmd {
        F_DUPFD | F_DUPFD_CLOEXEC => {
            if arg >= crate::fd::NOFILE as u64 {
                return Err(EINVAL);
            }
            t.process.files.lock().alloc(f, arg as usize, cmd == F_DUPFD_CLOEXEC).map(|n| n as i64).ok_or(EMFILE)
        }
        F_GETFD => Ok(t.process.files.lock().cloexec(fd as usize).map_or(0, |c| c as i64)),
        F_SETFD => {
            t.process.files.lock().set_cloexec(fd as usize, arg & 1 != 0);
            Ok(0)
        }
        F_GETFL => Ok(f.flags() as i64),
        F_SETFL => {
            let keep = f.flags() & O_ACCMODE;
            f.flags.store(keep | (arg as u32 & (O_APPEND | O_NONBLOCK)), Ordering::Relaxed);
            Ok(0)
        }
        F_GETLK | F_SETLK | F_SETLKW | crate::reclock::F_OFD_GETLK | crate::reclock::F_OFD_SETLK | crate::reclock::F_OFD_SETLKW => {
            crate::reclock::fcntl_lock(t, &f, cmd, arg, crate::reclock::FlockLayout::Native)
        }
        F_SETOWN => Ok(0),
        F_GETOWN => Ok(0),
        F_GETPIPE_SZ | F_SETPIPE_SZ => match &f.node {
            Node::Pipe(end) if cmd == F_GETPIPE_SZ => Ok(end.pipe.capacity() as i64),
            Node::Pipe(end) => end.pipe.set_capacity(arg as usize).map(|n| n as i64),
            _ => Err(EBADF),
        },
        F_ADD_SEALS | F_GET_SEALS => match &f.node {
            Node::File { file, .. } if cmd == F_GET_SEALS => crate::seals::get(file).map(|s| s as i64).ok_or(EINVAL),
            Node::File { file, .. } => crate::seals::add(file, f.writable(), arg as u32).map(|_| 0),
            _ => Err(EINVAL),
        },
        _ => Err(EINVAL),
    }
}

// --- Lecture et écriture -------------------------------------------------------------------

/// Lit dans un tampon noyau ; `at` : position explicite (`pread`), sinon celle du fichier.
/// Lit dans un tampon noyau. `nowait` : jamais d'attente, même sur une description bloquante
/// (tentative en ligne d'io_uring, comme `IOCB_NOWAIT`).
/// Lecture, puis `IN_ACCESS` pour un fichier (inotify).
fn kread(t: &Thread, f: &OpenFile, kb: &mut [u8], at: Option<usize>, nowait: bool) -> Result<usize, i64> {
    let r = kread_raw(t, f, kb, at, nowait);
    if matches!(r, Ok(n) if n > 0) && matches!(f.node, Node::File { .. }) {
        crate::inotify::open_file_event(f, crate::inotify::IN_ACCESS);
    }
    r
}

fn kread_raw(t: &Thread, f: &OpenFile, kb: &mut [u8], at: Option<usize>, nowait: bool) -> Result<usize, i64> {
    if !f.readable() {
        return Err(EBADF);
    }
    let read_bytes = |data: &[u8], kb: &mut [u8], off: usize| -> usize {
        if off >= data.len() {
            return 0;
        }
        let n = kb.len().min(data.len() - off);
        kb[..n].copy_from_slice(&data[off..off + n]);
        n
    };
    match &f.node {
        Node::Console { input: Some(r), .. } => {
            if at.is_some() {
                return Err(ESPIPE);
            }
            crate::console::read(t, r, kb, f.nonblock() || nowait)
        }
        Node::Console { input: None, .. } => Err(EBADF),
        Node::Pipe(end) => {
            if at.is_some() {
                return Err(ESPIPE);
            }
            crate::pipe::read(t, end, kb, f.flags() & O_NONBLOCK != 0 || nowait)
        }
        Node::Object(o) => {
            if at.is_some() {
                return Err(ESPIPE);
            }
            o.read(t, kb, f.nonblock() || nowait)
        }
        Node::Null => Ok(0),
        Node::Zero => {
            kb.fill(0);
            Ok(kb.len())
        }
        Node::Random => {
            for c in kb.chunks_mut(8) {
                let v = uvm::rand::random_u64().to_le_bytes();
                c.copy_from_slice(&v[..c.len()]);
            }
            Ok(kb.len())
        }
        Node::Dir { .. } | Node::VirtDir { .. } => Err(EISDIR),
        Node::Generated { data, .. } => {
            let off = at.unwrap_or_else(|| *f.pos.lock());
            let n = read_bytes(data, kb, off);
            if at.is_none() {
                *f.pos.lock() = off + n;
            }
            Ok(n)
        }
        Node::File { file, .. } => {
            let off = at.unwrap_or_else(|| *f.pos.lock());
            let n = {
                let mut locked = file.lock();
                let len = locked.len();
                if off >= len {
                    0
                } else if locked.read_only() && len <= crate::pagecache::MAX_READ_CACHED {
                    // Fichier immuable (ext4) : à travers le cache de pages, comme les
                    // projections (une seule lecture du disque, lecture anticipée).
                    drop(locked);
                    let want = kb.len().min(len - off);
                    crate::pagecache::get(file).read(off, &mut kb[..want])
                } else {
                    let want = kb.len().min(len - off);
                    locked.read_at(&mut kb[..want], off).map_err(|_| EIO)?
                }
            };
            crate::pagecache::overlay_read(file, off, &mut kb[..n]);
            if at.is_none() {
                *f.pos.lock() = off + n;
            }
            Ok(n)
        }
    }
}

/// Écrit un tampon noyau.
/// Écriture, puis `IN_MODIFY` pour un fichier (inotify).
fn kwrite(t: &Thread, f: &OpenFile, kb: &[u8], at: Option<usize>, nowait: bool) -> Result<usize, i64> {
    let r = kwrite_raw(t, f, kb, at, nowait);
    if matches!(r, Ok(n) if n > 0) && matches!(f.node, Node::File { .. }) {
        crate::inotify::open_file_event(f, crate::inotify::IN_MODIFY);
    }
    r
}

fn kwrite_raw(t: &Thread, f: &OpenFile, kb: &[u8], at: Option<usize>, nowait: bool) -> Result<usize, i64> {
    if !f.writable() {
        return Err(EBADF);
    }
    match &f.node {
        Node::Console { output: Some(w), .. } => {
            if at.is_some() {
                return Err(ESPIPE);
            }
            w.write_all(kb).map_err(|_| EIO)?;
            if t.process.log_output {
                t.process.log_bytes(kb);
            }
            Ok(kb.len())
        }
        Node::Console { output: None, .. } => Err(EBADF),
        Node::Pipe(end) => {
            if at.is_some() {
                return Err(ESPIPE);
            }
            crate::pipe::write(t, end, kb, f.flags() & O_NONBLOCK != 0 || nowait)
        }
        Node::Object(o) => {
            if at.is_some() {
                return Err(ESPIPE);
            }
            o.write(t, kb, f.nonblock() || nowait)
        }
        Node::Null | Node::Zero | Node::Random => Ok(kb.len()),
        Node::Dir { .. } | Node::VirtDir { .. } => Err(EISDIR),
        Node::Generated { .. } => Err(EBADF),
        Node::File { file, .. } => {
            let mut locked = file.lock();
            let off = match at {
                Some(o) => o,
                None if f.flags() & O_APPEND != 0 => locked.len(),
                None => *f.pos.lock(),
            };
            crate::seals::check_write(file, off, kb.len(), locked.len())?;
            let n = locked
                .write_at(kb, off)
                .map_err(|e| if locked.read_only() { EROFS } else { no_space("write", &e) })?;
            drop(locked);
            crate::pagecache::invalidate(file);
            crate::pagecache::after_write(file, off, &kb[..n]);
            if at.is_none() {
                *f.pos.lock() = off + n;
            }
            Ok(n)
        }
    }
}

/// Une lecture de terminal rend la main dès qu'une ligne (ou un octet en mode brut) est là.
fn is_stream(f: &OpenFile) -> bool { matches!(f.node, Node::Console { .. } | Node::Pipe(_) | Node::Object(_)) }

fn do_read(t: &Thread, f: &OpenFile, buf: u64, count: usize, at: Option<usize>, nowait: bool) -> SysResult {
    if count == 0 {
        return Ok(0);
    }
    let mut kb = vec![0u8; count.min(CHUNK)];
    let mut done = 0usize;
    while done < count {
        let want = (count - done).min(kb.len());
        let n = match kread(t, f, &mut kb[..want], at.map(|o| o + done), nowait) {
            Ok(n) => n,
            Err(_) if done > 0 => break,
            Err(e) => return Err(e),
        };
        if n == 0 {
            break;
        }
        t.mm().copy_to_user(buf as usize + done, &kb[..n]).map_err(fault)?;
        done += n;
        if n < want || is_stream(f) {
            break;
        }
    }
    Ok(done as i64)
}

/// `readv`/`writev` d'un flux en une seule opération (voir [`rw_iov`]).
fn stream_iov(t: &Thread, f: &OpenFile, write: bool, iov: &[(u64, u64)], sum: usize, nowait: bool) -> SysResult {
    let mut kb = vec![0u8; sum.min(16 * CHUNK)];
    if write {
        let mut off = 0;
        for &(base, len) in iov {
            let n = (len as usize).min(kb.len() - off);
            t.mm().copy_from_user(&mut kb[off..off + n], base as usize).map_err(fault)?;
            off += n;
        }
        return Ok(kwrite(t, f, &kb, None, nowait)? as i64);
    }
    let n = kread(t, f, &mut kb, None, nowait)?;
    let mut off = 0;
    for &(base, len) in iov {
        if off >= n {
            break;
        }
        let k = (len as usize).min(n - off);
        if let Err(e) = t.mm().copy_to_user(base as usize, &kb[off..off + k]) {
            return if off > 0 { Ok(off as i64) } else { Err(fault(e)) };
        }
        off += k;
    }
    Ok(n as i64)
}

fn do_write(t: &Thread, f: &OpenFile, buf: u64, count: usize, at: Option<usize>, nowait: bool) -> SysResult {
    if count == 0 {
        return Ok(0);
    }
    let mut kb = vec![0u8; count.min(CHUNK)];
    let mut done = 0usize;
    while done < count {
        let want = (count - done).min(kb.len());
        if let Err(e) = t.mm().copy_from_user(&mut kb[..want], buf as usize + done) {
            if done > 0 {
                break;
            }
            return Err(fault(e));
        }
        let n = match kwrite(t, f, &kb[..want], at.map(|o| o + done), nowait) {
            Ok(n) => n,
            Err(_) if done > 0 => break,
            Err(e) => return Err(e),
        };
        done += n;
        if n < want {
            break;
        }
    }
    Ok(done as i64)
}

pub fn read(t: &Thread, fd: u64, buf: u64, count: u64) -> SysResult {
    let f = get_fd(t, fd)?;
    do_read(t, &f, buf, count as usize, None, false)
}

pub fn write(t: &Thread, fd: u64, buf: u64, count: u64) -> SysResult {
    let f = get_fd(t, fd)?;
    do_write(t, &f, buf, count as usize, None, false)
}

pub fn pread(t: &Thread, fd: u64, buf: u64, count: u64, pos: i64) -> SysResult {
    if pos < 0 {
        return Err(EINVAL);
    }
    let f = get_fd(t, fd)?;
    do_read(t, &f, buf, count as usize, Some(pos as usize), false)
}

pub fn pwrite(t: &Thread, fd: u64, buf: u64, count: u64, pos: i64) -> SysResult {
    if pos < 0 {
        return Err(EINVAL);
    }
    let f = get_fd(t, fd)?;
    do_write(t, &f, buf, count as usize, Some(pos as usize), false)
}

pub(crate) fn iovecs(t: &Thread, iov: u64, cnt: u64) -> Result<Vec<(u64, u64)>, i64> {
    if cnt > 1024 {
        return Err(EINVAL);
    }
    if super::compat::active(t) {
        // `struct compat_iovec` : base et longueur de 4 octets.
        let mut raw = vec![0u8; cnt as usize * 8];
        t.mm().copy_from_user(&mut raw, iov as usize).map_err(fault)?;
        return Ok(raw
            .chunks_exact(8)
            .map(|c| (u32::from_le_bytes(c[..4].try_into().unwrap()) as u64, u32::from_le_bytes(c[4..].try_into().unwrap()) as u64))
            .collect());
    }
    let mut raw = vec![0u8; cnt as usize * 16];
    t.mm().copy_from_user(&mut raw, iov as usize).map_err(fault)?;
    Ok(raw
        .chunks_exact(16)
        .map(|c| (u64::from_le_bytes(c[..8].try_into().unwrap()), u64::from_le_bytes(c[8..].try_into().unwrap())))
        .collect())
}

pub fn readv(t: &Thread, fd: u64, iov: u64, cnt: u64, pos: Option<i64>) -> SysResult {
    let f = get_fd(t, fd)?;
    rw_iov(t, &f, false, &iovecs(t, iov, cnt)?, pos.map(|p| p as usize), false)
}

pub fn writev(t: &Thread, fd: u64, iov: u64, cnt: u64, pos: Option<i64>) -> SysResult {
    let f = get_fd(t, fd)?;
    rw_iov(t, &f, true, &iovecs(t, iov, cnt)?, pos.map(|p| p as usize), false)
}

/// Lecture ou écriture vectorielle sur une description (`readv`, `writev`, io_uring).
/// `nowait` : `EAGAIN` au lieu d'attendre (tentative en ligne d'io_uring).
pub(crate) fn rw_iov(t: &Thread, f: &OpenFile, write: bool, iov: &[(u64, u64)], pos: Option<usize>, nowait: bool) -> SysResult {
    // Flux (tube, terminal, socket, objet) : une seule opération sur toute la longueur, comme
    // l'`iov_iter` de Linux. Une lecture vide alors le tube à travers tous les segments (au
    // lieu de s'arrêter au premier) et prend un datagramme entier ; une écriture d'au plus
    // `CHUNK` octets reste d'un seul tenant (atomique jusqu'à `PIPE_BUF` pour un tube).
    let sum: usize = iov.iter().map(|&(_, l)| l as usize).fold(0usize, |a, l| a.saturating_add(l));
    if is_stream(f) && sum > 0 && (!write || sum <= CHUNK) {
        return stream_iov(t, f, write, iov, sum, nowait);
    }
    let mut total = 0usize;
    for &(base, len) in iov {
        if len == 0 {
            continue;
        }
        let at = pos.map(|p| p + total);
        let r = if write { do_write(t, f, base, len as usize, at, nowait) } else { do_read(t, f, base, len as usize, at, nowait) };
        let n = match r {
            Ok(n) => n as usize,
            Err(e) if total == 0 => return Err(e),
            Err(_) => break,
        };
        total += n;
        if n < len as usize || (!write && is_stream(f)) {
            break;
        }
    }
    Ok(total as i64)
}

/// `sendfile(out_fd, in_fd, offset, count)`. Comme Linux : une erreur (`EAGAIN` d'une socket non
/// bloquante pleine, signal) après des octets transmis rend leur nombre, jamais l'erreur — sinon
/// l'appelant renverrait les mêmes octets ; la position (`*offset`, ou celle du fichier) n'avance
/// que des octets réellement écrits. Une sortie bloquante est servie en entier.
pub fn sendfile(t: &Thread, out_fd: u64, in_fd: u64, off_ptr: u64, count: u64) -> SysResult {
    sendfile_as(t, out_fd, in_fd, off_ptr, count, false)
}

/// `sendfile` i386 (appel 187) : `*offset` est un `compat_off_t` de 4 octets, et la position
/// ne dépasse pas 2 Gio (`MAX_NON_LFS`, `EOVERFLOW`), comme `compat_sys_sendfile`.
pub fn sendfile32(t: &Thread, out_fd: u64, in_fd: u64, off_ptr: u64, count: u64) -> SysResult {
    sendfile_as(t, out_fd, in_fd, off_ptr, count, true)
}

fn sendfile_as(t: &Thread, out_fd: u64, in_fd: u64, off_ptr: u64, count: u64, narrow: bool) -> SysResult {
    const MAX_NON_LFS: usize = i32::MAX as usize;
    let out = get_fd(t, out_fd)?;
    let inp = get_fd(t, in_fd)?;
    if !inp.readable() || !out.writable() {
        return Err(EBADF);
    }
    if matches!(inp.node, Node::Dir { .. } | Node::VirtDir { .. }) {
        return Err(EINVAL);
    }
    if out.flags() & O_APPEND != 0 && matches!(out.node, Node::File { .. }) {
        return Err(EINVAL);
    }
    if count == 0 {
        return Ok(0);
    }
    let pos = if narrow {
        if off_ptr == 0 {
            None
        } else {
            let v = t.mm().read_user::<i32>(off_ptr as usize).map_err(fault)?;
            if v < 0 {
                return Err(EINVAL);
            }
            Some(v as usize)
        }
    } else {
        read_off(t, off_ptr)?
    };
    let mut count = (count as usize).min(0x7fff_f000); // MAX_RW_COUNT
    if narrow {
        let at = match pos {
            Some(p) => p,
            None => *inp.pos.lock(),
        };
        if at >= MAX_NON_LFS {
            return Err(EOVERFLOW);
        }
        count = count.min(MAX_NON_LFS - at);
    }
    let (n, ip, _) = copy_fds(t, &inp, pos, &out, None, count, false, true)?;
    if narrow {
        if let (true, Some(v)) = (off_ptr != 0, ip) {
            t.mm().write_user::<i32>(off_ptr as usize, &(v as i32)).map_err(fault)?;
        }
    } else {
        write_off(t, off_ptr, ip)?;
    }
    Ok(n as i64)
}

pub fn lseek(t: &Thread, fd: u64, off: i64, whence: u64) -> SysResult {
    let f = get_fd(t, fd)?;
    let len = match &f.node {
        Node::Console { .. } | Node::Pipe(_) | Node::Object(_) => return Err(ESPIPE),
        Node::Null | Node::Zero | Node::Random => return Ok(0),
        Node::File { file, .. } => file.lock().len() as i64,
        Node::Generated { data, .. } => data.len() as i64,
        Node::Dir { .. } | Node::VirtDir { .. } => 0,
    };
    let mut pos = f.pos.lock();
    let new = match whence {
        0 => off,
        1 => *pos as i64 + off,
        2 => len + off,
        3 => if off < len { off } else { return Err(ENXIO) }, // SEEK_DATA
        4 => if off < len { len } else { return Err(ENXIO) }, // SEEK_HOLE
        _ => return Err(EINVAL),
    };
    if new < 0 {
        return Err(EINVAL);
    }
    *pos = new as usize;
    Ok(new)
}

pub fn ftruncate(t: &Thread, fd: u64, len: i64) -> SysResult {
    if len < 0 {
        return Err(EINVAL);
    }
    let f = get_fd(t, fd)?;
    match &f.node {
        Node::File { file, .. } if f.writable() => {
            let mut locked = file.lock();
            crate::seals::check_resize(file, locked.len(), len as usize)?;
            crate::pagecache::invalidate(file);
            locked.set_len(len as usize).map_err(|_| EROFS)?;
            drop(locked);
            crate::inotify::open_file_event(&f, crate::inotify::IN_MODIFY);
            Ok(0)
        }
        Node::Dir { .. } => Err(EISDIR),
        _ => Err(EINVAL),
    }
}

pub fn truncate_path(t: &Thread, path: u64, len: i64) -> SysResult {
    if len < 0 {
        return Err(EINVAL);
    }
    let abs = absolute(t, AT_FDCWD, &user_path(t, path)?)?;
    match walk_mnt(t, &abs, true) {
        Ok((FileOrDir::File(f), mnt)) => {
            if mnt.readonly() {
                return Err(EROFS);
            }
            check_access(t, &FileOrDir::File(f.clone()), crate::creds::MAY_WRITE)?;
            let mut locked = f.lock();
            crate::seals::check_resize(&f, locked.len(), len as usize)?;
            crate::pagecache::invalidate(&f);
            locked.set_len(len as usize).map_err(|_| EROFS)?;
            drop(locked);
            crate::inotify::file_event(&FileOrDir::File(f), &abs, crate::inotify::IN_MODIFY);
            Ok(0)
        }
        Ok((FileOrDir::Dir(_), _)) => Err(EISDIR),
        Err(e) => Err(e),
    }
}

pub fn fsync(t: &Thread, fd: u64) -> SysResult {
    let f = get_fd(t, fd)?;
    if let Node::File { file, .. } = &f.node {
        crate::pagecache::sync_file(file);
    }
    Ok(0)
}

// --- Terminal -------------------------------------------------------------------------------

pub fn ioctl(t: &Thread, fd: u64, req: u64, arg: u64) -> SysResult {
    const TCGETS: u64 = 0x5401;
    const TCSETS: u64 = 0x5402;
    const TCSETSW: u64 = 0x5403;
    const TCSETSF: u64 = 0x5404;
    const TIOCSCTTY: u64 = 0x540E;
    const TIOCGPGRP: u64 = 0x540F;
    const TIOCSPGRP: u64 = 0x5410;
    const TIOCGWINSZ: u64 = 0x5413;
    const TIOCSWINSZ: u64 = 0x5414;
    const FIONREAD: u64 = 0x541B;
    const FIONBIO: u64 = 0x5421;
    const FIONCLEX: u64 = 0x5450;
    const FIOCLEX: u64 = 0x5451;
    const ISIG: u32 = 0x1;
    const ICANON: u32 = 0x2;
    const ECHO: u32 = 0x8;
    const ECHOE: u32 = 0x10;
    const ECHOK: u32 = 0x20;
    const ECHOCTL: u32 = 0x200;
    const ECHOKE: u32 = 0x800;
    const IEXTEN: u32 = 0x8000;

    let f = get_fd(t, fd)?;
    match req {
        FIOCLEX | FIONCLEX => {
            t.process.files.lock().set_cloexec(fd as usize, req == FIOCLEX);
            return Ok(0);
        }
        FIONBIO => {
            let on = t.mm().read_user::<i32>(arg as usize).map_err(fault)? != 0;
            let fl = f.flags();
            f.flags.store(if on { fl | O_NONBLOCK } else { fl & !O_NONBLOCK }, Ordering::Relaxed);
            return Ok(0);
        }
        _ => {}
    }
    if let Node::Object(o) = &f.node {
        // Interfaces réseau (`SIOCGIFINDEX`, `SIOCGIFCONF`…) : servies sur une socket de toute
        // famille (`sock_ioctl` → `dev_ioctl`) ; glibc ouvre d'abord une socket `AF_UNIX`.
        if let Some(s) = o.as_socket().filter(|_| (0x8910..=0x89ff).contains(&req)) {
            let net = s.sock_net().unwrap_or_else(|| crate::netns::of(&t.process));
            return crate::inet::if_ioctl(t, &net, req, arg);
        }
        return match (req, o.pending_bytes()) {
            (FIONREAD, Some(n)) => {
                t.mm().write_user::<i32>(arg as usize, &(n as i32)).map_err(fault)?;
                Ok(0)
            }
            _ => o.ioctl(t, req, arg),
        };
    }
    if let Node::Pipe(end) = &f.node {
        return match req {
            FIONREAD => {
                t.mm().write_user::<i32>(arg as usize, &(end.pipe.available() as i32)).map_err(fault)?;
                Ok(0)
            }
            _ => Err(ENOTTY),
        };
    }
    let Node::Console { discipline, input, .. } = &f.node else {
        return Err(ENOTTY);
    };
    let mm = t.mm();
    if req == FIONREAD {
        let n = input.as_ref().and_then(|i| i.available()).unwrap_or(0);
        mm.write_user::<i32>(arg as usize, &(n as i32)).map_err(fault)?;
        return Ok(0);
    }
    match req {
        TCGETS => {
            let (canon, echo) = discipline.as_ref().map_or((true, true), |d| (d.canonical(), d.echo()));
            let mut lflag = ISIG | IEXTEN | ECHOCTL | ECHOKE;
            if canon { lflag |= ICANON; }
            if echo { lflag |= ECHO | ECHOE | ECHOK; }
            let mut tio = [0u8; 36];
            tio[0..4].copy_from_slice(&0x500u32.to_le_bytes()); // ICRNL | IXON
            tio[4..8].copy_from_slice(&0x5u32.to_le_bytes()); // OPOST | ONLCR
            tio[8..12].copy_from_slice(&0xbfu32.to_le_bytes()); // B38400 | CS8 | CREAD
            tio[12..16].copy_from_slice(&lflag.to_le_bytes());
            let cc = &mut tio[17..36];
            cc[0] = 3; // VINTR
            cc[1] = 0x1c; // VQUIT
            cc[2] = 0x7f; // VERASE
            cc[3] = 0x15; // VKILL
            cc[4] = 4; // VEOF
            cc[5] = 0; // VTIME
            cc[6] = 1; // VMIN
            cc[8] = 0x11; // VSTART
            cc[9] = 0x13; // VSTOP
            cc[10] = 0x1a; // VSUSP
            cc[12] = 0x12; // VREPRINT
            cc[14] = 0x17; // VWERASE
            cc[15] = 0x16; // VLNEXT
            mm.copy_to_user(arg as usize, &tio).map_err(fault)?;
            Ok(0)
        }
        TCSETS | TCSETSW | TCSETSF => {
            let lflag = mm.read_user::<u32>(arg as usize + 12).map_err(fault)?;
            if let Some(d) = discipline {
                d.set_canonical(lflag & ICANON != 0);
                d.set_echo(lflag & ECHO != 0);
            }
            Ok(0)
        }
        TIOCGWINSZ => {
            let ws: [u16; 4] = [25, 80, 0, 0];
            mm.write_user(arg as usize, &ws).map_err(fault)?;
            Ok(0)
        }
        TIOCSWINSZ | TIOCSCTTY | TIOCSPGRP => Ok(0),
        TIOCGPGRP => {
            mm.write_user::<i32>(arg as usize, &(t.process.vpid as i32)).map_err(fault)?;
            Ok(0)
        }
        FIONREAD => {
            mm.write_user::<i32>(arg as usize, &0).map_err(fault)?;
            Ok(0)
        }
        _ => Err(ENOTTY),
    }
}

const POLLIN: i16 = 0x1;
const POLLPRI: i16 = 0x2;
const POLLOUT: i16 = 0x4;
const POLLERR: i16 = 0x8;
const POLLHUP: i16 = 0x10;
const POLLNVAL: i16 = 0x20;

/// Événements prêts sur `f` parmi `events` (erreur, raccroché et invalide toujours rapportés).
fn revents_of(t: &Thread, f: &OpenFile, events: i16) -> i16 {
    let r = crate::poll::poll_file(f, Some(t)) as i16;
    r & (events | POLLERR | POLLHUP | POLLNVAL)
}

/// Attend qu'au moins un des `(fd, événements)` soit prêt (ou le délai, ou un signal) ; renvoie
/// les événements prêts de chacun. `timeout` : `None` = sans limite.
fn wait_ready(t: &Thread, entries: &[(i32, i16)], timeout: Option<core::time::Duration>) -> Result<Vec<i16>, i64> {
    let files: Vec<Option<Arc<OpenFile>>> = {
        let table = t.process.files.lock();
        entries.iter().map(|&(fd, _)| if fd < 0 { None } else { table.get(fd as usize) }).collect()
    };
    let compute = || -> (usize, Vec<i16>) {
        let mut n = 0;
        let rev: Vec<i16> = entries
            .iter()
            .zip(files.iter())
            .map(|(&(fd, ev), f)| {
                let r = match f {
                    _ if fd < 0 => 0,
                    None => POLLNVAL,
                    Some(f) => revents_of(t, f, ev),
                };
                if r != 0 {
                    n += 1;
                }
                r
            })
            .collect();
        (n, rev)
    };
    if timeout == Some(core::time::Duration::ZERO) {
        return Ok(compute().1);
    }
    let mut queues: Vec<&crate::wait::WaitQueue> = Vec::new();
    for f in files.iter().flatten() {
        crate::poll::for_each_queue(f, &mut |q| queues.push(q));
    }
    let deadline = timeout.map(|d| time::Instant::now() + d);
    match crate::wait::wait_event(&queues, deadline, Some(t), || {
        let (n, rev) = compute();
        (n > 0).then_some(rev)
    }) {
        Ok(rev) => Ok(rev),
        Err(crate::wait::WaitError::TimedOut) => Ok(compute().1),
        Err(crate::wait::WaitError::Interrupted) => Err(crate::signal::ERESTARTNOHAND),
    }
}

fn do_poll(t: &Thread, fds: u64, nfds: u64, timeout: Option<core::time::Duration>) -> SysResult {
    if nfds > crate::fd::NOFILE as u64 {
        return Err(EINVAL);
    }
    let mut raw = vec![0u8; nfds as usize * 8];
    t.mm().copy_from_user(&mut raw, fds as usize).map_err(fault)?;
    let entries: Vec<(i32, i16)> = raw
        .chunks_exact(8)
        .map(|e| (i32::from_le_bytes(e[0..4].try_into().unwrap()), i16::from_le_bytes(e[4..6].try_into().unwrap())))
        .collect();
    let rev = wait_ready(t, &entries, timeout)?;
    let mut ready = 0;
    for (e, r) in raw.chunks_exact_mut(8).zip(rev.iter()) {
        e[6..8].copy_from_slice(&r.to_le_bytes());
        if *r != 0 {
            ready += 1;
        }
    }
    t.mm().copy_to_user(fds as usize, &raw).map_err(fault)?;
    Ok(ready)
}

pub fn poll(t: &Thread, fds: u64, nfds: u64, timeout_ms: i64) -> SysResult {
    let timeout = if timeout_ms < 0 { None } else { Some(core::time::Duration::from_millis(timeout_ms as u64)) };
    do_poll(t, fds, nfds, timeout)
}

pub(crate) fn read_ts(t: &Thread, p: u64) -> Result<Option<core::time::Duration>, i64> {
    if p == 0 {
        return Ok(None);
    }
    let ts = super::compat::get_timespec(t, p)?;
    if ts[0] < 0 || !(0..1_000_000_000).contains(&ts[1]) {
        return Err(EINVAL);
    }
    Ok(Some(core::time::Duration::new(ts[0] as u64, ts[1] as u32)))
}

/// Masque temporaire de `ppoll`/`pselect6` : rétabli au retour, sauf si un gestionnaire s'exécute
/// (le cadre l'enregistre, `rt_sigreturn` le rétablira).
pub(crate) fn with_temp_mask(t: &Thread, mask_ptr: u64, f: impl FnOnce() -> SysResult) -> SysResult {
    if mask_ptr == 0 {
        return f();
    }
    let m = t.mm().read_user::<u64>(mask_ptr as usize).map_err(fault)?;
    let old = t.sigmask.swap(crate::signal::sanitize_mask(m), Ordering::AcqRel);
    *t.saved_sigmask.lock() = Some(old);
    crate::signal::recalc(t);
    let r = f();
    if r != Err(crate::signal::ERESTARTNOHAND) {
        if let Some(o) = t.saved_sigmask.lock().take() {
            t.sigmask.store(o, Ordering::Release);
        }
    }
    r
}

pub fn ppoll(t: &Thread, fds: u64, nfds: u64, tmo: u64, sigmask: u64, sigsetsize: u64) -> SysResult {
    if sigmask != 0 && sigsetsize != 8 {
        return Err(EINVAL);
    }
    let timeout = read_ts(t, tmo)?;
    with_temp_mask(t, sigmask, || do_poll(t, fds, nfds, timeout))
}

/// `select` et `pselect6` : ensembles de bits convertis en attente `poll`.
fn do_select(t: &Thread, nfds: u64, rset: u64, wset: u64, eset: u64, timeout: Option<core::time::Duration>) -> SysResult {
    if nfds > crate::fd::NOFILE as u64 {
        return Err(EINVAL);
    }
    let words = (nfds as usize + 63) / 64;
    // Octets lus et écrits de chaque `fd_set` : des `long` entiers, de 4 octets en i386
    // (`compat_get_fd_set`). Petit-boutiste : les mêmes bits, mais jamais au-delà du dernier mot.
    let bytes = if super::compat::active(t) { (nfds as usize + 31) / 32 * 4 } else { words * 8 };
    let read_set = |p: u64| -> Result<Vec<u64>, i64> {
        let mut v = vec![0u64; words];
        if p != 0 && words > 0 {
            let mut raw = vec![0u8; words * 8];
            t.mm().copy_from_user(&mut raw[..bytes], p as usize).map_err(fault)?;
            for (d, c) in v.iter_mut().zip(raw.chunks_exact(8)) {
                *d = u64::from_le_bytes(c.try_into().unwrap());
            }
        }
        Ok(v)
    };
    let (r, w, e) = (read_set(rset)?, read_set(wset)?, read_set(eset)?);
    let bit = |v: &[u64], fd: usize| v[fd / 64] & (1 << (fd % 64)) != 0;
    let mut entries = Vec::new();
    for fd in 0..nfds as usize {
        let mut ev = 0;
        if bit(&r, fd) { ev |= POLLIN; }
        if bit(&w, fd) { ev |= POLLOUT; }
        if bit(&e, fd) { ev |= POLLPRI; }
        if ev != 0 {
            entries.push((fd as i32, ev));
        }
    }
    let rev = wait_ready(t, &entries, timeout)?;
    let (mut ro, mut wo, mut eo) = (vec![0u64; words], vec![0u64; words], vec![0u64; words]);
    let mut count = 0;
    for (&(fd, ev), &rv) in entries.iter().zip(rev.iter()) {
        if rv & POLLNVAL != 0 {
            return Err(EBADF);
        }
        let fd = fd as usize;
        let mark = |v: &mut Vec<u64>| v[fd / 64] |= 1 << (fd % 64);
        if ev & POLLIN != 0 && rv & (POLLIN | POLLHUP | POLLERR) != 0 {
            mark(&mut ro);
            count += 1;
        }
        if ev & POLLOUT != 0 && rv & (POLLOUT | POLLERR) != 0 {
            mark(&mut wo);
            count += 1;
        }
        if ev & POLLPRI != 0 && rv & POLLPRI != 0 {
            mark(&mut eo);
            count += 1;
        }
    }
    for (p, v) in [(rset, &ro), (wset, &wo), (eset, &eo)] {
        if p != 0 && words > 0 {
            let raw: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
            t.mm().copy_to_user(p as usize, &raw[..bytes]).map_err(fault)?;
        }
    }
    Ok(count)
}

pub fn select(t: &Thread, nfds: u64, r: u64, w: u64, e: u64, tv: u64) -> SysResult {
    let timeout = if tv == 0 {
        None
    } else {
        let v = super::compat::get_timespec(t, tv)?;
        if v[0] < 0 || !(0..1_000_000).contains(&v[1]) {
            return Err(EINVAL);
        }
        Some(core::time::Duration::new(v[0] as u64, v[1] as u32 * 1000))
    };
    let start = time::Instant::now();
    let res = do_select(t, nfds, r, w, e, timeout);
    if let Some(d) = timeout {
        // Linux écrit le temps restant.
        let left = d.saturating_sub(time::Instant::now().duration_since(start));
        let _ = super::compat::put_timespec(t, tv, left.as_secs() as i64, left.subsec_micros() as i64);
    }
    res
}

pub fn pselect6(t: &Thread, nfds: u64, r: u64, w: u64, e: u64, ts: u64, sig: u64) -> SysResult {
    let timeout = read_ts(t, ts)?;
    let mask_ptr = if sig != 0 {
        // { const sigset_t *ss; size_t ss_len } : deux mots de 4 octets en i386.
        let v = if super::compat::active(t) {
            let w = t.mm().read_user::<[u32; 2]>(sig as usize).map_err(fault)?;
            [w[0] as u64, w[1] as u64]
        } else {
            t.mm().read_user::<[u64; 2]>(sig as usize).map_err(fault)?
        };
        if v[0] != 0 && v[1] != 8 {
            return Err(EINVAL);
        }
        v[0]
    } else {
        0
    };
    with_temp_mask(t, mask_ptr, || do_select(t, nfds, r, w, e, timeout))
}

/// `pipe2(fds, flags)`.
pub fn pipe2(t: &Thread, fds: u64, flags: u64) -> SysResult {
    const O_DIRECT: u64 = 0x4000;
    if flags & !(O_CLOEXEC | O_NONBLOCK as u64 | O_DIRECT) != 0 {
        return Err(EINVAL);
    }
    let (r, w) = crate::pipe::new_pipe();
    let status = (flags as u32) & O_NONBLOCK;
    let rf = OpenFile::new(Node::Pipe(r), status);
    let wf = OpenFile::new(Node::Pipe(w), status | O_WRONLY);
    let cloexec = flags & O_CLOEXEC != 0;
    let (a, b) = {
        let mut table = t.process.files.lock();
        let a = table.alloc(rf, 0, cloexec).ok_or(EMFILE)?;
        match table.alloc(wf, 0, cloexec) {
            Some(b) => (a, b),
            None => {
                table.close(a);
                return Err(EMFILE);
            }
        }
    };
    if let Err(e) = t.mm().write_user(fds as usize, &[a as i32, b as i32]) {
        let mut table = t.process.files.lock();
        table.close(a);
        table.close(b);
        return Err(fault(e));
    }
    Ok(0)
}

// --- Répertoires ----------------------------------------------------------------------------

pub fn getdents64(t: &Thread, fd: u64, dirp: u64, count: u64) -> SysResult {
    getdents_as(t, fd, dirp, count, false)
}

/// Ancien `getdents` d'i386 (appel 141) : `struct compat_linux_dirent` { `d_ino` et `d_off` de
/// 32 bits, `d_reclen`, nom, zéro, `d_type` dans le dernier octet }, enregistrements alignés
/// sur 4 ; un inode qui ne tient pas en 32 bits → `EOVERFLOW` (comme `compat_filldir`).
pub fn getdents32(t: &Thread, fd: u64, dirp: u64, count: u64) -> SysResult {
    getdents_as(t, fd, dirp, count, true)
}

fn getdents_as(t: &Thread, fd: u64, dirp: u64, count: u64, old: bool) -> SysResult {
    const DT_DIR: u8 = 4;
    const DT_REG: u8 = 8;
    let f = get_fd(t, fd)?;
    let mut names: Vec<(String, u8, Option<u64>)> = vec![(String::from("."), DT_DIR, None), (String::from(".."), DT_DIR, None)];
    let path = match &f.node {
        Node::Dir { dir, path } => {
            // Le système de fichiers donne inode et type sans ouvrir les enfants (ext4) ; sinon,
            // noms d'abord, verrou relâché avant de prendre chaque enfant (règle du VFS).
            let entries = dir.lock().entries();
            match entries {
                Some(list) => {
                    for e in list {
                        let ty = match e.kind {
                            Some(NodeKind::Dir) => DT_DIR,
                            Some(NodeKind::Symlink) => 10,
                            Some(NodeKind::CharDev) => 2,
                            Some(NodeKind::BlockDev) => 6,
                            Some(NodeKind::Fifo) => 1,
                            Some(NodeKind::Socket) => 12,
                            Some(NodeKind::File) => DT_REG,
                            None => 0, // DT_UNKNOWN : l'appelant fera un `stat`.
                        };
                        names.push((e.name, ty, Some(e.ino)));
                    }
                }
                None => {
                    let list = dir.lock().list();
                    for name in list {
                        let child = dir.lock().get(&name);
                        let ty = match child {
                            Some(FileOrDir::Dir(_)) => DT_DIR,
                            Some(c) if c.read_link().is_some() => 10,
                            _ => DT_REG,
                        };
                        names.push((name, ty, None));
                    }
                }
            }
            path
        }
        Node::VirtDir { path, entries } => {
            names.extend(entries.iter().map(|(n, t)| (n.clone(), *t, None)));
            path
        }
        _ => return Err(ENOTDIR),
    };
    let mut pos = f.pos.lock();
    let mut out: Vec<u8> = Vec::new();
    let mut i = *pos;
    while i < names.len() {
        let (name, ty, real_ino) = &names[i];
        let raw = fs_node::rawname::encode(name);
        let reclen = if old { (10 + raw.len() + 2 + 3) & !3 } else { (19 + raw.len() + 1 + 7) & !7 };
        if out.len() + reclen > count as usize {
            if out.is_empty() {
                return Err(EINVAL);
            }
            break;
        }
        let ino = real_ino.unwrap_or_else(|| ino_of(&format!("{}/{}", path, name)));
        let start = out.len();
        if old {
            if ino > u32::MAX as u64 {
                if out.is_empty() {
                    return Err(EOVERFLOW);
                }
                break;
            }
            out.extend_from_slice(&(ino as u32).to_le_bytes());
            out.extend_from_slice(&((i + 1) as u32).to_le_bytes());
            out.extend_from_slice(&(reclen as u16).to_le_bytes());
            out.extend_from_slice(&raw);
            out.resize(start + reclen, 0);
            out[start + reclen - 1] = *ty;
        } else {
            out.extend_from_slice(&ino.to_le_bytes());
            out.extend_from_slice(&((i + 1) as i64).to_le_bytes());
            out.extend_from_slice(&(reclen as u16).to_le_bytes());
            out.push(*ty);
            out.extend_from_slice(&raw);
            out.resize(start + reclen, 0);
        }
        i += 1;
    }
    t.mm().copy_to_user(dirp as usize, &out).map_err(fault)?;
    *pos = i;
    if !out.is_empty() {
        crate::inotify::open_file_event(&f, crate::inotify::IN_ACCESS);
    }
    Ok(out.len() as i64)
}

pub fn getcwd(t: &Thread, buf: u64, size: u64) -> SysResult {
    let path = t.process.cwd_path();
    let mut bytes = fs_node::rawname::encode(&path);
    bytes.push(0);
    if (size as usize) < bytes.len() {
        return Err(ERANGE);
    }
    t.mm().copy_to_user(buf as usize, &bytes).map_err(fault)?;
    Ok(bytes.len() as i64)
}

/// Nouveau dossier courant : position (montage, nœud) du chemin, droit de traverser.
fn set_cwd(t: &Thread, abs: &str) -> SysResult {
    let f = crate::mount::lookup(t, abs, true)?;
    match &f.target {
        crate::mount::Target::Node(FileOrDir::File(_)) => return Err(ENOTDIR),
        crate::mount::Target::Node(n) => check_access(t, n, crate::creds::MAY_EXEC)?,
        crate::mount::Target::Virtual(..) => {}
    }
    let loc = f.loc().ok_or(ENOTDIR)?;
    t.process.fs.with(|fs| fs.cwd = loc);
    Ok(0)
}

pub fn chdir(t: &Thread, path: u64) -> SysResult {
    let abs = resolved(t, AT_FDCWD, &user_path(t, path)?)?;
    set_cwd(t, &abs)
}

pub fn fchdir(t: &Thread, fd: u64) -> SysResult {
    let f = get_fd(t, fd)?;
    match &f.node {
        Node::Dir { path, .. } | Node::VirtDir { path, .. } => set_cwd(t, &path.clone()),
        _ => Err(ENOTDIR),
    }
}

/// Contrôle d'accès discrétionnaire (`crate::creds`) sur un nœud qui a des métadonnées.
pub(crate) fn check_access(t: &Thread, node: &FileOrDir, want: u32) -> Result<(), i64> {
    match node.metadata() {
        Some(m) if !t.process.creds.lock().may_access(&m, want, false) => Err(EACCES),
        _ => Ok(()),
    }
}

/// Un nœud créé appartient aux identités de système de fichiers du créateur.
fn give_to_caller(t: &Thread, mut node: FileOrDir) -> FileOrDir {
    let (uid, gid) = {
        let c = t.process.creds.lock();
        (c.fsuid, c.fsgid)
    };
    if uid != 0 || gid != 0 {
        let _ = node.set_metadata(MetaChange::Owner { uid: Some(uid), gid: Some(gid) });
    }
    node
}

/// Droits d'un nœud créé : `mode` moins le `umask` du processus.
fn creation_perm(t: &Thread, mode: u64) -> u16 {
    (mode as u32 & 0o7777 & !t.process.umask.load(Ordering::Relaxed)) as u16
}

fn create_error(e: &str) -> i64 {
    if e.contains("existe") {
        EEXIST
    } else if e.contains("nom invalide") {
        EINVAL
    } else {
        no_space("creation", &e)
    }
}

/// `ENOSPC` traduit d'une erreur du système de fichiers : la cause d'origine est journalisée,
/// car `ENOSPC` sert aussi de repli pour des erreurs sans équivalent POSIX (rare).
fn no_space(what: &str, e: &dyn core::fmt::Debug) -> i64 {
    log::warn!("maiproc : {} : {:?} -> ENOSPC", what, e);
    ENOSPC
}

/// Crée un fichier vide dans `parent` : du type de son système de fichiers (tmpfs), ou un
/// fichier en mémoire si le dossier n'a pas de fabrique.
pub(crate) fn create_file_in(parent: &DirRef, name: &str, perm: u16) -> Result<FileRef, i64> {
    if parent.lock().read_only() {
        return Err(EROFS);
    }
    let made = parent.lock().create_file(name, perm);
    match made {
        Some(r) => r.map_err(create_error),
        None => memfs::MemFile::create(String::from(name), parent).map_err(|e| no_space("creation memfs", &e)),
    }
}

pub fn mkdirat(t: &Thread, dirfd: i64, path: u64, mode: u64) -> SysResult {
    let abs = absolute(t, dirfd, &user_path(t, path)?)?;
    if walk(t, &abs, false).is_ok() || special(t, &abs).is_some() {
        return Err(EEXIST);
    }
    let (parent, name) = split_parent(t, &abs)?;
    if parent.lock().read_only() {
        return Err(EROFS);
    }
    check_access(t, &FileOrDir::Dir(parent.clone()), crate::creds::MAY_WRITE | crate::creds::MAY_EXEC)?;
    let made = parent.lock().create_dir(&name, creation_perm(t, mode));
    let r = match made {
        Some(r) => r.map(|d| {
            give_to_caller(t, FileOrDir::Dir(d));
            0
        }).map_err(create_error),
        None => vfs_node::VFSDirectory::create(name, &parent).map(|_| 0).map_err(|_| EROFS),
    };
    if r.is_ok() {
        crate::inotify::created(&abs, true);
    }
    r
}

/// `symlinkat(target, dirfd, linkpath)` : la cible est gardée telle quelle, jamais vérifiée.
pub fn symlinkat(t: &Thread, target: u64, dirfd: i64, linkpath: u64) -> SysResult {
    let target = user_path(t, target)?;
    if target.is_empty() {
        return Err(ENOENT);
    }
    let abs = absolute(t, dirfd, &user_path(t, linkpath)?)?;
    if walk(t, &abs, false).is_ok() || special(t, &abs).is_some() {
        return Err(EEXIST);
    }
    let (parent, name) = split_parent(t, &abs)?;
    if parent.lock().read_only() {
        return Err(EROFS);
    }
    check_access(t, &FileOrDir::Dir(parent.clone()), crate::creds::MAY_WRITE | crate::creds::MAY_EXEC)?;
    let made = parent.lock().create_symlink(&name, &target);
    let r = match made {
        Some(r) => r.map(|f| {
            give_to_caller(t, FileOrDir::File(f));
            0
        }).map_err(create_error),
        None => Err(EPERM), // système de fichiers sans liens symboliques
    };
    if r.is_ok() {
        crate::inotify::created(&abs, false);
    }
    r
}

/// `linkat(olddirfd, old, newdirfd, new, flags)` : nouveau nom du même inode, dans le même
/// volume (`EXDEV` sinon) ; pas de lien dur sur un dossier (`EPERM`).
pub fn linkat(t: &Thread, olddirfd: i64, old: u64, newdirfd: i64, new: u64, flags: u64) -> SysResult {
    const AT_SYMLINK_FOLLOW: u64 = 0x400;
    if flags & !(AT_SYMLINK_FOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(EINVAL);
    }
    let old_raw = user_path(t, old)?;
    let node = if old_raw.is_empty() && flags & AT_EMPTY_PATH != 0 {
        match &get_fd(t, olddirfd as u64)?.node {
            Node::File { file, .. } => FileOrDir::File(file.clone()),
            Node::Dir { .. } => return Err(EPERM),
            _ => return Err(ENOENT),
        }
    } else {
        walk(t, &absolute(t, olddirfd, &old_raw)?, flags & AT_SYMLINK_FOLLOW != 0)?
    };
    let FileOrDir::File(file) = node else { return Err(EPERM) };
    let abs = absolute(t, newdirfd, &user_path(t, new)?)?;
    if walk(t, &abs, false).is_ok() || special(t, &abs).is_some() {
        return Err(EEXIST);
    }
    let (parent, name) = split_parent(t, &abs)?;
    if parent.lock().read_only() || file.lock().read_only() {
        return Err(EROFS);
    }
    let dev_file = file.lock().metadata().map(|m| m.dev);
    let dev_dir = parent.lock().metadata().map(|m| m.dev);
    if dev_file.is_none() || dev_file != dev_dir {
        return Err(EXDEV);
    }
    let link = file.lock().new_link(name, Arc::downgrade(&parent)).ok_or(EPERM)?;
    let inserted = parent.lock().insert(FileOrDir::File(link));
    inserted.map_err(|e| no_space("lien", &e))?;
    if crate::inotify::active() {
        // Nombre de liens changé, puis nouvelle entrée.
        crate::inotify::file_event(&FileOrDir::File(file), &abs, crate::inotify::IN_ATTRIB);
        crate::inotify::created(&abs, false);
    }
    Ok(0)
}

pub fn unlinkat(t: &Thread, dirfd: i64, path: u64, flags: u64) -> SysResult {
    let abs = absolute(t, dirfd, &user_path(t, path)?)?;
    let node = walk(t, &abs, false)?;
    if node.read_only() {
        return Err(EROFS);
    }
    match (&node, flags & AT_REMOVEDIR != 0) {
        (FileOrDir::Dir(d), true) => {
            if !d.lock().list().is_empty() {
                return Err(ENOTEMPTY);
            }
        }
        (FileOrDir::Dir(_), false) => return Err(EISDIR),
        (FileOrDir::File(_), true) => return Err(ENOTDIR),
        (FileOrDir::File(_), false) => {}
    }
    let (parent, _) = split_parent(t, &abs)?;
    let key = crate::inotify::active().then(|| inode_key(&node, &abs));
    let removed = parent.lock().remove(&node);
    removed.ok_or(EBUSY)?;
    if let Some(key) = key {
        let isdir = matches!(node, FileOrDir::Dir(_));
        let linked = !isdir && node.metadata().map_or(false, |m| m.nlink > 0);
        crate::inotify::deleted(&abs, key, isdir, linked);
    }
    Ok(0)
}

pub fn renameat(t: &Thread, olddirfd: i64, old: u64, newdirfd: i64, new: u64) -> SysResult {
    renameat2(t, olddirfd, old, newdirfd, new, 0)
}

/// `renameat2(…, flags)` : `RENAME_NOREPLACE` (`EEXIST` si la cible existe) ; l'échange
/// (`RENAME_EXCHANGE`) et `RENAME_WHITEOUT` ne sont pas pris en charge (`EINVAL`, ce que Linux
/// rend aussi pour un système de fichiers qui ne les connaît pas).
pub fn renameat2(t: &Thread, olddirfd: i64, old: u64, newdirfd: i64, new: u64, flags: u64) -> SysResult {
    const RENAME_NOREPLACE: u64 = 1;
    if flags & !RENAME_NOREPLACE != 0 {
        return Err(EINVAL);
    }
    let old_abs = absolute(t, olddirfd, &user_path(t, old)?)?;
    let new_abs = absolute(t, newdirfd, &user_path(t, new)?)?;
    if flags & RENAME_NOREPLACE != 0 && lookup_nofollow(t, &new_abs).is_some() {
        return Err(EEXIST);
    }
    if old_abs == new_abs {
        return Ok(0);
    }
    let node = lookup(t, &old_abs).ok_or(ENOENT)?;
    let watch = crate::inotify::active().then(|| {
        let victim = lookup(t, &new_abs).map(|v| (inode_key(&v, &new_abs), matches!(v, FileOrDir::Dir(_)), v));
        (inode_key(&node, &old_abs), victim)
    });
    let r = rename_node(t, node.clone(), &old_abs, &new_abs);
    if let (Ok(_), Some((key, victim))) = (&r, watch) {
        let victim = victim.map(|(k, d, v)| (k, d, !d && v.metadata().map_or(false, |m| m.nlink > 0)));
        crate::inotify::moved(&old_abs, &new_abs, key, matches!(node, FileOrDir::Dir(_)), victim);
    }
    r
}

/// Déplacement de `node` de `old_abs` vers `new_abs` (règles de rename(2)).
fn rename_node(t: &Thread, node: FileOrDir, old_abs: &str, new_abs: &str) -> SysResult {
    let (old_parent, _) = split_parent(t, &old_abs)?;
    let (new_parent, new_name) = split_parent(t, &new_abs)?;
    // Règles de remplacement de rename(2).
    if let Some(existing) = lookup(t, &new_abs) {
        match (&node, &existing) {
            (FileOrDir::File(_), FileOrDir::Dir(_)) => return Err(EISDIR),
            (FileOrDir::Dir(_), FileOrDir::File(_)) => return Err(ENOTDIR),
            (FileOrDir::Dir(_), FileOrDir::Dir(d)) if !d.lock().list().is_empty() => return Err(ENOTEMPTY),
            _ => {}
        }
        if let (FileOrDir::Dir(a), FileOrDir::Dir(b)) = (&node, &existing) {
            if fs_node::same_dir(a, b) {
                return Ok(0);
            }
        }
    }
    if let FileOrDir::Dir(d) = &node {
        if fs_node::is_within(&new_parent, d) {
            return Err(EINVAL); // un dossier dans lui-même
        }
    }
    // Déplacement du nœud lui-même, sans copie (le nœud remplacé, s'il y en a un, est rendu).
    match fs_node::move_node(&node, &old_parent, &new_parent, &new_name) {
        Ok(_) => return Ok(0),
        Err(_) if matches!(node, FileOrDir::Dir(_)) => return Err(EXDEV),
        Err(_) => {}
    }
    // Système de fichiers sans renommage : copie du contenu dans un nouveau fichier, puis retrait.
    let FileOrDir::File(src) = &node else { return Err(EXDEV) };
    if let Some(existing) = lookup(t, &new_abs) {
        new_parent.lock().remove(&existing);
    }
    let data = {
        let mut l = src.lock();
        let mut d = vec![0u8; l.len()];
        let n = if d.is_empty() { 0 } else { l.read_at(&mut d, 0).map_err(|_| EIO)? };
        d.truncate(n);
        d
    };
    let perm = src.lock().metadata().map_or(0o644, |m| m.perm);
    let dst = create_file_in(&new_parent, &new_name, perm)?;
    if !data.is_empty() {
        dst.lock().write_at(&data, 0).map_err(|e| no_space("copie de renommage", &e))?;
    }
    old_parent.lock().remove(&node);
    Ok(0)
}

pub fn readlinkat(t: &Thread, dirfd: i64, path: u64, buf: u64, size: u64) -> SysResult {
    let raw = user_path(t, path)?;
    let abs = absolute(t, dirfd, &raw)?;
    // Le dernier composant n'est jamais suivi : un lien dont la cible est dans `/proc` ou
    // devpts (`/dev/fd`, `/dev/ptmx` de bubblewrap) reste un lien.
    let target = if let Some(l) = proc_link(t, &abs) {
        l
    } else {
        match lookup_any(t, &abs, false) {
            Ok(Lookup::Vfs(f)) => match f.target {
                crate::mount::Target::Node(n) => n.read_link(),
                crate::mount::Target::Virtual(..) => None,
            },
            Ok(_) => None,
            Err(ENOENT) if abs == "/etc/mtab" => None,
            Err(e) => return Err(e),
        }
    };
    let Some(target) = target else { return Err(EINVAL) };
    let target = fs_node::rawname::encode(&target);
    let n = target.len().min(size as usize);
    t.mm().copy_to_user(buf as usize, &target[..n]).map_err(fault)?;
    Ok(n as i64)
}

/// `access` : tout processus est root (uid 0), qui peut tout lire et écrire ; il ne peut
/// pourtant exécuter qu'un fichier qui a au moins un bit `x`, et rien écrire sur un volume en
/// lecture seule. `bash` et `execvp` cherchent ainsi les programmes dans `PATH` comme sous Linux.
pub fn faccessat(t: &Thread, dirfd: i64, path: u64, mode: u64, flags: u64) -> SysResult {
    const W_OK: u64 = 2;
    const AT_EACCESS: u64 = 0x200;
    if mode & !7 != 0 || flags & !(AT_EACCESS | AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(EINVAL);
    }
    let abs = resolved(t, dirfd, &user_path(t, path)?)?;
    if special(t, &abs).is_some() {
        return Ok(0);
    }
    let (node, mnt) = walk_mnt(t, &abs, flags & AT_SYMLINK_NOFOLLOW == 0)?;
    if mode & W_OK != 0 && (node.read_only() || mnt.readonly()) {
        return Err(EROFS);
    }
    // `access` juge avec les identités réelles (le cas d'un programme set-user-ID qui vérifie
    // ce que son appelant a le droit de faire) ; `AT_EACCESS`, avec les effectives.
    if let Some(m) = node.metadata() {
        if !t.process.creds.lock().may_access(&m, mode as u32, flags & AT_EACCESS == 0) {
            return Err(EACCES);
        }
    }
    Ok(0)
}

/// Changement demandé, tel que les arguments de l'appel le décrivent.
#[derive(Clone, Copy)]
pub enum MetaArg {
    /// `chmod` : mode.
    Mode(u64),
    /// `chown` : uid, gid (`-1` : inchangé).
    Owner(u64, u64),
    /// `utimensat` : adresse de `timespec[2]` (0 : maintenant).
    Utimens(u64),
    /// `utime` : adresse d'`utimbuf` (0 : maintenant).
    Utime(u64),
    /// `utimes`/`futimesat` : adresse de `timeval[2]` (0 : maintenant).
    Utimes(u64),
}

fn decode_meta(t: &Thread, arg: MetaArg) -> Result<Option<MetaChange>, i64> {
    const UTIME_NOW: i64 = (1 << 30) - 1;
    const UTIME_OMIT: i64 = (1 << 30) - 2;
    // Mots `time_t`/`long` : 4 octets pour un appel i386 « time32 » (`utime`, `utimes`,
    // `utimensat`, `futimesat`).
    let narrow = super::compat::time32(t);
    let words = |ptr: u64, n: usize| -> Result<[i64; 4], i64> {
        let mut raw = [0u8; 32];
        if narrow {
            t.mm().copy_from_user(&mut raw[..n * 4], ptr as usize).map_err(fault)?;
            let w = |k: usize| i32::from_le_bytes(raw[k * 4..k * 4 + 4].try_into().unwrap()) as i64;
            return Ok([w(0), w(1), w(2), w(3)]);
        }
        t.mm().copy_from_user(&mut raw[..n * 8], ptr as usize).map_err(fault)?;
        let w = |k: usize| i64::from_le_bytes(raw[k * 8..k * 8 + 8].try_into().unwrap());
        Ok([w(0), w(1), w(2), w(3)])
    };
    let at = |sec: i64, nsec: i64| -> Result<TimeSet, i64> {
        if !(0..1_000_000_000).contains(&nsec) {
            return Err(EINVAL);
        }
        Ok(TimeSet::At(Timespec { sec, nsec: nsec as u32 }))
    };
    let now = MetaChange::Times { atime: TimeSet::Now, mtime: TimeSet::Now };
    Ok(Some(match arg {
        MetaArg::Mode(m) => MetaChange::Perm(m as u16 & 0o7777),
        MetaArg::Owner(u, g) => {
            let ns = t.process.creds.lock().user_ns.clone();
            let uid = if u as u32 == u32::MAX { None } else { Some(ns.make_kuid(u as u32).ok_or(EINVAL)?) };
            let gid = if g as u32 == u32::MAX { None } else { Some(ns.make_kgid(g as u32).ok_or(EINVAL)?) };
            MetaChange::Owner { uid, gid }
        }
        MetaArg::Utimens(0) | MetaArg::Utime(0) | MetaArg::Utimes(0) => now,
        MetaArg::Utimens(p) => {
            let v = words(p, 4)?;
            let one = |sec: i64, nsec: i64| match nsec {
                UTIME_NOW => Ok(TimeSet::Now),
                UTIME_OMIT => Ok(TimeSet::Omit),
                _ => at(sec, nsec),
            };
            let (atime, mtime) = (one(v[0], v[1])?, one(v[2], v[3])?);
            if atime == TimeSet::Omit && mtime == TimeSet::Omit {
                return Ok(None);
            }
            MetaChange::Times { atime, mtime }
        }
        MetaArg::Utime(p) => {
            let v = words(p, 2)?;
            MetaChange::Times { atime: at(v[0], 0)?, mtime: at(v[1], 0)? }
        }
        MetaArg::Utimes(p) => {
            let v = words(p, 4)?;
            if !(0..1_000_000).contains(&v[1]) || !(0..1_000_000).contains(&v[3]) {
                return Err(EINVAL);
            }
            MetaChange::Times { atime: at(v[0], v[1] * 1000)?, mtime: at(v[2], v[3] * 1000)? }
        }
    }))
}

/// Droits d'un changement d'attributs (`notify_change`, `chown_ok`, `chgrp_ok`,
/// `inode_owner_or_capable`) ; `chmod` par un non-membre du groupe sans `CAP_FSETID` perd
/// `S_ISGID`.
fn check_meta(t: &Thread, node: &FileOrDir, change: Option<MetaChange>) -> Result<Option<MetaChange>, i64> {
    use crate::userns::{CAP_CHOWN, CAP_FOWNER, CAP_FSETID};
    let Some(change) = change else { return Ok(None) };
    let Some(m) = node.metadata() else { return Ok(Some(change)) };
    let c = t.process.creds.lock().clone();
    let owner = c.fsuid == m.uid || c.inode_capable(&m, CAP_FOWNER);
    match change {
        MetaChange::Perm(mut mode) => {
            if !owner {
                return Err(EPERM);
            }
            if mode & 0o2000 != 0 && !c.in_group(m.gid) && !c.inode_capable(&m, CAP_FSETID) {
                mode &= !0o2000;
            }
            Ok(Some(MetaChange::Perm(mode)))
        }
        MetaChange::Owner { uid, gid } => {
            let uid_ok = match uid {
                None => true,
                Some(u) => (c.fsuid == m.uid && u == m.uid) || c.inode_capable(&m, CAP_CHOWN),
            };
            let gid_ok = match gid {
                None => true,
                Some(g) => (c.fsuid == m.uid && (g == m.gid || c.in_group(g))) || c.inode_capable(&m, CAP_CHOWN),
            };
            if uid_ok && gid_ok {
                Ok(Some(change))
            } else {
                Err(EPERM)
            }
        }
        MetaChange::Times { atime, mtime } => {
            // « Maintenant » : propriétaire ou droit d'écrire ; une date choisie : propriétaire.
            let now_only = matches!(atime, TimeSet::Now | TimeSet::Omit) && matches!(mtime, TimeSet::Now | TimeSet::Omit);
            if owner || (now_only && c.may_access(&m, crate::creds::MAY_WRITE, false)) {
                Ok(Some(change))
            } else {
                Err(if now_only { EACCES } else { EPERM })
            }
        }
    }
}

/// Applique un changement : `EROFS` sur un volume en lecture seule ; un système de fichiers
/// qui ne conserve pas ces informations (VFS en mémoire) l'accepte sans effet.
fn apply_meta(node: &mut FileOrDir, change: Option<MetaChange>) -> SysResult {
    if node.read_only() {
        return Err(EROFS);
    }
    let Some(change) = change else { return Ok(0) };
    if let (FileOrDir::File(file), MetaChange::Perm(new)) = (&*node, change) {
        let old = fs_node::FsNode::metadata(&*file.lock()).map_or(0, |m| m.perm);
        crate::seals::check_chmod(file, old as u32, new as u32)?;
    }
    match node.set_metadata(change) {
        Ok(()) => Ok(0),
        Err(e) if e == fs_node::NOT_SUPPORTED => Ok(0),
        Err(_) => Err(EPERM),
    }
}

/// `chmod`, `chown`, `utimensat`… sur un chemin ; un chemin absent rend `ENOENT` (`touch`
/// s'en sert pour savoir si sa création a échoué).
pub fn meta_change_path(t: &Thread, dirfd: i64, path: u64, flags: u64, arg: MetaArg) -> SysResult {
    let change = decode_meta(t, arg)?;
    let p = user_path(t, path)?;
    if p.is_empty() {
        if flags & AT_EMPTY_PATH != 0 && dirfd != AT_FDCWD {
            return meta_change_fd(t, dirfd as u64, arg);
        }
        return Err(ENOENT);
    }
    let abs = resolved(t, dirfd, &p)?;
    if special(t, &abs).is_some() {
        return Ok(0);
    }
    let (mut node, mnt) = walk_mnt(t, &abs, flags & AT_SYMLINK_NOFOLLOW == 0)?;
    if mnt.readonly() {
        return Err(EROFS);
    }
    let change = check_meta(t, &node, change)?;
    let r = apply_meta(&mut node, change)?;
    crate::inotify::file_event(&node, &abs, crate::inotify::IN_ATTRIB);
    Ok(r)
}

/// Même chose sur un descripteur (`fchmod`, `fchown`, `futimens`).
pub fn meta_change_fd(t: &Thread, fd: u64, arg: MetaArg) -> SysResult {
    let change = decode_meta(t, arg)?;
    let f = get_fd(t, fd)?;
    let mut node = match &f.node {
        Node::File { file, .. } => FileOrDir::File(file.clone()),
        Node::Dir { dir, .. } => FileOrDir::Dir(dir.clone()),
        _ => return Ok(0),
    };
    let change = check_meta(t, &node, change)?;
    let r = apply_meta(&mut node, change)?;
    crate::inotify::open_file_event(&f, crate::inotify::IN_ATTRIB);
    Ok(r)
}

// --- Verrous, copies entre descripteurs, fichiers anonymes ------------------------------------

/// `flock(fd, op)` : voir `crate::flock`.
pub fn flock(t: &Thread, fd: u64, op: u64) -> SysResult {
    let f = get_fd(t, fd)?;
    crate::flock::flock(t, &f, op)
}

/// `preadv2`/`pwritev2` : comme `preadv`/`pwritev`, position `-1` = position courante ;
/// `RWF_NOWAIT` n'attend pas (`EAGAIN`), `RWF_APPEND` écrit à la fin. `RWF_HIPRI`, `RWF_DSYNC`
/// et `RWF_SYNC` sont acceptés (tout est déjà synchrone en mémoire).
pub fn rw_iov2(t: &Thread, fd: u64, iov: u64, cnt: u64, pos: i64, flags: u64, write: bool) -> SysResult {
    const RWF_HIPRI: u64 = 1;
    const RWF_DSYNC: u64 = 2;
    const RWF_SYNC: u64 = 4;
    const RWF_NOWAIT: u64 = 8;
    const RWF_APPEND: u64 = 0x10;
    if flags & !(RWF_HIPRI | RWF_DSYNC | RWF_SYNC | RWF_NOWAIT | RWF_APPEND) != 0 || (flags & RWF_APPEND != 0 && !write) {
        return Err(EOPNOTSUPP);
    }
    if pos < -1 {
        return Err(EINVAL);
    }
    let f = get_fd(t, fd)?;
    let mut at = if pos == -1 { None } else { Some(pos as usize) };
    if flags & RWF_APPEND != 0 {
        if let Node::File { file, .. } = &f.node {
            at = Some(file.lock().len());
        }
    }
    rw_iov(t, &f, write, &iovecs(t, iov, cnt)?, at, flags & RWF_NOWAIT != 0)
}

/// Copie au plus `count` octets de `inp` vers `out` par tranches de 64 Kio. Seule la première
/// lecture peut attendre (un tube vide) : ensuite, on rend ce qui a été copié plutôt que de
/// bloquer. Renvoie les octets copiés et les positions finales.
///
/// `greedy` (`sendfile`) : les morceaux suivants attendent comme le premier (une sortie bloquante
/// reçoit tout) ; sinon seul le premier peut attendre. Sans position explicite, une entrée
/// positionnable est lue à sa position courante, qui n'avance à la fin que des octets écrits
/// (aucun octet lu puis perdu quand l'écriture est partielle).
#[allow(clippy::too_many_arguments)]
fn copy_fds(
    t: &Thread,
    inp: &OpenFile,
    mut in_pos: Option<usize>,
    out: &OpenFile,
    mut out_pos: Option<usize>,
    count: usize,
    nonblock: bool,
    greedy: bool,
) -> Result<(usize, Option<usize>, Option<usize>), i64> {
    let implicit_in = in_pos.is_none() && matches!(inp.node, Node::File { .. } | Node::Generated { .. });
    if implicit_in {
        in_pos = Some(*inp.pos.lock());
    }
    let mut kb = vec![0u8; count.min(CHUNK).max(1)];
    let mut done = 0usize;
    let r = loop {
        if done >= count {
            break Ok(());
        }
        let wait_free = nonblock || (done > 0 && !greedy);
        let want = (count - done).min(kb.len());
        let n = match kread(t, inp, &mut kb[..want], in_pos, wait_free) {
            Ok(n) => n,
            Err(e) if done == 0 => break Err(e),
            Err(_) => break Ok(()),
        };
        if n == 0 {
            break Ok(());
        }
        let w = match kwrite(t, out, &kb[..n], out_pos, wait_free) {
            Ok(w) => w,
            Err(e) if done == 0 => break Err(e),
            Err(_) => break Ok(()),
        };
        done += w;
        if let Some(p) = in_pos.as_mut() {
            *p += w;
        }
        if let Some(p) = out_pos.as_mut() {
            *p += w;
        }
        if w < n || n < want {
            break Ok(());
        }
    };
    if implicit_in {
        if let Some(p) = in_pos.take() {
            *inp.pos.lock() = p;
        }
    }
    r.map(|()| (done, in_pos, out_pos))
}

fn read_off(t: &Thread, ptr: u64) -> Result<Option<usize>, i64> {
    if ptr == 0 {
        return Ok(None);
    }
    let v = t.mm().read_user::<i64>(ptr as usize).map_err(fault)?;
    if v < 0 {
        return Err(EINVAL);
    }
    Ok(Some(v as usize))
}

fn write_off(t: &Thread, ptr: u64, v: Option<usize>) -> Result<(), i64> {
    if let (true, Some(v)) = (ptr != 0, v) {
        t.mm().write_user::<i64>(ptr as usize, &(v as i64)).map_err(fault)?;
    }
    Ok(())
}

/// `copy_file_range(fd_in, off_in, fd_out, off_out, len, flags)` entre fichiers réguliers.
pub fn copy_file_range(t: &Thread, fd_in: u64, off_in: u64, fd_out: u64, off_out: u64, len: u64, flags: u64) -> SysResult {
    if flags != 0 {
        return Err(EINVAL);
    }
    let inp = get_fd(t, fd_in)?;
    let out = get_fd(t, fd_out)?;
    if !inp.readable() || !out.writable() || out.flags() & O_APPEND != 0 {
        return Err(EBADF);
    }
    let (Node::File { file: a, .. }, Node::File { file: b, .. }) = (&inp.node, &out.node) else {
        return Err(if matches!(inp.node, Node::Dir { .. }) || matches!(out.node, Node::Dir { .. }) { EISDIR } else { EINVAL });
    };
    let in_pos = read_off(t, off_in)?;
    let out_pos = read_off(t, off_out)?;
    // Même fichier : les deux plages ne doivent pas se chevaucher.
    if Arc::ptr_eq(a, b) {
        let s = in_pos.unwrap_or(*inp.pos.lock());
        let d = out_pos.unwrap_or(*out.pos.lock());
        let n = len as usize;
        if s < d.saturating_add(n) && d < s.saturating_add(n) {
            return Err(EINVAL);
        }
    }
    let (n, ip, op) = copy_fds(t, &inp, in_pos, &out, out_pos, len as usize, false, false)?;
    write_off(t, off_in, ip)?;
    write_off(t, off_out, op)?;
    Ok(n as i64)
}

/// `splice(fd_in, off_in, fd_out, off_out, len, flags)` : l'un des deux est un tube (dont la
/// position doit être nulle, `ESPIPE` sinon). Les octets passent par un tampon noyau.
pub fn splice(t: &Thread, fd_in: u64, off_in: u64, fd_out: u64, off_out: u64, len: u64, flags: u64) -> SysResult {
    const SPLICE_F_NONBLOCK: u64 = 2;
    let inp = get_fd(t, fd_in)?;
    let out = get_fd(t, fd_out)?;
    let in_pipe = matches!(inp.node, Node::Pipe(_));
    let out_pipe = matches!(out.node, Node::Pipe(_));
    if !in_pipe && !out_pipe {
        return Err(EINVAL);
    }
    if (in_pipe && off_in != 0) || (out_pipe && off_out != 0) {
        return Err(ESPIPE);
    }
    if !inp.readable() || !out.writable() {
        return Err(EBADF);
    }
    let in_pos = read_off(t, off_in)?;
    let out_pos = read_off(t, off_out)?;
    let (n, ip, op) = copy_fds(t, &inp, in_pos, &out, out_pos, len as usize, flags & SPLICE_F_NONBLOCK != 0, false)?;
    write_off(t, off_in, ip)?;
    write_off(t, off_out, op)?;
    Ok(n as i64)
}

/// `fallocate(fd, mode, offset, len)`. Mode 0 : la taille couvre au moins `offset + len` ;
/// `FALLOC_FL_KEEP_SIZE` seul : rien à faire (pas de réservation d'avance) ;
/// `FALLOC_FL_PUNCH_HOLE | KEEP_SIZE` : la plage se relit nulle. Autres modes : `EOPNOTSUPP`.
pub fn fallocate(t: &Thread, fd: u64, mode: u64, offset: i64, len: i64) -> SysResult {
    const KEEP_SIZE: u64 = 1;
    const PUNCH_HOLE: u64 = 2;
    if offset < 0 || len <= 0 {
        return Err(EINVAL);
    }
    let f = get_fd(t, fd)?;
    if !f.writable() {
        return Err(EBADF);
    }
    let file = match &f.node {
        Node::File { file, .. } => file.clone(),
        Node::Pipe(_) => return Err(ESPIPE),
        Node::Dir { .. } => return Err(EISDIR),
        _ => return Err(ENODEV),
    };
    if file.lock().read_only() {
        return Err(EROFS);
    }
    let end = (offset as u64).checked_add(len as u64).ok_or(EFBIG)? as usize;
    {
        // Sceaux : agrandir (mode 0) ou écrire des zéros (trou percé) est une écriture.
        let size = file.lock().len();
        if mode == 0 {
            crate::seals::check_resize(&file, size, end.max(size))?;
        } else if mode != KEEP_SIZE {
            crate::seals::check_write(&file, offset as usize, len as usize, size)?;
        }
    }
    match mode {
        0 => {
            let mut l = file.lock();
            if l.len() < end {
                l.set_len(end).map_err(|e| no_space("fallocate", &e))?;
            }
        }
        KEEP_SIZE => {}
        m if m == PUNCH_HOLE | KEEP_SIZE => {
            let size = file.lock().len();
            let stop = end.min(size);
            let mut at = offset as usize;
            let zeros = vec![0u8; CHUNK];
            while at < stop {
                let n = (stop - at).min(CHUNK);
                file.lock().write_at(&zeros[..n], at).map_err(|e| no_space("fallocate", &e))?;
                at += n;
            }
        }
        _ => return Err(EOPNOTSUPP),
    }
    crate::pagecache::invalidate(&file);
    if mode != KEEP_SIZE {
        crate::inotify::open_file_event(&f, crate::inotify::IN_MODIFY);
    }
    Ok(0)
}

/// `memfd_create(name, flags)` : fichier tmpfs anonyme, en lecture/écriture.
pub fn memfd_create(t: &Thread, name: u64, flags: u64) -> SysResult {
    const MFD_CLOEXEC: u64 = 1;
    const MFD_ALLOW_SEALING: u64 = 2;
    // Linux 6.3 : `MFD_NOEXEC_SEAL` (jamais exécutable : pas de bit `x`, sceau `F_SEAL_EXEC`,
    // scellable) ou `MFD_EXEC` (exécutable, le défaut ici), pas les deux. `MFD_HUGETLB` : pas
    // de hugetlbfs (`EINVAL`, les programmes retombent sur un memfd ordinaire).
    const MFD_NOEXEC_SEAL: u64 = 8;
    const MFD_EXEC: u64 = 0x10;
    if flags & !(MFD_CLOEXEC | MFD_ALLOW_SEALING | MFD_NOEXEC_SEAL | MFD_EXEC) != 0
        || flags & (MFD_NOEXEC_SEAL | MFD_EXEC) == MFD_NOEXEC_SEAL | MFD_EXEC
    {
        return Err(EINVAL);
    }
    let name = user_path(t, name)?;
    if name.len() > 249 || name.contains('/') {
        return Err(EINVAL);
    }
    let noexec = flags & MFD_NOEXEC_SEAL != 0;
    let file = tmpfs::anon_file(if noexec { 0o666 } else { 0o777 }).map_err(|_| ENOMEM)?;
    let seals = if noexec {
        crate::seals::F_SEAL_EXEC
    } else if flags & MFD_ALLOW_SEALING != 0 {
        0
    } else {
        crate::seals::F_SEAL_SEAL
    };
    crate::seals::register(&file, seals);
    let of = OpenFile::new(Node::File { file, path: format!("/memfd:{name} (deleted)") }, O_RDWR);
    t.process.files.lock().alloc(of, 0, flags & MFD_CLOEXEC != 0).map(|fd| fd as i64).ok_or(EMFILE)
}

/// `mknodat(dirfd, path, mode, dev)` : fichiers réguliers ; tubes nommés, sockets et fichiers de
/// périphérique ne sont pas encore pris en charge (`EPERM`).
/// Périphérique caractère d'un nœud (`/dev` ou `mknod`) : ceux que MaiProc sait servir, par
/// (majeur, mineur) comme `chrdev_open`.
/// Instance de devpts que désigne le `ptmx` de chemin `abs` (`devpts_acquire`) : celle montée sur
/// `pts` dans le même dossier ; sinon `ENODEV`.
fn ptmx_instance(t: &Thread, abs: &str) -> Result<Arc<crate::pty::PtsFs>, i64> {
    let dir = abs.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    match lookup_any(t, &format!("{}/pts", dir), true)? {
        Lookup::Pts(fs, c) if c == "/dev/pts" => Ok(fs),
        _ => Err(ENODEV),
    }
}

fn char_device(t: &Thread, abs: &str, rdev: (u32, u32), flags: u32) -> Result<Node, i64> {
    match rdev {
        (1, 3) => Ok(Node::Null),
        (1, 5) | (1, 7) => Ok(Node::Zero),
        (1, 8) | (1, 9) => Ok(Node::Random),
        // `/dev/tty` : terminal de contrôle, sinon la console du processus.
        (5, 0) => {
            let ctty = t.process.ctty.lock().as_ref().and_then(|w| w.upgrade());
            match ctty {
                Some(_) => crate::pty::open_ctty(t, flags).unwrap_or(Err(ENXIO)),
                None => console_of(t).ok_or(ENXIO),
            }
        }
        (5, 1) => console_of(t).ok_or(ENXIO),
        (5, 2) => crate::pty::open_ptmx(t, &ptmx_instance(t, abs)?),
        (136, n) => crate::pty::open_init_slave(t, n, flags).unwrap_or(Err(ENXIO)),
        _ => Err(ENXIO),
    }
}

/// `mknodat(dirfd, path, mode, dev)` : fichier régulier, tube nommé (`mkfifo`), socket, et pour
/// un processus privilégié (`CAP_MKNOD`) périphérique caractère ou bloc (`dev` au format de
/// Linux : majeur et mineur répartis sur 64 bits).
pub fn mknodat(t: &Thread, dirfd: i64, path: u64, mode: u64, dev: u64) -> SysResult {
    const S_IFMT: u64 = 0o170000;
    let kind = match mode & S_IFMT {
        0 | 0o100000 => NodeKind::File,
        0o010000 => NodeKind::Fifo,
        0o140000 => NodeKind::Socket,
        0o020000 => NodeKind::CharDev,
        0o060000 => NodeKind::BlockDev,
        0o040000 => return Err(EPERM), // un dossier se crée par `mkdir`
        _ => return Err(EINVAL),
    };
    if matches!(kind, NodeKind::CharDev | NodeKind::BlockDev) && !crate::userns::current_capable(t, crate::userns::CAP_MKNOD) {
        return Err(EPERM);
    }
    let abs = absolute(t, dirfd, &user_path(t, path)?)?;
    if walk(t, &abs, false).is_ok() || special(t, &abs).is_some() {
        return Err(EEXIST);
    }
    let (parent, name) = split_parent(t, &abs)?;
    if parent.lock().read_only() {
        return Err(EROFS);
    }
    check_access(t, &FileOrDir::Dir(parent.clone()), crate::creds::MAY_WRITE | crate::creds::MAY_EXEC)?;
    let perm = creation_perm(t, mode);
    let f = if kind == NodeKind::File {
        create_file_in(&parent, &name, perm)?
    } else {
        let major = (((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff)) as u32;
        let minor = ((dev & 0xff) | ((dev >> 12) & !0xff)) as u32;
        let made = parent.lock().create_special(&name, kind, perm, (major, minor));
        match made {
            Some(r) => r.map_err(create_error)?,
            // Ce système de fichiers n'a pas de nœuds spéciaux (Linux : `EPERM`).
            None => return Err(EPERM),
        }
    };
    give_to_caller(t, FileOrDir::File(f));
    crate::inotify::created(&abs, false);
    Ok(0)
}
