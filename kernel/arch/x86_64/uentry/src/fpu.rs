//! État vectoriel (x87/SSE/AVX/AVX-512) des fils utilisateur.
//!
//! Le noyau de MaiOS est compilé sans SSE (cible « soft-float ») : les registres vectoriels
//! n'appartiennent qu'aux fils utilisateur. Ils sont sauvés quand un fil utilisateur quitte le
//! cœur et restaurés quand il le reprend (XSAVEOPT, qui n'écrit que les composants modifiés,
//! sinon XSAVE ; FXSAVE sur les processeurs sans XSAVE).
//!
//! XCR0 active tout ce que le processeur sait sauver parmi x87, SSE, AVX et AVX-512 : les
//! applications peuvent utiliser AVX-512 si le cœur le permet (glibc et musl le détectent par
//! `xgetbv`). AMX et MPX restent désactivés.

use alloc::alloc::{alloc_zeroed, dealloc, Layout};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};

/// Composants activés : x87 (0), SSE (1), AVX (2), opmask (5), ZMM_Hi256 (6), Hi16_ZMM (7).
const WANTED_XCR0: u64 = 0b1110_0111;

static XCR0: AtomicU64 = AtomicU64::new(0);
static AREA_SIZE: AtomicUsize = AtomicUsize::new(512);
/// 0 : FXSAVE, 1 : XSAVE, 2 : XSAVEOPT.
static MODE: AtomicU8 = AtomicU8::new(0);

/// Active XSAVE sur ce cœur et renvoie XCR0.
pub(crate) unsafe fn enable_xsave() -> u64 {
    let mut cr4: u64;
    core::arch::asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
    cr4 |= 1 << 18; // OSXSAVE
    core::arch::asm!("mov cr4, {}", in(reg) cr4, options(nostack, preserves_flags));

    let leaf_d = core::arch::x86_64::__cpuid_count(0xD, 0);
    let supported = ((leaf_d.edx as u64) << 32) | leaf_d.eax as u64;
    let mut xcr0 = supported & WANTED_XCR0;
    // AVX-512 n'est valable que complet ; AVX exige SSE.
    if xcr0 & 0b1110_0000 != 0b1110_0000 { xcr0 &= !0b1110_0000; }
    if xcr0 & 0b100 != 0 && xcr0 & 0b10 == 0 { xcr0 &= !0b100; }
    xcr0 |= 0b11;
    core::arch::asm!("xsetbv", in("ecx") 0u32, in("eax") xcr0 as u32, in("edx") (xcr0 >> 32) as u32, options(nomem, nostack));

    // Taille de la zone pour XCR0 courant (EBX après xsetbv).
    let size = core::arch::x86_64::__cpuid_count(0xD, 0).ebx as usize;
    let leaf_d1 = core::arch::x86_64::__cpuid_count(0xD, 1);
    let opt = leaf_d1.eax & 1 != 0;
    XCR0.store(xcr0, Ordering::Relaxed);
    AREA_SIZE.store(size.max(576), Ordering::Relaxed);
    MODE.store(if opt { 2 } else { 1 }, Ordering::Relaxed);
    xcr0
}

/// XCR0 actif (0 si XSAVE est absent).
pub fn xcr0() -> u64 {
    XCR0.load(Ordering::Relaxed)
}

/// Taille d'une zone de sauvegarde.
pub fn xsave_area_size() -> usize {
    AREA_SIZE.load(Ordering::Relaxed)
}

/// Zone de sauvegarde de l'état vectoriel d'un fil (alignée sur 64 octets).
pub struct FpuState {
    area: UnsafeCell<*mut u8>,
    layout: Layout,
}

// SAFETY : la zone n'est touchée que par le cœur qui exécute le fil, interruptions coupées.
unsafe impl Send for FpuState {}
unsafe impl Sync for FpuState {}

impl FpuState {
    /// État initial : x87 FCW = 0x37F, MXCSR = 0x1F80, tous les composants à leur valeur d'origine.
    pub fn new() -> Option<FpuState> {
        let layout = Layout::from_size_align(xsave_area_size(), 64).ok()?;
        // SAFETY : taille non nulle.
        let area = unsafe { alloc_zeroed(layout) };
        if area.is_null() {
            return None;
        }
        // SAFETY : la zone fait au moins 512 octets.
        unsafe {
            (area as *mut u16).write(0x037F);
            (area.add(24) as *mut u32).write(0x1F80);
            (area.add(28) as *mut u32).write(0xFFFF);
        }
        Some(FpuState { area: UnsafeCell::new(area), layout })
    }

    /// Copie de l'état d'un autre fil (`fork`, `clone`).
    pub fn clone_from_state(other: &FpuState) -> Option<FpuState> {
        let new = FpuState::new()?;
        // SAFETY : même taille, zones distinctes.
        unsafe { core::ptr::copy_nonoverlapping(*other.area.get(), *new.area.get(), new.layout.size()) };
        Some(new)
    }

    /// Sauve les registres vectoriels du cœur dans cette zone.
    ///
    /// # Safety
    /// À appeler sur le cœur qui exécutait ce fil, interruptions coupées.
    pub unsafe fn save(&self) {
        let area = *self.area.get();
        match MODE.load(Ordering::Relaxed) {
            2 => core::arch::asm!("xsaveopt64 [{}]", in(reg) area, in("eax") u32::MAX, in("edx") u32::MAX, options(nostack, preserves_flags)),
            1 => core::arch::asm!("xsave64 [{}]", in(reg) area, in("eax") u32::MAX, in("edx") u32::MAX, options(nostack, preserves_flags)),
            _ => core::arch::asm!("fxsave64 [{}]", in(reg) area, options(nostack, preserves_flags)),
        }
    }

    /// Charge cette zone dans les registres vectoriels du cœur.
    ///
    /// # Safety
    /// À appeler sur le cœur qui va exécuter ce fil, interruptions coupées.
    pub unsafe fn restore(&self) {
        let area = *self.area.get();
        match MODE.load(Ordering::Relaxed) {
            1 | 2 => core::arch::asm!("xrstor64 [{}]", in(reg) area, in("eax") u32::MAX, in("edx") u32::MAX, options(nostack, preserves_flags)),
            _ => core::arch::asm!("fxrstor64 [{}]", in(reg) area, options(nostack, preserves_flags)),
        }
    }
}

impl Drop for FpuState {
    fn drop(&mut self) {
        // SAFETY : allouée par `new` avec ce `layout`.
        unsafe { dealloc(*self.area.get(), self.layout) };
    }
}

// ---------------------------------------------------------------------------------------------
// État vectoriel partagé des tâches noyau
//
// Les tâches noyau en C (Doom…) utilisent SSE sans que le changement de contexte du noyau ne le
// sauve : elles partagent, sur chaque cœur, un même jeu de registres. Pour qu'un fil utilisateur
// ne l'écrase pas, ce jeu est sauvé quand un fil utilisateur prend le cœur et restauré quand le
// cœur revient à une tâche noyau. Entre deux tâches noyau, rien n'est fait (comme avant).
// ---------------------------------------------------------------------------------------------

const MAX_CPUS: usize = 512;
const NO_STATE: Option<FpuState> = None;
static mut KERNEL_FPU: [Option<FpuState>; MAX_CPUS] = [NO_STATE; MAX_CPUS];

#[inline(always)]
fn cpu_index() -> usize {
    let mut aux = 0u32;
    // SAFETY : lecture de TSC_AUX.
    unsafe { core::arch::x86_64::__rdtscp(&mut aux) };
    aux as usize % MAX_CPUS
}

/// Prépare la zone noyau du cœur `cpu` (après l'activation de XSAVE).
pub(crate) fn init_kernel_area(cpu: usize) {
    // SAFETY : appelé une fois par cœur, à son initialisation.
    unsafe {
        let slot = &mut KERNEL_FPU[cpu % MAX_CPUS];
        if slot.is_none() {
            *slot = FpuState::new();
        }
    }
}

fn kernel_area() -> Option<&'static FpuState> {
    // SAFETY : zone propre à ce cœur, posée à son initialisation.
    unsafe { KERNEL_FPU[cpu_index()].as_ref() }
}

/// Changement de contexte de `prev` vers `next` (`None` : tâche noyau), interruptions coupées.
///
/// # Safety
/// `prev` est l'état du fil qui vient de s'exécuter sur ce cœur, `next` celui qui le prend.
pub unsafe fn switch_fpu(prev: Option<&FpuState>, next: Option<&FpuState>) {
    match (prev, next) {
        (None, None) => {}
        (Some(p), Some(n)) => {
            p.save();
            n.restore();
        }
        (None, Some(n)) => {
            if let Some(k) = kernel_area() { k.save(); }
            n.restore();
        }
        (Some(p), None) => {
            p.save();
            if let Some(k) = kernel_area() { k.restore(); }
        }
    }
}

/// Premier passage d'une tâche noyau en mode utilisateur : les registres du cœur appartiennent
/// encore aux tâches noyau ; ils sont mis de côté, puis l'état du fil est chargé.
///
/// # Safety
/// Interruptions coupées, sur le cœur qui exécute le fil.
pub unsafe fn begin_user_fpu(state: &FpuState) {
    switch_fpu(None, Some(state));
}
