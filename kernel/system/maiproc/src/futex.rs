//! Futex : files d'attente par clé, `WAIT`/`WAKE`, `REQUEUE`, `WAKE_OP`, verrous à héritage de
//! priorité (sémantique de propriété ; l'héritage lui-même est à brancher sur MaiSched),
//! `futex_waitv` (Linux 5.16, conçu pour le `WaitForMultipleObjects` de Wine/Proton) et listes
//! robustes à la fin d'un fil.
//!
//! Clés ([`uvm::FutexKey`]) : propres à l'espace pour la mémoire privée, communes (objet partagé,
//! décalage) pour `MAP_SHARED` — un `fork` ne mélange donc pas les futex du parent et de l'enfant.
//!
//! Les seaux coupent la préemption mais pas les interruptions : la lecture du mot surveillé se
//! fait sous leur verrou et peut prendre une faute (et une IPI d'invalidation) dans `uvm`.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use sync_preemption::PreemptionSafeMutex;
use task::TaskRef;
use time::Instant;
use uvm::{FutexKey, Mm};

use crate::linux::errno::*;
use crate::Thread;

const NBUCKETS: usize = 256;
const ETIMEDOUT: i64 = 110;
const EDEADLK: i64 = 35;

pub const FUTEX_WAITERS: u32 = 0x8000_0000;
pub const FUTEX_OWNER_DIED: u32 = 0x4000_0000;
pub const FUTEX_TID_MASK: u32 = 0x3FFF_FFFF;

/// État d'un fil qui attend (partagé par toutes ses entrées : `futex_waitv`).
struct WaitState {
    task: TaskRef,
    tid: u32,
    /// -1 : en attente ; ≥ 0 : réveillé par l'entrée de ce rang ; -2 : parti.
    woken: AtomicIsize,
}

struct Entry {
    key: FutexKey,
    bitset: u32,
    index: usize,
    state: Arc<WaitState>,
}

static BUCKETS: spin::Once<Vec<PreemptionSafeMutex<Vec<Entry>>>> = spin::Once::new();

fn buckets() -> &'static [PreemptionSafeMutex<Vec<Entry>>] {
    BUCKETS.call_once(|| (0..NBUCKETS).map(|_| PreemptionSafeMutex::new(Vec::new())).collect())
}

fn hash(key: &FutexKey) -> usize {
    let (a, b) = match *key {
        FutexKey::Private { mm, addr } => (mm as u64, addr as u64),
        FutexKey::Shared { obj, offset } => (obj ^ 0x9E37_79B9_7F4A_7C15, offset as u64),
    };
    let h = (a.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ b.rotate_left(17)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    (h >> 56) as usize % NBUCKETS
}

/// Réveille au plus `n` entrées de `key` (dans le seau verrouillé `b`) dont le masque croise
/// `bitset` ; les tâches sont ajoutées à `out` (débloquées par l'appelant, verrou rendu).
fn wake_locked(b: &mut Vec<Entry>, key: &FutexKey, n: usize, bitset: u32, out: &mut Vec<TaskRef>) -> usize {
    let mut woken = 0;
    let mut i = 0;
    while i < b.len() && woken < n {
        let e = &b[i];
        if e.key == *key && e.bitset & bitset != 0 {
            let e = b.remove(i);
            if e.state.woken.compare_exchange(-1, e.index as isize, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                out.push(e.state.task.clone());
                woken += 1;
            }
            continue;
        }
        if e.state.woken.load(Ordering::Acquire) == -2 {
            b.remove(i);
            continue;
        }
        i += 1;
    }
    woken
}

fn unblock_all(tasks: Vec<TaskRef>) {
    for t in tasks {
        let _ = t.unblock();
    }
}

/// Retire les entrées d'un waiter qui repart (délai, signal) et marque son départ. Renvoie le rang
/// qui l'a réveillé s'il l'a été entre-temps.
fn leave(state: &Arc<WaitState>, keys: &[FutexKey]) -> Option<usize> {
    match state.woken.compare_exchange(-1, -2, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => {}
        Err(i) if i >= 0 => return Some(i as usize),
        Err(_) => return None,
    }
    for k in keys {
        buckets()[hash(k)].lock().retain(|e| !Arc::ptr_eq(&e.state, state));
    }
    None
}

/// Délai d'un futex : relatif (`FUTEX_WAIT`) ou absolu (`*_BITSET`, `LOCK_PI`), horloge
/// monotone ou réelle (`FUTEX_CLOCK_REALTIME`).
pub fn deadline_from(t: &Thread, ts: u64, absolute: bool, realtime: bool) -> Result<Option<Instant>, i64> {
    if ts == 0 {
        return Ok(None);
    }
    let v = crate::linux::compat::get_timespec(t, ts)?;
    if v[0] < 0 || !(0..1_000_000_000).contains(&v[1]) {
        return Err(EINVAL);
    }
    let d = core::time::Duration::new(v[0] as u64, v[1] as u32);
    Ok(Some(if absolute {
        crate::linux::sys::instant_from_clock(d, realtime)
    } else {
        Instant::now() + d
    }))
}

/// Attente sur une ou plusieurs clés ; `check` lit les mots sous les verrous des seaux (tous
/// tenus) et dit s'il faut dormir. Renvoie le rang réveillé.
fn wait_on(t: &Thread, keys: &[(FutexKey, u32)], deadline: Option<Instant>, check: impl FnOnce() -> Result<(), i64>) -> Result<usize, i64> {
    let task = task::get_my_current_task().ok_or(EINVAL)?;
    // Numéro du fil vu de son espace pid : c'est lui qu'on écrit dans le mot d'un verrou PI.
    let state = Arc::new(WaitState { task: task.clone(), tid: t.vtid() as u32, woken: AtomicIsize::new(-1) });
    let _ = task.block();
    {
        // Seaux verrouillés dans l'ordre de leur rang (pas d'interblocage entre attentes multiples).
        let mut idx: Vec<usize> = keys.iter().map(|(k, _)| hash(k)).collect();
        idx.sort_unstable();
        idx.dedup();
        let mut guards: Vec<_> = idx.iter().map(|&i| (i, buckets()[i].lock())).collect();
        if let Err(e) = check() {
            drop(guards);
            let _ = task.unblock();
            return Err(e);
        }
        for (n, (k, bitset)) in keys.iter().enumerate() {
            let h = hash(k);
            if let Some((_, g)) = guards.iter_mut().find(|(i, _)| *i == h) {
                g.push(Entry { key: *k, bitset: *bitset, index: n, state: state.clone() });
            }
        }
    }
    if let Some(d) = deadline {
        sleep::wake_at(task.clone(), d);
    }
    let key_list: Vec<FutexKey> = keys.iter().map(|(k, _)| *k).collect();
    loop {
        if state.woken.load(Ordering::Acquire) >= 0 {
            break;
        }
        if t.signal_pending() || deadline.map_or(false, |d| Instant::now() >= d) {
            let _ = task.unblock();
        } else {
            task::schedule();
        }
        let w = state.woken.load(Ordering::Acquire);
        if w >= 0 {
            return Ok(w as usize);
        }
        let interrupted = t.signal_pending();
        let timed_out = deadline.map_or(false, |d| Instant::now() >= d);
        if interrupted || timed_out {
            if let Some(i) = leave(&state, &key_list) {
                return Ok(i);
            }
            return Err(if interrupted {
                if deadline.is_some() { EINTR } else { crate::signal::ERESTARTSYS }
            } else {
                ETIMEDOUT
            });
        }
        // Réveil parasite : on se rebloque (l'entrée est toujours en file).
        let _ = task.block();
        if state.woken.load(Ordering::Acquire) >= 0 || t.signal_pending() {
            let _ = task.unblock();
        }
    }
    Ok(state.woken.load(Ordering::Acquire).max(0) as usize)
}

fn key_of(mm: &Mm, uaddr: u64, private: bool) -> Result<FutexKey, i64> {
    mm.futex_key(uaddr as usize, private).map_err(crate::linux::fault)
}

/// `futex(2)`.
pub fn futex(t: &Thread, uaddr: u64, op: u64, val: u64, timeout: u64, uaddr2: u64, val3: u64) -> Result<i64, i64> {
    const FUTEX_WAIT: u64 = 0;
    const FUTEX_WAKE: u64 = 1;
    const FUTEX_REQUEUE: u64 = 3;
    const FUTEX_CMP_REQUEUE: u64 = 4;
    const FUTEX_WAKE_OP: u64 = 5;
    const FUTEX_LOCK_PI: u64 = 6;
    const FUTEX_UNLOCK_PI: u64 = 7;
    const FUTEX_TRYLOCK_PI: u64 = 8;
    const FUTEX_WAIT_BITSET: u64 = 9;
    const FUTEX_WAKE_BITSET: u64 = 10;
    const FUTEX_LOCK_PI2: u64 = 13;
    const FUTEX_PRIVATE_FLAG: u64 = 128;
    const FUTEX_CLOCK_REALTIME: u64 = 256;
    let private = op & FUTEX_PRIVATE_FLAG != 0;
    let realtime = op & FUTEX_CLOCK_REALTIME != 0;
    let cmd = op & 0x7f;
    let mm = t.mm();
    match cmd {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            let bitset = if cmd == FUTEX_WAIT { u32::MAX } else { val3 as u32 };
            if bitset == 0 {
                return Err(EINVAL);
            }
            let key = key_of(mm, uaddr, private)?;
            let deadline = deadline_from(t, timeout, cmd == FUTEX_WAIT_BITSET, realtime)?;
            let expected = val as u32;
            wait_on(t, &[(key, bitset)], deadline, || {
                let cur = mm.read_user::<u32>(uaddr as usize).map_err(crate::linux::fault)?;
                if cur != expected { Err(EAGAIN) } else { Ok(()) }
            })
            .map(|_| 0)
        }
        FUTEX_WAKE | FUTEX_WAKE_BITSET => {
            let bitset = if cmd == FUTEX_WAKE { u32::MAX } else { val3 as u32 };
            if bitset == 0 {
                return Err(EINVAL);
            }
            let key = key_of(mm, uaddr, private)?;
            Ok(wake(&key, val as usize, bitset) as i64)
        }
        FUTEX_REQUEUE | FUTEX_CMP_REQUEUE => {
            let k1 = key_of(mm, uaddr, private)?;
            let k2 = key_of(mm, uaddr2, private)?;
            let nr_wake = val as usize;
            let nr_requeue = timeout as usize; // `val2` passe par l'argument du délai
            let (h1, h2) = (hash(&k1), hash(&k2));
            let mut out = Vec::new();
            let total = {
                let (mut g1, mut g2) = lock_two(h1, h2);
                if cmd == FUTEX_CMP_REQUEUE {
                    let cur = mm.read_user::<u32>(uaddr as usize).map_err(crate::linux::fault)?;
                    if cur != val3 as u32 {
                        return Err(EAGAIN);
                    }
                }
                let woken = wake_locked(&mut g1, &k1, nr_wake, u32::MAX, &mut out);
                let mut moved = 0;
                let mut i = 0;
                while i < g1.len() && moved < nr_requeue {
                    if g1[i].key == k1 && g1[i].state.woken.load(Ordering::Acquire) == -1 {
                        let mut e = g1.remove(i);
                        e.key = k2;
                        match g2.as_mut() {
                            Some(g2) => g2.push(e),
                            None => g1.push(e), // même seau : l'entrée reste (clé changée)
                        }
                        moved += 1;
                        if g2.is_none() {
                            // L'entrée déplacée est en fin de seau ; on évite de la revisiter.
                            continue;
                        }
                        continue;
                    }
                    i += 1;
                }
                woken + moved
            };
            unblock_all(out);
            Ok(total as i64)
        }
        FUTEX_WAKE_OP => {
            let k1 = key_of(mm, uaddr, private)?;
            let k2 = key_of(mm, uaddr2, private)?;
            let nr2 = timeout as usize;
            let mut out = Vec::new();
            let total = {
                let (mut g1, mut g2) = lock_two(hash(&k1), hash(&k2));
                let old = mm
                    .with_user_atomic_u32(uaddr2 as usize, true, |a| wake_op_apply(a, val3 as u32))
                    .map_err(crate::linux::fault)??;
                let mut n = wake_locked(&mut g1, &k1, val as usize, u32::MAX, &mut out);
                if wake_op_cmp(old, val3 as u32) {
                    n += match g2.as_mut() {
                        Some(g2) => wake_locked(g2, &k2, nr2, u32::MAX, &mut out),
                        None => wake_locked(&mut g1, &k2, nr2, u32::MAX, &mut out),
                    };
                }
                n
            };
            unblock_all(out);
            Ok(total as i64)
        }
        FUTEX_LOCK_PI | FUTEX_LOCK_PI2 | FUTEX_TRYLOCK_PI => {
            let key = key_of(mm, uaddr, private)?;
            let deadline = if cmd == FUTEX_TRYLOCK_PI {
                None
            } else {
                // LOCK_PI : délai absolu CLOCK_REALTIME ; LOCK_PI2 : monotone sauf drapeau.
                deadline_from(t, timeout, true, cmd == FUTEX_LOCK_PI || realtime)?
            };
            lock_pi(t, mm, uaddr as usize, key, deadline, cmd == FUTEX_TRYLOCK_PI)
        }
        FUTEX_UNLOCK_PI => {
            let key = key_of(mm, uaddr, private)?;
            unlock_pi(t, mm, uaddr as usize, key)
        }
        _ => Err(ENOSYS),
    }
}

/// Verrouille deux seaux (dans l'ordre) ; le second est `None` s'il est le même que le premier.
fn lock_two(
    h1: usize,
    h2: usize,
) -> (sync_preemption::PreemptionSafeMutexGuard<'static, Vec<Entry>>, Option<sync_preemption::PreemptionSafeMutexGuard<'static, Vec<Entry>>>) {
    let b = buckets();
    match h1.cmp(&h2) {
        core::cmp::Ordering::Equal => (b[h1].lock(), None),
        core::cmp::Ordering::Less => {
            let g1 = b[h1].lock();
            let g2 = b[h2].lock();
            (g1, Some(g2))
        }
        core::cmp::Ordering::Greater => {
            let g2 = b[h2].lock();
            let g1 = b[h1].lock();
            (g1, Some(g2))
        }
    }
}

/// Réveille au plus `n` fils qui attendent `key`.
pub fn wake(key: &FutexKey, n: usize, bitset: u32) -> usize {
    let mut out = Vec::new();
    let w = {
        let mut b = buckets()[hash(key)].lock();
        wake_locked(&mut b, key, n, bitset, &mut out)
    };
    unblock_all(out);
    w
}

/// `FUTEX_WAKE_OP` : opération sur le mot, renvoie l'ancienne valeur.
fn wake_op_apply(a: &AtomicU32, encoded: u32) -> Result<u32, i64> {
    let mut op = (encoded >> 28) & 0x7;
    let shift = (encoded >> 28) & 0x8 != 0;
    let mut oparg = ((encoded >> 12) & 0xfff) as i32;
    oparg = (oparg << 20) >> 20; // signe sur 12 bits
    if shift {
        if !(0..32).contains(&oparg) {
            oparg &= 31;
        }
        oparg = 1 << oparg;
    }
    let arg = oparg as u32;
    if op > 4 {
        op = 5;
    }
    let old = match op {
        0 => a.swap(arg, Ordering::AcqRel),
        1 => a.fetch_add(arg, Ordering::AcqRel),
        2 => a.fetch_or(arg, Ordering::AcqRel),
        3 => a.fetch_and(!arg, Ordering::AcqRel),
        4 => a.fetch_xor(arg, Ordering::AcqRel),
        _ => return Err(ENOSYS),
    };
    Ok(old)
}

fn wake_op_cmp(old: u32, encoded: u32) -> bool {
    let cmp = (encoded >> 24) & 0xf;
    let mut cmparg = (encoded & 0xfff) as i32;
    cmparg = (cmparg << 20) >> 20;
    let (o, c) = (old as i32, cmparg);
    match cmp {
        0 => o == c,
        1 => o != c,
        2 => o < c,
        3 => o <= c,
        4 => o > c,
        5 => o >= c,
        _ => false,
    }
}

/// `FUTEX_LOCK_PI` : prend le verrou (mot = TID du propriétaire), sinon pose `FUTEX_WAITERS` et
/// attend que le propriétaire le **transmette** (`unlock_pi` écrit notre TID).
fn lock_pi(t: &Thread, mm: &Mm, uaddr: usize, key: FutexKey, deadline: Option<Instant>, try_only: bool) -> Result<i64, i64> {
    let me = t.vtid() as u32;
    loop {
        // Tentative : 0 → TID ; propriétaire mort sans successeur → reprise.
        let r = mm
            .with_user_atomic_u32(uaddr, true, |a| {
                let cur = a.load(Ordering::Acquire);
                if cur & FUTEX_TID_MASK == 0 {
                    let new = me | (cur & FUTEX_OWNER_DIED) | (cur & FUTEX_WAITERS);
                    match a.compare_exchange(cur, new, Ordering::AcqRel, Ordering::Acquire) {
                        Ok(_) => Ok(true),
                        Err(_) => Ok(false),
                    }
                } else if cur & FUTEX_TID_MASK == me {
                    Err(EDEADLK)
                } else {
                    Ok(false)
                }
            })
            .map_err(crate::linux::fault)?;
        match r {
            Err(e) => return Err(e),
            Ok(true) => return Ok(0),
            Ok(false) if try_only => return Err(EAGAIN),
            Ok(false) => {}
        }
        // Attente : le mot doit porter FUTEX_WAITERS et un autre propriétaire.
        let res = wait_on(t, &[(key, u32::MAX)], deadline, || {
            mm.with_user_atomic_u32(uaddr, true, |a| loop {
                let cur = a.load(Ordering::Acquire);
                if cur & FUTEX_TID_MASK == 0 {
                    return Err(EAGAIN); // libéré entre-temps : nouvelle tentative
                }
                if a.compare_exchange(cur, cur | FUTEX_WAITERS, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                    return Ok(());
                }
            })
            .map_err(crate::linux::fault)?
        });
        match res {
            Ok(_) => {
                let cur = mm.read_user::<u32>(uaddr).map_err(crate::linux::fault)?;
                if cur & FUTEX_TID_MASK == me {
                    return Ok(0); // transmis par le propriétaire
                }
            }
            Err(EAGAIN) => {}
            Err(crate::signal::ERESTARTSYS) => return Err(crate::signal::ERESTARTNOINTR),
            Err(e) => return Err(e),
        }
    }
}

/// `FUTEX_UNLOCK_PI` : transmet le verrou au premier fil en attente (son TID dans le mot), ou le
/// libère.
fn unlock_pi(t: &Thread, mm: &Mm, uaddr: usize, key: FutexKey) -> Result<i64, i64> {
    let me = t.vtid() as u32;
    let mut out = Vec::new();
    {
        let mut b = buckets()[hash(&key)].lock();
        let cur = mm.read_user::<u32>(uaddr).map_err(crate::linux::fault)?;
        if cur & FUTEX_TID_MASK != me {
            return Err(EPERM);
        }
        // Premier en attente encore présent.
        let pos = b.iter().position(|e| e.key == key && e.state.woken.load(Ordering::Acquire) == -1);
        let next = pos.map(|p| b.remove(p));
        let more = b.iter().any(|e| e.key == key && e.state.woken.load(Ordering::Acquire) == -1);
        let new = match &next {
            Some(e) => e.state.tid | if more { FUTEX_WAITERS } else { 0 },
            None => 0,
        };
        mm.with_user_atomic_u32(uaddr, true, |a| a.store(new, Ordering::Release)).map_err(crate::linux::fault)?;
        if let Some(e) = next {
            if e.state.woken.compare_exchange(-1, 0, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                out.push(e.state.task.clone());
            }
        }
    }
    unblock_all(out);
    Ok(0)
}

/// `futex_waitv(2)` : attend que l'un des futex soit réveillé ; renvoie son rang.
pub fn futex_waitv(t: &Thread, waiters: u64, nr: u64, flags: u64, timeout: u64, clockid: u64) -> Result<i64, i64> {
    const FUTEX2_SIZE_U32: u32 = 2;
    const FUTEX2_SIZE_MASK: u32 = 3;
    const FUTEX2_PRIVATE: u32 = 128;
    if flags != 0 || nr == 0 || nr > 128 {
        return Err(EINVAL);
    }
    let realtime = match clockid {
        0 => true,  // CLOCK_REALTIME
        1 => false, // CLOCK_MONOTONIC
        _ => return Err(EINVAL),
    };
    let mm = t.mm();
    let mut raw = alloc::vec![0u8; nr as usize * 24];
    mm.copy_from_user(&mut raw, waiters as usize).map_err(crate::linux::fault)?;
    let mut list: Vec<(u64, usize, FutexKey)> = Vec::with_capacity(nr as usize);
    for c in raw.chunks_exact(24) {
        let val = u64::from_le_bytes(c[0..8].try_into().unwrap());
        let uaddr = u64::from_le_bytes(c[8..16].try_into().unwrap()) as usize;
        let fl = u32::from_le_bytes(c[16..20].try_into().unwrap());
        let reserved = u32::from_le_bytes(c[20..24].try_into().unwrap());
        if reserved != 0 || fl & FUTEX2_SIZE_MASK != FUTEX2_SIZE_U32 || fl & !(FUTEX2_SIZE_MASK | FUTEX2_PRIVATE) != 0 {
            return Err(EINVAL);
        }
        let key = mm.futex_key(uaddr, fl & FUTEX2_PRIVATE != 0).map_err(crate::linux::fault)?;
        list.push((val, uaddr, key));
    }
    let deadline = deadline_from(t, timeout, true, realtime)?;
    let keys: Vec<(FutexKey, u32)> = list.iter().map(|(_, _, k)| (*k, u32::MAX)).collect();
    wait_on(t, &keys, deadline, || {
        for (val, uaddr, _) in &list {
            let cur = mm.read_user::<u32>(*uaddr).map_err(crate::linux::fault)?;
            if cur as u64 != *val {
                return Err(EAGAIN);
            }
        }
        Ok(())
    })
    .map(|i| i as i64)
}

/// Fin d'un fil : verrous robustes encore tenus marqués `FUTEX_OWNER_DIED` (et un fil en attente
/// réveillé), puis `clear_child_tid` remis à zéro avec réveil (`pthread_join`).
pub fn exit_thread(t: &Thread, mm: &Mm) {
    let me = t.vtid() as u32;
    let head = t.robust_list.swap(0, Ordering::AcqRel);
    if head != 0 {
        let handle = |addr: usize| {
            let r = mm.with_user_atomic_u32(addr, true, |a| loop {
                let cur = a.load(Ordering::Acquire);
                if cur & FUTEX_TID_MASK != me {
                    return false;
                }
                let new = (cur & FUTEX_WAITERS) | FUTEX_OWNER_DIED;
                if a.compare_exchange(cur, new, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                    return cur & FUTEX_WAITERS != 0;
                }
            });
            if let Ok(true) = r {
                if let Ok(k) = mm.futex_key(addr, false) {
                    wake(&k, 1, u32::MAX);
                }
            }
        };
        // struct robust_list_head { next, futex_offset, list_op_pending } : mots de 4 octets
        // pour une liste posée par un appel i386 (`compat_exit_robust_list`). Bit 0 d'un lien :
        // verrou à héritage de priorité (ignoré ici).
        let compat = t.robust_compat.load(Ordering::Relaxed);
        let read_head = || -> Option<[u64; 3]> {
            if compat {
                mm.read_user::<[u32; 3]>(head).ok().map(|h| [h[0] as u64, h[1] as i32 as i64 as u64, h[2] as u64])
            } else {
                mm.read_user::<[u64; 3]>(head).ok()
            }
        };
        let read_next = |e: usize| -> usize {
            if compat {
                mm.read_user::<u32>(e).unwrap_or(0) as usize
            } else {
                mm.read_user::<u64>(e).unwrap_or(0) as usize
            }
        };
        if let Some(h) = read_head() {
            let h = [h[0] & !1, h[1], h[2] & !1];
            let offset = h[1] as i64;
            let mut entry = h[0] as usize;
            let mut n = 0;
            while entry != head && entry != 0 && n < 2048 {
                let next = read_next(entry) & !1;
                if entry as u64 != h[2] {
                    handle((entry as i64 + offset) as usize);
                }
                entry = next;
                n += 1;
            }
            if h[2] != 0 {
                handle((h[2] as i64 + offset) as usize);
            }
        }
    }
    let ctid = t.clear_child_tid.swap(0, Ordering::AcqRel);
    if ctid != 0 && mm.write_user::<u32>(ctid, &0).is_ok() {
        if let Ok(k) = mm.futex_key(ctid, false) {
            wake(&k, 1, u32::MAX);
        }
    }
}
