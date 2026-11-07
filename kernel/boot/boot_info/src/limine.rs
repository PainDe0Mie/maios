//! Informations de démarrage du protocole Limine natif.
//!
//! Limine décrit la machine par des réponses éparpillées dans sa mémoire « récupérable », lues
//! via son HHDM (projection de toute la mémoire physique dans la moitié haute). Ce HHDM disparaît
//! dès que le noyau passe sur ses propres tables de pages : le point d'entrée du nano_core copie
//! donc tout ce qui sert au démarrage dans une [`LimineInfo`], placée dans la section dédiée
//! `.boot_info` de l'image du noyau. `paging::init` la projette comme les informations Multiboot2,
//! puis la rend une fois les modules relevés.

use crate::{ElfSectionFlags, FramebufferFormat, FramebufferInfo, ReservedMemoryRegion};
use core::{iter, slice, str};
use memory_structs::{PhysicalAddress, VirtualAddress};

/// Régions de mémoire gardées (après fusion des régions voisines de même nature).
pub const MAX_MEMORY_REGIONS: usize = 64;
/// Sections allouées de l'image du noyau.
pub const MAX_ELF_SECTIONS: usize = 32;
/// Modules chargés par Limine (en pratique une seule archive).
pub const MAX_MODULES: usize = 8;

const SECTION_NAME_LEN: usize = 32;
const MODULE_NAME_LEN: usize = 64;

/// Type de mémoire « utilisable » de Limine (les autres sont gardés réservés).
pub const MEMMAP_USABLE: u64 = 0;

/// L'allocateur de frames d'avant le tas tient 32 régions libres et 32 réservées, dont deux
/// réservées posées par `memory::init` (premier Mio, VGA).
const MAX_USABLE: usize = 32;
const MAX_RESERVED: usize = 30;
/// Le premier Mio reste à part (réservé par `memory::init`).
const LOW_MEMORY_END: usize = 0x10_0000;

/// Une région de mémoire physique.
#[derive(Clone, Copy, Debug)]
pub struct Region {
    pub start: usize,
    pub len: usize,
    pub usable: bool,
}

/// Une section allouée de l'image ELF du noyau.
#[derive(Clone, Copy)]
pub struct Section {
    name: [u8; SECTION_NAME_LEN],
    name_len: u8,
    pub start: usize,
    pub len: usize,
    pub flags: u64,
}

/// Un module chargé par Limine.
#[derive(Clone, Copy)]
pub struct Module {
    name: [u8; MODULE_NAME_LEN],
    name_len: u8,
    pub phys: usize,
    pub len: usize,
}

/// Le framebuffer graphique choisi par Limine (GOP en UEFI, VBE en BIOS).
#[derive(Clone, Copy, Debug)]
pub struct Framebuffer {
    pub virt: usize,
    pub phys: usize,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u16,
    pub red_shift: u8,
    pub red_size: u8,
    pub green_shift: u8,
    pub green_size: u8,
    pub blue_shift: u8,
    pub blue_size: u8,
}

/// Copie de tout ce que Limine a fourni et dont le démarrage a besoin.
///
/// Alignée sur une page : elle occupe seule ses pages, que `paging::init` reprojette et que
/// `memory_initialization` rend ensuite.
#[repr(C, align(4096))]
pub struct LimineInfo {
    regions: [Region; MAX_MEMORY_REGIONS],
    region_count: usize,
    sections: [Section; MAX_ELF_SECTIONS],
    section_count: usize,
    modules: [Module; MAX_MODULES],
    module_count: usize,
    pub rsdp: Option<usize>,
    pub framebuffer: Option<Framebuffer>,
    pub hhdm_offset: usize,
    pub kernel_phys: usize,
    pub kernel_virt: usize,
    /// Nombre de régions que la fusion a évitées (diagnostic).
    pub merged_regions: usize,
    /// Régions perdues faute de place (diagnostic ; normalement 0).
    pub dropped_regions: usize,
    usable_count: usize,
    reserved_count: usize,
}

const EMPTY_REGION: Region = Region { start: 0, len: 0, usable: false };
const EMPTY_SECTION: Section = Section { name: [0; SECTION_NAME_LEN], name_len: 0, start: 0, len: 0, flags: 0 };
const EMPTY_MODULE: Module = Module { name: [0; MODULE_NAME_LEN], name_len: 0, phys: 0, len: 0 };

fn copy_name<const N: usize>(src: &[u8]) -> ([u8; N], u8) {
    let mut name = [0u8; N];
    let len = src.len().min(N).min(u8::MAX as usize);
    name[..len].copy_from_slice(&src[..len]);
    (name, len as u8)
}

impl LimineInfo {
    pub const fn new() -> Self {
        Self {
            regions: [EMPTY_REGION; MAX_MEMORY_REGIONS],
            region_count: 0,
            sections: [EMPTY_SECTION; MAX_ELF_SECTIONS],
            section_count: 0,
            modules: [EMPTY_MODULE; MAX_MODULES],
            module_count: 0,
            rsdp: None,
            framebuffer: None,
            hhdm_offset: 0,
            kernel_phys: 0,
            kernel_virt: 0,
            merged_regions: 0,
            dropped_regions: 0,
            usable_count: 0,
            reserved_count: 0,
        }
    }

    /// Ajoute une région en la fusionnant avec la précédente si elle la prolonge et qu'elles sont
    /// de même nature : les tableaux de `memory::init` sont bornés, et Limine découpe beaucoup sa
    /// carte (mémoire récupérable, image du noyau, framebuffer…).
    pub fn push_region(&mut self, mut start: usize, mut len: usize, usable: bool) -> Result<(), &'static str> {
        if usable && start < LOW_MEMORY_END {
            let cut = (LOW_MEMORY_END - start).min(len);
            start += cut;
            len -= cut;
        }
        if len == 0 {
            return Ok(());
        }
        if let Some(last) = self.regions[..self.region_count].last_mut() {
            if last.usable == usable && last.start + last.len == start {
                last.len += len;
                self.merged_regions += 1;
                return Ok(());
            }
        }
        let count = if usable { &mut self.usable_count } else { &mut self.reserved_count };
        let max = if usable { MAX_USABLE } else { MAX_RESERVED };
        if *count == max || self.region_count == MAX_MEMORY_REGIONS {
            self.dropped_regions += 1;
            return Ok(());
        }
        *count += 1;
        self.regions[self.region_count] = Region { start, len, usable };
        self.region_count += 1;
        Ok(())
    }

    pub fn push_section(&mut self, name: &[u8], start: usize, len: usize, flags: u64) -> Result<(), &'static str> {
        let slot = self.sections.get_mut(self.section_count).ok_or("limine : trop de sections ELF")?;
        let (name, name_len) = copy_name(name);
        *slot = Section { name, name_len, start, len, flags };
        self.section_count += 1;
        Ok(())
    }

    pub fn push_module(&mut self, name: &[u8], phys: usize, len: usize) -> Result<(), &'static str> {
        let slot = self.modules.get_mut(self.module_count).ok_or("limine : trop de modules")?;
        let (name, name_len) = copy_name(name);
        *slot = Module { name, name_len, phys, len };
        self.module_count += 1;
        Ok(())
    }

    pub fn regions(&self) -> &[Region] {
        &self.regions[..self.region_count]
    }

    pub fn sections(&self) -> &[Section] {
        &self.sections[..self.section_count]
    }

    pub fn modules_slice(&self) -> &[Module] {
        &self.modules[..self.module_count]
    }
}

impl Default for LimineInfo {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::MemoryRegion for &Region {
    fn start(&self) -> PhysicalAddress {
        PhysicalAddress::new_canonical(self.start)
    }

    fn len(&self) -> usize {
        self.len
    }

    fn is_usable(&self) -> bool {
        self.usable
    }
}

impl crate::ElfSection for &Section {
    fn name(&self) -> &str {
        str::from_utf8(&self.name[..self.name_len as usize]).unwrap_or("")
    }

    fn start(&self) -> VirtualAddress {
        VirtualAddress::new_canonical(self.start)
    }

    fn len(&self) -> usize {
        self.len
    }

    fn flags(&self) -> ElfSectionFlags {
        // Mêmes bits que les `sh_flags` ELF : SHF_WRITE, SHF_ALLOC, SHF_EXECINSTR.
        ElfSectionFlags::from_bits_truncate(self.flags)
    }
}

impl crate::Module for &Module {
    fn name(&self) -> Result<&str, &'static str> {
        str::from_utf8(&self.name[..self.name_len as usize]).map_err(|_| "nom de module limine invalide")
    }

    fn start(&self) -> PhysicalAddress {
        PhysicalAddress::new_canonical(self.phys)
    }

    fn len(&self) -> usize {
        self.len
    }
}

impl crate::BootInformation for &'static LimineInfo {
    type MemoryRegion<'a> = &'a Region;
    type MemoryRegions<'a> = slice::Iter<'a, Region>;

    type ElfSection<'a> = &'a Section;
    type ElfSections<'a> = slice::Iter<'a, Section>;

    type Module<'a> = &'a Module;
    type Modules<'a> = slice::Iter<'a, Module>;

    type AdditionalReservedMemoryRegions = iter::Empty<ReservedMemoryRegion>;

    fn start(&self) -> Option<VirtualAddress> {
        VirtualAddress::new(*self as *const LimineInfo as usize)
    }

    fn len(&self) -> usize {
        core::mem::size_of::<LimineInfo>()
    }

    fn memory_regions(&self) -> Result<Self::MemoryRegions<'_>, &'static str> {
        Ok(self.regions().iter())
    }

    fn elf_sections(&self) -> Result<Self::ElfSections<'_>, &'static str> {
        Ok(self.sections().iter())
    }

    fn modules(&self) -> Self::Modules<'_> {
        self.modules_slice().iter()
    }

    fn additional_reserved_memory_regions(&self) -> Result<Self::AdditionalReservedMemoryRegions, &'static str> {
        // Tout ce qui n'est pas « utilisable » dans la carte de Limine est déjà réservé : image du
        // noyau et modules, mémoire du chargeur (tables de pages et piles des cœurs parqués),
        // framebuffer, ACPI.
        Ok(iter::empty())
    }

    fn kernel_end(&self) -> Result<VirtualAddress, &'static str> {
        use crate::ElfSection;
        self.sections()
            .iter()
            .filter(|s| s.flags().contains(ElfSectionFlags::ALLOCATED) && s.len != 0)
            .map(|s| s.start + s.len)
            .max()
            .and_then(VirtualAddress::new)
            .ok_or("limine : fin du noyau introuvable")
    }

    fn rsdp(&self) -> Option<PhysicalAddress> {
        self.rsdp.and_then(PhysicalAddress::new)
    }

    fn stack_size(&self) -> Result<usize, &'static str> {
        use crate::ElfSection;
        self.sections()
            .iter()
            .find(|s| s.name() == ".stack")
            .map(|s| s.len)
            .ok_or("limine : pas de section .stack")
    }

    fn framebuffer_info(&self) -> Option<FramebufferInfo> {
        let fb = self.framebuffer?;
        let bytes_per_pixel = (fb.bpp as u32 + 7) / 8;
        if bytes_per_pixel == 0 {
            return None;
        }
        let format = match (fb.red_shift, fb.green_shift, fb.blue_shift, fb.red_size, fb.green_size, fb.blue_size) {
            (16, 8, 0, 8, 8, 8) => FramebufferFormat::RgbPixel,
            (0, 8, 16, 8, 8, 8) => FramebufferFormat::BgrPixel,
            _ => FramebufferFormat::CustomPixel {
                red_bit_position: fb.red_shift,
                red_size_in_bits: fb.red_size,
                green_bit_position: fb.green_shift,
                green_size_in_bits: fb.green_size,
                blue_bit_position: fb.blue_shift,
                blue_size_in_bits: fb.blue_size,
            },
        };
        Some(FramebufferInfo {
            virt_addr: Some(VirtualAddress::new_canonical(fb.virt)),
            phys_addr: PhysicalAddress::new_canonical(fb.phys),
            total_size_in_bytes: fb.pitch as u64 * fb.height as u64,
            width: fb.width,
            height: fb.height,
            bits_per_pixel: fb.bpp as u8,
            stride: fb.pitch / bytes_per_pixel,
            format,
        })
    }
}
