//! `inotify` : surveillance des fichiers et dossiers (Linux `fs/notify/inotify`).
//!
//! Une instance (`inotify_init1`) tient des surveillances (`inotify_add_watch`), chacune sur un
//! inode désigné par `(st_dev, st_ino)` ; les événements s'y lisent comme des
//! `struct inotify_event` (16 octets + nom complété de zéros jusqu'à un multiple de 16).
//!
//! Sources : les opérations de MaiProc sur les fichiers (`linux::fs`) appellent les crochets
//! [`file_event`], [`created`], [`deleted`], [`moved`]. Comme Linux, un événement sur un fichier
//! (`IN_ACCESS`, `IN_MODIFY`, `IN_ATTRIB`, `IN_OPEN`, `IN_CLOSE_*`) va aux surveillances du
//! fichier (sans nom) et à celles de son dossier (avec le nom) ; créations, suppressions et
//! déplacements vont au dossier, `IN_DELETE_SELF`/`IN_MOVE_SELF` à l'inode lui-même. Les
//! applications natives de MaiOS ne passent pas par là : leurs modifications ne se voient pas.
//!
//! Coût nul sans surveillance (un compteur atomique). Sinon, le dossier parent d'un fichier
//! n'est cherché que si son chemin est celui d'une surveillance (les chemins surveillés sont
//! tenus à jour aux déplacements).

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use alloc::format;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use fs_node::FileOrDir;
use spin::Mutex;

use crate::fd::{Node, OpenFile, O_NONBLOCK, O_RDWR};
use crate::linux::errno::*;
use crate::linux::SysResult;
use crate::object::{ev, new_ino, object_any, wait_errno, FileObject};
use crate::wait::{wait_event, WaitQueue};
use crate::Thread;

pub const IN_ACCESS: u32 = 0x1;
pub const IN_MODIFY: u32 = 0x2;
pub const IN_ATTRIB: u32 = 0x4;
pub const IN_CLOSE_WRITE: u32 = 0x8;
pub const IN_CLOSE_NOWRITE: u32 = 0x10;
pub const IN_OPEN: u32 = 0x20;
pub const IN_MOVED_FROM: u32 = 0x40;
pub const IN_MOVED_TO: u32 = 0x80;
pub const IN_CREATE: u32 = 0x100;
pub const IN_DELETE: u32 = 0x200;
pub const IN_DELETE_SELF: u32 = 0x400;
pub const IN_MOVE_SELF: u32 = 0x800;
const IN_Q_OVERFLOW: u32 = 0x4000;
const IN_IGNORED: u32 = 0x8000;
const IN_ALL_EVENTS: u32 = 0xfff;
const IN_ONLYDIR: u32 = 0x0100_0000;
const IN_DONT_FOLLOW: u32 = 0x0200_0000;
const IN_EXCL_UNLINK: u32 = 0x0400_0000;
const IN_MASK_CREATE: u32 = 0x1000_0000;
const IN_MASK_ADD: u32 = 0x2000_0000;
const IN_ISDIR: u32 = 0x4000_0000;
const IN_ONESHOT: u32 = 0x8000_0000;
const IN_NONBLOCK: u64 = 0x800;
const IN_CLOEXEC: u64 = 0x80000;

/// `/proc/sys/fs/inotify/max_queued_events`.
const MAX_QUEUED: usize = 16384;
/// Surveillances par instance (`max_user_watches` de Linux est par utilisateur).
const MAX_WATCHES: usize = 65536;

/// Identité d'un inode : `(st_dev, st_ino)`.
pub(crate) type Key = (u64, u64);

struct Event {
    wd: i32,
    mask: u32,
    cookie: u32,
    name: Vec<u8>,
}

impl Event {
    /// Longueur du nom dans l'événement : 0, ou nom + zéro final complétés à 16 octets.
    fn name_len(&self) -> usize {
        if self.name.is_empty() { 0 } else { (self.name.len() + 1 + 15) & !15 }
    }

    fn size(&self) -> usize { 16 + self.name_len() }
}

struct Watch {
    key: Key,
    mask: u32,
}

struct State {
    events: VecDeque<Event>,
    bytes: usize,
    watches: BTreeMap<i32, Watch>,
    by_key: BTreeMap<Key, i32>,
    next_wd: i32,
}

pub struct Inotify {
    st: Mutex<State>,
    wq: WaitQueue,
    ino: u64,
}

/// Registre global : inode → surveillances, et chemin surveillé → inode (pour trouver le
/// dossier d'un fichier sans le chercher dans le VFS).
struct Registry {
    by_key: BTreeMap<Key, Vec<(Weak<Inotify>, i32)>>,
    paths: BTreeMap<String, Key>,
}

static REG: Mutex<Registry> = Mutex::new(Registry { by_key: BTreeMap::new(), paths: BTreeMap::new() });
/// Nombre de surveillances (crochets sans effet à 0).
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
/// Cookie qui relie `IN_MOVED_FROM` et `IN_MOVED_TO`.
static COOKIE: AtomicU32 = AtomicU32::new(1);

fn register(inst: &Arc<Inotify>, wd: i32, key: Key, path: &str) {
    let mut r = REG.lock();
    r.by_key.entry(key).or_default().push((Arc::downgrade(inst), wd));
    r.paths.insert(String::from(path), key);
    ACTIVE.fetch_add(1, Ordering::AcqRel);
}

fn unregister(inst: *const Inotify, wd: i32, key: Key) {
    let mut r = REG.lock();
    let Some(v) = r.by_key.get_mut(&key) else { return };
    let before = v.len();
    v.retain(|(w, d)| !(w.as_ptr() == inst && *d == wd));
    if v.len() == before {
        return;
    }
    ACTIVE.fetch_sub(1, Ordering::AcqRel);
    if v.is_empty() {
        r.by_key.remove(&key);
        r.paths.retain(|_, k| *k != key);
    }
}

impl Inotify {
    /// Ajoute un événement (fusionné avec le dernier s'il est identique, comme Linux) ;
    /// file pleine : un seul `IN_Q_OVERFLOW`.
    fn push(st: &mut State, e: Event) {
        if let Some(last) = st.events.back() {
            if last.wd == e.wd && last.mask == e.mask && last.cookie == e.cookie && last.name == e.name {
                return;
            }
        }
        if st.events.len() >= MAX_QUEUED {
            if st.events.back().map_or(true, |l| l.mask != IN_Q_OVERFLOW) {
                let o = Event { wd: -1, mask: IN_Q_OVERFLOW, cookie: 0, name: Vec::new() };
                st.bytes += o.size();
                st.events.push_back(o);
            }
            return;
        }
        st.bytes += e.size();
        st.events.push_back(e);
    }

    /// Événement pour la surveillance `wd`, s'il l'intéresse ; rend la surveillance retirée par
    /// `IN_ONESHOT` (à désinscrire du registre).
    fn deliver(&self, wd: i32, mask: u32, cookie: u32, name: &[u8]) -> Option<Key> {
        let mut gone = None;
        {
            let mut st = self.st.lock();
            let Some(w) = st.watches.get(&wd) else { return None };
            let wmask = w.mask;
            if wmask & mask & IN_ALL_EVENTS == 0 {
                return None;
            }
            // `IN_EXCL_UNLINK` : rien pour un enfant déjà retiré (lectures d'un fichier supprimé).
            Self::push(&mut st, Event { wd, mask, cookie, name: name.to_vec() });
            if wmask & IN_ONESHOT != 0 {
                if let Some(w) = st.watches.remove(&wd) {
                    st.by_key.remove(&w.key);
                    Self::push(&mut st, Event { wd, mask: IN_IGNORED, cookie: 0, name: Vec::new() });
                    gone = Some(w.key);
                }
            }
        }
        self.wq.wake_all();
        gone
    }

    /// L'inode surveillé a disparu : `IN_IGNORED`, surveillance retirée.
    fn drop_watch(&self, wd: i32) -> Option<Key> {
        let w = {
            let mut st = self.st.lock();
            let w = st.watches.remove(&wd)?;
            st.by_key.remove(&w.key);
            Self::push(&mut st, Event { wd, mask: IN_IGNORED, cookie: 0, name: Vec::new() });
            w
        };
        self.wq.wake_all();
        Some(w.key)
    }
}

impl Drop for Inotify {
    fn drop(&mut self) {
        let me = self as *const Inotify;
        let watches = core::mem::take(&mut self.st.get_mut().watches);
        for (wd, w) in watches {
            unregister(me, wd, w.key);
        }
    }
}

impl FileObject for Inotify {
    object_any!();

    fn link_name(&self) -> String { String::from("anon_inode:inotify") }

    fn stat(&self) -> (u32, u64) { (0o600, self.ino) }

    fn read(&self, t: &Thread, kb: &mut [u8], nonblock: bool) -> Result<usize, i64> {
        wait_event(&[&self.wq], None, Some(t), || {
            let mut st = self.st.lock();
            let Some(first) = st.events.front().map(Event::size) else {
                return nonblock.then_some(Err(EAGAIN));
            };
            if first > kb.len() {
                return Some(Err(EINVAL));
            }
            let mut off = 0;
            while let Some(e) = st.events.front() {
                let sz = e.size();
                if off + sz > kb.len() {
                    break;
                }
                let nl = e.name_len();
                kb[off..off + 4].copy_from_slice(&e.wd.to_le_bytes());
                kb[off + 4..off + 8].copy_from_slice(&e.mask.to_le_bytes());
                kb[off + 8..off + 12].copy_from_slice(&e.cookie.to_le_bytes());
                kb[off + 12..off + 16].copy_from_slice(&(nl as u32).to_le_bytes());
                let dst = &mut kb[off + 16..off + sz];
                dst.fill(0);
                dst[..e.name.len()].copy_from_slice(&e.name);
                off += sz;
                st.bytes -= sz;
                st.events.pop_front();
            }
            Some(Ok(off))
        })
        .map_err(wait_errno)?
    }

    fn poll(&self, _t: Option<&Thread>) -> u32 {
        if self.st.lock().events.is_empty() { 0 } else { ev::READABLE }
    }

    fn queues<'a>(&'a self, cb: &mut dyn FnMut(&'a WaitQueue)) {
        cb(&self.wq);
    }

    fn pending_bytes(&self) -> Option<usize> { Some(self.st.lock().bytes) }

    fn fdinfo(&self) -> String {
        let st = self.st.lock();
        let mut s = String::new();
        for (wd, w) in &st.watches {
            s += &format!(
                "inotify wd:{:x} ino:{:x} sdev:{:x} mask:{:x} ignored_mask:0\n",
                wd, w.key.1, w.key.0, w.mask & !(IN_ONESHOT | IN_MASK_ADD | IN_MASK_CREATE)
            );
        }
        s
    }
}

// --- Appels système ---------------------------------------------------------------------------

/// `inotify_init1(flags)` (et `inotify_init` avec `flags = 0`).
pub fn inotify_init1(t: &Thread, flags: u64) -> SysResult {
    if flags & !(IN_NONBLOCK | IN_CLOEXEC) != 0 {
        return Err(EINVAL);
    }
    let obj = Arc::new(Inotify {
        st: Mutex::new(State { events: VecDeque::new(), bytes: 0, watches: BTreeMap::new(), by_key: BTreeMap::new(), next_wd: 1 }),
        wq: WaitQueue::new(),
        ino: new_ino(),
    });
    let status = O_RDWR | if flags & IN_NONBLOCK != 0 { O_NONBLOCK } else { 0 };
    let file = OpenFile::new(Node::Object(obj), status);
    t.process.files.lock().alloc(file, 0, flags & IN_CLOEXEC != 0).map(|fd| fd as i64).ok_or(EMFILE)
}

fn instance(t: &Thread, fd: u64) -> Result<Arc<Inotify>, i64> {
    let f = crate::linux::fs::get_fd(t, fd)?;
    let o = f.object().ok_or(EINVAL)?.clone();
    o.into_any().downcast::<Inotify>().map_err(|_| EINVAL)
}

/// `inotify_add_watch(fd, path, mask)` : rend le numéro de la surveillance (le même pour un
/// inode déjà surveillé par cette instance).
pub fn inotify_add_watch(t: &Thread, fd: u64, path: u64, mask: u64) -> SysResult {
    let mask = mask as u32;
    let inst = instance(t, fd)?;
    if mask & IN_ALL_EVENTS == 0 || (mask & IN_MASK_ADD != 0 && mask & IN_MASK_CREATE != 0) {
        return Err(EINVAL);
    }
    let raw = crate::linux::user_path(t, path)?;
    let abs = crate::linux::fs::resolved(t, crate::linux::fs::AT_FDCWD, &raw)?;
    let node = crate::linux::fs::walk(t, &abs, mask & IN_DONT_FOLLOW == 0)?;
    if mask & IN_ONLYDIR != 0 && !matches!(node, FileOrDir::Dir(_)) {
        return Err(ENOTDIR);
    }
    crate::linux::fs::check_access(t, &node, crate::creds::MAY_READ)?;
    let key = crate::linux::fs::inode_key(&node, &abs);
    let keep = mask & (IN_ALL_EVENTS | IN_ONESHOT | IN_EXCL_UNLINK);
    let (wd, fresh) = {
        let mut st = inst.st.lock();
        if let Some(&wd) = st.by_key.get(&key) {
            if mask & IN_MASK_CREATE != 0 {
                return Err(EEXIST);
            }
            let w = st.watches.get_mut(&wd).unwrap();
            w.mask = if mask & IN_MASK_ADD != 0 { w.mask | keep } else { keep };
            (wd, false)
        } else {
            if st.watches.len() >= MAX_WATCHES {
                return Err(ENOSPC);
            }
            let mut wd = st.next_wd;
            while st.watches.contains_key(&wd) {
                wd = if wd == i32::MAX { 1 } else { wd + 1 };
            }
            st.next_wd = if wd == i32::MAX { 1 } else { wd + 1 };
            st.watches.insert(wd, Watch { key, mask: keep });
            st.by_key.insert(key, wd);
            (wd, true)
        }
    };
    if fresh {
        register(&inst, wd, key, &abs);
    }
    // Le chemin physique (liens résolus) désigne aussi l'inode : les événements venus d'un
    // chemin sans lien y retrouvent la surveillance.
    if let Some(phys) = crate::linux::fs::physical(t, &abs).filter(|p| *p != abs) {
        REG.lock().paths.insert(phys, key);
    }
    Ok(wd as i64)
}

/// `inotify_rm_watch(fd, wd)` : `IN_IGNORED`, puis plus rien de cette surveillance.
pub fn inotify_rm_watch(t: &Thread, fd: u64, wd: u64) -> SysResult {
    let inst = instance(t, fd)?;
    let key = inst.drop_watch(wd as i32).ok_or(EINVAL)?;
    unregister(Arc::as_ptr(&inst), wd as i32, key);
    Ok(0)
}

// --- Crochets des opérations sur les fichiers --------------------------------------------------

/// Y a-t-il au moins une surveillance ?
#[inline]
pub(crate) fn active() -> bool {
    ACTIVE.load(Ordering::Acquire) != 0
}

/// Envoie un événement aux surveillances de `key` ; retire celles qui se sont éteintes.
fn emit(key: Key, mask: u32, cookie: u32, name: &[u8]) {
    let list: Vec<(Weak<Inotify>, i32)> = match REG.lock().by_key.get(&key) {
        Some(v) => v.clone(),
        None => return,
    };
    for (w, wd) in list {
        let Some(inst) = w.upgrade() else { continue };
        if let Some(k) = inst.deliver(wd, mask, cookie, name) {
            unregister(Arc::as_ptr(&inst), wd, k);
        }
    }
}

/// L'inode `key` a disparu : `IN_DELETE_SELF` puis `IN_IGNORED` pour chaque surveillance.
fn gone(key: Key, isdir: bool) {
    let list: Vec<(Weak<Inotify>, i32)> = match REG.lock().by_key.get(&key) {
        Some(v) => v.clone(),
        None => return,
    };
    for (w, wd) in list {
        let Some(inst) = w.upgrade() else { continue };
        inst.deliver(wd, IN_DELETE_SELF | if isdir { IN_ISDIR } else { 0 }, 0, b"");
        if let Some(k) = inst.drop_watch(wd) {
            unregister(Arc::as_ptr(&inst), wd, k);
        }
    }
}

/// `(dossier, nom)` d'un chemin absolu.
fn split(abs: &str) -> (&str, &str) {
    match abs.rfind('/') {
        Some(0) => ("/", &abs[1..]),
        Some(i) => (&abs[..i], &abs[i + 1..]),
        None => ("/", abs),
    }
}

/// Inode du dossier `dir` si un chemin surveillé le désigne.
fn watched_dir(dir: &str) -> Option<Key> {
    REG.lock().paths.get(dir).copied()
}

/// Comme [`watched_dir`], en essayant aussi le chemin physique (créations, suppressions et
/// déplacements, rares : la résolution des liens n'y coûte rien de notable).
fn watched_dir_any(dir: &str) -> Option<Key> {
    watched_dir(dir).or_else(|| crate::mount::resolve(&crate::mount::init_root(), dir, true).ok().and_then(|f| watched_dir(&f.path)))
}

/// Nom en octets (noms PEP 383 réencodés).
fn name_bytes(name: &str) -> Vec<u8> {
    fs_node::rawname::encode(name)
}

/// Événement sur un fichier ou un dossier (`IN_ACCESS`, `IN_MODIFY`, `IN_ATTRIB`, `IN_OPEN`,
/// `IN_CLOSE_*`) : à ses surveillances, et à celles de son dossier avec son nom.
pub(crate) fn file_event(node: &FileOrDir, abs: &str, mask: u32) {
    if !active() {
        return;
    }
    let isdir = if matches!(node, FileOrDir::Dir(_)) { IN_ISDIR } else { 0 };
    emit(crate::linux::fs::inode_key(node, abs), mask | isdir, 0, b"");
    if abs == "/" {
        return;
    }
    let (dir, name) = split(abs);
    if let Some(k) = watched_dir(dir) {
        emit(k, mask | isdir, 0, &name_bytes(name));
    }
}

/// Même chose pour une description ouverte.
pub(crate) fn open_file_event(f: &OpenFile, mask: u32) {
    if !active() {
        return;
    }
    match &f.node {
        Node::File { file, path } => file_event(&FileOrDir::File(file.clone()), path, mask),
        Node::Dir { dir, path } => file_event(&FileOrDir::Dir(dir.clone()), path, mask),
        _ => {}
    }
}

/// Dernière fermeture d'une description : `IN_CLOSE_WRITE` ou `IN_CLOSE_NOWRITE`.
pub(crate) fn on_close(f: &OpenFile) {
    if active() {
        open_file_event(f, if f.writable() { IN_CLOSE_WRITE } else { IN_CLOSE_NOWRITE });
    }
}

/// Entrée `abs` créée (`IN_CREATE` à son dossier).
pub(crate) fn created(abs: &str, isdir: bool) {
    if !active() {
        return;
    }
    let (dir, name) = split(abs);
    if let Some(k) = watched_dir_any(dir) {
        emit(k, IN_CREATE | if isdir { IN_ISDIR } else { 0 }, 0, &name_bytes(name));
    }
}

/// Entrée `abs` (inode `key`) supprimée : `IN_DELETE` à son dossier ; l'inode lui-même reçoit
/// `IN_DELETE_SELF` s'il n'a plus de lien (`still_linked` faux), sinon `IN_ATTRIB` (nombre de liens).
pub(crate) fn deleted(abs: &str, key: Key, isdir: bool, still_linked: bool) {
    if !active() {
        return;
    }
    let d = if isdir { IN_ISDIR } else { 0 };
    if still_linked {
        emit(key, IN_ATTRIB | d, 0, b"");
    }
    let (dir, name) = split(abs);
    if let Some(k) = watched_dir_any(dir) {
        emit(k, IN_DELETE | d, 0, &name_bytes(name));
    }
    if !still_linked {
        gone(key, isdir);
    }
}

/// `old` déplacé en `new` (inode `key`) ; `victim` : inode remplacé à l'arrivée (et s'il
/// n'a plus de lien). Les chemins surveillés sous `old` suivent.
pub(crate) fn moved(old: &str, new: &str, key: Key, isdir: bool, victim: Option<(Key, bool, bool)>) {
    if !active() {
        return;
    }
    let d = if isdir { IN_ISDIR } else { 0 };
    let cookie = COOKIE.fetch_add(1, Ordering::Relaxed).max(1);
    let (odir, oname) = split(old);
    if let Some(k) = watched_dir_any(odir) {
        emit(k, IN_MOVED_FROM | d, cookie, &name_bytes(oname));
    }
    let (ndir, nname) = split(new);
    if let Some(k) = watched_dir_any(ndir) {
        emit(k, IN_MOVED_TO | d, cookie, &name_bytes(nname));
    }
    emit(key, IN_MOVE_SELF | d, 0, b"");
    if let Some((vk, visdir, vlinked)) = victim {
        if vlinked {
            emit(vk, IN_ATTRIB, 0, b"");
        } else {
            gone(vk, visdir);
        }
    }
    // Les chemins surveillés de l'arborescence déplacée changent.
    let mut r = REG.lock();
    let prefix = format!("{}/", old);
    let moved: Vec<(String, Key)> =
        r.paths.iter().filter(|(p, _)| p.as_str() == old || p.starts_with(&prefix)).map(|(p, k)| (p.clone(), *k)).collect();
    for (p, k) in moved {
        r.paths.remove(&p);
        r.paths.insert(format!("{}{}", new, &p[old.len()..]), k);
    }
}
