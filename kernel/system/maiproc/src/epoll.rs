//! `epoll` : surveillance d'un grand nombre de descripteurs en O(prêts).
//!
//! Même architecture que Linux (`fs/eventpoll.c`) : chaque entrée surveillée ([`EpItem`]) est
//! un observateur permanent des files d'attente du fichier ; un réveil de ces files la met dans
//! la liste des prêts de l'`epoll` (une seule fois, drapeau `queued`) et réveille les fils dans
//! `epoll_wait`. `epoll_wait` ne parcourt donc que les entrées prêtes, jamais toutes les
//! entrées : le coût ne dépend pas du nombre de descripteurs surveillés.
//!
//! - Déclenchement par niveau (par défaut) : une entrée encore prête après avoir été rapportée
//!   est remise en fin de liste (elle sera revérifiée au prochain appel).
//! - Par front (`EPOLLET`) : rapportée une fois par réveil du fichier.
//! - `EPOLLONESHOT` : désactivée après un rapport, jusqu'au prochain `EPOLL_CTL_MOD`.
//! - Une entrée vit tant que la description de fichier ouverte existe (pas le descripteur) :
//!   à la dernière fermeture, [`crate::fd::OpenFile`] la retire (`eventpoll_release`).
//! - Un `epoll` peut en surveiller un autre ; les boucles et les imbrications de plus de 4
//!   niveaux sont refusées (`ELOOP`), comme sous Linux.

use alloc::{
    collections::{BTreeMap, VecDeque},
    format,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use sync_irq::IrqSafeMutex;
use sync_preemption::PreemptionSafeMutex as Mutex;

use crate::fd::{Node, OpenFile, O_RDWR};
use crate::linux::errno::*;
use crate::linux::fs::get_fd;
use crate::linux::{fault, SysResult};
use crate::object::{ev, new_ino, FileObject};
use crate::poll::{for_each_queue, poll_file};
use crate::wait::{wait_event, WaitError, WaitQueue, Watcher};
use crate::Thread;

pub const EPOLLEXCLUSIVE: u32 = 1 << 28;
pub const EPOLLWAKEUP: u32 = 1 << 29;
pub const EPOLLONESHOT: u32 = 1 << 30;
pub const EPOLLET: u32 = 1 << 31;
/// Bits de comportement (pas des événements).
const FLAGS: u32 = EPOLLEXCLUSIVE | EPOLLWAKEUP | EPOLLONESHOT | EPOLLET;

const EPOLL_CTL_ADD: u64 = 1;
const EPOLL_CTL_DEL: u64 = 2;
const EPOLL_CTL_MOD: u64 = 3;
const EPOLL_CLOEXEC: u64 = 0x80000;

/// Profondeur maximale d'imbrication (`EP_MAX_NESTS`).
const MAX_NESTS: usize = 4;
/// Taille d'un `struct epoll_event` (compacté sur x86_64).
const EVENT_SIZE: usize = 12;

/// Instance `epoll`.
pub struct Epoll {
    /// Entrées par (descripteur, description de fichier).
    items: Mutex<BTreeMap<(i32, usize), Arc<EpItem>>>,
    /// Entrées à vérifier (prêtes ou peut-être prêtes).
    ready: IrqSafeMutex<VecDeque<Arc<EpItem>>>,
    /// Fils dans `epoll_wait`.
    wq: WaitQueue,
    /// Qui surveille cet `epoll` lui-même (`poll`, autre `epoll`).
    pollers: WaitQueue,
    ino: u64,
}

/// Entrée surveillée.
pub struct EpItem {
    ep: Weak<Epoll>,
    fd: i32,
    file: Weak<OpenFile>,
    key: usize,
    events: AtomicU32,
    data: AtomicU64,
    /// Déjà dans la liste des prêts.
    queued: AtomicBool,
    /// Retirée (`EPOLL_CTL_DEL` ou fermeture) : ignorée si elle traîne encore dans la liste.
    removed: AtomicBool,
}

impl Watcher for EpItem {
    fn notify(self: Arc<Self>) {
        if self.removed.load(Ordering::Acquire) || self.events.load(Ordering::Acquire) & !FLAGS == 0 {
            return;
        }
        let Some(ep) = self.ep.upgrade() else { return };
        if !self.queued.swap(true, Ordering::AcqRel) {
            ep.ready.lock().push_back(self.clone());
        }
        ep.wq.wake_all();
        ep.pollers.wake_all();
    }
}

impl EpItem {
    fn as_watcher(self: &Arc<Self>) -> Arc<dyn Watcher> {
        self.clone()
    }

    /// Retrait à la dernière fermeture de la description `file` (appelé par son `Drop`).
    pub(crate) fn detach(self: &Arc<Self>, file: &OpenFile) {
        self.removed.store(true, Ordering::Release);
        let w = self.as_watcher();
        for_each_queue(file, &mut |q| q.unwatch(&w));
        if let Some(ep) = self.ep.upgrade() {
            ep.items.lock().remove(&(self.fd, self.key));
        }
    }
}

impl Epoll {
    pub fn new() -> Arc<Epoll> {
        Arc::new(Epoll {
            items: Mutex::new(BTreeMap::new()),
            ready: IrqSafeMutex::new(VecDeque::new()),
            wq: WaitQueue::new(),
            pollers: WaitQueue::new(),
            ino: new_ino(),
        })
    }

    /// Vrai si `target` est atteignable depuis `from` en suivant les `epoll` surveillés
    /// (`depth` : niveau courant). Erreur `ELOOP` au-delà de [`MAX_NESTS`].
    fn reaches(from: &Epoll, target: *const Epoll, depth: usize) -> Result<bool, i64> {
        if depth > MAX_NESTS {
            return Err(ELOOP);
        }
        let nested: Vec<Arc<OpenFile>> = from.items.lock().values().filter_map(|it| it.file.upgrade()).collect();
        for f in nested {
            if let Some(inner) = f.object_as::<Epoll>() {
                if core::ptr::eq(inner, target) || Self::reaches(inner, target, depth + 1)? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn add(self: &Arc<Self>, fd: i32, file: &Arc<OpenFile>, events: u32, data: u64) -> SysResult {
        let key = (fd, Arc::as_ptr(file) as usize);
        if let Some(inner) = file.object_as::<Epoll>() {
            if core::ptr::eq(inner, Arc::as_ptr(self)) || Self::reaches(inner, Arc::as_ptr(self), 1)? {
                return Err(if core::ptr::eq(inner, Arc::as_ptr(self)) { EINVAL } else { ELOOP });
            }
        }
        let item = Arc::new(EpItem {
            ep: Arc::downgrade(self),
            fd,
            file: Arc::downgrade(file),
            key: key.1,
            events: AtomicU32::new(events | ev::ERR | ev::HUP),
            data: AtomicU64::new(data),
            queued: AtomicBool::new(false),
            removed: AtomicBool::new(false),
        });
        {
            let mut items = self.items.lock();
            if items.contains_key(&key) {
                return Err(EEXIST);
            }
            items.insert(key, item.clone());
        }
        file.epitems.lock().push(Arc::downgrade(&item));
        let w = item.as_watcher();
        for_each_queue(file, &mut |q| q.watch(w.clone()));
        // Déjà prêt : rapporté au prochain `epoll_wait`.
        if poll_file(file, None) & (events | ev::ERR | ev::HUP) != 0 {
            item.notify();
        }
        Ok(0)
    }

    fn del(&self, fd: i32, file: &Arc<OpenFile>) -> SysResult {
        let key = (fd, Arc::as_ptr(file) as usize);
        let item = self.items.lock().remove(&key).ok_or(ENOENT)?;
        item.removed.store(true, Ordering::Release);
        let w = item.as_watcher();
        for_each_queue(file, &mut |q| q.unwatch(&w));
        let me = Arc::as_ptr(&item);
        file.epitems.lock().retain(|x| x.as_ptr() != me);
        Ok(0)
    }

    fn modify(&self, fd: i32, file: &Arc<OpenFile>, events: u32, data: u64) -> SysResult {
        let key = (fd, Arc::as_ptr(file) as usize);
        let item = self.items.lock().get(&key).cloned().ok_or(ENOENT)?;
        if events & EPOLLEXCLUSIVE != 0 || item.events.load(Ordering::Acquire) & EPOLLEXCLUSIVE != 0 {
            return Err(EINVAL);
        }
        item.data.store(data, Ordering::Release);
        item.events.store(events | ev::ERR | ev::HUP, Ordering::Release);
        if poll_file(file, None) & (events | ev::ERR | ev::HUP) != 0 {
            item.notify();
        }
        Ok(0)
    }

    /// Relève au plus `max` événements prêts (sans attendre).
    fn harvest(&self, t: &Thread, max: usize) -> Vec<(u32, u64)> {
        let mut out = Vec::new();
        let pending = self.ready.lock().len();
        for _ in 0..pending {
            if out.len() >= max {
                break;
            }
            let Some(item) = self.ready.lock().pop_front() else { break };
            // Avant de vérifier : un réveil qui arrive maintenant la remettra en liste.
            item.queued.store(false, Ordering::Release);
            if item.removed.load(Ordering::Acquire) {
                continue;
            }
            let Some(file) = item.file.upgrade() else { continue };
            let events = item.events.load(Ordering::Acquire);
            if events & !FLAGS == 0 {
                continue; // EPOLLONESHOT déjà rapporté
            }
            let rev = poll_file(&file, Some(t)) & events & !FLAGS;
            if rev == 0 {
                continue;
            }
            out.push((rev, item.data.load(Ordering::Acquire)));
            if events & EPOLLONESHOT != 0 {
                item.events.store(events & FLAGS, Ordering::Release);
            } else if events & EPOLLET == 0 && !item.queued.swap(true, Ordering::AcqRel) {
                // Niveau : revérifiée au prochain appel.
                self.ready.lock().push_back(item);
            }
        }
        out
    }

    /// Nombre d'entrées surveillées.
    pub fn len(&self) -> usize {
        self.items.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl FileObject for Epoll {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn link_name(&self) -> String {
        String::from("anon_inode:[eventpoll]")
    }

    fn stat(&self) -> (u32, u64) {
        (0o600, self.ino)
    }

    fn poll(&self, _t: Option<&Thread>) -> u32 {
        // Approximation de Linux (qui revérifie chaque entrée) : une entrée en liste rend
        // l'`epoll` lisible ; `epoll_wait` dira s'il y a vraiment quelque chose.
        if self.ready.lock().iter().any(|i| !i.removed.load(Ordering::Relaxed)) {
            ev::READABLE
        } else {
            0
        }
    }

    fn queues<'a>(&'a self, cb: &mut dyn FnMut(&'a WaitQueue)) {
        cb(&self.pollers);
    }

    fn fdinfo(&self) -> String {
        let mut s = String::new();
        for it in self.items.lock().values() {
            s.push_str(&format!(
                "tfd: {:8} events: {:8x} data: {:16x}\n",
                it.fd,
                it.events.load(Ordering::Relaxed),
                it.data.load(Ordering::Relaxed)
            ));
        }
        s
    }
}

// --- Appels système ----------------------------------------------------------------------------

pub fn epoll_create1(t: &Thread, flags: u64) -> SysResult {
    if flags & !EPOLL_CLOEXEC != 0 {
        return Err(EINVAL);
    }
    let file = OpenFile::new(Node::Object(Epoll::new()), O_RDWR);
    t.process.files.lock().alloc(file, 0, flags & EPOLL_CLOEXEC != 0).map(|fd| fd as i64).ok_or(EMFILE)
}

pub fn epoll_create(t: &Thread, size: u64) -> SysResult {
    if size as i32 <= 0 {
        return Err(EINVAL);
    }
    epoll_create1(t, 0)
}

fn epoll_of(t: &Thread, epfd: u64) -> Result<(Arc<OpenFile>, Arc<dyn FileObject>), i64> {
    let f = get_fd(t, epfd)?;
    let o = f.object().cloned().ok_or(EINVAL)?;
    if crate::object::downcast::<Epoll>(&*o).is_none() {
        return Err(EINVAL);
    }
    Ok((f, o))
}

pub fn epoll_ctl(t: &Thread, epfd: u64, op: u64, fd: u64, event: u64) -> SysResult {
    let (_ef, obj) = epoll_of(t, epfd)?;
    let ep = crate::object::downcast::<Epoll>(&*obj).ok_or(EINVAL)?;
    let target = get_fd(t, fd)?;
    if epfd == fd {
        return Err(EINVAL);
    }
    // Fichiers ordinaires et répertoires : toujours prêts, refusés par Linux (`EPERM`).
    if matches!(target.node, Node::File { .. } | Node::Dir { .. } | Node::Generated { .. }) {
        return Err(EPERM);
    }
    let (events, data) = if op == EPOLL_CTL_DEL {
        (0, 0)
    } else {
        let mut raw = [0u8; EVENT_SIZE];
        t.mm().copy_from_user(&mut raw, event as usize).map_err(fault)?;
        (u32::from_le_bytes(raw[0..4].try_into().unwrap()), u64::from_le_bytes(raw[4..12].try_into().unwrap()))
    };
    // `Arc<Epoll>` pour l'ajout (les entrées gardent une référence faible vers l'instance).
    let me = crate::object::downcast_arc::<Epoll>(&obj).ok_or(EINVAL)?;
    let fd = fd as i32;
    match op {
        EPOLL_CTL_ADD => {
            if events & EPOLLEXCLUSIVE != 0 && events & !(EPOLLEXCLUSIVE | EPOLLET | EPOLLWAKEUP | ev::IN | ev::OUT | ev::ERR | ev::HUP) != 0 {
                return Err(EINVAL);
            }
            me.add(fd, &target, events, data)
        }
        EPOLL_CTL_DEL => ep.del(fd, &target),
        EPOLL_CTL_MOD => ep.modify(fd, &target, events, data),
        _ => Err(EINVAL),
    }
}

/// `epoll_wait`, `epoll_pwait`, `epoll_pwait2` (délai `None` : sans limite).
pub fn epoll_wait(t: &Thread, epfd: u64, events: u64, maxevents: u64, timeout: Option<core::time::Duration>) -> SysResult {
    let max = maxevents as i32;
    if max <= 0 || max as usize > i32::MAX as usize / EVENT_SIZE {
        return Err(EINVAL);
    }
    let (_ef, obj) = epoll_of(t, epfd)?;
    let ep = crate::object::downcast::<Epoll>(&*obj).ok_or(EINVAL)?;
    let max = max as usize;
    let got = if timeout == Some(core::time::Duration::ZERO) {
        ep.harvest(t, max)
    } else {
        let deadline = timeout.map(|d| time::Instant::now() + d);
        match wait_event(&[&ep.wq], deadline, Some(t), || {
            let v = ep.harvest(t, max);
            (!v.is_empty()).then_some(v)
        }) {
            Ok(v) => v,
            Err(WaitError::TimedOut) => Vec::new(),
            // Jamais relancé, même avec `SA_RESTART` (comme Linux).
            Err(WaitError::Interrupted) => return Err(crate::signal::ERESTARTNOHAND),
        }
    };
    let mut raw = Vec::with_capacity(got.len() * EVENT_SIZE);
    for (e, d) in &got {
        raw.extend_from_slice(&e.to_le_bytes());
        raw.extend_from_slice(&d.to_le_bytes());
    }
    t.mm().copy_to_user(events as usize, &raw).map_err(fault)?;
    Ok(got.len() as i64)
}
