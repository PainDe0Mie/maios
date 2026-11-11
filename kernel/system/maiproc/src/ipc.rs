//! IPC System V : mémoire partagée (`shmget`, `shmat`, `shmdt`, `shmctl`).
//!
//! Le serveur X (MIT-SHM : Xwayland, et ses clients Chromium/CEF, Mesa, SDL) s'en sert pour
//! échanger les images sans copie ; d'anciennes bibliothèques et des jeux aussi.
//!
//! Règles de Linux (`ipc/shm.c`, `ipc/util.c`) :
//! - identifiant = `séquence << 15 | rang` : un identifiant périmé (segment détruit puis rang
//!   réutilisé) ne désigne jamais le nouveau segment (`EINVAL`) ;
//! - droits (`ipcperms`) jugés avec l'uid et le gid **effectifs**, propriétaire ou créateur ;
//!   root passe outre (`CAP_IPC_OWNER`) ;
//! - `IPC_RMID` d'un segment encore attaché : marqué `SHM_DEST`, sa clé devient `IPC_PRIVATE`
//!   (plus trouvable par `shmget`), toujours attachable par son identifiant ; détruit au dernier
//!   détachement ;
//! - `shm_nattch` compte les **zones** qui projettent le segment (un `fork` ou une zone coupée
//!   par `munmap`/`mprotect` en ajoutent une, un `munmap`, `exec` ou une sortie en retirent).
//!   Chaque zone tient une référence de l'objet partagé : le compte est celui des références,
//!   moins celle de la table tant que le segment n'est pas détruit.
//!
//! Les pages sont celles d'un `uvm::ShmObject` (créées à la première faute, mises à zéro) :
//! la taille demandée ne réserve rien, comme sous Linux sans `SHM_NORESERVE`.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use kernel_config::memory::PAGE_SIZE;
use spin::Mutex;
use uvm::{page_up, Backing, Prot, ShmObject};

use crate::creds::Creds;
use crate::userns::{UserNs, CAP_IPC_OWNER};
use crate::linux::errno::*;
use crate::Thread;

type SysResult = Result<i64, i64>;

pub const IPC_PRIVATE: i32 = 0;
const IPC_CREAT: u64 = 0o1000;
const IPC_EXCL: u64 = 0o2000;

const IPC_RMID: u64 = 0;
const IPC_SET: u64 = 1;
const IPC_STAT: u64 = 2;
const IPC_INFO: u64 = 3;
/// Drapeau de version des structures (`ipc_parse_version`) : seule la version 64 existe ici.
const IPC_64: u64 = 0x100;

const SHM_LOCK: u64 = 11;
const SHM_UNLOCK: u64 = 12;
const SHM_STAT: u64 = 13;
const SHM_INFO: u64 = 14;
const SHM_STAT_ANY: u64 = 15;

/// Bits de `shm_perm.mode` posés par le noyau.
const SHM_DEST: u32 = 0o1000;
const SHM_LOCKED: u32 = 0o2000;

const SHM_HUGETLB: u64 = 0o4000;
const SHM_NORESERVE: u64 = 0o10000;

const SHM_RDONLY: u64 = 0o10000;
const SHM_RND: u64 = 0o20000;
const SHM_REMAP: u64 = 0o40000;
const SHM_EXEC: u64 = 0o100000;

/// Limites par défaut de Linux (`include/uapi/linux/shm.h`).
const SHMMIN: u64 = 1;
const SHMMNI: usize = 4096;
const SHMMAX: u64 = u64::MAX - (1 << 24);
const SHMALL: u64 = u64::MAX - (1 << 24);
const SHMLBA: u64 = PAGE_SIZE as u64;
/// `IPCMNI_SHIFT` : bits du rang dans un identifiant.
const IPCMNI_SHIFT: u32 = 15;

/// `struct kern_ipc_perm` : clé, propriétaire, créateur, droits.
#[derive(Clone, Copy)]
pub struct IpcPerm {
    pub key: i32,
    pub uid: u32,
    pub gid: u32,
    pub cuid: u32,
    pub cgid: u32,
    /// 9 bits de droits, plus `SHM_DEST`/`SHM_LOCKED`.
    pub mode: u32,
    pub seq: u16,
}

impl IpcPerm {
    fn new(key: i32, mode: u32, c: &Creds, seq: u16) -> IpcPerm {
        IpcPerm { key, uid: c.euid, gid: c.egid, cuid: c.euid, cgid: c.egid, mode: mode & 0o777, seq }
    }

    /// `ipcperms` : les bits demandés (`flag` : `0o400`, `0o200`… pour n'importe quelle
    /// catégorie) sont-ils accordés ?
    fn allows(&self, c: &Creds, ns: &Arc<UserNs>, flag: u32) -> bool {
        let requested = (flag >> 6) | (flag >> 3) | flag;
        let mut granted = self.mode;
        if c.euid == self.cuid || c.euid == self.uid {
            granted >>= 6;
        } else if c.in_group(self.cgid) || c.in_group(self.gid) {
            granted >>= 3;
        }
        requested & !granted & 0o7 == 0 || crate::userns::ns_capable(c, ns, CAP_IPC_OWNER)
    }

    /// Propriétaire ou créateur (ou root) : `IPC_SET`, `IPC_RMID`.
    fn owned_by(&self, c: &Creds, ns: &Arc<UserNs>) -> bool {
        c.euid == self.uid || c.euid == self.cuid || crate::userns::ns_capable(c, ns, crate::userns::CAP_SYS_ADMIN)
    }

    /// `struct ipc64_perm` (48 octets).
    fn write(&self, out: &mut [u8]) {
        out[0..4].copy_from_slice(&self.key.to_le_bytes());
        out[4..8].copy_from_slice(&self.uid.to_le_bytes());
        out[8..12].copy_from_slice(&self.gid.to_le_bytes());
        out[12..16].copy_from_slice(&self.cuid.to_le_bytes());
        out[16..20].copy_from_slice(&self.cgid.to_le_bytes());
        out[20..24].copy_from_slice(&self.mode.to_le_bytes());
        out[24..26].copy_from_slice(&self.seq.to_le_bytes());
        out[26..48].fill(0);
    }
}

/// Identifiant visible d'un objet de rang `idx`.
fn build_id(seq: u16, idx: usize) -> i64 {
    ((seq as i64) << IPCMNI_SHIFT) | idx as i64
}

fn now() -> i64 {
    crate::linux::sys::realtime().0
}

/// Segment de mémoire partagée.
struct Segment {
    perm: IpcPerm,
    size: u64,
    /// Référence de la table tant que le segment n'est pas détruit (`IPC_RMID`).
    obj: Option<Arc<ShmObject>>,
    /// Toujours là : le segment vit tant qu'une zone le projette.
    weak: Weak<ShmObject>,
    obj_id: u64,
    atime: i64,
    dtime: i64,
    ctime: i64,
    cpid: i32,
    lpid: i32,
}

impl Segment {
    /// Zones qui projettent le segment (`None` : plus aucune et détruit).
    fn nattch(&self) -> Option<u64> {
        let strong = self.weak.strong_count() as u64;
        let table = self.obj.is_some() as u64;
        if strong == 0 {
            return None;
        }
        Some(strong - table)
    }

    fn alive(&self) -> bool {
        self.obj.is_some() || self.weak.strong_count() > 0
    }

    fn mode(&self) -> u32 {
        self.perm.mode | if self.obj.is_none() { SHM_DEST } else { 0 }
    }
}

struct ShmTable {
    /// Rang → segment.
    segs: BTreeMap<usize, Segment>,
    /// Séquence du prochain segment créé (`ids->seq` de Linux, sur 16 bits moins le signe).
    seq: u16,
}

impl ShmTable {
    /// Retire les segments détruits que plus personne ne projette.
    fn prune(&mut self) {
        let dead: Vec<usize> = self.segs.iter().filter(|(_, s)| !s.alive()).map(|(&i, _)| i).collect();
        for i in dead {
            self.segs.remove(&i);
        }
    }

    fn by_id(&mut self, id: i64) -> Result<(usize, &mut Segment), i64> {
        self.prune();
        if id < 0 {
            return Err(EINVAL);
        }
        let idx = (id & ((1 << IPCMNI_SHIFT) - 1)) as usize;
        let seq = (id >> IPCMNI_SHIFT) as u64;
        match self.segs.get_mut(&idx) {
            Some(s) if s.perm.seq as u64 == seq => Ok((idx, s)),
            _ => Err(EINVAL),
        }
    }

    fn by_key(&mut self, key: i32) -> Option<(usize, &mut Segment)> {
        self.segs.iter_mut().find(|(_, s)| s.obj.is_some() && s.perm.key == key).map(|(&i, s)| (i, s))
    }

    fn free_index(&self) -> Option<usize> {
        (0..SHMMNI).find(|i| !self.segs.contains_key(i))
    }

    fn total_pages(&self) -> u64 {
        self.segs.values().map(|s| page_up(s.size as usize) as u64 / PAGE_SIZE as u64).sum()
    }
}

/// Espace IPC (`ipc_namespace`) : sa propre table de segments.
pub struct IpcNs {
    pub ino: u64,
    /// Propriétaire : `CAP_IPC_OWNER` s'y juge.
    pub user_ns: Arc<UserNs>,
    shm: Mutex<ShmTable>,
}

impl IpcNs {
    pub fn new(ino: u64, user_ns: Arc<UserNs>) -> Arc<IpcNs> {
        Arc::new(IpcNs { ino, user_ns, shm: Mutex::new(ShmTable { segs: BTreeMap::new(), seq: 0 }) })
    }
}

fn ipc_of(t: &Thread) -> Arc<IpcNs> {
    crate::ns::of(&t.process).ipc.clone()
}

fn creds(t: &Thread) -> Creds {
    t.process.creds.lock().clone()
}

/// pid **global** de l'appelant (`shm_cpid`, `shm_lpid`), traduit à la lecture.
fn pid(t: &Thread) -> i32 {
    t.process.pid as i32
}

/// `shmget(key, size, shmflg)`.
pub fn shmget(t: &Thread, key: i32, size: u64, flg: u64) -> SysResult {
    let c = creds(t);
    let ipc = ipc_of(t);
    let mut tab = ipc.shm.lock();
    tab.prune();
    if key != IPC_PRIVATE {
        if let Some((idx, s)) = tab.by_key(key) {
            if flg & IPC_CREAT != 0 && flg & IPC_EXCL != 0 {
                return Err(EEXIST);
            }
            if s.size < size {
                return Err(EINVAL);
            }
            if !s.perm.allows(&c, &ipc.user_ns, (flg & 0o777) as u32) {
                return Err(EACCES);
            }
            return Ok(build_id(s.perm.seq, idx));
        }
        if flg & IPC_CREAT == 0 {
            return Err(ENOENT);
        }
    }
    if flg & SHM_HUGETLB != 0 {
        // Pas de hugetlbfs (comme `memfd_create(MFD_HUGETLB)`).
        return Err(EINVAL);
    }
    let _ = flg & SHM_NORESERVE; // rien n'est réservé de toute façon
    if !(SHMMIN..=SHMMAX).contains(&size) {
        return Err(EINVAL);
    }
    let pages = page_up(size as usize) as u64 / PAGE_SIZE as u64;
    if tab.total_pages().saturating_add(pages) > SHMALL {
        return Err(ENOSPC);
    }
    let idx = tab.free_index().ok_or(ENOSPC)?;
    let seq = tab.seq;
    tab.seq = (seq + 1) & 0x7fff;
    let obj = ShmObject::new();
    let now = now();
    tab.segs.insert(
        idx,
        Segment {
            perm: IpcPerm::new(key, flg as u32, &c, seq),
            size,
            weak: Arc::downgrade(&obj),
            obj_id: obj.id(),
            obj: Some(obj),
            atime: 0,
            dtime: 0,
            ctime: now,
            cpid: pid(t),
            lpid: 0,
        },
    );
    Ok(build_id(seq, idx))
}

/// `shmat(shmid, shmaddr, shmflg)` : adresse d'attache.
pub fn shmat(t: &Thread, id: i64, addr: u64, flg: u64) -> SysResult {
    let mut addr = addr;
    if addr != 0 {
        if addr & (SHMLBA - 1) != 0 {
            if flg & SHM_RND == 0 {
                return Err(EINVAL);
            }
            addr &= !(SHMLBA - 1);
            if addr == 0 {
                // Arrondi à zéro : Linux le refuse sauf avec `SHM_REMAP`… qui l'interdit aussi.
                return Err(EINVAL);
            }
        }
    } else if flg & SHM_REMAP != 0 {
        return Err(EINVAL);
    }
    let mut prot = Prot::READ;
    let mut want = 0o444;
    if flg & SHM_RDONLY == 0 {
        prot |= Prot::WRITE;
        want |= 0o222;
    }
    if flg & SHM_EXEC != 0 {
        prot |= Prot::EXEC;
        want |= 0o111;
    }
    let c = creds(t);
    let (obj, size, key) = {
        let ipc = ipc_of(t);
    let mut tab = ipc.shm.lock();
        let (_, s) = tab.by_id(id)?;
        if !s.perm.allows(&c, &ipc.user_ns, want) {
            return Err(EACCES);
        }
        // Un segment détruit mais encore attaché reste attachable par son identifiant.
        let obj = s.weak.upgrade().ok_or(EIDRM)?;
        (obj, s.size, s.perm.key)
    };
    let len = page_up(size as usize);
    let mm = t.mm();
    if addr != 0 && flg & SHM_REMAP == 0 && !mm.is_free_range(addr as usize, len) {
        return Err(EINVAL);
    }
    let name = uvm::VmaName::Path(Arc::from(format!("/SYSV{:08x} (deleted)", key as u32).as_str()));
    let start = mm
        .mmap(addr as usize, len, Prot(prot), Backing::Shared { obj, pgoff: 0 }, addr != 0, name)
        .map_err(|e| match e {
            uvm::VmError::NoMemory => ENOMEM,
            _ => EINVAL,
        })?;
    if flg & SHM_RDONLY != 0 {
        // Comme un fichier ouvert en lecture seule : `mprotect(PROT_WRITE)` → `EACCES`.
        mm.deny_write(start);
    }
    let ipc = ipc_of(t);
    let mut tab = ipc.shm.lock();
    if let Ok((_, s)) = tab.by_id(id) {
        s.atime = now();
        s.lpid = pid(t);
    }
    Ok(start as i64)
}

/// `shmdt(shmaddr)` : retire les zones du segment attaché à `shmaddr` (celle qui commence à
/// cette adresse au début du segment, puis ses suites dans la taille du segment).
pub fn shmdt(t: &Thread, addr: u64) -> SysResult {
    if addr & (PAGE_SIZE as u64 - 1) != 0 {
        return Err(EINVAL);
    }
    let addr = addr as usize;
    if addr >= uvm::USER_END {
        return Err(EINVAL);
    }
    let mm = t.mm();
    let Some(&(_, _, obj_id, _)) = mm
        .shared_vmas_in(addr, addr + PAGE_SIZE)
        .iter()
        .find(|&&(s, _, _, pgoff)| s == addr && pgoff == 0)
    else {
        return Err(EINVAL);
    };
    let size = {
        let ipc = ipc_of(t);
    let tab = ipc.shm.lock();
        match tab.segs.values().find(|s| s.obj_id == obj_id) {
            Some(s) => s.size,
            None => return Err(EINVAL),
        }
    };
    let end = addr.saturating_add(page_up(size as usize)).min(uvm::USER_END);
    for (s, e, o, pgoff) in mm.shared_vmas_in(addr, end) {
        if o == obj_id && s >= addr && (s - addr) / PAGE_SIZE == pgoff {
            let _ = mm.munmap(s, e.min(end) - s);
        }
    }
    let ipc = ipc_of(t);
    let mut tab = ipc.shm.lock();
    tab.prune();
    if let Some(s) = tab.segs.values_mut().find(|s| s.obj_id == obj_id) {
        s.dtime = now();
        s.lpid = pid(t);
    }
    Ok(0)
}

/// `struct shmid64_ds` (112 octets).
fn shmid_ds(s: &Segment, ns: &UserNs, pid_ns: &crate::pidns::PidNs) -> [u8; 112] {
    let mut b = [0u8; 112];
    let mut perm = s.perm;
    perm.mode = s.mode();
    // Identités vues de l'espace utilisateur de l'appelant.
    perm.uid = ns.from_kuid_munged(perm.uid);
    perm.cuid = ns.from_kuid_munged(perm.cuid);
    perm.gid = ns.from_kgid_munged(perm.gid);
    perm.cgid = ns.from_kgid_munged(perm.cgid);
    perm.write(&mut b[0..48]);
    b[48..56].copy_from_slice(&s.size.to_le_bytes());
    b[56..64].copy_from_slice(&s.atime.to_le_bytes());
    b[64..72].copy_from_slice(&s.dtime.to_le_bytes());
    b[72..80].copy_from_slice(&s.ctime.to_le_bytes());
    // pid vus de l'espace pid de l'appelant (0 hors de cet espace).
    b[80..84].copy_from_slice(&(pid_ns.nr_of(s.cpid as usize) as i32).to_le_bytes());
    b[84..88].copy_from_slice(&(pid_ns.nr_of(s.lpid as usize) as i32).to_le_bytes());
    b[88..96].copy_from_slice(&s.nattch().unwrap_or(0).to_le_bytes());
    b
}

fn put(t: &Thread, uaddr: u64, bytes: &[u8]) -> Result<(), i64> {
    t.mm().copy_to_user(uaddr as usize, bytes).map_err(|_| EFAULT)
}

/// Identité 32 bits → 16 bits (`high2lowuid` : au-delà, `overflowuid` 65534).
fn low16(v: u32) -> u16 {
    if v > 0xffff { 65534 } else { v as u16 }
}

/// `shmid_ds` 64 bits → disposition i386 : `compat_shmid64_ds` (84 octets, avec `IPC_64` :
/// `compat_ipc64_perm` de 36 octets, mode sur 16 bits, dates en mots bas et hauts) ou ancien
/// `compat_shmid_ds` (48 octets : identités, mode, pids et compteur sur 16 bits).
fn shmid_ds32(n: &[u8; 112], ipc64: bool) -> alloc::vec::Vec<u8> {
    let d = |o: usize| u32::from_le_bytes(n[o..o + 4].try_into().unwrap());
    let q = |o: usize| u64::from_le_bytes(n[o..o + 8].try_into().unwrap());
    let mut b = alloc::vec![0u8; if ipc64 { 84 } else { 48 }];
    let w32 = |b: &mut alloc::vec::Vec<u8>, o: usize, v: u32| b[o..o + 4].copy_from_slice(&v.to_le_bytes());
    if ipc64 {
        b[0..20].copy_from_slice(&n[0..20]); // clé, uid, gid, cuid, cgid
        b[20..22].copy_from_slice(&n[20..22]); // mode
        b[24..26].copy_from_slice(&n[24..26]); // seq
        w32(&mut b, 36, q(48) as u32);
        for (i, o) in [56usize, 64, 72].iter().enumerate() {
            w32(&mut b, 40 + i * 8, q(*o) as u32);
            w32(&mut b, 44 + i * 8, (q(*o) >> 32) as u32);
        }
        w32(&mut b, 64, d(80));
        w32(&mut b, 68, d(84));
        w32(&mut b, 72, q(88) as u32);
    } else {
        b[0..4].copy_from_slice(&n[0..4]);
        for (i, o) in [4usize, 8, 12, 16].iter().enumerate() {
            b[4 + i * 2..6 + i * 2].copy_from_slice(&low16(d(*o)).to_le_bytes());
        }
        b[12..14].copy_from_slice(&n[20..22]);
        b[14..16].copy_from_slice(&n[24..26]);
        w32(&mut b, 16, q(48).min(i32::MAX as u64) as u32);
        w32(&mut b, 20, q(56) as u32);
        w32(&mut b, 24, q(64) as u32);
        w32(&mut b, 28, q(72) as u32);
        b[32..34].copy_from_slice(&(d(80) as u16).to_le_bytes());
        b[34..36].copy_from_slice(&(d(84) as u16).to_le_bytes());
        b[36..38].copy_from_slice(&(q(88).min(0xffff) as u16).to_le_bytes());
    }
    b
}

/// Écrit un `shmid_ds` dans la disposition de l'appelant.
fn put_ds(t: &Thread, uaddr: u64, ds: &[u8; 112], ipc64: bool) -> Result<(), i64> {
    if crate::linux::compat::active(t) {
        put(t, uaddr, &shmid_ds32(ds, ipc64))
    } else {
        put(t, uaddr, ds)
    }
}

/// `shmctl(shmid, cmd, buf)`.
pub fn shmctl(t: &Thread, id: i64, cmd: u64, buf: u64) -> SysResult {
    // i386 : `IPC_64` choisit `compat_shmid64_ds`/`compat_shminfo64`, sinon les anciennes
    // structures (`compat_shmid_ds`, `compat_shminfo`).
    let ipc64 = cmd & IPC_64 != 0;
    let compat = crate::linux::compat::active(t);
    let cmd = cmd & !IPC_64;
    let c = creds(t);
    match cmd {
        IPC_INFO => {
            // `struct shminfo64` : shmmax, shmmin, shmmni, shmseg, shmall, 4 réservés ; en
            // i386, mots de 32 bits (9 avec `IPC_64`, 5 sinon), shmmax borné à `INT_MAX`.
            let v = [SHMMAX, SHMMIN, SHMMNI as u64, SHMMNI as u64, SHMALL];
            if compat {
                let n = if ipc64 { 9 } else { 5 };
                let mut b = alloc::vec![0u8; n * 4];
                for (i, x) in v.iter().enumerate() {
                    let x = if i == 0 { (*x).min(i32::MAX as u64) } else { (*x).min(u32::MAX as u64) };
                    b[i * 4..i * 4 + 4].copy_from_slice(&(x as u32).to_le_bytes());
                }
                put(t, buf, &b)?;
            } else {
                let mut b = [0u8; 72];
                for (i, x) in v.iter().enumerate() {
                    b[i * 8..i * 8 + 8].copy_from_slice(&x.to_le_bytes());
                }
                put(t, buf, &b)?;
            }
            let ipc = ipc_of(t);
    let mut tab = ipc.shm.lock();
            tab.prune();
            Ok(tab.segs.keys().next_back().map_or(0, |&i| i as i64))
        }
        SHM_INFO => {
            // `struct shm_info` : used_ids, shm_tot, shm_rss, shm_swp, swap_attempts,
            // swap_successes.
            let (used, tot, rss, last) = {
                let ipc = ipc_of(t);
    let mut tab = ipc.shm.lock();
                tab.prune();
                let tot = tab.total_pages();
                let rss: u64 = tab.segs.values().filter_map(|s| s.weak.upgrade()).map(|o| o.resident() as u64).sum();
                (tab.segs.len() as i32, tot, rss, tab.segs.keys().next_back().map_or(0, |&i| i as i64))
            };
            if compat {
                // `struct compat_shm_info` : un `int` puis cinq `compat_ulong_t`.
                let mut b = [0u8; 24];
                b[0..4].copy_from_slice(&used.to_le_bytes());
                b[4..8].copy_from_slice(&(tot as u32).to_le_bytes());
                b[8..12].copy_from_slice(&(rss as u32).to_le_bytes());
                put(t, buf, &b)?;
                return Ok(last);
            }
            let mut b = [0u8; 48];
            b[0..4].copy_from_slice(&used.to_le_bytes());
            b[8..16].copy_from_slice(&tot.to_le_bytes());
            b[16..24].copy_from_slice(&rss.to_le_bytes());
            put(t, buf, &b)?;
            Ok(last)
        }
        SHM_STAT | SHM_STAT_ANY => {
            // `id` est un rang de la table ; rend l'identifiant complet.
            let (ds, full) = {
                let ipc = ipc_of(t);
    let mut tab = ipc.shm.lock();
                tab.prune();
                let idx = usize::try_from(id).map_err(|_| EINVAL)?;
                let s = tab.segs.get(&idx).ok_or(EINVAL)?;
                if cmd == SHM_STAT && !s.perm.allows(&c, &ipc.user_ns, 0o444) {
                    return Err(EACCES);
                }
                (shmid_ds(s, &c.user_ns, &t.process.pid_ns), build_id(s.perm.seq, idx))
            };
            put_ds(t, buf, &ds, ipc64)?;
            Ok(full)
        }
        IPC_STAT => {
            let ds = {
                let ipc = ipc_of(t);
    let mut tab = ipc.shm.lock();
                let (_, s) = tab.by_id(id)?;
                if !s.perm.allows(&c, &ipc.user_ns, 0o444) {
                    return Err(EACCES);
                }
                shmid_ds(s, &c.user_ns, &t.process.pid_ns)
            };
            put_ds(t, buf, &ds, ipc64)?;
            Ok(0)
        }
        IPC_SET => {
            let mut b = [0u8; 112];
            let (uid, gid, mode) = if compat && !ipc64 {
                // `compat_ipc_perm` : identités et mode sur 16 bits (0xffff → −1).
                t.mm().copy_from_user(&mut b[..48], buf as usize).map_err(|_| EFAULT)?;
                let h = |o: usize| match u16::from_le_bytes([b[o], b[o + 1]]) {
                    0xffff => u32::MAX,
                    v => v as u32,
                };
                (h(4), h(6), u16::from_le_bytes([b[12], b[13]]) as u32)
            } else if compat {
                // `compat_ipc64_perm` : identités de 32 bits, mode sur 16 bits.
                t.mm().copy_from_user(&mut b[..84], buf as usize).map_err(|_| EFAULT)?;
                let w = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
                (w(4), w(8), u16::from_le_bytes([b[20], b[21]]) as u32)
            } else {
                t.mm().copy_from_user(&mut b, buf as usize).map_err(|_| EFAULT)?;
                let w = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
                (w(4), w(8), w(20))
            };
            let ipc = ipc_of(t);
    let mut tab = ipc.shm.lock();
            let (_, s) = tab.by_id(id)?;
            if !s.perm.owned_by(&c, &ipc.user_ns) {
                return Err(EPERM);
            }
            let uid = c.user_ns.make_kuid(uid).ok_or(EINVAL)?;
            let gid = c.user_ns.make_kgid(gid).ok_or(EINVAL)?;
            s.perm.uid = uid;
            s.perm.gid = gid;
            s.perm.mode = (s.perm.mode & !0o777) | (mode & 0o777);
            s.ctime = now();
            Ok(0)
        }
        IPC_RMID => {
            let dropped = {
                let ipc = ipc_of(t);
    let mut tab = ipc.shm.lock();
                let (_, s) = tab.by_id(id)?;
                if !s.perm.owned_by(&c, &ipc.user_ns) {
                    return Err(EPERM);
                }
                s.perm.key = IPC_PRIVATE;
                s.ctime = now();
                let obj = s.obj.take();
                tab.prune();
                obj
            };
            // Dernière référence hors du verrou : rendre les frames peut être long.
            drop(dropped);
            let ipc = ipc_of(t);
    let mut tab = ipc.shm.lock();
            tab.prune();
            Ok(0)
        }
        SHM_LOCK | SHM_UNLOCK => {
            // Rien ne part en échange : seul l'état visible change.
            let ipc = ipc_of(t);
    let mut tab = ipc.shm.lock();
            let (_, s) = tab.by_id(id)?;
            if !s.perm.owned_by(&c, &ipc.user_ns) {
                return Err(EPERM);
            }
            if cmd == SHM_LOCK {
                s.perm.mode |= SHM_LOCKED;
            } else {
                s.perm.mode &= !SHM_LOCKED;
            }
            s.ctime = now();
            Ok(0)
        }
        _ => Err(EINVAL),
    }
}

/// `/proc/sysvipc/shm`.
pub fn proc_shm(t: &Thread) -> alloc::string::String {
    let ns = t.process.creds.lock().user_ns.clone();
    use core::fmt::Write;
    let mut out = alloc::string::String::from(
        "       key      shmid perms                  size  cpid  lpid nattch   uid   gid  cuid  cgid      atime      dtime      ctime                   rss                  swap\n",
    );
    let ipc = ipc_of(t);
    let mut tab = ipc.shm.lock();
    tab.prune();
    for (&idx, s) in tab.segs.iter() {
        let rss = s.weak.upgrade().map_or(0, |o| o.resident() * PAGE_SIZE);
        let _ = writeln!(
            out,
            "{:10} {:10} {:5o} {:21} {:5} {:5} {:6} {:5} {:5} {:5} {:5} {:10} {:10} {:10} {:21} {:21}",
            s.perm.key,
            build_id(s.perm.seq, idx),
            s.mode(),
            s.size,
            t.process.pid_ns.nr_of(s.cpid as usize),
            t.process.pid_ns.nr_of(s.lpid as usize),
            s.nattch().unwrap_or(0),
            ns.from_kuid_munged(s.perm.uid),
            ns.from_kgid_munged(s.perm.gid),
            ns.from_kuid_munged(s.perm.cuid),
            ns.from_kgid_munged(s.perm.cgid),
            s.atime,
            s.dtime,
            s.ctime,
            rss,
            0
        );
    }
    out
}
