//! `xdg_wm_base`, `xdg_surface`, `xdg_toplevel`, `xdg_popup` et `zxdg_decoration_manager_v1`.
//!
//! Une `xdg_toplevel` affichée est une [`window::Window`] de MaiCompositor : décorations
//! dessinées par le serveur (titre, boutons, bordure : l'utilisateur sait toujours quelle
//! application il a devant lui, principe du chemin de confiance de Nitpicker), contenu copié du
//! tampon du client à chaque `commit`. La fenêtre suit la géométrie du client
//! (`set_window_geometry`, sinon le tampon entier) ; un redimensionnement à la souris ou un
//! agrandissement devient une configuration (`xdg_toplevel.configure`, avec les états
//! `activated` et `maximized`), le bouton de fermeture `xdg_toplevel.close`.
//!
//! `xdg-decoration` : le client choisit. `server_side` (ou `unset_mode`) : barre de titre,
//! bordure et boutons du système ; `client_side` : aucune décoration, le client dessine les
//! siennes (Firefox, GTK). Une fenêtre sans objet de décoration est en `client_side`, le mode
//! par défaut du protocole (GTK 3 et 4 ne lient pas `zxdg_decoration_manager_v1` et dessinent
//! toujours leur barre). Le mode peut changer fenêtre affichée : elle grandit ou rétrécit autour
//! du contenu, qui ne bouge pas.
//!
//! Un client figé est repéré par `xdg_wm_base.ping` (envoyé quand l'utilisateur agit sur une
//! de ses fenêtres) : sans `pong` en 5 s, ses titres l'indiquent, comme dans Mutter.

use alloc::{
    collections::VecDeque,
    format,
    rc::{Rc, Weak},
    string::String,
    vec::Vec,
};
use core::cell::RefCell;
use core::sync::atomic::Ordering;
use event_types::{Event, MousePositionEvent};
use log::error;
use region2d::{Rect, Region};
use shapes::{Coord, Rectangle};
use time::{Duration, Instant};
use window::Window;
use window_inner::{DirectContent, ResizeEdge, DEFAULT_BORDER_SIZE as BORDER, DEFAULT_TITLE_BAR_HEIGHT as TITLE, MIN_WINDOW_HEIGHT, MIN_WINDOW_WIDTH};
use wl_proto::protocols::*;
use wl_proto::{Arg, Fixed, ObjectId, ProtocolError, Request, DISPLAY_ID};

use crate::client::{Fd, Obj, State};
use crate::popup::{Overlay, Popup, Positioner};
use crate::region::rect;
use crate::surface::{Committed, Role, Surface};
use crate::{seat, Shared};

/// Configurations sans réponse gardées au plus.
const MAX_SERIALS: usize = 64;
/// Événements de fenêtre traités par tour et par fenêtre.
const MAX_EVENTS: usize = 64;
/// Pixels de fenêtres au plus par client, en écrans (borne la mémoire du noyau qu'un client
/// peut faire allouer).
const SCREENS_PER_CLIENT: usize = 8;
/// Fond d'une fenêtre (sous un contenu plus petit que la taille minimale).
const BACKGROUND: color::Color = color::Color::new(0x0020_2020);
/// Longueur maximale d'un titre ou d'un identifiant d'application.
const MAX_TEXT: usize = 256;
/// Délai de réponse à `ping` avant de déclarer le client figé.
pub(crate) const PING_TIMEOUT: Duration = Duration::from_secs(5);
/// Menus au plus par client.
const MAX_POPUPS: usize = 64;

pub(crate) struct Toplevel {
    pub id: ObjectId,
    title: String,
    app_id: String,
    window: Option<Window>,
    /// Taille du contenu affiché (géométrie du client), en pixels de surface.
    shown: (usize, usize),
    /// Pixels du tampon de la fenêtre (décorations comprises), comptés au client.
    pixels: usize,
    /// La fenêtre a le focus du clavier (état `activated`).
    focused: bool,
    /// Objet `zxdg_toplevel_decoration_v1` du client.
    pub decoration: Option<ObjectId>,
    /// Mode demandé par `set_mode` (`None` : `unset_mode` ou jamais demandé).
    deco_mode: Option<u32>,
    /// Partie de la surface affichée (pour placer le pointeur).
    geom: Rect,
    /// Contenu lu directement dans le tampon du client (composition directe).
    direct: bool,
}

pub(crate) enum XdgRole {
    None,
    Toplevel(Toplevel),
    Popup(Popup),
    /// L'objet du rôle a été détruit : plus de rôle possible pour cet `xdg_surface`.
    Gone,
}

pub(crate) struct XdgSurface {
    pub id: ObjectId,
    wm_base: ObjectId,
    pub surface: Rc<RefCell<Surface>>,
    pub role: XdgRole,
    /// Configuration initiale envoyée (remise à zéro par un démappage).
    configured: bool,
    /// Une configuration a reçu son `ack_configure`.
    acked: bool,
    serials: VecDeque<u32>,
    geometry: Option<Rect>,
    pending_geometry: Option<Rect>,
}

impl XdgSurface {
    /// Retire la fenêtre de l'écran ; le client devra refaire une configuration initiale.
    pub fn unmap(&mut self, win_pixels: &mut usize) {
        match &mut self.role {
            XdgRole::Toplevel(tl) => {
                if tl.window.take().is_some() {
                    *win_pixels -= tl.pixels.min(*win_pixels);
                }
                tl.pixels = 0;
                tl.shown = (0, 0);
            }
            XdgRole::Popup(p) => {
                if p.window.take().is_some() {
                    *win_pixels -= p.pixels.min(*win_pixels);
                }
                p.pixels = 0;
                p.shown = (0, 0);
            }
            _ => {}
        }
        self.configured = false;
        self.acked = false;
        self.serials.clear();
    }

    /// La surface a une fenêtre à l'écran (un tampon a été montré depuis la configuration).
    fn is_mapped(&self) -> bool {
        match &self.role {
            XdgRole::Toplevel(tl) => tl.window.is_some(),
            XdgRole::Popup(p) => p.window.is_some(),
            _ => false,
        }
    }

    fn surface_id(&self) -> ObjectId {
        self.surface.borrow().id
    }

    /// Géométrie demandée depuis le dernier `commit` (elle voyage avec la mise à jour).
    pub fn take_pending_geometry(&mut self) -> Option<Rect> {
        self.pending_geometry.take()
    }

    /// Le rôle est affiché (fenêtre ou menu à l'écran).
    pub fn mapped(&self) -> bool {
        match &self.role {
            XdgRole::Toplevel(tl) => tl.window.is_some(),
            XdgRole::Popup(p) => p.window.is_some() && !p.dismissed,
            _ => false,
        }
    }

    /// Le contenu affiché est lu directement dans le tampon du client.
    pub fn direct(&self) -> bool {
        match &self.role {
            XdgRole::Toplevel(tl) => tl.window.is_some() && tl.direct,
            XdgRole::Popup(p) => p.window.is_some() && p.direct,
            _ => false,
        }
    }
}

/// Texte reçu (UTF-8 du protocole), borné.
fn text(s: Option<&[u8]>) -> String {
    let mut t = String::from_utf8_lossy(s.unwrap_or(&[])).into_owned();
    if t.len() > MAX_TEXT {
        let mut end = MAX_TEXT;
        while !t.is_char_boundary(end) {
            end -= 1;
        }
        t.truncate(end);
    }
    t
}

impl Toplevel {
    /// Le système dessine les décorations : le client a un objet de décoration et n'a pas
    /// demandé `client_side`.
    fn server_side(&self) -> bool {
        self.decoration.is_some() && self.deco_mode != Some(zxdg_toplevel_decoration_v1::mode::CLIENT_SIDE)
    }
}

/// Épaisseurs `(bordure, barre de titre)` des décorations du système.
fn deco_size(server_side: bool) -> (usize, usize) {
    if server_side {
        (BORDER, TITLE)
    } else {
        (0, 0)
    }
}

/// Épaisseurs des décorations d'une fenêtre affichée.
fn deco_of(w: &Window) -> (usize, usize) {
    let i = w.inner().lock();
    (i.border_size, i.title_bar_height)
}

/// Taille d'une fenêtre (décorations comprises) pour un contenu de `w` × `h`.
fn window_size(w: usize, h: usize, server_side: bool) -> (usize, usize) {
    let (b, t) = deco_size(server_side);
    ((w + 2 * b).max(MIN_WINDOW_WIDTH), (h + t + b).max(MIN_WINDOW_HEIGHT))
}

/// Zone de travail (l'écran moins la barre des tâches).
pub(crate) fn work_size(screen: (usize, usize)) -> (usize, usize) {
    window_manager::WINDOW_MANAGER
        .get()
        .map(|wm| {
            let r = wm.lock().work_area();
            (r.width(), r.height())
        })
        .unwrap_or(screen)
}

/// Titre affiché d'une fenêtre (avec la mention d'un client figé).
fn display_title(tl: &Toplevel, unresponsive: bool) -> String {
    let base = if !tl.title.is_empty() {
        tl.title.clone()
    } else if !tl.app_id.is_empty() {
        tl.app_id.clone()
    } else {
        String::from("Wayland")
    };
    if unresponsive {
        format!("{base} (ne repond pas)")
    } else {
        base
    }
}

/// Coin (écran) de la géométrie d'une surface xdg, si elle est placée.
fn origin_of(x: &Rc<RefCell<XdgSurface>>) -> Option<Coord> {
    let parent = {
        let xs = x.borrow();
        match &xs.role {
            XdgRole::Toplevel(tl) => {
                return tl.window.as_ref().map(|w| {
                    let (b, t) = deco_of(w);
                    w.inner().lock().get_position() + (b as isize, t as isize)
                });
            }
            XdgRole::Popup(p) => {
                if let Some(o) = &p.window {
                    return Some(o.position());
                }
                (p.parent.upgrade()?, p.rect)
            }
            _ => return None,
        }
    };
    let (px, (x0, y0, _, _)) = parent;
    origin_of(&px).map(|o| o + (x0 as isize, y0 as isize))
}

pub(crate) fn wm_base_request(c: &mut State, id: ObjectId, op: u16, req: &Request<'_, Fd>) -> Result<(), ProtocolError> {
    let a = &req.args;
    match op {
        xdg_wm_base::req::DESTROY => {
            let alive = c.xdgs.iter().filter_map(|w| w.upgrade()).any(|x| x.borrow().wm_base == id);
            if alive {
                return Err(ProtocolError::new(id, xdg_wm_base::error::DEFUNCT_SURFACES, String::from("xdg_wm_base detruit avant ses surfaces")));
            }
        }
        xdg_wm_base::req::CREATE_POSITIONER => {
            let p = Rc::new(RefCell::new(Positioner::default()));
            c.create(a.new_id(0), &xdg_positioner::INTERFACE, req.version, Obj::Positioner(p))?;
        }
        xdg_wm_base::req::GET_XDG_SURFACE => {
            let (xid, sid) = (a.new_id(0), a.object(1));
            let Some(Obj::Surface(s)) = c.objs.get(sid).map(|o| o.data.clone()) else { return Ok(()) };
            {
                let sb = s.borrow();
                if sb.xdg.upgrade().is_some() || !matches!(sb.role, Role::None | Role::XdgToplevel | Role::XdgPopup) {
                    return Err(ProtocolError::new(id, xdg_wm_base::error::ROLE, format!("la surface {sid} a deja un role ({:?})", sb.role)));
                }
                if sb.has_buffer() {
                    return Err(ProtocolError::new(
                        id,
                        xdg_wm_base::error::INVALID_SURFACE_STATE,
                        format!("la surface {sid} a un tampon avant get_xdg_surface"),
                    ));
                }
            }
            let x = Rc::new(RefCell::new(XdgSurface {
                id: xid,
                wm_base: id,
                surface: s.clone(),
                role: XdgRole::None,
                configured: false,
                acked: false,
                serials: VecDeque::new(),
                geometry: None,
                pending_geometry: None,
            }));
            c.create(xid, &xdg_surface::INTERFACE, req.version, Obj::XdgSurface(x.clone()))?;
            s.borrow_mut().xdg = Rc::downgrade(&x);
            c.xdgs.push(Rc::downgrade(&x));
        }
        xdg_wm_base::req::PONG => {
            if c.ping.map(|(s, _)| s) == Some(a.uint(0)) {
                c.ping = None;
                if c.unresponsive {
                    c.unresponsive = false;
                    retitle(c);
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// `xdg_wm_base.ping` si aucun n'est en route (l'utilisateur agit sur une fenêtre du client).
fn maybe_ping(c: &mut State, sh: &mut Shared, wm_base: ObjectId) {
    if c.ping.is_some() || c.objs.get(wm_base).is_none() {
        return;
    }
    let serial = sh.next_serial();
    c.send(wm_base, xdg_wm_base::evt::PING, [Arg::Uint(serial)]);
    c.ping = Some((serial, Instant::now()));
}

/// Délai de `pong` dépassé ?
pub(crate) fn check_ping(c: &mut State, now: Instant) {
    if let Some((_, at)) = c.ping {
        if !c.unresponsive && now >= at + PING_TIMEOUT {
            c.unresponsive = true;
            retitle(c);
        }
    }
}

/// Met à jour les titres affichés (client figé ou non).
fn retitle(c: &mut State) {
    let unresponsive = c.unresponsive;
    for x in c.xdgs.iter().filter_map(|w| w.upgrade()) {
        let mut xs = x.borrow_mut();
        if let XdgRole::Toplevel(tl) = &mut xs.role {
            let t = display_title(tl, unresponsive);
            if let Some(w) = tl.window.as_mut() {
                let _ = w.set_title(t);
            }
        }
    }
}

pub(crate) fn surface_request(
    c: &mut State,
    x: &Rc<RefCell<XdgSurface>>,
    op: u16,
    req: &Request<'_, Fd>,
    sh: &mut Shared,
) -> Result<(), ProtocolError> {
    let a = &req.args;
    let mut xs = x.borrow_mut();
    let xid = xs.id;
    match op {
        xdg_surface::req::DESTROY => {
            if matches!(xs.role, XdgRole::Toplevel(_) | XdgRole::Popup(_)) {
                return Err(ProtocolError::new(
                    xid,
                    xdg_surface::error::DEFUNCT_ROLE_OBJECT,
                    String::from("xdg_surface detruit avant son role"),
                ));
            }
            xs.surface.borrow_mut().xdg = Weak::new();
        }
        xdg_surface::req::GET_TOPLEVEL | xdg_surface::req::GET_POPUP => {
            let nid = a.new_id(0);
            if !matches!(xs.role, XdgRole::None) {
                return Err(ProtocolError::new(xid, xdg_surface::error::ALREADY_CONSTRUCTED, String::from("xdg_surface a deja un role")));
            }
            let toplevel = op == xdg_surface::req::GET_TOPLEVEL;
            let want = if toplevel { Role::XdgToplevel } else { Role::XdgPopup };
            let current = xs.surface.borrow().role;
            if current != Role::None && current != want {
                return Err(ProtocolError::new(xs.wm_base, xdg_wm_base::error::ROLE, format!("la surface a deja le role {current:?}")));
            }
            if toplevel {
                c.create(nid, &xdg_toplevel::INTERFACE, req.version, Obj::Toplevel(x.clone()))?;
                xs.role = XdgRole::Toplevel(Toplevel {
                    id: nid,
                    title: String::new(),
                    app_id: String::new(),
                    window: None,
                    shown: (0, 0),
                    pixels: 0,
                    focused: false,
                    decoration: None,
                    deco_mode: None,
                    geom: Rect::EMPTY,
                    direct: false,
                });
            } else {
                let (parent_id, pos_id) = (a.object(1), a.object(2));
                let positioner = match c.objs.get(pos_id).map(|o| &o.data) {
                    Some(Obj::Positioner(p)) => p.borrow().clone(),
                    _ => Positioner::default(),
                };
                if !positioner.complete() {
                    return Err(ProtocolError::new(
                        xs.wm_base,
                        xdg_wm_base::error::INVALID_POSITIONER,
                        String::from("positionneur sans taille ou sans rectangle d'ancrage"),
                    ));
                }
                let parent = match c.objs.get(parent_id).map(|o| &o.data) {
                    Some(Obj::XdgSurface(p)) => Some(p.clone()),
                    _ => None,
                };
                if let Some(p) = &parent {
                    if Rc::ptr_eq(p, x) {
                        return Err(ProtocolError::new(xs.wm_base, xdg_wm_base::error::INVALID_POPUP_PARENT, String::from("menu parent de lui-meme")));
                    }
                }
                c.create(nid, &xdg_popup::INTERFACE, req.version, Obj::Popup(x.clone()))?;
                let mut p = Popup {
                    id: nid,
                    parent: parent.as_ref().map(Rc::downgrade).unwrap_or_default(),
                    positioner,
                    rect: (0, 0, 1, 1),
                    grab: false,
                    dismissed: false,
                    window: None,
                    shown: (0, 0),
                    pixels: 0,
                    geom: Rect::EMPTY,
                    direct: false,
                };
                // Sans parent (il viendrait d'un autre protocole) ou trop de menus : fermé
                // d'emblée (`popup_done` est permis à tout moment).
                if parent.is_none() || crate::popup::stack(c).len() >= MAX_POPUPS {
                    p.dismissed = true;
                    c.send(nid, xdg_popup::evt::POPUP_DONE, []);
                }
                xs.role = XdgRole::Popup(p);
                c.popups.push(Rc::downgrade(x));
            }
            xs.surface.borrow_mut().role = want;
        }
        xdg_surface::req::SET_WINDOW_GEOMETRY => {
            let r = rect(a.int(0), a.int(1), a.int(2), a.int(3));
            if r.is_empty() {
                return Err(ProtocolError::new(xid, xdg_surface::error::INVALID_SIZE, format!("geometrie {}x{} invalide", a.int(2), a.int(3))));
            }
            xs.pending_geometry = Some(r);
        }
        xdg_surface::req::ACK_CONFIGURE => {
            let serial = a.uint(0);
            let Some(pos) = xs.serials.iter().position(|&s| s == serial) else {
                return Err(ProtocolError::new(xid, xdg_surface::error::INVALID_SERIAL, format!("configuration {serial} inconnue")));
            };
            xs.serials.drain(..=pos);
            xs.acked = true;
        }
        _ => {}
    }
    let _ = sh;
    Ok(())
}

/// Bords de `xdg_toplevel.resize` → bord du gestionnaire de fenêtres.
fn resize_edge(e: u32) -> Option<ResizeEdge> {
    use xdg_toplevel::resize_edge::*;
    Some(match e {
        TOP => ResizeEdge::Top,
        BOTTOM => ResizeEdge::Bottom,
        LEFT => ResizeEdge::Left,
        RIGHT => ResizeEdge::Right,
        TOP_LEFT => ResizeEdge::TopLeft,
        TOP_RIGHT => ResizeEdge::TopRight,
        BOTTOM_LEFT => ResizeEdge::BottomLeft,
        BOTTOM_RIGHT => ResizeEdge::BottomRight,
        _ => return None,
    })
}

pub(crate) fn toplevel_request(
    c: &mut State,
    x: &Rc<RefCell<XdgSurface>>,
    op: u16,
    req: &Request<'_, Fd>,
    sh: &mut Shared,
) -> Result<(), ProtocolError> {
    let a = &req.args;
    let mut xs = x.borrow_mut();
    let unresponsive = c.unresponsive;
    let XdgRole::Toplevel(tl) = &mut xs.role else { return Ok(()) };
    let tid = tl.id;
    match op {
        xdg_toplevel::req::DESTROY => {
            xs.unmap(&mut c.win_pixels);
            xs.role = XdgRole::Gone;
            let sid = xs.surface_id();
            drop(xs);
            seat::surface_gone(c, sh, sid);
            dismiss_popups(c, sh);
        }
        xdg_toplevel::req::SET_TITLE => {
            tl.title = text(a.str(0));
            let t = display_title(tl, unresponsive);
            if let Some(w) = tl.window.as_mut() {
                let _ = w.set_title(t);
            }
        }
        xdg_toplevel::req::SET_APP_ID => {
            tl.app_id = text(a.str(0));
            if let Some(w) = tl.window.as_mut() {
                w.set_app_id(&tl.app_id);
            }
        }
        xdg_toplevel::req::SET_MIN_SIZE | xdg_toplevel::req::SET_MAX_SIZE => {
            if a.int(0) < 0 || a.int(1) < 0 {
                return Err(ProtocolError::new(tid, xdg_toplevel::error::INVALID_SIZE, format!("taille {}x{} negative", a.int(0), a.int(1))));
            }
        }
        xdg_toplevel::req::SET_MINIMIZED => {
            if let (Some(w), Some(wm)) = (tl.window.as_ref(), window_manager::WINDOW_MANAGER.get()) {
                let _ = wm.lock().hide_window(w.inner());
            }
        }
        xdg_toplevel::req::SET_MAXIMIZED | xdg_toplevel::req::UNSET_MAXIMIZED => {
            let on = op == xdg_toplevel::req::SET_MAXIMIZED;
            match tl.window.as_mut() {
                // La fenêtre change de taille : sa configuration suit (événement de taille).
                Some(w) if w.is_maximized() != on => w.set_maximized(on),
                _ => {
                    let (w, h) = tl.shown;
                    if xs.configured {
                        configure(c, &mut xs, w as i32, h as i32, sh);
                    }
                }
            }
        }
        xdg_toplevel::req::SET_FULLSCREEN | xdg_toplevel::req::UNSET_FULLSCREEN => {
            // Plein écran : phase W3 (composition directe). Le protocole veut une réponse,
            // ce sera la configuration courante.
            let (w, h) = tl.shown;
            if xs.configured {
                configure(c, &mut xs, w as i32, h as i32, sh);
            }
        }
        xdg_toplevel::req::MOVE | xdg_toplevel::req::RESIZE => {
            // Seulement en réponse à un bouton qui vient d'être enfoncé dans la fenêtre.
            if a.uint(1) != c.seat.input_serial {
                return Ok(());
            }
            if let Some(w) = tl.window.as_mut() {
                if op == xdg_toplevel::req::MOVE {
                    w.begin_move();
                } else if let Some(e) = resize_edge(a.uint(2)) {
                    w.begin_resize(e);
                }
            }
        }
        // `set_parent`, `show_window_menu` : sans effet pour l'instant.
        _ => {}
    }
    Ok(())
}

/// Envoie une configuration de taille `w` × `h` (0 : au choix du client).
fn configure(c: &mut State, xs: &mut XdgSurface, w: i32, h: i32, sh: &mut Shared) {
    let XdgRole::Toplevel(tl) = &xs.role else { return };
    let mut states = Vec::new();
    if tl.window.as_ref().is_some_and(|w| w.is_maximized()) {
        states.extend_from_slice(&xdg_toplevel::state::MAXIMIZED.to_le_bytes());
    }
    if tl.focused {
        states.extend_from_slice(&xdg_toplevel::state::ACTIVATED.to_le_bytes());
    }
    if let Some(d) = tl.decoration {
        let mode = if tl.server_side() { zxdg_toplevel_decoration_v1::mode::SERVER_SIDE } else { zxdg_toplevel_decoration_v1::mode::CLIENT_SIDE };
        c.send(d, zxdg_toplevel_decoration_v1::evt::CONFIGURE, [Arg::Uint(mode)]);
    }
    c.send(tl.id, xdg_toplevel::evt::CONFIGURE, [Arg::Int(w), Arg::Int(h), Arg::Array(&states)]);
    send_surface_configure(c, xs, sh);
}

fn send_surface_configure(c: &mut State, xs: &mut XdgSurface, sh: &mut Shared) {
    let serial = sh.next_serial();
    c.send(xs.id, xdg_surface::evt::CONFIGURE, [Arg::Uint(serial)]);
    xs.serials.push_back(serial);
    while xs.serials.len() > MAX_SERIALS {
        xs.serials.pop_front();
    }
}

/// Reconfigure une fenêtre déjà configurée avec sa taille courante (états changés).
fn reconfigure(c: &mut State, xs: &mut XdgSurface, sh: &mut Shared) {
    if !xs.configured {
        return;
    }
    let XdgRole::Toplevel(tl) = &xs.role else { return };
    let (w, h) = if tl.window.is_some() { tl.shown } else { (0, 0) };
    configure(c, xs, w as i32, h as i32, sh);
}

/// Première configuration d'une fenêtre : place disponible, capacités, taille libre.
fn initial_configure(c: &mut State, xs: &mut XdgSurface, sh: &mut Shared) {
    let XdgRole::Toplevel(tl) = &xs.role else { return };
    let tid = tl.id;
    let (sw, shh) = work_size(sh.screen);
    let (b, t) = deco_size(tl.server_side());
    let bounds = [Arg::Int(sw.saturating_sub(2 * b) as i32), Arg::Int(shh.saturating_sub(t + b) as i32)];
    c.send(tid, xdg_toplevel::evt::CONFIGURE_BOUNDS, bounds);
    let mut caps = Vec::new();
    caps.extend_from_slice(&xdg_toplevel::wm_capabilities::MAXIMIZE.to_le_bytes());
    caps.extend_from_slice(&xdg_toplevel::wm_capabilities::MINIMIZE.to_le_bytes());
    c.send(tid, xdg_toplevel::evt::WM_CAPABILITIES, [Arg::Array(&caps)]);
    configure(c, xs, 0, 0, sh);
    xs.configured = true;
}

/// Géométrie affichée (coordonnées de la surface) pour une surface de `w` × `h`.
fn shown_geometry(xs: &XdgSurface, w: usize, h: usize) -> Rect {
    let full = Rect::new(0, 0, w as i32, h as i32);
    xs.geometry
        .map(|g| g.intersect(&full))
        .filter(|g| !g.is_empty())
        .unwrap_or(full)
}

/// `wl_surface.commit` d'une surface xdg.
pub(crate) fn commit(
    c: &mut State,
    x: &Rc<RefCell<XdgSurface>>,
    s: &Rc<RefCell<Surface>>,
    done: Committed,
    sh: &mut Shared,
) -> Result<(), ProtocolError> {
    {
        let mut xs = x.borrow_mut();
        if let Some(g) = done.geometry {
            xs.geometry = Some(g);
        }
        let xid = xs.id;
        match xs.role {
            XdgRole::None => {
                return Err(ProtocolError::new(xid, xdg_surface::error::NOT_CONSTRUCTED, String::from("commit d'un xdg_surface sans role")));
            }
            XdgRole::Gone => return Ok(()),
            XdgRole::Popup(_) => {
                drop(xs);
                return popup_commit(c, x, s, done, sh);
            }
            XdgRole::Toplevel(_) => {}
        }
    }
    let mut xs = x.borrow_mut();
    let xid = xs.id;
    let (buffer, view, sid, sversion) = {
        let sb = s.borrow();
        (sb.buffer.clone(), sb.view, sb.id, sb.version)
    };
    if !xs.configured {
        if buffer.is_some() {
            return Err(ProtocolError::new(
                xid,
                xdg_surface::error::UNCONFIGURED_BUFFER,
                String::from("tampon attache avant la configuration initiale"),
            ));
        }
        initial_configure(c, &mut xs, sh);
        return Ok(());
    }
    let Some(buf) = buffer else {
        // Commit sans tampon d'une fenêtre pas encore montrée (Xwayland le fait juste après
        // `ack_configure`) : la configuration tient toujours.
        if !xs.is_mapped() {
            return Ok(());
        }
        // `attach(NULL)` : la fenêtre disparaît ; tout recommence par un commit sans tampon.
        xs.unmap(&mut c.win_pixels);
        drop(xs);
        seat::surface_gone(c, sh, sid);
        return Ok(());
    };
    if !xs.acked {
        return Err(ProtocolError::new(xid, xdg_surface::error::UNCONFIGURED_BUFFER, String::from("tampon attache avant ack_configure")));
    }

    let Some(view) = view else { return Ok(()) };
    // Géométrie de la fenêtre, en coordonnées de la surface (un pixel d'écran chacune).
    let (vw, vh) = view.size();
    let geom = shown_geometry(&xs, vw as usize, vh as usize);
    let (cw, ch) = (geom.width() as usize, geom.height() as usize);
    let translucent = !buf.opaque_format() && !s.borrow().opaque_covers(geom);
    let unresponsive = c.unresponsive;

    let mut damage = done.damage;
    damage.add_region(&s.borrow_mut().tree_damage.take());
    // Sous-surfaces affichées : l'arbre est composé dans la fenêtre.
    let layers = crate::subsurface::tree(s);
    let tree = layers.len() > 1;
    let mut entered = false;
    let XdgRole::Toplevel(tl) = &mut xs.role else { return Ok(()) };
    tl.geom = geom;
    let server_side = tl.server_side();
    if tl.window.is_none() || tl.shown != (cw, ch) {
        let (ww, wh) = window_size(cw, ch, server_side);
        let pixels = ww * wh;
        let (sw, shh) = sh.screen;
        let others = c.win_pixels - tl.pixels.min(c.win_pixels);
        if cw > 4 * sw || ch > 4 * shh || others + pixels > SCREENS_PER_CLIENT * sw * shh {
            return Err(ProtocolError::new(
                DISPLAY_ID,
                wl_display::error::NO_MEMORY,
                format!("fenetre de {cw}x{ch} refusee (pixels de fenetres du client epuises)"),
            ));
        }
        let no_memory = |e: &str| ProtocolError::new(DISPLAY_ID, wl_display::error::NO_MEMORY, format!("fenetre de {cw}x{ch} impossible : {e}"));
        match tl.window.as_mut() {
            None => {
                let title = display_title(tl, unresponsive);
                let pos = sh.place(ww, wh);
                let mut win = Window::with_options(title, pos, ww, wh, BACKGROUND, server_side).map_err(no_memory)?;
                win.inner().lock().on_event = Some(crate::window_event);
                win.set_app_id(&tl.app_id);
                tl.window = Some(win);
                entered = true;
                // Le focus donné à la création est déjà dans la file, avant le crochet.
                crate::window_event();
            }
            Some(w) => {
                // Une fenêtre agrandie garde la taille de l'écran : le client a dessiné moins
                // (ou plus), on montre ce qu'il a donné sans retailler la fenêtre.
                if !w.is_maximized() {
                    let (b, t) = deco_of(w);
                    w.resize_content(ww.saturating_sub(2 * b).max(cw), wh.saturating_sub(t + b).max(ch)).map_err(no_memory)?;
                }
            }
        }
        c.win_pixels = others + pixels;
        tl.pixels = pixels;
        tl.shown = (cw, ch);
        damage = Region::from_rect(geom);
    }
    let Some(win) = tl.window.as_mut() else { return Ok(()) };
    let (bx, ty) = deco_of(win);

    // Composition directe si le tampon se montre tel quel : MaiCompositor le lira, seuls les
    // coins arrondis du bas sont copiés (la décoration les arrondit dans la fenêtre). Sinon,
    // copie (transformée, filtrée) des rectangles abîmés.
    let plane = if view.is_identity() && !tree { buf.plane(c.client, geom.x0 as usize, geom.y0 as usize) } else { None };
    let direct = plane.is_some();
    if tl.direct && !direct {
        damage = Region::from_rect(geom); // le tampon de la fenêtre n'a plus ce contenu
    }
    tl.direct = direct;
    let damage = damage.intersect_rect(&geom);
    let copy = {
        let mut inner = win.inner().lock();
        inner.translucent = translucent;
        inner.direct = plane.map(|plane| DirectContent { plane, x: bx, y: ty, width: cw, height: ch });
        if direct {
            let (ww, wh) = inner.get_size();
            let corners = bottom_corners(ww, wh, inner.corner_radius, geom, (bx, ty));
            let mut part = Region::new();
            for r in damage.iter() {
                for k in corners.iter() {
                    part.add(r.intersect(k));
                }
            }
            part
        } else {
            damage.clone()
        }
    };
    let copied = {
        let mut fb = win.framebuffer_mut();
        let (fw, fh) = fb.get_size();
        if tree {
            let l = crate::subsurface::layers(&layers);
            crate::pixels::compose(&l, &copy, geom, fb.buffer_mut(), (fw, fh), (bx, ty))
        } else {
            crate::pixels::blit(&buf, &view, &copy, geom, fb.buffer_mut(), (fw, fh), (bx, ty)).1
        }
    };
    crate::STAT_PIXELS.fetch_add(copied, Ordering::Relaxed);
    let bounds = damage.bounds();
    if !bounds.is_empty() {
        let tl_x = (bx as i32 + bounds.x0 - geom.x0) as isize;
        let tl_y = (ty as i32 + bounds.y0 - geom.y0) as isize;
        let area = Rectangle {
            top_left: Coord::new(tl_x, tl_y),
            bottom_right: Coord::new(tl_x + bounds.width() as isize, tl_y + bounds.height() as isize),
        };
        if let Err(e) = win.render(Some(area)) {
            error!("MaiLand : rendu de la fenetre impossible : {e}");
        }
    }
    drop(xs);
    if entered {
        surface_entered(c, sid, sversion);
    }
    Ok(())
}

/// Coins arrondis du bas d'une fenêtre de `ww` × `wh` (côté `rayon + 1`, comme la
/// décoration), en coordonnées de la surface dont la géométrie `geom` est au coin du contenu.
fn bottom_corners(ww: usize, wh: usize, radius: usize, geom: Rect, (b, t): (usize, usize)) -> [Rect; 2] {
    if radius == 0 {
        return [Rect::EMPTY; 2];
    }
    let (ww, wh, k) = (ww as i32, wh as i32, radius as i32 + 1);
    let (dx, dy) = (geom.x0 - b as i32, geom.y0 - t as i32);
    [Rect::new(0, wh - k, k, wh), Rect::new(ww - k, wh - k, ww, wh)].map(|r| r.translate(dx, dy).intersect(&geom))
}

/// Première apparition d'une surface : écrans, échelle et transformation préférées.
fn surface_entered(c: &mut State, sid: ObjectId, sversion: u32) {
    let outputs = c.outputs.clone();
    for o in outputs {
        c.send(sid, wl_surface::evt::ENTER, [Arg::Object(o)]);
    }
    if sversion >= 6 {
        c.send(sid, wl_surface::evt::PREFERRED_BUFFER_SCALE, [Arg::Int(1)]);
        c.send(sid, wl_surface::evt::PREFERRED_BUFFER_TRANSFORM, [Arg::Uint(wl_output::transform::NORMAL)]);
    }
}

// ── Menus ─────────────────────────────────────────────────────────────────────────────────

/// Requêtes de `xdg_popup`.
pub(crate) fn popup_request(
    c: &mut State,
    x: &Rc<RefCell<XdgSurface>>,
    op: u16,
    req: &Request<'_, Fd>,
    sh: &mut Shared,
) -> Result<(), ProtocolError> {
    let a = &req.args;
    let (pid, wm_base, configured, dismissed) = {
        let xs = x.borrow();
        let XdgRole::Popup(p) = &xs.role else { return Ok(()) };
        (p.id, xs.wm_base, xs.configured, p.dismissed)
    };
    match op {
        xdg_popup::req::DESTROY => {
            // Seul le menu le plus haut encore ouvert peut être détruit.
            let stack = crate::popup::stack(c);
            let me = stack.iter().position(|p| Rc::ptr_eq(p, x));
            let above = me.map_or(false, |i| {
                stack[i + 1..].iter().any(|p| matches!(&p.borrow().role, XdgRole::Popup(q) if !q.dismissed && q.parent.upgrade().is_some_and(|par| Rc::ptr_eq(&par, x))))
            });
            if above && !dismissed {
                return Err(ProtocolError::new(wm_base, xdg_wm_base::error::NOT_THE_TOPMOST_POPUP, format!("menu {pid} detruit sous un autre menu")));
            }
            let sid = {
                let mut xs = x.borrow_mut();
                xs.unmap(&mut c.win_pixels);
                xs.role = XdgRole::Gone;
                xs.surface_id()
            };
            c.popups.retain(|w| w.upgrade().is_some_and(|p| !Rc::ptr_eq(&p, x)));
            seat::surface_gone(c, sh, sid);
            update_grab(c, sh);
        }
        xdg_popup::req::GRAB => {
            if configured {
                return Err(ProtocolError::new(pid, xdg_popup::error::INVALID_GRAB, String::from("saisie demandee apres le premier commit")));
            }
            // Une saisie répond à une action de l'utilisateur sur ce client.
            let serial = a.uint(1);
            let fresh = serial == c.seat.input_serial && (c.seat.kbd.is_some() || c.seat.ptr.is_some());
            if dismissed || !fresh {
                dismiss_one(c, x);
                return Ok(());
            }
            if let XdgRole::Popup(p) = &mut x.borrow_mut().role {
                p.grab = true;
            }
            if let Some(other) = sh.grab_client {
                if other != c.client {
                    sh.outbox.push(crate::Op::DismissPopups { client: other });
                }
            }
        }
        xdg_popup::req::REPOSITION => {
            let token = a.uint(1);
            let positioner = match c.objs.get(a.object(0)).map(|o| &o.data) {
                Some(Obj::Positioner(p)) => p.borrow().clone(),
                _ => return Ok(()),
            };
            if !positioner.complete() {
                return Err(ProtocolError::new(wm_base, xdg_wm_base::error::INVALID_POSITIONER, String::from("positionneur incomplet")));
            }
            if let XdgRole::Popup(p) = &mut x.borrow_mut().role {
                p.positioner = positioner;
            }
            if configured && !dismissed {
                c.send(pid, xdg_popup::evt::REPOSITIONED, [Arg::Uint(token)]);
                popup_configure(c, x, sh);
            }
        }
        _ => {}
    }
    Ok(())
}

/// Calcule la place du menu et l'envoie (`xdg_popup.configure` puis `xdg_surface.configure`).
fn popup_configure(c: &mut State, x: &Rc<RefCell<XdgSurface>>, sh: &mut Shared) {
    let parent = match &x.borrow().role {
        XdgRole::Popup(p) => p.parent.upgrade(),
        _ => return,
    };
    let origin = parent.as_ref().and_then(origin_of).unwrap_or(Coord::new(0, 0));
    let (sw, shh) = sh.screen;
    let bounds = (-(origin.x as i32), -(origin.y as i32), sw as i32, shh as i32);
    let mut xs = x.borrow_mut();
    let XdgRole::Popup(p) = &mut xs.role else { return };
    p.rect = p.positioner.place(bounds);
    let (px, py, pw, ph) = p.rect;
    let pid = p.id;
    c.send(pid, xdg_popup::evt::CONFIGURE, [Arg::Int(px), Arg::Int(py), Arg::Int(pw), Arg::Int(ph)]);
    send_surface_configure(c, &mut xs, sh);
}

fn popup_commit(
    c: &mut State,
    x: &Rc<RefCell<XdgSurface>>,
    s: &Rc<RefCell<Surface>>,
    done: Committed,
    sh: &mut Shared,
) -> Result<(), ProtocolError> {
    let (buffer, view, sid, sversion) = {
        let sb = s.borrow();
        (sb.buffer.clone(), sb.view, sb.id, sb.version)
    };
    let (xid, configured, acked, dismissed) = {
        let xs = x.borrow();
        let XdgRole::Popup(p) = &xs.role else { return Ok(()) };
        (xs.id, xs.configured, xs.acked, p.dismissed)
    };
    if dismissed {
        return Ok(());
    }
    if !configured {
        if buffer.is_some() {
            return Err(ProtocolError::new(xid, xdg_surface::error::UNCONFIGURED_BUFFER, String::from("tampon de menu avant la configuration")));
        }
        x.borrow_mut().configured = true;
        popup_configure(c, x, sh);
        return Ok(());
    }
    let Some(buf) = buffer else {
        let mut xs = x.borrow_mut();
        if !xs.is_mapped() {
            return Ok(());
        }
        xs.unmap(&mut c.win_pixels);
        drop(xs);
        seat::surface_gone(c, sh, sid);
        update_grab(c, sh);
        return Ok(());
    };
    if !acked {
        return Err(ProtocolError::new(xid, xdg_surface::error::UNCONFIGURED_BUFFER, String::from("tampon de menu avant ack_configure")));
    }
    let parent = match &x.borrow().role {
        XdgRole::Popup(p) => p.parent.upgrade(),
        _ => None,
    };
    let Some(origin) = parent.as_ref().and_then(origin_of) else {
        // Parent pas (ou plus) affiché : le menu n'a pas de place.
        dismiss_one(c, x);
        return Ok(());
    };
    let Some(view) = view else { return Ok(()) };
    let mut xs = x.borrow_mut();
    let (vw, vh) = view.size();
    let geom = shown_geometry(&xs, vw as usize, vh as usize);
    let (cw, ch) = (geom.width() as usize, geom.height() as usize);
    let translucent = !buf.opaque_format() && !s.borrow().opaque_covers(geom);
    let XdgRole::Popup(p) = &mut xs.role else { return Ok(()) };
    let pos = origin + (p.rect.0 as isize, p.rect.1 as isize);
    let mut damage = done.damage;
    damage.add_region(&s.borrow_mut().tree_damage.take());
    let layers = crate::subsurface::tree(s);
    let tree = layers.len() > 1;
    let mut entered = false;
    p.geom = geom;
    let (sw, shh) = sh.screen;
    let pixels = cw * ch;
    let others = c.win_pixels - p.pixels.min(c.win_pixels);
    if cw > 4 * sw || ch > 4 * shh || others + pixels > SCREENS_PER_CLIENT * sw * shh {
        return Err(ProtocolError::new(DISPLAY_ID, wl_display::error::NO_MEMORY, format!("menu de {cw}x{ch} refuse")));
    }
    let no_memory = |e: &str| ProtocolError::new(DISPLAY_ID, wl_display::error::NO_MEMORY, format!("menu de {cw}x{ch} impossible : {e}"));
    match p.window.as_mut() {
        None => {
            p.window = Some(Overlay::new(pos, cw, ch).map_err(no_memory)?);
            damage = Region::from_rect(geom);
            entered = true;
        }
        Some(o) => {
            if p.shown != (cw, ch) || o.position() != pos {
                o.reshape(pos, cw, ch).map_err(no_memory)?;
                damage = Region::from_rect(geom);
            }
        }
    }
    c.win_pixels = others + pixels;
    p.pixels = pixels;
    p.shown = (cw, ch);
    let grab = p.grab;
    let plane = if view.is_identity() && !tree { buf.plane(c.client, geom.x0 as usize, geom.y0 as usize) } else { None };
    let direct = plane.is_some();
    if p.direct && !direct {
        damage = Region::from_rect(geom);
    }
    p.direct = direct;
    let Some(o) = p.window.as_ref() else { return Ok(()) };
    let damage = damage.intersect_rect(&geom);
    let copied = {
        let mut inner = o.inner.lock();
        inner.translucent = translucent;
        inner.direct = plane.map(|plane| DirectContent { plane, x: 0, y: 0, width: cw, height: ch });
        if direct {
            0
        } else {
            let (fw, fh) = inner.get_size();
            let px = inner.framebuffer_mut().buffer_mut();
            if tree {
                crate::pixels::compose(&crate::subsurface::layers(&layers), &damage, geom, px, (fw, fh), (0, 0))
            } else {
                crate::pixels::blit(&buf, &view, &damage, geom, px, (fw, fh), (0, 0)).1
            }
        }
    };
    crate::STAT_PIXELS.fetch_add(copied, Ordering::Relaxed);
    let bounds = damage.bounds();
    if !bounds.is_empty() {
        let tl = Coord::new((bounds.x0 - geom.x0) as isize, (bounds.y0 - geom.y0) as isize);
        o.render(Rectangle { top_left: tl, bottom_right: tl + (bounds.width() as isize, bounds.height() as isize) });
    }
    drop(xs);
    if entered {
        surface_entered(c, sid, sversion);
        if grab {
            update_grab(c, sh);
        }
    }
    Ok(())
}

/// Ferme un menu (et le retire de l'écran) : `popup_done`.
fn dismiss_one(c: &mut State, x: &Rc<RefCell<XdgSurface>>) {
    let mut xs = x.borrow_mut();
    let pid = match &mut xs.role {
        XdgRole::Popup(p) if !p.dismissed => {
            p.dismissed = true;
            p.id
        }
        _ => return,
    };
    xs.unmap(&mut c.win_pixels);
    // Un menu fermé ne se rouvre pas : plus de configuration à attendre.
    xs.configured = true;
    drop(xs);
    c.send(pid, xdg_popup::evt::POPUP_DONE, []);
}

/// Ferme tous les menus du client (du plus haut au plus bas) et rend pointeur et clavier.
pub(crate) fn dismiss_popups(c: &mut State, sh: &mut Shared) {
    let stack = crate::popup::stack(c);
    for x in stack.iter().rev() {
        let sid = x.borrow().surface_id();
        dismiss_one(c, x);
        seat::surface_gone(c, sh, sid);
    }
    update_grab(c, sh);
}

/// Après un changement de menus : la saisie va au menu saisissant le plus haut encore
/// affiché, sinon elle est rendue (pointeur libre, clavier à la fenêtre qui a le focus).
fn update_grab(c: &mut State, sh: &mut Shared) {
    let stack = crate::popup::stack(c);
    let top = stack.iter().rev().find(|x| matches!(&x.borrow().role, XdgRole::Popup(p) if p.grab && !p.dismissed && p.window.is_some()));
    let wm = window_manager::WINDOW_MANAGER.get();
    match top {
        Some(x) => {
            let (inner, sid) = {
                let xs = x.borrow();
                let XdgRole::Popup(p) = &xs.role else { return };
                (p.window.as_ref().map(|o| o.inner.clone()), xs.surface_id())
            };
            if let (Some(inner), Some(wm)) = (inner, wm) {
                wm.lock().set_pointer_grab(Some(&inner));
            }
            sh.grab_client = Some(c.client);
            seat::keyboard_enter(c, sh, sid);
        }
        None => {
            if sh.grab_client == Some(c.client) {
                sh.grab_client = None;
                if let Some(wm) = wm {
                    wm.lock().set_pointer_grab(None);
                }
            }
            match focused_toplevel(c) {
                Some(sid) => seat::keyboard_enter(c, sh, sid),
                None => seat::keyboard_leave(c, sh),
            }
        }
    }
}

/// Surface de la fenêtre du client qui a le focus du clavier.
/// Met la fenêtre de `x` au premier plan et lui donne le focus (`xdg_activation_v1`).
pub(crate) fn raise(x: &Rc<RefCell<XdgSurface>>) {
    let xs = x.borrow();
    let XdgRole::Toplevel(tl) = &xs.role else { return };
    let Some(w) = tl.window.as_ref() else { return };
    let inner = w.inner().clone();
    drop(xs);
    if let Some(wm) = window_manager::WINDOW_MANAGER.get() {
        if let Err(e) = wm.lock().set_active(&inner, true) {
            error!("MaiLand : activation de la fenetre impossible : {e}");
        }
    }
}

pub(crate) fn focused_toplevel(c: &State) -> Option<ObjectId> {
    c.xdgs.iter().filter_map(|w| w.upgrade()).find_map(|x| {
        let xs = x.borrow();
        match &xs.role {
            XdgRole::Toplevel(tl) if tl.focused => Some(xs.surface_id()),
            _ => None,
        }
    })
}

/// Le client tient-il une saisie de menu ?
fn grabbing(c: &State, sh: &Shared) -> bool {
    sh.grab_client == Some(c.client)
}

/// Coordonnées dans une surface affichée à l'écran : `(gx, gy)` relatif au coin de la
/// géométrie affichée → virgule fixe de la surface ; `None` hors de la partie affichée.
fn surface_point(local: Coord, geom: Rect) -> Option<(Fixed, Fixed)> {
    let sx = local.x as i32 + geom.x0;
    let sy = local.y as i32 + geom.y0;
    if sx < geom.x0 || sy < geom.y0 || sx >= geom.x1 || sy >= geom.y1 {
        return None;
    }
    Some((Fixed::from_int(sx), Fixed::from_int(sy)))
}

/// Surface de l'arbre de `root` sous le point `local` (relatif au coin de la géométrie
/// affichée `geom`) et coordonnées dans celle-ci ; `None` hors de la partie affichée ou hors
/// des régions d'entrée. `held` : surface qui a reçu l'appui d'un bouton encore enfoncé
/// (saisie implicite : elle garde le pointeur, même hors d'elle).
fn tree_point(root: &Rc<RefCell<Surface>>, local: Coord, geom: Rect, held: Option<ObjectId>) -> Option<(ObjectId, Fixed, Fixed)> {
    let (sx, sy) = (local.x as i32 + geom.x0, local.y as i32 + geom.y0);
    if let Some(h) = held.filter(|&h| h != root.borrow().id) {
        if let Some((_, x, y)) = crate::subsurface::tree(root).iter().find(|(s, _, _)| s.borrow().id == h) {
            return Some((h, Fixed::from_int(sx - x), Fixed::from_int(sy - y)));
        }
    }
    surface_point(local, geom)?;
    let (id, lx, ly) = crate::subsurface::pick(root, sx, sy)?;
    Some((id, Fixed::from_int(lx), Fixed::from_int(ly)))
}

/// Le pointeur est-il sur une surface de l'arbre de `root` ?
fn pointer_in_tree(c: &State, root: &Rc<RefCell<Surface>>) -> bool {
    let Some(p) = c.seat.ptr.as_ref() else { return false };
    crate::subsurface::tree(root).iter().any(|(s, _, _)| s.borrow().id == p.surface) || root.borrow().id == p.surface
}

/// Événements de la fenêtre de `x` : pointeur, clavier, focus, fermeture, redimensionnement.
pub(crate) fn window_events(c: &mut State, x: &Rc<RefCell<XdgSurface>>, sh: &mut Shared) {
    if matches!(x.borrow().role, XdgRole::Popup(_)) {
        popup_events(c, x, sh);
        return;
    }
    for _ in 0..MAX_EVENTS {
        let mut xs = x.borrow_mut();
        let wm_base = xs.wm_base;
        let sid = xs.surface_id();
        let (tid, shown, geom, ev) = {
            let XdgRole::Toplevel(tl) = &mut xs.role else { return };
            let Some(w) = tl.window.as_mut() else { return };
            match w.handle_event() {
                Ok(Some(e)) => (tl.id, tl.shown, tl.geom, e),
                Ok(None) => return,
                Err(e) => {
                    error!("MaiLand : evenements de fenetre : {e}");
                    return;
                }
            }
        };
        match ev {
            Event::ExitEvent => {
                drop(xs);
                c.send(tid, xdg_toplevel::evt::CLOSE, []);
                maybe_ping(c, sh, wm_base);
            }
            Event::WindowResizeEvent(r) => {
                let (w, h) = (r.width(), r.height());
                let maximized = matches!(&xs.role, XdgRole::Toplevel(tl) if tl.window.as_ref().is_some_and(|w| w.is_maximized()));
                if xs.configured && ((w, h) != shown || maximized) {
                    configure(c, &mut xs, w as i32, h as i32, sh);
                }
            }
            Event::FocusEvent(f) => {
                if let XdgRole::Toplevel(tl) = &mut xs.role {
                    tl.focused = f;
                }
                reconfigure(c, &mut xs, sh);
                drop(xs);
                if f {
                    if !grabbing(c, sh) {
                        seat::keyboard_enter(c, sh, sid);
                    }
                    maybe_ping(c, sh, wm_base);
                } else {
                    dismiss_popups(c, sh);
                    seat::keyboard_leave(c, sh);
                }
            }
            Event::KeyboardEvent(k) => {
                drop(xs);
                seat::key(c, sh, &k.key_event);
            }
            Event::MousePositionEvent(m) => {
                let root = xs.surface.clone();
                let (b, t) = match &xs.role {
                    XdgRole::Toplevel(tl) => tl.window.as_ref().map(deco_of).unwrap_or((BORDER, TITLE)),
                    _ => (BORDER, TITLE),
                };
                drop(xs);
                if grabbing(c, sh) {
                    continue; // le menu saisissant reçoit le pointeur
                }
                let local = m.coordinate - (b as isize, t as isize);
                let held = c.seat.ptr.as_ref().filter(|p| p.buttons != 0).map(|p| p.surface);
                match tree_point(&root, local, geom, held) {
                    Some((target, fx, fy)) => {
                        let pressed = m.left_button_hold || m.right_button_hold || m.middle_button_hold;
                        seat::pointer_event(c, sh, target, fx, fy, &m);
                        if pressed {
                            maybe_ping(c, sh, wm_base);
                        }
                    }
                    None => seat::pointer_leave(c, sh),
                }
            }
            Event::PointerLeave => {
                let root = xs.surface.clone();
                drop(xs);
                if pointer_in_tree(c, &root) {
                    seat::pointer_leave(c, sh);
                }
            }
            _ => {}
        }
    }
    // Il en reste : au prochain tour.
    crate::window_event();
}

/// Événements de la fenêtre d'un menu.
fn popup_events(c: &mut State, x: &Rc<RefCell<XdgSurface>>, sh: &mut Shared) {
    for _ in 0..MAX_EVENTS {
        let ev = {
            let xs = x.borrow();
            let XdgRole::Popup(p) = &xs.role else { return };
            let Some(o) = &p.window else { return };
            match o.events.pop() {
                Some(e) => e,
                None => return,
            }
        };
        match ev {
            Event::MousePositionEvent(m) => popup_pointer(c, x, sh, &m),
            Event::PointerLeave => {
                let root = x.borrow().surface.clone();
                if pointer_in_tree(c, &root) {
                    seat::pointer_leave(c, sh);
                }
            }
            _ => {}
        }
    }
    crate::window_event();
}

/// Pointeur reçu par la fenêtre d'un menu. Pendant une saisie, tout arrive au menu le plus
/// haut : le point d'écran désigne le menu visé ; un bouton enfoncé hors des menus les ferme.
fn popup_pointer(c: &mut State, x: &Rc<RefCell<XdgSurface>>, sh: &mut Shared, m: &MousePositionEvent) {
    let buttons = (m.left_button_hold as u8) | (m.right_button_hold as u8) << 1 | (m.middle_button_hold as u8) << 2;
    let newly = buttons & !c.grab_buttons != 0;
    c.grab_buttons = buttons;
    let target = if grabbing(c, sh) {
        let stack = crate::popup::stack(c);
        stack.into_iter().rev().find(|p| match &p.borrow().role {
            XdgRole::Popup(q) => !q.dismissed && q.window.as_ref().is_some_and(|o| o.contains(m.gcoordinate)),
            _ => false,
        })
    } else {
        Some(x.clone())
    };
    let Some(t) = target else {
        if newly {
            dismiss_popups(c, sh);
        } else {
            seat::pointer_leave(c, sh);
        }
        return;
    };
    let hit = {
        let xs = t.borrow();
        let XdgRole::Popup(p) = &xs.role else { return };
        let Some(o) = &p.window else { return };
        tree_point(&xs.surface, m.gcoordinate - o.position(), p.geom, None)
    };
    match hit {
        Some((sid, fx, fy)) => seat::pointer_event(c, sh, sid, fx, fy, m),
        None => seat::pointer_leave(c, sh),
    }
}

// ── Décorations ───────────────────────────────────────────────────────────────────────────

/// `zxdg_decoration_manager_v1.get_toplevel_decoration`.
pub(crate) fn decoration_manager_request(c: &mut State, id: ObjectId, op: u16, req: &Request<'_, Fd>, sh: &mut Shared) -> Result<(), ProtocolError> {
    if op != zxdg_decoration_manager_v1::req::GET_TOPLEVEL_DECORATION {
        return Ok(());
    }
    let (did, tid) = (req.args.new_id(0), req.args.object(1));
    let Some(Obj::Toplevel(x)) = c.objs.get(tid).map(|o| o.data.clone()) else { return Ok(()) };
    {
        let xs = x.borrow();
        if let XdgRole::Toplevel(tl) = &xs.role {
            if tl.decoration.is_some() {
                return Err(ProtocolError::new(id, zxdg_toplevel_decoration_v1::error::ALREADY_CONSTRUCTED, format!("la fenetre {tid} a deja une decoration")));
            }
        }
        if xs.surface.borrow().buffer.is_some() {
            return Err(ProtocolError::new(id, zxdg_toplevel_decoration_v1::error::UNCONFIGURED_BUFFER, format!("la fenetre {tid} a deja un tampon")));
        }
    }
    c.create(did, &zxdg_toplevel_decoration_v1::INTERFACE, req.version, Obj::Decoration(Rc::downgrade(&x)))?;
    let mut xs = x.borrow_mut();
    if let XdgRole::Toplevel(tl) = &mut xs.role {
        tl.decoration = Some(did);
    }
    // `configure` envoie le mode de la décoration avant la configuration de la fenêtre (avant
    // la configuration initiale : c'est elle qui le portera).
    reconfigure(c, &mut xs, sh);
    Ok(())
}

/// Requêtes de `zxdg_toplevel_decoration_v1` : la réponse est toujours `server_side`.
pub(crate) fn decoration_request(c: &mut State, id: ObjectId, x: &Weak<RefCell<XdgSurface>>, op: u16, req: &Request<'_, Fd>, sh: &mut Shared) -> Result<(), ProtocolError> {
    let Some(x) = x.upgrade() else { return Ok(()) };
    match op {
        zxdg_toplevel_decoration_v1::req::DESTROY => {
            // Retour au mode par défaut du protocole (décorations du client).
            let mut xs = x.borrow_mut();
            if let XdgRole::Toplevel(tl) = &mut xs.role {
                tl.decoration = None;
                tl.deco_mode = None;
            }
            apply_decoration(&mut xs);
        }
        zxdg_toplevel_decoration_v1::req::SET_MODE | zxdg_toplevel_decoration_v1::req::UNSET_MODE => {
            if op == zxdg_toplevel_decoration_v1::req::SET_MODE {
                let mode = req.args.uint(0);
                if mode != zxdg_toplevel_decoration_v1::mode::CLIENT_SIDE && mode != zxdg_toplevel_decoration_v1::mode::SERVER_SIDE {
                    return Err(ProtocolError::new(id, zxdg_toplevel_decoration_v1::error::INVALID_MODE, format!("mode de decoration {mode} invalide")));
                }
            }
            let mut xs = x.borrow_mut();
            if let XdgRole::Toplevel(tl) = &mut xs.role {
                tl.deco_mode = (op == zxdg_toplevel_decoration_v1::req::SET_MODE).then(|| req.args.uint(0));
            }
            apply_decoration(&mut xs);
            reconfigure(c, &mut xs, sh);
        }
        _ => {}
    }
    Ok(())
}

/// Met les décorations d'une fenêtre affichée en accord avec le mode choisi : la fenêtre
/// grandit ou rétrécit autour du contenu, la composition directe suit le nouveau coin.
fn apply_decoration(xs: &mut XdgSurface) {
    let XdgRole::Toplevel(tl) = &mut xs.role else { return };
    let on = tl.server_side();
    let Some(w) = tl.window.as_mut() else { return };
    if w.is_decorated() == on {
        return;
    }
    if let Err(e) = w.set_decorated(on) {
        error!("MaiLand : decorations de la fenetre : {e}");
        return;
    }
    let (b, t) = deco_of(w);
    let mut inner = w.inner().lock();
    if let Some(d) = inner.direct.as_mut() {
        d.x = b;
        d.y = t;
    }
}
