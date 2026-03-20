//! Driver AHCI (Advanced Host Controller Interface) pour MaiOS.
//!
//! Implémente l'accès aux disques SATA via le standard AHCI 1.3.
//! Chaque port AHCI actif avec un disque connecté est exposé comme un
//! `StorageDevice` utilisable par le reste du kernel (swap, FAT32, etc.).
//!
//! # Pipeline d'initialisation
//! ```text
//! init_from_pci(pci_device)
//!   └─ Mapper ABAR (BAR5) en MMIO
//!   └─ Activer AHCI mode (GHC.AE=1)
//!   └─ Scanner les ports implémentés (PI register)
//!   └─ Pour chaque port avec un disque SATA :
//!      └─ Stopper le port (CMD.ST=0, CMD.FRE=0)
//!      └─ Allouer Command List (1 KiB) + FIS Receive (256 B)
//!      └─ Redémarrer le port (CMD.FRE=1, CMD.ST=1)
//!      └─ IDENTIFY DEVICE (ATA command 0xEC)
//!      └─ Créer AhciDrive implémentant StorageDevice
//! ```
//!
//! # Références
//! - AHCI 1.3.1 Specification — <https://www.intel.com/content/www/us/en/io/serial-ata/ahci.html>
//! - Serial ATA Revision 3.0

#![no_std]

extern crate alloc;
#[macro_use] extern crate log;

use alloc::{sync::Arc, vec::Vec, string::String};
use spin::Mutex;
use core::sync::atomic::{fence, Ordering};

use memory::{
    allocate_pages_by_bytes, get_kernel_mmi_ref,
    PhysicalAddress, MappedPages, PteFlags,
};
use pci::PciDevice;
use storage_device::{StorageDevice, StorageDeviceRef, StorageController, StorageControllerRef};
use io::{BlockIo, BlockReader, BlockWriter, IoError, KnownLength};

// ────────────────────────────────────────────────────────────────────────────
// Constantes PCI
// ────────────────────────────────────────────────────────────────────────────

/// PCI class pour les contrôleurs de stockage de masse.
pub const AHCI_PCI_CLASS: u8 = 0x01;
/// PCI subclass pour AHCI (Serial ATA).
pub const AHCI_PCI_SUBCLASS: u8 = 0x06;

const SECTOR_SIZE: usize = 512;

// ────────────────────────────────────────────────────────────────────────────
// HBA (Host Bus Adapter) Memory Registers — offsets depuis ABAR
// ────────────────────────────────────────────────────────────────────────────

/// Host Capabilities
const HBA_CAP: usize = 0x00;
/// Global Host Control
const HBA_GHC: usize = 0x04;
/// Interrupt Status
const HBA_IS: usize = 0x08;
/// Ports Implemented
const HBA_PI: usize = 0x0C;
/// Version
const HBA_VS: usize = 0x10;

/// GHC bit: AHCI Enable
const GHC_AE: u32 = 1 << 31;
/// GHC bit: HBA Reset
const GHC_HR: u32 = 1 << 0;

// ────────────────────────────────────────────────────────────────────────────
// Port Registers — offsets depuis le début du port (ABAR + 0x100 + port*0x80)
// ────────────────────────────────────────────────────────────────────────────

const PORT_BASE: usize = 0x100;
const PORT_SIZE: usize = 0x80;

/// Port Command List Base Address (lower 32)
const PORT_CLB: usize = 0x00;
/// Port Command List Base Address (upper 32)
const PORT_CLBU: usize = 0x04;
/// Port FIS Base Address (lower 32)
const PORT_FB: usize = 0x08;
/// Port FIS Base Address (upper 32)
const PORT_FBU: usize = 0x0C;
/// Port Interrupt Status
const PORT_IS: usize = 0x10;
/// Port Interrupt Enable
#[allow(dead_code)]
const PORT_IE: usize = 0x14;
/// Port Command and Status
const PORT_CMD: usize = 0x18;
/// Port Task File Data
const PORT_TFD: usize = 0x20;
/// Port Signature
const PORT_SIG: usize = 0x24;
/// Port SATA Status (SCR0: SStatus)
const PORT_SSTS: usize = 0x28;
/// Port SATA Control (SCR2: SControl)
#[allow(dead_code)]
const PORT_SCTL: usize = 0x2C;
/// Port SATA Error (SCR1: SError)
const PORT_SERR: usize = 0x30;
/// Port Command Issue
const PORT_CI: usize = 0x38;

/// CMD bit: Start (process command list)
const CMD_ST: u32 = 1 << 0;
/// CMD bit: FIS Receive Enable
const CMD_FRE: u32 = 1 << 4;
/// CMD bit: FIS Receive Running
const CMD_FR: u32 = 1 << 14;
/// CMD bit: Command List Running
const CMD_CR: u32 = 1 << 15;

/// SATA device signature for a standard ATA disk.
const SATA_SIG_ATA: u32 = 0x00000101;
/// SATA device signature for ATAPI (CD/DVD).
#[allow(dead_code)]
const SATA_SIG_ATAPI: u32 = 0xEB140101;

// ────────────────────────────────────────────────────────────────────────────
// Command Header (dans la Command List — 32 octets chacun, 32 slots max)
// ────────────────────────────────────────────────────────────────────────────

/// Un Command Header dans la Command List (32 octets).
#[derive(Clone, Copy, Default)]
#[repr(C)]
struct CommandHeader {
    /// DW0: CFL[4:0] | A | W | P | R | B | C | reserved | PMP | PRDTL[15:0]
    dw0: u32,
    /// DW1: PRD Byte Count (mis à jour par le HBA après le transfert)
    prd_byte_count: u32,
    /// DW2: Command Table Base Address (lower 32)
    ctba: u32,
    /// DW3: Command Table Base Address (upper 32)
    ctbau: u32,
    /// DW4-7: reserved
    _reserved: [u32; 4],
}
const _: () = assert!(core::mem::size_of::<CommandHeader>() == 32);

// ────────────────────────────────────────────────────────────────────────────
// Command Table (AHCI 1.3 spec, section 4.2.3)
//
// Offset  Size   Description
// 0x00    64B    Command FIS (CFIS)
// 0x40    16B    ATAPI Command
// 0x50    48B    Reserved
// 0x80    N×16B  PRDT entries
// ────────────────────────────────────────────────────────────────────────────

/// Physical Region Descriptor Table entry (16 octets).
#[derive(Clone, Copy, Default)]
#[repr(C)]
struct PrdtEntry {
    /// Data Base Address (lower 32, must be word-aligned)
    dba: u32,
    /// Data Base Address (upper 32)
    dbau: u32,
    /// Reserved
    _reserved: u32,
    /// DW3: Byte Count [21:0] (0-based: 0 = 1 byte, max 4 MiB) | I bit [31]
    dbc_i: u32,
}
const _: () = assert!(core::mem::size_of::<PrdtEntry>() == 16);

/// Offset du PRDT dans la Command Table (AHCI spec : 0x80).
const PRDT_OFFSET: usize = 0x80;

/// Taille minimale d'une Command Table : header (0x80) + 1 PRDT entry.
const CMD_TABLE_SIZE: usize = PRDT_OFFSET + core::mem::size_of::<PrdtEntry>();

// ────────────────────────────────────────────────────────────────────────────
// FIS types (Frame Information Structure)
// ────────────────────────────────────────────────────────────────────────────

/// Register H2D FIS (Host to Device) — 20 octets utilisés.
#[derive(Clone, Copy, Default)]
#[repr(C)]
struct FisRegH2D {
    /// FIS type = 0x27
    fis_type: u8,
    /// [7]: C (command/control), [3:0]: PM Port
    pm_c: u8,
    /// ATA command register
    command: u8,
    /// Feature low
    feature_lo: u8,

    /// LBA low
    lba0: u8,
    /// LBA mid
    lba1: u8,
    /// LBA high
    lba2: u8,
    /// Device register
    device: u8,

    /// LBA3 (exp)
    lba3: u8,
    /// LBA4 (exp)
    lba4: u8,
    /// LBA5 (exp)
    lba5: u8,
    /// Feature high
    feature_hi: u8,

    /// Sector count low
    count_lo: u8,
    /// Sector count high
    count_hi: u8,
    /// Reserved
    _rsv0: u8,
    /// Control
    control: u8,

    /// Reserved
    _rsv1: [u8; 4],
}

/// ATA IDENTIFY DEVICE command.
const ATA_CMD_IDENTIFY: u8 = 0xEC;
/// ATA READ DMA EXT command (48-bit LBA).
const ATA_CMD_READ_DMA_EXT: u8 = 0x25;
/// ATA WRITE DMA EXT command (48-bit LBA).
const ATA_CMD_WRITE_DMA_EXT: u8 = 0x35;

// ────────────────────────────────────────────────────────────────────────────
// MMIO helpers
// ────────────────────────────────────────────────────────────────────────────

#[inline]
unsafe fn mmio_read32(base: usize, offset: usize) -> u32 {
    ((base + offset) as *const u32).read_volatile()
}

#[inline]
unsafe fn mmio_write32(base: usize, offset: usize, val: u32) {
    ((base + offset) as *mut u32).write_volatile(val);
}

// ────────────────────────────────────────────────────────────────────────────
// AhciPort — état interne d'un port AHCI initialisé
// ────────────────────────────────────────────────────────────────────────────

struct AhciPort {
    /// Adresse virtuelle de la base ABAR.
    abar_va: usize,
    /// Numéro de port (0-31).
    port_num: u8,
    /// Command List — 1 KiB, 32 command headers de 32 octets.
    _clb_pages: MappedPages,
    clb_va: usize,
    _clb_pa: PhysicalAddress,
    /// FIS Receive area — 256 octets minimum.
    _fb_pages: MappedPages,
    _fb_pa: PhysicalAddress,
    /// Command Table for slot 0 (on n'utilise qu'un seul slot).
    _ct_pages: MappedPages,
    ct_va: usize,
    ct_pa: PhysicalAddress,
    /// Nombre total de secteurs sur le disque.
    sector_count: u64,
    /// Taille d'un secteur en octets (typiquement 512).
    sector_size: usize,
    /// Modèle du disque (depuis IDENTIFY).
    model: String,
}

impl AhciPort {
    /// Adresse virtuelle de la base des registres du port.
    #[inline]
    fn port_base(&self) -> usize {
        self.abar_va + PORT_BASE + self.port_num as usize * PORT_SIZE
    }

    /// Stopper le moteur de commandes du port.
    #[allow(dead_code)]
    fn stop(&self) {
        let base = self.port_base();
        unsafe {
            let cmd = mmio_read32(base, PORT_CMD);
            // Clear ST
            mmio_write32(base, PORT_CMD, cmd & !CMD_ST);
            // Attendre que CR (Command List Running) passe à 0
            for _ in 0..1_000_000 {
                if mmio_read32(base, PORT_CMD) & CMD_CR == 0 { break; }
                core::hint::spin_loop();
            }
            // Clear FRE
            let cmd = mmio_read32(base, PORT_CMD);
            mmio_write32(base, PORT_CMD, cmd & !CMD_FRE);
            // Attendre que FR (FIS Receive Running) passe à 0
            for _ in 0..1_000_000 {
                if mmio_read32(base, PORT_CMD) & CMD_FR == 0 { break; }
                core::hint::spin_loop();
            }
        }
    }

    /// Démarrer le moteur de commandes du port.
    #[allow(dead_code)]
    fn start(&self) {
        let base = self.port_base();
        unsafe {
            // Attendre que CR soit à 0 avant de démarrer
            for _ in 0..1_000_000 {
                if mmio_read32(base, PORT_CMD) & CMD_CR == 0 { break; }
                core::hint::spin_loop();
            }
            let cmd = mmio_read32(base, PORT_CMD);
            mmio_write32(base, PORT_CMD, cmd | CMD_FRE | CMD_ST);
        }
    }

    /// Soumet une commande ATA sur le slot 0 et attend la complétion par polling.
    fn issue_command(&mut self, fis: &FisRegH2D, dma_pa: u64, byte_count: u32, write: bool) -> Result<(), &'static str> {
        let base = self.port_base();

        // 1. Préparer le Command Header (slot 0)
        let cmd_header = unsafe { &mut *(self.clb_va as *mut CommandHeader) };
        // CFL = 5 DWORDs (taille du FIS H2D = 20 octets = 5 DW)
        // W bit si écriture
        let cfl: u32 = 5;
        let w_bit: u32 = if write { 1 << 6 } else { 0 };
        let prdtl: u32 = if byte_count > 0 { 1 } else { 0 };
        cmd_header.dw0 = cfl | w_bit | (prdtl << 16);
        cmd_header.prd_byte_count = 0;
        cmd_header.ctba = self.ct_pa.value() as u32;
        cmd_header.ctbau = (self.ct_pa.value() >> 32) as u32;

        // 2. Préparer la Command Table
        // Zéroïser la zone CFIS (128 octets)
        unsafe {
            core::ptr::write_bytes(self.ct_va as *mut u8, 0, CMD_TABLE_SIZE);
        }

        // Copier le FIS H2D au début de la Command Table
        unsafe {
            core::ptr::copy_nonoverlapping(
                fis as *const FisRegH2D as *const u8,
                self.ct_va as *mut u8,
                core::mem::size_of::<FisRegH2D>(),
            );
        }

        // 3. Préparer le PRDT entry (si transfert de données)
        if byte_count > 0 {
            let prdt = unsafe {
                &mut *((self.ct_va + PRDT_OFFSET) as *mut PrdtEntry)
            };
            prdt.dba = dma_pa as u32;
            prdt.dbau = (dma_pa >> 32) as u32;
            prdt._reserved = 0;
            // DBC est 0-based (byte_count - 1), bit 0 doit être 1 (pair)
            prdt.dbc_i = (byte_count - 1) & 0x003F_FFFF;
        }

        fence(Ordering::Release);

        // 4. Nettoyer les erreurs et les interruptions précédentes
        unsafe {
            mmio_write32(base, PORT_SERR, 0xFFFF_FFFF); // clear all errors
            mmio_write32(base, PORT_IS, 0xFFFF_FFFF);   // clear all interrupts
        }

        // 5. Émettre la commande (slot 0)
        unsafe {
            mmio_write32(base, PORT_CI, 1);
        }

        // 6. Polling : attendre que CI bit 0 passe à 0 (commande terminée)
        for _ in 0..10_000_000u32 {
            fence(Ordering::Acquire);
            let ci = unsafe { mmio_read32(base, PORT_CI) };
            if ci & 1 == 0 {
                // Vérifier les erreurs
                let tfd = unsafe { mmio_read32(base, PORT_TFD) };
                if tfd & 0x01 != 0 {
                    // ERR bit set dans le Task File
                    let err = (tfd >> 8) & 0xFF;
                    error!("AHCI port {}: TFD error {:#x}", self.port_num, err);
                    return Err("AHCI: ATA command error (TFD.ERR)");
                }
                return Ok(());
            }
            // Vérifier si une erreur d'interface est apparue
            let is = unsafe { mmio_read32(base, PORT_IS) };
            if is & (1 << 30) != 0 {
                error!("AHCI port {}: task file error interrupt", self.port_num);
                return Err("AHCI: task file error");
            }
            core::hint::spin_loop();
        }

        error!("AHCI port {}: command timeout (CI still set)", self.port_num);
        Err("AHCI: command timeout")
    }

    /// Lit des secteurs depuis le disque via DMA.
    fn read_sectors(&mut self, buf: &mut [u8], lba: u64) -> Result<usize, &'static str> {
        let sectors = buf.len() / self.sector_size;
        if sectors == 0 { return Ok(0); }

        let kernel_mmi = get_kernel_mmi_ref().ok_or("AHCI: no kernel MMI")?;
        let pages = allocate_pages_by_bytes(buf.len())
            .ok_or("AHCI read: cannot alloc DMA buf")?;
        let flags = PteFlags::new().valid(true).writable(true).device_memory(true);
        let dma = kernel_mmi.lock().page_table.map_allocated_pages(pages, flags)?;
        let va = dma.start_address().value();
        let pa = kernel_mmi.lock().page_table.translate(dma.start_address())
            .ok_or("AHCI read: VA→PA failed")?;

        let fis = FisRegH2D {
            fis_type: 0x27,
            pm_c: 0x80, // C bit = 1 (command)
            command: ATA_CMD_READ_DMA_EXT,
            device: 1 << 6, // LBA mode
            lba0: (lba & 0xFF) as u8,
            lba1: ((lba >> 8) & 0xFF) as u8,
            lba2: ((lba >> 16) & 0xFF) as u8,
            lba3: ((lba >> 24) & 0xFF) as u8,
            lba4: ((lba >> 32) & 0xFF) as u8,
            lba5: ((lba >> 40) & 0xFF) as u8,
            count_lo: (sectors & 0xFF) as u8,
            count_hi: ((sectors >> 8) & 0xFF) as u8,
            ..Default::default()
        };

        self.issue_command(&fis, pa.value() as u64, buf.len() as u32, false)?;

        unsafe {
            core::ptr::copy_nonoverlapping(va as *const u8, buf.as_mut_ptr(), buf.len());
        }

        Ok(sectors)
    }

    /// Écrit des secteurs sur le disque via DMA.
    fn write_sectors(&mut self, buf: &[u8], lba: u64) -> Result<usize, &'static str> {
        let sectors = buf.len() / self.sector_size;
        if sectors == 0 { return Ok(0); }

        let kernel_mmi = get_kernel_mmi_ref().ok_or("AHCI: no kernel MMI")?;
        let pages = allocate_pages_by_bytes(buf.len())
            .ok_or("AHCI write: cannot alloc DMA buf")?;
        let flags = PteFlags::new().valid(true).writable(true).device_memory(true);
        let dma = kernel_mmi.lock().page_table.map_allocated_pages(pages, flags)?;
        let va = dma.start_address().value();
        let pa = kernel_mmi.lock().page_table.translate(dma.start_address())
            .ok_or("AHCI write: VA→PA failed")?;

        // Copier les données dans le buffer DMA
        unsafe {
            core::ptr::copy_nonoverlapping(buf.as_ptr(), va as *mut u8, buf.len());
        }

        let fis = FisRegH2D {
            fis_type: 0x27,
            pm_c: 0x80,
            command: ATA_CMD_WRITE_DMA_EXT,
            device: 1 << 6, // LBA mode
            lba0: (lba & 0xFF) as u8,
            lba1: ((lba >> 8) & 0xFF) as u8,
            lba2: ((lba >> 16) & 0xFF) as u8,
            lba3: ((lba >> 24) & 0xFF) as u8,
            lba4: ((lba >> 32) & 0xFF) as u8,
            lba5: ((lba >> 40) & 0xFF) as u8,
            count_lo: (sectors & 0xFF) as u8,
            count_hi: ((sectors >> 8) & 0xFF) as u8,
            ..Default::default()
        };

        self.issue_command(&fis, pa.value() as u64, buf.len() as u32, true)?;
        Ok(sectors)
    }
}

// ────────────────────────────────────────────────────────────────────────────
// AhciDrive — type public implémentant StorageDevice
// ────────────────────────────────────────────────────────────────────────────

/// Un disque SATA accessible via un port AHCI.
pub struct AhciDrive {
    port: AhciPort,
}

impl AhciDrive {
    /// Retourne le modèle du disque (depuis IDENTIFY DEVICE).
    pub fn model(&self) -> &str {
        &self.port.model
    }

    /// Retourne le numéro de port AHCI (0-31).
    pub fn port_number(&self) -> u8 {
        self.port.port_num
    }
}

impl StorageDevice for AhciDrive {
    fn size_in_blocks(&self) -> usize {
        self.port.sector_count as usize
    }
}

impl BlockIo for AhciDrive {
    fn block_size(&self) -> usize {
        self.port.sector_size
    }
}

impl KnownLength for AhciDrive {
    fn len(&self) -> usize {
        self.port.sector_size * self.port.sector_count as usize
    }
}

impl BlockReader for AhciDrive {
    fn read_blocks(&mut self, buffer: &mut [u8], block_offset: usize) -> Result<usize, IoError> {
        self.port.read_sectors(buffer, block_offset as u64)
            .map_err(|_| IoError::InvalidInput)
    }
}

impl BlockWriter for AhciDrive {
    fn write_blocks(&mut self, buffer: &[u8], block_offset: usize) -> Result<usize, IoError> {
        self.port.write_sectors(buffer, block_offset as u64)
            .map_err(|_| IoError::InvalidInput)
    }
    fn flush(&mut self) -> Result<(), IoError> {
        Ok(())
    }
}

// ────────────────────────────────────────────────────────────────────────────
// AhciController — contrôleur AHCI (implémente StorageController)
// ────────────────────────────────────────────────────────────────────────────

/// Un contrôleur AHCI avec un ou plusieurs disques SATA.
pub struct AhciController {
    /// Pages MMIO de l'ABAR — gardées vivantes tant que le contrôleur existe.
    _abar_pages: MappedPages,
    /// Les disques détectés et initialisés.
    drives: Vec<StorageDeviceRef>,
}

impl StorageController for AhciController {
    fn devices<'c>(&'c self) -> alloc::boxed::Box<(dyn Iterator<Item = StorageDeviceRef> + 'c)> {
        alloc::boxed::Box::new(self.drives.iter().cloned())
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Helpers d'allocation physiquement contiguë
// ────────────────────────────────────────────────────────────────────────────

/// Alloue `size` octets de mémoire MMIO-safe (non-cacheable, writable)
/// et retourne (MappedPages, VA, PA).
fn alloc_dma(size: usize) -> Result<(MappedPages, usize, PhysicalAddress), &'static str> {
    let kernel_mmi = get_kernel_mmi_ref().ok_or("AHCI: no kernel MMI")?;
    let pages = allocate_pages_by_bytes(size)
        .ok_or("AHCI: failed to allocate DMA pages")?;
    let flags = PteFlags::new().valid(true).writable(true).device_memory(true);
    let mapped = kernel_mmi.lock().page_table.map_allocated_pages(pages, flags)?;
    let va = mapped.start_address().value();
    let pa = kernel_mmi.lock().page_table.translate(mapped.start_address())
        .ok_or("AHCI: VA→PA translation failed")?;
    // Zero-fill
    unsafe { core::ptr::write_bytes(va as *mut u8, 0, size); }
    Ok((mapped, va, pa))
}

// ────────────────────────────────────────────────────────────────────────────
// Initialisation d'un port AHCI
// ────────────────────────────────────────────────────────────────────────────

/// Initialise un port AHCI : alloue CLB/FB/CT, identifie le disque.
fn init_port(abar_va: usize, port_num: u8) -> Result<AhciPort, &'static str> {
    let base = abar_va + PORT_BASE + port_num as usize * PORT_SIZE;

    // Vérifier que le port a un disque connecté (SSTS.DET = 3 = device present & phy established)
    let ssts = unsafe { mmio_read32(base, PORT_SSTS) };
    let det = ssts & 0x0F;
    if det != 3 {
        return Err("AHCI: no device on this port");
    }

    // Vérifier la signature — on ne supporte que les disques ATA
    let sig = unsafe { mmio_read32(base, PORT_SIG) };
    if sig != SATA_SIG_ATA {
        info!("AHCI port {}: non-ATA signature {:#x}, skipping", port_num, sig);
        return Err("AHCI: not an ATA device");
    }

    // ── 1. Stopper le port ──────────────────────────────────────────────────
    // Clear ST
    unsafe {
        let cmd = mmio_read32(base, PORT_CMD);
        mmio_write32(base, PORT_CMD, cmd & !CMD_ST);
    }
    for _ in 0..1_000_000 {
        if unsafe { mmio_read32(base, PORT_CMD) } & CMD_CR == 0 { break; }
        core::hint::spin_loop();
    }
    unsafe {
        let cmd = mmio_read32(base, PORT_CMD);
        mmio_write32(base, PORT_CMD, cmd & !CMD_FRE);
    }
    for _ in 0..1_000_000 {
        if unsafe { mmio_read32(base, PORT_CMD) } & CMD_FR == 0 { break; }
        core::hint::spin_loop();
    }

    // ── 2. Allouer la Command List (1 KiB — 32 headers × 32 B) ─────────────
    let (clb_pages, clb_va, clb_pa) = alloc_dma(1024)?;

    // ── 3. Allouer le FIS Receive buffer (256 B min) ────────────────────────
    let (fb_pages, _fb_va, fb_pa) = alloc_dma(256)?;

    // ── 4. Allouer la Command Table pour le slot 0 ──────────────────────────
    let (ct_pages, ct_va, ct_pa) = alloc_dma(CMD_TABLE_SIZE)?;

    // ── 5. Programmer les registres du port ─────────────────────────────────
    unsafe {
        mmio_write32(base, PORT_CLB, clb_pa.value() as u32);
        mmio_write32(base, PORT_CLBU, (clb_pa.value() >> 32) as u32);
        mmio_write32(base, PORT_FB, fb_pa.value() as u32);
        mmio_write32(base, PORT_FBU, (fb_pa.value() >> 32) as u32);

        // Nettoyer les erreurs
        mmio_write32(base, PORT_SERR, 0xFFFF_FFFF);
        mmio_write32(base, PORT_IS, 0xFFFF_FFFF);
    }

    // ── 6. Pointer le Command Header 0 vers la Command Table ────────────────
    let cmd_header = unsafe { &mut *(clb_va as *mut CommandHeader) };
    cmd_header.ctba = ct_pa.value() as u32;
    cmd_header.ctbau = (ct_pa.value() >> 32) as u32;

    // ── 7. Démarrer le port (FRE + ST) ──────────────────────────────────────
    unsafe {
        let cmd = mmio_read32(base, PORT_CMD);
        mmio_write32(base, PORT_CMD, cmd | CMD_FRE);
        // Petit délai pour que FRE soit pris en compte
        for _ in 0..10_000 { core::hint::spin_loop(); }
        let cmd = mmio_read32(base, PORT_CMD);
        mmio_write32(base, PORT_CMD, cmd | CMD_ST);
    }

    let mut port = AhciPort {
        abar_va,
        port_num,
        _clb_pages: clb_pages,
        clb_va,
        _clb_pa: clb_pa,
        _fb_pages: fb_pages,
        _fb_pa: fb_pa,
        _ct_pages: ct_pages,
        ct_va,
        ct_pa,
        sector_count: 0,
        sector_size: SECTOR_SIZE,
        model: String::new(),
    };

    // ── 8. IDENTIFY DEVICE ──────────────────────────────────────────────────
    let (id_pages, id_va, id_pa) = alloc_dma(512)?;

    let fis = FisRegH2D {
        fis_type: 0x27,
        pm_c: 0x80, // C bit
        command: ATA_CMD_IDENTIFY,
        device: 0,
        ..Default::default()
    };

    port.issue_command(&fis, id_pa.value() as u64, 512, false)?;

    // Lire les données IDENTIFY (512 octets = 256 mots de 16 bits)
    let id_data = unsafe { core::slice::from_raw_parts(id_va as *const u16, 256) };

    // Mots 100-103 : nombre total de secteurs LBA48 (64-bit)
    let sector_count =
        (id_data[100] as u64)
        | ((id_data[101] as u64) << 16)
        | ((id_data[102] as u64) << 32)
        | ((id_data[103] as u64) << 48);

    // Si LBA48 est 0, utiliser LBA28 (mots 60-61)
    let sector_count = if sector_count > 0 {
        sector_count
    } else {
        (id_data[60] as u64) | ((id_data[61] as u64) << 16)
    };

    // Mot 106 : logical/physical sector size info
    let word106 = id_data[106];
    let sector_size = if word106 & (1 << 12) != 0 {
        // Mots 117-118 : taille en mots de 16-bit
        let words = (id_data[117] as u32) | ((id_data[118] as u32) << 16);
        if words > 0 { words as usize * 2 } else { SECTOR_SIZE }
    } else {
        SECTOR_SIZE
    };

    // Mots 27-46 : modèle du disque (40 caractères ASCII, byte-swapped)
    let mut model_bytes = [0u8; 40];
    for i in 0..20 {
        let word = id_data[27 + i];
        model_bytes[i * 2] = (word >> 8) as u8;
        model_bytes[i * 2 + 1] = (word & 0xFF) as u8;
    }
    let model = core::str::from_utf8(&model_bytes)
        .unwrap_or("Unknown")
        .trim()
        .into();

    // On n'a plus besoin du buffer IDENTIFY
    drop(id_pages);

    port.sector_count = sector_count;
    port.sector_size = sector_size;
    port.model = model;

    info!(
        "AHCI port {}: \"{}\" — {} sectors × {} B = {} MiB",
        port_num, port.model, sector_count, sector_size,
        (sector_count * sector_size as u64) / (1024 * 1024)
    );

    Ok(port)
}

// ────────────────────────────────────────────────────────────────────────────
// Point d'entrée public
// ────────────────────────────────────────────────────────────────────────────

/// Tente d'initialiser un contrôleur AHCI depuis un device PCI.
///
/// Retourne `Ok(Some(controller))` si réussi, `Ok(None)` si ce n'est pas un
/// device AHCI, ou `Err` si l'initialisation échoue.
pub fn init_from_pci(pci_dev: &PciDevice) -> Result<Option<StorageControllerRef>, &'static str> {
    if pci_dev.class != AHCI_PCI_CLASS || pci_dev.subclass != AHCI_PCI_SUBCLASS {
        return Ok(None);
    }

    info!(
        "AHCI: found controller {:04x}:{:04x} at {}",
        pci_dev.vendor_id, pci_dev.device_id, pci_dev.location
    );

    // ── 1. Mapper ABAR (BAR5) en MMIO ───────────────────────────────────────
    let abar_phys = PhysicalAddress::new_canonical((pci_dev.bars[5] & !0xF) as usize);
    if abar_phys.value() == 0 {
        return Err("AHCI: BAR5 (ABAR) is zero — not a valid AHCI controller");
    }
    let abar_size = 0x2000usize; // 8 KiB couvre HBA registers + 32 ports

    let kernel_mmi = get_kernel_mmi_ref().ok_or("AHCI: no kernel MMI")?;
    let pages = allocate_pages_by_bytes(abar_size)
        .ok_or("AHCI: cannot allocate ABAR pages")?;
    let flags = PteFlags::new().valid(true).writable(true).device_memory(true);
    let abar_pages = kernel_mmi.lock().page_table
        .map_allocated_pages_to(
            pages,
            memory::allocate_frames_by_bytes_at(abar_phys, abar_size)
                .map_err(|_| "AHCI: cannot allocate ABAR frames")?,
            flags,
        )?;
    let abar_va = abar_pages.start_address().value();

    // ── 2. Lire les capacités ────────────────────────────────────────────────
    let cap = unsafe { mmio_read32(abar_va, HBA_CAP) };
    let vs = unsafe { mmio_read32(abar_va, HBA_VS) };
    let pi = unsafe { mmio_read32(abar_va, HBA_PI) };
    let num_ports = ((cap & 0x1F) + 1) as u8; // CAP.NP[4:0] + 1
    let num_cmd_slots = (((cap >> 8) & 0x1F) + 1) as u8; // CAP.NCS[12:8] + 1
    info!(
        "AHCI: version {}.{}, {} ports, {} cmd slots, PI={:#010x}",
        vs >> 16, vs & 0xFFFF, num_ports, num_cmd_slots, pi
    );

    // ── 3. Activer le mode AHCI (GHC.AE=1) ──────────────────────────────────
    unsafe {
        let ghc = mmio_read32(abar_va, HBA_GHC);
        if ghc & GHC_AE == 0 {
            mmio_write32(abar_va, HBA_GHC, ghc | GHC_AE);
            info!("AHCI: enabled AHCI mode (GHC.AE set)");
        }
    }

    // ── 4. Nettoyer les interrupts globales ──────────────────────────────────
    unsafe {
        mmio_write32(abar_va, HBA_IS, 0xFFFF_FFFF);
    }

    // ── 5. Scanner les ports implémentés ─────────────────────────────────────
    let mut drives = Vec::new();

    for port_num in 0..32u8 {
        if pi & (1 << port_num) == 0 {
            continue;
        }

        match init_port(abar_va, port_num) {
            Ok(port) => {
                let drive = AhciDrive { port };
                let drive_ref: StorageDeviceRef = Arc::new(Mutex::new(drive));
                drives.push(drive_ref);
            }
            Err(e) => {
                // Pas d'erreur fatale — un port peut simplement être vide
                info!("AHCI port {}: {}", port_num, e);
            }
        }
    }

    if drives.is_empty() {
        warn!("AHCI: controller detected but no SATA drives initialized");
        warn!("AHCI: verify QEMU has a disk attached: -drive file=disk.img,format=raw,if=none,id=disk0 -device ahci,id=ahci -device ide-hd,drive=disk0,bus=ahci.0");
        return Ok(None);
    }

    info!("AHCI: initialized {} SATA drive(s)", drives.len());

    let controller = AhciController {
        _abar_pages: abar_pages,
        drives,
    };

    Ok(Some(Arc::new(Mutex::new(controller))))
}
