//! Pilote virtio-input (VirtIO 1.3, § 5.8) pour les pointeurs : tablette (`virtio-tablet-pci`,
//! position absolue) et souris (`virtio-mouse-pci`, déplacements relatifs).
//!
//! Pourquoi une tablette : avec une souris relative (PS/2) et un curseur **matériel**
//! (MaiGPU), l'affichage de QEMU recale le pointeur de l'hôte sur le curseur de l'invité à
//! chaque `MOVE_CURSOR` (`dpy_mouse_set`) ; fenêtre mise à l'échelle, le recalage ignore
//! l'échelle et le pointeur fuit hors de la fenêtre. Un pointeur absolu supprime le recalage
//! et la capture de la souris : le curseur de l'hôte prend l'image de celui de MaiOS.
//!
//! * Configuration (§ 5.8.4) : `select`/`subsel` écrits octet par octet, puis `size` et
//!   l'union lus d'une seule génération : nom (`ID_NAME`), types d'événements (`EV_BITS`),
//!   étendue des axes (`ABS_INFO`).
//! * `eventq` : tampons de 8 octets (`virtio_input_event`, codes `evdev` de Linux) tous
//!   offerts à l'appareil et rendus aussitôt lus. Les événements s'accumulent jusqu'à
//!   `SYN_REPORT`, qui produit **un** `MouseEvent` (position, boutons, molette) dans la file
//!   de la souris ; `SYN_DROPPED` jette le paquet en cours (sémantique d'`evdev`).
//! * MSI-X : entrée 0 configuration (rien à faire), 1 `eventq`, vidée dans l'interruption
//!   (comme le PS/2, qui pousse dans la même file) ; sans MSI-X, une tâche sonde toutes les
//!   8 ms.
//! * Les claviers virtio (`virtio-keyboard-pci`) sont reconnus et laissés de côté : le
//!   clavier PS/2 reste la source des touches.

#![no_std]

extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use core::time::Duration;

use event_types::Event;
use log::warn;
use mouse_data::{MouseButtons, MouseEvent, MouseMovementRelative, MousePositionAbsolute};
use mpmc::Queue;
use pci::PciDevice;
use spin::Once;
use sync_irq::IrqSafeMutex;
use virtio::pci::PciTransport;
use virtio::queue::{Buf, SplitQueue};
use virtio::{Dma, NO_VECTOR};

/// Type d'appareil virtio (§ 5.8).
pub const DEVICE_TYPE: u16 = 18;

/// Configuration (§ 5.8.4).
mod cfg {
    pub const SELECT: usize = 0;
    pub const SUBSEL: usize = 1;
    pub const SIZE: usize = 2;
    pub const UNION: usize = 8;
    pub const ID_NAME: u8 = 0x01;
    pub const EV_BITS: u8 = 0x11;
    pub const ABS_INFO: u8 = 0x12;
}

/// Codes `evdev` (`linux/input-event-codes.h`).
mod ev {
    pub const SYN: u16 = 0x00;
    pub const KEY: u16 = 0x01;
    pub const REL: u16 = 0x02;
    pub const ABS: u16 = 0x03;
    pub const SYN_REPORT: u16 = 0;
    pub const SYN_DROPPED: u16 = 3;
    pub const REL_X: u16 = 0x00;
    pub const REL_Y: u16 = 0x01;
    pub const REL_WHEEL: u16 = 0x08;
    pub const ABS_X: u16 = 0x00;
    pub const ABS_Y: u16 = 0x01;
    pub const BTN_LEFT: u16 = 0x110;
    pub const BTN_RIGHT: u16 = 0x111;
    pub const BTN_MIDDLE: u16 = 0x112;
    pub const BTN_SIDE: u16 = 0x113;
    pub const BTN_EXTRA: u16 = 0x114;
    pub const BTN_TOUCH: u16 = 0x14a;
    pub const KEY_A: u16 = 30;
}

/// Taille d'un `virtio_input_event`.
const EVENT_SIZE: usize = 8;
/// Tampons offerts au plus (QEMU en propose 64).
const QUEUE_MAX: u16 = 64;
/// Période de sondage sans MSI-X.
const POLL_PERIOD: Duration = Duration::from_millis(8);

/// Genre de pointeur.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Position absolue : étendue des axes `[min, max]`.
    Tablet { x: (i32, i32), y: (i32, i32) },
    /// Déplacements relatifs.
    Mouse,
}

/// Description d'un appareil (pour les journaux et les tests).
#[derive(Clone, Debug)]
pub struct InputInfo {
    pub name: String,
    pub location: String,
    pub kind: Kind,
    pub msix: bool,
    pub queue_size: u16,
}

/// Compteurs.
#[derive(Default)]
struct Stats {
    events: AtomicU64,
    reports: AtomicU64,
    dropped: AtomicU64,
    interrupts: AtomicU64,
}

/// Instantané des compteurs.
#[derive(Clone, Copy, Debug, Default)]
pub struct InputStats {
    /// Événements `evdev` lus.
    pub events: u64,
    /// Paquets (`SYN_REPORT`) livrés à MaiCompositor.
    pub reports: u64,
    /// Paquets jetés (`SYN_DROPPED`, file pleine).
    pub dropped: u64,
    pub interrupts: u64,
}

/// État accumulé jusqu'au `SYN_REPORT`.
#[derive(Default)]
struct Packet {
    buttons: u8,
    x: i32,
    y: i32,
    abs_seen: bool,
    dx: i32,
    dy: i32,
    wheel: i32,
    /// `SYN_DROPPED` reçu : tout jusqu'au prochain `SYN_REPORT` est ignoré.
    dropping: bool,
}

struct EventQ {
    q: SplitQueue,
    /// Tampon `i` (8 octets) en `bufs[i * 8]`.
    bufs: Dma,
    /// Tampon publié sous chaque tête de descripteur.
    head_buf: Vec<u16>,
    packet: Packet,
}

pub struct VirtioInput {
    t: PciTransport,
    info: InputInfo,
    eventq: IrqSafeMutex<EventQ>,
    queue: Queue<Event>,
    stats: Stats,
}

/// Appareils en service.
static DEVICES: IrqSafeMutex<Vec<&'static VirtioInput>> = IrqSafeMutex::new(Vec::new());

/// Vrai si `dev` est un appareil virtio-input moderne.
pub fn matches(dev: &PciDevice) -> bool {
    dev.vendor_id == virtio::VIRTIO_VENDOR && dev.device_id == virtio::modern_device_id(DEVICE_TYPE)
}

/// Appareils en service.
pub fn devices() -> Vec<&'static VirtioInput> {
    DEVICES.lock().clone()
}

/// Vrai si un pointeur absolu est en service.
pub fn has_tablet() -> bool {
    DEVICES.lock().iter().any(|d| matches!(d.info.kind, Kind::Tablet { .. }))
}

/// `select`/`subsel` puis lit `size` et l'union.
fn query(t: &PciTransport, select: u8, subsel: u8) -> Vec<u8> {
    t.device_write8(cfg::SELECT, select);
    t.device_write8(cfg::SUBSEL, subsel);
    let size = (t.device_read8(cfg::SIZE) as usize).min(128);
    let mut v = alloc::vec![0u8; size];
    t.device_read_bytes(cfg::UNION, &mut v);
    v
}

fn bit(bitmap: &[u8], n: u16) -> bool {
    bitmap.get(n as usize / 8).map_or(false, |b| b & (1 << (n % 8)) != 0)
}

/// Étendue `[min, max]` de l'axe `axis` (`virtio_input_absinfo`).
fn abs_range(t: &PciTransport, axis: u16) -> Option<(i32, i32)> {
    let v = query(t, cfg::ABS_INFO, axis as u8);
    if v.len() < 8 {
        return None;
    }
    let min = i32::from_le_bytes([v[0], v[1], v[2], v[3]]);
    let max = i32::from_le_bytes([v[4], v[5], v[6], v[7]]);
    (max > min).then_some((min, max))
}

fn irq_config(_data: usize) {}

fn irq_events(data: usize) {
    // SAFETY : `data` est l'adresse d'un `Once` fuité par `probe` (vit pour toujours).
    let slot = unsafe { &*(data as *const Once<&'static VirtioInput>) };
    if let Some(d) = slot.get() {
        d.stats.interrupts.fetch_add(1, Relaxed);
        d.drain();
    }
}

/// Met en route le pointeur virtio `dev`, dont les paquets iront dans `queue` (la file de la
/// souris, lue par MaiCompositor). Un clavier virtio est refusé (`Err`).
pub fn probe(dev: &PciDevice, queue: Queue<Event>) -> Result<&'static VirtioInput, &'static str> {
    let mut t = PciTransport::new(dev)?;
    let (_offered, features) = t.negotiate(virtio::feature::RING_EVENT_IDX)?;
    let event_idx = features & virtio::feature::RING_EVENT_IDX != 0;

    let name = {
        let raw = query(&t, cfg::ID_NAME, 0);
        String::from_utf8_lossy(&raw).trim_end_matches('\0').into()
    };
    let abs_bits = query(&t, cfg::EV_BITS, ev::ABS as u8);
    let rel_bits = query(&t, cfg::EV_BITS, ev::REL as u8);
    let key_bits = query(&t, cfg::EV_BITS, ev::KEY as u8);
    let kind = if bit(&abs_bits, ev::ABS_X) && bit(&abs_bits, ev::ABS_Y) {
        match (abs_range(&t, ev::ABS_X), abs_range(&t, ev::ABS_Y)) {
            (Some(x), Some(y)) => Kind::Tablet { x, y },
            _ => {
                t.fail();
                return Err("virtio-input : axes absolus sans étendue");
            }
        }
    } else if bit(&rel_bits, ev::REL_X) && bit(&rel_bits, ev::REL_Y) {
        Kind::Mouse
    } else {
        t.fail();
        return Err(if bit(&key_bits, ev::KEY_A) {
            "virtio-input : clavier (laissé au clavier PS/2)"
        } else {
            "virtio-input : ni pointeur ni souris"
        });
    };

    // MSI-X : entrée 0 configuration, 1 eventq. L'appareil n'existe pas encore : l'argument
    // est un emplacement fuité, rempli plus bas.
    let slot: &'static Once<&'static VirtioInput> = Box::leak(Box::new(Once::new()));
    let handlers: [(interrupts::MsiCallback, usize); 2] =
        [(irq_config, 0), (irq_events, slot as *const _ as usize)];
    let msix = match t.enable_msix(dev, &handlers) {
        Ok(_) => {
            let _ = t.set_config_vector(0);
            true
        }
        Err(e) => {
            warn!("virtio-input : pas de MSI-X ({}) : sondage toutes les {} ms", e, POLL_PERIOD.as_millis());
            false
        }
    };
    let mut q = t.setup_queue(0, QUEUE_MAX, event_idx, if msix { 1 } else { NO_VECTOR })?;
    let n = q.size();
    let bufs = Dma::new(n as usize * EVENT_SIZE)?;
    let mut head_buf = alloc::vec![u16::MAX; n as usize];
    for i in 0..n {
        let b = Buf { phys: bufs.phys() + (i as usize * EVENT_SIZE) as u64, len: EVENT_SIZE as u32, write: true };
        let head = q.add(&[b]).map_err(|_| "virtio-input : file pleine à l'amorçage")?;
        head_buf[head as usize] = i;
    }
    // La statusq (DEL du clavier) n'est pas utilisée par un pointeur.
    t.driver_ok();
    if msix {
        t.msix_unmask(dev)?;
    }
    if q.kick_needed() {
        q.notify();
    }
    let info = InputInfo { name, location: format!("{}", dev.location), kind, msix, queue_size: n };
    let d: &'static VirtioInput = Box::leak(Box::new(VirtioInput {
        t,
        info,
        eventq: IrqSafeMutex::new(EventQ { q, bufs, head_buf, packet: Packet::default() }),
        queue,
        stats: Stats::default(),
    }));
    slot.call_once(|| d);
    DEVICES.lock().push(d);
    {
        let mut g = d.eventq.lock();
        if msix {
            let _ = g.q.enable_interrupts();
        } else {
            g.q.disable_interrupts();
        }
    }
    // Ce qui est arrivé avant que l'interruption soit branchée.
    d.drain();
    warn!(
        "virtio-input : {} « {} » : {}, file de {} tampons, {}, EVENT_IDX {}",
        d.info.location,
        d.info.name,
        match kind {
            Kind::Tablet { x, y } => format!("pointeur absolu (x {}..{}, y {}..{})", x.0, x.1, y.0, y.1),
            Kind::Mouse => String::from("souris relative"),
        },
        n,
        if msix { format!("MSI-X (vecteurs {:?})", d.t.idt_vectors()) } else { String::from("sondage") },
        if event_idx { "oui" } else { "non" }
    );
    if !msix {
        spawn::new_task_builder(poll_loop, d).name(String::from("virtio_input_poll")).spawn()?;
    }
    Ok(d)
}

fn poll_loop(d: &'static VirtioInput) -> Result<(), &'static str> {
    loop {
        d.drain();
        let _ = sleep::sleep(POLL_PERIOD);
    }
}

impl VirtioInput {
    pub fn info(&self) -> &InputInfo {
        &self.info
    }

    pub fn stats(&self) -> InputStats {
        let s = &self.stats;
        InputStats {
            events: s.events.load(Relaxed),
            reports: s.reports.load(Relaxed),
            dropped: s.dropped.load(Relaxed),
            interrupts: s.interrupts.load(Relaxed),
        }
    }

    /// Lit les événements rendus, les accumule, rend leurs tampons à l'appareil.
    fn drain(&self) {
        let mut out: [Option<MouseEvent>; 16] = Default::default();
        loop {
            let mut n_out = 0;
            let mut more = false;
            {
                let mut guard = self.eventq.lock();
                let g = &mut *guard;
                loop {
                    let Some((head, len)) = g.q.pop_used() else {
                        // Réarmer, puis revérifier : une fin arrivée entre-temps n'interromprait pas.
                        if self.info.msix && g.q.enable_interrupts() {
                            continue;
                        }
                        break;
                    };
                    let i = g.head_buf[head as usize];
                    g.head_buf[head as usize] = u16::MAX;
                    if i == u16::MAX {
                        continue;
                    }
                    let mut e = [0u8; EVENT_SIZE];
                    g.bufs.read(i as usize * EVENT_SIZE, &mut e);
                    // Le tampon repart aussitôt (le même nombre est toujours offert).
                    let b = Buf { phys: g.bufs.phys() + (i as usize * EVENT_SIZE) as u64, len: EVENT_SIZE as u32, write: true };
                    if let Ok(h) = g.q.add(&[b]) {
                        g.head_buf[h as usize] = i;
                    }
                    if (len as usize) < EVENT_SIZE {
                        continue;
                    }
                    self.stats.events.fetch_add(1, Relaxed);
                    let ty = u16::from_le_bytes([e[0], e[1]]);
                    let code = u16::from_le_bytes([e[2], e[3]]);
                    let value = i32::from_le_bytes([e[4], e[5], e[6], e[7]]);
                    if let Some(m) = self.event(&mut g.packet, ty, code, value) {
                        out[n_out] = Some(m);
                        n_out += 1;
                        if n_out == out.len() {
                            more = true;
                            break;
                        }
                    }
                }
                if g.q.kick_needed() {
                    g.q.notify();
                }
            }
            // Livraison hors du verrou de la file.
            for m in out.iter_mut().take(n_out) {
                if let Some(m) = m.take() {
                    if self.queue.push(Event::MouseMovementEvent(m)).is_ok() {
                        self.stats.reports.fetch_add(1, Relaxed);
                    } else {
                        self.stats.dropped.fetch_add(1, Relaxed);
                    }
                }
            }
            if n_out > 0 {
                event_types::notify_input();
            }
            if !more {
                return;
            }
        }
    }

    /// Ajoute un événement au paquet ; rend le paquet complet à `SYN_REPORT`.
    fn event(&self, p: &mut Packet, ty: u16, code: u16, value: i32) -> Option<MouseEvent> {
        if p.dropping {
            if ty == ev::SYN && code == ev::SYN_REPORT {
                p.dropping = false;
                p.dx = 0;
                p.dy = 0;
                p.wheel = 0;
            }
            return None;
        }
        match (ty, code) {
            (ev::SYN, ev::SYN_REPORT) => return Some(self.report(p)),
            (ev::SYN, ev::SYN_DROPPED) => {
                self.stats.dropped.fetch_add(1, Relaxed);
                p.dropping = true;
            }
            (ev::ABS, ev::ABS_X) => {
                p.x = value;
                p.abs_seen = true;
            }
            (ev::ABS, ev::ABS_Y) => {
                p.y = value;
                p.abs_seen = true;
            }
            (ev::REL, ev::REL_X) => p.dx = p.dx.saturating_add(value),
            (ev::REL, ev::REL_Y) => p.dy = p.dy.saturating_add(value),
            (ev::REL, ev::REL_WHEEL) => p.wheel = p.wheel.saturating_add(value),
            (ev::KEY, c) => {
                let b = match c {
                    ev::BTN_LEFT | ev::BTN_TOUCH => 0,
                    ev::BTN_RIGHT => 1,
                    ev::BTN_MIDDLE => 2,
                    ev::BTN_SIDE => 3,
                    ev::BTN_EXTRA => 4,
                    _ => return None,
                };
                if value != 0 {
                    p.buttons |= 1 << b;
                } else {
                    p.buttons &= !(1 << b);
                }
            }
            _ => {}
        }
        None
    }

    fn report(&self, p: &mut Packet) -> MouseEvent {
        let b = p.buttons;
        let buttons = MouseButtons::new()
            .with_left(b & 1 != 0)
            .with_right(b & 2 != 0)
            .with_middle(b & 4 != 0)
            .with_fourth(b & 8 != 0)
            .with_fifth(b & 16 != 0);
        // `REL_WHEEL` : positif vers le haut, comme MaiOS.
        let scroll = p.wheel.clamp(i8::MIN as i32, i8::MAX as i32) as i8;
        p.wheel = 0;
        match self.info.kind {
            Kind::Tablet { x, y } if p.abs_seen => {
                let pos = MousePositionAbsolute {
                    x: (p.x.clamp(x.0, x.1) - x.0) as u32,
                    y: (p.y.clamp(y.0, y.1) - y.0) as u32,
                    max_x: (x.1 - x.0) as u32,
                    max_y: (y.1 - y.0) as u32,
                };
                MouseEvent::new_absolute(buttons, pos, scroll)
            }
            _ => {
                // `REL_Y` positif vers le bas ; MaiOS (PS/2) : positif vers le haut.
                let dx = p.dx.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                let dy = (-p.dy).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                p.dx = 0;
                p.dy = 0;
                MouseEvent::new(buttons, MouseMovementRelative::new(dx, dy, scroll))
            }
        }
    }
}
