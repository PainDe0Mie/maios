//! Une connexion : lecture, exécution des requêtes, file de sortie, rappels `frame`, registre.

use alloc::{
    format,
    rc::{Rc, Weak},
    string::String,
    sync::Arc,
    vec,
    vec::Vec,
};
use core::cell::RefCell;
use core::sync::atomic::Ordering;
use log::{error, warn};
use maiproc::fd::OpenFile;
use maiproc::socket::Ucred;
use maiproc::unix::{KernelConn, KernelRecv};
use maiproc::wait::Watcher;
use time::Instant;
use wl_proto::protocols::*;
use wl_proto::{Arg, IdError, InBuf, Interface, Limits, ObjectId, ObjectMap, OutBuf, ProtocolError, Request, SendError, DISPLAY_ID, SERVER_ID_START};

use crate::data::{DataState, Kind, Offer, Source};
use crate::popup::Positioner;
use crate::region::RegionOps;
use crate::seat::Seat;
use crate::shm::{Buffer, Pool};
use crate::surface::Surface;
use crate::xdg::XdgSurface;
use crate::timing::Feedback;
use crate::{now_ms, Bell, Op, Shared, SERVER_CRED};

pub(crate) type Fd = Arc<OpenFile>;

/// Octets lus au plus par client et par tour (les autres clients passent ensuite).
const TURN_BYTES: usize = 256 * 1024;
/// Tampon de lecture.
const SCRATCH: usize = 16 * 1024;
/// Rappels `frame` en attente au plus par client.
const MAX_FRAMES: usize = 4096;
/// Plus lointaine échéance d'un instant visé (`commit-timing`) prise pour réveil.
const MAX_TIMER_WAIT_NS: u64 = 3_600_000_000_000;

/// Globales annoncées par le registre : le nom d'une globale est son rang + 1.
static GLOBALS: [(&Interface, u32); 18] = [
    (&wl_compositor::INTERFACE, 6),
    (&wl_subcompositor::INTERFACE, 1),
    (&wl_shm::INTERFACE, 2),
    (&wl_output::INTERFACE, 4),
    (&xdg_wm_base::INTERFACE, 6),
    (&wl_seat::INTERFACE, 9),
    (&wl_data_device_manager::INTERFACE, 3),
    (&zxdg_decoration_manager_v1::INTERFACE, 1),
    (&wp_cursor_shape_manager_v1::INTERFACE, 2),
    (&wp_presentation::INTERFACE, 2),
    (&wp_fifo_manager_v1::INTERFACE, 1),
    (&wp_commit_timing_manager_v1::INTERFACE, 1),
    (&wp_viewporter::INTERFACE, 1),
    (&wp_fractional_scale_manager_v1::INTERFACE, 1),
    (&wp_single_pixel_buffer_manager_v1::INTERFACE, 1),
    (&xdg_activation_v1::INTERFACE, 1),
    (&zwp_primary_selection_device_manager_v1::INTERFACE, 1),
    (&wp_security_context_manager_v1::INTERFACE, 1),
];

/// Donnée du serveur pour chaque objet.
#[derive(Clone)]
pub(crate) enum Obj {
    Display,
    Registry,
    Callback,
    Compositor,
    Shm,
    Output,
    WmBase,
    Positioner(Rc<RefCell<Positioner>>),
    Region(Rc<RefCell<RegionOps>>),
    Pool(Rc<Pool>),
    Buffer(Rc<Buffer>),
    Surface(Rc<RefCell<Surface>>),
    XdgSurface(Rc<RefCell<XdgSurface>>),
    Toplevel(Rc<RefCell<XdgSurface>>),
    Popup(Rc<RefCell<XdgSurface>>),
    Seat,
    Pointer,
    Keyboard,
    SecurityManager,
    SecurityContext(Rc<RefCell<crate::security::Context>>),
    /// Gestionnaire et périphérique du presse-papiers ou de la sélection primaire.
    DataDeviceManager(Kind),
    DataDevice(Kind),
    DataSource(Rc<RefCell<Source>>),
    DataOffer(Rc<Offer>),
    DecorationManager,
    Decoration(Weak<RefCell<XdgSurface>>),
    CursorShapeManager,
    CursorShapeDevice,
    Presentation,
    Feedback,
    FifoManager,
    Fifo(Weak<RefCell<Surface>>),
    TimingManager,
    Timer(Weak<RefCell<Surface>>),
    Viewporter,
    Viewport(Weak<RefCell<Surface>>),
    FractionalManager,
    Fractional(Weak<RefCell<Surface>>),
    SinglePixelManager,
    Subcompositor,
    Activation,
    ActivationToken(Rc<RefCell<crate::activation::Builder>>),
    Subsurface(Weak<RefCell<Surface>>),
}

/// Connexion d'un client.
pub(crate) struct Client {
    pub id: u64,
    conn: KernelConn,
    pub bell: Arc<Bell>,
    watcher: Arc<dyn Watcher>,
    inb: InBuf<Fd>,
    st: State,
    scratch: Vec<u8>,
}

/// État d'un client, à part du tampon d'entrée (une requête décodée emprunte ce dernier).
pub(crate) struct State {
    pub client: u64,
    pub objs: ObjectMap<Obj>,
    pub out: OutBuf<Fd>,
    pub cred: Ucred,
    /// Rappels `frame` appliqués et la génération de MaiCompositor qu'ils attendent.
    pub frames: Vec<(ObjectId, u64)>,
    /// Retours de présentation appliqués, en attente de leur image.
    pub feedbacks: Vec<Feedback>,
    /// Surfaces dont des mises à jour attendent en file (barrière, instant visé).
    pub waiting: Vec<Rc<RefCell<Surface>>>,
    /// Objets `wl_output` liés (pour `wl_surface.enter`).
    pub outputs: Vec<ObjectId>,
    /// Surfaces xdg du client (fenêtres : événements, comptes).
    pub xdgs: Vec<Weak<RefCell<XdgSurface>>>,
    /// Menus, du plus ancien (le plus bas) au plus récent.
    pub popups: Vec<Weak<RefCell<XdgSurface>>>,
    /// Entrées : pointeurs, claviers, focus.
    pub seat: Seat,
    /// Presse-papiers.
    pub data: DataState,
    /// Sélection primaire.
    pub primary: DataState,
    /// Connexion venue d'un bac à sable (`security-context-v1`) : pas de globale privilégiée.
    pub sandbox: Option<Rc<crate::security::Sandbox>>,
    /// `xdg_wm_base.ping` en route (numéro, envoi).
    pub ping: Option<(u32, Instant)>,
    /// Pas de `pong` à temps : les titres l'indiquent.
    pub unresponsive: bool,
    /// Boutons vus par le dernier événement d'un menu saisissant.
    pub grab_buttons: u8,
    /// Pixels alloués aux fenêtres de ce client.
    pub win_pixels: usize,
    /// Erreur envoyée ou file de sortie débordée : la connexion se ferme après la vidange.
    pub closing: bool,
    /// Le client est parti (fin de fichier, `EPIPE`).
    pub gone: bool,
}

impl Client {
    pub fn new(id: u64, conn: KernelConn, sandbox: Option<Rc<crate::security::Sandbox>>) -> Client {
        let limits = Limits::default();
        let bell = Bell::new();
        let watcher: Arc<dyn Watcher> = bell.clone();
        conn.watch(watcher.clone());
        let cred = conn.peer_cred().unwrap_or(Ucred { pid: 0, uid: 0, gid: 0 });
        Client {
            id,
            conn,
            bell,
            watcher,
            inb: InBuf::new(&limits),
            st: State {
                client: id,
                objs: ObjectMap::new(limits.objects, Obj::Display),
                out: OutBuf::new(&limits),
                cred,
                frames: Vec::new(),
                feedbacks: Vec::new(),
                waiting: Vec::new(),
                outputs: Vec::new(),
                xdgs: Vec::new(),
                popups: Vec::new(),
                seat: Seat::default(),
                data: DataState::default(),
                primary: DataState::default(),
                sandbox,
                ping: None,
                unresponsive: false,
                grab_buttons: 0,
                win_pixels: 0,
                closing: false,
                gone: false,
            },
            scratch: vec![0u8; SCRATCH],
        }
    }

    /// Lit et exécute ce que le client a envoyé (au plus [`TURN_BYTES`] par tour).
    pub fn service(&mut self, sh: &mut Shared) {
        let mut budget = TURN_BYTES;
        loop {
            if self.st.closing || self.st.gone {
                return;
            }
            // Toutes les requêtes complètes.
            loop {
                let req = match self.inb.next(&self.st.objs) {
                    Ok(Some(r)) => r,
                    Ok(None) => break,
                    Err(e) => {
                        self.st.post_error(e);
                        return;
                    }
                };
                crate::STAT_REQUESTS.fetch_add(1, Ordering::Relaxed);
                if let Err(e) = self.st.dispatch(req, sh) {
                    self.st.post_error(e);
                    return;
                }
                if self.st.closing {
                    return;
                }
            }
            if budget == 0 {
                self.bell.again();
                return;
            }
            let room = self.inb.room().min(SCRATCH).min(budget);
            if room == 0 {
                // Impossible tant qu'un message tient dans le tampon d'entrée (64 Kio < 1 Mio).
                self.bell.again();
                return;
            }
            let mut fds = Vec::new();
            match self.conn.recv(&mut self.scratch[..room], &mut fds) {
                KernelRecv::Data(n) => {
                    budget -= n.min(budget);
                    let _ = self.inb.push_bytes(&self.scratch[..n]);
                    for f in fds {
                        if self.inb.push_fd(f).is_err() {
                            self.st.post_error(ProtocolError::new(
                                DISPLAY_ID,
                                wl_display::error::NO_MEMORY,
                                String::from("trop de descripteurs en attente"),
                            ));
                            return;
                        }
                    }
                }
                KernelRecv::Empty => return,
                KernelRecv::Eof => {
                    self.st.gone = true;
                    return;
                }
            }
        }
    }

    /// Événements des fenêtres du client (fermeture, redimensionnement).
    pub fn window_events(&mut self, sh: &mut Shared) {
        if self.st.closing || self.st.gone {
            return;
        }
        self.st.xdgs.retain(|w| w.strong_count() > 0);
        let xdgs: Vec<_> = self.st.xdgs.iter().filter_map(|w| w.upgrade()).collect();
        for x in xdgs {
            crate::xdg::window_events(&mut self.st, &x, sh);
        }
    }

    /// Une image a été présentée : rappels `frame` et retours de présentation qu'elle couvre.
    pub fn presented(&mut self, p: &window_manager::Presentation) {
        if self.st.closing || self.st.gone {
            return;
        }
        if self.st.frames.iter().any(|&(_, g)| g <= p.gen) {
            let ms = (p.time_ns / 1_000_000) as u32;
            let mut due = Vec::new();
            self.st.frames.retain(|&(id, g)| {
                if g <= p.gen {
                    due.push(id);
                    false
                } else {
                    true
                }
            });
            for id in due {
                self.st.fire_callback(id, ms);
            }
        }
        crate::timing::presented(&mut self.st, p);
    }

    /// Applique les mises à jour en file devenues prêtes (barrière levée, instant atteint).
    pub fn advance(&mut self, sh: &mut Shared) {
        if self.st.waiting.is_empty() || self.st.closing || self.st.gone {
            return;
        }
        for s in self.st.waiting.clone() {
            if let Err(e) = crate::surface::flush(&mut self.st, &s, sh) {
                self.st.post_error(e);
                return;
            }
        }
    }

    /// Prochaine échéance de ce client (instant visé d'une mise à jour, réponse à `ping`).
    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        let now_ns = window_manager::monotonic_ns(now);
        let timer = self
            .st
            .waiting
            .iter()
            .filter_map(|s| s.borrow().front_timestamp())
            .min()
            .map(|t| now + time::Duration::from_nanos(t.saturating_sub(now_ns).min(MAX_TIMER_WAIT_NS)));
        let ping = match self.st.ping {
            Some((_, at)) if !self.st.unresponsive => Some(at + crate::xdg::PING_TIMEOUT),
            _ => None,
        };
        match (timer, ping) {
            (Some(a), Some(b)) => Some(if b < a { b } else { a }),
            (a, b) => a.or(b),
        }
    }

    /// Travaux de fin de tour : client figé, sélection à présenter.
    pub fn housekeeping(&mut self, sh: &mut Shared, now: Instant) {
        if self.st.closing || self.st.gone {
            return;
        }
        crate::xdg::check_ping(&mut self.st, now);
        crate::data::sync(&mut self.st, sh);
    }

    /// Opération demandée par un autre client (presse-papiers, menus).
    pub fn run_op(&mut self, op: Op, sh: &mut Shared) {
        match op {
            Op::SourceSend { kind, source, mime, fd, .. } => crate::data::source_send(&mut self.st, kind, source, &mime, fd),
            Op::SourceCancelled { kind, source, .. } => crate::data::source_cancelled(&mut self.st, kind, source),
            Op::DismissPopups { .. } => crate::xdg::dismiss_popups(&mut self.st, sh),
        }
    }

    /// Le client va être détruit : ce qu'il tenait dans l'état commun est rendu.
    pub fn detach(&mut self, sh: &mut Shared) {
        sh.clip.source_gone(self.id, None);
        sh.primary.source_gone(self.id, None);
        if sh.cursor_owner.is_some_and(|(c, _)| c == self.id) {
            sh.cursor_owner = None;
            window_manager::set_cursor_image(None);
        }
        if sh.grab_client == Some(self.id) {
            sh.grab_client = None;
            if let Some(wm) = window_manager::WINDOW_MANAGER.get() {
                wm.lock().set_pointer_grab(None);
            }
        }
    }

    /// Dépose la file de sortie chez le client, tant que sa file de réception a de la place
    /// (sinon on attend qu'il lise : sa lecture sonne notre observateur).
    pub fn flush(&mut self) {
        while !self.st.out.is_empty() && !self.st.gone {
            let room = match self.conn.send_room() {
                Ok(r) => r,
                Err(_) => {
                    self.st.gone = true;
                    break;
                }
            };
            let (bytes, _) = self.st.out.chunk(room);
            if bytes.is_empty() || bytes.len() > room {
                break;
            }
            let data = bytes.to_vec();
            let fds = self.st.out.consume(data.len());
            if self.conn.send(data, fds, SERVER_CRED).is_err() {
                self.st.gone = true;
            }
        }
    }

    /// Plus aucun plan de composition directe ne lit le tampon `id` de numéro `serial`.
    pub fn release_serial(&mut self, id: ObjectId, serial: u64) {
        if self.st.closing || self.st.gone {
            return;
        }
        let same = matches!(self.st.objs.get(id).map(|o| &o.data), Some(Obj::Buffer(b)) if b.serial == serial);
        if same {
            self.st.send(id, wl_buffer::evt::RELEASE, []);
        }
    }

    /// Connexion à détruire : client parti, ou fermée par le serveur (après une vidange).
    pub fn finished(&self) -> bool {
        self.st.gone || self.st.closing
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.conn.unwatch(&self.watcher);
        self.conn.shutdown();
        // Fenêtres d'abord (elles quittent l'écran), puis le reste des objets.
        for x in self.st.xdgs.iter().filter_map(|w| w.upgrade()) {
            x.borrow_mut().unmap(&mut self.st.win_pixels);
        }
        drop(self.st.objs.drain());
        if self.st.closing && !self.st.gone {
            warn!("MaiLand : client {} (pid {}) deconnecte", self.id, self.st.cred.pid);
        }
    }
}

impl State {
    /// Ajoute un événement sur `id` (ignoré si l'objet n'existe plus ou si sa version ne le
    /// connaît pas). File pleine : le client ne lit plus, il sera déconnecté.
    pub fn send<'a, I: IntoIterator<Item = Arg<'a, Fd>>>(&mut self, id: ObjectId, opcode: u16, args: I) {
        let Some(o) = self.objs.get(id) else { return };
        let (iface, version) = (o.interface, o.version);
        match self.out.event(id, iface, version, opcode, args) {
            Ok(()) | Err(SendError::Version) => {}
            Err(SendError::Full) => {
                if !self.closing {
                    warn!("MaiLand : client {} ne lit plus (file de sortie pleine)", self.client);
                    crate::STAT_ERRORS.fetch_add(1, Ordering::Relaxed);
                }
                self.closing = true;
            }
            Err(SendError::Wire(e)) => error!("MaiLand : evenement {}.{} mal forme : {}", iface.name, opcode, e),
        }
    }

    /// `wl_display.error`, puis fermeture.
    pub fn post_error(&mut self, e: ProtocolError) {
        warn!(
            "MaiLand : client {} (pid {}) : erreur de protocole, objet {} code {} : {}",
            self.client, self.cred.pid, e.object, e.code, e.message
        );
        crate::STAT_ERRORS.fetch_add(1, Ordering::Relaxed);
        self.send(DISPLAY_ID, wl_display::evt::ERROR, [Arg::Object(e.object), Arg::Uint(e.code), Arg::Str(Some(e.message.as_bytes()))]);
        self.closing = true;
    }

    /// `wl_display.delete_id` : l'identifiant du client est libre.
    fn delete_id(&mut self, id: ObjectId) {
        self.send(DISPLAY_ID, wl_display::evt::DELETE_ID, [Arg::Uint(id)]);
    }

    /// Crée l'objet `id` demandé par le client.
    pub fn create(&mut self, id: ObjectId, iface: &'static Interface, version: u32, data: Obj) -> Result<(), ProtocolError> {
        self.objs.insert_client(id, iface, version, data).map_err(|e| {
            let code = if e == IdError::TooMany { wl_display::error::NO_MEMORY } else { wl_display::error::INVALID_OBJECT };
            ProtocolError::new(DISPLAY_ID, code, format!("identifiant {id} refuse ({e:?})"))
        })
    }

    /// `wl_callback.done(data)`, puis destruction par le serveur.
    pub fn fire_callback(&mut self, id: ObjectId, data: u32) {
        if !matches!(self.objs.get(id).map(|o| &o.data), Some(Obj::Callback)) {
            return;
        }
        self.send(id, wl_callback::evt::DONE, [Arg::Uint(data)]);
        self.server_destroyed(id);
    }

    /// Rappels `frame` d'une mise à jour appliquée : `done` quand l'image de génération `gen`
    /// est présentée.
    pub fn wait_frames(&mut self, ids: Vec<ObjectId>, gen: u64) {
        for id in ids {
            if self.frames.len() >= MAX_FRAMES {
                // Le client empile sans jamais attendre : on répond tout de suite.
                self.fire_callback(id, now_ms(Instant::now()));
            } else {
                self.frames.push((id, gen));
            }
        }
    }

    /// Objet détruit par le serveur après son dernier événement (`wl_callback.done`,
    /// `presented`, `discarded`).
    pub fn server_destroyed(&mut self, id: ObjectId) {
        self.delete_id(id);
        self.objs.kill(id);
    }

    /// `wl_buffer.release` si `buf` est toujours l'objet de son identifiant.
    pub fn release(&mut self, buf: &Rc<Buffer>) {
        let same = matches!(self.objs.get(buf.id).map(|o| &o.data), Some(Obj::Buffer(b)) if Rc::ptr_eq(b, buf));
        if same {
            self.send(buf.id, wl_buffer::evt::RELEASE, []);
        }
    }

    /// Exécute une requête validée par `wl_proto`.
    fn dispatch(&mut self, mut req: Request<'_, Fd>, sh: &mut Shared) -> Result<(), ProtocolError> {
        let id = req.object;
        let op = req.opcode;
        let destructor = req.desc.destructor;
        let Some(data) = self.objs.get(id).map(|o| o.data.clone()) else { return Ok(()) };
        match data {
            Obj::Display => self.display_request(op, &req, sh)?,
            Obj::Registry => self.registry_request(id, op, &req, sh)?,
            Obj::Callback | Obj::Buffer(_) => {}
            Obj::Positioner(p) => crate::popup::positioner_request(id, &p, op, &req)?,
            Obj::Popup(x) => crate::xdg::popup_request(self, &x, op, &req, sh)?,
            Obj::Seat => crate::seat::seat_request(self, id, op, &req, sh)?,
            Obj::Pointer => crate::seat::pointer_request(self, id, op, &req, sh)?,
            Obj::Keyboard => crate::seat::keyboard_request(self, id),
            Obj::SecurityManager => crate::security::manager_request(self, id, op, &mut req)?,
            Obj::SecurityContext(x) => crate::security::context_request(self, id, &x, op, &req, sh)?,
            Obj::DataDeviceManager(kind) => crate::data::manager_request(self, kind, op, &req, sh)?,
            Obj::DataDevice(kind) => crate::data::device_request(self, kind, id, op, &req, sh)?,
            Obj::DataSource(src) => crate::data::source_request(self, &src, op, &req, sh)?,
            Obj::DataOffer(o) => crate::data::offer_request(self, id, &o, op, &mut req, sh)?,
            Obj::DecorationManager => crate::xdg::decoration_manager_request(self, id, op, &req, sh)?,
            Obj::Decoration(x) => crate::xdg::decoration_request(self, id, &x, op, &req, sh)?,
            Obj::CursorShapeManager => self.cursor_shape_manager_request(op, &req)?,
            Obj::CursorShapeDevice => self.cursor_shape_request(id, op, &req, sh)?,
            Obj::Presentation => crate::timing::presentation_request(self, op, &req)?,
            Obj::Feedback => {}
            Obj::SinglePixelManager => crate::viewport::single_pixel_request(self, op, &req)?,
            Obj::FifoManager => crate::timing::fifo_manager_request(self, id, op, &req)?,
            Obj::Fifo(w) => crate::timing::fifo_request(id, &w, op)?,
            Obj::TimingManager => crate::timing::timing_manager_request(self, id, op, &req)?,
            Obj::Timer(w) => crate::timing::timer_request(id, &w, op, &req)?,
            Obj::Viewporter => crate::viewport::viewporter_request(self, id, op, &req)?,
            Obj::Viewport(w) => crate::viewport::viewport_request(id, &w, op, &req)?,
            Obj::FractionalManager => crate::viewport::fractional_manager_request(self, id, op, &req)?,
            Obj::Fractional(w) => crate::viewport::fractional_request(&w, op),
            Obj::Compositor => self.compositor_request(op, &req)?,
            Obj::Activation => crate::activation::manager_request(self, op, &req, sh)?,
            Obj::ActivationToken(b) => crate::activation::token_request(self, id, &b, op, &req, sh)?,
            Obj::Subcompositor => crate::subsurface::subcompositor_request(self, id, op, &req)?,
            Obj::Subsurface(w) => crate::subsurface::request(self, id, &w, op, &req, sh)?,
            Obj::Region(r) => r.borrow_mut().request(op, &req.args),
            Obj::Shm => crate::shm::shm_request(self, id, op, &mut req)?,
            Obj::Pool(p) => crate::shm::pool_request(self, id, &p, op, &req)?,
            Obj::Output => {
                if destructor {
                    self.outputs.retain(|&o| o != id);
                }
            }
            Obj::Surface(s) => crate::surface::request(self, id, &s, op, &req, sh)?,
            Obj::WmBase => crate::xdg::wm_base_request(self, id, op, &req)?,
            Obj::XdgSurface(x) => crate::xdg::surface_request(self, &x, op, &req, sh)?,
            Obj::Toplevel(x) => crate::xdg::toplevel_request(self, &x, op, &req, sh)?,
        }
        if destructor {
            self.objs.remove(id);
            if id < SERVER_ID_START {
                self.delete_id(id);
            }
        }
        Ok(())
    }

    fn display_request(&mut self, op: u16, req: &Request<'_, Fd>, sh: &mut Shared) -> Result<(), ProtocolError> {
        match op {
            wl_display::req::SYNC => {
                let cb = req.args.new_id(0);
                self.create(cb, &wl_callback::INTERFACE, 1, Obj::Callback)?;
                let serial = sh.next_serial();
                self.fire_callback(cb, serial);
            }
            wl_display::req::GET_REGISTRY => {
                let rid = req.args.new_id(0);
                self.create(rid, &wl_registry::INTERFACE, 1, Obj::Registry)?;
                for (i, (iface, version)) in GLOBALS.iter().enumerate() {
                    if self.sandbox.is_some() && crate::security::privileged(iface) {
                        continue;
                    }
                    self.send(
                        rid,
                        wl_registry::evt::GLOBAL,
                        [Arg::Uint(i as u32 + 1), Arg::Str(Some(iface.name.as_bytes())), Arg::Uint(*version)],
                    );
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn registry_request(&mut self, rid: ObjectId, op: u16, req: &Request<'_, Fd>, sh: &mut Shared) -> Result<(), ProtocolError> {
        if op != wl_registry::req::BIND {
            return Ok(());
        }
        let name = req.args.uint(0);
        let Some(Arg::NewIdDyn { interface, version, id }) = req.args.get(1) else {
            return Ok(());
        };
        let (interface, version, id) = (*interface, *version, *id);
        let invalid = |msg: String| ProtocolError::new(rid, wl_display::error::INVALID_OBJECT, msg);
        let Some(&(iface, max)) = (name as usize)
            .checked_sub(1)
            .and_then(|i| GLOBALS.get(i))
            .filter(|(iface, _)| self.sandbox.is_none() || !crate::security::privileged(iface))
        else {
            return Err(invalid(format!("globale {name} inconnue")));
        };
        if interface != iface.name.as_bytes() {
            return Err(invalid(format!("la globale {name} est {}, pas {}", iface.name, String::from_utf8_lossy(interface))));
        }
        if version == 0 || version > max {
            return Err(invalid(format!("version {version} de {} invalide (au plus {max})", iface.name)));
        }
        if iface.is(&wl_compositor::INTERFACE) {
            self.create(id, iface, version, Obj::Compositor)?;
        } else if iface.is(&xdg_activation_v1::INTERFACE) {
            self.create(id, iface, version, Obj::Activation)?;
        } else if iface.is(&wl_subcompositor::INTERFACE) {
            self.create(id, iface, version, Obj::Subcompositor)?;
        } else if iface.is(&wl_shm::INTERFACE) {
            self.create(id, iface, version, Obj::Shm)?;
            for f in crate::shm::FORMATS {
                self.send(id, wl_shm::evt::FORMAT, [Arg::Uint(f)]);
            }
        } else if iface.is(&wl_output::INTERFACE) {
            self.create(id, iface, version, Obj::Output)?;
            self.outputs.push(id);
            self.describe_output(id, sh);
        } else if iface.is(&xdg_wm_base::INTERFACE) {
            self.create(id, iface, version, Obj::WmBase)?;
        } else if iface.is(&wl_seat::INTERFACE) {
            self.create(id, iface, version, Obj::Seat)?;
            crate::seat::describe_seat(self, id);
        } else if iface.is(&wl_data_device_manager::INTERFACE) {
            self.create(id, iface, version, Obj::DataDeviceManager(Kind::Clipboard))?;
        } else if iface.is(&wp_security_context_manager_v1::INTERFACE) {
            self.create(id, iface, version, Obj::SecurityManager)?;
        } else if iface.is(&zwp_primary_selection_device_manager_v1::INTERFACE) {
            self.create(id, iface, version, Obj::DataDeviceManager(Kind::Primary))?;
        } else if iface.is(&zxdg_decoration_manager_v1::INTERFACE) {
            self.create(id, iface, version, Obj::DecorationManager)?;
        } else if iface.is(&wp_cursor_shape_manager_v1::INTERFACE) {
            self.create(id, iface, version, Obj::CursorShapeManager)?;
        } else if iface.is(&wp_presentation::INTERFACE) {
            self.create(id, iface, version, Obj::Presentation)?;
            crate::timing::presentation_bound(self, id);
        } else if iface.is(&wp_fifo_manager_v1::INTERFACE) {
            self.create(id, iface, version, Obj::FifoManager)?;
        } else if iface.is(&wp_commit_timing_manager_v1::INTERFACE) {
            self.create(id, iface, version, Obj::TimingManager)?;
        } else if iface.is(&wp_viewporter::INTERFACE) {
            self.create(id, iface, version, Obj::Viewporter)?;
        } else if iface.is(&wp_fractional_scale_manager_v1::INTERFACE) {
            self.create(id, iface, version, Obj::FractionalManager)?;
        } else if iface.is(&wp_single_pixel_buffer_manager_v1::INTERFACE) {
            self.create(id, iface, version, Obj::SinglePixelManager)?;
        }
        Ok(())
    }

    /// `wp_cursor_shape_manager_v1` : un objet de forme par pointeur (ou outil de tablette,
    /// sans effet : pas de tablette).
    fn cursor_shape_manager_request(&mut self, op: u16, req: &Request<'_, Fd>) -> Result<(), ProtocolError> {
        match op {
            wp_cursor_shape_manager_v1::req::GET_POINTER | wp_cursor_shape_manager_v1::req::GET_TABLET_TOOL_V2 => {
                self.create(req.args.new_id(0), &wp_cursor_shape_device_v1::INTERFACE, req.version, Obj::CursorShapeDevice)
            }
            _ => Ok(()),
        }
    }

    /// `wp_cursor_shape_device_v1.set_shape`.
    fn cursor_shape_request(&mut self, id: ObjectId, op: u16, req: &Request<'_, Fd>, sh: &mut Shared) -> Result<(), ProtocolError> {
        if op != wp_cursor_shape_device_v1::req::SET_SHAPE {
            return Ok(());
        }
        let (serial, v) = (req.args.uint(0), req.args.uint(1));
        // Les formes 35 et 36 (dnd_ask, all_resize) sont de la version 2.
        let known = if req.version >= 2 { v <= 36 } else { v <= 34 };
        let Some(shape) = window_manager::CursorShape::from_wayland(v).filter(|_| known) else {
            return Err(ProtocolError::new(id, wp_cursor_shape_device_v1::error::INVALID_SHAPE, format!("forme de curseur {v} invalide")));
        };
        crate::seat::set_shape(self, sh, serial, shape);
        Ok(())
    }

    /// Décrit l'écran sur un `wl_output` qui vient d'être lié.
    fn describe_output(&mut self, id: ObjectId, sh: &Shared) {
        let (w, h) = (sh.screen.0 as i32, sh.screen.1 as i32);
        // 96 ppp tant que MGI ne lit pas l'EDID.
        let (mm_w, mm_h) = (w * 254 / 960, h * 254 / 960);
        self.send(
            id,
            wl_output::evt::GEOMETRY,
            [
                Arg::Int(0),
                Arg::Int(0),
                Arg::Int(mm_w),
                Arg::Int(mm_h),
                Arg::Int(wl_output::subpixel::UNKNOWN as i32),
                Arg::Str(Some(b"MaiOS")),
                Arg::Str(Some(b"MGI")),
                Arg::Int(wl_output::transform::NORMAL as i32),
            ],
        );
        self.send(
            id,
            wl_output::evt::MODE,
            [Arg::Uint(wl_output::mode::CURRENT | wl_output::mode::PREFERRED), Arg::Int(w), Arg::Int(h), Arg::Int(60_000)],
        );
        self.send(id, wl_output::evt::SCALE, [Arg::Int(1)]);
        self.send(id, wl_output::evt::NAME, [Arg::Str(Some(b"MAI-1"))]);
        let desc = format!("Ecran MaiOS {w}x{h}");
        self.send(id, wl_output::evt::DESCRIPTION, [Arg::Str(Some(desc.as_bytes()))]);
        self.send(id, wl_output::evt::DONE, []);
    }

    fn compositor_request(&mut self, op: u16, req: &Request<'_, Fd>) -> Result<(), ProtocolError> {
        let id = req.args.new_id(0);
        match op {
            wl_compositor::req::CREATE_SURFACE => {
                let s = Rc::new(RefCell::new(Surface::new(id, req.version)));
                self.create(id, &wl_surface::INTERFACE, req.version, Obj::Surface(s))
            }
            wl_compositor::req::CREATE_REGION => {
                self.create(id, &wl_region::INTERFACE, 1, Obj::Region(Rc::new(RefCell::new(RegionOps::default()))))
            }
            _ => Ok(()),
        }
    }
}
