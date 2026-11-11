//! Presse-papiers et sélection primaire.
//!
//! * Presse-papiers : `wl_data_device_manager`, `wl_data_source`, `wl_data_offer`,
//!   `wl_data_device`, relié au presse-papiers de MaiOS (`mai_ui::clipboard`).
//! * Sélection primaire (`zwp_primary_selection_*_v1`, le texte sélectionné qu'un clic du
//!   milieu colle, comme sous X11) : mêmes règles, entre clients Wayland seulement (MaiOS n'a
//!   pas de sélection primaire). Les deux protocoles ne diffèrent que par leurs opcodes
//!   ([`Kind`]) : un seul code les sert.
//!
//! Règles communes :
//! * Entre clients Wayland, le descripteur de `receive` est passé tel quel à la source
//!   (`send`) : aucune copie par le serveur, comme dans tout compositeur.
//! * Vers MaiOS (presse-papiers seulement) : quand un client pose une sélection qui a du
//!   texte, le serveur la lit aussitôt par un tube (sans jamais attendre : observateur de la
//!   file du tube) et la copie dans le presse-papiers du système. Le texte survit donc au
//!   client qui l'a copié (ce que font les gestionnaires de presse-papiers).
//! * Depuis MaiOS : une copie faite dans une application native devient la sélection ; un
//!   client qui la colle reçoit le texte, écrit par le serveur dans son tube, morceau par
//!   morceau, au rythme où il lit.
//!
//! Le glisser-déposer (`start_drag`) n'est pas encore pris en charge : la source reçoit
//! `cancelled` (réponse permise par le protocole).

use alloc::{
    format,
    rc::Rc,
    string::String,
    sync::Arc,
    vec::Vec,
};
use core::cell::RefCell;
use log::warn;
use maiproc::fd::{Node, OpenFile, O_WRONLY};
use maiproc::pipe::{self, PipeEnd};
use maiproc::wait::Watcher;
use wl_proto::protocols::*;
use wl_proto::{Arg, Interface, ObjectId, ProtocolError, Request, SERVER_ID_START};

use crate::client::{Fd, Obj, State};
use crate::{Bell, Op, Shared};

/// Types MIME au plus par source, et longueur d'un type.
const MAX_MIMES: usize = 64;
const MAX_MIME_LEN: usize = 256;
/// Taille maximale d'une sélection recopiée dans le presse-papiers du système.
const MAX_FETCH: usize = 16 << 20;
/// Écritures en cours au plus (collages vers des clients).
const MAX_WRITES: usize = 64;

/// Types du texte, du plus précis au plus vague.
const TEXT_MIMES: [&str; 5] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "TEXT", "STRING"];

/// Quelle sélection : presse-papiers (`wl_data_*`) ou sélection primaire
/// (`zwp_primary_selection_*_v1`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Clipboard,
    Primary,
}

/// Opcodes et interfaces propres à chaque protocole.
struct Proto {
    create_source: u16,
    get_device: u16,
    source_iface: &'static Interface,
    device_iface: &'static Interface,
    offer_iface: &'static Interface,
    source_offer: u16,
    source_destroy: u16,
    device_set_selection: u16,
    offer_receive: u16,
    offer_destroy: u16,
    evt_source_send: u16,
    evt_source_cancelled: u16,
    evt_device_offer: u16,
    evt_device_selection: u16,
    evt_offer_offer: u16,
    /// Erreur d'une source réutilisée (`None` : le protocole n'en a pas, la source est
    /// simplement reposée).
    used_source_error: Option<u32>,
}

impl Kind {
    fn proto(self) -> &'static Proto {
        static CLIPBOARD: Proto = Proto {
            create_source: wl_data_device_manager::req::CREATE_DATA_SOURCE,
            get_device: wl_data_device_manager::req::GET_DATA_DEVICE,
            source_iface: &wl_data_source::INTERFACE,
            device_iface: &wl_data_device::INTERFACE,
            offer_iface: &wl_data_offer::INTERFACE,
            source_offer: wl_data_source::req::OFFER,
            source_destroy: wl_data_source::req::DESTROY,
            device_set_selection: wl_data_device::req::SET_SELECTION,
            offer_receive: wl_data_offer::req::RECEIVE,
            offer_destroy: wl_data_offer::req::DESTROY,
            evt_source_send: wl_data_source::evt::SEND,
            evt_source_cancelled: wl_data_source::evt::CANCELLED,
            evt_device_offer: wl_data_device::evt::DATA_OFFER,
            evt_device_selection: wl_data_device::evt::SELECTION,
            evt_offer_offer: wl_data_offer::evt::OFFER,
            used_source_error: Some(wl_data_device::error::USED_SOURCE),
        };
        static PRIMARY: Proto = Proto {
            create_source: zwp_primary_selection_device_manager_v1::req::CREATE_SOURCE,
            get_device: zwp_primary_selection_device_manager_v1::req::GET_DEVICE,
            source_iface: &zwp_primary_selection_source_v1::INTERFACE,
            device_iface: &zwp_primary_selection_device_v1::INTERFACE,
            offer_iface: &zwp_primary_selection_offer_v1::INTERFACE,
            source_offer: zwp_primary_selection_source_v1::req::OFFER,
            source_destroy: zwp_primary_selection_source_v1::req::DESTROY,
            device_set_selection: zwp_primary_selection_device_v1::req::SET_SELECTION,
            offer_receive: zwp_primary_selection_offer_v1::req::RECEIVE,
            offer_destroy: zwp_primary_selection_offer_v1::req::DESTROY,
            evt_source_send: zwp_primary_selection_source_v1::evt::SEND,
            evt_source_cancelled: zwp_primary_selection_source_v1::evt::CANCELLED,
            evt_device_offer: zwp_primary_selection_device_v1::evt::DATA_OFFER,
            evt_device_selection: zwp_primary_selection_device_v1::evt::SELECTION,
            evt_offer_offer: zwp_primary_selection_offer_v1::evt::OFFER,
            used_source_error: None,
        };
        match self {
            Kind::Clipboard => &CLIPBOARD,
            Kind::Primary => &PRIMARY,
        }
    }
}

/// Source d'un client.
pub(crate) struct Source {
    pub id: ObjectId,
    pub kind: Kind,
    pub mimes: Vec<String>,
    /// Déjà donnée à `set_selection` ou `start_drag` (une source ne sert qu'une fois).
    pub used: bool,
}

/// Offre présentée à un client : elle désigne une génération de la sélection.
pub(crate) struct Offer {
    pub kind: Kind,
    pub gen: u64,
}

/// Sélection courante.
pub(crate) enum Selection {
    None,
    /// Source vivante d'un client.
    Client { client: u64, source: ObjectId, mimes: Vec<String> },
    /// Contenu fourni par le serveur : (type, octets).
    Native(Vec<(String, Arc<[u8]>)>),
}

/// Lecture en cours de la sélection d'un client (vers le presse-papiers du système).
struct Fetch {
    gen: u64,
    end: Arc<PipeEnd>,
    buf: Vec<u8>,
}

/// Écriture en cours vers le tube d'un client qui colle.
struct Write {
    end: Arc<PipeEnd>,
    data: Arc<[u8]>,
    pos: usize,
}

/// Une sélection (presse-papiers ou primaire), commune à tous les clients.
pub(crate) struct Clipboard {
    kind: Kind,
    pub sel: Selection,
    /// Change à chaque nouvelle sélection.
    pub gen: u64,
    /// Génération du presse-papiers de MaiOS déjà reprise (ou posée par nous).
    native_gen: u64,
    /// Texte de la sélection courante, une fois lu (sert quand la source disparaît).
    captured: Option<(u64, Arc<[u8]>)>,
    fetch: Option<Fetch>,
    writes: Vec<Write>,
    bell: Arc<Bell>,
    watcher: Arc<dyn Watcher>,
}

impl Clipboard {
    pub fn new(kind: Kind) -> Clipboard {
        let bell = Bell::new();
        let watcher: Arc<dyn Watcher> = bell.clone();
        Clipboard { kind, sel: Selection::None, gen: 0, native_gen: u64::MAX, captured: None, fetch: None, writes: Vec::new(), bell, watcher }
    }

    /// Reliée au presse-papiers de MaiOS ?
    fn native(&self) -> bool {
        self.kind == Kind::Clipboard
    }

    fn set(&mut self, sel: Selection) {
        self.sel = sel;
        self.gen += 1;
        self.captured = None;
        if let Some(f) = self.fetch.take() {
            f.end.pipe.read_wait.unwatch(&self.watcher);
        }
    }

    /// Reprend une copie faite par une application native (appelé à chaque tour).
    pub fn sync_native(&mut self) {
        if !self.native() {
            return;
        }
        let g = mai_ui::clipboard::generation();
        if g == self.native_gen {
            return;
        }
        self.native_gen = g;
        let entries = match mai_ui::clipboard::get() {
            mai_ui::clipboard::Clip::Empty => return,
            mai_ui::clipboard::Clip::Text(t) => text_entries(t.as_bytes()),
            mai_ui::clipboard::Clip::Files { paths, .. } => {
                let mut uris = String::new();
                for p in &paths {
                    uris.push_str("file://");
                    uris.push_str(p);
                    uris.push_str("\r\n");
                }
                let mut e = text_entries(paths.join("\n").as_bytes());
                e.insert(0, (String::from("text/uri-list"), Arc::from(uris.as_bytes())));
                e
            }
        };
        self.set(Selection::Native(entries));
    }

    /// Avance les lectures et les écritures de tubes (quand un tube a bougé).
    pub fn pump(&mut self) {
        if !self.bell.take() {
            return;
        }
        self.pump_fetch();
        let watcher = self.watcher.clone();
        self.writes.retain_mut(|w| {
            loop {
                if w.pos >= w.data.len() {
                    w.end.pipe.write_wait.unwatch(&watcher);
                    return false; // fin : le tube se ferme, le client voit la fin du fichier
                }
                match pipe::kernel_write(&w.end, &w.data[w.pos..]) {
                    Ok(n) => w.pos += n,
                    Err(EAGAIN) => return true,
                    Err(_) => {
                        w.end.pipe.write_wait.unwatch(&watcher);
                        return false;
                    }
                }
            }
        });
    }

    fn pump_fetch(&mut self) {
        let Some(f) = self.fetch.as_mut() else { return };
        let mut chunk = [0u8; 4096];
        let done = loop {
            match pipe::kernel_read(&f.end, &mut chunk) {
                Ok(0) => break true,
                Ok(n) => {
                    if f.buf.len() + n > MAX_FETCH {
                        warn!("MaiLand : selection de plus de {} Mio, non recopiee", MAX_FETCH >> 20);
                        f.buf.clear();
                        break true;
                    }
                    f.buf.extend_from_slice(&chunk[..n]);
                }
                Err(_) => break false,
            }
        };
        if !done {
            return;
        }
        let f = self.fetch.take().unwrap();
        f.end.pipe.read_wait.unwatch(&self.watcher);
        if f.gen != self.gen || f.buf.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(&f.buf).into_owned();
        mai_ui::clipboard::set_text(&text);
        self.native_gen = mai_ui::clipboard::generation();
        self.captured = Some((f.gen, Arc::from(f.buf)));
    }

    /// Écrit `data` dans le tube `fd` d'un client qui colle.
    fn write_to(&mut self, fd: Fd, data: Arc<[u8]>) {
        let Node::Pipe(end) = &fd.node else { return };
        if !end.write || self.writes.len() >= MAX_WRITES {
            return;
        }
        end.pipe.write_wait.watch(self.watcher.clone());
        self.writes.push(Write { end: end.clone(), data, pos: 0 });
        self.bell.again();
    }

    /// La source `source` du client `client` disparaît (objet détruit ou client parti) :
    /// le texte déjà lu reste collable, sinon plus de sélection.
    pub fn source_gone(&mut self, client: u64, source: Option<ObjectId>) {
        let Selection::Client { client: c, source: s, .. } = &self.sel else { return };
        if *c != client || source.is_some_and(|x| x != *s) {
            return;
        }
        let keep = match &self.captured {
            Some((g, text)) if *g == self.gen => Some(text.clone()),
            _ => None,
        };
        match keep {
            Some(text) => self.set(Selection::Native(text_entries(&text))),
            None => self.set(Selection::None),
        }
    }
}

/// `EAGAIN` des tubes de MaiProc (code de Linux, positif).
const EAGAIN: i64 = 11;

fn text_entries(text: &[u8]) -> Vec<(String, Arc<[u8]>)> {
    let data: Arc<[u8]> = Arc::from(text);
    TEXT_MIMES.iter().map(|m| (String::from(*m), data.clone())).collect()
}

fn clip(sh: &mut Shared, kind: Kind) -> &mut Clipboard {
    match kind {
        Kind::Clipboard => &mut sh.clip,
        Kind::Primary => &mut sh.primary,
    }
}

fn state(c: &mut State, kind: Kind) -> &mut DataState {
    match kind {
        Kind::Clipboard => &mut c.data,
        Kind::Primary => &mut c.primary,
    }
}

/// Requêtes de `wl_data_device_manager` / `zwp_primary_selection_device_manager_v1`.
pub(crate) fn manager_request(c: &mut State, kind: Kind, op: u16, req: &Request<'_, Fd>, sh: &mut Shared) -> Result<(), ProtocolError> {
    let p = kind.proto();
    if op == p.create_source {
        let id = req.args.new_id(0);
        let s = Rc::new(RefCell::new(Source { id, kind, mimes: Vec::new(), used: false }));
        c.create(id, p.source_iface, req.version, Obj::DataSource(s))?;
    } else if op == p.get_device {
        let id = req.args.new_id(0);
        c.create(id, p.device_iface, req.version, Obj::DataDevice(kind))?;
        state(c, kind).devices.push(id);
        if c.seat.kbd.is_some() {
            state(c, kind).sent_gen = u64::MAX;
            sync_kind(c, sh, kind);
        }
    }
    // destroy (sélection primaire) : rien à rendre.
    Ok(())
}

/// Requêtes d'une source.
pub(crate) fn source_request(c: &mut State, s: &Rc<RefCell<Source>>, op: u16, req: &Request<'_, Fd>, sh: &mut Shared) -> Result<(), ProtocolError> {
    let kind = s.borrow().kind;
    let p = kind.proto();
    if op == p.source_offer {
        let mime = req.args.str(0).unwrap_or(&[]);
        let mut sb = s.borrow_mut();
        if sb.mimes.len() < MAX_MIMES && mime.len() <= MAX_MIME_LEN {
            let m = String::from_utf8_lossy(mime).into_owned();
            if !sb.mimes.contains(&m) {
                sb.mimes.push(m);
            }
        }
    } else if op == p.source_destroy {
        let id = s.borrow().id;
        clip(sh, kind).source_gone(c.client, Some(id));
    }
    // set_actions : glisser-déposer
    Ok(())
}

/// Requêtes d'un périphérique de sélection.
pub(crate) fn device_request(c: &mut State, kind: Kind, id: ObjectId, op: u16, req: &Request<'_, Fd>, sh: &mut Shared) -> Result<(), ProtocolError> {
    let p = kind.proto();
    if op == p.device_set_selection {
        let sid = req.args.object(0);
        let source = match c.objs.get(sid).map(|o| &o.data) {
            Some(Obj::DataSource(s)) if s.borrow().kind == kind => Some(s.clone()),
            _ => None,
        };
        if let Some(s) = &source {
            if let (true, Some(code)) = (s.borrow().used, p.used_source_error) {
                return Err(ProtocolError::new(id, code, format!("source {sid} deja utilisee")));
            }
            s.borrow_mut().used = true;
        }
        // Seul un client qui a le focus (il vient de recevoir une entrée de l'utilisateur)
        // peut changer la sélection.
        if c.seat.kbd.is_none() && c.seat.ptr.is_none() {
            if source.is_some() {
                c.send(sid, p.evt_source_cancelled, []);
            }
            return Ok(());
        }
        set_selection(c, sh, kind, source);
    } else if kind == Kind::Clipboard && op == wl_data_device::req::START_DRAG {
        let sid = req.args.object(0);
        if let Some(Obj::DataSource(s)) = c.objs.get(sid).map(|o| o.data.clone()) {
            if s.borrow().used {
                return Err(ProtocolError::new(id, wl_data_device::error::USED_SOURCE, format!("source {sid} deja utilisee")));
            }
            s.borrow_mut().used = true;
            c.send(sid, p.evt_source_cancelled, []);
        }
    } else {
        // release / destroy (destructeur)
        state(c, kind).devices.retain(|&d| d != id);
    }
    Ok(())
}

fn set_selection(c: &mut State, sh: &mut Shared, kind: Kind, source: Option<Rc<RefCell<Source>>>) {
    let p = kind.proto();
    // L'ancienne source est remplacée.
    if let Selection::Client { client, source: old, .. } = &clip(sh, kind).sel {
        let (client, old) = (*client, *old);
        if client == c.client {
            if source.as_ref().map(|s| s.borrow().id) != Some(old) {
                c.send(old, p.evt_source_cancelled, []);
            }
        } else {
            sh.outbox.push(Op::SourceCancelled { client, kind, source: old });
        }
    }
    let Some(s) = source else {
        clip(sh, kind).set(Selection::None);
        return;
    };
    let (sid, mimes) = {
        let sb = s.borrow();
        (sb.id, sb.mimes.clone())
    };
    let cb = clip(sh, kind);
    cb.set(Selection::Client { client: c.client, source: sid, mimes: mimes.clone() });
    // Copie du texte vers le presse-papiers du système, tout de suite.
    if !cb.native() {
        return;
    }
    if let Some(mime) = TEXT_MIMES.iter().find(|m| mimes.iter().any(|x| x == *m)) {
        let (r, w) = pipe::new_pipe();
        r.pipe.read_wait.watch(cb.watcher.clone());
        let wfd: Fd = OpenFile::new(Node::Pipe(w), O_WRONLY);
        c.send(sid, p.evt_source_send, [Arg::Str(Some(mime.as_bytes())), Arg::Fd(wfd)]);
        cb.fetch = Some(Fetch { gen: cb.gen, end: r, buf: Vec::new() });
        cb.bell.again();
    }
}

/// Requêtes d'une offre.
pub(crate) fn offer_request(c: &mut State, id: ObjectId, o: &Rc<Offer>, op: u16, req: &mut Request<'_, Fd>, sh: &mut Shared) -> Result<(), ProtocolError> {
    let kind = o.kind;
    let p = kind.proto();
    if op == p.offer_receive {
        let mime = String::from_utf8_lossy(req.args.str(0).unwrap_or(&[])).into_owned();
        let Some(fd) = req.args.take_fd(1) else { return Ok(()) };
        let cb = clip(sh, kind);
        if o.gen != cb.gen {
            return Ok(()); // offre périmée : le descripteur est fermé, le client lit une fin
        }
        let owner = match &cb.sel {
            Selection::None => None,
            Selection::Client { client, source, mimes } => mimes.contains(&mime).then_some((*client, *source)),
            Selection::Native(entries) => {
                if let Some((_, data)) = entries.iter().find(|(m, _)| *m == mime) {
                    let data = data.clone();
                    cb.write_to(fd, data);
                }
                return Ok(());
            }
        };
        if let Some((client, source)) = owner {
            if client == c.client {
                c.send(source, p.evt_source_send, [Arg::Str(Some(mime.as_bytes())), Arg::Fd(fd)]);
            } else {
                sh.outbox.push(Op::SourceSend { client, kind, source, mime, fd });
            }
        }
    } else if op == p.offer_destroy {
        state(c, kind).offers.retain(|&x| x != id);
    }
    // accept, finish, set_actions : glisser-déposer
    Ok(())
}

/// Le client reçoit le focus du clavier : il apprend les sélections courantes.
pub(crate) fn focus_in(c: &mut State, sh: &mut Shared) {
    sh.clip.sync_native();
    c.data.sent_gen = u64::MAX;
    c.primary.sent_gen = u64::MAX;
    sync(c, sh);
}

/// Présente les sélections courantes aux périphériques du client, si elles ont changé depuis
/// la dernière fois et qu'il a le focus du clavier.
pub(crate) fn sync(c: &mut State, sh: &mut Shared) {
    sync_kind(c, sh, Kind::Clipboard);
    sync_kind(c, sh, Kind::Primary);
}

fn sync_kind(c: &mut State, sh: &mut Shared, kind: Kind) {
    let gen = clip(sh, kind).gen;
    if c.seat.kbd.is_none() || state(c, kind).devices.is_empty() || state(c, kind).sent_gen == gen {
        return;
    }
    let p = kind.proto();
    state(c, kind).sent_gen = gen;
    let cb = clip(sh, kind);
    let mimes: Vec<String> = match &cb.sel {
        Selection::None => Vec::new(),
        Selection::Client { mimes, .. } => mimes.clone(),
        Selection::Native(e) => e.iter().map(|(m, _)| m.clone()).collect(),
    };
    let none = matches!(cb.sel, Selection::None);
    for dev in state(c, kind).devices.clone() {
        if none {
            c.send(dev, p.evt_device_selection, [Arg::Object(0)]);
            continue;
        }
        let version = c.objs.get(dev).map_or(1, |o| o.version);
        let offer = Rc::new(Offer { kind, gen });
        let Ok(oid) = c.objs.insert_server(p.offer_iface, version, Obj::DataOffer(offer)) else {
            continue; // table pleine : le client ne détruit pas ses offres
        };
        debug_assert!(oid >= SERVER_ID_START);
        state(c, kind).offers.push(oid);
        c.send(dev, p.evt_device_offer, [Arg::NewId(oid)]);
        for m in &mimes {
            c.send(oid, p.evt_offer_offer, [Arg::Str(Some(m.as_bytes()))]);
        }
        c.send(dev, p.evt_device_selection, [Arg::Object(oid)]);
    }
}

/// État d'une sélection pour un client.
#[derive(Default)]
pub(crate) struct DataState {
    pub devices: Vec<ObjectId>,
    pub offers: Vec<ObjectId>,
    /// Génération de la sélection déjà présentée.
    pub sent_gen: u64,
}

/// Exécute pour le client `c` une opération demandée par un autre client.
pub(crate) fn source_send(c: &mut State, kind: Kind, source: ObjectId, mime: &str, fd: Fd) {
    if matches!(c.objs.get(source).map(|o| &o.data), Some(Obj::DataSource(s)) if s.borrow().kind == kind) {
        c.send(source, kind.proto().evt_source_send, [Arg::Str(Some(mime.as_bytes())), Arg::Fd(fd)]);
    }
}

pub(crate) fn source_cancelled(c: &mut State, kind: Kind, source: ObjectId) {
    if matches!(c.objs.get(source).map(|o| &o.data), Some(Obj::DataSource(s)) if s.borrow().kind == kind) {
        c.send(source, kind.proto().evt_source_cancelled, []);
    }
}
