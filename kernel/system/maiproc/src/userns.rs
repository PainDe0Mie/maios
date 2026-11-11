//! Espaces de noms utilisateur (`user_namespaces(7)`) et capacités (`capabilities(7)`).
//!
//! Comme Linux, les identités tenues par le noyau (`Creds`, propriétaires des fichiers, IPC)
//! sont des identités **globales** (`kuid_t`/`kgid_t`) ; un espace utilisateur ne fait que les
//! traduire à la frontière des appels système ([`UserNs::make_kuid`] à l'entrée,
//! [`UserNs::from_kuid_munged`] à la sortie, 65534 pour une identité sans correspondance).
//!
//! - Cartes `uid_map`/`gid_map` : écrites **une fois** (`spin::Once`, lues sans verrou), 340
//!   tranches au plus, sans recouvrement ni débordement, bornes basses traduites par la carte de
//!   l'espace parent au moment de l'écriture (`map_write` de `kernel/user_namespace.c`).
//! - Droit d'écrire une carte (`new_idmap_permitted`) : l'ouvreur est dans l'espace visé ou dans
//!   son parent et tient `CAP_SYS_ADMIN` sur l'espace visé ; une seule tranche de longueur 1 qui
//!   projette son propre uid effectif (et, pour les groupes, après `setgroups` = `deny`) est
//!   permise sans privilège ; sinon `CAP_SETUID`/`CAP_SETGID` dans l'espace parent.
//! - Capacités (`cap_capable`) : effectives dans l'espace des identités ; le propriétaire d'un
//!   espace enfant (uid effectif = créateur, dans l'espace parent) les a toutes dans cet enfant
//!   et ses descendants.

use alloc::{format, string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::creds::Creds;
use crate::linux::errno::{EINVAL, EPERM, EUSERS};
use crate::Thread;

/// `overflowuid` / `overflowgid`.
pub const OVERFLOW_ID: u32 = 65534;
/// Profondeur maximale d'imbrication (`MAX_USER_NS_LEVEL`).
pub const MAX_LEVEL: u32 = 32;
/// Tranches d'une carte (`UID_GID_MAP_MAX_EXTENTS`).
pub const MAX_EXTENTS: usize = 340;

// --- Capacités ---------------------------------------------------------------------------------

pub const CAP_CHOWN: u32 = 0;
pub const CAP_DAC_OVERRIDE: u32 = 1;
pub const CAP_DAC_READ_SEARCH: u32 = 2;
pub const CAP_FOWNER: u32 = 3;
pub const CAP_FSETID: u32 = 4;
pub const CAP_KILL: u32 = 5;
pub const CAP_SETGID: u32 = 6;
pub const CAP_SETUID: u32 = 7;
pub const CAP_SETPCAP: u32 = 8;
pub const CAP_LINUX_IMMUTABLE: u32 = 9;
pub const CAP_NET_BIND_SERVICE: u32 = 10;
pub const CAP_NET_ADMIN: u32 = 12;
pub const CAP_IPC_OWNER: u32 = 15;
pub const CAP_SYS_CHROOT: u32 = 18;
pub const CAP_SYS_PTRACE: u32 = 19;
pub const CAP_SYS_ADMIN: u32 = 21;
pub const CAP_SYS_BOOT: u32 = 22;
pub const CAP_SYS_NICE: u32 = 23;
pub const CAP_SYS_RESOURCE: u32 = 24;
pub const CAP_SYS_TIME: u32 = 25;
pub const CAP_MKNOD: u32 = 27;
pub const CAP_SETFCAP: u32 = 31;
pub const CAP_MAC_OVERRIDE: u32 = 32;
/// Restauration de processus (`set_tid`, `ns_last_pid` ; Linux 5.9).
pub const CAP_CHECKPOINT_RESTORE: u32 = 40;
/// Dernière capacité connue (`/proc/sys/kernel/cap_last_cap`, Linux 5.9+).
pub const CAP_LAST_CAP: u32 = 40;
/// Toutes les capacités.
pub const CAP_FULL: u64 = (1u64 << (CAP_LAST_CAP + 1)) - 1;
/// Capacités de système de fichiers, suivies par l'uid de système de fichiers (`CAP_FS_MASK`).
pub const CAP_FS_MASK: u64 = (1 << CAP_CHOWN)
    | (1 << CAP_DAC_OVERRIDE)
    | (1 << CAP_DAC_READ_SEARCH)
    | (1 << CAP_FOWNER)
    | (1 << CAP_FSETID)
    | (1 << CAP_LINUX_IMMUTABLE)
    | (1 << CAP_MKNOD)
    | (1 << CAP_MAC_OVERRIDE);

#[inline]
pub const fn bit(cap: u32) -> u64 {
    1u64 << cap
}

pub fn cap_valid(cap: u32) -> bool {
    cap <= CAP_LAST_CAP
}

// --- Espaces de noms ----------------------------------------------------------------------------

/// Numéros d'inode des espaces initiaux (ceux de Linux, `PROC_*_INIT_INO`).
pub const INIT_IPC_INO: u64 = 0xEFFF_FFFF;
pub const INIT_UTS_INO: u64 = 0xEFFF_FFFE;
pub const INIT_USER_INO: u64 = 0xEFFF_FFFD;
pub const INIT_PID_INO: u64 = 0xEFFF_FFFC;
pub const INIT_CGROUP_INO: u64 = 0xEFFF_FFFB;
pub const INIT_TIME_INO: u64 = 0xEFFF_FFFA;
pub const INIT_NET_INO: u64 = 0xF000_0000;
pub const INIT_MNT_INO: u64 = 0xF000_0001;

static NEXT_INO: AtomicU64 = AtomicU64::new(0xF000_0100);

/// Numéro d'inode d'un nouvel espace de noms (`proc_alloc_inum`).
pub fn alloc_ns_ino() -> u64 {
    NEXT_INO.fetch_add(1, Ordering::Relaxed)
}

/// Tranche d'une carte : `[first, first + count)` dans l'espace → `[lower, lower + count)` en
/// identités globales.
#[derive(Clone, Copy, Debug)]
pub struct Extent {
    pub first: u32,
    pub lower: u32,
    pub count: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MapKind {
    Uid,
    Gid,
}

pub struct UserNs {
    pub ino: u64,
    pub parent: Option<Arc<UserNs>>,
    pub level: u32,
    /// Créateur (identités globales) : il a toutes les capacités dans l'espace.
    pub owner: u32,
    pub group: u32,
    uid_map: spin::Once<Vec<Extent>>,
    gid_map: spin::Once<Vec<Extent>>,
    /// `setgroups` permis (`/proc/<pid>/setgroups` = `allow`), hérité du parent.
    setgroups_allowed: AtomicBool,
    /// Sérialise les écritures des cartes et de `setgroups` (`userns_state_mutex`).
    write_lock: spin::Mutex<()>,
}

static INIT: spin::Once<Arc<UserNs>> = spin::Once::new();

/// Espace utilisateur initial : cartes identité sur tout l'intervalle.
pub fn init_user_ns() -> Arc<UserNs> {
    INIT.call_once(|| {
        let full = alloc::vec![Extent { first: 0, lower: 0, count: u32::MAX }];
        let ns = UserNs {
            ino: INIT_USER_INO,
            parent: None,
            level: 0,
            owner: 0,
            group: 0,
            uid_map: spin::Once::new(),
            gid_map: spin::Once::new(),
            setgroups_allowed: AtomicBool::new(true),
            write_lock: spin::Mutex::new(()),
        };
        ns.uid_map.call_once(|| full.clone());
        ns.gid_map.call_once(|| full);
        Arc::new(ns)
    })
    .clone()
}

fn map_down(map: &[Extent], id: u32, count: u32) -> Option<u32> {
    let last = id.checked_add(count - 1)?;
    map.iter()
        .find(|e| id >= e.first && last <= e.first.wrapping_add(e.count - 1) && last >= e.first)
        .map(|e| e.lower + (id - e.first))
}

fn map_up(map: &[Extent], kid: u32) -> Option<u32> {
    map.iter().find(|e| kid >= e.lower && kid - e.lower < e.count).map(|e| e.first + (kid - e.lower))
}

impl UserNs {
    pub fn is_init(&self) -> bool {
        self.level == 0
    }

    fn map(&self, kind: MapKind) -> Option<&Vec<Extent>> {
        match kind {
            MapKind::Uid => self.uid_map.get(),
            MapKind::Gid => self.gid_map.get(),
        }
    }

    /// Identité de l'espace → identité globale (`None` : sans correspondance).
    pub fn make_kid(&self, kind: MapKind, id: u32) -> Option<u32> {
        if id == u32::MAX {
            return None;
        }
        map_down(self.map(kind)?, id, 1)
    }

    pub fn make_kuid(&self, id: u32) -> Option<u32> {
        self.make_kid(MapKind::Uid, id)
    }

    pub fn make_kgid(&self, id: u32) -> Option<u32> {
        self.make_kid(MapKind::Gid, id)
    }

    /// Identité globale → identité de l'espace.
    pub fn from_kid(&self, kind: MapKind, kid: u32) -> Option<u32> {
        map_up(self.map(kind)?, kid)
    }

    pub fn from_kuid_munged(&self, kid: u32) -> u32 {
        self.from_kid(MapKind::Uid, kid).unwrap_or(OVERFLOW_ID)
    }

    pub fn from_kgid_munged(&self, kid: u32) -> u32 {
        self.from_kid(MapKind::Gid, kid).unwrap_or(OVERFLOW_ID)
    }

    pub fn has_kuid(&self, kid: u32) -> bool {
        self.from_kid(MapKind::Uid, kid).is_some()
    }

    pub fn has_kgid(&self, kid: u32) -> bool {
        self.from_kid(MapKind::Gid, kid).is_some()
    }

    pub fn setgroups_allowed(&self) -> bool {
        self.setgroups_allowed.load(Ordering::Acquire)
    }

    /// `setgroups(2)` possible dans cet espace (`userns_may_setgroups`) : carte des groupes
    /// écrite et `setgroups` permis.
    pub fn may_setgroups(&self) -> bool {
        self.gid_map.get().is_some() && self.setgroups_allowed()
    }

    /// `self` est `other` ou un de ses ancêtres.
    pub fn is_ancestor_of(self: &Arc<Self>, other: &Arc<UserNs>) -> bool {
        let mut ns = other.clone();
        loop {
            if Arc::ptr_eq(self, &ns) {
                return true;
            }
            match ns.parent.clone() {
                Some(p) => ns = p,
                None => return false,
            }
        }
    }

    /// Nouvel espace enfant de `parent` créé par `creator` (`create_user_ns`).
    pub fn new_child(parent: &Arc<UserNs>, creator: &Creds) -> Result<Arc<UserNs>, i64> {
        if parent.level >= MAX_LEVEL {
            return Err(EUSERS);
        }
        // Le créateur doit avoir une identité dans l'espace parent.
        if !parent.has_kuid(creator.euid) || !parent.has_kgid(creator.egid) {
            return Err(EPERM);
        }
        Ok(Arc::new(UserNs {
            ino: alloc_ns_ino(),
            parent: Some(parent.clone()),
            level: parent.level + 1,
            owner: creator.euid,
            group: creator.egid,
            uid_map: spin::Once::new(),
            gid_map: spin::Once::new(),
            setgroups_allowed: AtomicBool::new(parent.setgroups_allowed()),
            write_lock: spin::Mutex::new(()),
        }))
    }

    /// Texte de `/proc/<pid>/{uid,gid}_map` lu par un processus de l'espace `reader`
    /// (`uid_m_show` : bornes basses vues de l'espace du lecteur, ou du parent si le lecteur
    /// est dans l'espace même).
    pub fn map_text(self: &Arc<Self>, kind: MapKind, reader: &Arc<UserNs>) -> String {
        let mut lower_ns = reader.clone();
        if Arc::ptr_eq(&lower_ns, self) {
            if let Some(p) = lower_ns.parent.clone() {
                lower_ns = p;
            }
        }
        let mut out = String::new();
        if let Some(map) = self.map(kind) {
            for e in map {
                let lower = lower_ns.from_kid(kind, e.lower).unwrap_or(u32::MAX);
                out.push_str(&format!("{:>10} {:>10} {:>10}\n", e.first, lower, e.count));
            }
        }
        out
    }

    /// Écriture de `/proc/<pid>/{uid,gid}_map` (`map_write`). `opener` : identités de qui a
    /// ouvert le fichier.
    pub fn write_map(self: &Arc<Self>, kind: MapKind, opener: &Creds, text: &[u8]) -> Result<(), i64> {
        let parent = self.parent.clone().ok_or(EPERM)?;
        // L'ouvreur est dans l'espace visé ou dans son parent.
        if !Arc::ptr_eq(&opener.user_ns, self) && !Arc::ptr_eq(&opener.user_ns, &parent) {
            return Err(EPERM);
        }
        let _g = self.write_lock.lock();
        if self.map(kind).is_some() {
            return Err(EPERM); // une seule écriture réussie
        }
        if !ns_capable(opener, self, CAP_SYS_ADMIN) {
            return Err(EPERM);
        }
        let text = core::str::from_utf8(text).map_err(|_| EINVAL)?;
        let mut map: Vec<Extent> = Vec::new();
        for line in text.split('\n') {
            if line.trim().is_empty() {
                continue;
            }
            let mut f = line.split_ascii_whitespace();
            let mut num = || -> Result<u32, i64> { f.next().ok_or(EINVAL)?.parse::<u32>().map_err(|_| EINVAL) };
            let e = Extent { first: num()?, lower: num()?, count: num()? };
            if f.next().is_some() {
                return Err(EINVAL);
            }
            // Ni longueur nulle, ni débordement, ni `-1` dans la tranche.
            if e.count == 0 || e.first.checked_add(e.count).is_none() || e.lower.checked_add(e.count).is_none() {
                return Err(EINVAL);
            }
            let overlap = |a: u32, b: u32, n1: u32, n2: u32| a < b + n2 && b < a + n1;
            if map.iter().any(|o| overlap(o.first, e.first, o.count, e.count) || overlap(o.lower, e.lower, o.count, e.count)) {
                return Err(EINVAL);
            }
            map.push(e);
            if map.len() > MAX_EXTENTS {
                return Err(EINVAL);
            }
        }
        if map.is_empty() {
            return Err(EINVAL);
        }
        // Droit d'utiliser les identités projetées (`new_idmap_permitted`).
        let cap = match kind {
            MapKind::Uid => CAP_SETUID,
            MapKind::Gid => CAP_SETGID,
        };
        let unprivileged_ok = map.len() == 1 && map[0].count == 1 && self.owner == opener.euid && {
            let id = map[0].lower;
            match kind {
                MapKind::Uid => parent.make_kuid(id) == Some(opener.euid),
                MapKind::Gid => !self.setgroups_allowed() && parent.make_kgid(id) == Some(opener.egid),
            }
        };
        if !unprivileged_ok && !ns_capable(opener, &parent, cap) {
            return Err(EPERM);
        }
        // Bornes basses : identités du parent → identités globales.
        let pmap = parent.map(kind).ok_or(EPERM)?;
        for e in map.iter_mut() {
            e.lower = map_down(pmap, e.lower, e.count).ok_or(EPERM)?;
        }
        match kind {
            MapKind::Uid => self.uid_map.call_once(|| map),
            MapKind::Gid => self.gid_map.call_once(|| map),
        };
        Ok(())
    }

    /// Écriture de `/proc/<pid>/setgroups` (`proc_setgroups_write`).
    pub fn write_setgroups(self: &Arc<Self>, opener: &Creds, text: &[u8]) -> Result<(), i64> {
        let word = core::str::from_utf8(text).map_err(|_| EINVAL)?.trim();
        let allow = match word {
            "allow" => true,
            "deny" => false,
            _ => return Err(EINVAL),
        };
        if !ns_capable(opener, self, CAP_SYS_ADMIN) {
            return Err(EPERM);
        }
        let _g = self.write_lock.lock();
        if allow {
            // Pas de retour à `allow` une fois `deny` posé.
            if !self.setgroups_allowed() {
                return Err(EPERM);
            }
        } else {
            // `deny` seulement avant la carte des groupes.
            if self.gid_map.get().is_some() {
                return Err(EPERM);
            }
            self.setgroups_allowed.store(false, Ordering::Release);
        }
        Ok(())
    }
}

/// `cap_capable` : `creds` tient `cap` dans l'espace `target`.
pub fn ns_capable(creds: &Creds, target: &Arc<UserNs>, cap: u32) -> bool {
    let mut ns = target.clone();
    loop {
        if Arc::ptr_eq(&ns, &creds.user_ns) {
            return creds.cap_eff & bit(cap) != 0;
        }
        // Un espace de niveau inférieur ou égal ne peut pas descendre de celui des identités.
        if ns.level <= creds.user_ns.level {
            return false;
        }
        let parent = ns.parent.clone().expect("espace utilisateur sans parent au-dessus du niveau 0");
        // Le créateur d'un espace enfant y a toutes les capacités.
        if Arc::ptr_eq(&parent, &creds.user_ns) && ns.owner == creds.euid {
            return true;
        }
        ns = parent;
    }
}

/// Capacité dans l'espace initial (`capable`).
pub fn capable(creds: &Creds, cap: u32) -> bool {
    ns_capable(creds, &init_user_ns(), cap)
}

/// Capacité du processus appelant dans l'espace initial.
pub fn current_capable(t: &Thread, cap: u32) -> bool {
    capable(&t.process.creds.lock(), cap)
}

/// Capacité du processus appelant dans son propre espace (`ns_capable(current_user_ns(), cap)`).
pub fn current_ns_capable(t: &Thread, cap: u32) -> bool {
    let c = t.process.creds.lock();
    c.cap_eff & bit(cap) != 0
}

// --- Fichiers de /proc/<pid>/ --------------------------------------------------------------------

/// `/proc/<pid>/uid_map`, `gid_map` et `setgroups` : une ouverture. Les identités de l'ouvreur
/// sont figées à l'ouverture (`file->f_cred`), comme Linux ; l'écriture se fait en une fois, au
/// début du fichier (`EINVAL` sinon).
pub struct NsMapFile {
    pub ns: Arc<UserNs>,
    pub what: NsMapWhat,
    pub pid: usize,
    pub opener: Creds,
    pub pos: spin::Mutex<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NsMapWhat {
    Map(MapKind),
    Setgroups,
}

impl NsMapFile {
    fn text(&self) -> String {
        match self.what {
            NsMapWhat::Map(k) => self.ns.map_text(k, &self.opener.user_ns),
            NsMapWhat::Setgroups => String::from(if self.ns.setgroups_allowed() { "allow\n" } else { "deny\n" }),
        }
    }
}

impl crate::object::FileObject for NsMapFile {
    crate::object::object_any!();

    fn link_name(&self) -> String {
        let name = match self.what {
            NsMapWhat::Map(MapKind::Uid) => "uid_map",
            NsMapWhat::Map(MapKind::Gid) => "gid_map",
            NsMapWhat::Setgroups => "setgroups",
        };
        format!("/proc/{}/{}", self.pid, name)
    }

    fn stat(&self) -> (u32, u64) {
        (0o100644, 0x6d61_6903)
    }

    fn read(&self, _t: &Thread, kb: &mut [u8], _nonblock: bool) -> Result<usize, i64> {
        let text = self.text();
        let mut pos = self.pos.lock();
        let rest = text.as_bytes().get(*pos..).unwrap_or(&[]);
        let n = rest.len().min(kb.len());
        kb[..n].copy_from_slice(&rest[..n]);
        *pos += n;
        Ok(n)
    }

    fn write(&self, _t: &Thread, kb: &[u8], _nonblock: bool) -> Result<usize, i64> {
        let mut pos = self.pos.lock();
        if *pos != 0 || kb.len() >= 4096 {
            return Err(EINVAL);
        }
        match self.what {
            NsMapWhat::Map(k) => self.ns.write_map(k, &self.opener, kb)?,
            NsMapWhat::Setgroups => self.ns.write_setgroups(&self.opener, kb)?,
        }
        *pos += kb.len();
        Ok(kb.len())
    }

    fn poll(&self, _t: Option<&Thread>) -> u32 {
        crate::object::ev::READABLE | crate::object::ev::WRITABLE
    }

    fn queues<'a>(&'a self, _cb: &mut dyn FnMut(&'a crate::wait::WaitQueue)) {}
}
