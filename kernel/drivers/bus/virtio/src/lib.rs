//! Transport VirtIO 1.x de MaiOS (spécification OASIS VirtIO 1.3).
//!
//! * [`pci::PciTransport`] : transport PCI **moderne** (§ 4.1) — capacités constructeur
//!   (configuration commune, notification et son multiplicateur, ISR, configuration de
//!   l'appareil, régions de mémoire partagée), négociation des fonctions sur 64 bits, mise en
//!   route dans l'ordre de § 3.1.1, MSI-X (un vecteur pour la configuration, un par file).
//! * [`queue::SplitQueue`] : virtqueue séparée (§ 2.7) — descripteurs libres en liste
//!   chaînée, chaînes de tampons lisibles puis inscriptibles, suppression des notifications et
//!   des interruptions par *event index* (`VIRTIO_F_EVENT_IDX`, § 2.7.7/2.7.10) : le pilote
//!   ne sonne (une sortie de VM) que si l'appareil l'a demandé, et l'appareil n'interrompt
//!   que si le pilote l'a demandé.
//! * [`Dma`] : frames contiguës en mémoire cachée, vues par la physmap (le DMA de x86 est
//!   cohérent ; sans `VIRTIO_F_ACCESS_PLATFORM`, l'adresse donnée à l'appareil est
//!   l'adresse physique).
//!
//! Les pilotes (`virtio_gpu`, …) gardent leurs files sous un `IrqSafeMutex` : la même file
//! est vidée par l'interruption et par le sondage.

#![no_std]

extern crate alloc;

pub mod pci;
pub mod queue;

use kernel_config::memory::PAGE_SIZE;
use memory::paging::kernel_half::phys_to_virt;

/// Constructeur PCI de tous les appareils virtio.
pub const VIRTIO_VENDOR: u16 = 0x1AF4;

/// Identifiant PCI moderne d'un appareil virtio de type `device_type` (§ 4.1.2.1).
pub const fn modern_device_id(device_type: u16) -> u16 {
    0x1040 + device_type
}

/// Bits de fonctions communs à tous les appareils (§ 6).
pub mod feature {
    pub const RING_INDIRECT_DESC: u64 = 1 << 28;
    pub const RING_EVENT_IDX: u64 = 1 << 29;
    pub const VERSION_1: u64 = 1 << 32;
    pub const ACCESS_PLATFORM: u64 = 1 << 33;
    pub const RING_PACKED: u64 = 1 << 34;
    pub const IN_ORDER: u64 = 1 << 35;
    pub const ORDER_PLATFORM: u64 = 1 << 36;
    pub const NOTIFICATION_DATA: u64 = 1 << 38;
    pub const RING_RESET: u64 = 1 << 40;
}

/// Bits de `device_status` (§ 2.1).
pub mod status {
    pub const ACKNOWLEDGE: u8 = 1;
    pub const DRIVER: u8 = 2;
    pub const DRIVER_OK: u8 = 4;
    pub const FEATURES_OK: u8 = 8;
    pub const DEVICE_NEEDS_RESET: u8 = 64;
    pub const FAILED: u8 = 128;
}

/// Vecteur MSI-X « aucun » (§ 4.1.5.1.2).
pub const NO_VECTOR: u16 = 0xFFFF;

/// Frames physiquement contiguës, remises à zéro, vues par la physmap. Rendues à la
/// destruction : l'appareil ne doit plus y accéder (file désactivée, ressource détachée ou
/// appareil remis à zéro).
pub struct Dma {
    pfn: usize,
    frames: usize,
}

impl Dma {
    /// Au moins `bytes` octets (une frame au minimum).
    pub fn new(bytes: usize) -> Result<Dma, &'static str> {
        let frames = bytes.div_ceil(PAGE_SIZE).max(1);
        let pfn = frame_allocator::alloc_raw_frames(frames).ok_or("virtio : mémoire DMA insuffisante")?;
        let d = Dma { pfn, frames };
        // SAFETY : frames neuves, à nous, projetées par la physmap.
        unsafe { core::ptr::write_bytes(d.va() as *mut u8, 0, frames * PAGE_SIZE) };
        Ok(d)
    }

    /// Adresse donnée à l'appareil.
    #[inline]
    pub fn phys(&self) -> u64 {
        (self.pfn * PAGE_SIZE) as u64
    }

    /// Adresse virtuelle (physmap).
    #[inline]
    pub fn va(&self) -> usize {
        phys_to_virt(self.pfn * PAGE_SIZE)
    }

    /// Taille en octets.
    #[inline]
    pub fn len(&self) -> usize {
        self.frames * PAGE_SIZE
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Écrit `bytes` à partir de `offset`.
    pub fn write(&self, offset: usize, bytes: &[u8]) {
        assert!(offset + bytes.len() <= self.len(), "Dma::write hors bornes");
        // SAFETY : dans nos frames (vérifié ci-dessus).
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), (self.va() + offset) as *mut u8, bytes.len()) }
    }

    /// Lit `out.len()` octets à partir de `offset` (lecture volatile : écrits par l'appareil).
    pub fn read(&self, offset: usize, out: &mut [u8]) {
        assert!(offset + out.len() <= self.len(), "Dma::read hors bornes");
        let base = (self.va() + offset) as *const u8;
        for (i, b) in out.iter_mut().enumerate() {
            // SAFETY : dans nos frames (vérifié ci-dessus).
            *b = unsafe { base.add(i).read_volatile() };
        }
    }
}

impl Drop for Dma {
    fn drop(&mut self) {
        // SAFETY : frames à nous, que l'appareil n'utilise plus (contrat de `Dma`).
        unsafe { frame_allocator::free_raw_frames(self.pfn, self.frames) }
    }
}
