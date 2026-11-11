//! Signaux des gestionnaires i386 (`SA_IA32_ABI`) : cadres `sigframe_ia32` et
//! `rt_sigframe_ia32`, `sigreturn` et `rt_sigreturn` 32 bits, `siginfo` au format
//! `compat_siginfo` (comme `arch/x86/kernel/signal_32.c` de Linux).
//!
//! ```text
//!  sigframe_ia32    : pretcode, sig, sigcontext_32 (88), _fpstate_32 inutilisé (624),
//!                     extramask, retcode[8]                                  = 732 octets
//!  rt_sigframe_ia32 : pretcode, sig, pinfo, puc, compat_siginfo (128),
//!                     ucontext_ia32 (flags, link, stack_t 12, sigcontext 88, masque 8),
//!                     retcode[8]                                             = 268 octets
//! ```
//!
//! État vectoriel : en tête l'ancien format `fsave` (environnement x87 converti depuis FXSAVE
//! et 8 registres de 10 octets, `status`, `magic` = 0 « FXSR suit »), puis l'image XSAVE
//! alignée sur 64 avec son en-tête logiciel (`fx_sw_bytes`) et le marqueur final, comme
//! `fpu__alloc_mathframe` pour un cadre ia32. Au retour, l'environnement x87 de l'en-tête
//! l'emporte sur celui de l'image (`convert_to_fxsr`).

use alloc::vec;
use core::sync::atomic::Ordering;
use uentry::{UserRegs, FORCE_IRET_VECTOR};

use crate::signal::{self, sanitize_mask, sigbit, AltStack, BadFrame, SigAction, SigInfo};
use crate::Thread;

const SIGCONTEXT_SIZE: usize = 88;
const FPSTATE32_SIZE: usize = 624;
const FSAVE_HDR: usize = 112;
const SIGFRAME_SIZE: usize = 8 + SIGCONTEXT_SIZE + FPSTATE32_SIZE + 4 + 8;
const RT_SIGFRAME_SIZE: usize = 16 + 128 + 20 + SIGCONTEXT_SIZE + 8 + 8;
const NR_SIGRETURN: u32 = 119;
const NR_RT_SIGRETURN: u32 = 173;
const UC_FP_XSTATE: u32 = 1;
const RFLAGS_TF: u64 = 1 << 8;
const RFLAGS_DF: u64 = 1 << 10;
const RFLAGS_RF: u64 = 1 << 16;

fn put32(b: &mut [u8], off: usize, v: u32) { b[off..off + 4].copy_from_slice(&v.to_le_bytes()); }
fn get32(b: &[u8], off: usize) -> u32 { u32::from_le_bytes(b[off..off + 4].try_into().unwrap()) }
fn get16(b: &[u8], off: usize) -> u16 { u16::from_le_bytes(b[off..off + 2].try_into().unwrap()) }

// --- siginfo ------------------------------------------------------------------------------------

#[derive(PartialEq, Eq)]
enum Layout { Kill, Timer, Rt, Chld, Fault, Poll, Sys }

/// Disposition de l'union de `siginfo` (`siginfo_layout` de Linux).
fn layout(sig: i32, code: i32) -> Layout {
    const SI_USER: i32 = 0;
    const SI_KERNEL: i32 = 0x80;
    const SI_TIMER: i32 = -2;
    const SI_SIGIO: i32 = -5;
    if code > SI_USER && code < SI_KERNEL {
        match sig {
            4 | 8 | 11 | 7 | 5 => Layout::Fault, // SIGILL SIGFPE SIGSEGV SIGBUS SIGTRAP
            17 => Layout::Chld,
            29 => Layout::Poll,
            31 => Layout::Sys,
            _ => Layout::Kill,
        }
    } else if code == SI_TIMER {
        Layout::Timer
    } else if code == SI_SIGIO {
        Layout::Poll
    } else if code < 0 {
        Layout::Rt
    } else {
        Layout::Kill
    }
}

/// `siginfo` 64 bits → `compat_siginfo` (128 octets, union à 12 au lieu de 16).
pub fn to_compat(info: &SigInfo) -> [u8; 128] {
    let b = &info.bytes;
    let mut o = [0u8; 128];
    o[0..12].copy_from_slice(&b[0..12]);
    let u32_at = |off: usize| u32::from_le_bytes(b[off..off + 4].try_into().unwrap());
    let u64_at = |off: usize| u64::from_le_bytes(b[off..off + 8].try_into().unwrap());
    match layout(info.signo(), info.get_i32(8)) {
        Layout::Kill => {
            put32(&mut o, 12, u32_at(16));
            put32(&mut o, 16, u32_at(20));
        }
        Layout::Timer | Layout::Rt => {
            put32(&mut o, 12, u32_at(16));
            put32(&mut o, 16, u32_at(20));
            put32(&mut o, 20, u64_at(24) as u32);
        }
        Layout::Chld => {
            put32(&mut o, 12, u32_at(16));
            put32(&mut o, 16, u32_at(20));
            put32(&mut o, 20, u32_at(24));
            put32(&mut o, 24, u64_at(32) as u32);
            put32(&mut o, 28, u64_at(40) as u32);
        }
        Layout::Fault => put32(&mut o, 12, u64_at(16) as u32),
        Layout::Poll => {
            put32(&mut o, 12, u64_at(16) as u32);
            put32(&mut o, 16, u32_at(24));
        }
        Layout::Sys => {
            put32(&mut o, 12, u64_at(16) as u32);
            put32(&mut o, 16, u32_at(24));
            put32(&mut o, 20, u32_at(28));
        }
    }
    o
}

/// `compat_siginfo` → `siginfo` 64 bits (`rt_sigqueueinfo` i386).
pub fn from_compat(c: &[u8; 128]) -> SigInfo {
    let mut info = SigInfo { bytes: [0; 128] };
    info.bytes[0..12].copy_from_slice(&c[0..12]);
    let g = |off: usize| get32(c, off);
    let b = &mut info.bytes;
    let mut p32 = |off: usize, v: u32| b[off..off + 4].copy_from_slice(&v.to_le_bytes());
    let sig = i32::from_le_bytes(c[0..4].try_into().unwrap());
    let code = i32::from_le_bytes(c[8..12].try_into().unwrap());
    match layout(sig, code) {
        Layout::Kill => {
            p32(16, g(12));
            p32(20, g(16));
        }
        Layout::Timer | Layout::Rt => {
            p32(16, g(12));
            p32(20, g(16));
            p32(24, g(20));
        }
        Layout::Chld => {
            p32(16, g(12));
            p32(20, g(16));
            p32(24, g(20));
            p32(32, g(24));
            p32(40, g(28));
        }
        Layout::Fault => p32(16, g(12)),
        Layout::Poll => {
            p32(16, g(12));
            p32(24, g(16));
        }
        Layout::Sys => {
            p32(16, g(12));
            p32(24, g(16));
            p32(28, g(20));
        }
    }
    info
}

// --- État x87 : FXSAVE ↔ ancien format fsave ---------------------------------------------------

/// Mot d'étiquettes complet (2 bits par registre) depuis l'abrégé de FXSAVE.
fn twd_fxsr_to_i387(fx: &[u8]) -> u32 {
    let swd = get16(fx, 2) as u32;
    let tos = (swd >> 11) & 7;
    let mut twd = fx[4] as u32;
    let mut ret = 0xffff_0000u32;
    for i in 0..8u32 {
        let tag = if twd & 1 != 0 {
            let r = 32 + (((i.wrapping_sub(tos)) & 7) as usize) * 16;
            let sig = [get16(fx, r), get16(fx, r + 2), get16(fx, r + 4), get16(fx, r + 6)];
            match get16(fx, r + 8) & 0x7fff {
                0x7fff => 2,
                0 => if sig == [0; 4] { 1 } else { 2 },
                _ => if sig[3] & 0x8000 != 0 { 0 } else { 2 },
            }
        } else {
            3
        };
        ret |= tag << (2 * i);
        twd >>= 1;
    }
    ret
}

fn twd_i387_to_fxsr(twd: u32) -> u8 {
    let mut t = !twd & 0xffff;
    t = (t | (t >> 1)) & 0x5555;
    t = (t | (t >> 1)) & 0x3333;
    t = (t | (t >> 2)) & 0x0f0f;
    t = (t | (t >> 4)) & 0x00ff;
    t as u8
}

/// En-tête `fsave` (112 octets) d'après l'image FXSAVE `fx` (`convert_from_fxsr`).
fn fsave_header(fx: &[u8], cs: u16, ds: u16) -> [u8; FSAVE_HDR] {
    let mut h = [0u8; FSAVE_HDR];
    put32(&mut h, 0, get16(fx, 0) as u32 | 0xffff_0000);
    put32(&mut h, 4, get16(fx, 2) as u32 | 0xffff_0000);
    put32(&mut h, 8, twd_fxsr_to_i387(fx));
    put32(&mut h, 12, get32(fx, 8)); // fip (bas de rip)
    put32(&mut h, 16, cs as u32);
    put32(&mut h, 20, get32(fx, 16)); // foo (bas de rdp)
    put32(&mut h, 24, ds as u32 | 0xffff_0000);
    for i in 0..8 {
        h[28 + i * 10..38 + i * 10].copy_from_slice(&fx[32 + i * 16..42 + i * 16]);
    }
    h[108..110].copy_from_slice(&fx[2..4]); // status = swd
    // magic (110) = 0 : l'image FXSAVE suit.
    h
}

/// Reporte l'environnement x87 de l'en-tête `fsave` dans l'image FXSAVE (`convert_to_fxsr`).
fn apply_fsave_header(h: &[u8], fx: &mut [u8]) {
    fx[0..2].copy_from_slice(&h[0..2]);
    fx[2..4].copy_from_slice(&h[4..6]);
    fx[4] = twd_i387_to_fxsr(get32(h, 8));
    fx[6..8].copy_from_slice(&h[18..20]); // fop = fcs >> 16
    fx[8..16].copy_from_slice(&(get32(h, 12) as u64).to_le_bytes());
    fx[16..24].copy_from_slice(&(get32(h, 20) as u64).to_le_bytes());
    for i in 0..8 {
        fx[32 + i * 16..42 + i * 16].copy_from_slice(&h[28 + i * 10..38 + i * 10]);
    }
}

// --- Cadre ------------------------------------------------------------------------------------

/// `sigcontext_32` des registres `regs`.
fn sigcontext(t: &Thread, regs: &UserRegs, fp: u32, mask: u64) -> [u8; SIGCONTEXT_SIZE] {
    let sel = uentry::current_selectors();
    let mut sc = [0u8; SIGCONTEXT_SIZE];
    put32(&mut sc, 0, sel.gs as u32);
    put32(&mut sc, 4, sel.fs as u32);
    put32(&mut sc, 8, sel.es as u32);
    put32(&mut sc, 12, sel.ds as u32);
    let gpr = [regs.rdi, regs.rsi, regs.rbp, regs.rsp, regs.rbx, regs.rdx, regs.rcx, regs.rax];
    for (i, v) in gpr.iter().enumerate() {
        put32(&mut sc, 16 + i * 4, *v as u32);
    }
    let (trapno, err, cr2) = t.fault_info.lock().take().unwrap_or((0, 0, 0));
    put32(&mut sc, 48, trapno as u32);
    put32(&mut sc, 52, err as u32);
    put32(&mut sc, 56, regs.rip as u32);
    put32(&mut sc, 60, regs.cs as u32);
    put32(&mut sc, 64, regs.rflags as u32);
    put32(&mut sc, 68, regs.rsp as u32);
    put32(&mut sc, 72, regs.ss as u32);
    put32(&mut sc, 76, fp);
    put32(&mut sc, 80, mask as u32);
    put32(&mut sc, 84, cr2 as u32);
    sc
}

/// Livre `info` à un gestionnaire i386 (`ia32_setup_frame` / `ia32_setup_rt_frame`).
pub(crate) fn setup_frame(t: &Thread, regs: &mut UserRegs, info: &SigInfo, act: &SigAction) -> Result<(), ()> {
    let mm = t.mm();
    let rt = act.flags & signal::SA_SIGINFO != 0;
    let alt = *t.altstack.lock();
    let mut sp = regs.rsp & 0xFFFF_FFFF;
    if act.flags & signal::SA_ONSTACK != 0 && alt.enabled() && !alt.contains(sp) {
        sp = alt.sp + alt.size;
    }
    // État vectoriel : image XSAVE alignée sur 64, en-tête fsave de 112 octets juste avant.
    let xsize = signal::fpstate_size() as u64;
    let buf_fx = (sp.wrapping_sub(xsize)) & !63;
    let fp_addr = buf_fx - FSAVE_HDR as u64;
    let size = if rt { RT_SIGFRAME_SIZE } else { SIGFRAME_SIZE } as u64;
    let frame = ((fp_addr.wrapping_sub(size) + 4) & !15).wrapping_sub(4);
    if frame >= 1 << 32 || frame < 4096 {
        return Err(());
    }
    if alt.contains(regs.rsp) && !alt.contains(frame + 1) && act.flags & signal::SA_ONSTACK != 0 {
        return Err(()); // débordement de la pile alternative
    }
    let fx = signal::save_fpu_frame();
    let sel = uentry::current_selectors();
    let mut fpbuf = vec![0u8; FSAVE_HDR + fx.len()];
    fpbuf[..FSAVE_HDR].copy_from_slice(&fsave_header(&fx, regs.cs as u16, sel.ds));
    fpbuf[FSAVE_HDR..].copy_from_slice(&fx);
    mm.copy_to_user(fp_addr as usize, &fpbuf).map_err(|_| ())?;

    let old_mask = t.saved_sigmask.lock().take().unwrap_or_else(|| t.sigmask.load(Ordering::Acquire));
    let sig = info.signo();
    // Retour : restaurateur fourni (glibc, musl), sinon le code de `retcode` dans le cadre.
    let restorer = if act.flags & signal::SA_RESTORER != 0 && act.restorer != 0 {
        act.restorer as u32
    } else if rt {
        (frame + RT_SIGFRAME_SIZE as u64 - 8) as u32
    } else {
        (frame + SIGFRAME_SIZE as u64 - 8) as u32
    };
    let sc = sigcontext(t, regs, fp_addr as u32, old_mask);
    if rt {
        let mut f = [0u8; RT_SIGFRAME_SIZE];
        put32(&mut f, 0, restorer);
        put32(&mut f, 4, sig as u32);
        put32(&mut f, 8, (frame + 16) as u32);
        put32(&mut f, 12, (frame + 144) as u32);
        f[16..144].copy_from_slice(&to_compat(info));
        let uc = 144;
        put32(&mut f, uc, UC_FP_XSTATE * uentry::uses_xsave() as u32);
        put32(&mut f, uc + 4, 0);
        put32(&mut f, uc + 8, alt.sp as u32);
        put32(&mut f, uc + 12, alt.user_flags(regs.rsp) as u32);
        put32(&mut f, uc + 16, alt.size as u32);
        f[uc + 20..uc + 20 + SIGCONTEXT_SIZE].copy_from_slice(&sc);
        f[uc + 108..uc + 116].copy_from_slice(&old_mask.to_le_bytes());
        // retcode : movl $__NR_rt_sigreturn, %eax ; int $0x80
        let rc = RT_SIGFRAME_SIZE - 8;
        f[rc] = 0xb8;
        put32(&mut f, rc + 1, NR_RT_SIGRETURN);
        f[rc + 5] = 0xcd;
        f[rc + 6] = 0x80;
        mm.copy_to_user(frame as usize, &f).map_err(|_| ())?;
        regs.rdx = frame + 16;
        regs.rcx = frame + 144;
    } else {
        let mut f = [0u8; SIGFRAME_SIZE];
        put32(&mut f, 0, restorer);
        put32(&mut f, 4, sig as u32);
        f[8..8 + SIGCONTEXT_SIZE].copy_from_slice(&sc);
        put32(&mut f, 8 + SIGCONTEXT_SIZE + FPSTATE32_SIZE, (old_mask >> 32) as u32);
        // retcode : popl %eax ; movl $__NR_sigreturn, %eax ; int $0x80
        let rc = SIGFRAME_SIZE - 8;
        f[rc] = 0x58;
        f[rc + 1] = 0xb8;
        put32(&mut f, rc + 2, NR_SIGRETURN);
        f[rc + 6] = 0xcd;
        f[rc + 7] = 0x80;
        mm.copy_to_user(frame as usize, &f).map_err(|_| ())?;
        regs.rdx = 0;
        regs.rcx = 0;
    }

    let mut mask = old_mask | act.mask;
    if act.flags & signal::SA_NODEFER == 0 {
        mask |= sigbit(sig);
    }
    t.sigmask.store(sanitize_mask(mask), Ordering::Release);

    // Gestionnaire : arguments sur la pile (cdecl) et aussi dans eax/edx/ecx (regparm(3)).
    regs.rax = sig as u64;
    regs.rsp = frame;
    regs.rip = act.handler & 0xFFFF_FFFF;
    regs.cs = uentry::USER32_CS;
    regs.rflags &= !(RFLAGS_TF | RFLAGS_DF | RFLAGS_RF);
    regs.vector = FORCE_IRET_VECTOR;
    // `ds`/`es` = données utilisateur pour le gestionnaire ; `fs`/`gs` gardés.
    if let Some(ctx) = crate::user_context() {
        let ds = uentry::USER_SS as u16;
        uentry::load_segments(&ctx.seg, uentry::Selectors { ds, es: ds, ..sel });
    }
    signal::reset_fpu();
    Ok(())
}

/// Rétablit les registres d'un `sigcontext_32` (`ia32_restore_sigcontext`).
fn restore_sigcontext(t: &Thread, regs: &mut UserRegs, sc: &[u8]) -> Result<(), BadFrame> {
    let mm = t.mm();
    let fp_addr = get32(sc, 76) as usize;
    let fp = if fp_addr != 0 {
        let mut b = vec![0u8; FSAVE_HDR + signal::fpstate_size()];
        mm.copy_from_user(&mut b, fp_addr).map_err(|_| BadFrame)?;
        let (hdr, fx) = b.split_at_mut(FSAVE_HDR);
        apply_fsave_header(hdr, fx);
        Some(fx.to_vec())
    } else {
        None
    };
    regs.rdi = get32(sc, 16) as u64;
    regs.rsi = get32(sc, 20) as u64;
    regs.rbp = get32(sc, 24) as u64;
    regs.rsp = get32(sc, 28) as u64;
    regs.rbx = get32(sc, 32) as u64;
    regs.rdx = get32(sc, 36) as u64;
    regs.rcx = get32(sc, 40) as u64;
    regs.rax = get32(sc, 44) as u64;
    regs.rip = get32(sc, 56) as u64;
    // CS : code 32 ou 64 bits utilisateur seulement (assaini par `uentry` au retour).
    let cs = get16(sc, 60) as u64;
    regs.cs = if cs == uentry::USER_CS { uentry::USER_CS } else { uentry::USER32_CS };
    regs.rflags = get32(sc, 64) as u64;
    regs.vector = FORCE_IRET_VECTOR;
    if let Some(ctx) = crate::user_context() {
        let want = uentry::Selectors { gs: get16(sc, 0), fs: get16(sc, 4), es: get16(sc, 8), ds: get16(sc, 12) };
        let (loaded, fs_base, gs_base) = uentry::load_segments(&ctx.seg, want);
        if loaded.fs & !3 != 0 {
            regs.fs_base = fs_base;
        }
        if loaded.gs & !3 != 0 {
            regs.gs_base = gs_base;
        }
    }
    signal::restore_fpu_frame(fp.as_deref());
    Ok(())
}

/// `sigreturn` (cadre `sigframe_ia32`, `esp` = cadre + 8 après le `popl %eax` du restaurateur).
pub(crate) fn sigreturn(t: &Thread, regs: &mut UserRegs) -> Result<(), BadFrame> {
    let frame = (regs.rsp & 0xFFFF_FFFF).wrapping_sub(8);
    let mut f = [0u8; SIGFRAME_SIZE];
    t.mm().copy_from_user(&mut f, frame as usize).map_err(|_| BadFrame)?;
    let sc = &f[8..8 + SIGCONTEXT_SIZE];
    let mask = get32(sc, 80) as u64 | (get32(&f, 8 + SIGCONTEXT_SIZE + FPSTATE32_SIZE) as u64) << 32;
    restore_sigcontext(t, regs, sc)?;
    t.sigmask.store(sanitize_mask(mask), Ordering::Release);
    signal::recalc(t);
    Ok(())
}

/// `rt_sigreturn` (cadre `rt_sigframe_ia32`, `esp` = cadre + 4 après le `ret` du gestionnaire).
pub(crate) fn rt_sigreturn(t: &Thread, regs: &mut UserRegs) -> Result<(), BadFrame> {
    let frame = (regs.rsp & 0xFFFF_FFFF).wrapping_sub(4);
    let mut f = [0u8; RT_SIGFRAME_SIZE];
    t.mm().copy_from_user(&mut f, frame as usize).map_err(|_| BadFrame)?;
    let uc = 144;
    let mask = u64::from_le_bytes(f[uc + 108..uc + 116].try_into().unwrap());
    let rsp_before = regs.rsp;
    restore_sigcontext(t, regs, &f[uc + 20..uc + 20 + SIGCONTEXT_SIZE])?;
    t.sigmask.store(sanitize_mask(mask), Ordering::Release);
    // Pile alternative enregistrée (erreurs ignorées, comme `compat_restore_altstack`).
    let ss_sp = get32(&f, uc + 8) as u64;
    let ss_flags = get32(&f, uc + 12) as i32;
    let ss_size = get32(&f, uc + 16) as u64;
    {
        let mut alt = t.altstack.lock();
        if !alt.contains(rsp_before) {
            *alt = if ss_flags & 2 != 0 { AltStack::disabled() } else { AltStack { sp: ss_sp, size: ss_size, flags: 0 } };
        }
    }
    signal::recalc(t);
    Ok(())
}
