//! Transport PCI moderne (VirtIO 1.3, § 4.1).
//!
//! Les régions des capacités (configuration commune, notification, ISR, configuration de
//! l'appareil) et la table MSI-X sont projetées **une fois par BAR**, sur l'union de ce dont on
//! a besoin : QEMU les range souvent dans la même BAR, et deux projections des mêmes frames
//! MMIO seraient refusées par l'allocateur.

use alloc::vec::Vec;

use interrupts::{MsiCallback, MsiVector};
use kernel_config::memory::PAGE_SIZE;
use log::warn;
use memory::{MappedPages, PhysicalAddress, MMIO_FLAGS};
use pci::PciDevice;

use crate::queue::SplitQueue;
use crate::{feature, status, NO_VECTOR};

// Types de capacités (§ 4.1.4).
const CAP_COMMON: u8 = 1;
const CAP_NOTIFY: u8 = 2;
const CAP_ISR: u8 = 3;
const CAP_DEVICE: u8 = 4;
const CAP_SHM: u8 = 8;

// Configuration commune (§ 4.1.4.3).
const DEVICE_FEATURE_SELECT: usize = 0x00;
const DEVICE_FEATURE: usize = 0x04;
const DRIVER_FEATURE_SELECT: usize = 0x08;
const DRIVER_FEATURE: usize = 0x0C;
const CONFIG_MSIX_VECTOR: usize = 0x10;
const NUM_QUEUES: usize = 0x12;
const DEVICE_STATUS: usize = 0x14;
const CONFIG_GENERATION: usize = 0x15;
const QUEUE_SELECT: usize = 0x16;
const QUEUE_SIZE: usize = 0x18;
const QUEUE_MSIX_VECTOR: usize = 0x1A;
const QUEUE_ENABLE: usize = 0x1C;
const QUEUE_NOTIFY_OFF: usize = 0x1E;
const QUEUE_DESC: usize = 0x20;
const QUEUE_DRIVER: usize = 0x28;
const QUEUE_DEVICE: usize = 0x30;

/// Région de mémoire partagée de l'appareil (capacité 8 : `hostmem` de virtio-gpu, DAX…),
/// non projetée ici.
#[derive(Clone, Copy, Debug)]
pub struct ShmRegion {
    pub id: u8,
    pub phys: u64,
    pub len: u64,
}

/// Table MSI-X projetée et vecteurs réservés.
struct Msix {
    table: usize,
    size: usize,
    vectors: Vec<MsiVector>,
}

pub struct PciTransport {
    common: usize,
    notify: usize,
    notify_len: usize,
    notify_mul: u32,
    isr: usize,
    device: usize,
    device_len: usize,
    _maps: Vec<MappedPages>,
    msix: Option<Msix>,
    msix_info: Option<pci::MsixInfo>,
    msix_base: usize,
    pub shm: Vec<ShmRegion>,
}

// SAFETY : adresses MMIO projetées pour la vie du transport ; l'appareil sérialise ses
// registres, et le pilote n'écrit la configuration commune que pendant la mise en route.
unsafe impl Send for PciTransport {}
unsafe impl Sync for PciTransport {}

impl PciTransport {
    /// Trouve et projette les capacités de `dev`. Active le bus maître et l'espace mémoire.
    pub fn new(dev: &PciDevice) -> Result<PciTransport, &'static str> {
        let caps = dev.get_virtio_caps();
        let first = |t: u8| caps.iter().find(|c| c.cfg_type == t && c.bar < 6).copied();
        let common = first(CAP_COMMON).ok_or("virtio : pas de configuration commune (appareil ancien ?)")?;
        let notify = first(CAP_NOTIFY).ok_or("virtio : pas de capacité de notification")?;
        let isr = first(CAP_ISR).ok_or("virtio : pas de capacité ISR")?;
        let device = first(CAP_DEVICE);

        dev.pci_set_command_bus_master_bit();
        dev.pci_enable_mem_space();

        // Régions voulues : (BAR, début, fin) en octets dans la BAR.
        let mut want: Vec<(u8, usize, usize)> = Vec::new();
        for c in [Some(common), Some(notify), Some(isr), device].into_iter().flatten() {
            want.push((c.bar, c.bar_offset as usize, c.bar_offset as usize + c.length.max(1) as usize));
        }
        let msix_info = dev.msix_info();
        if let Some(m) = msix_info {
            want.push((m.table_bar as u8, m.table_offset, m.table_offset + 16 * m.table_size));
        }

        // Une projection par BAR, sur l'union des régions voulues.
        let mut maps: Vec<MappedPages> = Vec::new();
        let mut bases: [Option<(usize, usize)>; 6] = [None; 6]; // (début projeté dans la BAR, va)
        for bar in 0..6u8 {
            let regions: Vec<&(u8, usize, usize)> = want.iter().filter(|r| r.0 == bar).collect();
            if regions.is_empty() {
                continue;
            }
            let lo = regions.iter().map(|r| r.1).min().unwrap_or(0) & !(PAGE_SIZE - 1);
            let hi = regions.iter().map(|r| r.2).max().unwrap_or(0);
            let base = dev.determine_mem_base(bar as usize)?.value();
            if base == 0 {
                return Err("virtio : BAR non assignée");
            }
            let pa = PhysicalAddress::new(base + lo).ok_or("virtio : adresse de BAR invalide")?;
            let mp = memory::map_frame_range(pa, hi - lo, MMIO_FLAGS)?;
            bases[bar as usize] = Some((lo, mp.start_address().value()));
            maps.push(mp);
        }
        let va = |bar: u8, off: usize| -> usize {
            let (lo, v) = bases[bar as usize].expect("BAR projetée");
            v + off - lo
        };

        let mut shm = Vec::new();
        for c in caps.iter().filter(|c| c.cfg_type == CAP_SHM && c.bar < 6) {
            let base = dev.determine_mem_base(c.bar as usize)?.value() as u64;
            shm.push(ShmRegion {
                id: c.id,
                phys: base + (c.bar_offset as u64 | (c.extra as u64) << 32),
                len: c.length as u64 | (c.length_hi as u64) << 32,
            });
        }

        let msix_base = match msix_info {
            Some(m) => va(m.table_bar as u8, m.table_offset),
            None => 0,
        };
        Ok(PciTransport {
            common: va(common.bar, common.bar_offset as usize),
            notify: va(notify.bar, notify.bar_offset as usize),
            notify_len: notify.length as usize,
            notify_mul: notify.extra,
            isr: va(isr.bar, isr.bar_offset as usize),
            device: device.map_or(0, |d| va(d.bar, d.bar_offset as usize)),
            device_len: device.map_or(0, |d| d.length as usize),
            _maps: maps,
            msix: None,
            msix_info,
            msix_base,
            shm,
        })
    }

    // ── Accès aux registres ──────────────────────────────────────────────────────────

    #[inline]
    fn r8(&self, off: usize) -> u8 {
        // SAFETY : registre de la configuration commune projetée.
        unsafe { ((self.common + off) as *const u8).read_volatile() }
    }
    #[inline]
    fn w8(&self, off: usize, v: u8) {
        // SAFETY : comme `r8`.
        unsafe { ((self.common + off) as *mut u8).write_volatile(v) }
    }
    #[inline]
    fn r16(&self, off: usize) -> u16 {
        // SAFETY : comme `r8`.
        unsafe { ((self.common + off) as *const u16).read_volatile() }
    }
    #[inline]
    fn w16(&self, off: usize, v: u16) {
        // SAFETY : comme `r8`.
        unsafe { ((self.common + off) as *mut u16).write_volatile(v) }
    }
    #[inline]
    fn r32(&self, off: usize) -> u32 {
        // SAFETY : comme `r8`.
        unsafe { ((self.common + off) as *const u32).read_volatile() }
    }
    #[inline]
    fn w32(&self, off: usize, v: u32) {
        // SAFETY : comme `r8`.
        unsafe { ((self.common + off) as *mut u32).write_volatile(v) }
    }
    /// Les champs 64 bits s'écrivent en deux moitiés, la basse d'abord (§ 4.1.3.1).
    #[inline]
    fn w64(&self, off: usize, v: u64) {
        self.w32(off, v as u32);
        self.w32(off + 4, (v >> 32) as u32);
    }

    pub fn status(&self) -> u8 {
        self.r8(DEVICE_STATUS)
    }

    fn set_status(&self, s: u8) {
        self.w8(DEVICE_STATUS, s);
    }

    /// Remise à zéro : écrire 0, puis attendre que l'appareil relise 0 (§ 4.1.4.3.2).
    pub fn reset(&self) -> Result<(), &'static str> {
        self.set_status(0);
        for _ in 0..1_000_000 {
            if self.status() == 0 {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err("virtio : l'appareil ne se remet pas à zéro")
    }

    /// Fonctions offertes par l'appareil (64 bits).
    pub fn device_features(&self) -> u64 {
        self.w32(DEVICE_FEATURE_SELECT, 0);
        let lo = self.r32(DEVICE_FEATURE) as u64;
        self.w32(DEVICE_FEATURE_SELECT, 1);
        let hi = self.r32(DEVICE_FEATURE) as u64;
        lo | hi << 32
    }

    /// Étapes 1 à 6 de § 3.1.1 : remise à zéro, ACKNOWLEDGE, DRIVER, négociation de
    /// `wanted ∩ offertes` (VERSION_1 obligatoire), FEATURES_OK relu. Rend
    /// `(offertes, retenues)` ; les files se créent ensuite, puis [`Self::driver_ok`].
    pub fn negotiate(&self, wanted: u64) -> Result<(u64, u64), &'static str> {
        self.reset()?;
        self.set_status(status::ACKNOWLEDGE);
        self.set_status(status::ACKNOWLEDGE | status::DRIVER);
        let offered = self.device_features();
        if offered & feature::VERSION_1 == 0 {
            self.fail();
            return Err("virtio : appareil sans VIRTIO_F_VERSION_1");
        }
        let accepted = offered & (wanted | feature::VERSION_1);
        self.w32(DRIVER_FEATURE_SELECT, 0);
        self.w32(DRIVER_FEATURE, accepted as u32);
        self.w32(DRIVER_FEATURE_SELECT, 1);
        self.w32(DRIVER_FEATURE, (accepted >> 32) as u32);
        self.set_status(status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK);
        if self.status() & status::FEATURES_OK == 0 {
            self.fail();
            return Err("virtio : fonctions refusées par l'appareil (FEATURES_OK)");
        }
        Ok((offered, accepted))
    }

    /// Étape 8 : l'appareil peut travailler.
    pub fn driver_ok(&self) {
        self.set_status(self.status() | status::DRIVER_OK);
    }

    /// Le pilote abandonne l'appareil.
    pub fn fail(&self) {
        self.set_status(self.status() | status::FAILED);
    }

    pub fn num_queues(&self) -> u16 {
        self.r16(NUM_QUEUES)
    }

    /// Crée la file `index` avec au plus `max_size` entrées, liée au vecteur MSI-X `vector`
    /// ([`NO_VECTOR`] : sans interruption). Refuse si l'appareil ne retient pas le vecteur.
    pub fn setup_queue(&self, index: u16, max_size: u16, event_idx: bool, vector: u16) -> Result<SplitQueue, &'static str> {
        self.w16(QUEUE_SELECT, index);
        if self.r16(QUEUE_ENABLE) != 0 {
            return Err("virtio : file déjà active");
        }
        let dev_max = self.r16(QUEUE_SIZE);
        if dev_max == 0 {
            return Err("virtio : file absente");
        }
        // La plus grande puissance de deux ≤ min(appareil, demandé).
        let lim = dev_max.min(max_size).max(1);
        let size = 1u16 << (15 - lim.leading_zeros().min(15));
        let off = self.r16(QUEUE_NOTIFY_OFF) as usize;
        let at = off * self.notify_mul as usize;
        if at + 2 > self.notify_len.max(2) && self.notify_mul != 0 {
            return Err("virtio : registre de notification hors de sa région");
        }
        let q = SplitQueue::new(index, size, event_idx, self.notify + at)?;
        self.w16(QUEUE_SIZE, size);
        self.w64(QUEUE_DESC, q.desc_phys);
        self.w64(QUEUE_DRIVER, q.avail_phys);
        self.w64(QUEUE_DEVICE, q.used_phys);
        self.w16(QUEUE_MSIX_VECTOR, vector);
        if self.r16(QUEUE_MSIX_VECTOR) != vector {
            return Err("virtio : vecteur MSI-X refusé pour la file");
        }
        self.w16(QUEUE_ENABLE, 1);
        Ok(q)
    }

    /// Vecteur MSI-X des changements de configuration ; faux si l'appareil le refuse.
    pub fn set_config_vector(&self, vector: u16) -> bool {
        self.w16(CONFIG_MSIX_VECTOR, vector);
        self.r16(CONFIG_MSIX_VECTOR) == vector
    }

    /// Réserve `handlers.len()` vecteurs (entrée MSI-X `i` → `handlers[i]`), dirigés vers le
    /// cœur d'amorçage, et active MSI-X avec la fonction masquée : l'appelant lie les vecteurs
    /// (configuration, files) puis appelle [`Self::msix_unmask`]. Rend les numéros d'entrée.
    pub fn enable_msix(&mut self, dev: &PciDevice, handlers: &[(MsiCallback, usize)]) -> Result<Vec<u16>, &'static str> {
        let info = self.msix_info.ok_or("virtio : pas de MSI-X")?;
        if info.table_size < handlers.len() {
            return Err("virtio : table MSI-X trop petite");
        }
        dev.pci_enable_intx(false);
        dev.pci_msix_control(true, true)?;
        for i in 0..info.table_size {
            // SAFETY : entrée `i` de la table projetée.
            unsafe { pci::msix_mask_entry((self.msix_base + 16 * i) as *mut u32, true) };
        }
        let apic = cpu::bootstrap_cpu().map(|c| c.value()).unwrap_or(0);
        let mut vectors = Vec::new();
        let mut entries = Vec::new();
        for (i, (f, data)) in handlers.iter().enumerate() {
            match interrupts::register_msi_callback(*f, *data) {
                Ok(v) => {
                    // SAFETY : entrée `i` de la table projetée.
                    unsafe { pci::msix_write_entry((self.msix_base + 16 * i) as *mut u32, apic, v.vector) };
                    vectors.push(v);
                    entries.push(i as u16);
                }
                Err(e) => {
                    warn!("virtio : vecteur MSI-X {} non réservé : {}", i, e);
                    for v in vectors.drain(..) {
                        let _ = interrupts::unregister_msi_callback(v);
                    }
                    let _ = dev.pci_msix_control(false, false);
                    return Err(e);
                }
            }
        }
        self.msix = Some(Msix { table: self.msix_base, size: info.table_size, vectors });
        Ok(entries)
    }

    /// Lève le masque de fonction MSI-X (après la liaison des vecteurs).
    pub fn msix_unmask(&self, dev: &PciDevice) -> Result<(), &'static str> {
        dev.pci_msix_control(true, false)
    }

    /// Lit l'ISR (remis à zéro par la lecture) : bit 0 file, bit 1 configuration. Sert au
    /// sondage sans MSI-X.
    pub fn read_isr(&self) -> u8 {
        // SAFETY : registre ISR projeté.
        unsafe { (self.isr as *const u8).read_volatile() }
    }

    /// Lit un mot de 32 bits de la configuration de l'appareil, de façon cohérente
    /// (relecture tant que `config_generation` change, § 4.1.4.3.1).
    pub fn device_read32(&self, off: usize) -> u32 {
        assert!(self.device != 0 && off + 4 <= self.device_len, "virtio : hors de la configuration de l'appareil");
        loop {
            let g = self.r8(CONFIG_GENERATION);
            // SAFETY : dans la configuration de l'appareil projetée (vérifié ci-dessus).
            let v = unsafe { ((self.device + off) as *const u32).read_volatile() };
            if self.r8(CONFIG_GENERATION) == g {
                return v;
            }
        }
    }

    /// Écrit un mot de 32 bits de la configuration de l'appareil.
    pub fn device_write32(&self, off: usize, v: u32) {
        assert!(self.device != 0 && off + 4 <= self.device_len, "virtio : hors de la configuration de l'appareil");
        // SAFETY : comme `device_read32`.
        unsafe { ((self.device + off) as *mut u32).write_volatile(v) }
    }

    /// Lit un octet de la configuration de l'appareil (champ `u8` : largeur naturelle,
    /// § 4.1.3.1), de façon cohérente comme [`Self::device_read32`].
    pub fn device_read8(&self, off: usize) -> u8 {
        assert!(self.device != 0 && off < self.device_len, "virtio : hors de la configuration de l'appareil");
        loop {
            let g = self.r8(CONFIG_GENERATION);
            // SAFETY : dans la configuration de l'appareil projetée (vérifié ci-dessus).
            let v = unsafe { ((self.device + off) as *const u8).read_volatile() };
            if self.r8(CONFIG_GENERATION) == g {
                return v;
            }
        }
    }

    /// Écrit un octet de la configuration de l'appareil.
    pub fn device_write8(&self, off: usize, v: u8) {
        assert!(self.device != 0 && off < self.device_len, "virtio : hors de la configuration de l'appareil");
        // SAFETY : comme `device_read8`.
        unsafe { ((self.device + off) as *mut u8).write_volatile(v) }
    }

    /// Lit `out.len()` octets de la configuration à partir de `off`, d'une seule génération
    /// (relecture complète si l'appareil l'a changée entre-temps).
    pub fn device_read_bytes(&self, off: usize, out: &mut [u8]) {
        assert!(self.device != 0 && off + out.len() <= self.device_len, "virtio : hors de la configuration de l'appareil");
        loop {
            let g = self.r8(CONFIG_GENERATION);
            for (i, b) in out.iter_mut().enumerate() {
                // SAFETY : dans la configuration projetée (vérifié ci-dessus).
                *b = unsafe { ((self.device + off + i) as *const u8).read_volatile() };
            }
            if self.r8(CONFIG_GENERATION) == g {
                return;
            }
        }
    }

    /// Vrai si des vecteurs MSI-X sont en service.
    pub fn has_msix(&self) -> bool {
        self.msix.is_some()
    }

    /// Nombre d'entrées de la table MSI-X (0 sans MSI-X).
    pub fn msix_table_size(&self) -> usize {
        self.msix.as_ref().map_or(self.msix_info.map_or(0, |m| m.table_size), |m| m.size)
    }

    /// Numéros des vecteurs de l'IDT réservés (diagnostic).
    pub fn idt_vectors(&self) -> Vec<u8> {
        self.msix.as_ref().map_or(Vec::new(), |m| m.vectors.iter().map(|v| v.vector).collect())
    }

    /// Adresse de la table MSI-X projetée (diagnostic).
    pub fn msix_table(&self) -> usize {
        self.msix.as_ref().map_or(0, |m| m.table)
    }
}

/// [`NO_VECTOR`] réexporté pour les pilotes.
pub const NO_MSIX: u16 = NO_VECTOR;
