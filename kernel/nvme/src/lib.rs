//! Driver NVMe (NVM Express) pour mai_os.
//!
//! Implémente le NVM Command Set (lecture/écriture de blocs) via PIO sur les
//! queues admin et I/O.  Les interruptions MSI-X sont enregistrées mais on
//! utilise le polling pour la première version (plus simple, plus sûr en
//! environnement no_std sans runtime async).
//!
//! # Pipeline d'initialisation
//! ```text
//! IdeController::new(pci_device)
//!   └─ reset controller (CC.EN=0 → CSTS.RDY=0)
//!   └─ alloue Admin SQ + Admin CQ (contiguës en RAM physique)
//!   └─ configure AQA / ASQ / ACQ
//!   └─ CC.EN=1, attend CSTS.RDY=1
//!   └─ IDENTIFY Controller → détermine ns_count, max_transfer
//!   └─ IDENTIFY Namespace 1 → nsze (taille), lbads (block size)
//!   └─ CREATE_IO_CQ + CREATE_IO_SQ (queue #1, 256 entrées)
//! ```
//!
//! # Références
//! - NVMe Base Specification 2.0 — <https://nvmexpress.org/specifications/>

#![no_std]

extern crate alloc;
#[macro_use] extern crate log;

use alloc::{sync::Arc, vec::Vec};
use spin::Mutex;
use core::sync::atomic::{fence, Ordering};

use memory::{
    allocate_pages_by_bytes, get_kernel_mmi_ref,
    PhysicalAddress, VirtualAddress, MappedPages, PteFlags,
};
use pci::PciDevice;
use storage_device::{StorageDevice, StorageDeviceRef};
use io::{BlockIo, BlockReader, BlockWriter, IoError, KnownLength};

// ────────────────────────────────────────────────────────────────────────────
// Constantes
// ────────────────────────────────────────────────────────────────────────────

const SECTOR_SIZE: usize = 512;

/// Nombre d'entrées dans les queues admin et I/O.
/// Doit être une puissance de 2, ≤ 4096 (max spécifié par le contrôleur).
const QUEUE_DEPTH: u16 = 64;

/// Taille d'une entrée de Submission Queue (SQE) en octets.
const SQE_SIZE: usize = 64;
/// Taille d'une entrée de Completion Queue (CQE) en octets.
const CQE_SIZE: usize = 16;

// ────────────────────────────────────────────────────────────────────────────
// Registres MMIO (offset depuis BAR0)
// ────────────────────────────────────────────────────────────────────────────

const REG_CAP:  usize = 0x00; // Controller Capabilities (8 octets)
const REG_VS:   usize = 0x08; // Version (4 octets)
const REG_CC:   usize = 0x14; // Controller Configuration (4 octets)
const REG_CSTS: usize = 0x1C; // Controller Status (4 octets)
const REG_AQA:  usize = 0x24; // Admin Queue Attributes (4 octets)
const REG_ASQ:  usize = 0x28; // Admin SQ Base Address (8 octets)
const REG_ACQ:  usize = 0x30; // Admin CQ Base Address (8 octets)

/// Offset de base des doorbells dans BAR0.
const DOORBELL_BASE: usize = 0x1000;

// ────────────────────────────────────────────────────────────────────────────
// Opcodes de commandes NVMe
// ────────────────────────────────────────────────────────────────────────────

/// Admin command opcodes
#[repr(u8)]
#[allow(dead_code)]
enum AdminOpcode {
    DeleteIoSq    = 0x00,
    CreateIoSq    = 0x01,
    GetLogPage    = 0x02,
    DeleteIoCq    = 0x04,
    CreateIoCq    = 0x05,
    Identify      = 0x06,
    Abort         = 0x08,
    SetFeatures   = 0x09,
    GetFeatures   = 0x0A,
}

/// NVM command opcodes (I/O)
#[repr(u8)]
#[allow(dead_code)]
enum NvmOpcode {
    Flush  = 0x00,
    Write  = 0x01,
    Read   = 0x02,
}

// ────────────────────────────────────────────────────────────────────────────
// Structures NVMe (layout mémoire exact selon la spec)
// ────────────────────────────────────────────────────────────────────────────

/// Submission Queue Entry (64 octets).
#[derive(Clone, Copy, Default)]
#[repr(C, align(64))]
struct SubmissionEntry {
    /// CDW0 : opcode[7:0], fuse[9:8], psdt[15:14], CID[31:16]
    cdw0:    u32,
    nsid:    u32,
    _rsvd:   [u32; 2],
    /// Metadata Pointer
    mptr:    u64,
    /// PRP Entry 1 (Physical Region Page — adresse physique du buffer)
    prp1:    u64,
    /// PRP Entry 2 (si le buffer dépasse une page)
    prp2:    u64,
    cdw10:   u32,
    cdw11:   u32,
    cdw12:   u32,
    cdw13:   u32,
    cdw14:   u32,
    cdw15:   u32,
}
const _: () = assert!(core::mem::size_of::<SubmissionEntry>() == SQE_SIZE);

/// Completion Queue Entry (16 octets).
#[derive(Clone, Copy, Default)]
#[repr(C, align(16))]
struct CompletionEntry {
    dw0:    u32,
    _rsvd:  u32,
    /// SQ Head Pointer
    sq_hd:  u16,
    /// SQ Identifier
    sq_id:  u16,
    /// Command Identifier (CID)
    cid:    u16,
    /// Status Field : Phase Tag (bit 0) + SC/SCT
    status: u16,
}
const _: () = assert!(core::mem::size_of::<CompletionEntry>() == CQE_SIZE);

// ────────────────────────────────────────────────────────────────────────────
// Queue (SQ ou CQ)
// ────────────────────────────────────────────────────────────────────────────

struct Queue {
    /// Pages physiquement contiguës allouées pour la queue.
    pages:   MappedPages,
    /// Adresse physique du début de la queue (pour la passer au contrôleur).
    phys:    PhysicalAddress,
    /// Capacité (nombre d'entrées).
    depth:   u16,
    /// Tail (pour SQ) ou Head (pour CQ) — position courante.
    head:    u16,
    /// Phase tag courant pour la CQ.
    phase:   u8,
}

impl Queue {
    fn alloc(depth: u16, entry_size: usize) -> Result<Self, &'static str> {
        let total_bytes = depth as usize * entry_size;
        let kernel_mmi = get_kernel_mmi_ref().ok_or("NVMe: no kernel MMI")?;
        let pages = allocate_pages_by_bytes(total_bytes)
            .ok_or("NVMe: failed to allocate queue pages")?;
        let flags = PteFlags::new()
            .valid(true)
            .writable(true)
            .device_memory(true); // non-cacheable pour les DMA
        let mapped = kernel_mmi.lock().page_table
            .map_allocated_pages(pages, flags)?;
        let phys = kernel_mmi.lock().page_table
            .translate(mapped.start_address())
            .ok_or("NVMe: failed to translate queue VA→PA")?;

        // Zéroïse la queue.
        let va = mapped.start_address().value();
        unsafe { core::ptr::write_bytes(va as *mut u8, 0, total_bytes); }

        Ok(Queue { pages: mapped, phys, depth, head: 0, phase: 1 })
    }

    /// Adresse virtuelle d'une entrée de la SQ au `tail`.
    fn sq_entry_mut(&mut self, tail: u16) -> &mut SubmissionEntry {
        let va = self.pages.start_address().value();
        let offset = tail as usize * SQE_SIZE;
        unsafe { &mut *((va + offset) as *mut SubmissionEntry) }
    }

    /// Adresse virtuelle d'une entrée de la CQ à `head`.
    fn cq_entry(&self) -> &CompletionEntry {
        let va = self.pages.start_address().value();
        let offset = self.head as usize * CQE_SIZE;
        unsafe { &*((va + offset) as *const CompletionEntry) }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Doorbell helpers
// ────────────────────────────────────────────────────────────────────────────

/// Écrit la valeur dans le doorbell de la SQ ou CQ indiquée.
///
/// `sq=true` → Submission Queue Tail Doorbell.
/// `sq=false` → Completion Queue Head Doorbell.
unsafe fn write_doorbell(bar0_va: usize, dbl_stride: u32, queue_id: u16, sq: bool, val: u16) {
    let stride = 4 << dbl_stride; // en octets
    let offset = DOORBELL_BASE + (2 * queue_id as usize + if sq { 0 } else { 1 }) * stride;
    let ptr = (bar0_va + offset) as *mut u32;
    ptr.write_volatile(val as u32);
}

// ────────────────────────────────────────────────────────────────────────────
// NvmeController (interne — non partagé directement)
// ────────────────────────────────────────────────────────────────────────────

struct NvmeInner {
    bar0:        MappedPages,
    bar0_va:     usize,
    dbl_stride:  u32,
    /// Submission Queue admin (queue #0)
    admin_sq:    Queue,
    /// Completion Queue admin (queue #0)
    admin_cq:    Queue,
    /// Submission Queue I/O (queue #1)
    io_sq:       Queue,
    /// Completion Queue I/O (queue #1)
    io_cq:       Queue,
    /// Tail courant de la SQ admin
    admin_sq_tail: u16,
    /// Tail courant de la SQ I/O
    io_sq_tail:  u16,
    /// Namespace 1 : nombre de blocs LBA
    ns_size:     u64,
    /// Taille d'un bloc LBA en octets (typiquement 512 ou 4096)
    lba_size:    usize,
    /// Dernier CID utilisé
    next_cid:    u16,
}

// ────────────────────────────────────────────────────────────────────────────
// Lecture/écriture MMIO génériques
// ────────────────────────────────────────────────────────────────────────────

impl NvmeInner {
    #[inline]
    fn read32(&self, offset: usize) -> u32 {
        unsafe { ((self.bar0_va + offset) as *const u32).read_volatile() }
    }
    #[inline]
    fn write32(&mut self, offset: usize, val: u32) {
        unsafe { ((self.bar0_va + offset) as *mut u32).write_volatile(val); }
    }
    #[inline]
    fn read64(&self, offset: usize) -> u64 {
        unsafe { ((self.bar0_va + offset) as *const u64).read_volatile() }
    }
    #[inline]
    fn write64(&mut self, offset: usize, val: u64) {
        unsafe { ((self.bar0_va + offset) as *mut u64).write_volatile(val); }
    }

    /// Attend que `CSTS.RDY` soit égal à `desired_ready` (0 ou 1).
    /// Timeout arbitraire : 1M itérations.
    fn wait_ready(&self, desired: u32) -> Result<(), &'static str> {
        for _ in 0..1_000_000 {
            let csts = self.read32(REG_CSTS);
            if csts & 0x1 == desired {
                return Ok(());
            }
            if csts & 0x2 != 0 {
                return Err("NVMe: controller fatal status");
            }
            core::hint::spin_loop();
        }
        Err("NVMe: timeout waiting for CSTS.RDY")
    }

    // ── Soumission de commande admin ─────────────────────────────────────────

    /// Soumet une commande sur la queue admin et attend la completion (polling).
    fn submit_admin(&mut self, mut cmd: SubmissionEntry) -> Result<u32, &'static str> {
        let cid = self.next_cid;
        self.next_cid = self.next_cid.wrapping_add(1);
        cmd.cdw0 = (cmd.cdw0 & 0x0000_FFFF) | ((cid as u32) << 16);

        let tail = self.admin_sq_tail;
        *self.admin_sq.sq_entry_mut(tail) = cmd;
        fence(Ordering::Release);

        self.admin_sq_tail = (tail + 1) % self.admin_sq.depth;
        unsafe { write_doorbell(self.bar0_va, self.dbl_stride, 0, true, self.admin_sq_tail); }

        self.poll_admin_cq(cid)
    }

    /// Polling sur la CQ admin jusqu'à trouver le CID attendu.
    fn poll_admin_cq(&mut self, expected_cid: u16) -> Result<u32, &'static str> {
        for _ in 0..1_000_000 {
            fence(Ordering::Acquire);
            let entry = *self.admin_cq.cq_entry();
            let phase = (entry.status & 1) as u8;
            if phase == self.admin_cq.phase && entry.cid == expected_cid {
                // Avance le head
                self.admin_cq.head = (self.admin_cq.head + 1) % self.admin_cq.depth;
                if self.admin_cq.head == 0 { self.admin_cq.phase ^= 1; }
                unsafe { write_doorbell(self.bar0_va, self.dbl_stride, 0, false, self.admin_cq.head); }

                let sc = (entry.status >> 1) & 0xFF;
                let sct = (entry.status >> 9) & 0x7;
                if sc != 0 || sct != 0 {
                    error!("NVMe admin cmd cid={} SC={:#x} SCT={:#x}", expected_cid, sc, sct);
                    return Err("NVMe: admin command failed");
                }
                return Ok(entry.dw0);
            }
            core::hint::spin_loop();
        }
        Err("NVMe: admin CQ poll timeout")
    }

    // ── Soumission de commande I/O ───────────────────────────────────────────

    fn submit_io(&mut self, mut cmd: SubmissionEntry) -> Result<u32, &'static str> {
        let cid = self.next_cid;
        self.next_cid = self.next_cid.wrapping_add(1);
        cmd.cdw0 = (cmd.cdw0 & 0x0000_FFFF) | ((cid as u32) << 16);

        let tail = self.io_sq_tail;
        *self.io_sq.sq_entry_mut(tail) = cmd;
        fence(Ordering::Release);

        self.io_sq_tail = (tail + 1) % self.io_sq.depth;
        unsafe { write_doorbell(self.bar0_va, self.dbl_stride, 1, true, self.io_sq_tail); }

        self.poll_io_cq(cid)
    }

    fn poll_io_cq(&mut self, expected_cid: u16) -> Result<u32, &'static str> {
        for _ in 0..10_000_000 {
            fence(Ordering::Acquire);
            let entry = *self.io_cq.cq_entry();
            let phase = (entry.status & 1) as u8;
            if phase == self.io_cq.phase && entry.cid == expected_cid {
                self.io_cq.head = (self.io_cq.head + 1) % self.io_cq.depth;
                if self.io_cq.head == 0 { self.io_cq.phase ^= 1; }
                unsafe { write_doorbell(self.bar0_va, self.dbl_stride, 1, false, self.io_cq.head); }

                let sc  = (entry.status >> 1) & 0xFF;
                let sct = (entry.status >> 9) & 0x7;
                if sc != 0 || sct != 0 {
                    error!("NVMe I/O cmd cid={} SC={:#x} SCT={:#x}", expected_cid, sc, sct);
                    return Err("NVMe: I/O command failed");
                }
                return Ok(entry.dw0);
            }
            core::hint::spin_loop();
        }
        Err("NVMe: I/O CQ poll timeout")
    }

    // ── IDENTIFY ─────────────────────────────────────────────────────────────

    /// Exécute IDENTIFY Controller ou Namespace et copie le résultat dans `out_buf`.
    /// `cns` : 0x00 = Namespace, 0x01 = Controller
    fn identify(&mut self, nsid: u32, cns: u8, out_buf: &mut [u8; 4096]) -> Result<(), &'static str> {
        let kernel_mmi = get_kernel_mmi_ref().ok_or("no kernel MMI")?;
        let pages = allocate_pages_by_bytes(4096)
            .ok_or("NVMe: failed to alloc IDENTIFY buffer")?;
        let flags = PteFlags::new().valid(true).writable(true).device_memory(true);
        let mapped = kernel_mmi.lock().page_table.map_allocated_pages(pages, flags)?;
        let va = mapped.start_address().value();
        let pa = kernel_mmi.lock().page_table.translate(mapped.start_address())
            .ok_or("NVMe: IDENTIFY VA→PA failed")?;

        unsafe { core::ptr::write_bytes(va as *mut u8, 0, 4096); }

        let cmd = SubmissionEntry {
            cdw0:  AdminOpcode::Identify as u32,
            nsid,
            prp1:  pa.value() as u64,
            prp2:  0,
            cdw10: cns as u32,
            ..Default::default()
        };
        self.submit_admin(cmd)?;

        out_buf.copy_from_slice(unsafe { core::slice::from_raw_parts(va as *const u8, 4096) });
        Ok(())
    }

    // ── CREATE_IO_CQ / CREATE_IO_SQ ──────────────────────────────────────────

    fn create_io_cq(&mut self, q_id: u16, phys: PhysicalAddress, depth: u16) -> Result<(), &'static str> {
        let cmd = SubmissionEntry {
            cdw0:  AdminOpcode::CreateIoCq as u32,
            prp1:  phys.value() as u64,
            // cdw10 : QSIZE[31:16] | QID[15:0]
            cdw10: ((depth as u32 - 1) << 16) | q_id as u32,
            // cdw11 : IEN=0 (polling), PC=1 (physically contiguous)
            cdw11: 0x1,
            ..Default::default()
        };
        self.submit_admin(cmd)?;
        Ok(())
    }

    fn create_io_sq(&mut self, q_id: u16, sq_phys: PhysicalAddress, depth: u16, cq_id: u16) -> Result<(), &'static str> {
        let cmd = SubmissionEntry {
            cdw0:  AdminOpcode::CreateIoSq as u32,
            prp1:  sq_phys.value() as u64,
            cdw10: ((depth as u32 - 1) << 16) | q_id as u32,
            // cdw11 : CQID[31:16] | PC=1
            cdw11: ((cq_id as u32) << 16) | 0x1,
            ..Default::default()
        };
        self.submit_admin(cmd)?;
        Ok(())
    }
}

// ────────────────────────────────────────────────────────────────────────────
// NvmeDrive — type public implémentant StorageDevice
// ────────────────────────────────────────────────────────────────────────────

/// Un disque NVMe initialisé et prêt à l'emploi.
pub struct NvmeDrive {
    inner:    Arc<Mutex<NvmeInner>>,
    ns_size:  u64,   // blocs LBA dans le namespace 1
    lba_size: usize, // octets par bloc LBA
}

pub type NvmeDriveRef = Arc<Mutex<NvmeDrive>>;

impl NvmeDrive {
    /// Initialise le contrôleur NVMe à partir du PCI device fourni.
    pub fn new(pci_dev: &PciDevice) -> Result<NvmeDriveRef, &'static str> {
        // ── 1. Lire et mapper BAR0 ───────────────────────────────────────────
        let bar0_phys = PhysicalAddress::new_canonical((pci_dev.bars[0] & !0xF) as usize);
        // NVMe BAR0 fait au minimum 16 KiB (doorbells inclus)
        let bar0_size = 0x4000usize;

        let kernel_mmi = get_kernel_mmi_ref().ok_or("NVMe: no kernel MMI")?;
        let pages = allocate_pages_by_bytes(bar0_size)
            .ok_or("NVMe: cannot allocate BAR0 pages")?;
        let flags = PteFlags::new().valid(true).writable(true).device_memory(true);
        let bar0 = kernel_mmi.lock().page_table
            .map_allocated_pages_to(pages,
                memory::allocate_frames_by_bytes_at(bar0_phys, bar0_size)
                    .map_err(|_| "NVMe: cannot allocate BAR0 frames")?,
                flags)?;
        let bar0_va = bar0.start_address().value();

        // ── 2. Lire les capabilities ─────────────────────────────────────────
        let cap = unsafe { (bar0_va as *const u64).read_volatile() };
        let dbl_stride = ((cap >> 32) & 0xF) as u32; // DSTRD
        let mqes = (cap & 0xFFFF) as u16 + 1;        // Max Queue Entries Supported
        let depth = QUEUE_DEPTH.min(mqes);
        let vs = unsafe { ((bar0_va + REG_VS) as *const u32).read_volatile() };
        info!("NVMe: CAP={:#018x} VS={:#010x} DSTRD={} MQES={}", cap, vs, dbl_stride, mqes);

        // ── 3. Reset du contrôleur (CC.EN=0) ─────────────────────────────────
        unsafe { ((bar0_va + REG_CC) as *mut u32).write_volatile(0x0); }
        // Attente CSTS.RDY=0
        for _ in 0..1_000_000 {
            let csts = unsafe { ((bar0_va + REG_CSTS) as *const u32).read_volatile() };
            if csts & 1 == 0 { break; }
            core::hint::spin_loop();
        }

        // ── 4. Allouer les queues admin ───────────────────────────────────────
        let admin_sq = Queue::alloc(depth, SQE_SIZE)?;
        let admin_cq = Queue::alloc(depth, CQE_SIZE)?;

        // ── 5. Configurer AQA / ASQ / ACQ ────────────────────────────────────
        let aqa: u32 = ((depth as u32 - 1) << 16) | (depth as u32 - 1);
        unsafe {
            ((bar0_va + REG_AQA) as *mut u32).write_volatile(aqa);
            ((bar0_va + REG_ASQ) as *mut u64).write_volatile(admin_sq.phys.value() as u64);
            ((bar0_va + REG_ACQ) as *mut u64).write_volatile(admin_cq.phys.value() as u64);
        }

        // ── 6. CC.EN=1 — démarrage ───────────────────────────────────────────
        // CC : MPS=0 (4KiB), AMS=000 (round-robin), CSS=000 (NVM cmd set),
        //      IOSQES=6 (64B), IOCQES=4 (16B), EN=1
        let cc: u32 = (6 << 20) | (4 << 16) | 0x1;
        unsafe { ((bar0_va + REG_CC) as *mut u32).write_volatile(cc); }
        // Attente CSTS.RDY=1
        for _ in 0..2_000_000 {
            let csts = unsafe { ((bar0_va + REG_CSTS) as *const u32).read_volatile() };
            if csts & 1 == 1 { break; }
            if csts & 2 != 0 { return Err("NVMe: CFS set during init"); }
            core::hint::spin_loop();
        }
        info!("NVMe: controller ready");

        // ── 7. Allouer les queues I/O ─────────────────────────────────────────
        let io_cq = Queue::alloc(depth, CQE_SIZE)?;
        let io_sq = Queue::alloc(depth, SQE_SIZE)?;

        let mut inner = NvmeInner {
            bar0,
            bar0_va,
            dbl_stride,
            admin_sq,
            admin_cq,
            io_sq,
            io_cq,
            admin_sq_tail: 0,
            io_sq_tail:    0,
            ns_size:       0,
            lba_size:      SECTOR_SIZE,
            next_cid:      0,
        };

        // ── 8. IDENTIFY Controller ────────────────────────────────────────────
        let mut id_ctrl = [0u8; 4096];
        inner.identify(0, 0x01, &mut id_ctrl)?;
        let mdts = id_ctrl[77]; // Maximum Data Transfer Size (log2 of pages)
        info!("NVMe: IDENTIFY Controller OK, MDTS={}", mdts);

        // ── 9. IDENTIFY Namespace 1 ───────────────────────────────────────────
        let mut id_ns = [0u8; 4096];
        inner.identify(1, 0x00, &mut id_ns)?;
        // nsze : octets [7:0] LE u64
        let ns_size = u64::from_le_bytes(id_ns[0..8].try_into().unwrap());
        // lbaf : id_ns[128..] array de 16 LBA Format entries (4 octets chacune)
        // flbas[3:0] indique l'index du format LBA actif
        let flbas = id_ns[26] & 0x0F;
        let lbaf_offset = 128 + flbas as usize * 4;
        let lbads = id_ns[lbaf_offset + 2]; // data shift
        let lba_size = if lbads >= 9 { 1usize << lbads } else { SECTOR_SIZE };
        info!("NVMe: ns1 nsze={} lba_size={}", ns_size, lba_size);
        inner.ns_size  = ns_size;
        inner.lba_size = lba_size;

        // ── 10. Créer I/O CQ #1 puis I/O SQ #1 ───────────────────────────────
        let io_cq_phys = inner.io_cq.phys;
        let io_sq_phys = inner.io_sq.phys;
        inner.create_io_cq(1, io_cq_phys, depth)?;
        inner.create_io_sq(1, io_sq_phys, depth, 1)?;
        info!("NVMe: I/O queues created (depth={})", depth);

        let drive = NvmeDrive {
            inner:    Arc::new(Mutex::new(inner)),
            ns_size,
            lba_size,
        };
        Ok(Arc::new(Mutex::new(drive)))
    }

    // ── Lecture ──────────────────────────────────────────────────────────────

    fn read_lba(&mut self, buf: &mut [u8], lba: u64) -> Result<usize, &'static str> {
        let sectors = buf.len() / self.lba_size;
        if sectors == 0 { return Ok(0); }

        let kernel_mmi = get_kernel_mmi_ref().ok_or("no kernel MMI")?;
        let pages = allocate_pages_by_bytes(buf.len())
            .ok_or("NVMe read: cannot alloc DMA buf")?;
        let flags = PteFlags::new().valid(true).writable(true).device_memory(true);
        let dma = kernel_mmi.lock().page_table.map_allocated_pages(pages, flags)?;
        let va  = dma.start_address().value();
        let pa  = kernel_mmi.lock().page_table.translate(dma.start_address())
            .ok_or("NVMe read: VA→PA failed")?.value() as u64;

        let cmd = SubmissionEntry {
            cdw0:  NvmOpcode::Read as u32,
            nsid:  1,
            prp1:  pa,
            prp2:  if buf.len() > 4096 { pa + 4096 } else { 0 },
            cdw10: (lba & 0xFFFF_FFFF) as u32,
            cdw11: (lba >> 32) as u32,
            cdw12: (sectors as u32 - 1) & 0xFFFF, // NLB
            ..Default::default()
        };

        self.inner.lock().submit_io(cmd)?;

        unsafe { core::ptr::copy_nonoverlapping(va as *const u8, buf.as_mut_ptr(), buf.len()); }
        Ok(sectors)
    }

    // ── Écriture ─────────────────────────────────────────────────────────────

    fn write_lba(&mut self, buf: &[u8], lba: u64) -> Result<usize, &'static str> {
        let sectors = buf.len() / self.lba_size;
        if sectors == 0 { return Ok(0); }

        let kernel_mmi = get_kernel_mmi_ref().ok_or("no kernel MMI")?;
        let pages = allocate_pages_by_bytes(buf.len())
            .ok_or("NVMe write: cannot alloc DMA buf")?;
        let flags = PteFlags::new().valid(true).writable(true).device_memory(true);
        let dma = kernel_mmi.lock().page_table.map_allocated_pages(pages, flags)?;
        let va  = dma.start_address().value();
        let pa  = kernel_mmi.lock().page_table.translate(dma.start_address())
            .ok_or("NVMe write: VA→PA failed")?.value() as u64;

        unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), va as *mut u8, buf.len()); }

        let cmd = SubmissionEntry {
            cdw0:  NvmOpcode::Write as u32,
            nsid:  1,
            prp1:  pa,
            prp2:  if buf.len() > 4096 { pa + 4096 } else { 0 },
            cdw10: (lba & 0xFFFF_FFFF) as u32,
            cdw11: (lba >> 32) as u32,
            cdw12: (sectors as u32 - 1) & 0xFFFF,
            ..Default::default()
        };
        self.inner.lock().submit_io(cmd)?;
        Ok(sectors)
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Implémentations des traits storage
// ────────────────────────────────────────────────────────────────────────────

impl StorageDevice for NvmeDrive {
    fn size_in_blocks(&self) -> usize {
        self.ns_size as usize
    }
}
impl BlockIo for NvmeDrive {
    fn block_size(&self) -> usize { self.lba_size }
}
impl KnownLength for NvmeDrive {
    fn len(&self) -> usize { self.lba_size * self.ns_size as usize }
}
impl BlockReader for NvmeDrive {
    fn read_blocks(&mut self, buffer: &mut [u8], block_offset: usize) -> Result<usize, IoError> {
        self.read_lba(buffer, block_offset as u64).map_err(|_| IoError::InvalidInput)
    }
}
impl BlockWriter for NvmeDrive {
    fn write_blocks(&mut self, buffer: &[u8], block_offset: usize) -> Result<usize, IoError> {
        self.write_lba(buffer, block_offset as u64).map_err(|_| IoError::InvalidInput)
    }
    fn flush(&mut self) -> Result<(), IoError> { Ok(()) }
}

// ────────────────────────────────────────────────────────────────────────────
// Détection PCI + enregistrement dans storage_manager
// ────────────────────────────────────────────────────────────────────────────

/// Classe PCI NVMe : class=0x01, subclass=0x08, prog_if=0x02
pub const NVME_PCI_CLASS:    u8 = 0x01;
pub const NVME_PCI_SUBCLASS: u8 = 0x08;
pub const NVME_PCI_PROG_IF:  u8 = 0x02;

/// Tente d'initialiser un contrôleur NVMe depuis un device PCI.
/// Retourne `Ok(Some(ref))` si réussi, `Ok(None)` si pas NVMe.
pub fn init_from_pci(pci_dev: &PciDevice) -> Result<Option<NvmeDriveRef>, &'static str> {
    if pci_dev.class != NVME_PCI_CLASS
        || pci_dev.subclass != NVME_PCI_SUBCLASS
    {
        return Ok(None);
    }
    info!("NVMe: found device {:04x}:{:04x} at {}", pci_dev.vendor_id, pci_dev.device_id, pci_dev.location);
    let drive = NvmeDrive::new(pci_dev)?;
    Ok(Some(drive))
}