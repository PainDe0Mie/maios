//! MaiSS (Mai Storage System) : couche bloc à files multiples.
//!
//! # Modèle
//! Comme blk-mq de Linux (Bjørling, Axboe, Nellans, Bonnet, « Linux Block IO: Introducing
//! Multi-queue SSD Access on Multi-core Systems », SYSTOR 2013), chaque cœur soumet dans *sa*
//! file matérielle (une paire SQ/CQ de NVMe, dont l'interruption MSI-X revient sur ce cœur) :
//! pas de verrou global, pas de rebond de ligne de cache entre cœurs, et la fin d'une requête
//! réveille la tâche sur le cœur qui l'a lancée.
//!
//! * [`BlkDriver`] : ce qu'un pilote fournit (placer une requête, sonner, sonder la file de
//!   complétion). Il publie les fins dans son [`TagSet`].
//! * [`BlockDevice`] : une unité (un espace de noms NVMe), avec ses limites ([`QueueLimits`]),
//!   ses statistiques, et les entrées synchrones (`read`, `write`, `flush`, `submit_batch`).
//! * [`BlkStorage`] : adaptateur vers [`storage_device::StorageDevice`] (tables de partitions,
//!   FAT32, cache de blocs).
//!
//! # Sans copie
//! Une requête décrit ses données par des segments physiques ([`Segment`]). Un tampon du noyau
//! est traduit page par page et donné tel quel à l'appareil (DMA directement dans le tampon de
//! l'appelant). Les segments respectent la « frontière virtuelle » de NVMe (`virt_boundary` de
//! Linux) : seul le premier commence au milieu d'une page, seul le dernier y finit. Un tampon
//! mal aligné passe par un tampon de rebond.
//!
//! # Attente hybride
//! Yang, Minturn, Hady, « When Poll Is Better than Interrupt » (FAST 2012) : sur un SSD rapide,
//! le coût d'une interruption et du réveil d'une tâche dépasse la latence de l'appareil. Si la
//! latence moyenne de la file est courte ([`HYBRID_POLL_MAX_NS`], ou [`IDLE_POLL_MAX_NS`] quand
//! aucune autre tâche n'attend le cœur : le sondage ne prend alors que du temps inactif), la
//! tâche sonde la file de complétion au plus deux fois cette moyenne, puis se bloque et laisse
//! l'interruption la réveiller (le *classic polling* de Linux, borné, sans brûler un cœur
//! utile sur un appareil lent).
//! Sans interruptions (appareil sans MSI-X, ou appel interruptions coupées), elle sonde.
//!
//! # Chien de garde
//! Une tâche sonde toutes les [`WATCHDOG_PERIOD`] les files qui ont des requêtes en vol : une
//! interruption perdue ne bloque jamais une tâche (le rôle de `blk_mq_timeout_work`).

#![no_std]

extern crate alloc;

mod tags;

pub use tags::{Status, TagSet};

use alloc::{format, string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::*};
use core::time::Duration;
use kernel_config::memory::PAGE_SIZE;
use log::{error, warn};
use memory::VirtualAddress;
use spin::Mutex;
use tags::{DONE, INFLIGHT};

/// Opération d'une requête.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Read,
    Write,
    /// Vide le cache d'écriture volatil de l'appareil.
    Flush,
    /// Écrit des zéros sans transfert (`Write Zeroes` de NVMe).
    WriteZeroes,
    /// Libère des blocs (TRIM, `Dataset Management` de NVMe).
    Discard,
}

/// Zone physiquement contiguë des données d'une requête.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub phys: u64,
    pub len: u32,
}

/// Requête donnée au pilote.
#[derive(Debug)]
pub struct Request<'a> {
    pub op: Op,
    /// Unité visée (espace de noms NVMe).
    pub unit: u32,
    /// Premier bloc logique (en blocs de l'unité).
    pub lba: u64,
    /// Nombre de blocs.
    pub blocks: u32,
    pub segs: &'a [Segment],
    /// Écriture forcée sur le support (*Force Unit Access*).
    pub fua: bool,
}

/// Erreurs d'E/S bloc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlkError {
    /// Échec signalé par l'appareil (statut du pilote).
    Io(Status),
    Unsupported,
    OutOfRange,
    BadBuffer,
    NoMemory,
}

impl BlkError {
    pub fn as_str(&self) -> &'static str {
        match self {
            BlkError::Io(_) => "blk : erreur d'E/S de l'appareil",
            BlkError::Unsupported => "blk : opération non prise en charge",
            BlkError::OutOfRange => "blk : au-delà de la fin de l'unité",
            BlkError::BadBuffer => "blk : tampon invalide",
            BlkError::NoMemory => "blk : mémoire insuffisante",
        }
    }
}

/// Limites d'une unité, publiées par le pilote.
#[derive(Clone, Copy, Debug)]
pub struct QueueLimits {
    /// log2 de la taille d'un bloc logique.
    pub lba_shift: u32,
    /// Octets au plus par requête (multiple de [`PAGE_SIZE`]).
    pub max_transfer: usize,
    /// Segments au plus par requête.
    pub max_segments: usize,
    /// Alignement exigé de l'adresse physique des données (4 pour les PRP de NVMe).
    pub dma_align: usize,
    /// Le cache d'écriture est volatil : `flush` a un effet.
    pub volatile_cache: bool,
}

/// Ce qu'un pilote fournit à la couche bloc.
pub trait BlkDriver: Send + Sync {
    /// Étiquettes et emplacements de fin de toutes les files.
    fn tags(&self) -> &TagSet;
    /// Les fins arrivent par interruption (sinon : sondage seul).
    fn irq_driven(&self) -> bool;
    fn supports(&self, op: Op) -> bool;
    /// Place la requête `tag` dans la file `hwq`, sans sonner. Le pilote garde ce dont il a
    /// besoin (liste PRP par étiquette) jusqu'à la fin.
    fn queue_rq(&self, hwq: usize, tag: u16, rq: &Request) -> Result<(), BlkError>;
    /// Sonne la cloche de `hwq` pour les requêtes placées (une seule écriture MMIO par lot).
    fn commit(&self, hwq: usize);
    /// Traite les fins présentes dans la file de complétion de `hwq` ; renvoie leur nombre.
    fn poll(&self, hwq: usize) -> usize;
    /// État de la file `hwq` pour le diagnostic (requête coincée).
    fn debug_queue(&self, _hwq: usize) -> String {
        String::new()
    }
}

/// Latence moyenne au-dessous de laquelle l'attente commence par sonder (valeur par défaut).
pub const HYBRID_POLL_MAX_NS: u64 = 40_000;
static POLL_MAX_NS: AtomicU64 = AtomicU64::new(HYBRID_POLL_MAX_NS);

/// Seuil quand rien d'autre n'attend le cœur : sonder ne prend alors que du temps inactif, et
/// économise l'interruption et le réveil (−21 % de latence QD1 sous QEMU, où l'appareil répond
/// en 40 µs environ).
pub const IDLE_POLL_MAX_NS: u64 = 200_000;

/// Règle le seuil du sondage hybride (0 : jamais de sondage, toujours l'interruption).
pub fn set_hybrid_poll_max_ns(ns: u64) {
    POLL_MAX_NS.store(ns, Relaxed);
}

pub fn hybrid_poll_max_ns() -> u64 {
    POLL_MAX_NS.load(Relaxed)
}
/// Période du chien de garde.
pub const WATCHDOG_PERIOD: Duration = Duration::from_millis(100);
/// Âge au-delà duquel une requête en vol est considérée comme sans interruption.
pub const WATCHDOG_STALE_NS: u64 = 50_000_000;

/// Statistiques d'une unité (lues par `blkbench`, puis `/proc/diskstats`).
#[derive(Default)]
pub struct DevStats {
    pub reads: AtomicU64,
    pub writes: AtomicU64,
    pub read_bytes: AtomicU64,
    pub write_bytes: AtomicU64,
    pub flushes: AtomicU64,
    /// Fins vues pendant le sondage hybride (sans interruption ni réveil).
    pub polled: AtomicU64,
    /// Fins attendues en se bloquant.
    pub slept: AtomicU64,
    /// Somme des latences (ns).
    pub busy_ns: AtomicU64,
    pub errors: AtomicU64,
    /// Requêtes passées par un tampon de rebond.
    pub bounced: AtomicU64,
}

/// Correspondance cœur → file matérielle : la file `i` sert les cœurs `j` tels que
/// `j % nr == i` dans l'ordre de [`cpu::cpus`] trié.
#[derive(Clone)]
pub struct CpuMap {
    /// Par identifiant d'APIC (modulo 256) : file.
    map: [u8; 256],
    /// Cœur qui reçoit l'interruption de chaque file.
    pub irq_cpu: Vec<cpu::CpuId>,
}

impl CpuMap {
    pub fn new(nr_hw: usize) -> CpuMap {
        let mut cpus: Vec<cpu::CpuId> = cpu::cpus().collect();
        cpus.sort_by_key(|c| c.value());
        let nr = nr_hw.max(1);
        let mut map = [0u8; 256];
        for (j, c) in cpus.iter().enumerate() {
            map[(c.value() & 0xFF) as usize] = (j % nr) as u8;
        }
        let irq_cpu = (0..nr).map(|i| cpus.get(i).copied().unwrap_or_else(cpu::current_cpu)).collect();
        CpuMap { map, irq_cpu }
    }

    #[inline]
    pub fn hwq_of(&self, cpu: cpu::CpuId) -> usize {
        self.map[(cpu.value() & 0xFF) as usize] as usize
    }
}

/// Une unité de stockage (un espace de noms NVMe).
pub struct BlockDevice {
    pub name: String,
    pub unit: u32,
    /// Nombre de blocs logiques.
    pub capacity: u64,
    pub limits: QueueLimits,
    pub stats: DevStats,
    driver: Arc<dyn BlkDriver>,
    cpu_map: CpuMap,
}

fn now_ns() -> u64 {
    time::Instant::now().duration_since(time::Instant::ZERO).as_nanos() as u64
}

/// Un « tampon de rebond » : frames contiguës, projetées par la physmap.
struct Bounce {
    pfn: usize,
    frames: usize,
}

impl Bounce {
    fn new(len: usize) -> Result<Bounce, BlkError> {
        let frames = len.div_ceil(PAGE_SIZE).max(1);
        let pfn = frame_allocator::alloc_raw_frames(frames).ok_or(BlkError::NoMemory)?;
        Ok(Bounce { pfn, frames })
    }
    fn ptr(&self) -> *mut u8 {
        memory::paging::kernel_half::phys_to_virt(self.pfn * PAGE_SIZE) as *mut u8
    }
    fn seg(&self, len: usize) -> Segment {
        Segment { phys: (self.pfn * PAGE_SIZE) as u64, len: len as u32 }
    }
}

impl Drop for Bounce {
    fn drop(&mut self) {
        // SAFETY : frames prises par `new`, plus référencées par l'appareil (requête finie).
        unsafe { frame_allocator::free_raw_frames(self.pfn, self.frames) }
    }
}

/// Segments physiques du tampon `[va, va + len)` du noyau (fusionnés quand ils se suivent).
pub fn segments_of(va: usize, len: usize) -> Result<Vec<Segment>, BlkError> {
    let mut segs: Vec<Segment> = Vec::new();
    let mut pos = va;
    let end = va + len;
    while pos < end {
        let n = (PAGE_SIZE - pos % PAGE_SIZE).min(end - pos);
        let pa = memory::translate(VirtualAddress::new(pos).ok_or(BlkError::BadBuffer)?)
            .ok_or(BlkError::BadBuffer)?
            .value() as u64;
        match segs.last_mut() {
            Some(last) if last.phys + last.len as u64 == pa => last.len += n as u32,
            _ => segs.push(Segment { phys: pa, len: n as u32 }),
        }
        pos += n;
    }
    Ok(segs)
}

/// Découpe `segs` (couvrant `blocks` blocs à partir de `lba`) en morceaux qui tiennent dans les
/// limites : au plus `max_transfer` octets et `max_segments` segments chacun. Les coupes tombent
/// sur des multiples du bloc.
fn split<'s>(segs: &'s [Segment], lim: &QueueLimits) -> Vec<Vec<Segment>> {
    let bs = 1usize << lim.lba_shift;
    let max_bytes = lim.max_transfer.max(bs) / bs * bs;
    let mut out: Vec<Vec<Segment>> = Vec::new();
    let mut cur: Vec<Segment> = Vec::new();
    let mut cur_bytes = 0usize;
    for s in segs {
        let mut s = *s;
        while s.len > 0 {
            if cur.len() == lim.max_segments.max(1) || cur_bytes == max_bytes {
                out.push(core::mem::take(&mut cur));
                cur_bytes = 0;
            }
            let room = max_bytes - cur_bytes;
            let mut take = (s.len as usize).min(room);
            // Un morceau qui n'est pas le dernier doit finir sur un multiple du bloc.
            if take < s.len as usize && take < room {
                take = take / bs * bs;
            }
            if take == 0 {
                out.push(core::mem::take(&mut cur));
                cur_bytes = 0;
                continue;
            }
            cur.push(Segment { phys: s.phys, len: take as u32 });
            cur_bytes += take;
            s.phys += take as u64;
            s.len -= take as u32;
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

impl BlockDevice {
    pub fn new(name: String, unit: u32, capacity: u64, limits: QueueLimits, driver: Arc<dyn BlkDriver>, cpu_map: CpuMap) -> Arc<BlockDevice> {
        Arc::new(BlockDevice { name, unit, capacity, limits, stats: DevStats::default(), driver, cpu_map })
    }

    pub fn block_size(&self) -> usize {
        1 << self.limits.lba_shift
    }

    pub fn nr_hw_queues(&self) -> usize {
        self.driver.tags().nr_hw_queues()
    }

    /// Latence moyenne glissante de la file `hwq` (ns).
    pub fn latency_ewma_ns(&self, hwq: usize) -> u64 {
        self.driver.tags().queues.get(hwq).map_or(0, |q| q.lat_ewma_ns.load(Relaxed))
    }

    fn current_hwq(&self) -> usize {
        self.cpu_map.hwq_of(cpu::current_cpu())
    }

    /// Prend une étiquette dans `hwq` ; si la file est pleine, sonde puis cède le cœur.
    fn alloc_tag(&self, hwq: usize) -> u16 {
        let q = &self.driver.tags().queues[hwq];
        loop {
            if let Some(t) = q.alloc() {
                return t;
            }
            self.driver.poll(hwq);
            if irq_safety::interrupts_enabled() && preemption::preemption_enabled() {
                task::schedule();
            } else {
                core::hint::spin_loop();
            }
        }
    }

    /// Place une requête (étiquette prise, attente inscrite), sans sonner.
    fn start(&self, hwq: usize, rq: &Request) -> Result<u16, BlkError> {
        let tag = self.alloc_tag(hwq);
        let q = &self.driver.tags().queues[hwq];
        let slot = &q.slots[tag as usize];
        *slot.waiter.lock() = task::get_my_current_task();
        slot.start_ns.store(now_ns(), Relaxed);
        slot.state.store(INFLIGHT, Release);
        if let Err(e) = self.driver.queue_rq(hwq, tag, rq) {
            q.free(tag);
            return Err(e);
        }
        Ok(tag)
    }

    /// Attend la fin de `tag` dans `hwq`, rend l'étiquette, et renvoie le statut.
    fn wait(&self, hwq: usize, tag: u16) -> Result<(), BlkError> {
        let q = &self.driver.tags().queues[hwq];
        let slot = &q.slots[tag as usize];
        let done = || slot.state.load(Acquire) == DONE;
        let can_sleep = self.driver.irq_driven() && irq_safety::interrupts_enabled() && preemption::preemption_enabled();
        let start = slot.start_ns.load(Relaxed);

        if !done() {
            let ewma = q.lat_ewma_ns.load(Relaxed);
            if !can_sleep {
                while !done() {
                    self.driver.poll(hwq);
                    core::hint::spin_loop();
                }
            } else {
                // Sondage hybride, borné à deux fois la latence moyenne.
                let max = POLL_MAX_NS.load(Relaxed);
                let max = if max != 0 && !task::scheduler::cpu_has_other_work() { max.max(IDLE_POLL_MAX_NS) } else { max };
                if ewma != 0 && ewma <= max {
                    let deadline = start + 2 * ewma;
                    while !done() && now_ns() < deadline {
                        self.driver.poll(hwq);
                        core::hint::spin_loop();
                    }
                }
                if done() {
                    self.stats.polled.fetch_add(1, Relaxed);
                } else {
                    // Se bloquer, puis revérifier : une fin publiée entre les deux nous a vus
                    // bloqués et nous rend exécutables (pas de réveil perdu).
                    self.stats.slept.fetch_add(1, Relaxed);
                    if let Some(me) = task::get_my_current_task() {
                        while !done() {
                            let _ = me.block();
                            if done() {
                                    let _ = me.unblock();
                                break;
                            }
                            task::schedule();
                        }
                    } else {
                        while !done() {
                            self.driver.poll(hwq);
                            core::hint::spin_loop();
                        }
                    }
                }
            }
        } else {
            self.stats.polled.fetch_add(1, Relaxed);
        }

        let lat = now_ns().saturating_sub(start);
        q.update_latency(lat);
        self.stats.busy_ns.fetch_add(lat, Relaxed);
        let status = slot.status.load(Relaxed);
        q.free(tag);
        if status != 0 {
            self.stats.errors.fetch_add(1, Relaxed);
            return Err(BlkError::Io(status));
        }
        Ok(())
    }

    fn account(&self, op: Op, bytes: usize) {
        match op {
            Op::Read => {
                self.stats.reads.fetch_add(1, Relaxed);
                self.stats.read_bytes.fetch_add(bytes as u64, Relaxed);
            }
            Op::Write | Op::WriteZeroes => {
                self.stats.writes.fetch_add(1, Relaxed);
                self.stats.write_bytes.fetch_add(bytes as u64, Relaxed);
            }
            Op::Flush => {
                self.stats.flushes.fetch_add(1, Relaxed);
            }
            Op::Discard => {}
        }
    }

    /// Soumet un lot de requêtes d'un coup (*plugging*) : toutes sont placées, la cloche sonne
    /// une fois, puis on attend chacune. Les requêtes sont prises par morceaux de la profondeur
    /// de la file.
    pub fn submit_batch(&self, rqs: &[Request]) -> Result<(), BlkError> {
        let hwq = self.current_hwq();
        let depth = self.driver.tags().queues[hwq].depth.max(1);
        for rq in rqs {
            self.check(rq)?;
        }
        let mut first_err = Ok(());
        for chunk in rqs.chunks(depth) {
            let mut tags: Vec<Result<u16, BlkError>> = Vec::with_capacity(chunk.len());
            for rq in chunk {
                tags.push(self.start(hwq, rq));
            }
            self.driver.commit(hwq);
            for (rq, t) in chunk.iter().zip(tags) {
                let r = t.and_then(|tag| self.wait(hwq, tag));
                if r.is_ok() {
                    self.account(rq.op, rq.blocks as usize * self.block_size());
                } else if first_err.is_ok() {
                    first_err = r;
                }
            }
        }
        first_err
    }

    fn check(&self, rq: &Request) -> Result<(), BlkError> {
        if !self.driver.supports(rq.op) {
            return Err(BlkError::Unsupported);
        }
        if rq.op != Op::Flush && rq.lba.checked_add(rq.blocks as u64).map_or(true, |e| e > self.capacity) {
            return Err(BlkError::OutOfRange);
        }
        Ok(())
    }

    /// Lit ou écrit `segs` (déjà alignés pour le DMA) à partir du bloc `lba`, découpés selon les
    /// limites et soumis en un lot.
    pub fn rw_segments(&self, op: Op, lba: u64, segs: &[Segment], fua: bool) -> Result<(), BlkError> {
        let bs = self.block_size();
        let pieces = split(segs, &self.limits);
        let mut rqs = Vec::with_capacity(pieces.len());
        let mut at = lba;
        for p in &pieces {
            let bytes: usize = p.iter().map(|s| s.len as usize).sum();
            if bytes % bs != 0 {
                return Err(BlkError::BadBuffer);
            }
            rqs.push(Request { op, unit: self.unit, lba: at, blocks: (bytes / bs) as u32, segs: p, fua });
            at += (bytes / bs) as u64;
        }
        self.submit_batch(&rqs)
    }

    fn rw_buf(&self, op: Op, lba: u64, ptr: *mut u8, len: usize) -> Result<(), BlkError> {
        let bs = self.block_size();
        if len == 0 {
            return Ok(());
        }
        if len % bs != 0 {
            return Err(BlkError::BadBuffer);
        }
        if (ptr as usize) % self.limits.dma_align.max(1) == 0 {
            if let Ok(segs) = segments_of(ptr as usize, len) {
                return self.rw_segments(op, lba, &segs, false);
            }
        }
        // Tampon mal aligné (ou hors de l'espace du noyau) : rebond.
        self.stats.bounced.fetch_add(1, Relaxed);
        let b = Bounce::new(len)?;
        if op == Op::Write {
            // SAFETY : `ptr` désigne `len` octets lisibles (contrat de `write`), `b` en a autant.
            unsafe { core::ptr::copy_nonoverlapping(ptr as *const u8, b.ptr(), len) };
        }
        self.rw_segments(op, lba, &[b.seg(len)], false)?;
        if op == Op::Read {
            // SAFETY : comme ci-dessus, dans l'autre sens.
            unsafe { core::ptr::copy_nonoverlapping(b.ptr(), ptr, len) };
        }
        Ok(())
    }

    /// Lit `buf.len()` octets (multiple du bloc) à partir du bloc `lba`.
    pub fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlkError> {
        self.rw_buf(Op::Read, lba, buf.as_mut_ptr(), buf.len())
    }

    /// Écrit `buf` (multiple du bloc) à partir du bloc `lba`.
    pub fn write(&self, lba: u64, buf: &[u8]) -> Result<(), BlkError> {
        self.rw_buf(Op::Write, lba, buf.as_ptr() as *mut u8, buf.len())
    }

    /// Vide le cache d'écriture de l'appareil (sans effet s'il n'est pas volatil).
    pub fn flush(&self) -> Result<(), BlkError> {
        if !self.limits.volatile_cache || !self.driver.supports(Op::Flush) {
            return Ok(());
        }
        self.submit_batch(&[Request { op: Op::Flush, unit: self.unit, lba: 0, blocks: 0, segs: &[], fua: false }])
    }

    /// Chien de garde : sonde les files qui ont une requête en vol depuis plus de
    /// [`WATCHDOG_STALE_NS`], et renvoie le nombre de ces requêtes finies par ce sondage (fins
    /// dont l'interruption s'est perdue). Une file active dont toutes les requêtes sont récentes
    /// n'est pas touchée : son interruption est simplement en route.
    fn watchdog(&self) -> usize {
        let tags = self.driver.tags();
        let now = now_ns();
        let mut rescued = 0;
        for (hwq, q) in tags.queues.iter().enumerate() {
            if q.inflight.load(Relaxed) == 0 {
                continue;
            }
            let stale: Vec<usize> = q
                .slots
                .iter()
                .enumerate()
                .filter(|(_, s)| s.state.load(Acquire) == INFLIGHT && now.saturating_sub(s.start_ns.load(Relaxed)) > WATCHDOG_STALE_NS)
                .map(|(i, _)| i)
                .collect();
            if stale.is_empty() {
                continue;
            }
            self.driver.poll(hwq);
            rescued += stale.iter().filter(|&&i| q.slots[i].state.load(Acquire) == DONE).count();
            let stuck: Vec<usize> = stale.iter().copied().filter(|&i| q.slots[i].state.load(Acquire) == INFLIGHT).collect();
            if !stuck.is_empty() && !STUCK_WARNED.swap(true, Relaxed) {
                error!(
                    "MaiSS : {} : file {} : {} requête(s) sans fin depuis plus de {} ms (étiquettes {:?}, en vol {}) ; {}",
                    self.name,
                    hwq,
                    stuck.len(),
                    WATCHDOG_STALE_NS / 1_000_000,
                    &stuck[..stuck.len().min(8)],
                    q.inflight.load(Relaxed),
                    self.driver.debug_queue(hwq)
                );
            }
        }
        rescued
    }
}

// ─── Registre des unités ────────────────────────────────────────────────────────────────────

static DEVICES: Mutex<Vec<Arc<BlockDevice>>> = Mutex::new(Vec::new());
static WATCHDOG: spin::Once<()> = spin::Once::new();
static WATCHDOG_RESCUES: AtomicU64 = AtomicU64::new(0);
static WATCHDOG_WARNED: AtomicBool = AtomicBool::new(false);
static STUCK_WARNED: AtomicBool = AtomicBool::new(false);

/// Enregistre une unité (visible par [`devices`], surveillée par le chien de garde).
pub fn register(dev: Arc<BlockDevice>) {
    warn!(
        "MaiSS : {} : {} blocs de {} o ({} Mio), {} file(s) matérielle(s), {}",
        dev.name,
        dev.capacity,
        dev.block_size(),
        (dev.capacity << dev.limits.lba_shift) >> 20,
        dev.nr_hw_queues(),
        if dev.driver.irq_driven() { "interruptions MSI-X" } else { "sondage" }
    );
    DEVICES.lock().push(dev);
    WATCHDOG.call_once(|| {
        if let Err(e) = spawn::new_task_builder(watchdog_task, ()).name(String::from("blk_watchdog")).spawn() {
            error!("MaiSS : chien de garde non créé : {}", e);
        }
    });
}

/// Unités enregistrées.
pub fn devices() -> Vec<Arc<BlockDevice>> {
    DEVICES.lock().clone()
}

/// Unité de nom `name`.
pub fn find(name: &str) -> Option<Arc<BlockDevice>> {
    DEVICES.lock().iter().find(|d| d.name == name).cloned()
}

/// Fins rattrapées par le chien de garde (interruptions perdues ou en retard).
pub fn watchdog_rescues() -> u64 {
    WATCHDOG_RESCUES.load(Relaxed)
}

fn watchdog_task(_: ()) {
    loop {
        let _ = sleep::sleep(WATCHDOG_PERIOD);
        let devs = devices();
        for d in &devs {
            let n = d.watchdog();
            if n > 0 {
                WATCHDOG_RESCUES.fetch_add(n as u64, Relaxed);
                if !WATCHDOG_WARNED.swap(true, Relaxed) {
                    warn!("MaiSS : {} : {} fin(s) rattrapée(s) par le chien de garde (interruption perdue ?)", d.name, n);
                }
            }
        }
    }
}

// ─── Adaptateur vers `storage_device` ───────────────────────────────────────────────────────

/// Une unité vue comme [`storage_device::StorageDevice`] (FAT32, partitions, cache de blocs).
pub struct BlkStorage {
    pub dev: Arc<BlockDevice>,
}

impl BlkStorage {
    pub fn new(dev: Arc<BlockDevice>) -> storage_device::StorageDeviceRef {
        Arc::new(spin::Mutex::new(BlkStorage { dev }))
    }
}

impl storage_device::StorageDevice for BlkStorage {
    fn size_in_blocks(&self) -> usize {
        self.dev.capacity as usize
    }
}
impl io::BlockIo for BlkStorage {
    fn block_size(&self) -> usize {
        self.dev.block_size()
    }
}
impl io::KnownLength for BlkStorage {
    fn len(&self) -> usize {
        (self.dev.capacity as usize) << self.dev.limits.lba_shift
    }
}
impl io::BlockReader for BlkStorage {
    fn read_blocks(&mut self, buffer: &mut [u8], block_offset: usize) -> Result<usize, io::IoError> {
        let bs = self.dev.block_size();
        let n = buffer.len() / bs;
        self.dev.read(block_offset as u64, &mut buffer[..n * bs]).map_err(|_| io::IoError::InvalidInput)?;
        Ok(n)
    }
}
impl io::BlockWriter for BlkStorage {
    fn write_blocks(&mut self, buffer: &[u8], block_offset: usize) -> Result<usize, io::IoError> {
        let bs = self.dev.block_size();
        let n = buffer.len() / bs;
        self.dev.write(block_offset as u64, &buffer[..n * bs]).map_err(|_| io::IoError::InvalidInput)?;
        Ok(n)
    }
    fn flush(&mut self) -> Result<(), io::IoError> {
        self.dev.flush().map_err(|_| io::IoError::InvalidInput)
    }
}

/// État détaillé des files d'une unité (diagnostic) : requêtes en vol, âge de la plus ancienne,
/// latence moyenne, et état vu par le pilote.
pub fn debug_state(d: &BlockDevice) -> String {
    let tags = d.driver.tags();
    let now = now_ns();
    let mut out = String::new();
    for (hwq, q) in tags.queues.iter().enumerate() {
        let mut oldest = 0u64;
        let mut states = [0usize; 3];
        for s in q.slots.iter() {
            let st = s.state.load(Acquire) as usize;
            if st < 3 {
                states[st] += 1;
            }
            if st == INFLIGHT as usize {
                oldest = oldest.max(now.saturating_sub(s.start_ns.load(Relaxed)));
            }
        }
        out += &format!(
            "[file {} : en vol {} (libres {}, en vol {}, finies {}), plus ancienne {} us, ewma {} ns ; {}] ",
            hwq,
            q.inflight.load(Relaxed),
            states[0],
            states[1],
            states[2],
            oldest / 1000,
            q.lat_ewma_ns.load(Relaxed),
            d.driver.debug_queue(hwq)
        );
    }
    out
}

/// Résumé lisible d'une unité (pour les commandes et `/proc`).
pub fn describe(d: &BlockDevice) -> String {
    let s = &d.stats;
    format!(
        "{} : {} Mio, {} files, lectures {} ({} Kio), écritures {} ({} Kio), sondées {}, endormies {}, rebonds {}, erreurs {}",
        d.name,
        (d.capacity << d.limits.lba_shift) >> 20,
        d.nr_hw_queues(),
        s.reads.load(Relaxed),
        s.read_bytes.load(Relaxed) >> 10,
        s.writes.load(Relaxed),
        s.write_bytes.load(Relaxed) >> 10,
        s.polled.load(Relaxed),
        s.slept.load(Relaxed),
        s.bounced.load(Relaxed),
        s.errors.load(Relaxed),
    )
}
