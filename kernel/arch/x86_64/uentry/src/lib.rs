//! MaiProc — entrées et sorties du noyau depuis le mode utilisateur (x86_64).
//!
//! ## Invariants
//! * `IA32_KERNEL_GS_BASE` contient **en permanence** l'adresse du [`CpuEntryBlock`] du cœur.
//!   Seule l'entrée `syscall` exécute `swapgs`, toujours par paire, pour trouver la pile noyau ;
//!   le chemin commun relit le bloc de la même manière.
//! * Dans le noyau, `GS_BASE` = base CLS du cœur et `FS_BASE` = TLS du noyau. Les valeurs
//!   utilisateur sont sauvées dans le cadre ([`UserRegs`]) par `rdgsbase`/`rdfsbase` à l'entrée et
//!   rétablies par `wrgsbase`/`wrfsbase` à la sortie : aucune hypothèse sur l'état de `swapgs`,
//!   ce qui rend les changements de contexte en plein appel système triviaux.
//! * NMI et #MC passent par une entrée « paranoïaque » (pile IST, cœur retrouvé par `rdtscp`) :
//!   elles peuvent tomber n'importe où, y compris entre les deux `swapgs` de l'entrée `syscall`.
//!
//! ## IDT
//! 256 talons de 32 octets. Depuis le noyau, un talon coûte deux instructions (test du CPL, saut
//! indirect vers le gestionnaire historique `x86-interrupt` enregistré dans [`HANDLERS`]).
//! Depuis l'utilisateur, il construit le cadre complet puis appelle [`uentry_trap`] : exceptions
//! vers le crochet de fautes (mémoire virtuelle des processus, signaux), IRQ vers le gestionnaire
//! historique, appelé avec un faux cadre d'interruption de niveau 0.
//!
//! La conception reste compatible avec FRED (Intel, Linux 6.9+), qui fait matériellement ce que
//! ce crate fait à la main (sauvegarde de GS, pile d'entrée, cadre unique).

#![no_std]
#![feature(naked_functions)]
#![feature(asm_const)]

extern crate alloc;

mod entry;
mod fpu;
mod seg;

pub use fpu::{FpuState, xsave_area_size, xcr0, uses_xsave, switch_fpu, begin_user_fpu};
pub use seg::{
    SegState, Selectors, TLS_ENTRY_MIN, TLS_ENTRIES, switch_segments, load_segments, set_tls,
    descriptor_base, current_selectors,
};

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Sélecteurs (ordre fixé par `gdt::create_gdt`).
pub const KERNEL_CS: u64 = 0x08;
pub const KERNEL_SS: u64 = 0x10;
/// Code 64 bits utilisateur (index 5, RPL 3).
pub const USER_CS: u64 = 0x2b;
/// Code 32 bits utilisateur (index 3, RPL 3) : mode compatible (binaires i386). C'est le CS que
/// charge `sysretl` (STAR[63:48]).
pub const USER32_CS: u64 = 0x1b;
/// Données utilisateur (index 4, RPL 3) : c'est le segment que charge `sysret` (STAR + 8).
pub const USER_SS: u64 = 0x23;

/// Valeur de `UserRegs::vector` pour une entrée par `syscall` (retour rapide par `sysret`).
pub const SYSCALL_VECTOR: u64 = 0x100;
/// À placer dans `UserRegs::vector` pour forcer un retour par `iretq` (registres arbitraires,
/// après `execve` ou la livraison d'un signal).
pub const FORCE_IRET_VECTOR: u64 = 0x101;
/// Appel système i386 par `int 0x80` (porte DPL 3) : table 32 bits, quel que soit le mode du
/// code appelant (comme Linux). Retour par `iretq`.
pub const INT80_VECTOR: u64 = 0x80;
/// Appel système i386 par `syscall` en mode compatible (IA32_CSTAR, AMD) : retour rapide par
/// `sysretl`. Convention du vDSO de Linux : 2e argument dans `ebp`, 6e sur la pile.
pub const SYSCALL32_VECTOR: u64 = 0x102;

/// Vrai si le cadre est celui d'un appel système (toutes entrées confondues).
#[inline]
pub fn is_syscall_vector(v: u64) -> bool {
    v == SYSCALL_VECTOR || v == INT80_VECTOR || v == SYSCALL32_VECTOR
}

/// Vrai si le cadre est celui d'un appel système de la table i386.
#[inline]
pub fn is_syscall32_vector(v: u64) -> bool {
    v == INT80_VECTOR || v == SYSCALL32_VECTOR
}

/// Plus haute adresse de retour acceptée par le chemin `sysret` (dernière page exclue).
const SYSRET_RIP_LIMIT: u64 = 0x0000_7FFF_FFFF_F000;

/// Registres d'un fil utilisateur, tels qu'empilés à l'entrée dans le noyau.
#[repr(C)]
#[derive(Clone, Default, Debug)]
pub struct UserRegs {
    pub fs_base: u64,
    pub gs_base: u64,
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    /// Vecteur d'entrée (0..256), [`SYSCALL_VECTOR`] ou [`FORCE_IRET_VECTOR`].
    pub vector: u64,
    pub error_code: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

impl UserRegs {
    /// Registres initiaux d'un fil : `rip`, pile `rsp`, interruptions actives.
    pub fn new_user(rip: u64, rsp: u64) -> UserRegs {
        UserRegs {
            vector: FORCE_IRET_VECTOR,
            rip,
            cs: USER_CS,
            rflags: RFLAGS_IF | RFLAGS_RESERVED,
            rsp,
            ss: USER_SS,
            ..Default::default()
        }
    }

    /// Registres initiaux d'un fil 32 bits (mode compatible).
    pub fn new_user32(eip: u32, esp: u32) -> UserRegs {
        UserRegs { cs: USER32_CS, ..UserRegs::new_user(eip as u64, esp as u64) }
    }

    /// Vrai si le cadre vient du mode utilisateur.
    pub fn from_user(&self) -> bool {
        self.cs & 3 == 3
    }

    /// Vrai si le fil exécute du code 32 bits (CS du mode compatible).
    pub fn is_compat(&self) -> bool {
        self.cs == USER32_CS
    }
}

const RFLAGS_RESERVED: u64 = 1 << 1;
const RFLAGS_IF: u64 = 1 << 9;
const RFLAGS_TF: u64 = 1 << 8;
/// Bits que l'utilisateur peut garder : CF PF AF ZF SF TF DF OF AC ID (pas d'IOPL, NT, RF, VM, VIF/VIP).
const RFLAGS_USER_MASK: u64 = 0x0000_0000_0024_0DD5;

/// Bloc d'entrée par cœur (pointé par `IA32_KERNEL_GS_BASE`). Disposition figée : l'assembleur
/// y accède par déplacement fixe.
#[repr(C, align(64))]
struct CpuEntryBlock {
    /// 0 : pile utilisateur, le temps de passer sur la pile noyau (`syscall`).
    scratch_rsp: u64,
    /// 8 : sommet de la pile noyau de la tâche utilisateur courante.
    kstack_top: u64,
    /// 16 : base CLS du cœur (`GS_BASE` du noyau).
    cls_base: u64,
    /// 24 : TLS du noyau (`FS_BASE` du noyau).
    kernel_fs: u64,
    /// 32 : pointeur sur le champ RSP0 de la TSS du cœur.
    tss_rsp0: u64,
    /// 40 : identifiant du cœur (+1 ; 0 = bloc non initialisé).
    cpu_plus_one: u64,
    /// 48 : base de la GDT du cœur (entrées TLS des fils 32 bits).
    gdt_base: u64,
    /// 56 : entrées TLS de la GDT non nulles (à effacer pour un fil sans TLS).
    tls_loaded: u64,
}

const MAX_CPUS: usize = 512;
const BLOCK_SHIFT: u32 = 6;
const EMPTY_BLOCK: CpuEntryBlock = CpuEntryBlock {
    scratch_rsp: 0, kstack_top: 0, cls_base: 0, kernel_fs: 0, tss_rsp0: 0, cpu_plus_one: 0, gdt_base: 0, tls_loaded: 0,
};
static mut BLOCKS: [CpuEntryBlock; MAX_CPUS] = [EMPTY_BLOCK; MAX_CPUS];

/// Gestionnaires historiques (`x86-interrupt`) appelés par les talons, par vecteur.
static mut HANDLERS: [u64; 256] = [0; 256];
static INSTALLED: AtomicBool = AtomicBool::new(false);
/// 1 si SMAP est actif (les talons exécutent alors `clac`).
static mut SMAP_ON: u8 = 0;
static SMEP_ON: AtomicBool = AtomicBool::new(false);
static USER_MODE_OK: AtomicBool = AtomicBool::new(true);

/// Table chargée par `lidt` (256 portes de 16 octets).
#[repr(C, align(16))]
struct RawIdt([u64; 512]);
static mut ENTRY_IDT: RawIdt = RawIdt([0; 512]);

// --- Crochets fournis par les couches supérieures (processus, mémoire, signaux) -----------------

/// Appel système d'un fil utilisateur (interruptions actives).
pub type SyscallHook = fn(&mut UserRegs);
/// Exception levée en mode utilisateur : `(registres, vecteur, CR2)`.
pub type FaultHook = fn(&mut UserRegs, u8, u64);
/// Juste avant le retour en mode utilisateur (interruptions coupées) : signaux, fin de fil.
pub type ReturnHook = fn(&mut UserRegs);
/// NMI : renvoie `true` si elle était attendue.
pub type NmiHook = fn(&UserRegs) -> bool;

static SYSCALL_HOOK: AtomicUsize = AtomicUsize::new(0);
static FAULT_HOOK: AtomicUsize = AtomicUsize::new(0);
static RETURN_HOOK: AtomicUsize = AtomicUsize::new(0);
static NMI_HOOK: AtomicUsize = AtomicUsize::new(0);

pub fn set_syscall_hook(h: SyscallHook) { SYSCALL_HOOK.store(h as usize, Ordering::Release); }
pub fn set_fault_hook(h: FaultHook) { FAULT_HOOK.store(h as usize, Ordering::Release); }
pub fn set_return_hook(h: ReturnHook) { RETURN_HOOK.store(h as usize, Ordering::Release); }
pub fn set_nmi_hook(h: NmiHook) { NMI_HOOK.store(h as usize, Ordering::Release); }

// --- Fonctionnalités du processeur ---------------------------------------------------------------

/// Fonctionnalités activées sur ce cœur.
#[derive(Debug, Clone, Copy)]
pub struct CpuFeatures {
    pub fsgsbase: bool,
    pub smep: bool,
    pub smap: bool,
    pub xsave: bool,
    pub xcr0: u64,
}

const MSR_FS_BASE: u32 = 0xC000_0100;
const MSR_GS_BASE: u32 = 0xC000_0101;
const MSR_KERNEL_GS_BASE: u32 = 0xC000_0102;
const MSR_EFER: u32 = 0xC000_0080;
const MSR_STAR: u32 = 0xC000_0081;
const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_CSTAR: u32 = 0xC000_0083;
const MSR_SYSENTER_CS: u32 = 0x174;
const MSR_FMASK: u32 = 0xC000_0084;

#[inline(always)]
unsafe fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    core::arch::asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags));
    ((hi as u64) << 32) | lo as u64
}

#[inline(always)]
unsafe fn wrmsr(msr: u32, value: u64) {
    core::arch::asm!("wrmsr", in("ecx") msr, in("eax") value as u32, in("edx") (value >> 32) as u32, options(nostack, preserves_flags));
}

#[inline(always)]
unsafe fn read_cr4() -> u64 {
    let v: u64;
    core::arch::asm!("mov {}, cr4", out(reg) v, options(nomem, nostack, preserves_flags));
    v
}

#[inline(always)]
unsafe fn write_cr4(v: u64) {
    core::arch::asm!("mov cr4, {}", in(reg) v, options(nostack, preserves_flags));
}

pub(crate) fn block_mut(cpu: usize) -> &'static mut CpuEntryBlock {
    // SAFETY : chaque cœur ne touche qu'à son bloc (ou un initialiseur unique avant son démarrage).
    unsafe { &mut BLOCKS[cpu % MAX_CPUS] }
}

/// Bloc du cœur courant, d'après `IA32_KERNEL_GS_BASE` (None si pas encore initialisé).
pub(crate) fn current_block() -> Option<&'static mut CpuEntryBlock> {
    let ptr = unsafe { rdmsr(MSR_KERNEL_GS_BASE) } as usize;
    let base = unsafe { core::ptr::addr_of!(BLOCKS) } as usize;
    let end = base + core::mem::size_of::<[CpuEntryBlock; MAX_CPUS]>();
    if ptr >= base && ptr < end {
        Some(unsafe { &mut *(ptr as *mut CpuEntryBlock) })
    } else {
        None
    }
}

/// Initialise les entrées utilisateur du cœur `cpu` (identifiant APIC) : bloc d'entrée, MSR de
/// `syscall`, SMEP/SMAP/FSGSBASE, XSAVE. `tss_rsp0` pointe sur le champ RSP0 de sa TSS.
pub fn init_this_cpu(cpu: usize, tss_rsp0: *mut u64) -> CpuFeatures {
    let b = block_mut(cpu);
    b.tss_rsp0 = tss_rsp0 as u64;
    b.cpu_plus_one = cpu as u64 + 1;

    // SAFETY : CPUID est toujours disponible ; les MSR et CR4 sont ceux de ce cœur.
    unsafe {
        let leaf7 = core::arch::x86_64::__cpuid_count(7, 0);
        let leaf1 = core::arch::x86_64::__cpuid(1);
        let fsgsbase = leaf7.ebx & (1 << 0) != 0;
        let smep = leaf7.ebx & (1 << 7) != 0;
        let smap = leaf7.ebx & (1 << 20) != 0;
        let xsave = leaf1.ecx & (1 << 26) != 0;

        b.cls_base = rdmsr(MSR_GS_BASE);
        b.kernel_fs = rdmsr(MSR_FS_BASE);
        wrmsr(MSR_KERNEL_GS_BASE, b as *mut CpuEntryBlock as u64);

        // syscall/sysret : CS noyau 0x08 (SS 0x10), base utilisateur 0x18 (SS 0x23, CS 0x2b).
        wrmsr(MSR_STAR, (0x18u64 << 48) | (KERNEL_CS << 32));
        wrmsr(MSR_LSTAR, entry::syscall_entry_address() as u64);
        // `syscall` en mode compatible (AMD ; Intel lève #UD) : table i386.
        wrmsr(MSR_CSTAR, entry::cstar_entry_address() as u64);
        // `sysenter` pas encore pris en charge : CS nul → #GP → SIGSEGV dans le fil (le vDSO
        // 32 bits passe par `int 0x80` ou `syscall`).
        if leaf1.edx & (1 << 11) != 0 {
            wrmsr(MSR_SYSENTER_CS, 0);
        }
        b.gdt_base = seg::current_gdt_base();
        b.tls_loaded = 1; // inconnu : la première commutation efface les entrées TLS
        // Comme Linux : CF PF AF ZF SF TF IF DF OF IOPL NT RF AC ID masqués à l'entrée.
        wrmsr(MSR_FMASK, 0x0025_7FD5);
        wrmsr(MSR_EFER, rdmsr(MSR_EFER) | 1);

        let mut cr4 = read_cr4();
        if fsgsbase { cr4 |= 1 << 16; }
        if smep { cr4 |= 1 << 20; }
        if smap { cr4 |= 1 << 21; }
        write_cr4(cr4);
        if !fsgsbase {
            USER_MODE_OK.store(false, Ordering::Relaxed);
            log::error!("MaiProc : processeur sans FSGSBASE, mode utilisateur indisponible");
        }
        if smap { SMAP_ON = 1; }
        if smep { SMEP_ON.store(true, Ordering::Relaxed); }

        let xcr0 = if xsave { fpu::enable_xsave() } else { 0 };
        fpu::init_kernel_area(cpu);
        CpuFeatures { fsgsbase, smep, smap, xsave, xcr0 }
    }
}

/// Note un changement de base CLS (`GS_BASE` du noyau) sur le cœur courant.
pub fn note_cls_base(base: u64) {
    if let Some(b) = current_block() {
        b.cls_base = base;
    }
}

/// Note un changement de TLS du noyau (`FS_BASE`) sur le cœur courant.
pub fn note_kernel_fs(base: u64) {
    if let Some(b) = current_block() {
        b.kernel_fs = base;
    }
}

/// Pile noyau de la tâche utilisateur qui prend le cœur (appelé au changement de contexte,
/// interruptions coupées) : bloc d'entrée (`syscall`) et RSP0 de la TSS (interruptions).
#[inline]
pub fn set_kernel_stack(top: usize) {
    if let Some(b) = current_block() {
        b.kstack_top = top as u64;
        if b.tss_rsp0 != 0 {
            // SAFETY : champ RSP0 (non aligné : TSS « packed ») de la TSS de ce cœur, lu par le
            // processeur à chaque entrée depuis l'anneau 3.
            unsafe { (b.tss_rsp0 as *mut u64).write_unaligned(top as u64) };
        }
    }
}

/// Vrai si le mode utilisateur est utilisable (FSGSBASE présent, talons installés).
pub fn user_mode_available() -> bool {
    USER_MODE_OK.load(Ordering::Relaxed) && INSTALLED.load(Ordering::Acquire)
}

/// Vrai si SMAP est actif.
pub fn smap_enabled() -> bool {
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!(SMAP_ON)) != 0 }
}

// --- IDT -------------------------------------------------------------------------------------------

/// Enregistre le gestionnaire historique du vecteur `v` (appelé par `interrupts` à chaque
/// modification de son IDT logique).
pub fn sync_vector(v: u8, handler: u64) {
    // SAFETY : écriture atomique d'un u64 aligné, lue par les talons.
    unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(HANDLERS[v as usize]), handler) };
}

/// Pile IST (1 à 7) de chaque vecteur particulier : double faute (1), NMI (2), #MC (3).
fn ist_of(v: usize) -> u64 {
    match v {
        8 => 1,
        2 => 2,
        18 => 3,
        _ => 0,
    }
}

/// Construit l'IDT d'entrée à partir des gestionnaires `handlers` et la charge sur ce cœur.
pub fn install(handlers: &[u64; 256]) {
    unsafe {
        for (v, &h) in handlers.iter().enumerate() {
            HANDLERS[v] = h;
            let stub = entry::stub_address(v) as u64;
            // `int3`, `into` et `int 0x80` (appels système i386) sont permis à l'anneau 3.
            let dpl: u64 = if v == 3 || v == 4 || v == INT80_VECTOR as usize { 3 } else { 0 };
            let attr: u64 = 0x8E | (dpl << 5);
            let low = (stub & 0xFFFF)
                | (KERNEL_CS << 16)
                | (ist_of(v) << 32)
                | (attr << 40)
                | (((stub >> 16) & 0xFFFF) << 48);
            ENTRY_IDT.0[2 * v] = low;
            ENTRY_IDT.0[2 * v + 1] = stub >> 32;
        }
    }
    INSTALLED.store(true, Ordering::Release);
    load_idt();
    log::warn!(
        "MaiProc : IDT d'entree installee (256 talons), SMEP {}, SMAP {}",
        SMEP_ON.load(Ordering::Relaxed), smap_enabled(),
    );
}

/// Vrai une fois l'IDT d'entrée construite.
pub fn is_installed() -> bool {
    INSTALLED.load(Ordering::Acquire)
}

/// Charge l'IDT d'entrée sur le cœur courant.
pub fn load_idt() {
    #[repr(C, packed)]
    struct Ptr { limit: u16, base: u64 }
    let p = Ptr { limit: (256 * 16 - 1) as u16, base: unsafe { core::ptr::addr_of!(ENTRY_IDT) } as u64 };
    // SAFETY : la table est statique et complète.
    unsafe { core::arch::asm!("lidt [{}]", in(reg) &p, options(readonly, nostack, preserves_flags)) };
}

// --- Passage en mode utilisateur ---------------------------------------------------------------

/// Bascule le fil courant en mode utilisateur avec les registres `regs`. Le cadre est construit
/// au sommet de la pile noyau `kstack_top` (alignée sur 16), qui doit déjà être celle déclarée
/// par [`set_kernel_stack`]. Ne revient jamais : les entrées suivantes repartent de ce sommet.
///
/// # Safety
/// `kstack_top` est le sommet de la pile noyau de la tâche courante, inutilisée au-dessus de
/// l'appelant ; l'espace d'adresses du processus est chargé et l'état vectoriel restauré.
pub unsafe fn enter_user(regs: &UserRegs, kstack_top: usize) -> ! {
    let frame = (kstack_top - core::mem::size_of::<UserRegs>()) as *mut UserRegs;
    frame.write(regs.clone());
    set_kernel_stack(kstack_top);
    entry::jump_to_return_path(frame as usize)
}

/// Prépare le premier passage d'un fil : reçoit `arg` et le sommet de pile noyau choisi
/// (aligné sur 16, sous le point de retour), renvoie l'adresse du cadre ([`frame_at`]) une fois
/// les registres initiaux écrits, l'espace d'adresses chargé, l'état vectoriel en place et la pile
/// déclarée ([`set_kernel_stack`]). Appelée interruptions coupées.
pub type PrepareFn = extern "C" fn(arg: usize, kstack_top: usize) -> usize;

/// Exécute un fil en mode utilisateur **avec point de retour** : la pile noyau du fil commence
/// juste sous l'appelant, et [`exit_user`] (depuis un appel système ou une faute du fil) fait
/// revenir `run_user` avec un code, comme un `longjmp`. La tâche noyau qui porte le fil termine
/// donc normalement (nettoyage, `join`) au lieu d'être abandonnée au milieu de sa pile.
///
/// Les 16 octets `[kstack_top, kstack_top + 16)` sont libres pour l'appelant ([`thread_slot`]).
///
/// # Safety
/// `jmp_slot` reste valide jusqu'au retour ; `prepare` respecte son contrat.
pub unsafe fn run_user(prepare: PrepareFn, arg: usize, jmp_slot: *mut usize) -> u64 {
    entry::run_user_trampoline(prepare as usize, arg, jmp_slot)
}

/// Termine le fil utilisateur courant : `run_user` revient avec `code` (interruptions actives).
///
/// # Safety
/// `jmp` est la valeur posée par `run_user` pour le fil courant, sur la pile courante.
pub unsafe fn exit_user(jmp: usize, code: u64) -> ! {
    entry::exit_user_trampoline(jmp, code)
}

/// Cadre utilisateur d'une pile noyau de sommet `kstack_top`.
#[inline(always)]
pub fn frame_at(kstack_top: usize) -> *mut UserRegs {
    (kstack_top - core::mem::size_of::<UserRegs>()) as *mut UserRegs
}

/// Zone d'information du fil, juste au-dessus du cadre : `regs` est toujours le cadre d'une
/// entrée depuis le mode utilisateur, au sommet de la pile noyau (comme l'ancien `thread_info`
/// de Linux en bas de pile, mais sans recherche par cœur).
#[inline(always)]
pub fn thread_slot(regs: &UserRegs) -> *mut usize {
    (regs as *const UserRegs as usize + core::mem::size_of::<UserRegs>()) as *mut usize
}

// --- Partie Rust des entrées --------------------------------------------------------------------

#[inline(always)]
fn hook<T: Copy>(slot: &AtomicUsize) -> Option<T> {
    let raw = slot.load(Ordering::Acquire);
    if raw == 0 {
        None
    } else {
        // SAFETY : `slot` ne contient que des pointeurs de fonction du type `T`.
        Some(unsafe { core::mem::transmute_copy::<usize, T>(&raw) })
    }
}

/// Entrée commune depuis le mode utilisateur (interruptions coupées).
extern "C" fn uentry_trap(regs: &mut UserRegs) {
    let v = regs.vector;
    if is_syscall_vector(v) {
        // SAFETY : pile noyau propre à la tâche, GS/FS du noyau en place.
        unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
        match hook::<SyscallHook>(&SYSCALL_HOOK) {
            Some(h) => h(regs),
            None => regs.rax = (-38i64) as u64, // ENOSYS
        }
        return;
    }
    if v < 32 {
        let cr2: u64;
        // SAFETY : lecture de CR2 avant toute autre faute possible.
        unsafe { core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack, preserves_flags)) };
        match hook::<FaultHook>(&FAULT_HOOK) {
            Some(h) => h(regs, v as u8, cr2),
            None => panic!("MaiProc : exception {} en mode utilisateur sans gestionnaire ({:#x?})", v, regs),
        }
        return;
    }
    // IRQ : gestionnaire historique, avec un faux cadre d'interruption de niveau 0.
    let handler = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(HANDLERS[v as usize])) };
    if handler != 0 {
        // SAFETY : gestionnaire `x86-interrupt` sans code d'erreur (vecteurs ≥ 32).
        unsafe { entry::invoke_legacy(handler) };
    }
}

/// Juste avant le retour en mode utilisateur (interruptions coupées).
extern "C" fn uentry_prepare_return(regs: &mut UserRegs) {
    if let Some(h) = hook::<ReturnHook>(&RETURN_HOOK) {
        h(regs);
        unsafe { core::arch::asm!("cli", options(nomem, nostack)) };
    }
    regs.ss = USER_SS;
    regs.rflags = (regs.rflags & RFLAGS_USER_MASK) | RFLAGS_IF | RFLAGS_RESERVED;
    if regs.cs == USER32_CS {
        // Mode compatible : `eip`/`esp` sur 32 bits. `sysretl` seulement pour un cadre venu
        // de `syscall` 32 bits et sans pas-à-pas (TF déclencherait #DB sur la 1re instruction).
        regs.rip &= 0xFFFF_FFFF;
        regs.rsp &= 0xFFFF_FFFF;
        if regs.vector == SYSCALL_VECTOR || (regs.vector == SYSCALL32_VECTOR && regs.rflags & RFLAGS_TF != 0) {
            regs.vector = FORCE_IRET_VECTOR;
        }
        return;
    }
    regs.cs = USER_CS;
    if regs.vector == SYSCALL32_VECTOR || (regs.vector == SYSCALL_VECTOR && regs.rip >= SYSRET_RIP_LIMIT) {
        regs.vector = FORCE_IRET_VECTOR;
    }
}

/// Entrée paranoïaque (NMI, #MC), quel que soit le mode interrompu.
extern "C" fn uentry_paranoid(regs: &mut UserRegs) {
    if regs.vector == 2 {
        if let Some(h) = hook::<NmiHook>(&NMI_HOOK) {
            if h(regs) {
                return;
            }
        }
        log::error!("MaiProc : NMI inattendue (rip {:#x}, cs {:#x})", regs.rip, regs.cs);
        return;
    }
    let handler = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(HANDLERS[regs.vector as usize])) };
    if handler != 0 {
        // SAFETY : #MC n'a pas de code d'erreur ; son gestionnaire ne revient pas.
        unsafe { entry::invoke_legacy(handler) };
    }
}
