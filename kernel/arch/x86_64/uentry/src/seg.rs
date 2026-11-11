//! Segments des fils utilisateur : sélecteurs `ds`, `es`, `fs`, `gs` et entrées TLS de la GDT.
//!
//! En mode 64 bits, seuls comptent les bases de `fs`/`gs` (sauvées dans le cadre par
//! `rdfsbase`/`rdgsbase`) ; un fil 32 bits (mode compatible) a besoin de vrais sélecteurs :
//! `ds`/`es` = données utilisateur, `gs` (glibc) ou `fs` désignant une entrée TLS de la GDT
//! posée par `set_thread_area`. Comme Linux (`__switch_to`, `load_TLS`) :
//!
//! * chaque fil garde ses sélecteurs et ses trois descripteurs TLS ([`SegState`]) ;
//! * au changement de contexte, les sélecteurs du fil qui part sont relus dans les registres
//!   (le noyau ne les touche jamais), les descripteurs TLS de celui qui arrive sont écrits dans
//!   la GDT du cœur, puis ses sélecteurs rechargés s'ils diffèrent ;
//! * un sélecteur qui ne désigne plus un segment valable (entrée TLS effacée entre-temps) est
//!   remplacé par le sélecteur nul au lieu de lever une faute dans le noyau.
//!
//! Charger `fs` ou `gs` écrase la base cachée : celle du noyau (TLS du noyau, CLS du cœur) est
//! remise aussitôt par `wrfsbase`/`wrgsbase`, interruptions coupées. La base utilisateur vient
//! ensuite du cadre au retour (chemin commun de `entry.rs`).

use core::cell::UnsafeCell;

/// Première entrée TLS de la GDT (`GDT_ENTRY_TLS_MIN` de Linux x86_64) : sélecteurs 0x63,
/// 0x6b, 0x73.
pub const TLS_ENTRY_MIN: usize = 12;
/// Nombre d'entrées TLS par fil.
pub const TLS_ENTRIES: usize = 3;
/// Entrées de la GDT (voir `gdt::Gdt`).
const GDT_ENTRIES: usize = 16;

/// Interruptions coupées jusqu'à la fin de la portée (rétablies si elles étaient actives).
struct NoIrq(bool);

impl NoIrq {
    fn new() -> NoIrq {
        let flags: u64;
        // SAFETY : lecture de RFLAGS puis `cli`.
        unsafe { core::arch::asm!("pushfq", "pop {}", "cli", out(reg) flags, options(nomem)) };
        NoIrq(flags & (1 << 9) != 0)
    }
}

impl Drop for NoIrq {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY : rétablit l'état d'avant `new`.
            unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
        }
    }
}

/// Sélecteurs de données d'un fil.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Selectors {
    pub ds: u16,
    pub es: u16,
    pub fs: u16,
    pub gs: u16,
}

#[derive(Clone, Copy, Default)]
struct Inner {
    sel: Selectors,
    tls: [u64; TLS_ENTRIES],
}

/// État des segments d'un fil. Lu et écrit par le fil lui-même interruptions coupées, et par
/// le changement de contexte (qui n'écrit que l'état du fil qui part : le fil courant).
pub struct SegState {
    inner: UnsafeCell<Inner>,
}

// SAFETY : voir la règle d'accès ci-dessus.
unsafe impl Sync for SegState {}
unsafe impl Send for SegState {}

impl Default for SegState {
    fn default() -> Self { SegState::new() }
}

impl SegState {
    /// Fil 64 bits : sélecteurs nuls, aucune entrée TLS.
    pub const fn new() -> SegState {
        SegState { inner: UnsafeCell::new(Inner { sel: Selectors { ds: 0, es: 0, fs: 0, gs: 0 }, tls: [0; TLS_ENTRIES] }) }
    }

    /// Fil 32 bits neuf (`start_thread_ia32` de Linux) : `ds` = `es` = données utilisateur,
    /// `fs` = `gs` = 0, aucune entrée TLS.
    pub fn compat() -> SegState {
        let ds = crate::USER_SS as u16;
        SegState { inner: UnsafeCell::new(Inner { sel: Selectors { ds, es: ds, fs: 0, gs: 0 }, tls: [0; TLS_ENTRIES] }) }
    }

    /// Copie pour un fil créé par `clone`/`fork` (même sélecteurs, mêmes entrées TLS). Appelé
    /// par le fil courant : ses sélecteurs vivants sont relus dans les registres.
    pub fn duplicate_current(&self) -> SegState {
        let held = NoIrq::new();
        // SAFETY : interruptions coupées, état du fil courant.
        let mut inner = unsafe { *self.inner.get() };
        inner.sel = read_selectors();
        drop(held);
        SegState { inner: UnsafeCell::new(inner) }
    }

    /// `execve` du fil courant : plus aucune entrée TLS, sélecteurs initiaux du nouveau
    /// programme (32 ou 64 bits), chargés aussitôt.
    pub fn exec_reset(&self, compat: bool) {
        {
            let _held = NoIrq::new();
            // SAFETY : interruptions coupées ; état du fil courant.
            unsafe { (*self.inner.get()).tls = [0; TLS_ENTRIES] };
        }
        let ds = if compat { crate::USER_SS as u16 } else { 0 };
        load_segments(self, Selectors { ds, es: ds, fs: 0, gs: 0 });
    }

    /// Sélecteurs et descripteurs TLS (fil courant : sélecteurs vivants).
    pub fn snapshot(&self, current: bool) -> (Selectors, [u64; TLS_ENTRIES]) {
        let held = NoIrq::new();
        // SAFETY : interruptions coupées.
        let mut inner = unsafe { *self.inner.get() };
        if current {
            inner.sel = read_selectors();
        }
        drop(held);
        (inner.sel, inner.tls)
    }
}

#[inline(always)]
fn read_selectors() -> Selectors {
    let (ds, es, fs, gs): (u16, u16, u16, u16);
    // SAFETY : lecture des registres de segment.
    unsafe {
        core::arch::asm!("mov {0:x}, ds", out(reg) ds, options(nomem, nostack, preserves_flags));
        core::arch::asm!("mov {0:x}, es", out(reg) es, options(nomem, nostack, preserves_flags));
        core::arch::asm!("mov {0:x}, fs", out(reg) fs, options(nomem, nostack, preserves_flags));
        core::arch::asm!("mov {0:x}, gs", out(reg) gs, options(nomem, nostack, preserves_flags));
    }
    Selectors { ds, es, fs, gs }
}

/// Sélecteurs vivants du cœur (ceux du dernier fil utilisateur qui y a tourné).
pub fn current_selectors() -> Selectors {
    read_selectors()
}

/// Base de la GDT chargée sur ce cœur (`sgdt`).
pub(crate) fn current_gdt_base() -> u64 {
    #[repr(C, packed)]
    struct Ptr { limit: u16, base: u64 }
    let mut p = Ptr { limit: 0, base: 0 };
    // SAFETY : `sgdt` écrit 10 octets dans `p`.
    unsafe { core::arch::asm!("sgdt [{}]", in(reg) &mut p, options(nostack, preserves_flags)) };
    p.base
}

fn gdt() -> Option<*mut u64> {
    let b = crate::current_block()?;
    if b.gdt_base == 0 { None } else { Some(b.gdt_base as *mut u64) }
}

/// Base (32 bits) d'un descripteur de segment.
pub fn descriptor_base(desc: u64) -> u32 {
    (((desc >> 16) & 0xFF_FFFF) | ((desc >> 56) & 0xFF) << 24) as u32
}

/// Vrai si `sel` peut être chargé dans un registre de données depuis l'anneau 3 avec la GDT
/// `gdt` : sélecteur nul, ou entrée présente de la GDT, segment de code ou de données de DPL 3,
/// lisible (données, ou code lisible).
fn selector_ok(sel: u16, gdt: *const u64) -> bool {
    if sel & !3 == 0 {
        return true;
    }
    if sel & 4 != 0 || sel & 3 != 3 {
        return false; // LDT absente ; RPL 3 seulement
    }
    let idx = (sel >> 3) as usize;
    if idx >= GDT_ENTRIES || idx == 0 {
        return false;
    }
    // SAFETY : entrée de la GDT de ce cœur (16 entrées).
    let d = unsafe { core::ptr::read_volatile(gdt.add(idx)) };
    let present = d & (1 << 47) != 0;
    let user_seg = d & (1 << 44) != 0;
    let dpl3 = (d >> 45) & 3 == 3;
    let code = d & (1 << 43) != 0;
    let readable = d & (1 << 41) != 0;
    present && user_seg && dpl3 && (!code || readable)
}

/// Écrit les entrées TLS du fil qui prend le cœur (interruptions coupées).
unsafe fn load_tls(tls: &[u64; TLS_ENTRIES]) {
    let Some(b) = crate::current_block() else { return };
    let Some(g) = gdt() else { return };
    let any = tls.iter().any(|&d| d != 0);
    if !any && b.tls_loaded == 0 {
        return;
    }
    for (i, &d) in tls.iter().enumerate() {
        let slot = g.add(TLS_ENTRY_MIN + i);
        if core::ptr::read_volatile(slot) != d {
            core::ptr::write_volatile(slot, d);
        }
    }
    b.tls_loaded = any as u64;
}

/// Charge `want` dans les registres de segment, en gardant les bases du noyau. Interruptions
/// coupées. `ds`/`es` ne sont rechargés que s'ils diffèrent de `cur` (entrées fixes de la GDT) ;
/// un `fs`/`gs` non nul l'est toujours : deux fils peuvent avoir le même sélecteur TLS avec des
/// descripteurs différents (comme `load_seg_legacy` de Linux). Renvoie les sélecteurs chargés.
unsafe fn load_selectors(cur: Selectors, want: Selectors) -> Selectors {
    let g = gdt().unwrap_or(core::ptr::null_mut());
    let fix = |s: u16| if g.is_null() || selector_ok(s, g) { s } else { 0 };
    let want = Selectors { ds: fix(want.ds), es: fix(want.es), fs: fix(want.fs), gs: fix(want.gs) };
    if want.ds != cur.ds {
        core::arch::asm!("mov ds, {0:x}", in(reg) want.ds, options(nostack, preserves_flags));
    }
    if want.es != cur.es {
        core::arch::asm!("mov es, {0:x}", in(reg) want.es, options(nostack, preserves_flags));
    }
    if want.fs != cur.fs || want.fs & !3 != 0 {
        core::arch::asm!(
            "rdfsbase {b}",
            "mov fs, {s:x}",
            "wrfsbase {b}",
            b = out(reg) _, s = in(reg) want.fs, options(nostack, preserves_flags),
        );
    }
    if want.gs != cur.gs || want.gs & !3 != 0 {
        core::arch::asm!(
            "rdgsbase {b}",
            "mov gs, {s:x}",
            "wrgsbase {b}",
            b = out(reg) _, s = in(reg) want.gs, options(nostack, preserves_flags),
        );
    }
    want
}

/// Changement de contexte (interruptions coupées) : sauve les sélecteurs du fil utilisateur qui
/// part, pose les entrées TLS et les sélecteurs de celui qui arrive.
///
/// # Safety
/// Appelé par l'ordonnanceur interruptions coupées, `prev` étant le fil qui quitte ce cœur.
pub unsafe fn switch_segments(prev: Option<&SegState>, next: Option<&SegState>) {
    let cur = read_selectors();
    if let Some(p) = prev {
        (*p.inner.get()).sel = cur;
    }
    if let Some(n) = next {
        let inner = &mut *n.inner.get();
        load_tls(&inner.tls);
        inner.sel = load_selectors(cur, inner.sel);
    }
}

/// Fixe les sélecteurs du fil courant et les charge aussitôt (`execve`, `sigreturn`, signal
/// livré à un fil 32 bits). Renvoie les sélecteurs chargés (un sélecteur invalide devient nul)
/// et les bases (`fs`, `gs`) des descripteurs désignés (0 pour un sélecteur nul), à recopier
/// dans le cadre : c'est lui qui donne les bases au retour.
pub fn load_segments(state: &SegState, want: Selectors) -> (Selectors, u64, u64) {
    let held = NoIrq::new();
    // SAFETY : interruptions coupées ; état du fil courant.
    let r = unsafe {
        let inner = &mut *state.inner.get();
        load_tls(&inner.tls);
        let loaded = load_selectors(read_selectors(), want);
        inner.sel = loaded;
        let base = |s: u16| -> u64 {
            match gdt() {
                Some(g) if s & !3 != 0 => descriptor_base(core::ptr::read_volatile(g.add((s >> 3) as usize))) as u64,
                _ => 0,
            }
        };
        (loaded, base(loaded.fs), base(loaded.gs))
    };
    drop(held);
    r
}

/// Remplace l'entrée TLS `idx` (0..3) de `state` par `desc` ; `current` : c'est le fil courant,
/// la GDT du cœur est mise à jour aussitôt (un `mov gs` qui suit la voit). Renvoie les bases
/// à recopier dans le cadre pour `fs` et `gs` s'ils désignent cette entrée (comme
/// `do_set_thread_area`, qui recharge un sélecteur modifié).
pub fn set_tls(state: &SegState, idx: usize, desc: u64, current: bool) -> (Option<u64>, Option<u64>) {
    if idx >= TLS_ENTRIES {
        return (None, None);
    }
    let held = NoIrq::new();
    // SAFETY : interruptions coupées ; état du fil courant, ou d'un fil pas encore démarré.
    let r = unsafe {
        let inner = &mut *state.inner.get();
        inner.tls[idx] = desc;
        let sel = if current { read_selectors() } else { inner.sel };
        if current {
            load_tls(&inner.tls);
        }
        let me = (((TLS_ENTRY_MIN + idx) << 3) | 3) as u16;
        let base = descriptor_base(desc) as u64;
        ((sel.fs == me).then_some(base), (sel.gs == me).then_some(base))
    };
    drop(held);
    r
}
