//! Menus et bulles : `xdg_positioner` et `xdg_popup`.
//!
//! * **Placement** : règles de `xdg_positioner` (point d'ancrage sur le rectangle d'ancrage,
//!   gravité, décalage), puis ajustements quand le menu sortirait de l'écran, axe par axe et
//!   dans l'ordre du protocole : retournement (`flip`), glissement (`slide`), rétrécissement
//!   (`resize`). Coordonnées relatives à la géométrie de la surface parente.
//! * **Affichage** : un menu est une fenêtre de surimpression de MaiCompositor (au-dessus de
//!   tout, sans décoration ni focus : `WindowManager::add_overlay`).
//! * **Saisie** (`xdg_popup.grab`) : le pointeur entier va au menu le plus haut
//!   (`set_pointer_grab`), le clavier aussi ; un clic hors de la chaîne de menus les ferme tous
//!   (`popup_done`, du plus haut au plus bas), comme dans Mutter et KWin.

use alloc::{
    format,
    rc::{Rc, Weak},
    string::String,
    sync::Arc,
};
use core::cell::RefCell;
use event_types::Event;
use framebuffer::{AlphaPixel, Framebuffer};
use log::error;
use mpmc::Queue;
use shapes::{Coord, Rectangle};
use spin::Mutex;
use window_inner::WindowInner;
use wl_proto::protocols::*;
use wl_proto::{ObjectId, ProtocolError, Request};

use crate::client::{Fd, State};

/// État d'un `xdg_positioner` (copié à la création du menu).
#[derive(Clone, Default)]
pub(crate) struct Positioner {
    pub size: Option<(i32, i32)>,
    pub anchor_rect: Option<(i32, i32, i32, i32)>,
    pub anchor: u32,
    pub gravity: u32,
    pub adjust: u32,
    pub offset: (i32, i32),
}

impl Positioner {
    /// Taille et rectangle d'ancrage posés (sinon `invalid_positioner`).
    pub fn complete(&self) -> bool {
        self.size.is_some() && self.anchor_rect.is_some()
    }

    /// Rectangle `(x, y, w, h)` du menu, relatif à la géométrie du parent, contraint à
    /// `bounds` (l'écran dans les mêmes coordonnées).
    pub fn place(&self, bounds: (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
        let (w, h) = self.size.unwrap_or((1, 1));
        let (x, _) = self.axis(true, self.anchor, self.gravity, w);
        let (y, _) = self.axis(false, self.anchor, self.gravity, h);
        let (bx, by, bw, bh) = bounds;
        let (x, w) = self.constrain(true, x, w, bx, bx.saturating_add(bw));
        let (y, h) = self.constrain(false, y, h, by, by.saturating_add(bh));
        (x, y, w, h)
    }

    /// Position sur un axe pour l'ancre et la gravité données (sans contrainte).
    fn axis(&self, horizontal: bool, anchor: u32, gravity: u32, len: i32) -> (i32, i32) {
        let (ax, ay, aw, ah) = self.anchor_rect.unwrap_or((0, 0, 0, 0));
        let (start, size, off) = if horizontal { (ax, aw, self.offset.0) } else { (ay, ah, self.offset.1) };
        // Côté de l'ancre et de la gravité sur cet axe : -1 (gauche/haut), 0 (centre), 1.
        let side = |v: u32| -> i32 {
            use xdg_positioner::anchor::*;
            match (horizontal, v) {
                (true, LEFT | TOP_LEFT | BOTTOM_LEFT) => -1,
                (true, RIGHT | TOP_RIGHT | BOTTOM_RIGHT) => 1,
                (false, TOP | TOP_LEFT | TOP_RIGHT) => -1,
                (false, BOTTOM | BOTTOM_LEFT | BOTTOM_RIGHT) => 1,
                _ => 0,
            }
        };
        let point = match side(anchor) {
            -1 => start,
            1 => start.saturating_add(size),
            _ => start.saturating_add(size / 2),
        };
        let pos = match side(gravity) {
            -1 => point.saturating_sub(len),
            1 => point,
            _ => point.saturating_sub(len / 2),
        };
        (pos.saturating_add(off), len)
    }

    fn constrain(&self, horizontal: bool, pos: i32, len: i32, lo: i32, hi: i32) -> (i32, i32) {
        use xdg_positioner::constraint_adjustment::*;
        let out = |p: i32, l: i32| p < lo || p.saturating_add(l) > hi;
        if !out(pos, len) {
            return (pos, len);
        }
        let (flip, slide, resize) = if horizontal { (FLIP_X, SLIDE_X, RESIZE_X) } else { (FLIP_Y, SLIDE_Y, RESIZE_Y) };
        let (mut pos, mut len) = (pos, len);
        if self.adjust & flip != 0 {
            let (p, _) = self.axis(horizontal, flip_value(horizontal, self.anchor), flip_value(horizontal, self.gravity), len);
            if !out(p, len) {
                return (p, len);
            }
        }
        if self.adjust & slide != 0 {
            if pos.saturating_add(len) > hi {
                pos = hi.saturating_sub(len);
            }
            if pos < lo {
                pos = lo;
            }
            if !out(pos, len) {
                return (pos, len);
            }
        }
        if self.adjust & resize != 0 {
            let (a, b) = (pos.max(lo), pos.saturating_add(len).min(hi));
            if b > a {
                pos = a;
                len = b - a;
            }
        }
        (pos, len)
    }
}

/// Ancre ou gravité retournée sur un axe (gauche ↔ droite, ou haut ↔ bas).
fn flip_value(horizontal: bool, v: u32) -> u32 {
    use xdg_positioner::anchor::*;
    if horizontal {
        match v {
            LEFT => RIGHT,
            RIGHT => LEFT,
            TOP_LEFT => TOP_RIGHT,
            TOP_RIGHT => TOP_LEFT,
            BOTTOM_LEFT => BOTTOM_RIGHT,
            BOTTOM_RIGHT => BOTTOM_LEFT,
            o => o,
        }
    } else {
        match v {
            TOP => BOTTOM,
            BOTTOM => TOP,
            TOP_LEFT => BOTTOM_LEFT,
            BOTTOM_LEFT => TOP_LEFT,
            TOP_RIGHT => BOTTOM_RIGHT,
            BOTTOM_RIGHT => TOP_RIGHT,
            o => o,
        }
    }
}

/// Requêtes de `xdg_positioner`.
pub(crate) fn positioner_request(id: ObjectId, p: &Rc<RefCell<Positioner>>, op: u16, req: &Request<'_, Fd>) -> Result<(), ProtocolError> {
    let a = &req.args;
    let invalid = |m: String| ProtocolError::new(id, xdg_positioner::error::INVALID_INPUT, m);
    let mut p = p.borrow_mut();
    match op {
        xdg_positioner::req::SET_SIZE => {
            let (w, h) = (a.int(0), a.int(1));
            if w <= 0 || h <= 0 {
                return Err(invalid(format!("taille {w}x{h} invalide")));
            }
            p.size = Some((w, h));
        }
        xdg_positioner::req::SET_ANCHOR_RECT => {
            let (x, y, w, h) = (a.int(0), a.int(1), a.int(2), a.int(3));
            if w < 0 || h < 0 {
                return Err(invalid(format!("rectangle d'ancrage {w}x{h} invalide")));
            }
            p.anchor_rect = Some((x, y, w, h));
        }
        xdg_positioner::req::SET_ANCHOR => {
            let v = a.uint(0);
            if v > xdg_positioner::anchor::BOTTOM_RIGHT {
                return Err(invalid(format!("ancre {v} invalide")));
            }
            p.anchor = v;
        }
        xdg_positioner::req::SET_GRAVITY => {
            let v = a.uint(0);
            if v > xdg_positioner::gravity::BOTTOM_RIGHT {
                return Err(invalid(format!("gravite {v} invalide")));
            }
            p.gravity = v;
        }
        xdg_positioner::req::SET_CONSTRAINT_ADJUSTMENT => p.adjust = a.uint(0) & 63,
        xdg_positioner::req::SET_OFFSET => p.offset = (a.int(0), a.int(1)),
        // set_reactive, set_parent_size, set_parent_configure : le menu ne suit pas encore
        // son parent pendant un déplacement (la saisie le ferme au premier clic de toute
        // façon).
        _ => {}
    }
    Ok(())
}

/// Fenêtre de surimpression d'un menu (sans décoration, au-dessus de tout).
pub(crate) struct Overlay {
    pub inner: Arc<Mutex<WindowInner>>,
    pub events: Queue<Event>,
}

impl Overlay {
    pub fn new(pos: Coord, w: usize, h: usize) -> Result<Overlay, &'static str> {
        let wm = window_manager::WINDOW_MANAGER.get().ok_or("gestionnaire de fenetres absent")?;
        let fb = Framebuffer::new(w.max(1), h.max(1), None)?;
        let events = Queue::with_capacity(128);
        let mut inner = WindowInner::new(pos, fb, events.clone(), None);
        inner.border_size = 0;
        inner.title_bar_height = 0;
        inner.corner_radius = 0;
        inner.on_event = Some(crate::window_event);
        let inner = Arc::new(Mutex::new(inner));
        wm.lock().add_overlay(&inner);
        Ok(Overlay { inner, events })
    }

    /// Nouvelle taille ou nouvelle place (le contenu est à recopier en entier).
    pub fn reshape(&mut self, pos: Coord, w: usize, h: usize) -> Result<(), &'static str> {
        let old = self.inner.lock().get_envelope();
        {
            let mut i = self.inner.lock();
            let (cw, ch) = i.get_size();
            if (cw, ch) != (w.max(1), h.max(1)) {
                let mut fb = Framebuffer::new(w.max(1), h.max(1), None)?;
                fb.fill(AlphaPixel { alpha: 255, ..Default::default() });
                *i.framebuffer_mut() = fb;
            }
            i.set_position(pos);
        }
        window_manager::damage(old);
        window_manager::damage(self.inner.lock().get_envelope());
        Ok(())
    }

    pub fn position(&self) -> Coord {
        self.inner.lock().get_position()
    }

    /// Le point d'écran `g` est-il dans la fenêtre ?
    pub fn contains(&self, g: Coord) -> bool {
        let i = self.inner.lock();
        i.contains(g - i.get_position())
    }

    /// Présente `area` (coordonnées de la fenêtre).
    pub fn render(&self, area: Rectangle) {
        let pos = self.position();
        window_manager::damage(Rectangle { top_left: pos + area.top_left, bottom_right: pos + area.bottom_right });
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        if let Some(wm) = window_manager::WINDOW_MANAGER.get() {
            let mut wm = wm.lock();
            if let Err(e) = wm.delete_window(&self.inner) {
                error!("MaiLand : menu introuvable a sa destruction : {e}");
            }
        }
    }
}

/// Menu d'un `xdg_surface`.
pub(crate) struct Popup {
    pub id: ObjectId,
    pub parent: Weak<RefCell<crate::xdg::XdgSurface>>,
    pub positioner: Positioner,
    /// Rectangle courant relatif à la géométrie du parent.
    pub rect: (i32, i32, i32, i32),
    /// `xdg_popup.grab` demandé.
    pub grab: bool,
    /// Fermé par le serveur (`popup_done` envoyé) : plus rien ne s'affiche.
    pub dismissed: bool,
    pub window: Option<Overlay>,
    /// Taille du contenu affiché (pixels de surface).
    pub shown: (usize, usize),
    /// Pixels de la fenêtre, comptés au client.
    pub pixels: usize,
    /// Partie de la surface affichée (géométrie de la fenêtre), pour le pointeur.
    pub geom: region2d::Rect,
    /// Contenu lu directement dans le tampon du client (composition directe).
    pub direct: bool,
}

/// Menus du client, du plus bas au plus haut, encore vivants.
pub(crate) fn stack(c: &mut State) -> alloc::vec::Vec<Rc<RefCell<crate::xdg::XdgSurface>>> {
    c.popups.retain(|w| w.strong_count() > 0);
    c.popups.iter().filter_map(|w| w.upgrade()).collect()
}
