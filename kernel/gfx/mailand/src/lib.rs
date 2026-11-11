//! # MaiLand : serveur Wayland de MaiOS
//!
//! Plan : `.claude/plans/06-mailand-wayland.md` (phases W1 à W3). Le serveur vit dans le noyau,
//! à côté de MaiCompositor :
//!
//! ```text
//!  client Linux ── sendmsg/recvmsg ──► /run/user/0/wayland-0 (AF_UNIX)
//!                                          │ maiproc::unix::KernelConn : messages et
//!                                          │ descripteurs pris dans la file de réception
//!                                          ▼
//!  tâche « mailand » ── wl_proto : requêtes validées ── client.rs : objets, registre
//!        │                                              shm.rs : pools, tampons
//!        │                                              surface.rs : mises à jour en file
//!        │                                              view.rs, pixels.rs : transformation,
//!        │                                                échelle, viewport, filtre
//!        │                                              timing.rs : présentation, fifo
//!        │                                              viewport.rs : viewporter, échelle
//!        │                                              xdg.rs : fenêtres, décorations
//!        │                                              popup.rs : menus, positionneur
//!        │                                              seat.rs : pointeur, clavier, curseur
//!        │                                              data.rs : presse-papiers
//!        ▼
//!  window::Window (décorations du serveur) ──damage──► mai_compositor
//! ```
//!
//! * **Une seule tâche**, qui ne se bloque jamais sur un client : elle dort sur une sonnette
//!   ([`window_manager::Doorbell`]) que sonnent les observateurs des files des sockets (données
//!   reçues, place libérée chez le client, raccrochage), les fenêtres (événements), chaque
//!   image présentée par MaiCompositor, les tampons qu'il ne lit plus et l'échéance des
//!   instants visés (`commit-timing`).
//! * **Pixels lus sans projection** : les pools `wl_shm` sont lus par la physmap dans les pages
//!   que le client a projetées (`uvm::ShmObject::read_at`) ; une page absente se lit dans le
//!   fichier, ou comme des zéros au-delà de sa fin (pas de `SIGBUS` à intercepter, contrairement
//!   à libwayland).
//! * **Composition directe** : un tampon montré tel quel est lu par MaiCompositor au moment
//!   de composer (`window_inner::DirectContent`), sans copie ; il est rendu
//!   (`wl_buffer.release`) quand plus aucune fenêtre ne le lit, c'est-à-dire quand un autre
//!   tampon l'a remplacé. Un tampon transformé, mis à l'échelle ou recadré (`view.rs`) est
//!   copié à l'application, filtré, puis rendu aussitôt.
//! * **Horloge de présentation** (W3, `surface.rs`, `timing.rs`) : les mises à jour de contenu
//!   attendent en file leur barrière `fifo` ou leur instant `commit-timing` ; rappels `frame` et
//!   `wp_presentation_feedback` partent quand l'image qui les contient a été présentée
//!   (générations de MaiCompositor, `window_manager::frame_gen`).
//! * **Bornes par client** : objets, octets et descripteurs en attente (`wl_proto::Limits`),
//!   fenêtres et pixels de fenêtres ; un client trop lent à lire est déconnecté (file de sortie
//!   de 1 Mio, comme Mutter et KWin).

#![no_std]
#![cfg(target_arch = "x86_64")]

extern crate alloc;

mod activation;
mod client;
mod data;
mod keymap;
mod pixels;
mod popup;
mod region;
mod seat;
mod security;
mod shm;
mod subsurface;
mod surface;
mod timing;
mod view;
mod viewport;
mod xdg;

use alloc::{format, string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use log::{error, warn};
use maiproc::socket::Ucred;
use maiproc::unix::KernelListener;
use maiproc::wait::Watcher;
use time::{Duration, Instant};
use window_manager::Doorbell;

use client::{Client, Fd};
use wl_proto::ObjectId;

/// Réveil de sûreté quand rien ne se passe.
const HEARTBEAT: Duration = Duration::from_secs(1);
/// Connexions simultanées au plus.
const MAX_CLIENTS: usize = 256;
/// Identité du serveur, rendue aux clients par `SO_PEERCRED`.
pub(crate) const SERVER_CRED: Ucred = Ucred { pid: 0, uid: 0, gid: 0 };

static DOORBELL: Doorbell = Doorbell::new();
static WINDOW_EVENTS: AtomicBool = AtomicBool::new(false);
/// Une image a été présentée depuis le dernier tour.
static PRESENTED: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);

static STAT_CLIENTS: AtomicU64 = AtomicU64::new(0);
static STAT_REQUESTS: AtomicU64 = AtomicU64::new(0);
static STAT_COMMITS: AtomicU64 = AtomicU64::new(0);
static STAT_PIXELS: AtomicU64 = AtomicU64::new(0);
static STAT_ERRORS: AtomicU64 = AtomicU64::new(0);

/// Compteurs cumulés du serveur.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    /// Connexions acceptées.
    pub clients: u64,
    /// Requêtes exécutées.
    pub requests: u64,
    /// `wl_surface.commit` exécutés.
    pub commits: u64,
    /// Pixels copiés des tampons des clients vers les fenêtres.
    pub pixels: u64,
    /// Clients déconnectés pour une erreur de protocole ou un dépassement de limite.
    pub errors: u64,
}

pub fn stats() -> Stats {
    Stats {
        clients: STAT_CLIENTS.load(Ordering::Relaxed),
        requests: STAT_REQUESTS.load(Ordering::Relaxed),
        commits: STAT_COMMITS.load(Ordering::Relaxed),
        pixels: STAT_PIXELS.load(Ordering::Relaxed),
        errors: STAT_ERRORS.load(Ordering::Relaxed),
    }
}

/// Observateur d'une file de socket : marque sa source prête et sonne (sûr en interruption).
pub(crate) struct Bell {
    ready: AtomicBool,
}

impl Bell {
    fn new() -> Arc<Bell> {
        // Prête d'emblée : ce qui est arrivé avant l'inscription est traité au premier tour.
        Arc::new(Bell { ready: AtomicBool::new(true) })
    }

    /// Consomme l'état « prête ».
    fn take(&self) -> bool {
        self.ready.swap(false, Ordering::AcqRel)
    }

    /// Reste à faire (budget du tour épuisé) : repasser au prochain tour.
    fn again(&self) {
        self.ready.store(true, Ordering::Release);
        DOORBELL.ring();
    }
}

impl Watcher for Bell {
    fn notify(self: Arc<Self>) {
        self.ready.store(true, Ordering::Release);
        DOORBELL.ring();
    }
}

/// Réveille la tâche du serveur (sûr dans toute tâche).
pub(crate) fn wake() {
    DOORBELL.ring();
}

/// Crochet des fenêtres (`WindowInner::on_event`) : un événement attend dans une file.
pub(crate) fn window_event() {
    WINDOW_EVENTS.store(true, Ordering::Release);
    DOORBELL.ring();
}

/// Crochet de MaiCompositor après chaque image présentée (`window_manager::set_present_hook`).
fn present_hook() {
    PRESENTED.store(true, Ordering::Release);
    DOORBELL.ring();
}

/// Opération qu'un client demande à un autre (exécutée après le tour de tous les clients).
pub(crate) enum Op {
    /// `wl_data_source.send` vers la source d'un autre client (collage).
    SourceSend { client: u64, kind: data::Kind, source: ObjectId, mime: String, fd: Fd },
    /// Sa source n'est plus la sélection.
    SourceCancelled { client: u64, kind: data::Kind, source: ObjectId },
    /// Un autre client prend la saisie du pointeur : ses menus se ferment.
    DismissPopups { client: u64 },
}

impl Op {
    fn client(&self) -> u64 {
        match self {
            Op::SourceSend { client, .. } | Op::SourceCancelled { client, .. } | Op::DismissPopups { client } => *client,
        }
    }
}

/// État commun à tous les clients.
pub(crate) struct Shared {
    serial: u32,
    placed: u32,
    /// Résolution de l'écran.
    pub screen: (usize, usize),
    /// Client et surface dont le curseur est affiché (ceux qui ont le pointeur).
    pub cursor_owner: Option<(u64, ObjectId)>,
    /// Client qui tient la saisie d'un menu.
    pub grab_client: Option<u64>,
    /// Presse-papiers.
    pub clip: data::Clipboard,
    /// Sélection primaire.
    pub primary: data::Clipboard,
    /// Sockets d'écoute des bacs à sable (`security-context-v1`).
    pub security: Vec<security::Listener>,
    /// Opérations entre clients en attente.
    pub outbox: Vec<Op>,
    /// Jetons d'activation émis (`xdg_activation_v1`).
    pub tokens: activation::Tokens,
}

impl Shared {
    /// Numéro de série suivant (configurations, `wl_display.sync`).
    pub fn next_serial(&mut self) -> u32 {
        self.serial = self.serial.wrapping_add(1);
        self.serial
    }

    /// Dernier numéro de série donné.
    pub fn current_serial(&self) -> u32 {
        self.serial
    }

    /// Position d'une nouvelle fenêtre de `w` × `h` : en cascade, entièrement à l'écran si
    /// possible.
    pub fn place(&mut self, w: usize, h: usize) -> shapes::Coord {
        let k = (self.placed % 12) as usize;
        self.placed = self.placed.wrapping_add(1);
        let (sw, sh) = crate::xdg::work_size(self.screen);
        let x = (64 + 36 * k).min(sw.saturating_sub(w));
        let y = (48 + 36 * k).min(sh.saturating_sub(h));
        shapes::Coord::new(x as isize, y as isize)
    }
}

/// Millisecondes de l'horloge monotone (horodatage des rappels `frame`).
pub(crate) fn now_ms(now: Instant) -> u32 {
    now.duration_since(Instant::ZERO).as_millis() as u32
}

/// Démarre le serveur (une seule fois). À appeler une fois MaiCompositor initialisé.
pub fn start() -> Result<(), &'static str> {
    if STARTED.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    spawn::new_task_builder(server_task, ()).name(String::from("mailand")).spawn()?;
    Ok(())
}

fn server_task(_: ()) {
    DOORBELL.bind_current();
    let dir = match maiproc::runtime_dir(0) {
        Ok(d) => d,
        Err(e) => {
            error!("MaiLand : dossier d'execution impossible : {e}");
            return;
        }
    };
    let path = format!("{dir}/wayland-0");
    let listener = match KernelListener::bind(&path, SERVER_CRED, 128) {
        Ok(l) => l,
        Err(e) => {
            error!("MaiLand : liaison de {path} impossible (errno {})", -e);
            return;
        }
    };
    let lbell = Bell::new();
    listener.watch(lbell.clone());
    window_manager::set_present_hook(present_hook);
    warn!("MaiLand : serveur Wayland sur {path}");

    let mut shared = Shared {
        serial: 0,
        placed: 0,
        screen: window_manager::screen_size(),
        cursor_owner: None,
        grab_client: None,
        clip: data::Clipboard::new(data::Kind::Clipboard),
        primary: data::Clipboard::new(data::Kind::Primary),
        security: Vec::new(),
        outbox: Vec::new(),
        tokens: Vec::new(),
    };
    let mut clients: Vec<Client> = Vec::new();
    let mut next_id = 1u64;
    let mut incoming = Vec::new();
    loop {
        if lbell.take() {
            while let Some(conn) = listener.accept() {
                incoming.push((conn, None));
            }
        }
        if !shared.security.is_empty() {
            let mut sandboxed = Vec::new();
            security::poll(&mut shared, &mut sandboxed);
            incoming.extend(sandboxed.into_iter().map(|(c, s)| (c, Some(s))));
        }
        for (conn, sandbox) in incoming.drain(..) {
            if clients.len() >= MAX_CLIENTS {
                warn!("MaiLand : trop de clients, connexion refusee");
                conn.shutdown();
                continue;
            }
            STAT_CLIENTS.fetch_add(1, Ordering::Relaxed);
            clients.push(Client::new(next_id, conn, sandbox));
            next_id += 1;
        }
        let windows = WINDOW_EVENTS.swap(false, Ordering::AcqRel);
        let presented = PRESENTED.swap(false, Ordering::AcqRel);
        let pres = window_manager::last_presentation();
        let now = Instant::now();
        shared.clip.sync_native();
        shared.clip.pump();
        shared.primary.pump();
        for c in clients.iter_mut() {
            if c.bell.take() {
                c.service(&mut shared);
            }
            if windows {
                c.window_events(&mut shared);
            }
            if presented {
                c.presented(&pres);
            }
            c.advance(&mut shared);
        }
        // Opérations entre clients (une opération peut en produire d'autres : bornées).
        for _ in 0..4 {
            if shared.outbox.is_empty() {
                break;
            }
            let ops = core::mem::take(&mut shared.outbox);
            for op in ops {
                if let Some(c) = clients.iter_mut().find(|c| c.id == op.client()) {
                    c.run_op(op, &mut shared);
                }
            }
        }
        shared.outbox.clear();
        // Tampons qu'aucun plan de composition directe ne lit plus.
        for r in shm::take_releases() {
            if let Some(c) = clients.iter_mut().find(|c| c.id == r.client) {
                c.release_serial(r.buffer, r.serial);
            }
        }
        for c in clients.iter_mut() {
            c.housekeeping(&mut shared, now);
            c.flush();
        }
        // Un client fini est détruit ici : ses fenêtres disparaissent, sa socket se ferme.
        for c in clients.iter_mut().filter(|c| c.finished()) {
            c.detach(&mut shared);
        }
        clients.retain(|c| !c.finished());
        let now = Instant::now();
        let mut deadline = now + HEARTBEAT;
        for c in &clients {
            if let Some(d) = c.next_deadline(now) {
                deadline = deadline.min(d);
            }
        }
        DOORBELL.wait(deadline);
    }
}
