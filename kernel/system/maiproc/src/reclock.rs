//! Verrous d'enregistrement POSIX (`fcntl` : `F_SETLK`, `F_SETLKW`, `F_GETLK`) et leurs
//! variantes « description ouverte » (`F_OFD_*`, Linux 3.15), dont dépendent SQLite, les bases
//! de profils de Firefox, `dpkg`, etc.
//!
//! * Un verrou couvre une plage d'octets `[début, fin]` (fin infinie pour `l_len = 0`), en
//!   lecture (partagé) ou en écriture (exclusif) ; deux verrous de détenteurs différents sont en
//!   conflit s'ils se chevauchent et que l'un est en écriture.
//! * **Détenteur** : le processus pour les verrous POSIX (relâchés quand il ferme *n'importe
//!   quel* descripteur du fichier, et à sa fin — la sémantique historique, piégeuse mais
//!   normative), la description ouverte pour les OFD (relâchés à sa dernière fermeture,
//!   indépendants des fils et de `fork`, comme `flock` mais par plage).
//! * Poser un verrou remplace ceux du même détenteur sur la plage (coupés au besoin) et fusionne
//!   les plages voisines de même sorte, comme `posix_lock_inode` de Linux.
//! * `F_SETLKW` attend, interruptible par un signal. **Interblocage** : avant d'attendre, la
//!   chaîne « A attend B qui attend C… » des détenteurs POSIX est suivie ; si elle revient au
//!   demandeur, `EDEADLK` (comme `posix_locks_deadlock`).
//!
//! Le fichier est désigné comme pour `flock` : `(st_dev, st_ino)` s'il est connu.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use spin::Mutex;

use crate::fd::OpenFile;
use crate::linux::errno::{EAGAIN, EBADF, EDEADLK, EFAULT, EINVAL, EOVERFLOW};
use crate::wait::{wait_event, WaitQueue};
use crate::Thread;

const F_RDLCK: i16 = 0;
const F_WRLCK: i16 = 1;
const F_UNLCK: i16 = 2;

pub const F_GETLK: u64 = 5;
pub const F_SETLKW: u64 = 7;
pub const F_OFD_GETLK: u64 = 36;
pub const F_OFD_SETLK: u64 = 37;
pub const F_OFD_SETLKW: u64 = 38;

type Key = (u64, u64);

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Owner {
    /// Processus (pid).
    Posix(usize),
    /// Description ouverte (adresse de l'`OpenFile`).
    Ofd(usize),
}

#[derive(Clone, Copy, Debug)]
struct Lock {
    start: u64,
    /// Inclus ; `u64::MAX` : jusqu'à la fin du fichier, quelle qu'elle soit.
    end: u64,
    write: bool,
    owner: Owner,
    /// Rendu par `F_GETLK` (`-1` pour un OFD).
    pid: i32,
}

impl Lock {
    fn overlaps(&self, s: u64, e: u64) -> bool {
        self.start <= e && s <= self.end
    }
}

struct Table {
    files: BTreeMap<Key, Vec<Lock>>,
    /// Qui attend qui (détenteurs POSIX), pour la détection d'interblocage.
    blocked_on: BTreeMap<Owner, Owner>,
}

static TABLE: Mutex<Table> = Mutex::new(Table { files: BTreeMap::new(), blocked_on: BTreeMap::new() });
static WAITERS: WaitQueue = WaitQueue::new();
/// Verrous POSIX posés (tous fichiers) : une fermeture de descripteur ne consulte la table que
/// s'il y en a.
static POSIX_LOCKS: AtomicUsize = AtomicUsize::new(0);

fn conflict<'a>(locks: &'a [Lock], me: Owner, s: u64, e: u64, write: bool) -> Option<&'a Lock> {
    locks.iter().find(|l| l.owner != me && l.overlaps(s, e) && (write || l.write))
}

/// Pose (ou retire, `kind = None`) la plage de `me` : coupe ses verrous recouverts, ajoute le
/// nouveau, fusionne les voisins de même sorte.
fn apply(locks: &mut Vec<Lock>, me: Owner, pid: i32, s: u64, e: u64, kind: Option<bool>) {
    let mut kept = Vec::with_capacity(locks.len() + 2);
    for l in locks.drain(..) {
        if l.owner != me || !l.overlaps(s, e) {
            kept.push(l);
            continue;
        }
        if l.start < s {
            kept.push(Lock { end: s - 1, ..l });
        }
        if l.end > e {
            kept.push(Lock { start: e + 1, ..l });
        }
    }
    if let Some(write) = kind {
        let mut new = Lock { start: s, end: e, write, owner: me, pid };
        // Fusion avec les plages contiguës de même détenteur et même sorte.
        kept.retain(|l| {
            let adjacent = l.owner == me
                && l.write == write
                && (l.end != u64::MAX && l.end + 1 == new.start || new.end != u64::MAX && new.end + 1 == l.start);
            if adjacent {
                new.start = new.start.min(l.start);
                new.end = new.end.max(l.end);
            }
            !adjacent
        });
        kept.push(new);
    }
    kept.sort_by_key(|l| l.start);
    *locks = kept;
}

fn count_posix(t: &Table) -> usize {
    t.files.values().flat_map(|v| v.iter()).filter(|l| matches!(l.owner, Owner::Posix(_))).count()
}

fn store_count(t: &Table) {
    POSIX_LOCKS.store(count_posix(t), Ordering::Release);
}

/// La chaîne d'attente partant de `blocker` mène-t-elle à `me` ?
fn would_deadlock(t: &Table, me: Owner, blocker: Owner) -> bool {
    let mut cur = blocker;
    for _ in 0..1024 {
        if cur == me {
            return true;
        }
        match t.blocked_on.get(&cur) {
            Some(&next) => cur = next,
            None => return false,
        }
    }
    false
}

struct Request {
    kind: i16,
    start: u64,
    end: u64,
}

/// Disposition de `struct flock` : x86_64 (32 octets), `compat_flock` i386 (16 octets,
/// décalages de 32 bits) ou `compat_flock64` i386 (24 octets, groupée).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FlockLayout {
    Native,
    Compat32,
    Compat64,
}

impl FlockLayout {
    fn size(self) -> usize {
        match self {
            FlockLayout::Native => 32,
            FlockLayout::Compat32 => 16,
            FlockLayout::Compat64 => 24,
        }
    }

    /// Décalages de `l_start`, `l_len` (et leur taille), `l_pid`.
    fn offsets(self) -> (usize, usize, usize, usize) {
        match self {
            FlockLayout::Native => (8, 16, 8, 24),
            FlockLayout::Compat32 => (4, 8, 4, 12),
            FlockLayout::Compat64 => (4, 12, 8, 20),
        }
    }
}

fn read_request(t: &Thread, f: &OpenFile, arg: u64, ofd: bool, layout: FlockLayout) -> Result<Request, i64> {
    let mut raw = [0u8; 32];
    t.mm().copy_from_user(&mut raw[..layout.size()], arg as usize).map_err(|_| EFAULT)?;
    let kind = i16::from_le_bytes([raw[0], raw[1]]);
    let whence = i16::from_le_bytes([raw[2], raw[3]]);
    let (so, lo, w, po) = layout.offsets();
    let wide = |o: usize| if w == 8 { i64::from_le_bytes(raw[o..o + 8].try_into().unwrap()) } else { i32::from_le_bytes(raw[o..o + 4].try_into().unwrap()) as i64 };
    let l_start = wide(so);
    let l_len = wide(lo);
    let l_pid = i32::from_le_bytes(raw[po..po + 4].try_into().unwrap());
    if ofd && l_pid != 0 {
        return Err(EINVAL);
    }
    if !matches!(kind, F_RDLCK | F_WRLCK | F_UNLCK) {
        return Err(EINVAL);
    }
    let base: i64 = match whence {
        0 => 0,
        1 => *f.pos.lock() as i64,
        2 => match &f.node {
            crate::fd::Node::File { file, .. } => file.lock().len() as i64,
            _ => 0,
        },
        _ => return Err(EINVAL),
    };
    let start = base.checked_add(l_start).ok_or(EOVERFLOW)?;
    let (s, e) = if l_len > 0 {
        let end = start.checked_add(l_len - 1).ok_or(EOVERFLOW)?;
        (start, end as u64)
    } else if l_len == 0 {
        (start, u64::MAX)
    } else {
        let s = start.checked_add(l_len).ok_or(EINVAL)?;
        if start == 0 {
            return Err(EINVAL);
        }
        (s, (start - 1) as u64)
    };
    if s < 0 {
        return Err(EINVAL);
    }
    Ok(Request { kind, start: s as u64, end: e })
}

/// `fcntl(fd, F_*LK*, &flock)`.
pub fn fcntl_lock(t: &Thread, f: &OpenFile, cmd: u64, arg: u64, layout: FlockLayout) -> Result<i64, i64> {
    let ofd = matches!(cmd, F_OFD_GETLK | F_OFD_SETLK | F_OFD_SETLKW);
    let req = read_request(t, f, arg, ofd, layout)?;
    let key = crate::flock::key_of(f).ok_or(EINVAL)?;
    let pid = t.process.pid;
    let me = if ofd { Owner::Ofd(f as *const OpenFile as usize) } else { Owner::Posix(pid) };
    let my_pid = if ofd { -1 } else { pid as i32 };

    if cmd == F_GETLK || cmd == F_OFD_GETLK {
        if req.kind == F_UNLCK {
            return Err(EINVAL);
        }
        let found = {
            let tab = TABLE.lock();
            tab.files.get(&key).and_then(|v| conflict(v, me, req.start, req.end, req.kind == F_WRLCK).copied())
        };
        let mut raw = [0u8; 32];
        let (so, lo, w, po) = layout.offsets();
        match found {
            None => raw[0..2].copy_from_slice(&F_UNLCK.to_le_bytes()),
            Some(l) => {
                raw[0..2].copy_from_slice(&(if l.write { F_WRLCK } else { F_RDLCK }).to_le_bytes());
                let len = if l.end == u64::MAX { 0 } else { (l.end - l.start + 1) as i64 };
                if w == 8 {
                    raw[so..so + 8].copy_from_slice(&(l.start as i64).to_le_bytes());
                    raw[lo..lo + 8].copy_from_slice(&len.to_le_bytes());
                } else {
                    // `compat_flock` : un verrou au-delà de 2 Gio ne se décrit pas (`EOVERFLOW`).
                    if l.start > i32::MAX as u64 || len > i32::MAX as i64 {
                        return Err(EOVERFLOW);
                    }
                    raw[so..so + 4].copy_from_slice(&(l.start as i32).to_le_bytes());
                    raw[lo..lo + 4].copy_from_slice(&(len as i32).to_le_bytes());
                }
                // Propriétaire vu de l'appelant (0 hors de son espace pid ; −1 : verrou OFD).
                let lp = if l.pid < 0 { l.pid } else { t.process.vnr(l.pid as usize) as i32 };
                raw[po..po + 4].copy_from_slice(&lp.to_le_bytes());
            }
        }
        // Pas de conflit : seul `l_type` change. Conflit : toute la description.
        let n = if found.is_none() { 2 } else { layout.size() };
        t.mm().copy_to_user(arg as usize, &raw[..n]).map_err(|_| EFAULT)?;
        return Ok(0);
    }

    match req.kind {
        F_RDLCK if !f.readable() => return Err(EBADF),
        F_WRLCK if !f.writable() => return Err(EBADF),
        _ => {}
    }
    let kind = match req.kind {
        F_UNLCK => None,
        k => Some(k == F_WRLCK),
    };
    let wait = cmd == F_SETLKW || cmd == F_OFD_SETLKW;
    // Une tentative : `Some(Ok)` posé, `Some(Err)` refus définitif, `None` : à attendre.
    let attempt = |register: bool| -> Option<Result<i64, i64>> {
        let mut tab = TABLE.lock();
        if let Some(write) = kind {
            let blocker = tab.files.get(&key).and_then(|v| conflict(v, me, req.start, req.end, write)).map(|l| l.owner);
            if let Some(b) = blocker {
                if !register {
                    return Some(Err(EAGAIN));
                }
                if matches!((me, b), (Owner::Posix(_), Owner::Posix(_))) && would_deadlock(&tab, me, b) {
                    tab.blocked_on.remove(&me);
                    return Some(Err(EDEADLK));
                }
                tab.blocked_on.insert(me, b);
                return None;
            }
        }
        tab.blocked_on.remove(&me);
        let locks = tab.files.entry(key).or_default();
        apply(locks, me, my_pid, req.start, req.end, kind);
        if locks.is_empty() {
            tab.files.remove(&key);
        }
        store_count(&tab);
        drop(tab);
        if ofd {
            f.reclocked.store(true, Ordering::Release);
        }
        Some(Ok(0))
    };
    let res = if wait {
        match wait_event(&[&WAITERS], None, Some(t), || attempt(true)) {
            Ok(r) => r,
            Err(_) => {
                TABLE.lock().blocked_on.remove(&me);
                Err(crate::signal::ERESTARTSYS)
            }
        }
    } else {
        attempt(false).unwrap_or(Err(EAGAIN))
    };
    // Un retrait ou une conversion en lecture peut libérer des attentes.
    if res.is_ok() && kind != Some(true) {
        WAITERS.wake_all();
    }
    res
}

/// Retire les verrous de `owner` sur `key` (tous les fichiers si `key` est `None`).
fn release(owner: Owner, key: Option<Key>) {
    let mut tab = TABLE.lock();
    let keys: Vec<Key> = match key {
        Some(k) => alloc::vec![k],
        None => tab.files.keys().copied().collect(),
    };
    let mut changed = false;
    for k in keys {
        if let Some(v) = tab.files.get_mut(&k) {
            let before = v.len();
            v.retain(|l| l.owner != owner);
            changed |= v.len() != before;
            if v.is_empty() {
                tab.files.remove(&k);
            }
        }
    }
    tab.blocked_on.remove(&owner);
    if changed {
        store_count(&tab);
        drop(tab);
        WAITERS.wake_all();
    }
}

/// Le processus `pid` ferme un descripteur de `f` : ses verrous POSIX sur ce fichier tombent.
pub(crate) fn on_fd_close(pid: usize, f: &OpenFile) {
    if POSIX_LOCKS.load(Ordering::Acquire) == 0 || !matches!(f.node, crate::fd::Node::File { .. }) {
        return;
    }
    if let Some(key) = crate::flock::key_of(f) {
        release(Owner::Posix(pid), Some(key));
    }
}

/// Fin du processus `pid` : tous ses verrous POSIX tombent.
pub(crate) fn on_exit(pid: usize) {
    if POSIX_LOCKS.load(Ordering::Acquire) != 0 {
        release(Owner::Posix(pid), None);
    }
}

/// Dernière fermeture d'une description : ses verrous OFD tombent.
pub(crate) fn on_description_close(f: &OpenFile) {
    if f.reclocked.load(Ordering::Acquire) {
        release(Owner::Ofd(f as *const OpenFile as usize), None);
    }
}
