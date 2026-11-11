//! MaiGPU G0 : pilote virtio-gpu (VirtIO 1.3, § 5.7), partie 2D.
//!
//! # Files et commandes
//! Deux virtqueues : `controlq` (commandes avec réponse) et `cursorq` (curseur, sans réponse).
//! Chaque commande occupe une **case** préparée à la mise en route (une page DMA : commande en
//! tête, réponse à 2 Kio) ; une commande plus longue (liste de pages d'un *backing*) apporte sa
//! propre [`Dma`]. Une case vit en trois états : libre → en vol → finie, puis libre de nouveau
//! quand son attente la relâche (ou aussitôt pour une commande « sans attente »).
//!
//! Les fins arrivent par l'interruption MSI-X de la file (une par file, plus une pour les
//! changements de configuration) et par le sondage de celui qui attend : les deux vident
//! l'anneau sous le même verrou. Avec `EVENT_IDX`, l'appareil n'est sonné que s'il l'a demandé
//! et n'interrompt que si on l'a demandé.
//!
//! # Attentes
//! Se bloquer, s'inscrire, revérifier (règle de MaiSched), avec un minuteur de secours de
//! 50 ms qui revide l'anneau si une interruption manquait ; au bout de 5 s la commande est
//! déclarée perdue. Sans MSI-X, ou là où l'on ne peut pas dormir (mise en route), sondage.
//!
//! # Affichage
//! [`display::Display`] implémente [`mgi::Scanout`] : la ressource 2D du scanout a pour
//! *backing* les frames du tampon avant de MGI (liste dispersée), et chaque image transfère
//! seulement les rectangles abîmés, puis un `RESOURCE_FLUSH` avec fence. Le curseur est un plan
//! matériel (64 × 64) sur la `cursorq` : bouger la souris ne transfère aucun pixel.

#![no_std]

extern crate alloc;

pub mod display;
pub mod edid;
pub mod proto;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering::*};
use core::time::Duration;

use log::{error, warn};
use pci::PciDevice;
use spin::Once;
use sync_irq::IrqSafeMutex;
use task::TaskRef;
use time::{Instant, Monotonic};
use virtio::pci::PciTransport;
use virtio::queue::{Buf, SplitQueue};
use virtio::{feature as vfeature, Dma, NO_VECTOR};

pub use display::Display;
pub use edid::EdidInfo;
pub use proto::Mode;

/// Type d'appareil virtio du GPU.
pub const DEVICE_TYPE: u16 = 16;

/// Le GPU virtio trouvé à l'énumération (un seul pris en charge).
static GPU: Once<&'static VirtioGpu> = Once::new();

/// Le GPU, s'il a été trouvé et mis en route.
pub fn get() -> Option<&'static VirtioGpu> {
    GPU.get().copied()
}

/// Vrai si `dev` est un GPU virtio moderne.
pub fn matches(dev: &PciDevice) -> bool {
    dev.vendor_id == virtio::VIRTIO_VENDOR && dev.device_id == virtio::modern_device_id(DEVICE_TYPE)
}

// ── Cases de commande ───────────────────────────────────────────────────────────────────

const SLOT_FREE: u8 = 0;
const SLOT_INFLIGHT: u8 = 1;
const SLOT_DONE: u8 = 2;
/// Décalage de la réponse dans la page d'une case.
const RESP_OFF: usize = 2048;
/// Commande la plus longue copiée dans la case.
const CMD_MAX: usize = RESP_OFF;
/// Délai de secours d'une attente (interruption manquée).
const WATCHDOG: Duration = Duration::from_millis(50);
/// Au-delà, la commande est perdue.
const CMD_TIMEOUT: Duration = Duration::from_secs(5);

struct Slot {
    page: Dma,
    state: AtomicU8,
    /// Relâchée par la fin elle-même (personne n'attend sa réponse).
    autofree: AtomicBool,
    /// Génération : un ticket périmé ne peut pas relâcher la case d'un autre.
    gen: AtomicU32,
    /// Octets de réponse attendus.
    resp_len: AtomicU32,
    /// Octets écrits par l'appareil.
    used_len: AtomicU32,
    /// Type de la commande (journal des erreurs).
    kind: AtomicU32,
    /// Instant de soumission (ns monotones), pour la latence.
    t_submit: AtomicU64,
    /// Instant de la fin (ns monotones).
    t_done: AtomicU64,
    waiter: IrqSafeMutex<Option<TaskRef>>,
    /// Commande longue apportée par l'appelant (rendue au relâchement, hors interruption).
    ext: IrqSafeMutex<Option<Dma>>,
}

struct ChanInner {
    q: SplitQueue,
    free: Vec<u16>,
    /// Case de la chaîne commencée à chaque tête de descripteur.
    head_slot: Vec<u16>,
    /// Commandes soumises et pas encore rendues, début de la période d'activité en cours.
    inflight: u32,
    busy_since: u64,
}

/// Une virtqueue et ses cases.
pub struct Channel {
    name: &'static str,
    inner: IrqSafeMutex<ChanInner>,
    slots: Box<[Slot]>,
    /// Descripteurs par commande (2 : commande + réponse ; 1 : curseur).
    descs: usize,
    /// Temps cumulé avec au moins une commande en vol (périodes achevées).
    busy_ns: AtomicU64,
}

/// Désigne une commande en vol.
#[derive(Clone, Copy, Debug)]
pub struct Ticket {
    slot: u16,
    gen: u32,
}

/// Réponse d'une commande attendue.
pub struct Response {
    pub ty: u32,
    pub flags: u32,
    pub fence: u64,
    pub data: Vec<u8>,
    /// Instant de la fin (ns monotones).
    pub done_ns: u64,
}

impl Response {
    pub fn ok(&self) -> bool {
        !proto::resp::is_err(self.ty) && self.ty != 0
    }
}

/// Compteurs du pilote (lus sans verrou).
#[derive(Default)]
pub struct Stats {
    pub commands: AtomicU64,
    pub errors: AtomicU64,
    pub notifies: AtomicU64,
    pub notifies_suppressed: AtomicU64,
    pub interrupts: AtomicU64,
    pub completions: AtomicU64,
    pub lat_total_ns: AtomicU64,
    pub lat_max_ns: AtomicU64,
    pub waits_slept: AtomicU64,
    pub waits_polled: AtomicU64,
    pub transfers: AtomicU64,
    pub transfer_bytes: AtomicU64,
    pub flushes: AtomicU64,
    pub presents: AtomicU64,
    pub present_rects: AtomicU64,
    pub present_wait_ns: AtomicU64,
    pub present_lat_total_ns: AtomicU64,
    pub present_lat_max_ns: AtomicU64,
    pub present_lat_count: AtomicU64,
    pub cursor_updates: AtomicU64,
    pub cursor_moves: AtomicU64,
    pub display_events: AtomicU64,
}

/// Instantané des compteurs.
#[derive(Clone, Copy, Debug, Default)]
pub struct GpuStats {
    pub commands: u64,
    pub errors: u64,
    pub notifies: u64,
    pub notifies_suppressed: u64,
    pub interrupts: u64,
    pub completions: u64,
    pub lat_avg_ns: u64,
    pub lat_max_ns: u64,
    pub waits_slept: u64,
    pub waits_polled: u64,
    pub transfers: u64,
    pub transfer_bytes: u64,
    pub flushes: u64,
    pub presents: u64,
    pub present_rects: u64,
    pub present_wait_ns: u64,
    pub present_lat_avg_ns: u64,
    pub present_lat_max_ns: u64,
    /// Somme et nombre des latences de présentation (pour des moyennes par intervalle).
    pub present_lat_total_ns: u64,
    pub present_lat_count: u64,
    pub cursor_updates: u64,
    pub cursor_moves: u64,
    pub display_events: u64,
    /// Temps où la `controlq` avait au moins une commande en vol (utilisation de l'appareil).
    pub busy_ns: u64,
}

/// Description du GPU pour `gpu_test` et les journaux.
#[derive(Clone, Debug)]
pub struct GpuInfo {
    pub location: String,
    pub features_offered: u64,
    pub features: u64,
    pub num_scanouts: u32,
    pub num_capsets: u32,
    pub modes: Vec<Mode>,
    pub edid: Option<EdidInfo>,
    pub msix: bool,
    pub idt_vectors: Vec<u8>,
    pub event_idx: bool,
    pub ctrl_queue: u16,
    pub cursor_queue: u16,
    pub shm_regions: usize,
    pub display: Option<(u32, u32)>,
}

pub struct VirtioGpu {
    t: PciTransport,
    location: String,
    features_offered: u64,
    features: u64,
    num_scanouts: u32,
    num_capsets: u32,
    ctrl: Channel,
    cursor: Channel,
    /// Interruptions MSI-X en service (sinon : sondage).
    irq: bool,
    next_resource: AtomicU32,
    next_fence: AtomicU64,
    done_fence: AtomicU64,
    modes: spin::Mutex<Vec<Mode>>,
    edid: spin::Mutex<Option<EdidInfo>>,
    events_pending: AtomicBool,
    events_task: Once<TaskRef>,
    display: Once<alloc::sync::Arc<Display>>,
    display_hook: Once<fn(&[Mode])>,
    pub stats: Stats,
}

#[inline]
fn now_ns() -> u64 {
    time::now::<Monotonic>().duration_since(Instant::ZERO).as_nanos() as u64
}

fn stat_max(a: &AtomicU64, v: u64) {
    a.fetch_max(v, Relaxed);
}

// ── Interruptions ───────────────────────────────────────────────────────────────────────

fn irq_config(_data: usize) {
    if let Some(g) = get() {
        g.stats.interrupts.fetch_add(1, Relaxed);
        g.events_pending.store(true, Release);
        if let Some(t) = g.events_task.get() {
            let _ = t.unblock();
        }
    }
}

fn irq_ctrl(_data: usize) {
    if let Some(g) = get() {
        g.stats.interrupts.fetch_add(1, Relaxed);
        g.drain(&g.ctrl);
    }
}

fn irq_cursor(_data: usize) {
    if let Some(g) = get() {
        g.stats.interrupts.fetch_add(1, Relaxed);
        g.drain(&g.cursor);
    }
}

// ── Mise en route ───────────────────────────────────────────────────────────────────────

fn make_channel(name: &'static str, q: SplitQueue, descs: usize) -> Result<Channel, &'static str> {
    let n = (q.size() as usize / descs).max(1);
    let mut slots = Vec::with_capacity(n);
    for _ in 0..n {
        slots.push(Slot {
            page: Dma::new(4096)?,
            state: AtomicU8::new(SLOT_FREE),
            autofree: AtomicBool::new(false),
            gen: AtomicU32::new(0),
            resp_len: AtomicU32::new(0),
            used_len: AtomicU32::new(0),
            kind: AtomicU32::new(0),
            t_submit: AtomicU64::new(0),
            t_done: AtomicU64::new(0),
            waiter: IrqSafeMutex::new(None),
            ext: IrqSafeMutex::new(None),
        });
    }
    let size = q.size() as usize;
    Ok(Channel {
        name,
        inner: IrqSafeMutex::new(ChanInner {
            q,
            free: (0..n as u16).rev().collect(),
            head_slot: alloc::vec![u16::MAX; size],
            inflight: 0,
            busy_since: 0,
        }),
        slots: slots.into_boxed_slice(),
        descs,
        busy_ns: AtomicU64::new(0),
    })
}

/// Met en route le GPU virtio `dev` : transport, fonctions, files, MSI-X, puis interroge les
/// affichages et l'EDID. Un seul GPU est pris en charge (les suivants sont ignorés).
pub fn probe(dev: &PciDevice) -> Result<&'static VirtioGpu, &'static str> {
    if let Some(g) = get() {
        return Ok(g);
    }
    let mut t = PciTransport::new(dev)?;
    // G0 : 2D. VIRGL, RESOURCE_BLOB et CONTEXT_INIT ne sont pas acceptés (jalons G1 et G3).
    let wanted = proto::feature::EDID | vfeature::RING_EVENT_IDX;
    let (offered, features) = t.negotiate(wanted)?;
    let event_idx = features & vfeature::RING_EVENT_IDX != 0;
    let num_scanouts = t.device_read32(8);
    let num_capsets = t.device_read32(12);

    // MSI-X : entrée 0 configuration, 1 controlq, 2 cursorq.
    let handlers: [(interrupts::MsiCallback, usize); 3] = [(irq_config, 0), (irq_ctrl, 1), (irq_cursor, 2)];
    let irq = match t.enable_msix(dev, &handlers) {
        Ok(_) => {
            if !t.set_config_vector(0) {
                warn!("virtio-gpu : vecteur de configuration refusé (changements d'affichage sondés)");
            }
            true
        }
        Err(e) => {
            warn!("virtio-gpu : pas de MSI-X ({}) : fins par sondage", e);
            false
        }
    };
    let (vc, vk) = if irq { (1, 2) } else { (NO_VECTOR, NO_VECTOR) };
    let ctrl_q = t.setup_queue(0, 256, event_idx, vc)?;
    let cursor_q = t.setup_queue(1, 16, event_idx, vk)?;
    let (ctrl_size, cursor_size) = (ctrl_q.size(), cursor_q.size());
    let ctrl = make_channel("controlq", ctrl_q, 2)?;
    let cursor = make_channel("cursorq", cursor_q, 1)?;
    t.driver_ok();
    if irq {
        t.msix_unmask(dev)?;
    }
    let gpu: &'static VirtioGpu = Box::leak(Box::new(VirtioGpu {
        t,
        location: format!("{}", dev.location),
        features_offered: offered,
        features,
        num_scanouts,
        num_capsets,
        ctrl,
        cursor,
        irq,
        next_resource: AtomicU32::new(1),
        next_fence: AtomicU64::new(1),
        done_fence: AtomicU64::new(0),
        modes: spin::Mutex::new(Vec::new()),
        edid: spin::Mutex::new(None),
        events_pending: AtomicBool::new(false),
        events_task: Once::new(),
        display: Once::new(),
        display_hook: Once::new(),
        stats: Stats::default(),
    }));
    GPU.call_once(|| gpu);
    for ch in [&gpu.ctrl, &gpu.cursor] {
        if irq {
            let mut g = ch.inner.lock();
            let _ = g.q.enable_interrupts();
        } else {
            ch.inner.lock().q.disable_interrupts();
        }
    }

    gpu.refresh_modes()?;
    let modes = gpu.modes();
    let edid = gpu.edid().clone();
    let pref = modes.first().map(|m| (m.rect.width, m.rect.height)).unwrap_or((0, 0));
    warn!(
        "virtio-gpu : {} : {} scanout(s), mode prefere {}x{}, EDID {}, fonctions offertes {:#x} retenues {:#x}, files {}+{}, {}, EVENT_IDX {}",
        gpu.location,
        num_scanouts,
        pref.0,
        pref.1,
        edid.as_ref().map_or(String::from("absent"), |e| format!("{} « {} » (produit {:#06x})", e.manufacturer_str(), e.name, e.product)),
        offered,
        features,
        ctrl_size,
        cursor_size,
        if irq { format!("MSI-X (vecteurs {:?})", gpu.t.idt_vectors()) } else { String::from("sondage") },
        if event_idx { "oui" } else { "non" },
    );

    // Tâche des changements de configuration (branchement d'écran, redimensionnement).
    match spawn::new_task_builder(events_loop, gpu).name(String::from("virtio_gpu_events")).spawn() {
        Ok(_) => {}
        Err(e) => warn!("virtio-gpu : tâche des événements non lancée : {}", e),
    }
    Ok(gpu)
}

fn events_loop(gpu: &'static VirtioGpu) -> Result<(), &'static str> {
    let me = task::get_my_current_task().ok_or("virtio-gpu : pas de tâche courante")?;
    gpu.events_task.call_once(|| me.clone());
    loop {
        // Se bloquer, puis revérifier.
        let _ = me.block();
        let pending = gpu.events_pending.swap(false, AcqRel) || (!gpu.irq && gpu.t.read_isr() & 2 != 0);
        if pending {
            let _ = me.unblock();
        } else {
            if !gpu.irq {
                sleep::wake_at(me.clone(), time::now::<Monotonic>() + Duration::from_secs(1));
            }
            task::schedule();
            continue;
        }
        let ev = gpu.t.device_read32(0);
        if ev == 0 {
            continue;
        }
        gpu.t.device_write32(4, ev);
        if ev & proto::EVENT_DISPLAY != 0 {
            gpu.stats.display_events.fetch_add(1, Relaxed);
            match gpu.refresh_modes() {
                Ok(()) => {
                    let modes = gpu.modes();
                    let m = modes.first().map(|m| (m.rect.width, m.rect.height, m.enabled)).unwrap_or((0, 0, false));
                    warn!("virtio-gpu : configuration d'affichage changee : scanout 0 {}x{} ({})", m.0, m.1, if m.2 { "actif" } else { "inactif" });
                    if let Some(h) = gpu.display_hook.get() {
                        h(&modes);
                    }
                }
                Err(e) => warn!("virtio-gpu : relecture des modes impossible : {}", e),
            }
        }
    }
}

impl VirtioGpu {
    // ── Soumission ──────────────────────────────────────────────────────────────────────

    fn acquire_slot(&self, ch: &Channel) -> Result<u16, &'static str> {
        let start = now_ns();
        loop {
            if let Some(s) = ch.inner.lock().free.pop() {
                return Ok(s);
            }
            // Plus de case : les fins en attente en rendent peut-être (commandes sans attente).
            self.drain(ch);
            if let Some(s) = ch.inner.lock().free.pop() {
                return Ok(s);
            }
            if now_ns() - start > CMD_TIMEOUT.as_nanos() as u64 {
                error!("virtio-gpu : {} : aucune case libre depuis 5 s (appareil bloqué ?)", ch.name);
                return Err("virtio-gpu : file saturée");
            }
            core::hint::spin_loop();
        }
    }

    /// Envoie une commande. `cmd` est copiée dans la case, sauf si `ext` (commande longue
    /// déjà écrite par l'appelant, de longueur `cmd_len`) est donnée. `resp_len` : octets de
    /// réponse (0 : aucune, pour la `cursorq`). `autofree` : personne n'attendra la réponse
    /// (une erreur est seulement comptée et journalisée). `kick` : sonner l'appareil si
    /// besoin ; une rafale ne sonne qu'à sa dernière commande (les précédentes sont couvertes
    /// par l'intervalle d'indices de `kick_needed`).
    fn submit_raw(&self, ch: &Channel, cmd: &[u8], ext: Option<(Dma, usize)>, resp_len: usize, autofree: bool, kick: bool) -> Result<Ticket, &'static str> {
        if resp_len > 4096 - RESP_OFF || (ext.is_none() && cmd.len() > CMD_MAX) {
            return Err("virtio-gpu : commande trop longue pour une case");
        }
        let kind = if cmd.len() >= 4 { u32::from_le_bytes([cmd[0], cmd[1], cmd[2], cmd[3]]) } else { 0 };
        let s = self.acquire_slot(ch)?;
        let slot = &ch.slots[s as usize];
        let (cmd_phys, cmd_len) = match &ext {
            Some((d, len)) => (d.phys(), *len),
            None => {
                slot.page.write(0, cmd);
                (slot.page.phys(), cmd.len())
            }
        };
        if resp_len > 0 {
            slot.page.write(RESP_OFF, &[0u8; proto::HDR_LEN]);
        }
        *slot.ext.lock() = ext.map(|(d, _)| d);
        slot.resp_len.store(resp_len as u32, Relaxed);
        slot.used_len.store(0, Relaxed);
        slot.kind.store(kind, Relaxed);
        slot.autofree.store(autofree, Relaxed);
        slot.t_submit.store(now_ns(), Relaxed);
        let gen = slot.gen.fetch_add(1, Relaxed).wrapping_add(1);
        slot.state.store(SLOT_INFLIGHT, Release);

        let mut bufs = [Buf { phys: cmd_phys, len: cmd_len as u32, write: false }, Buf { phys: 0, len: 0, write: true }];
        let n = if resp_len > 0 {
            bufs[1] = Buf { phys: slot.page.phys() + RESP_OFF as u64, len: resp_len as u32, write: true };
            2
        } else {
            1
        };
        debug_assert!(n <= ch.descs);
        {
            let mut g = ch.inner.lock();
            let head = match g.q.add(&bufs[..n]) {
                Ok(h) => h,
                Err(_) => {
                    // Impossible : il y a autant de descripteurs que de cases × `descs`.
                    slot.state.store(SLOT_FREE, Release);
                    g.free.push(s);
                    return Err("virtio-gpu : plus de descripteurs");
                }
            };
            g.head_slot[head as usize] = s;
            if g.inflight == 0 {
                g.busy_since = now_ns();
            }
            g.inflight += 1;
            if !kick {
                // Sonné par la dernière commande de la rafale.
            } else if g.q.kick_needed() {
                g.q.notify();
                self.stats.notifies.fetch_add(1, Relaxed);
            } else {
                self.stats.notifies_suppressed.fetch_add(1, Relaxed);
            }
        }
        self.stats.commands.fetch_add(1, Relaxed);
        Ok(Ticket { slot: s, gen })
    }

    /// Vide les fins de `ch` (interruption ou sondage). Rend leur nombre.
    fn drain(&self, ch: &Channel) -> usize {
        let mut total = 0;
        loop {
            let mut done = [0u16; 32];
            let mut n = 0;
            {
                let mut g = ch.inner.lock();
                loop {
                    while n < done.len() {
                        match g.q.pop_used() {
                            Some((head, len)) => {
                                let s = g.head_slot[head as usize];
                                g.head_slot[head as usize] = u16::MAX;
                                if (s as usize) < ch.slots.len() {
                                    ch.slots[s as usize].used_len.store(len, Relaxed);
                                    done[n] = s;
                                    n += 1;
                                    g.inflight -= 1;
                                    if g.inflight == 0 {
                                        let busy = now_ns().saturating_sub(g.busy_since);
                                        ch.busy_ns.fetch_add(busy, Relaxed);
                                    }
                                }
                            }
                            None => break,
                        }
                    }
                    // Réarmer, puis revérifier : une fin arrivée entre-temps n'interromprait pas.
                    if n < done.len() && self.irq && g.q.enable_interrupts() {
                        continue;
                    }
                    break;
                }
            }
            for &s in &done[..n] {
                self.complete(ch, s);
            }
            total += n;
            if n < done.len() {
                return total;
            }
        }
    }

    fn complete(&self, ch: &Channel, s: u16) {
        let slot = &ch.slots[s as usize];
        let t = now_ns();
        slot.t_done.store(t, Relaxed);
        let lat = t.saturating_sub(slot.t_submit.load(Relaxed));
        self.stats.completions.fetch_add(1, Relaxed);
        self.stats.lat_total_ns.fetch_add(lat, Relaxed);
        stat_max(&self.stats.lat_max_ns, lat);
        if slot.resp_len.load(Relaxed) > 0 {
            let mut h = [0u8; proto::HDR_LEN];
            slot.page.read(RESP_OFF, &mut h);
            let (ty, flags, fence) = proto::parse_hdr(&h);
            if flags & proto::FLAG_FENCE != 0 {
                self.done_fence.fetch_max(fence, AcqRel);
            }
            if proto::resp::is_err(ty) || ty == 0 {
                self.stats.errors.fetch_add(1, Relaxed);
                if slot.autofree.load(Relaxed) {
                    warn!(
                        "virtio-gpu : commande {:#x} : {} ({:#x})",
                        slot.kind.load(Relaxed),
                        proto::resp::name(ty),
                        ty
                    );
                }
            }
        }
        if slot.autofree.load(Relaxed) {
            slot.state.store(SLOT_FREE, Release);
            ch.inner.lock().free.push(s);
        } else {
            slot.state.store(SLOT_DONE, Release);
            if let Some(t) = slot.waiter.lock().take() {
                let _ = t.unblock();
            }
        }
    }

    fn can_sleep(&self) -> bool {
        self.irq && irq_safety::interrupts_enabled() && preemption::preemption_enabled()
    }

    /// Vrai si la commande `t` est finie (sans attendre).
    fn is_done(&self, ch: &Channel, t: Ticket) -> bool {
        let slot = &ch.slots[t.slot as usize];
        slot.gen.load(Relaxed) != t.gen || slot.state.load(Acquire) != SLOT_INFLIGHT
    }

    /// Attend la fin de `t`, rend sa réponse et relâche la case.
    fn wait(&self, ch: &Channel, t: Ticket) -> Result<Response, &'static str> {
        let slot = &ch.slots[t.slot as usize];
        if slot.gen.load(Relaxed) != t.gen {
            return Err("virtio-gpu : ticket périmé");
        }
        let start = time::now::<Monotonic>();
        let mut slept = false;
        let mut armed = false;
        while slot.state.load(Acquire) != SLOT_DONE {
            self.drain(ch);
            if slot.state.load(Acquire) == SLOT_DONE {
                break;
            }
            let now = time::now::<Monotonic>();
            if now.duration_since(start) > CMD_TIMEOUT {
                error!(
                    "virtio-gpu : {} : commande {:#x} sans réponse depuis 5 s",
                    ch.name,
                    slot.kind.load(Relaxed)
                );
                // La case reviendra d'elle-même si l'appareil finit par répondre.
                slot.autofree.store(true, Release);
                if slot.state.load(Acquire) == SLOT_DONE {
                    break;
                }
                return Err("virtio-gpu : délai dépassé");
            }
            match task::get_my_current_task() {
                Some(me) if self.can_sleep() => {
                    slept = true;
                    *slot.waiter.lock() = Some(me.clone());
                    if me.block().is_err() {
                        slot.waiter.lock().take();
                        core::hint::spin_loop();
                        continue;
                    }
                    if slot.state.load(Acquire) == SLOT_DONE {
                        let _ = me.unblock();
                        slot.waiter.lock().take();
                        break;
                    }
                    if !armed {
                        // Secours si l'interruption manquait ; un réveil de trop est sans danger
                        // (toutes les attentes revérifient leur condition).
                        sleep::wake_at(me.clone(), now + WATCHDOG);
                        armed = true;
                    }
                    task::schedule();
                    slot.waiter.lock().take();
                }
                _ => core::hint::spin_loop(),
            }
        }
        if slept {
            self.stats.waits_slept.fetch_add(1, Relaxed);
        } else {
            self.stats.waits_polled.fetch_add(1, Relaxed);
        }
        let resp_len = slot.resp_len.load(Relaxed) as usize;
        let n = resp_len.min(slot.used_len.load(Relaxed) as usize);
        let mut data = alloc::vec![0u8; n];
        slot.page.read(RESP_OFF, &mut data);
        let (ty, flags, fence) = proto::parse_hdr(&data);
        let ext = slot.ext.lock().take();
        drop(ext);
        slot.state.store(SLOT_FREE, Release);
        ch.inner.lock().free.push(t.slot);
        Ok(Response { ty, flags, fence, data, done_ns: slot.t_done.load(Relaxed) })
    }

    /// Commande de la `controlq` attendue ; une réponse d'erreur devient `Err`.
    pub fn command(&self, cmd: &[u8], resp_len: usize) -> Result<Response, &'static str> {
        let t = self.submit_raw(&self.ctrl, cmd, None, resp_len.max(proto::HDR_LEN), false, true)?;
        let r = self.wait(&self.ctrl, t)?;
        if !r.ok() {
            let kind = u32::from_le_bytes([cmd[0], cmd[1], cmd[2], cmd[3]]);
            error!("virtio-gpu : commande {:#x} : {} ({:#x})", kind, proto::resp::name(r.ty), r.ty);
            return Err("virtio-gpu : commande refusée par l'appareil");
        }
        Ok(r)
    }

    /// Commande longue (déjà écrite dans `d`, `len` octets), attendue.
    fn command_ext(&self, d: Dma, len: usize) -> Result<Response, &'static str> {
        let mut h = [0u8; 4];
        d.read(0, &mut h);
        let t = self.submit_raw(&self.ctrl, &[], Some((d, len)), proto::HDR_LEN, false, true)?;
        let r = self.wait(&self.ctrl, t)?;
        if !r.ok() {
            error!("virtio-gpu : commande {:#x} : {} ({:#x})", u32::from_le_bytes(h), proto::resp::name(r.ty), r.ty);
            return Err("virtio-gpu : commande refusée par l'appareil");
        }
        Ok(r)
    }

    /// Commande de la `controlq` sans attente : rend son ticket. `kick` : voir `submit_raw`.
    fn command_async(&self, cmd: &[u8], kick: bool) -> Result<Ticket, &'static str> {
        self.submit_raw(&self.ctrl, cmd, None, proto::HDR_LEN, false, kick)
    }

    /// Commande de la `cursorq` (sans réponse, relâchée à sa fin).
    fn cursor_command(&self, cmd: &[u8]) -> Result<(), &'static str> {
        self.submit_raw(&self.cursor, cmd, None, 0, true, true).map(|_| ())
    }

    // ── Ressources ──────────────────────────────────────────────────────────────────────

    fn alloc_resource_id(&self) -> u32 {
        self.next_resource.fetch_add(1, Relaxed)
    }

    fn alloc_fence(&self) -> u64 {
        self.next_fence.fetch_add(1, Relaxed)
    }

    /// Lie à la ressource `id` les segments physiques `entries` (adresse, longueur).
    fn attach_backing(&self, id: u32, entries: &[(u64, u32)]) -> Result<(), &'static str> {
        let cmd = proto::attach_backing(id, entries);
        let d = Dma::new(cmd.len())?;
        d.write(0, &cmd);
        self.command_ext(d, cmd.len()).map(|_| ())
    }

    /// Relit les modes (`GET_DISPLAY_INFO`) et l'EDID du scanout 0.
    fn refresh_modes(&self) -> Result<(), &'static str> {
        let r = self.command(&proto::get_display_info(), proto::DISPLAY_INFO_LEN)?;
        *self.modes.lock() = proto::parse_display_info(&r.data);
        if self.features & proto::feature::EDID != 0 {
            match self.command(&proto::get_edid(0), proto::EDID_RESP_LEN) {
                Ok(r) if r.data.len() >= proto::HDR_LEN + 8 => {
                    let size = u32::from_le_bytes([r.data[24], r.data[25], r.data[26], r.data[27]]) as usize;
                    let blob = &r.data[proto::HDR_LEN + 8..];
                    *self.edid.lock() = edid::parse(&blob[..size.min(blob.len())]);
                }
                Ok(_) => {}
                Err(e) => warn!("virtio-gpu : EDID illisible : {}", e),
            }
        }
        Ok(())
    }

    // ── Accès publics ───────────────────────────────────────────────────────────────────

    /// Modes annoncés (scanouts actifs).
    pub fn modes(&self) -> Vec<Mode> {
        self.modes.lock().clone()
    }

    pub fn edid(&self) -> spin::MutexGuard<'_, Option<EdidInfo>> {
        self.edid.lock()
    }

    /// Dernière fence achevée.
    pub fn completed_fence(&self) -> u64 {
        self.done_fence.load(Acquire)
    }

    /// Fonction appelée (par la tâche des événements) quand la configuration des affichages
    /// change. Une seule, posée une fois.
    pub fn set_display_hook(&self, f: fn(&[Mode])) {
        self.display_hook.call_once(|| f);
    }

    /// Crée (une fois) le scanout 0 de `width × height` pixels : ressource 2D, *backing*,
    /// `SET_SCANOUT`, curseur matériel.
    pub fn create_display(&'static self, width: u32, height: u32) -> Result<alloc::sync::Arc<Display>, &'static str> {
        if let Some(d) = self.display.get() {
            return Ok(d.clone());
        }
        let d = alloc::sync::Arc::new(Display::new(self, 0, width, height)?);
        Ok(self.display.call_once(|| d).clone())
    }

    pub fn info(&self) -> GpuInfo {
        let display = self.display.get().map(|d| d.dims());
        GpuInfo {
            location: self.location.clone(),
            features_offered: self.features_offered,
            features: self.features,
            num_scanouts: self.num_scanouts,
            num_capsets: self.num_capsets,
            modes: self.modes(),
            edid: self.edid().clone(),
            msix: self.irq,
            idt_vectors: self.t.idt_vectors(),
            event_idx: self.features & vfeature::RING_EVENT_IDX != 0,
            ctrl_queue: self.ctrl.inner.lock().q.size(),
            cursor_queue: self.cursor.inner.lock().q.size(),
            shm_regions: self.t.shm.len(),
            display,
        }
    }

    pub fn stats(&self) -> GpuStats {
        let s = &self.stats;
        let l = |a: &AtomicU64| a.load(Relaxed);
        let completions = l(&s.completions).max(1);
        let pl = l(&s.present_lat_count).max(1);
        GpuStats {
            commands: l(&s.commands),
            errors: l(&s.errors),
            notifies: l(&s.notifies),
            notifies_suppressed: l(&s.notifies_suppressed),
            interrupts: l(&s.interrupts),
            completions: l(&s.completions),
            lat_avg_ns: l(&s.lat_total_ns) / completions,
            lat_max_ns: l(&s.lat_max_ns),
            waits_slept: l(&s.waits_slept),
            waits_polled: l(&s.waits_polled),
            transfers: l(&s.transfers),
            transfer_bytes: l(&s.transfer_bytes),
            flushes: l(&s.flushes),
            presents: l(&s.presents),
            present_rects: l(&s.present_rects),
            present_wait_ns: l(&s.present_wait_ns),
            present_lat_avg_ns: l(&s.present_lat_total_ns) / pl,
            present_lat_max_ns: l(&s.present_lat_max_ns),
            present_lat_total_ns: l(&s.present_lat_total_ns),
            present_lat_count: l(&s.present_lat_count),
            cursor_updates: l(&s.cursor_updates),
            cursor_moves: l(&s.cursor_moves),
            display_events: l(&s.display_events),
            busy_ns: self.busy_ns(),
        }
    }

    /// Temps d'activité de la `controlq`, période en cours comprise.
    pub fn busy_ns(&self) -> u64 {
        let g = self.ctrl.inner.lock();
        let open = if g.inflight > 0 { now_ns().saturating_sub(g.busy_since) } else { 0 };
        self.ctrl.busy_ns.load(Relaxed) + open
    }

    /// Fonctions retenues, lisibles.
    pub fn feature_names(&self) -> String {
        let mut v: Vec<&str> = Vec::new();
        for (bit, name) in [
            (proto::feature::VIRGL, "VIRGL"),
            (proto::feature::EDID, "EDID"),
            (proto::feature::RESOURCE_UUID, "RESOURCE_UUID"),
            (proto::feature::RESOURCE_BLOB, "RESOURCE_BLOB"),
            (proto::feature::CONTEXT_INIT, "CONTEXT_INIT"),
        ] {
            if self.features & bit != 0 {
                v.push(name);
            }
        }
        if self.features & vfeature::RING_EVENT_IDX != 0 {
            v.push("EVENT_IDX");
        }
        if self.irq {
            v.push("MSI-X");
        }
        v.join(", ")
    }
}

/// Rend `u32` lisible : `"OK"` ou le nom de l'erreur (pour les applis de test).
pub fn response_name(ty: u32) -> &'static str {
    proto::resp::name(ty)
}

#[allow(dead_code)]
fn _assert_send_sync() {
    fn f<T: Send + Sync>() {}
    f::<VirtioGpu>();
}
