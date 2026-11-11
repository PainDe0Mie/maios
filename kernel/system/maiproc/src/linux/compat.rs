//! Personnalité i386 : appels système des programmes 32 bits (`int 0x80`, `syscall` du mode
//! compatible), numéros de `arch/x86/entry/syscalls/syscall_32.tbl`, arguments dans
//! `ebx, ecx, edx, esi, edi, ebp`, résultat dans `eax`.
//!
//! Comme `CONFIG_IA32_EMULATION` de Linux : un appel i386 aboutit aux mêmes fonctions que son
//! équivalent 64 bits. Les arguments sont tronqués à 32 bits (signés quand le type C l'est :
//! descripteurs, `AT_FDCWD`, décalages `off_t`), les valeurs 64 bits passées en deux registres
//! (`pread64`, `ftruncate64`…) recomposées. Pendant l'appel, [`active`] est vrai : les
//! fonctions communes lisent et écrivent alors les structures au format i386 (`iovec`,
//! `msghdr`, `sigaction`, `stack_t`, `rusage`, tableaux d'`execve`, `user_desc` de `clone`) ;
//! [`time32`] dit en plus que les `timespec`/`timeval` font 2 × 32 bits (appels « time32 »,
//! les `*_time64` gardant le format 64 bits).

use core::sync::atomic::Ordering;
use uentry::UserRegs;

use super::errno::*;
use super::fs::{self, Stat32};
use super::{fault, mem, proc, sys, Outcome, SysResult};
use crate::Thread;

/// L'appel système en cours vient de la table i386.
#[inline]
pub(crate) fn active(t: &Thread) -> bool {
    t.in_compat.load(Ordering::Relaxed)
}

/// L'appel i386 en cours prend des `timespec`/`timeval` de 2 × 32 bits.
#[inline]
pub(crate) fn time32(t: &Thread) -> bool {
    t.time32.load(Ordering::Relaxed)
}

/// Lit un `timespec` (ou `timeval`) : `[secondes, ns ou µs]`.
pub(crate) fn get_timespec(t: &Thread, p: u64) -> Result<[i64; 2], i64> {
    if time32(t) {
        let v = t.mm().read_user::<[i32; 2]>(p as usize).map_err(fault)?;
        Ok([v[0] as i64, v[1] as i64])
    } else {
        t.mm().read_user::<[i64; 2]>(p as usize).map_err(fault)
    }
}

/// Écrit un `timespec` (ou `timeval`).
pub(crate) fn put_timespec(t: &Thread, p: u64, sec: i64, frac: i64) -> Result<(), i64> {
    if time32(t) {
        t.mm().write_user(p as usize, &[sec as i32, frac as i32]).map_err(fault)
    } else {
        t.mm().write_user(p as usize, &[sec, frac]).map_err(fault)
    }
}

/// Taille d'un `timespec`/`timeval` de l'appel en cours.
#[inline]
pub(crate) fn timespec_size(t: &Thread) -> u64 {
    if time32(t) { 8 } else { 16 }
}

/// Lit un `itimerspec` (ou `itimerval`) : `[intervalle s, ns|µs, valeur s, ns|µs]`.
pub(crate) fn get_itimerspec(t: &Thread, p: u64) -> Result<[i64; 4], i64> {
    let a = get_timespec(t, p)?;
    let b = get_timespec(t, p + timespec_size(t))?;
    Ok([a[0], a[1], b[0], b[1]])
}

/// Écrit un `itimerspec` (ou `itimerval`).
pub(crate) fn put_itimerspec(t: &Thread, p: u64, v: [i64; 4]) -> Result<(), i64> {
    put_timespec(t, p, v[0], v[1])?;
    put_timespec(t, p + timespec_size(t), v[2], v[3])
}

/// Écrit un `siginfo_t` (`compat_siginfo` pour un appel i386).
pub(crate) fn put_siginfo(t: &Thread, p: u64, info: &crate::signal::SigInfo) -> Result<(), i64> {
    if active(t) {
        t.mm().copy_to_user(p as usize, &crate::signal32::to_compat(info)).map_err(fault)
    } else {
        t.mm().copy_to_user(p as usize, &info.bytes).map_err(fault)
    }
}

/// Appels i386 « time32 » (structures de temps 32 bits).
fn is_time32(nr: u32) -> bool {
    matches!(
        nr,
        13 | 25 | 30 | 78 | 79 | 82 | 104 | 105 | 124 | 142 | 161 | 162 | 177 | 240 | 247 | 259..=267
            | 271 | 279 | 280 | 299 | 308 | 309 | 320 | 325 | 326 | 337 | 343 | 385
    )
}

// --- TLS : `set_thread_area`, `get_thread_area`, `clone(CLONE_SETTLS)` ----------------------

/// `struct user_desc` lu.
struct UserDesc {
    entry: u32,
    base: u32,
    limit: u32,
    flags: u32,
}

impl UserDesc {
    fn seg_32bit(&self) -> u32 { self.flags & 1 }
    fn contents(&self) -> u32 { (self.flags >> 1) & 3 }
    fn read_exec_only(&self) -> u32 { (self.flags >> 3) & 1 }
    fn limit_in_pages(&self) -> u32 { (self.flags >> 4) & 1 }
    fn seg_not_present(&self) -> u32 { (self.flags >> 5) & 1 }
    fn useable(&self) -> u32 { (self.flags >> 6) & 1 }
    fn lm(&self) -> u32 { (self.flags >> 7) & 1 }

    /// `LDT_empty` ou `LDT_zero` : demande d'effacement.
    fn clears(&self) -> bool {
        let zero = self.base == 0 && self.limit == 0 && self.flags & 0xff == 0;
        let empty = self.base == 0 && self.limit == 0 && self.contents() == 0 && self.read_exec_only() == 1
            && self.seg_32bit() == 0 && self.limit_in_pages() == 0 && self.seg_not_present() == 1
            && self.useable() == 0 && self.lm() == 0;
        zero || empty
    }

    /// `tls_desc_okay` : segments de données 32 bits présents seulement.
    fn okay(&self) -> bool {
        self.clears() || (self.seg_32bit() == 1 && self.contents() <= 1 && self.seg_not_present() == 0)
    }

    /// Descripteur de la GDT (`fill_ldt`).
    fn descriptor(&self) -> u64 {
        if self.clears() {
            return 0;
        }
        let ty = (((self.read_exec_only() ^ 1) << 1) | (self.contents() << 2) | 1) as u64;
        (self.limit as u64 & 0xffff)
            | (self.base as u64 & 0xff_ffff) << 16
            | ty << 40
            | 1 << 44 // segment de code ou de données
            | 3 << 45 // DPL 3
            | ((self.seg_not_present() ^ 1) as u64) << 47
            | ((self.limit as u64 >> 16) & 0xf) << 48
            | (self.useable() as u64) << 52
            | (self.lm() as u64) << 53
            | (self.seg_32bit() as u64) << 54
            | (self.limit_in_pages() as u64) << 55
            | (self.base as u64 >> 24) << 56
    }
}

fn read_user_desc(t: &Thread, p: u64) -> Result<UserDesc, i64> {
    let w = t.mm().read_user::<[u32; 4]>(p as usize).map_err(fault)?;
    Ok(UserDesc { entry: w[0], base: w[1], limit: w[2], flags: w[3] })
}

fn seg_state() -> Option<&'static uentry::SegState> {
    crate::user_context().map(|c| &c.seg)
}

/// `clone(CLONE_SETTLS)` i386 : entrée TLS (0..3) et descripteur du `user_desc` (l'indice
/// doit être donné : pas d'allocation, comme `do_set_thread_area(p, -1, utls, 0)`).
pub(crate) fn clone_tls(t: &Thread, p: u64) -> Result<(usize, u64), i64> {
    let d = read_user_desc(t, p)?;
    if !d.okay() {
        return Err(EINVAL);
    }
    let idx = (d.entry as usize).wrapping_sub(uentry::TLS_ENTRY_MIN);
    if idx >= uentry::TLS_ENTRIES {
        return Err(EINVAL);
    }
    Ok((idx, d.descriptor()))
}

/// `set_thread_area(u_info)`.
fn set_thread_area(t: &Thread, regs: &mut UserRegs, p: u64) -> SysResult {
    let d = read_user_desc(t, p)?;
    if !d.okay() {
        return Err(EINVAL);
    }
    let seg = seg_state().ok_or(EINVAL)?;
    let idx = if d.entry == u32::MAX {
        // Première entrée libre, écrite en retour dans `entry_number`.
        let (_, tls) = seg.snapshot(true);
        let free = tls.iter().position(|&x| x == 0).ok_or(ESRCH)?;
        t.mm().write_user::<u32>(p as usize, &((uentry::TLS_ENTRY_MIN + free) as u32)).map_err(fault)?;
        free
    } else {
        let i = (d.entry as usize).wrapping_sub(uentry::TLS_ENTRY_MIN);
        if i >= uentry::TLS_ENTRIES {
            return Err(EINVAL);
        }
        i
    };
    let (fs, gs) = uentry::set_tls(seg, idx, d.descriptor(), true);
    if let Some(b) = fs {
        regs.fs_base = b;
    }
    if let Some(b) = gs {
        regs.gs_base = b;
    }
    Ok(0)
}

/// `get_thread_area(u_info)` (`fill_user_desc`).
fn get_thread_area(t: &Thread, p: u64) -> SysResult {
    let entry = t.mm().read_user::<u32>(p as usize).map_err(fault)?;
    let idx = (entry as usize).wrapping_sub(uentry::TLS_ENTRY_MIN);
    if idx >= uentry::TLS_ENTRIES {
        return Err(EINVAL);
    }
    let (_, tls) = seg_state().ok_or(EINVAL)?.snapshot(true);
    let d = tls[idx];
    let ty = (d >> 40) & 0xf;
    let limit = (d & 0xffff) | ((d >> 48) & 0xf) << 16;
    let flags = ((d >> 54) & 1)                // seg_32bit
        | (ty >> 2) << 1                       // contents
        | ((ty & 2 == 0) as u64) << 3          // read_exec_only
        | ((d >> 55) & 1) << 4                 // limit_in_pages
        | (((d >> 47) & 1) ^ 1) << 5           // seg_not_present
        | ((d >> 52) & 1) << 6                 // useable
        | ((d >> 53) & 1) << 7; // lm
    let raw = [entry, uentry::descriptor_base(d), limit as u32, flags as u32];
    t.mm().write_user(p as usize, &raw).map_err(fault)?;
    Ok(0)
}

// --- Adaptateurs ------------------------------------------------------------------------------

/// `mmap` i386 (`old_mmap`) : arguments dans une structure de six mots.
fn old_mmap(t: &Thread, p: u64) -> SysResult {
    let w = t.mm().read_user::<[u32; 6]>(p as usize).map_err(fault)?;
    if w[5] & 0xfff != 0 {
        return Err(EINVAL);
    }
    mem::mmap(t, w[0] as u64, w[1] as u64, w[2] as u64, w[3] as u64, w[4] as i32 as i64 as u64, w[5] as u64)
}

/// `getrlimit`/`ugetrlimit`/`setrlimit` i386 : `struct compat_rlimit` (infini = 0xffffffff).
fn getrlimit32(t: &Thread, resource: u64, p: u64) -> SysResult {
    let lim = sys::rlimit_of(t, 0, resource)?;
    let c = |v: u64| if v > u32::MAX as u64 { u32::MAX } else { v as u32 };
    t.mm().write_user(p as usize, &[c(lim[0]), c(lim[1])]).map_err(fault)?;
    Ok(0)
}

/// Appels d'identité 16 bits (`*16`) : uid/gid tronqués, -1 (0xffff) conservé.
fn uid16_in(v: u64) -> u64 {
    let v = v as u16;
    if v == 0xffff { u32::MAX as u64 } else { v as u64 }
}

fn uid16_out(v: i64) -> i64 {
    if v > 0xffff { 65534 } else { v } // `high2lowuid` : uid de débordement
}

/// `getresuid16`/`getresgid16`.
fn getres16(t: &Thread, gid: bool, p: [u64; 3]) -> SysResult {
    let c = t.process.creds.lock().clone();
    let v = if gid {
        [c.rgid, c.egid, c.sgid].map(|k| c.user_ns.from_kgid_munged(k))
    } else {
        [c.ruid, c.euid, c.suid].map(|k| c.user_ns.from_kuid_munged(k))
    };
    for (ptr, v) in p.into_iter().zip(v) {
        let v = uid16_out(v as i64) as u16;
        t.mm().write_user::<u16>(ptr as usize, &v).map_err(fault)?;
    }
    Ok(0)
}

/// `getgroups16`.
fn getgroups16(t: &Thread, size: u64, list: u64) -> SysResult {
    let groups: alloc::vec::Vec<u32> = {
        let c = t.process.creds.lock();
        c.groups.iter().map(|&g| c.user_ns.from_kgid_munged(g)).collect()
    };
    let size = size as i32;
    if size < 0 {
        return Err(EINVAL);
    }
    if size == 0 {
        return Ok(groups.len() as i64);
    }
    if (size as usize) < groups.len() {
        return Err(EINVAL);
    }
    let raw: alloc::vec::Vec<u8> = groups.iter().flat_map(|&g| (uid16_out(g as i64) as u16).to_le_bytes()).collect();
    t.mm().copy_to_user(list as usize, &raw).map_err(fault)?;
    Ok(groups.len() as i64)
}

/// `_llseek(fd, haut, bas, résultat, whence)`.
fn llseek(t: &Thread, fd: u64, hi: u64, lo: u64, result: u64, whence: u64) -> SysResult {
    let off = ((hi << 32) | lo) as i64;
    let pos = fs::lseek(t, fd, off, whence)?;
    t.mm().write_user::<i64>(result as usize, &pos).map_err(fault)?;
    Ok(0)
}

/// `socketcall(appel, args)` : multiplexeur des sockets i386.
fn socketcall(t: &Thread, call: u64, args: u64) -> SysResult {
    const NARGS: [usize; 21] = [0, 3, 3, 3, 2, 3, 3, 3, 4, 4, 4, 6, 6, 2, 5, 5, 3, 3, 4, 5, 4];
    let n = *NARGS.get(call as usize).filter(|&&n| n > 0).ok_or(EINVAL)?;
    let mut a = [0u64; 6];
    for (i, v) in a.iter_mut().take(n).enumerate() {
        *v = t.mm().read_user::<u32>(args as usize + i * 4).map_err(fault)? as u64;
    }
    use crate::socket as s;
    match call {
        1 => s::socket(t, a[0], a[1], a[2]),
        2 => s::bind(t, a[0], a[1], a[2]),
        3 => s::connect(t, a[0], a[1], a[2]),
        4 => s::listen(t, a[0], a[1]),
        5 => s::accept4(t, a[0], a[1], a[2], 0),
        6 => s::getsockname(t, a[0], a[1], a[2]),
        7 => s::getpeername(t, a[0], a[1], a[2]),
        8 => s::socketpair(t, a[0], a[1], a[2], a[3]),
        9 => s::sendto(t, a[0], a[1], a[2], a[3], 0, 0),
        10 => s::recvfrom(t, a[0], a[1], a[2], a[3], 0, 0),
        11 => s::sendto(t, a[0], a[1], a[2], a[3], a[4], a[5]),
        12 => s::recvfrom(t, a[0], a[1], a[2], a[3], a[4], a[5]),
        13 => s::shutdown(t, a[0], a[1]),
        14 => s::setsockopt(t, a[0], a[1], a[2], a[3], a[4]),
        15 => s::getsockopt(t, a[0], a[1], a[2], a[3], a[4]),
        16 => s::sendmsg(t, a[0], a[1], a[2]),
        17 => s::recvmsg(t, a[0], a[1], a[2]),
        18 => s::accept4(t, a[0], a[1], a[2], a[3]),
        19 => {
            t.time32.store(true, Ordering::Relaxed);
            s::recvmmsg(t, a[0], a[1], a[2], a[3], a[4])
        }
        20 => s::sendmmsg(t, a[0], a[1], a[2], a[3]),
        _ => Err(EINVAL),
    }
}

/// `ipc(appel, a, b, c, ptr, fifth)` : multiplexeur System V i386 (mémoire partagée).
fn ipc(t: &Thread, call: u64, first: u64, second: u64, third: u64, ptr: u64) -> SysResult {
    let version = call >> 16;
    match call & 0xffff {
        // SHMAT : l'adresse est rendue par `*third` (version 0).
        21 => {
            if version == 1 {
                return Err(EINVAL);
            }
            let addr = crate::ipc::shmat(t, first as i32 as i64, ptr, second)?;
            t.mm().write_user::<u32>(third as usize, &(addr as u32)).map_err(fault)?;
            Ok(0)
        }
        22 => crate::ipc::shmdt(t, ptr),
        23 => crate::ipc::shmget(t, first as i32, second, third),
        24 => shmctl32(t, first as i32 as i64, second, ptr),
        _ => Err(ENOSYS),
    }
}

/// `fcntl` / `fcntl64` i386 : verrous au format `compat_flock` (`F_GETLK`…) ou
/// `compat_flock64` (`F_GETLK64` = 12…14, `F_OFD_*`), le reste comme en 64 bits.
fn fcntl32(t: &Thread, fd: u64, cmd: u64, arg: u64) -> SysResult {
    use crate::reclock::{FlockLayout, F_OFD_GETLK, F_OFD_SETLK, F_OFD_SETLKW};
    let (cmd, layout) = match cmd {
        5..=7 => (cmd, FlockLayout::Compat32),
        12..=14 => (cmd - 7, FlockLayout::Compat64),
        c if c == F_OFD_GETLK || c == F_OFD_SETLK || c == F_OFD_SETLKW => (cmd, FlockLayout::Compat64),
        _ => return fs::fcntl(t, fd, cmd, arg),
    };
    let f = fs::get_fd(t, fd)?;
    crate::reclock::fcntl_lock(t, &f, cmd, arg, layout)
}

/// `shmctl` i386 : `ipc::shmctl` écrit `compat_shmid64_ds` (avec `IPC_64`) ou l'ancien
/// `compat_shmid_ds`, et les `shminfo`/`shm_info` 32 bits.
fn shmctl32(t: &Thread, id: i64, cmd: u64, buf: u64) -> SysResult {
    crate::ipc::shmctl(t, id, cmd, buf)
}

/// Ancien `select` (appel 82) : un seul argument, `struct sel_arg_struct` { n, inp, outp,
/// exp, tvp } de mots de 32 bits ; délai `timeval` 32 bits.
fn old_select(t: &Thread, arg: u64) -> SysResult {
    let a = t.mm().read_user::<[u32; 5]>(arg as usize).map_err(fault)?;
    fs::select(t, a[0] as u64, a[1] as u64, a[2] as u64, a[3] as u64, a[4] as u64)
}

/// `stime(&t)` (appel 25) : règle l'horloge à la seconde (`time_t` de 32 bits).
fn stime(t: &Thread, p: u64) -> SysResult {
    let s = t.mm().read_user::<i32>(p as usize).map_err(fault)?;
    if s < 0 {
        return Err(EINVAL);
    }
    if !crate::userns::current_capable(t, crate::userns::CAP_SYS_TIME) {
        return Err(EPERM);
    }
    sys::set_realtime(s as i64, 0);
    Ok(0)
}

/// `nice(inc)` (appel 34, absent de x86_64) : `setpriority(PRIO_PROCESS, 0, nice + inc)`,
/// borné à [-20, 19] ; baisser le nice demande les droits de `setpriority`.
fn nice(t: &Thread, inc: i64) -> SysResult {
    // `getpriority` rend 20 − nice.
    let cur = 20 - super::prio::getpriority(t, 0, 0)?;
    let new = (cur + inc.clamp(-40, 40)).clamp(-20, 19);
    // `sys_nice` refuse par `EPERM` (`can_nice`), là où `setpriority` dit `EACCES`.
    super::prio::setpriority(t, 0, 0, new as u64).map_err(|e| if e == EACCES { EPERM } else { e })?;
    Ok(0)
}

/// `arch_prctl` i386 : seules les commandes communes aux deux modes (`ARCH_GET_CPUID`,
/// `ARCH_SET_CPUID`) ; les bases `fs`/`gs` passent par `set_thread_area`.
fn arch_prctl32(t: &Thread, regs: &mut UserRegs, code: u64, addr: u64) -> SysResult {
    match code {
        0x1011 | 0x1012 => sys::arch_prctl(t, regs, code, addr),
        _ => Err(EINVAL),
    }
}

/// `waitpid(pid, status, options)`.
fn waitpid(t: &Thread, pid: i64, status: u64, options: u64) -> SysResult {
    proc::wait4(t, pid, status, options, 0)
}

/// `set_robust_list` i386 : tête de liste de 12 octets.
fn set_robust_list32(t: &Thread, head: u64, len: u64) -> SysResult {
    if len != 12 {
        return Err(EINVAL);
    }
    t.robust_list.store(head as usize, Ordering::Release);
    t.robust_compat.store(true, Ordering::Release);
    Ok(0)
}

/// `get_robust_list` i386.
fn get_robust_list32(t: &Thread, pid: u64, head_ptr: u64, len_ptr: u64) -> SysResult {
    let target = if pid == 0 { None } else { Some(crate::find_thread_v(t, pid as usize).ok_or(ESRCH)?) };
    let head = match &target {
        Some(th) => th.robust_list.load(Ordering::Acquire),
        None => t.robust_list.load(Ordering::Acquire),
    };
    t.mm().write_user::<u32>(head_ptr as usize, &(head as u32)).map_err(fault)?;
    t.mm().write_user::<u32>(len_ptr as usize, &12).map_err(fault)?;
    Ok(0)
}

/// `sigprocmask` i386 (masque de 32 bits : les signaux 1 à 32 seulement).
fn sigprocmask32(t: &Thread, how: u64, set: u64, oset: u64) -> SysResult {
    let old = t.sigmask.load(Ordering::Acquire);
    if set != 0 {
        let v = t.mm().read_user::<u32>(set as usize).map_err(fault)? as u64;
        let new = match how {
            0 => old | v,
            1 => old & !v,
            2 => (old & !0xffff_ffff) | v,
            _ => return Err(EINVAL),
        };
        t.sigmask.store(crate::signal::sanitize_mask(new), Ordering::Release);
        crate::signal::recalc(t);
    }
    if oset != 0 {
        t.mm().write_user::<u32>(oset as usize, &(old as u32)).map_err(fault)?;
    }
    Ok(0)
}

/// `sigreturn` / `rt_sigreturn` i386.
fn sigreturn32(t: &Thread, regs: &mut UserRegs, rt: bool) -> Outcome {
    let r = if rt { crate::signal32::rt_sigreturn(t, regs) } else { crate::signal32::sigreturn(t, regs) };
    if r.is_err() {
        log::warn!("maiproc : pid {} {}sigreturn i386 : cadre invalide -> SIGSEGV", t.process.pid, if rt { "rt_" } else { "" });
        if let Some(me) = crate::current_thread_arc(t) {
            crate::signal::send_to_thread(&me, crate::signal::SigInfo::new(crate::signal::SIGSEGV, crate::signal::SI_KERNEL), true);
        }
    }
    Outcome::Keep
}

// --- Aiguillage -------------------------------------------------------------------------------

/// Aiguillage d'un appel i386.
pub fn dispatch(t: &Thread, regs: &mut UserRegs) -> Outcome {
    let nr = regs.rax as u32;
    let mut a = [regs.rbx, regs.rcx, regs.rdx, regs.rsi, regs.rdi, regs.rbp].map(|v| v as u32 as u64);
    if regs.vector == uentry::SYSCALL32_VECTOR {
        // `syscall` 32 bits (vDSO) : 2e argument dans `ebp` (`ecx` porte le retour), 6e
        // argument (l'`ebp` d'origine) empilé par le vDSO.
        a[1] = regs.rbp as u32 as u64;
        a[5] = match t.mm().read_user::<u32>(regs.rsp as u32 as usize) {
            Ok(v) => v as u64,
            Err(_) => return Outcome::Return(-EFAULT),
        };
        regs.rcx = a[1];
        regs.rbp = a[5];
    }
    let a = a;
    t.time32.store(is_time32(nr), Ordering::Relaxed);
    let o = dispatch_inner(t, regs, nr, &a);
    t.time32.store(false, Ordering::Relaxed);
    o
}

fn dispatch_inner(t: &Thread, regs: &mut UserRegs, nr: u32, a: &[u64; 6]) -> Outcome {
    // Argument signé (descripteur de dossier, décalage `off_t`, pid, délai).
    let s = |i: usize| a[i] as u32 as i32 as i64;
    // Valeur 64 bits en deux registres (bas, haut).
    let pair = |lo: usize, hi: usize| (a[lo] | (a[hi] << 32)) as i64;
    const AT_FDCWD: i64 = fs::AT_FDCWD;
    let r: SysResult = match nr {
        // --- Fichiers ---
        3 => fs::read(t, a[0], a[1], a[2]),
        4 => fs::write(t, a[0], a[1], a[2]),
        5 => fs::openat(t, AT_FDCWD, a[0], a[1], a[2]),
        6 => fs::close(t, a[0]),
        8 => fs::openat(t, AT_FDCWD, a[0], 0o1101, a[1]), // creat
        9 => fs::linkat(t, AT_FDCWD, a[0], AT_FDCWD, a[1], 0),
        10 => fs::unlinkat(t, AT_FDCWD, a[0], 0),
        12 => fs::chdir(t, a[0]),
        14 => fs::mknodat(t, AT_FDCWD, a[0], a[1], a[2]),
        15 => fs::meta_change_path(t, AT_FDCWD, a[0], 0, fs::MetaArg::Mode(a[1])),
        16 => fs::meta_change_path(t, AT_FDCWD, a[0], fs::AT_SYMLINK_NOFOLLOW, fs::MetaArg::Owner(uid16_in(a[1]), uid16_in(a[2]))),
        19 => fs::lseek(t, a[0], s(1), a[2]),
        30 => fs::meta_change_path(t, AT_FDCWD, a[0], 0, fs::MetaArg::Utime(a[1])),
        33 => fs::faccessat(t, AT_FDCWD, a[0], a[1], 0),
        36 => Ok(0), // sync
        38 => fs::renameat(t, AT_FDCWD, a[0], AT_FDCWD, a[1]),
        39 => fs::mkdirat(t, AT_FDCWD, a[0], a[1]),
        40 => fs::unlinkat(t, AT_FDCWD, a[0], fs::AT_REMOVEDIR),
        41 => fs::dup(t, a[0]),
        42 => fs::pipe2(t, a[0], 0),
        54 => fs::ioctl(t, a[0], a[1], a[2]),
        55 | 221 => fcntl32(t, a[0], a[1], a[2]),
        60 => Ok(t.process.umask.swap(a[0] as u32 & 0o777, Ordering::AcqRel) as i64),
        61 => crate::mount::sys_chroot(t, a[0]),
        21 => crate::mount::sys_mount(t, a[0], a[1], a[2], a[3], a[4]),
        22 => crate::mount::sys_umount2(t, a[0], 0),
        52 => crate::mount::sys_umount2(t, a[0], a[1]),
        217 => crate::mount::sys_pivot_root(t, a[0], a[1]),
        310 => crate::ns::unshare(t, a[0]),
        346 => crate::ns::setns(t, a[0], a[1]),
        184 => sys::capget(t, a[0], a[1]),
        185 => sys::capset(t, a[0], a[1]),
        63 => fs::dup3(t, a[0], a[1], 0, true),
        83 => fs::symlinkat(t, a[0], AT_FDCWD, a[1]),
        85 => fs::readlinkat(t, AT_FDCWD, a[0], a[1], a[2]),
        92 => fs::truncate_path(t, a[0], s(1)),
        93 => fs::ftruncate(t, a[0], s(1)),
        94 => fs::meta_change_fd(t, a[0], fs::MetaArg::Mode(a[1])),
        95 => fs::meta_change_fd(t, a[0], fs::MetaArg::Owner(uid16_in(a[1]), uid16_in(a[2]))),
        106 => fs::fstatat32(t, AT_FDCWD, a[0], a[1], 0, Stat32::Old),
        107 => fs::fstatat32(t, AT_FDCWD, a[0], a[1], fs::AT_SYMLINK_NOFOLLOW, Stat32::Old),
        108 => fs::fstat32(t, a[0], a[1], Stat32::Old),
        118 | 148 => fs::fsync(t, a[0]),
        133 => fs::fchdir(t, a[0]),
        140 => llseek(t, a[0], a[1], a[2], a[3], a[4]),
        143 => fs::flock(t, a[0], a[1]),
        145 => fs::readv(t, a[0], a[1], a[2], None),
        146 => fs::writev(t, a[0], a[1], a[2], None),
        168 => fs::poll(t, a[0], a[1], s(2)),
        180 => fs::pread(t, a[0], a[1], a[2], pair(3, 4)),
        181 => fs::pwrite(t, a[0], a[1], a[2], pair(3, 4)),
        182 => fs::meta_change_path(t, AT_FDCWD, a[0], 0, fs::MetaArg::Owner(uid16_in(a[1]), uid16_in(a[2]))),
        183 => fs::getcwd(t, a[0], a[1]),
        193 => fs::truncate_path(t, a[0], pair(1, 2)),
        194 => fs::ftruncate(t, a[0], pair(1, 2)),
        195 => fs::fstatat32(t, AT_FDCWD, a[0], a[1], 0, Stat32::Stat64),
        196 => fs::fstatat32(t, AT_FDCWD, a[0], a[1], fs::AT_SYMLINK_NOFOLLOW, Stat32::Stat64),
        197 => fs::fstat32(t, a[0], a[1], Stat32::Stat64),
        198 => fs::meta_change_path(t, AT_FDCWD, a[0], fs::AT_SYMLINK_NOFOLLOW, fs::MetaArg::Owner(a[1], a[2])),
        207 => fs::meta_change_fd(t, a[0], fs::MetaArg::Owner(a[1], a[2])),
        212 => fs::meta_change_path(t, AT_FDCWD, a[0], 0, fs::MetaArg::Owner(a[1], a[2])),
        141 => fs::getdents32(t, a[0], a[1], a[2]),
        220 => fs::getdents64(t, a[0], a[1], a[2]),
        82 => old_select(t, a[0]),
        142 => fs::select(t, a[0], a[1], a[2], a[3], a[4]),
        187 => fs::sendfile32(t, a[0], a[1], a[2], a[3]),
        271 => fs::meta_change_path(t, AT_FDCWD, a[0], 0, fs::MetaArg::Utimes(a[1])),
        299 => fs::meta_change_path(t, s(0), a[1], 0, fs::MetaArg::Utimes(a[2])), // futimesat
        // statfs/fstatfs : `compat_statfs` ; statfs64/fstatfs64 (taille en 2e argument).
        99 => fs::statfs_path(t, a[0], a[1], fs::StatfsLayout::Compat),
        100 => fs::fstatfs(t, a[0], a[1], fs::StatfsLayout::Compat),
        268 if a[1] != 84 => Err(EINVAL),
        268 => fs::statfs_path(t, a[0], a[2], fs::StatfsLayout::Compat64),
        269 if a[1] != 84 => Err(EINVAL),
        269 => fs::fstatfs(t, a[0], a[2], fs::StatfsLayout::Compat64),
        225 => fs::readahead(t, a[0], pair(1, 2), a[3]),
        226..=237 => Err(EOPNOTSUPP), // attributs étendus
        239 => fs::sendfile(t, a[0], a[1], a[2], a[3]), // sendfile64 : `loff_t *`
        250 => fs::fadvise64(t, a[0], pair(1, 2), a[3] as u32 as i32 as i64, a[4]),
        272 => fs::fadvise64(t, a[0], pair(1, 2), pair(3, 4), a[5]),
        295 => fs::openat(t, s(0), a[1], a[2], a[3]),
        296 => fs::mkdirat(t, s(0), a[1], a[2]),
        297 => fs::mknodat(t, s(0), a[1], a[2], a[3]),
        298 => fs::meta_change_path(t, s(0), a[1], a[4], fs::MetaArg::Owner(a[2], a[3])),
        300 => fs::fstatat32(t, s(0), a[1], a[2], a[3], Stat32::Stat64),
        301 => fs::unlinkat(t, s(0), a[1], a[2]),
        302 => fs::renameat(t, s(0), a[1], s(2), a[3]),
        303 => fs::linkat(t, s(0), a[1], s(2), a[3], a[4]),
        304 => fs::symlinkat(t, a[0], s(1), a[2]),
        305 => fs::readlinkat(t, s(0), a[1], a[2], a[3]),
        306 => fs::meta_change_path(t, s(0), a[1], 0, fs::MetaArg::Mode(a[2])),
        307 | 439 => fs::faccessat(t, s(0), a[1], a[2], a[3]),
        308 => fs::pselect6(t, a[0], a[1], a[2], a[3], a[4], a[5]),
        309 | 414 => fs::ppoll(t, a[0], a[1], a[2], a[3], a[4]),
        313 => fs::splice(t, a[0], a[1], a[2], a[3], a[4], a[5]),
        320 | 412 if a[1] == 0 => fs::meta_change_fd(t, a[0], fs::MetaArg::Utimens(a[2])),
        320 | 412 => fs::meta_change_path(t, s(0), a[1], a[3], fs::MetaArg::Utimens(a[2])),
        324 => fs::fallocate(t, a[0], a[1], pair(2, 3), pair(4, 5)),
        330 => fs::dup3(t, a[0], a[1], a[2], false),
        331 => fs::pipe2(t, a[0], a[1]),
        333 => fs::readv(t, a[0], a[1], a[2], Some(pair(3, 4))),
        334 => fs::writev(t, a[0], a[1], a[2], Some(pair(3, 4))),
        344 => fs::fsync(t, a[0]),
        353 => fs::renameat2(t, s(0), a[1], s(2), a[3], a[4]),
        356 => fs::memfd_create(t, a[0], a[1]),
        377 => fs::copy_file_range(t, a[0], a[1], a[2], a[3], a[4], a[5]),
        378 => fs::rw_iov2(t, a[0], a[1], a[2], pair(3, 4), a[5], false),
        379 => fs::rw_iov2(t, a[0], a[1], a[2], pair(3, 4), a[5], true),
        383 => fs::statx(t, s(0), a[1], a[2], a[3], a[4]),
        413 => fs::pselect6(t, a[0], a[1], a[2], a[3], a[4], a[5]),
        436 => fs::close_range(t, a[0], a[1], a[2]),
        437 => fs::openat2(t, s(0), a[1], a[2], a[3]),
        452 => fs::meta_change_path(t, s(0), a[1], a[3], fs::MetaArg::Mode(a[2])),

        // --- Sockets (appels directs, Linux 4.3+) ---
        102 => socketcall(t, a[0], a[1]),
        359 => crate::socket::socket(t, a[0], a[1], a[2]),
        360 => crate::socket::socketpair(t, a[0], a[1], a[2], a[3]),
        361 => crate::socket::bind(t, a[0], a[1], a[2]),
        362 => crate::socket::connect(t, a[0], a[1], a[2]),
        363 => crate::socket::listen(t, a[0], a[1]),
        364 => crate::socket::accept4(t, a[0], a[1], a[2], a[3]),
        365 => crate::socket::getsockopt(t, a[0], a[1], a[2], a[3], a[4]),
        366 => crate::socket::setsockopt(t, a[0], a[1], a[2], a[3], a[4]),
        367 => crate::socket::getsockname(t, a[0], a[1], a[2]),
        368 => crate::socket::getpeername(t, a[0], a[1], a[2]),
        369 => crate::socket::sendto(t, a[0], a[1], a[2], a[3], a[4], a[5]),
        370 => crate::socket::sendmsg(t, a[0], a[1], a[2]),
        371 => crate::socket::recvfrom(t, a[0], a[1], a[2], a[3], a[4], a[5]),
        372 => crate::socket::recvmsg(t, a[0], a[1], a[2]),
        373 => crate::socket::shutdown(t, a[0], a[1]),
        337 | 417 => crate::socket::recvmmsg(t, a[0], a[1], a[2], a[3], a[4]),
        345 => crate::socket::sendmmsg(t, a[0], a[1], a[2], a[3]),

        // --- Événements ---
        254 => crate::epoll::epoll_create(t, a[0]),
        329 => crate::epoll::epoll_create1(t, a[0]),
        255 => crate::epoll::epoll_ctl(t, a[0], a[1], a[2], a[3]),
        256 => crate::epoll::epoll_wait(t, a[0], a[1], a[2], super::ms_timeout(a[3])),
        319 => {
            if a[4] != 0 && a[5] != 8 {
                Err(EINVAL)
            } else {
                fs::with_temp_mask(t, a[4], || crate::epoll::epoll_wait(t, a[0], a[1], a[2], super::ms_timeout(a[3])))
            }
        }
        441 => {
            if a[4] != 0 && a[5] != 8 {
                Err(EINVAL)
            } else {
                match fs::read_ts(t, a[3]) {
                    Ok(tmo) => fs::with_temp_mask(t, a[4], || crate::epoll::epoll_wait(t, a[0], a[1], a[2], tmo)),
                    Err(e) => Err(e),
                }
            }
        }
        323 => crate::eventfd::eventfd2(t, a[0], 0),
        328 => crate::eventfd::eventfd2(t, a[0], a[1]),
        322 => crate::timerfd::timerfd_create(t, a[0], a[1]),
        410 => crate::timerfd::timerfd_gettime(t, a[0], a[1]),
        411 => crate::timerfd::timerfd_settime(t, a[0], a[1], a[2], a[3]),
        321 => crate::signalfd::signalfd4(t, a[0], a[1], a[2], 0),
        327 => crate::signalfd::signalfd4(t, a[0], a[1], a[2], a[3]),
        291 => crate::inotify::inotify_init1(t, 0),
        332 => crate::inotify::inotify_init1(t, a[0]),
        292 => crate::inotify::inotify_add_watch(t, a[0], a[1], a[2]),
        293 => crate::inotify::inotify_rm_watch(t, a[0], a[1]),
        434 => crate::pidfd::pidfd_open(t, a[0], a[1]),
        424 => crate::pidfd::pidfd_send_signal(t, a[0], a[1], a[2], a[3]),
        438 => crate::pidfd::pidfd_getfd(t, a[0], a[1], a[2]),
        325 => crate::timerfd::timerfd_settime(t, a[0], a[1], a[2], a[3]),
        326 => crate::timerfd::timerfd_gettime(t, a[0], a[1]),
        // io_uring : anneau marqué i386 à sa création (`Ring::compat`).
        425 => crate::uring::io_uring_setup(t, a[0], a[1]),
        426 => crate::uring::io_uring_enter(t, a[0], a[1], a[2], a[3], a[4], a[5]),
        427 => crate::uring::io_uring_register(t, a[0], a[1], a[2], a[3]),

        // --- Mémoire ---
        45 => Ok(t.mm().brk(a[0] as usize) as i64),
        90 => old_mmap(t, a[0]),
        91 => mem::munmap(t, a[0], a[1]),
        125 => mem::mprotect(t, a[0], a[1], a[2]),
        144 => mem::msync(t, a[0], a[1], a[2]),
        150..=153 | 376 => Ok(0), // mlock, munlock, mlockall, munlockall, mlock2
        163 => mem::mremap(t, a[0], a[1], a[2], a[3], a[4]),
        192 => mem::mmap(t, a[0], a[1], a[2], a[3], s(4) as u64, a[5] << 12), // mmap2 : pages
        218 => mem::mincore(t, a[0], a[1], a[2]),
        219 => mem::madvise(t, a[0], a[1], a[2]),
        // IPC System V (appels directs, Linux 5.1+) et multiplexeur.
        117 => ipc(t, a[0], a[1], a[2], a[3], a[4]),
        395 => crate::ipc::shmget(t, a[0] as i32, a[1], a[2]),
        396 => shmctl32(t, s(0), a[1], a[2]),
        397 => crate::ipc::shmat(t, s(0), a[1], a[2]),
        398 => crate::ipc::shmdt(t, a[0]),

        // --- Processus ---
        1 => return proc::exit(a[0] as i32),
        252 => return proc::exit_group(t, a[0] as i32),
        2 => proc::fork(t, regs),
        190 => proc::vfork(t, regs),
        // clone i386 : (flags, pile, parent_tid, tls, child_tid).
        120 => proc::clone(t, regs, a[0], a[1], a[2], a[4], a[3]),
        435 => proc::clone3(t, regs, a[0], a[1]),
        11 => match proc::execve(t, regs, AT_FDCWD, a[0], a[1], a[2], 0) {
            Ok(o) => return o,
            Err(e) => Err(e),
        },
        358 => match proc::execve(t, regs, s(0), a[1], a[2], a[3], a[4]) {
            Ok(o) => return o,
            Err(e) => Err(e),
        },
        7 => waitpid(t, s(0), a[1], a[2]),
        114 => proc::wait4(t, s(0), a[1], a[2], a[3]),
        // waitid(idtype, id, infop, options, rusage) : `compat_siginfo`, `compat_rusage`.
        284 => proc::waitid(t, a[0], a[1], a[2], a[3], a[4]),
        20 => Ok(t.process.vpid as i64),
        64 => Ok(t.process.vnr(t.process.ppid.load(Ordering::Relaxed)) as i64),
        224 => Ok(t.vtid() as i64),
        65 => Ok(t.process.vnr(t.process.pgid.load(Ordering::Relaxed)) as i64),
        57 => proc::setpgid(t, a[0], a[1]),
        132 => proc::getpgid(t, a[0]),
        66 => proc::setsid(t),
        147 => proc::getsid(t, a[0]),
        243 => set_thread_area(t, regs, a[0]),
        244 => get_thread_area(t, a[0]),
        258 => {
            t.clear_child_tid.store(a[0] as usize, Ordering::Release);
            Ok(t.vtid() as i64)
        }
        311 => set_robust_list32(t, a[0], a[1]),
        312 => get_robust_list32(t, a[0], a[1], a[2]),
        240 | 422 => crate::futex::futex(t, a[0], a[1], a[2], a[3], a[4], a[5]),
        449 => crate::futex::futex_waitv(t, a[0], a[1], a[2], a[3], a[4]),

        // --- Identités : appels 16 bits puis 32 bits (`*32`, mêmes que x86_64) ---
        24 => sys::identity(t, 102, 0, 0, 0).map(uid16_out),
        47 => sys::identity(t, 104, 0, 0, 0).map(uid16_out),
        49 => sys::identity(t, 107, 0, 0, 0).map(uid16_out),
        50 => sys::identity(t, 108, 0, 0, 0).map(uid16_out),
        23 => sys::identity(t, 105, uid16_in(a[0]), 0, 0),
        46 => sys::identity(t, 106, uid16_in(a[0]), 0, 0),
        70 => sys::identity(t, 113, uid16_in(a[0]), uid16_in(a[1]), 0),
        71 => sys::identity(t, 114, uid16_in(a[0]), uid16_in(a[1]), 0),
        164 => sys::identity(t, 117, uid16_in(a[0]), uid16_in(a[1]), uid16_in(a[2])),
        170 => sys::identity(t, 119, uid16_in(a[0]), uid16_in(a[1]), uid16_in(a[2])),
        165 => getres16(t, false, [a[0], a[1], a[2]]),
        171 => getres16(t, true, [a[0], a[1], a[2]]),
        138 => sys::identity(t, 122, uid16_in(a[0]), 0, 0),
        139 => sys::identity(t, 123, uid16_in(a[0]), 0, 0),
        80 => getgroups16(t, a[0], a[1]),
        199 => sys::identity(t, 102, 0, 0, 0),
        200 => sys::identity(t, 104, 0, 0, 0),
        201 => sys::identity(t, 107, 0, 0, 0),
        202 => sys::identity(t, 108, 0, 0, 0),
        203 => sys::identity(t, 113, a[0], a[1], 0),
        204 => sys::identity(t, 114, a[0], a[1], 0),
        205 => sys::identity(t, 115, a[0], a[1], 0),
        206 => sys::identity(t, 116, a[0], a[1], 0),
        208 => sys::identity(t, 117, a[0], a[1], a[2]),
        209 => sys::identity(t, 118, a[0], a[1], a[2]),
        210 => sys::identity(t, 119, a[0], a[1], a[2]),
        211 => sys::identity(t, 120, a[0], a[1], a[2]),
        213 => sys::identity(t, 105, a[0], 0, 0),
        214 => sys::identity(t, 106, a[0], 0, 0),
        215 => sys::identity(t, 122, a[0], 0, 0),
        216 => sys::identity(t, 123, a[0], 0, 0),

        // --- Signaux ---
        37 => proc::kill(t, s(0), a[1] as i32),
        238 => proc::tgkill(t, -1, s(0), a[1] as i32),
        270 => proc::tgkill(t, s(0), s(1), a[2] as i32),
        29 => proc::pause(t),
        27 => proc::alarm(t, a[0]),
        119 => return sigreturn32(t, regs, false),
        173 => return sigreturn32(t, regs, true),
        174 => proc::rt_sigaction(t, a[0], a[1], a[2], a[3]),
        175 => proc::rt_sigprocmask(t, a[0], a[1], a[2], a[3]),
        126 => sigprocmask32(t, a[0], a[1], a[2]),
        176 => proc::rt_sigpending(t, a[0], a[1]),
        178 => proc::rt_sigqueueinfo(t, s(0), None, a[1] as i32, a[2]),
        335 => proc::rt_sigqueueinfo(t, s(0), Some(s(1)), a[2] as i32, a[3]),
        179 => proc::rt_sigsuspend(t, a[0], a[1]),
        177 | 421 => proc::rt_sigtimedwait(t, a[0], a[1], a[2], a[3]),
        104 => proc::setitimer(t, a[0], a[1], a[2]),
        105 => proc::getitimer(t, a[0], a[1]),
        // Minuteurs POSIX : `compat_sigevent` ; `itimerspec` 32 bits (260, 261) ou 64 (408, 409).
        259 => crate::ptimer::timer_create(t, a[0], a[1], a[2]),
        260 | 409 => crate::ptimer::timer_settime(t, a[0], a[1], a[2], a[3]),
        261 | 408 => crate::ptimer::timer_gettime(t, a[0], a[1]),
        262 => crate::ptimer::timer_getoverrun(t, a[0]),
        263 => crate::ptimer::timer_delete(t, a[0]),
        186 => proc::sigaltstack(t, regs, a[0], a[1]),
        0 => sys::restart_syscall(t),

        // --- Contrôle, limites, ordonnancement ---
        172 => sys::prctl(t, a[0], a[1], a[2], a[3], a[4]),
        76 | 191 => getrlimit32(t, a[0], a[1]),
        75 => Ok(0), // setrlimit : limites fixes
        340 => sys::getrlimit(t, a[0], a[1], a[2], a[3]),
        77 => proc::getrusage(t, s(0), a[1]),
        43 => proc::times(t, a[0]),
        34 => nice(t, s(0)),
        384 => arch_prctl32(t, regs, a[0], a[1]),
        96 => super::prio::getpriority(t, a[0], a[1]),
        97 => super::prio::setpriority(t, a[0], a[1], a[2]),
        289 => super::prio::ioprio_set(t, a[0], a[1], a[2]),
        290 => super::prio::ioprio_get(t, a[0], a[1]),
        241 => sys::sched_setaffinity(t, a[0], a[1], a[2]),
        242 => sys::sched_getaffinity(t, a[0], a[1], a[2]),
        // sched_setparam … sched_getscheduler, sched_get_priority_max/min (142..147 en x86_64).
        154..=157 => sys::sched_policy(t, nr as u64 - 12, a[0], a[1], a[2]),
        159 | 160 => sys::sched_policy(t, nr as u64 - 13, a[0], a[1], a[2]),
        74 => sys::set_uts_name(t, 0, a[0], a[1]),
        121 => sys::set_uts_name(t, 1, a[0], a[1]),
        386 => Err(ENOSYS), // rseq : la libc s'en passe

        // --- Temps (time32 : `timespec` 32 bits ; *_time64 : format 64 bits) ---
        13 => sys::time(t, a[0]),
        78 => sys::gettimeofday(t, a[0]),
        162 => sys::nanosleep(t, a[0], a[1]),
        265 | 403 => sys::clock_gettime(t, a[0], a[1]),
        266 | 406 => sys::clock_getres(t, a[0], a[1]),
        267 | 407 => sys::clock_nanosleep(t, a[0], a[1], a[2], a[3]),
        264 | 404 => sys::clock_settime(t, a[0], a[1]),
        79 => sys::settimeofday(t, a[0]),
        25 => stime(t, a[0]),

        // --- Système ---
        122 => sys::uname(t, a[0]),
        116 => sys::sysinfo(t, a[0]),
        355 => sys::getrandom(t, a[0], a[1], a[2]),
        318 => sys::getcpu(t, a[0], a[1]),
        158 => {
            task::scheduler::schedule();
            Ok(0)
        }

        _ => {
            sys::unknown(t, nr as u64 | 0x8000_0000);
            Err(ENOSYS)
        }
    };
    Outcome::Return(match r {
        Ok(v) => v,
        Err(e) => -e,
    })
}
