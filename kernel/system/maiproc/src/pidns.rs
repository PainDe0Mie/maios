//! Espaces de noms de PID (`pid_namespaces(7)`), sur le modèle de `struct pid` de Linux
//! (`kernel/pid.c`, `kernel/pid_namespace.c`).
//!
//! Le noyau ne manipule que des numéros **globaux** : l'identifiant de la tâche du fil, qui est
//! aussi son numéro dans l'espace initial. Chaque espace descendant tient sa table « numéro local
//! ↔ numéro global » (l'`idr` de Linux), remplie à la création d'un fil dans son espace et dans
//! chacun de ses ancêtres. Toute valeur de pid qui franchit la frontière (appels système,
//! `siginfo`, `/proc`, `SO_PEERCRED`, mot d'un futex…) est traduite pour l'espace de
//! l'observateur : [`PidNs::nr_of`] à la sortie (0 si le processus y est invisible, comme
//! `pid_nr_ns`), [`PidNs::global_of`] à l'entrée (`find_vpid`). Dans l'espace initial les deux
//! sont l'identité, sans verrou.
//!
//! Durée de vie d'un numéro : un [`Pid`] est tenu (`Arc`) par tout ce qui s'y attache — le fil
//! (son tid), le processus (son pid), les membres d'un groupe (pgid) et d'une session (sid) —
//! comme les `pid_links` de Linux. Le dernier détachement rend les numéros (`free_pid`) : un
//! groupe de processus garde son numéro tant qu'il a des membres, même après la fin de son chef.
//!
//! Le premier processus d'un espace (numéro 1) en est l'init (`child_reaper`) : il recueille
//! les orphelins de l'espace, ne reçoit de ses voisins que les signaux pour lesquels il a un
//! gestionnaire (`SIGNAL_UNKILLABLE`), et sa fin tue tout l'espace (`zap_pid_ns_processes`,
//! dans `lib.rs`), qui ne prend alors plus aucun nouveau processus (`ENOMEM`).

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use sync_irq::IrqSafeMutex;

use crate::linux::errno::*;
use crate::userns::{self, UserNs};
use crate::wait::WaitQueue;
use crate::Process;

/// Profondeur maximale (`MAX_PID_NS_LEVEL`).
pub const MAX_PID_NS_LEVEL: u32 = 32;
/// Borne des numéros locaux (`pid_max` des systèmes 64 bits récents).
pub const PID_MAX: u32 = 4_194_304;
/// Après le premier tour, les numéros recommencent ici (`RESERVED_PIDS`).
const RESERVED_PIDS: u32 = 300;

#[derive(Default)]
struct PidMap {
    to_global: BTreeMap<u32, usize>,
    to_local: BTreeMap<usize, u32>,
    /// Dernier numéro alloué par le tour cyclique (`ns_last_pid`).
    last: u32,
}

/// Espace de noms de PID.
pub struct PidNs {
    pub ino: u64,
    /// Profondeur (0 : espace initial).
    pub level: u32,
    pub parent: Option<Arc<PidNs>>,
    /// Espace utilisateur propriétaire (droits de `setns`, de monter `proc`…).
    pub user_ns: Arc<UserNs>,
    map: IrqSafeMutex<PidMap>,
    /// Init de l'espace (`child_reaper`) ; vide pour l'espace initial (MaiOS n'a pas de
    /// processus 1 : ses orphelins sont recueillis par le noyau).
    reaper: spin::Mutex<Weak<Process>>,
    /// L'init est mort : plus d'allocation (`PIDNS_ADDING` retiré).
    dead: AtomicBool,
    /// Réveillée à chaque numéro rendu et à chaque processus recueilli (l'init qui meurt attend
    /// la fin de tout l'espace).
    pub wait: WaitQueue,
}

static INIT: spin::Once<Arc<PidNs>> = spin::Once::new();
/// Dernier numéro global attribué (`ns_last_pid` de l'espace initial).
static LAST_GLOBAL: AtomicUsize = AtomicUsize::new(0);

/// Espace initial.
pub fn init_pid_ns() -> Arc<PidNs> {
    INIT.call_once(|| Arc::new(PidNs::make(userns::INIT_PID_INO, 0, None, userns::init_user_ns()))).clone()
}

impl PidNs {
    fn make(ino: u64, level: u32, parent: Option<Arc<PidNs>>, user_ns: Arc<UserNs>) -> PidNs {
        PidNs {
            ino,
            level,
            parent,
            user_ns,
            map: IrqSafeMutex::new(PidMap::default()),
            reaper: spin::Mutex::new(Weak::new()),
            dead: AtomicBool::new(false),
            wait: WaitQueue::new(),
        }
    }

    /// Nouvel espace enfant de `parent`, possédé par `user_ns` (`create_pid_namespace`).
    pub fn new_child(parent: &Arc<PidNs>, user_ns: &Arc<UserNs>) -> Result<Arc<PidNs>, i64> {
        if parent.level >= MAX_PID_NS_LEVEL {
            return Err(ENOSPC);
        }
        // L'espace utilisateur du créateur doit descendre de celui du parent (`in_userns`).
        if !parent.user_ns.is_ancestor_of(user_ns) {
            return Err(EPERM);
        }
        Ok(Arc::new(PidNs::make(userns::alloc_ns_ino(), parent.level + 1, Some(parent.clone()), user_ns.clone())))
    }

    /// Numéro de `global` vu de cet espace ; 0 s'il y est invisible (`pid_nr_ns`).
    pub fn nr_of(&self, global: usize) -> usize {
        if global == 0 {
            return 0;
        }
        if self.level == 0 {
            return global;
        }
        self.map.lock().to_local.get(&global).map_or(0, |&n| n as usize)
    }

    /// Numéro global du numéro `nr` de cet espace (`find_vpid`).
    pub fn global_of(&self, nr: usize) -> Option<usize> {
        if nr == 0 || nr > i32::MAX as usize {
            return None;
        }
        if self.level == 0 {
            return Some(nr);
        }
        self.map.lock().to_global.get(&(nr as u32)).copied()
    }

    /// `global` a un numéro dans cet espace.
    pub fn sees(&self, global: usize) -> bool {
        self.nr_of(global) != 0
    }

    /// Numéros attribués dans un espace descendant (local, global), croissants. Vide pour
    /// l'espace initial (parcourir alors la table des processus).
    pub fn numbers(&self) -> Vec<(u32, usize)> {
        if self.level == 0 {
            return Vec::new();
        }
        self.map.lock().to_global.iter().map(|(&l, &g)| (l, g)).collect()
    }

    /// `self` est `other` ou l'un de ses ancêtres.
    pub fn is_ancestor_of(&self, other: &PidNs) -> bool {
        let mut cur: &PidNs = other;
        loop {
            if core::ptr::eq(cur, self) {
                return true;
            }
            if cur.level <= self.level {
                return false;
            }
            match &cur.parent {
                Some(p) => cur = p,
                None => return false,
            }
        }
    }

    /// Init de l'espace (`child_reaper`), s'il vit encore.
    pub fn reaper(&self) -> Option<Arc<Process>> {
        self.reaper.lock().upgrade()
    }

    pub(crate) fn set_reaper(&self, p: &Arc<Process>) {
        *self.reaper.lock() = Arc::downgrade(p);
    }

    /// L'init est mort : l'espace ne prend plus de processus.
    pub(crate) fn disable(&self) {
        self.dead.store(true, Ordering::Release);
    }

    /// L'espace et tous ses ancêtres prennent encore des processus (vérifié avant de créer la
    /// tâche d'un `fork` ; [`Pid::alloc`] le revérifie sous le verrou).
    pub fn accepting(&self) -> bool {
        let mut cur: &PidNs = self;
        loop {
            if cur.dead.load(Ordering::Acquire) {
                return false;
            }
            match &cur.parent {
                Some(p) => cur = p,
                None => return true,
            }
        }
    }

    /// `/proc/sys/kernel/ns_last_pid`.
    pub fn last_pid(&self) -> usize {
        if self.level == 0 {
            return LAST_GLOBAL.load(Ordering::Relaxed);
        }
        self.map.lock().last as usize
    }

    /// Écriture de `ns_last_pid` (CRIU) : le prochain numéro cyclique suit `v`. Dans l'espace
    /// initial, les numéros sont ceux des tâches : rien à régler (`EINVAL`).
    pub fn set_last_pid(&self, v: u32) -> Result<(), i64> {
        if self.level == 0 || v >= PID_MAX {
            return Err(EINVAL);
        }
        self.map.lock().last = v;
        Ok(())
    }

    /// Alloue un numéro local pour `global` : `want` (`clone3` avec `set_tid`) ou le suivant du
    /// tour cyclique (`idr_alloc_cyclic` de `pid_min` à `pid_max`).
    fn alloc_local(&self, want: Option<u32>, global: usize) -> Result<u32, i64> {
        let mut m = self.map.lock();
        if self.dead.load(Ordering::Acquire) {
            return Err(ENOMEM);
        }
        let local = match want {
            Some(w) => {
                if w == 0 || w >= PID_MAX {
                    return Err(EINVAL);
                }
                if m.to_global.contains_key(&w) {
                    return Err(EEXIST);
                }
                w
            }
            None => {
                let min = if m.last >= RESERVED_PIDS { RESERVED_PIDS } else { 1 };
                let start = m.last.saturating_add(1).max(min);
                let first_free = |m: &PidMap, from: u32, to: u32| -> Option<u32> {
                    let mut c = from;
                    for (&k, _) in m.to_global.range(from..to) {
                        if k != c {
                            break;
                        }
                        c += 1;
                    }
                    (c < to).then_some(c)
                };
                let n = first_free(&m, start, PID_MAX).or_else(|| first_free(&m, min, start)).ok_or(EAGAIN)?;
                m.last = n;
                n
            }
        };
        m.to_global.insert(local, global);
        m.to_local.insert(global, local);
        Ok(local)
    }

    fn release(&self, local: u32, global: usize) {
        {
            let mut m = self.map.lock();
            if m.to_global.get(&local) == Some(&global) {
                m.to_global.remove(&local);
            }
            if m.to_local.get(&global) == Some(&local) {
                m.to_local.remove(&global);
            }
        }
        self.wait.wake_all();
    }
}

/// Numéros d'un fil ou d'un processus dans son espace et ses ancêtres (`struct pid`).
pub struct Pid {
    /// Numéro global (identifiant de la tâche).
    pub nr: usize,
    /// Espace le plus profond (celui du processus).
    pub ns: Arc<PidNs>,
    /// Numéros locaux : `nrs[0]` dans `ns`, `nrs[1]` dans son parent… jusqu'au niveau 1.
    nrs: Vec<u32>,
}

impl Pid {
    /// Attribue à `nr` un numéro dans `ns` et dans chacun de ses ancêtres (`alloc_pid`).
    /// `set_tid[0]` est le numéro voulu dans `ns`, `set_tid[1]` dans son parent… ; au niveau 0,
    /// le numéro est celui de la tâche (`EEXIST` si un autre est demandé).
    pub fn alloc(nr: usize, ns: &Arc<PidNs>, set_tid: &[u32]) -> Result<Arc<Pid>, i64> {
        if set_tid.len() > ns.level as usize + 1 {
            return Err(EINVAL);
        }
        let mut pid = Pid { nr, ns: ns.clone(), nrs: Vec::with_capacity(ns.level as usize) };
        let mut cur: &Arc<PidNs> = ns;
        let mut i = 0;
        loop {
            let want = set_tid.get(i).copied();
            if cur.level == 0 {
                if want.map_or(false, |w| w as usize != nr) {
                    return Err(EEXIST); // `pid` rend ce qui a été pris
                }
                break;
            }
            pid.nrs.push(cur.alloc_local(want, nr)?);
            i += 1;
            cur = cur.parent.as_ref().expect("pidns : espace sans parent");
        }
        LAST_GLOBAL.fetch_max(nr, Ordering::Relaxed);
        Ok(Arc::new(pid))
    }

    /// Numéro dans son propre espace (`pid_vnr` vu de lui-même).
    pub fn vnr(&self) -> usize {
        self.nrs.first().map_or(self.nr, |&n| n as usize)
    }

    /// Numéros depuis l'espace `from` jusqu'au plus profond (`NSpid:` de `/proc/<pid>/status`) ;
    /// vide si `from` ne voit pas ce pid.
    pub fn numbers_from(&self, from: &PidNs) -> Vec<usize> {
        if !from.is_ancestor_of(&self.ns) {
            return Vec::new();
        }
        (from.level..=self.ns.level).map(|l| self.nr_at(l)).collect()
    }

    fn nr_at(&self, level: u32) -> usize {
        if level == 0 {
            self.nr
        } else {
            self.nrs[(self.ns.level - level) as usize] as usize
        }
    }
}

impl Drop for Pid {
    fn drop(&mut self) {
        let mut cur: &Arc<PidNs> = &self.ns;
        for &l in &self.nrs {
            cur.release(l, self.nr);
            match &cur.parent {
                Some(p) => cur = p,
                None => break,
            }
        }
    }
}
