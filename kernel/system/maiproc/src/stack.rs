//! Disposition aléatoire de l'espace et pile initiale (ABI System V x86_64 et i386).
//!
//! ```text
//!  fin ──────┬─ aléa (64 bits : 22 bits de pages, 16 Gio ; 32 bits : 11 bits, 8 Mio)
//!            ├─ [stack] 8 Mio (RLIMIT_STACK), pages créées à la demande
//!            │    chaînes argv/envp, AT_EXECFN, "x86_64" ou "i686", 16 octets AT_RANDOM
//!            │    auxv, NULL, envp[], NULL, argv[], argc  ← rsp (aligné sur 16)
//!            ├─ écart 128 Mio + aléa (64 bits : 28 bits de pages ; 32 bits : 8 bits)
//!  mmap_top ─┴─ zone mmap (descendante) … tas (brk) … exécutable
//! ```
//!
//! `fin` = [`USER_END`] pour un processus 64 bits, [`TASK_SIZE32`] pour un processus 32 bits
//! (comme `IA32_PAGE_OFFSET` de Linux) ; les mots de la pile font alors 4 octets.

use alloc::{string::String, vec::Vec};
use kernel_config::memory::PAGE_SIZE;
use uvm::{page_down, Backing, Mm, Prot, Vma, VmError, USER_END};

use crate::elf::Loaded;

/// Fin de l'espace d'un processus 32 bits (`IA32_PAGE_OFFSET` de Linux : 4 Gio − 8 Kio).
pub const TASK_SIZE32: usize = 0xFFFF_E000;

const STACK_SIZE: usize = 8 << 20;
const STACK_RANDOM_BITS: u32 = 22;
const STACK_RANDOM_BITS32: u32 = 11;
const MMAP_GAP: usize = 128 << 20;
const MMAP_RANDOM_BITS: u32 = 28;
const MMAP_RANDOM_BITS32: u32 = 8;

const AT_NULL: u64 = 0;
const AT_PHDR: u64 = 3;
const AT_PHENT: u64 = 4;
const AT_PHNUM: u64 = 5;
const AT_PAGESZ: u64 = 6;
const AT_BASE: u64 = 7;
const AT_FLAGS: u64 = 8;
const AT_ENTRY: u64 = 9;
const AT_UID: u64 = 11;
const AT_EUID: u64 = 12;
const AT_GID: u64 = 13;
const AT_EGID: u64 = 14;
const AT_PLATFORM: u64 = 15;
const AT_HWCAP: u64 = 16;
const AT_CLKTCK: u64 = 17;
const AT_SECURE: u64 = 23;
const AT_RANDOM: u64 = 25;
const AT_HWCAP2: u64 = 26;
const AT_EXECFN: u64 = 31;
const AT_SYSINFO: u64 = 32;
const AT_SYSINFO_EHDR: u64 = 33;
const AT_MINSIGSTKSZ: u64 = 51;
/// `HWCAP2_FSGSBASE` : les instructions `rd/wr{fs,gs}base` sont permises en mode utilisateur.
const HWCAP2_FSGSBASE: u64 = 1 << 1;

pub struct Layout {
    pub stack_top: usize,
    pub stack_bottom: usize,
    pub mmap_top: usize,
    /// Processus 32 bits.
    pub compat: bool,
}

impl Layout {
    pub fn randomized(compat: bool) -> Layout {
        let (end, sbits, mbits) = if compat {
            (TASK_SIZE32, STACK_RANDOM_BITS32, MMAP_RANDOM_BITS32)
        } else {
            (USER_END, STACK_RANDOM_BITS, MMAP_RANDOM_BITS)
        };
        let stack_top = page_down(end - uvm::rand::random_pages(sbits));
        let stack_bottom = stack_top - STACK_SIZE;
        let mmap_top = page_down(stack_bottom - MMAP_GAP - uvm::rand::random_pages(mbits));
        Layout { stack_top, stack_bottom, mmap_top, compat }
    }

    /// Fin de l'espace utilisable du processus.
    pub fn task_size(&self) -> usize {
        if self.compat { TASK_SIZE32 } else { USER_END }
    }
}

fn hwcap() -> u64 {
    // SAFETY : CPUID toujours disponible en mode long.
    unsafe { core::arch::x86_64::__cpuid(1).edx as u64 }
}

/// Pile initiale construite.
pub struct Built {
    pub sp: usize,
    /// `[début, fin)` des arguments puis de l'environnement (`/proc/<pid>/cmdline`, `environ`).
    pub args: [usize; 4],
}

/// Images chargées décrites par le vecteur auxiliaire.
pub struct Images<'a> {
    pub main: &'a Loaded,
    pub interp: Option<&'a Loaded>,
    /// Adresse de l'image du vDSO (`AT_SYSINFO_EHDR`).
    pub vdso: Option<usize>,
    /// Point d'entrée des appels système du vDSO 32 bits (`AT_SYSINFO`, `__kernel_vsyscall`).
    pub sysinfo: Option<usize>,
}

/// Projette la pile et y écrit l'état initial.
pub fn build(mm: &Mm, layout: &Layout, execfn: &str, argv: &[String], envp: &[String], img: Images<'_>) -> Result<Built, VmError> {
    let Images { main, interp, vdso, sysinfo } = img;
    let word = if layout.compat { 4 } else { 8 };
    let prot = if main.exec_stack { Prot(Prot::READ | Prot::WRITE | Prot::EXEC) } else { Prot::RW };
    mm.map_fixed(Vma { start: layout.stack_bottom, end: layout.stack_top, prot, backing: Backing::Anon, name: "[stack]".into(), may_write: true })?;

    // --- Zone des chaînes (au sommet, 8 octets nuls au-dessus comme Linux) ---
    let mut strings: Vec<u8> = Vec::new();
    let push_str = |s: &[u8], v: &mut Vec<u8>| -> usize {
        let off = v.len();
        v.extend_from_slice(s);
        v.push(0);
        off
    };
    let argv_off: Vec<usize> = argv.iter().map(|a| push_str(&fs_node::rawname::encode(a), &mut strings)).collect();
    let envp_off: Vec<usize> = envp.iter().map(|e| push_str(&fs_node::rawname::encode(e), &mut strings)).collect();
    let execfn_off = push_str(&fs_node::rawname::encode(execfn), &mut strings);
    let platform_off = push_str(if layout.compat { b"i686" } else { b"x86_64" }, &mut strings);
    let random_off = strings.len();
    let mut rnd = [0u8; 16];
    rnd[..8].copy_from_slice(&uvm::rand::random_u64().to_le_bytes());
    rnd[8..].copy_from_slice(&uvm::rand::random_u64().to_le_bytes());
    strings.extend_from_slice(&rnd);

    let str_base = (layout.stack_top - 8 - strings.len()) & !15;
    let at = |off: usize| (str_base + off) as u64;

    // --- Vecteur auxiliaire (même ordre que `create_elf_tables` de Linux) ---
    let minsig = if layout.compat {
        (crate::signal::fpstate_size() + 112 + 1024 + 15) & !15
    } else {
        (uentry::xsave_area_size() + 1024 + 15) & !15
    };
    let mut auxv: Vec<(u64, u64)> = Vec::with_capacity(24);
    if let (true, Some(entry)) = (layout.compat, sysinfo) {
        auxv.push((AT_SYSINFO, entry as u64));
    }
    if let Some(v) = vdso {
        auxv.push((AT_SYSINFO_EHDR, v as u64));
    }
    auxv.extend_from_slice(&[
        (AT_MINSIGSTKSZ, minsig as u64),
        (AT_HWCAP, hwcap()),
        (AT_PAGESZ, PAGE_SIZE as u64),
        (AT_CLKTCK, 100),
        (AT_PHDR, main.phdr as u64),
        (AT_PHENT, main.phent as u64),
        (AT_PHNUM, main.phnum as u64),
        (AT_BASE, interp.map_or(0, |i| i.bias) as u64),
        (AT_FLAGS, 0),
        (AT_ENTRY, main.entry as u64),
        (AT_UID, 0),
        (AT_EUID, 0),
        (AT_GID, 0),
        (AT_EGID, 0),
        (AT_SECURE, 0),
        (AT_RANDOM, at(random_off)),
        (AT_HWCAP2, HWCAP2_FSGSBASE),
        (AT_EXECFN, at(execfn_off)),
        (AT_PLATFORM, at(platform_off)),
        (AT_NULL, 0),
    ]);

    // --- argc, argv, envp, auxv ---
    let words = 1 + argv.len() + 1 + envp.len() + 1 + 2 * auxv.len();
    let sp = (str_base - words * word) & !15;
    if sp < layout.stack_bottom + PAGE_SIZE {
        return Err(VmError::NoMemory); // arguments trop gros (E2BIG)
    }
    let mut table: Vec<u64> = Vec::with_capacity(words);
    table.push(argv.len() as u64);
    table.extend(argv_off.iter().map(|&o| at(o)));
    table.push(0);
    table.extend(envp_off.iter().map(|&o| at(o)));
    table.push(0);
    for (k, v) in &auxv {
        table.push(*k);
        table.push(*v);
    }

    let bytes: Vec<u8> = if layout.compat {
        table.iter().flat_map(|w| (*w as u32).to_le_bytes()).collect()
    } else {
        table.iter().flat_map(|w| w.to_le_bytes()).collect()
    };
    mm.copy_to_user(sp, &bytes)?;
    mm.copy_to_user(str_base, &strings)?;
    let args_end = envp_off.first().copied().unwrap_or(execfn_off);
    Ok(Built { sp, args: [str_base, str_base + args_end, str_base + args_end, str_base + execfn_off] })
}
