//! Pilote NVMe (NVM Express) de MaiSS.
//!
//! # Files multiples
//! Une paire de files de soumission et de complétion par cœur (jusqu'à 64), chacune avec son
//! vecteur MSI-X dirigé vers ce cœur : un cœur soumet et reçoit ses fins sans verrou partagé
//! ni interruption inter-processeurs (NVMe 2.0, § 3.3 ; blk-mq, Bjørling et al., SYSTOR 2013).
//! Les fins sont publiées dans le [`blk::TagSet`] ; la couche bloc décide de sonder ou de
//! dormir ([`blk`], attente hybride).
//!
//! # Sans copie
//! Les données vont directement dans les pages de l'appelant : PRP1/PRP2, et au-delà de deux
//! pages une liste PRP (une frame par étiquette, prise à la première requête qui en a besoin).
//! Les files sont en mémoire cachée ordinaire (le DMA de x86 est cohérent).
//!
//! # Cloches fantômes
//! *Doorbell Buffer Config* (NVMe 1.3, § 5.7), conçu pour les contrôleurs émulés : la queue
//! d'une file est écrite en mémoire, et l'écriture MMIO (une sortie de VM à chaque fois) n'a lieu
//! que si le contrôleur l'a demandée par son *EventIdx*, avec la règle de virtio
//! (`need_event`). Activé quand le contrôleur l'annonce (bit 8 d'OACS), comme Linux.
//!
//! # Références
//! - NVM Express Base Specification 2.0 ; NVM Command Set Specification 1.0.
//! - Linux `drivers/nvme/host/pci.c` (cloches fantômes, `nvme_dbbuf_need_event`).

#![no_std]

extern crate alloc;

use alloc::{boxed::Box, format, string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{fence, AtomicU16, AtomicU32, AtomicUsize, Ordering::*};
use core::time::Duration;

use blk::{BlkDriver, BlkError, BlockDevice, CpuMap, Op, QueueLimits, Request, TagSet};
use kernel_config::memory::PAGE_SIZE;
use log::{error, warn};
use memory::{paging::kernel_half::phys_to_virt, MappedPages};
use pci::PciDevice;
use sync_irq::IrqSafeMutex;

/// Classe PCI NVMe : class=0x01, subclass=0x08, prog_if=0x02
pub const NVME_PCI_CLASS: u8 = 0x01;
pub const NVME_PCI_SUBCLASS: u8 = 0x08;
pub const NVME_PCI_PROG_IF: u8 = 0x02;

// Registres (décalages dans BAR0).
const REG_CAP: usize = 0x00;
const REG_VS: usize = 0x08;
const REG_INTMS: usize = 0x0C;
const REG_CC: usize = 0x14;
const REG_CSTS: usize = 0x1C;
const REG_AQA: usize = 0x24;
const REG_ASQ: usize = 0x28;
const REG_ACQ: usize = 0x30;
const DOORBELL_BASE: usize = 0x1000;

// Commandes d'administration.
const ADM_DELETE_SQ: u8 = 0x00;
const ADM_CREATE_SQ: u8 = 0x01;
const ADM_CREATE_CQ: u8 = 0x05;
const ADM_IDENTIFY: u8 = 0x06;
const ADM_SET_FEATURES: u8 = 0x09;
const ADM_DBBUF_CONFIG: u8 = 0x7C;

// Commandes NVM.
const NVM_FLUSH: u8 = 0x00;
const NVM_WRITE: u8 = 0x01;
const NVM_READ: u8 = 0x02;
const NVM_WRITE_ZEROES: u8 = 0x08;
const NVM_DSM: u8 = 0x09;

const ADMIN_DEPTH: u16 = 32;
/// Profondeur voulue des files d'E/S (bornée par CAP.MQES).
const IO_DEPTH: u16 = 256;
/// Files d'E/S au plus (vecteurs dynamiques disponibles).
const MAX_IO_QUEUES: usize = 32;
/// Transfert au plus par requête (au-delà, la couche bloc découpe).
const MAX_TRANSFER: usize = 1 << 20;
const ADMIN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Default)]
#[repr(C)]
struct Sqe {
    cdw0: u32,
    nsid: u32,
    cdw2: u32,
    cdw3: u32,
    mptr: u64,
    prp1: u64,
    prp2: u64,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
    cdw13: u32,
    cdw14: u32,
    cdw15: u32,
}
const _: () = assert!(core::mem::size_of::<Sqe>() == 64);

#[derive(Clone, Copy, Default)]
#[repr(C)]
struct Cqe {
    dw0: u32,
    dw1: u32,
    sq_head: u16,
    sq_id: u16,
    cid: u16,
    status: u16,
}
const _: () = assert!(core::mem::size_of::<Cqe>() == 16);

/// Frames contiguës, en mémoire cachée, vues par la physmap.
struct Dma {
    pfn: usize,
    frames: usize,
}

impl Dma {
    fn new(bytes: usize) -> Result<Dma, &'static str> {
        let frames = bytes.div_ceil(PAGE_SIZE).max(1);
        let pfn = frame_allocator::alloc_raw_frames(frames).ok_or("NVMe : mémoire DMA insuffisante")?;
        let d = Dma { pfn, frames };
        // SAFETY : frames neuves, projetées par la physmap.
        unsafe { core::ptr::write_bytes(d.va() as *mut u8, 0, frames * PAGE_SIZE) };
        Ok(d)
    }
    fn phys(&self) -> u64 {
        (self.pfn * PAGE_SIZE) as u64
    }
    fn va(&self) -> usize {
        phys_to_virt(self.pfn * PAGE_SIZE)
    }
}

impl Drop for Dma {
    fn drop(&mut self) {
        // SAFETY : plus utilisées par le contrôleur (files supprimées ou contrôleur arrêté).
        unsafe { frame_allocator::free_raw_frames(self.pfn, self.frames) }
    }
}

/// Cloche d'une file : registre MMIO, et emplacements fantômes s'ils sont configurés.
struct Bell {
    mmio: usize,
    shadow: usize,
    event: usize,
}

impl Bell {
    /// `need_event` de virtio et de Linux : le contrôleur veut être prévenu si la nouvelle
    /// valeur a dépassé l'indice d'événement qu'il a publié.
    #[inline]
    fn need_event(event: u16, new: u16, old: u16) -> bool {
        new.wrapping_sub(event).wrapping_sub(1) < new.wrapping_sub(old)
    }

    fn ring(&self, value: u16) {
        if self.shadow != 0 {
            // SAFETY : emplacements de la page fantôme et de la page d'événements, à nous.
            unsafe {
                fence(Release);
                let sp = self.shadow as *mut u32;
                let old = sp.read_volatile() as u16;
                sp.write_volatile(value as u32);
                fence(SeqCst);
                let ev = (self.event as *const u32).read_volatile() as u16;
                if !Self::need_event(ev, value, old) {
                    return;
                }
            }
        } else {
            fence(Release);
        }
        // SAFETY : registre de cloche de la BAR0 projetée.
        unsafe { (self.mmio as *mut u32).write_volatile(value as u32) }
    }
}

struct SqState {
    tail: u16,
    pending: u16,
}

struct CqState {
    head: u16,
    phase: u16,
}

struct IoQueue {
    depth: u16,
    sq: Dma,
    cq: Dma,
    sq_state: IrqSafeMutex<SqState>,
    cq_state: IrqSafeMutex<CqState>,
    /// `head | phase << 16`, lu sans verrou pour savoir s'il reste des fins.
    cq_peek: AtomicU32,
    sq_bell: Bell,
    cq_bell: Bell,
    /// Frame de liste PRP de chaque étiquette (0 : pas encore prise).
    prp: Box<[AtomicUsize]>,
}

struct Admin {
    sq: Dma,
    cq: Dma,
    tail: u16,
    head: u16,
    phase: u16,
    cid: u16,
}

/// Un contrôleur NVMe.
pub struct Nvme {
    _name: String,
    _bar: MappedPages,
    regs: usize,
    stride: usize,
    admin: spin::Mutex<Admin>,
    queues: Box<[IoQueue]>,
    tags: TagSet,
    irq: bool,
    oncs: u16,
    vwc: bool,
    _dbbuf: Option<(Dma, Dma)>,
    msix_table: usize,
    _msix_map: Option<MappedPages>,
    vectors: spin::Mutex<Vec<interrupts::MsiVector>>,
    cpu_map: CpuMap,
}

fn now() -> time::Instant {
    time::Instant::now()
}

impl Nvme {
    #[inline]
    fn r32(&self, off: usize) -> u32 {
        // SAFETY : registre de la BAR0 projetée.
        unsafe { ((self.regs + off) as *const u32).read_volatile() }
    }
    #[inline]
    fn w32(&self, off: usize, v: u32) {
        // SAFETY : comme `r32`.
        unsafe { ((self.regs + off) as *mut u32).write_volatile(v) }
    }

    fn bell_off(stride: usize, qid: u16, cq: bool) -> usize {
        (2 * qid as usize + cq as usize) * stride
    }

    /// Commande d'administration, attendue par sondage (borné par [`ADMIN_TIMEOUT`]).
    fn admin_cmd(&self, mut cmd: Sqe) -> Result<u32, &'static str> {
        let mut a = self.admin.lock();
        let cid = a.cid;
        a.cid = a.cid.wrapping_add(1);
        cmd.cdw0 = (cmd.cdw0 & 0xFFFF) | ((cid as u32) << 16);
        let tail = a.tail;
        // SAFETY : entrée `tail` de la file d'administration (ADMIN_DEPTH entrées).
        unsafe { ((a.sq.va() + tail as usize * 64) as *mut Sqe).write_volatile(cmd) };
        a.tail = (tail + 1) % ADMIN_DEPTH;
        fence(Release);
        self.w32(DOORBELL_BASE, a.tail as u32);
        let start = now();
        loop {
            // SAFETY : entrée `head` de la file de complétion d'administration.
            let e = unsafe { ((a.cq.va() + a.head as usize * 16) as *const Cqe).read_volatile() };
            if e.status & 1 == a.phase {
                fence(Acquire);
                a.head = (a.head + 1) % ADMIN_DEPTH;
                if a.head == 0 {
                    a.phase ^= 1;
                }
                self.w32(DOORBELL_BASE + self.stride, a.head as u32);
                if e.cid != cid {
                    continue;
                }
                let sc = e.status >> 1;
                if sc != 0 {
                    error!("NVMe : commande d'administration {:#x} : statut {:#x}", cmd.cdw0 & 0xFF, sc);
                    return Err("NVMe : échec d'une commande d'administration");
                }
                return Ok(e.dw0);
            }
            if now().duration_since(start) > ADMIN_TIMEOUT {
                // Diagnostic : commande, état du contrôleur (CSTS : RDY, CFS), entrée attendue.
                error!(
                    "NVMe : commande d'administration {:#x} (cid {}) sans réponse : CSTS {:#x}, file SQ queue {} CQ tête {} phase {}, entrée {{cid {}, statut {:#x}}}",
                    cmd.cdw0 & 0xFF,
                    cid,
                    self.r32(REG_CSTS),
                    a.tail,
                    a.head,
                    a.phase,
                    e.cid,
                    e.status
                );
                return Err(ADMIN_TIMEOUT_ERR);
            }
            core::hint::spin_loop();
        }
    }

    fn identify(&self, nsid: u32, cns: u32, buf: &Dma) -> Result<(), &'static str> {
        self.admin_cmd(Sqe { cdw0: ADM_IDENTIFY as u32, nsid, prp1: buf.phys(), cdw10: cns, ..Default::default() })
            .map(|_| ())
    }

    /// Traite les fins de la file `hwq`. Sans attendre : si un autre cœur tient la file de
    /// complétion, il les traitera ; on revérifie après lui pour ne pas laisser une fin arrivée
    /// pendant qu'il relâchait le verrou.
    fn drain(&self, hwq: usize) -> usize {
        let q = &self.queues[hwq];
        let mut n = 0;
        loop {
            let Some(mut cs) = q.cq_state.try_lock() else { return n };
            loop {
                // SAFETY : entrée `head` de la file de complétion `q`.
                let e = unsafe { ((q.cq.va() + cs.head as usize * 16) as *const Cqe).read_volatile() };
                if e.status & 1 != cs.phase {
                    break;
                }
                fence(Acquire);
                self.tags.complete(hwq, e.cid, e.status >> 1);
                n += 1;
                cs.head += 1;
                if cs.head == q.depth {
                    cs.head = 0;
                    cs.phase ^= 1;
                }
            }
            q.cq_peek.store(cs.head as u32 | (cs.phase as u32) << 16, Release);
            if n > 0 {
                q.cq_bell.ring(cs.head);
            }
            drop(cs);
            let peek = q.cq_peek.load(Acquire);
            let (head, phase) = ((peek & 0xFFFF) as usize, (peek >> 16) as u16);
            // SAFETY : comme ci-dessus.
            let e = unsafe { ((q.cq.va() + head * 16) as *const Cqe).read_volatile() };
            if e.status & 1 != phase {
                return n;
            }
        }
    }

    /// Liste PRP de l'étiquette `tag` de la file `q` (frame prise au premier besoin).
    fn prp_page(q: &IoQueue, tag: u16) -> Result<usize, BlkError> {
        let slot = &q.prp[tag as usize];
        let cur = slot.load(Acquire);
        if cur != 0 {
            return Ok(cur);
        }
        let pfn = frame_allocator::alloc_raw_frames(1).ok_or(BlkError::NoMemory)?;
        slot.store(pfn, Release);
        Ok(pfn)
    }

    /// PRP1 et PRP2 des segments (frontière de page respectée, voir `blk`).
    fn build_prps(q: &IoQueue, tag: u16, segs: &[blk::Segment]) -> Result<(u64, u64), BlkError> {
        let Some(first) = segs.first() else { return Ok((0, 0)) };
        let ps = PAGE_SIZE as u64;
        let last = segs.len() - 1;
        // Pages après la première : au plus MAX_TRANSFER / PAGE_SIZE + 1.
        let mut list: [u64; 260] = [0; 260];
        let mut n = 0usize;
        for (i, s) in segs.iter().enumerate() {
            if s.phys % 4 != 0 || (i > 0 && s.phys % ps != 0) || (i < last && (s.phys + s.len as u64) % ps != 0) {
                return Err(BlkError::BadBuffer);
            }
            let end = s.phys + s.len as u64;
            let mut p = if i == 0 { (s.phys & !(ps - 1)) + ps } else { s.phys };
            while p < end {
                if n == list.len() {
                    return Err(BlkError::BadBuffer);
                }
                list[n] = p;
                n += 1;
                p += ps;
            }
        }
        let prp2 = match n {
            0 => 0,
            1 => list[0],
            _ => {
                let pfn = Self::prp_page(q, tag)?;
                let dst = phys_to_virt(pfn * PAGE_SIZE) as *mut u64;
                // SAFETY : frame de liste PRP de cette étiquette (512 entrées, n ≤ 260).
                unsafe { core::ptr::copy_nonoverlapping(list.as_ptr(), dst, n) };
                (pfn * PAGE_SIZE) as u64
            }
        };
        Ok((first.phys, prp2))
    }

    fn controller_ready(&self, ready: bool, timeout: Duration) -> Result<(), &'static str> {
        let start = now();
        loop {
            let csts = self.r32(REG_CSTS);
            if csts & 2 != 0 {
                return Err("NVMe : erreur fatale du contrôleur (CSTS.CFS)");
            }
            if (csts & 1 == 1) == ready {
                return Ok(());
            }
            if now().duration_since(start) > timeout {
                return Err("NVMe : délai dépassé (CSTS.RDY)");
            }
            core::hint::spin_loop();
        }
    }
}

impl BlkDriver for Nvme {
    fn tags(&self) -> &TagSet {
        &self.tags
    }

    fn irq_driven(&self) -> bool {
        self.irq
    }

    fn supports(&self, op: Op) -> bool {
        match op {
            Op::Read | Op::Write | Op::Flush => true,
            Op::WriteZeroes => self.oncs & (1 << 3) != 0,
            Op::Discard => self.oncs & (1 << 2) != 0,
        }
    }

    fn queue_rq(&self, hwq: usize, tag: u16, rq: &Request) -> Result<(), BlkError> {
        let q = &self.queues[hwq];
        let nlb = rq.blocks.saturating_sub(1) & 0xFFFF;
        let mut cmd = Sqe { nsid: rq.unit, cdw10: rq.lba as u32, cdw11: (rq.lba >> 32) as u32, ..Default::default() };
        let opcode = match rq.op {
            Op::Read | Op::Write => {
                let (p1, p2) = Self::build_prps(q, tag, rq.segs)?;
                cmd.prp1 = p1;
                cmd.prp2 = p2;
                cmd.cdw12 = nlb | if rq.fua { 1 << 30 } else { 0 };
                if rq.op == Op::Read { NVM_READ } else { NVM_WRITE }
            }
            Op::Flush => {
                cmd.cdw10 = 0;
                cmd.cdw11 = 0;
                NVM_FLUSH
            }
            Op::WriteZeroes => {
                cmd.cdw12 = nlb;
                NVM_WRITE_ZEROES
            }
            Op::Discard => {
                // Une plage : attributs, nombre de blocs, premier bloc (16 octets).
                let pfn = Self::prp_page(q, tag)?;
                let r = phys_to_virt(pfn * PAGE_SIZE) as *mut u32;
                // SAFETY : frame de cette étiquette.
                unsafe {
                    r.write_volatile(0);
                    r.add(1).write_volatile(rq.blocks);
                    (r.add(2) as *mut u64).write_volatile(rq.lba);
                }
                cmd.prp1 = (pfn * PAGE_SIZE) as u64;
                cmd.cdw10 = 0;
                cmd.cdw11 = 1 << 2;
                NVM_DSM
            }
        };
        cmd.cdw0 = opcode as u32 | (tag as u32) << 16;
        let mut s = q.sq_state.lock();
        // SAFETY : entrée `tail` de la file de soumission (`depth` entrées) ; elle est libre
        // car il y a au plus `depth - 1` étiquettes.
        unsafe { ((q.sq.va() + s.tail as usize * 64) as *mut Sqe).write_volatile(cmd) };
        s.tail = (s.tail + 1) % q.depth;
        s.pending += 1;
        Ok(())
    }

    fn commit(&self, hwq: usize) {
        let q = &self.queues[hwq];
        let mut s = q.sq_state.lock();
        if s.pending > 0 {
            s.pending = 0;
            q.sq_bell.ring(s.tail);
        }
    }

    fn poll(&self, hwq: usize) -> usize {
        self.drain(hwq)
    }

    fn debug_queue(&self, hwq: usize) -> String {
        let q = &self.queues[hwq];
        let rd = |a: usize| if a == 0 { -1 } else { unsafe { (a as *const u32).read_volatile() as i64 } };
        let (tail, pending) = q.sq_state.try_lock().map_or((u16::MAX, u16::MAX), |s| (s.tail, s.pending));
        let peek = q.cq_peek.load(Acquire);
        let (head, phase) = ((peek & 0xFFFF) as usize, peek >> 16);
        // SAFETY : entrée de la file de complétion.
        let e = unsafe { ((q.cq.va() + head * 16) as *const Cqe).read_volatile() };
        format!(
            "SQ queue {} (attente {}), fantôme {} évén. {} ; CQ tête {} phase {}, fantôme {} évén. {} ; CQE[tête] cid {} statut {:#x} ; CSTS {:#x}",
            tail, pending, rd(q.sq_bell.shadow), rd(q.sq_bell.event), head, phase, rd(q.cq_bell.shadow), rd(q.cq_bell.event), e.cid, e.status, self.r32(REG_CSTS)
        )
    }
}

/// Interruption MSI-X d'une file : `data` désigne son [`IrqCtx`].
fn nvme_irq(data: usize) {
    // SAFETY : `data` vient de `Box::leak` d'un `IrqCtx` (vit autant que le contrôleur).
    let ctx = unsafe { &*(data as *const IrqCtx) };
    ctx.nvme.drain(ctx.hwq);
}

struct IrqCtx {
    nvme: &'static Nvme,
    hwq: usize,
}

static CONTROLLERS: AtomicU16 = AtomicU16::new(0);

/// Initialise un contrôleur NVMe et renvoie ses unités (espaces de noms actifs), déjà
/// enregistrées dans la couche bloc.
///
/// Une commande d'administration sans réponse pendant la mise en route fait réinitialiser le
/// contrôleur et tout reprendre, au plus [`INIT_ATTEMPTS`] fois (comme `nvme_timeout` →
/// `nvme_reset_ctrl` sous Linux) : un contrôleur lent ou une complétion perdue ne doit pas
/// coûter le disque.
pub fn init_from_pci(pci_dev: &PciDevice) -> Result<Vec<Arc<BlockDevice>>, &'static str> {
    if pci_dev.class != NVME_PCI_CLASS || pci_dev.subclass != NVME_PCI_SUBCLASS {
        return Ok(Vec::new());
    }
    let index = CONTROLLERS.fetch_add(1, Relaxed);
    let name = format!("nvme{}", index);
    let mut attempt = 1;
    loop {
        match init_controller(pci_dev, name.clone()) {
            Err(e) if e == ADMIN_TIMEOUT_ERR && attempt < INIT_ATTEMPTS => {
                warn!("NVMe : {} : {} — réinitialisation du contrôleur (essai {}/{})", name, e, attempt + 1, INIT_ATTEMPTS);
                attempt += 1;
            }
            r => return r,
        }
    }
}

/// Mises en route tentées avant d'abandonner le contrôleur.
const INIT_ATTEMPTS: u32 = 3;
const ADMIN_TIMEOUT_ERR: &str = "NVMe : délai dépassé (commande d'administration)";

fn init_controller(pci_dev: &PciDevice, name: alloc::string::String) -> Result<Vec<Arc<BlockDevice>>, &'static str> {
    pci_dev.pci_set_command_bus_master_bit();
    pci_dev.pci_enable_mem_space();
    let bar = pci_dev.pci_map_bar_mem(0)?;
    let regs = bar.start_address().value();
    let bar_len = bar.size_in_bytes();

    // SAFETY : registres de la BAR0 projetée.
    let cap = unsafe { ((regs + REG_CAP) as *const u64).read_volatile() };
    let vs = unsafe { ((regs + REG_VS) as *const u32).read_volatile() };
    let mqes = (cap & 0xFFFF) as u32 + 1;
    let stride = 4usize << ((cap >> 32) & 0xF);
    let timeout = Duration::from_millis(500 * (((cap >> 24) & 0xFF).max(1)));
    if (cap >> 37) & 1 == 0 {
        return Err("NVMe : jeu de commandes NVM non pris en charge");
    }
    if (cap >> 48) & 0xF != 0 {
        return Err("NVMe : pages de 4 Kio non prises en charge (CAP.MPSMIN)");
    }

    let msix = pci_dev.msix_info();
    let ncpu = cpu::cpu_count() as usize;
    let want = ncpu.min(MAX_IO_QUEUES).min(msix.map_or(usize::MAX, |m| m.table_size.saturating_sub(1)).max(1));
    let depth = (IO_DEPTH as u32).min(mqes) as u16;

    // Le contrôleur sans files : on lui donne la structure, puis les files plus bas.
    let mut nv = Nvme {
        _name: name.clone(),
        _bar: bar,
        regs,
        stride,
        admin: spin::Mutex::new(Admin { sq: Dma::new(ADMIN_DEPTH as usize * 64)?, cq: Dma::new(ADMIN_DEPTH as usize * 16)?, tail: 0, head: 0, phase: 1, cid: 0 }),
        queues: Box::new([]),
        tags: TagSet::new(0, 1),
        irq: false,
        oncs: 0,
        vwc: false,
        _dbbuf: None,
        msix_table: 0,
        _msix_map: None,
        vectors: spin::Mutex::new(Vec::new()),
        cpu_map: CpuMap::new(1),
    };

    // ── Remise à zéro et file d'administration ──────────────────────────────────────────
    if nv.r32(REG_CC) & 1 != 0 {
        nv.w32(REG_CC, 0);
    }
    nv.controller_ready(false, timeout)?;
    {
        let a = nv.admin.lock();
        let aqa = (ADMIN_DEPTH as u32 - 1) << 16 | (ADMIN_DEPTH as u32 - 1);
        let (asq, acq) = (a.sq.phys(), a.cq.phys());
        drop(a);
        nv.w32(REG_AQA, aqa);
        // SAFETY : registres 64 bits de la BAR0.
        unsafe {
            ((regs + REG_ASQ) as *mut u64).write_volatile(asq);
            ((regs + REG_ACQ) as *mut u64).write_volatile(acq);
        }
    }
    // Interruptions masquées pendant la mise en route (sans effet en MSI-X).
    nv.w32(REG_INTMS, u32::MAX);
    // EN, CSS = NVM, MPS = 4 Kio, arbitrage tourniquet, IOSQES = 64 o, IOCQES = 16 o.
    nv.w32(REG_CC, 1 | (6 << 16) | (4 << 20));
    nv.controller_ready(true, timeout)?;

    // ── Identification ─────────────────────────────────────────────────────────────────
    let page = Dma::new(PAGE_SIZE)?;
    nv.identify(0, 1, &page)?;
    // SAFETY : page d'identification remplie par le contrôleur.
    let id = unsafe { core::slice::from_raw_parts(page.va() as *const u8, PAGE_SIZE) };
    let mdts = id[77];
    let oacs = u16::from_le_bytes([id[256], id[257]]);
    let nn = u32::from_le_bytes([id[516], id[517], id[518], id[519]]);
    nv.oncs = u16::from_le_bytes([id[520], id[521]]);
    nv.vwc = id[525] & 1 != 0;
    let model = core::str::from_utf8(&id[24..64]).unwrap_or("?").trim();
    let max_transfer = if mdts == 0 { MAX_TRANSFER } else { MAX_TRANSFER.min(PAGE_SIZE << mdts) };
    warn!(
        "NVMe : {} : « {} », version {}.{}, MQES {}, MDTS {} ({} Kio par requête), cache d'écriture {}",
        name,
        model,
        vs >> 16,
        (vs >> 8) & 0xFF,
        mqes,
        mdts,
        max_transfer >> 10,
        if nv.vwc { "volatil" } else { "non volatil" }
    );

    // ── Nombre de files ────────────────────────────────────────────────────────────────
    let n1 = (want - 1) as u32;
    let got = nv.admin_cmd(Sqe { cdw0: ADM_SET_FEATURES as u32, cdw10: 0x07, cdw11: n1 << 16 | n1, ..Default::default() })?;
    let nr = want.min((got & 0xFFFF) as usize + 1).min((got >> 16) as usize + 1).max(1);
    let cpu_map = CpuMap::new(nr);

    // ── MSI-X : table masquée en bloc pendant la programmation ─────────────────────────
    let use_irq = match msix {
        Some(m) if m.table_size > nr => {
            let base = pci_dev.determine_mem_base(m.table_bar)?.value() + m.table_offset;
            if m.table_bar == 0 && m.table_offset + (nr + 1) * 16 <= bar_len {
                nv.msix_table = regs + m.table_offset;
            } else {
                let off = base % PAGE_SIZE;
                let mp = memory::map_frame_range(memory::PhysicalAddress::new_canonical(base - off), off + (nr + 1) * 16, memory::MMIO_FLAGS)?;
                nv.msix_table = mp.start_address().value() + off;
                nv._msix_map = Some(mp);
            }
            pci_dev.pci_enable_intx(false);
            pci_dev.pci_msix_control(true, true)?;
            for i in 0..m.table_size.min(nr + 1) {
                // SAFETY : entrée `i` de la table projetée.
                unsafe { pci::msix_mask_entry((nv.msix_table + i * 16) as *mut u32, true) };
            }
            true
        }
        _ => false,
    };
    nv.irq = use_irq;

    // ── Cloches fantômes ───────────────────────────────────────────────────────────────
    let dbbuf = if oacs & (1 << 8) != 0 {
        let (shadow, event) = (Dma::new(PAGE_SIZE)?, Dma::new(PAGE_SIZE)?);
        match nv.admin_cmd(Sqe { cdw0: ADM_DBBUF_CONFIG as u32, prp1: shadow.phys(), prp2: event.phys(), ..Default::default() }) {
            Ok(_) => Some((shadow, event)),
            Err(e) => {
                warn!("NVMe : {} : cloches fantômes refusées ({})", name, e);
                None
            }
        }
    } else {
        None
    };

    // ── Files d'E/S ────────────────────────────────────────────────────────────────────
    let mut queues = Vec::with_capacity(nr);
    for i in 0..nr {
        let qid = (i + 1) as u16;
        let sq = Dma::new(depth as usize * 64)?;
        let cq = Dma::new(depth as usize * 16)?;
        let cq_flags = 1 | if use_irq { 2 | ((qid as u32) << 16) } else { 0 };
        nv.admin_cmd(Sqe { cdw0: ADM_CREATE_CQ as u32, prp1: cq.phys(), cdw10: (depth as u32 - 1) << 16 | qid as u32, cdw11: cq_flags, ..Default::default() })?;
        if let Err(e) = nv.admin_cmd(Sqe { cdw0: ADM_CREATE_SQ as u32, prp1: sq.phys(), cdw10: (depth as u32 - 1) << 16 | qid as u32, cdw11: (qid as u32) << 16 | 1, ..Default::default() }) {
            let _ = nv.admin_cmd(Sqe { cdw0: ADM_DELETE_SQ as u32, cdw10: qid as u32, ..Default::default() });
            return Err(e);
        }
        let bell = |cq: bool| {
            let off = Nvme::bell_off(stride, qid, cq);
            match &dbbuf {
                Some((s, e)) => Bell { mmio: regs + DOORBELL_BASE + off, shadow: s.va() + off, event: e.va() + off },
                None => Bell { mmio: regs + DOORBELL_BASE + off, shadow: 0, event: 0 },
            }
        };
        queues.push(IoQueue {
            depth,
            sq,
            cq,
            sq_state: IrqSafeMutex::new(SqState { tail: 0, pending: 0 }),
            cq_state: IrqSafeMutex::new(CqState { head: 0, phase: 1 }),
            cq_peek: AtomicU32::new(1 << 16),
            sq_bell: bell(false),
            cq_bell: bell(true),
            prp: (0..depth).map(|_| AtomicUsize::new(0)).collect(),
        });
    }
    nv.queues = queues.into_boxed_slice();
    // Au plus `depth - 1` requêtes en vol par file : la file de soumission ne déborde jamais.
    nv.tags = TagSet::new(nr, depth as usize - 1);
    nv.cpu_map = cpu_map.clone();
    nv._dbbuf = dbbuf;

    // ── Unités ─────────────────────────────────────────────────────────────────────────
    let mut nsids: Vec<u32> = Vec::new();
    if nv.identify(0, 2, &page).is_ok() {
        // SAFETY : liste des espaces de noms actifs (u32 croissants, 0 = fin).
        let list = unsafe { core::slice::from_raw_parts(page.va() as *const u32, PAGE_SIZE / 4) };
        nsids.extend(list.iter().copied().take_while(|&n| n != 0));
    } else {
        nsids.extend(1..=nn.min(16));
    }
    let mut units = Vec::new();
    for nsid in nsids {
        if nv.identify(nsid, 0, &page).is_err() {
            continue;
        }
        // SAFETY : page d'identification de l'espace de noms.
        let ns = unsafe { core::slice::from_raw_parts(page.va() as *const u8, PAGE_SIZE) };
        let nsze = u64::from_le_bytes(ns[0..8].try_into().unwrap());
        if nsze == 0 {
            continue;
        }
        let flbas = (ns[26] & 0xF) as usize;
        let lbads = ns[128 + flbas * 4 + 2] as u32;
        let ms = u16::from_le_bytes([ns[128 + flbas * 4], ns[128 + flbas * 4 + 1]]);
        if !(9..=12).contains(&lbads) || ms != 0 {
            warn!("NVMe : {} : espace de noms {} ignoré (blocs de 2^{} o, {} o de métadonnées)", name, nsid, lbads, ms);
            continue;
        }
        units.push((nsid, nsze, lbads));
    }

    let nv = Arc::new(nv);
    // Le contrôleur vit jusqu'à l'arrêt : une référence est laissée aux interruptions.
    // SAFETY : `into_raw` garde l'objet en vie pour toujours.
    let nv_static: &'static Nvme = unsafe { &*Arc::into_raw(nv.clone()) };

    if use_irq {
        for i in 0..nr {
            let ctx = Box::leak(Box::new(IrqCtx { nvme: nv_static, hwq: i }));
            match interrupts::register_msi_callback(nvme_irq, ctx as *const IrqCtx as usize) {
                Ok(v) => {
                    let apic = cpu_map.irq_cpu[i].value();
                    // SAFETY : entrée `qid` de la table projetée (vecteur 0 : file
                    // d'administration, sondée, reste masqué).
                    unsafe { pci::msix_write_entry((nv.msix_table + (i + 1) * 16) as *mut u32, apic, v.vector) };
                    nv.vectors.lock().push(v);
                }
                Err(e) => {
                    error!("NVMe : {} : pas de vecteur pour la file {} : {} (fins par le chien de garde)", name, i + 1, e);
                }
            }
        }
        pci_dev.pci_msix_control(true, false)?;
    }

    warn!(
        "NVMe : {} : {} file(s) d'E/S de {} entrées, {}, cloches {}",
        name,
        nr,
        depth,
        if use_irq { "MSI-X par cœur" } else { "sondage (pas de MSI-X)" },
        if nv._dbbuf.is_some() { "fantômes (Doorbell Buffer Config)" } else { "MMIO" }
    );

    let driver: Arc<dyn BlkDriver> = nv.clone();
    let mut out = Vec::new();
    for (nsid, nsze, lbads) in units {
        let limits = QueueLimits {
            lba_shift: lbads,
            max_transfer,
            max_segments: max_transfer / PAGE_SIZE + 1,
            dma_align: 4,
            volatile_cache: nv.vwc,
        };
        let dev = BlockDevice::new(format!("{}n{}", name, nsid), nsid, nsze, limits, driver.clone(), cpu_map.clone());
        blk::register(dev.clone());
        out.push(dev);
    }
    if out.is_empty() {
        warn!("NVMe : {} : aucun espace de noms utilisable", name);
    }
    Ok(out)
}
