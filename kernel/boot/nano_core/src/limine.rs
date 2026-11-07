//! Point d'entrée depuis Limine (protocole Limine natif), en BIOS comme en UEFI.
//!
//! Limine charge l'image ELF du noyau directement en mode long, à ses adresses hautes, et lui
//! passe la machine par des « requêtes » placées dans l'image : le noyau les déclare ici, entre
//! les marqueurs de début et de fin, et Limine y accroche ses réponses avant de sauter sur
//! `_start` (`asm/limine/boot.asm`). Plus de code 32 bits, plus de tables de pages faites à la
//! main : `_start` règle le processeur, passe sur la pile du noyau et appelle [`limine_entry`].
//!
//! Les réponses vivent dans la mémoire du chargeur, lue via son HHDM, qui disparaît au premier
//! changement de tables de pages : [`limine_entry`] copie donc ce qui sert au démarrage dans une
//! [`LimineInfo`] et la liste des cœurs secondaires dans [`LIMINE_APS`].

#![allow(dead_code)] // champs des structures du protocole lus seulement en partie

use crate::nano_core;
use boot_info::limine::{Framebuffer, LimineInfo, MEMMAP_USABLE};
use captain::LimineAp;
use core::{
    fmt::{self, Write},
    ptr::{addr_of, addr_of_mut, read_volatile},
    slice,
};
use memory::VirtualAddress;

// ---------------------------------------------------------------------------------------------
// Requêtes (spécification : limine-protocol, PROTOCOL.md)
// ---------------------------------------------------------------------------------------------

const COMMON_MAGIC: [u64; 2] = [0xc7b1dd30df4c8b88, 0x0a82e883a194f07b];
/// Révision du protocole demandée (la plus récente ; sur x86_64 elle fixe un état processeur,
/// des APIC et des IOMMU entièrement défini à l'entrée).
const BASE_REVISION: u64 = 6;

const fn id(a: u64, b: u64) -> [u64; 4] {
    [COMMON_MAGIC[0], COMMON_MAGIC[1], a, b]
}

#[repr(C)]
struct Request<R> {
    id: [u64; 4],
    revision: u64,
    response: *const R,
}

#[repr(C)]
struct MpRequest {
    id: [u64; 4],
    revision: u64,
    response: *const MpResponse,
    flags: u64,
}

#[repr(C)]
struct ModuleRequest {
    id: [u64; 4],
    revision: u64,
    response: *const ModuleResponse,
    internal_module_count: u64,
    internal_modules: *const u8,
}

#[repr(C)]
struct BootloaderInfoResponse {
    revision: u64,
    name: *const u8,
    version: *const u8,
}

#[repr(C)]
struct HhdmResponse {
    revision: u64,
    offset: u64,
}

#[repr(C)]
struct MemmapEntry {
    base: u64,
    length: u64,
    typ: u64,
}

#[repr(C)]
struct MemmapResponse {
    revision: u64,
    entry_count: u64,
    entries: *const *const MemmapEntry,
}

/// Début de `struct limine_file` (les champs suivants ne servent pas).
#[repr(C)]
struct LimineFile {
    revision: u64,
    address: *const u8,
    size: u64,
    path: *const u8,
    string: *const u8,
}

#[repr(C)]
struct ExecutableFileResponse {
    revision: u64,
    file: *const LimineFile,
}

#[repr(C)]
struct ExecutableAddressResponse {
    revision: u64,
    physical_base: u64,
    virtual_base: u64,
}

#[repr(C)]
struct ModuleResponse {
    revision: u64,
    module_count: u64,
    modules: *const *const LimineFile,
}

#[repr(C)]
struct RsdpResponse {
    revision: u64,
    address: u64,
}

#[repr(C)]
struct LimineFramebuffer {
    address: u64,
    width: u64,
    height: u64,
    pitch: u64,
    bpp: u16,
    memory_model: u8,
    red_mask_size: u8,
    red_mask_shift: u8,
    green_mask_size: u8,
    green_mask_shift: u8,
    blue_mask_size: u8,
    blue_mask_shift: u8,
}

#[repr(C)]
struct FramebufferResponse {
    revision: u64,
    framebuffer_count: u64,
    framebuffers: *const *const LimineFramebuffer,
}

#[repr(C)]
struct MpInfo {
    processor_id: u32,
    lapic_id: u32,
    reserved: u64,
    goto_address: u64,
    extra_argument: u64,
}

#[repr(C)]
struct MpResponse {
    revision: u64,
    flags: u32,
    bsp_lapic_id: u32,
    cpu_count: u64,
    cpus: *const *const MpInfo,
}

#[used]
#[link_section = ".limine_requests_start"]
static mut REQUESTS_START: [u64; 4] = [0xf6b8f4b39de7d1ae, 0xfab91a6940fcb9cf, 0x785c6ed015d3e316, 0x181e920a7852b9d9];

#[used]
#[link_section = ".limine_requests"]
static mut BASE_REVISION_TAG: [u64; 3] = [0xf9562b2d5c95a6c8, 0x6a7b384944536bdc, BASE_REVISION];

#[used]
#[link_section = ".limine_requests"]
static mut BOOTLOADER_INFO: Request<BootloaderInfoResponse> = Request {
    id: id(0xf55038d8e2a1202f, 0x279426fcf5f59740), revision: 0, response: core::ptr::null(),
};

#[used]
#[link_section = ".limine_requests"]
static mut HHDM: Request<HhdmResponse> = Request {
    id: id(0x48dcf1cb8ad2b852, 0x63984e959a98244b), revision: 0, response: core::ptr::null(),
};

#[used]
#[link_section = ".limine_requests"]
static mut MEMMAP: Request<MemmapResponse> = Request {
    id: id(0x67cf3d9d378a806f, 0xe304acdfc50c3c62), revision: 0, response: core::ptr::null(),
};

#[used]
#[link_section = ".limine_requests"]
static mut EXECUTABLE_FILE: Request<ExecutableFileResponse> = Request {
    id: id(0xad97e90e83f1ed67, 0x31eb5d1c5ff23b69), revision: 0, response: core::ptr::null(),
};

#[used]
#[link_section = ".limine_requests"]
static mut EXECUTABLE_ADDRESS: Request<ExecutableAddressResponse> = Request {
    id: id(0x71ba76863cc55f63, 0xb2644a48c516a487), revision: 0, response: core::ptr::null(),
};

#[used]
#[link_section = ".limine_requests"]
static mut MODULES: ModuleRequest = ModuleRequest {
    id: id(0x3e7e279702be32af, 0xca1c4f3bd1280cee), revision: 0, response: core::ptr::null(),
    internal_module_count: 0, internal_modules: core::ptr::null(),
};

#[used]
#[link_section = ".limine_requests"]
static mut RSDP: Request<RsdpResponse> = Request {
    id: id(0xc5e77b6b397e7b43, 0x27637845accdcf3c), revision: 0, response: core::ptr::null(),
};

#[used]
#[link_section = ".limine_requests"]
static mut FRAMEBUFFER: Request<FramebufferResponse> = Request {
    id: id(0x9d5827dcd881dd75, 0xa3148604f6fab11b), revision: 0, response: core::ptr::null(),
};

/// Démarrage des cœurs secondaires par Limine, en xAPIC (le noyau passe lui-même en x2APIC si
/// le processeur le permet, cœur par cœur).
#[used]
#[link_section = ".limine_requests"]
static mut MP: MpRequest = MpRequest {
    id: id(0x95a67b819a1b857e, 0xa0b61b723b6a73e0), revision: 0, response: core::ptr::null(), flags: 0,
};

#[used]
#[link_section = ".limine_requests_end"]
static mut REQUESTS_END: [u64; 2] = [0xadc0e0531bb10d03, 0x9572709f31764c62];

// ---------------------------------------------------------------------------------------------
// Copies durables
// ---------------------------------------------------------------------------------------------

/// Copie des informations de démarrage, seule dans ses pages (voir `boot_info::limine`).
#[link_section = ".boot_info"]
static mut BOOT_INFO: LimineInfo = LimineInfo::new();

const MAX_APS: usize = 256;
const NO_AP: LimineAp = LimineAp { processor_id: 0, lapic_id: 0, mp_info_phys: 0 };
/// Cœurs secondaires parqués par Limine (lus par `multicore_bringup` bien après le démarrage).
static mut LIMINE_APS: [LimineAp; MAX_APS] = [NO_AP; MAX_APS];
static mut LIMINE_AP_COUNT: usize = 0;

/// Nom et version du chargeur, pour le journal.
static mut BOOTLOADER: [u8; 48] = [0; 48];
static mut BOOTLOADER_LEN: usize = 0;

/// Les cœurs secondaires à libérer.
pub fn aps() -> &'static [LimineAp] {
    // SAFETY: écrits une fois par le BSP dans `limine_entry`, avant tout autre cœur.
    unsafe { &*core::ptr::slice_from_raw_parts(addr_of!(LIMINE_APS) as *const LimineAp, LIMINE_AP_COUNT) }
}

/// Résumé du démarrage, une fois le journal en place.
pub fn log_summary() {
    // SAFETY: écrits une fois dans `limine_entry`.
    let (name, info) = unsafe {
        (
            core::str::from_utf8(&*core::ptr::slice_from_raw_parts(addr_of!(BOOTLOADER) as *const u8, BOOTLOADER_LEN))
                .unwrap_or("Limine"),
            &*addr_of!(BOOT_INFO),
        )
    };
    log::warn!(
        "MaiOS demarre par {} : noyau a {:#x} (phys {:#x}), {} regions memoire ({} fusionnees, {} perdues), {} module(s), {} coeur(s) secondaire(s)",
        name, info.kernel_virt, info.kernel_phys, info.regions().len(), info.merged_regions,
        info.dropped_regions, info.modules_slice().len(), aps().len(),
    );
}

// ---------------------------------------------------------------------------------------------
// Entrée
// ---------------------------------------------------------------------------------------------

/// Sortie série brute (COM1) pour les erreurs d'avant le journal.
struct RawSerial;
impl Write for RawSerial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            // SAFETY: ports d'E/S du COM1 ; attente du registre d'émission vide.
            unsafe {
                for _ in 0..100_000 {
                    let lsr: u8;
                    core::arch::asm!("in al, dx", out("al") lsr, in("dx") 0x3FDu16, options(nomem, nostack));
                    if lsr & 0x20 != 0 {
                        break;
                    }
                }
                core::arch::asm!("out dx, al", in("dx") 0x3F8u16, in("al") b, options(nomem, nostack));
            }
        }
        Ok(())
    }
}

fn fatal(msg: &str) -> ! {
    let _ = writeln!(RawSerial, "\r\nMaiOS : demarrage Limine impossible : {msg}\r");
    loop {
        // SAFETY: arrêt définitif du processeur.
        unsafe { core::arch::asm!("cli; hlt", options(nomem, nostack)) };
    }
}

/// Lit le pointeur de réponse d'une requête (écrit par Limine, invisible pour le compilateur).
unsafe fn response<R>(ptr: *const *const R) -> Option<&'static R> {
    read_volatile(ptr).as_ref()
}

unsafe fn c_str(ptr: *const u8) -> &'static [u8] {
    if ptr.is_null() {
        return &[];
    }
    let mut len = 0;
    while *ptr.add(len) != 0 {
        len += 1;
    }
    slice::from_raw_parts(ptr, len)
}

/// Appelée par `_start` sur la pile du noyau, processeur réglé.
#[no_mangle]
pub extern "C" fn limine_entry(double_fault_stack_top: usize) -> ! {
    // SAFETY: seul le BSP s'exécute ; les réponses de Limine sont valides jusqu'au changement de
    // tables de pages, qui a lieu bien après la copie.
    let info = unsafe { collect() }.unwrap_or_else(|e| fatal(e));

    let r = (|| {
        use boot_info::BootInformation;
        let stack_size = info.stack_size()?;
        let kernel_stack_start = VirtualAddress::new(double_fault_stack_top - stack_size)
            .ok_or("BUG: kernel_stack_start virtual address is invalid")?;
        let double_fault_stack_top = VirtualAddress::new(double_fault_stack_top)
            .ok_or("BUG: double_fault_stack_top virtual address is invalid")?;
        nano_core(info, double_fault_stack_top, kernel_stack_start)
    })();
    match r {
        Ok(()) => crate::shutdown(format_args!("BUG: nano_core() unexpectedly returned!")),
        Err(e) => crate::shutdown(format_args!("{e}")),
    }
}

unsafe fn collect() -> Result<&'static LimineInfo, &'static str> {
    // Les marqueurs ne sont lus par personne : on les référence pour que l'éditeur de liens ne
    // laisse jamais de côté le morceau d'archive qui les contient.
    let _ = read_volatile(addr_of!(REQUESTS_START));
    let _ = read_volatile(addr_of!(REQUESTS_END));
    if read_volatile(addr_of!(BASE_REVISION_TAG[2])) != 0 {
        return Err("ce Limine ne connait pas la revision 6 du protocole (Limine 9 ou plus requis)");
    }
    let info = &mut *addr_of_mut!(BOOT_INFO);

    let hhdm = response(addr_of!(HHDM.response)).ok_or("pas de reponse HHDM")?.offset as usize;
    info.hhdm_offset = hhdm;

    if let Some(bl) = response(addr_of!(BOOTLOADER_INFO.response)) {
        let buf = &mut *addr_of_mut!(BOOTLOADER);
        let mut n = 0;
        for part in [c_str(bl.name), &b" "[..], c_str(bl.version)] {
            for &b in part {
                if n < buf.len() {
                    buf[n] = b;
                    n += 1;
                }
            }
        }
        BOOTLOADER_LEN = n;
    }

    // Carte mémoire : seul « utilisable » est donné aux allocateurs. La mémoire du chargeur
    // (« récupérable ») reste réservée : tables de pages, GDT et piles des cœurs parqués servent
    // jusqu'à leur libération.
    let memmap = response(addr_of!(MEMMAP.response)).ok_or("pas de carte memoire")?;
    for i in 0..memmap.entry_count as usize {
        let e = &**memmap.entries.add(i);
        info.push_region(e.base as usize, e.length as usize, e.typ == MEMMAP_USABLE)?;
    }

    // Sections de l'image ELF du noyau (en-têtes de sections du fichier chargé).
    let file = response(addr_of!(EXECUTABLE_FILE.response)).ok_or("pas de fichier noyau")?;
    let file = file.file.as_ref().ok_or("pas de fichier noyau")?;
    collect_sections(info, slice::from_raw_parts(file.address, file.size as usize))?;

    if let Some(a) = response(addr_of!(EXECUTABLE_ADDRESS.response)) {
        info.kernel_phys = a.physical_base as usize;
        info.kernel_virt = a.virtual_base as usize;
    }

    if let Some(m) = response(addr_of!(MODULES.response)) {
        for i in 0..m.module_count as usize {
            let f = &**m.modules.add(i);
            // Nom : la chaîne de la configuration (`module_string`), sinon le nom du fichier.
            let mut name = c_str(f.string);
            if name.is_empty() {
                let path = c_str(f.path);
                name = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
            }
            info.push_module(name, f.address as usize - hhdm, f.size as usize)?;
        }
    }

    if let Some(r) = response(addr_of!(RSDP.response)) {
        // Révision 4 et plus : adresse virtuelle (HHDM).
        let a = r.address as usize;
        info.rsdp = Some(if a >= hhdm { a - hhdm } else { a });
    }

    if let Some(fbs) = response(addr_of!(FRAMEBUFFER.response)) {
        if fbs.framebuffer_count > 0 {
            let fb = &**fbs.framebuffers;
            info.framebuffer = Some(Framebuffer {
                virt: fb.address as usize,
                phys: fb.address as usize - hhdm,
                width: fb.width as u32,
                height: fb.height as u32,
                pitch: fb.pitch as u32,
                bpp: fb.bpp,
                red_shift: fb.red_mask_shift,
                red_size: fb.red_mask_size,
                green_shift: fb.green_mask_shift,
                green_size: fb.green_mask_size,
                blue_shift: fb.blue_mask_shift,
                blue_size: fb.blue_mask_size,
            });
        }
    }

    if let Some(mp) = response(addr_of!(MP.response)) {
        let aps = &mut *addr_of_mut!(LIMINE_APS);
        let mut n = 0;
        for i in 0..mp.cpu_count as usize {
            let cpu_ptr = *mp.cpus.add(i);
            let cpu = &*cpu_ptr;
            if cpu.lapic_id == mp.bsp_lapic_id || n == MAX_APS {
                continue;
            }
            aps[n] = LimineAp {
                processor_id: cpu.processor_id,
                lapic_id: cpu.lapic_id,
                mp_info_phys: cpu_ptr as usize - hhdm,
            };
            n += 1;
        }
        LIMINE_AP_COUNT = n;
    }

    install_recursive_mapping(hhdm)?;
    Ok(&*addr_of!(BOOT_INFO))
}

/// Lit les en-têtes de sections ELF64 et garde les sections allouées.
unsafe fn collect_sections(info: &mut LimineInfo, elf: &[u8]) -> Result<(), &'static str> {
    const SHF_ALLOC: u64 = 0x2;
    let rd16 = |o: usize| elf.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize);
    let rd32 = |o: usize| elf.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize);
    let rd64 = |o: usize| elf.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()));
    if elf.get(0..4) != Some(b"\x7fELF") || elf.get(4) != Some(&2) {
        return Err("le noyau n'est pas un ELF64");
    }
    let bad = "en-tetes de sections ELF illisibles";
    let shoff = rd64(0x28).ok_or(bad)? as usize;
    let shentsize = rd16(0x3A).ok_or(bad)?;
    let shnum = rd16(0x3C).ok_or(bad)?;
    let shstrndx = rd16(0x3E).ok_or(bad)?;
    let strtab_off = rd64(shoff + shstrndx * shentsize + 24).ok_or(bad)? as usize;
    for i in 0..shnum {
        let sh = shoff + i * shentsize;
        let flags = rd64(sh + 8).ok_or(bad)?;
        let size = rd64(sh + 32).ok_or(bad)? as usize;
        if flags & SHF_ALLOC == 0 || size == 0 {
            continue;
        }
        let addr = rd64(sh + 16).ok_or(bad)? as usize;
        let name_off = strtab_off + rd32(sh).ok_or(bad)?;
        let name = elf.get(name_off..).ok_or(bad)?;
        let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
        info.push_section(name, addr, size, flags)?;
    }
    Ok(())
}

/// Le gestionnaire de mémoire parcourt la table de pages active par projection récursive
/// (entrée 510 de la P4 pointant sur elle-même). Limine ne la pose pas : on l'ajoute à sa P4,
/// via le HHDM, le temps que `paging::init` construise la nôtre.
unsafe fn install_recursive_mapping(hhdm: usize) -> Result<(), &'static str> {
    const RECURSIVE_INDEX: usize = 510;
    let cr3: usize;
    core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
    let p4_phys = cr3 & 0x000F_FFFF_FFFF_F000;
    let p4 = (p4_phys + hhdm) as *mut u64;
    let slot = p4.add(RECURSIVE_INDEX);
    if read_volatile(slot) & 1 != 0 {
        return Err("l'entree 510 de la P4 de Limine est deja utilisee");
    }
    core::ptr::write_volatile(slot, (p4_phys as u64) | 0b11); // présente, écriture
    core::arch::asm!("mov cr3, {}", in(reg) cr3, options(nostack));
    Ok(())
}
