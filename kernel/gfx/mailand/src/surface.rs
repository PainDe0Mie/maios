//! `wl_surface` : état en attente, mises à jour de contenu en file, application.
//!
//! Un `commit` fige l'état en attente en une **mise à jour de contenu** (*content update*,
//! vocabulaire de wayland-protocols 1.38). Elle est appliquée tout de suite si elle est prête,
//! sinon elle attend en file, dans l'ordre des `commit` :
//! * `wp_fifo_v1.wait_barrier` : pas avant que la barrière posée par une mise à jour
//!   précédente (`set_barrier`) soit levée, c'est-à-dire que l'image qui contenait celle-ci ait
//!   été présentée (chaque image reste donc affichée au moins une période) ;
//! * `wp_commit_timer_v1.set_timestamp` : pas avant que la prochaine image de l'horloge puisse
//!   atteindre l'écran à l'instant demandé (« aussi près que possible, jamais avant »).
//!
//! Après application, les rappels `frame` et les `wp_presentation_feedback` attendent la
//! génération de MaiCompositor qui couvre la mise à jour ([`window_manager::frame_gen`]) : ils
//! partent quand l'image qui la contient a été présentée, avec son instant réel.
//!
//! Sous-surfaces ([`crate::subsurface`]) : en mode synchronisé, le `commit` est gardé en cache
//! et appliqué avec le parent ; toute mise à jour appliquée applique à son tour les positions,
//! l'ordre et les caches de ses enfants (`apply_nested`), puis la racine recompose une fois.

use alloc::{
    collections::VecDeque,
    format,
    rc::{Rc, Weak},
    string::String,
    sync::Arc,
    vec::Vec,
};
use core::cell::RefCell;
use core::sync::atomic::Ordering;
use log::warn;
use region2d::{Rect, Region};
use time::Instant;
use wl_proto::protocols::*;
use wl_proto::{ObjectId, ProtocolError, Request, DISPLAY_ID};

use crate::client::{Fd, Obj, State};
use crate::region::{rect, RegionOps};
use crate::shm::Buffer;
use crate::subsurface::{Stack, Sub};
use crate::view::{SrcRect, View, ViewError};
use window_inner::ContentPlane;
use crate::xdg::XdgSurface;
use crate::Shared;

/// Mises à jour de contenu en file au plus par surface.
const MAX_QUEUED: usize = 64;

/// Rôle d'une surface : donné une fois, il ne change plus (même si l'objet du rôle disparaît).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Role {
    None,
    XdgToplevel,
    XdgPopup,
    /// Image du pointeur (`wl_pointer.set_cursor`).
    Cursor,
    /// `wl_subcompositor.get_subsurface`.
    Subsurface,
}

/// État en attente du prochain `commit`, puis mise à jour de contenu en file.
#[derive(Default)]
pub(crate) struct Pending {
    /// `Some(None)` : `attach(NULL)`.
    buffer: Option<Option<Rc<Buffer>>>,
    damage_surface: Region,
    damage_buffer: Region,
    frames: Vec<ObjectId>,
    /// `wp_presentation.feedback`.
    pub feedbacks: Vec<ObjectId>,
    /// `Some(None)` : région opaque vidée.
    opaque: Option<Option<RegionOps>>,
    scale: Option<i32>,
    transform: Option<i32>,
    /// Décalage du contenu (`attach` avant la v5, `offset` ensuite), cumulé jusqu'au `commit`.
    offset: (i32, i32),
    /// `wp_viewport` : `Some(None)` = retiré.
    pub src: Option<Option<SrcRect>>,
    pub dst: Option<Option<(u32, u32)>>,
    /// `wp_fifo_v1`.
    pub set_barrier: bool,
    pub wait_barrier: bool,
    /// `wp_commit_timer_v1` : instant visé, en nanosecondes monotones.
    pub timestamp: Option<u64>,
    /// `xdg_surface.set_window_geometry`, pris au `commit` (état du rôle en double tampon).
    geometry: Option<Rect>,
    /// Nouvel ordre des enfants (`place_above`/`place_below`).
    pub stack: Option<Stack>,
    /// `set_input_region` : `Some(None)` = toute la surface.
    input: Option<Option<RegionOps>>,
}

impl Pending {
    /// Ajoute à cette mise à jour gardée en cache (sous-surface synchronisée) une plus récente.
    /// Un tampon remplacé sans avoir été montré est rendu au client.
    fn merge(&mut self, newer: Pending, c: &mut State, current: Option<&Rc<Buffer>>) {
        if let Some(b) = newer.buffer {
            if let Some(Some(old)) = self.buffer.replace(b) {
                let still = matches!(&self.buffer, Some(Some(n)) if Rc::ptr_eq(n, &old));
                if !still && !current.is_some_and(|cur| Rc::ptr_eq(cur, &old)) {
                    c.release(&old);
                }
            }
        }
        self.damage_surface.add_region(&newer.damage_surface);
        self.damage_buffer.add_region(&newer.damage_buffer);
        self.frames.extend(newer.frames);
        self.feedbacks.extend(newer.feedbacks);
        self.offset = (self.offset.0.saturating_add(newer.offset.0), self.offset.1.saturating_add(newer.offset.1));
        if newer.opaque.is_some() {
            self.opaque = newer.opaque;
        }
        if newer.scale.is_some() {
            self.scale = newer.scale;
        }
        if newer.transform.is_some() {
            self.transform = newer.transform;
        }
        if newer.src.is_some() {
            self.src = newer.src;
        }
        if newer.dst.is_some() {
            self.dst = newer.dst;
        }
        if newer.timestamp.is_some() {
            self.timestamp = newer.timestamp;
        }
        if newer.geometry.is_some() {
            self.geometry = newer.geometry;
        }
        if newer.stack.is_some() {
            self.stack = newer.stack;
        }
        if newer.input.is_some() {
            self.input = newer.input;
        }
        self.set_barrier |= newer.set_barrier;
        self.wait_barrier |= newer.wait_barrier;
    }
}

pub(crate) struct Surface {
    pub id: ObjectId,
    pub version: u32,
    pub pending: Pending,
    /// Mises à jour validées, pas encore appliquées.
    queue: VecDeque<Pending>,
    /// Tampon courant.
    pub buffer: Option<Rc<Buffer>>,
    pub scale: i32,
    pub transform: i32,
    src: Option<SrcRect>,
    dst: Option<(u32, u32)>,
    pub opaque: Option<RegionOps>,
    pub role: Role,
    pub xdg: Weak<RefCell<XdgSurface>>,
    /// Correspondance avec le tampon courant (`None` sans tampon).
    pub view: Option<View>,
    /// Barrière `fifo` : génération de MaiCompositor qui la lève.
    barrier: Option<u64>,
    /// Extensions liées à la surface (une de chaque au plus) : objet `wp_viewport`.
    pub viewport: Option<ObjectId>,
    pub has_fifo: bool,
    pub has_timer: bool,
    pub has_fraction: bool,
    /// `wl_surface.destroy` reçu (les objets d'extension le constatent).
    pub dead: bool,
    /// Rôle de sous-surface (`None` après `wl_subsurface.destroy`).
    pub sub: Option<Sub>,
    /// Ordre des sous-surfaces enfants.
    pub stack: Stack,
    /// Zone à recomposer de l'arbre dont cette surface est la racine (ses coordonnées).
    pub tree_damage: Region,
    /// Tampon courant gardé (surface d'un arbre : il peut être relu à toute recomposition).
    pub hold: Option<Arc<dyn ContentPlane>>,
    /// Région qui reçoit le pointeur (`None` : toute la surface).
    pub input: Option<RegionOps>,
}

/// Résultat de l'application d'une mise à jour, pour le rôle.
pub(crate) struct Committed {
    /// Un tampon (ou `NULL`) a été attaché.
    pub new_buffer: bool,
    /// Zone abîmée, en coordonnées de la surface.
    pub damage: Region,
    /// Décalage du contenu demandé pour ce `commit`.
    pub offset: (i32, i32),
    /// Nouvelle géométrie de fenêtre (`xdg_surface.set_window_geometry`).
    pub geometry: Option<Rect>,
    /// L'ordre des sous-surfaces enfants a changé.
    pub restacked: bool,
}

impl Committed {
    /// Recomposition seule (une sous-surface a changé) : rien de neuf pour la racine.
    pub fn repaint() -> Committed {
        Committed { new_buffer: false, damage: Region::new(), offset: (0, 0), geometry: None, restacked: false }
    }
}

impl Surface {
    pub fn new(id: ObjectId, version: u32) -> Surface {
        Surface {
            id,
            version,
            pending: Pending::default(),
            queue: VecDeque::new(),
            buffer: None,
            scale: 1,
            transform: 0,
            src: None,
            dst: None,
            opaque: None,
            role: Role::None,
            xdg: Weak::new(),
            view: None,
            barrier: None,
            viewport: None,
            has_fifo: false,
            has_timer: false,
            has_fraction: false,
            dead: false,
            sub: None,
            stack: Stack::default(),
            tree_damage: Region::new(),
            hold: None,
            input: None,
        }
    }

    /// Un tampon est attaché, validé, en file ou en attente (interdit avant `get_xdg_surface`).
    pub fn has_buffer(&self) -> bool {
        self.buffer.is_some()
            || matches!(self.pending.buffer, Some(Some(_)))
            || self.queue.iter().any(|p| matches!(p.buffer, Some(Some(_))))
    }

    /// La région opaque couvre-t-elle `r` (coordonnées de la surface) ?
    pub fn opaque_covers(&self, r: Rect) -> bool {
        self.opaque.as_ref().is_some_and(|o| o.covers(r))
    }

    /// La première mise à jour en file peut-elle être appliquée ?
    fn front_ready(&mut self, presented_gen: u64, next_frame_ns: u64) -> bool {
        let Some(p) = self.queue.front() else { return false };
        if let Some(g) = self.barrier {
            if presented_gen >= g {
                self.barrier = None;
            } else if p.wait_barrier {
                return false;
            }
        }
        p.timestamp.map_or(true, |t| t <= next_frame_ns)
    }

    /// Instant visé par la première mise à jour en file, si c'est lui qui la retient.
    pub fn front_timestamp(&self) -> Option<u64> {
        self.queue.front().and_then(|p| p.timestamp)
    }

    /// Applique une mise à jour ; rend aussi ses rappels `frame` et ses retours de présentation.
    fn apply(&mut self, p: Pending) -> Result<(Committed, Vec<ObjectId>, Vec<ObjectId>), ProtocolError> {
        let new_buffer = p.buffer.is_some();
        if let Some(b) = p.buffer {
            self.buffer = b;
        }
        if let Some(s) = p.scale {
            self.scale = s;
        }
        if let Some(t) = p.transform {
            self.transform = t;
        }
        if let Some(o) = p.opaque {
            self.opaque = o;
        }
        if let Some(s) = p.src {
            self.src = s;
        }
        if let Some(d) = p.dst {
            self.dst = d;
        }
        if let Some(i) = p.input {
            self.input = i;
        }
        let restacked = p.stack.is_some();
        if let Some(st) = p.stack {
            self.stack = st;
        }
        let old = self.view;
        self.view = self.buffer.as_ref().map(|b| View {
            buf: (b.width as u32, b.height as u32),
            transform: self.transform as u32,
            scale: self.scale as u32,
            src: self.src,
            dst: self.dst,
        });
        if let Some(v) = &self.view {
            match v.validate() {
                // Comme wlroots : pas d'erreur pour un curseur (thèmes Xcursor de taille impaire
                // à l'échelle 2) ; la taille est arrondie par défaut.
                Err(ViewError::InvalidSize) if self.role == Role::Cursor => {}
                Err(e) => return Err(self.view_error(e)),
                Ok(()) => {}
            }
        }
        let damage = match &self.view {
            None => Region::new(),
            Some(v) => {
                let (w, h) = v.size();
                let full = Rect::new(0, 0, w as i32, h as i32);
                if old != self.view {
                    // Taille, transformation ou recadrage changés : tout est à refaire.
                    Region::from_rect(full)
                } else {
                    let mut d = p.damage_surface.intersect_rect(&full);
                    for r in p.damage_buffer.iter() {
                        d.add(v.buffer_rect_to_surface(*r));
                    }
                    d
                }
            }
        };
        let done = Committed { new_buffer, damage, offset: p.offset, geometry: p.geometry, restacked };
        Ok((done, p.frames, p.feedbacks))
    }

    fn view_error(&self, e: ViewError) -> ProtocolError {
        let (code, obj) = match e {
            ViewError::BadSize => (wp_viewport::error::BAD_SIZE, self.viewport),
            ViewError::OutOfBuffer => (wp_viewport::error::OUT_OF_BUFFER, self.viewport),
            ViewError::InvalidSize => (wl_surface::error::INVALID_SIZE, None),
        };
        let msg = match e {
            ViewError::BadSize => String::from("source du viewport non entiere sans destination"),
            ViewError::OutOfBuffer => String::from("source du viewport hors du tampon"),
            ViewError::InvalidSize => format!("taille du tampon non multiple de l'echelle {}", self.scale),
        };
        ProtocolError::new(obj.unwrap_or(self.id), code, msg)
    }
}

pub(crate) fn request(
    c: &mut State,
    id: ObjectId,
    s: &Rc<RefCell<Surface>>,
    op: u16,
    req: &Request<'_, Fd>,
    sh: &mut Shared,
) -> Result<(), ProtocolError> {
    let a = &req.args;
    match op {
        wl_surface::req::DESTROY => destroy(c, s, sh),
        wl_surface::req::ATTACH => {
            let (bid, x, y) = (a.object(0), a.int(1), a.int(2));
            if req.version >= 5 && (x != 0 || y != 0) {
                return Err(ProtocolError::new(id, wl_surface::error::INVALID_OFFSET, format!("attach avec decalage ({x}, {y}) : utiliser offset")));
            }
            {
                let mut sb = s.borrow_mut();
                sb.pending.offset = (sb.pending.offset.0.saturating_add(x), sb.pending.offset.1.saturating_add(y));
            }
            let buf = match c.objs.get(bid).map(|o| &o.data) {
                Some(Obj::Buffer(b)) => Some(b.clone()),
                _ => None,
            };
            s.borrow_mut().pending.buffer = Some(buf);
        }
        wl_surface::req::DAMAGE => {
            s.borrow_mut().pending.damage_surface.add(rect(a.int(0), a.int(1), a.int(2), a.int(3)));
        }
        wl_surface::req::DAMAGE_BUFFER => {
            s.borrow_mut().pending.damage_buffer.add(rect(a.int(0), a.int(1), a.int(2), a.int(3)));
        }
        wl_surface::req::FRAME => {
            let cb = a.new_id(0);
            c.create(cb, &wl_callback::INTERFACE, 1, Obj::Callback)?;
            s.borrow_mut().pending.frames.push(cb);
        }
        wl_surface::req::SET_OPAQUE_REGION => {
            let rid = a.object(0);
            let ops = match c.objs.get(rid).map(|o| &o.data) {
                Some(Obj::Region(r)) => Some(r.borrow().clone()),
                _ => None,
            };
            s.borrow_mut().pending.opaque = Some(ops);
        }
        wl_surface::req::SET_BUFFER_TRANSFORM => {
            let t = a.int(0);
            if !(0..=7).contains(&t) {
                return Err(ProtocolError::new(id, wl_surface::error::INVALID_TRANSFORM, format!("transformation {t} invalide")));
            }
            s.borrow_mut().pending.transform = Some(t);
        }
        wl_surface::req::SET_BUFFER_SCALE => {
            let k = a.int(0);
            if k <= 0 {
                return Err(ProtocolError::new(id, wl_surface::error::INVALID_SCALE, format!("echelle {k} invalide")));
            }
            s.borrow_mut().pending.scale = Some(k);
        }
        wl_surface::req::OFFSET => {
            let mut sb = s.borrow_mut();
            sb.pending.offset = (sb.pending.offset.0.saturating_add(a.int(0)), sb.pending.offset.1.saturating_add(a.int(1)));
        }
        wl_surface::req::SET_INPUT_REGION => {
            let rid = a.object(0);
            let ops = match c.objs.get(rid).map(|o| &o.data) {
                Some(Obj::Region(r)) => Some(r.borrow().clone()),
                _ => None,
            };
            s.borrow_mut().pending.input = Some(ops);
        }
        wl_surface::req::COMMIT => commit(c, s, sh)?,
        _ => {}
    }
    Ok(())
}

/// `wl_surface.destroy`. Clients peu soigneux : la fenêtre disparaît avec la surface (au lieu
/// de l'erreur `defunct_role_object`, que Mutter et KWin n'envoient pas non plus).
fn destroy(c: &mut State, s: &Rc<RefCell<Surface>>, sh: &mut Shared) {
    let (id, xdg) = {
        let sb = s.borrow();
        (sb.id, sb.xdg.upgrade())
    };
    if let Some(x) = xdg {
        x.borrow_mut().unmap(&mut c.win_pixels);
    }
    // Sous-surface : elle quitte l'arbre (la racine recompose sans elle) ; ses propres enfants
    // ne sont plus affichés (`dead`).
    if let Err(e) = crate::subsurface::detach(c, s, sh) {
        warn!("MaiLand : recomposition apres destruction de la surface {id} : {}", e.message);
    }
    crate::seat::surface_gone(c, sh, id);
    if c.seat.cursor.as_ref().is_some_and(|cur| Rc::ptr_eq(&cur.surface, s)) {
        c.seat.cursor = None;
    }
    // Rien de ce qui attendait ne sera présenté : rappels répondus au plus tôt (pour qu'aucun
    // client n'attende en vain), retours de présentation écartés, tampons en file rendus.
    let mut updates: Vec<Pending> = {
        let mut sb = s.borrow_mut();
        sb.dead = true;
        sb.barrier = None;
        sb.hold = None;
        let mut v: Vec<Pending> = sb.queue.drain(..).collect();
        v.push(core::mem::take(&mut sb.pending));
        v
    };
    let ms = crate::now_ms(Instant::now());
    let current = s.borrow().buffer.clone();
    for p in updates.iter_mut() {
        for f in p.frames.drain(..) {
            c.fire_callback(f, ms);
        }
        for f in p.feedbacks.drain(..) {
            crate::timing::discard(c, f);
        }
        if let Some(Some(b)) = p.buffer.take() {
            if !current.as_ref().is_some_and(|cur| Rc::ptr_eq(cur, &b)) {
                c.release(&b);
            }
        }
    }
    crate::timing::surface_gone(c, id);
    c.waiting.retain(|w| !Rc::ptr_eq(w, s));
}

/// `wl_surface.commit` : l'état en attente devient une mise à jour de contenu.
fn commit(c: &mut State, s: &Rc<RefCell<Surface>>, sh: &mut Shared) -> Result<(), ProtocolError> {
    let xdg = s.borrow().xdg.upgrade();
    let geometry = xdg.and_then(|x| x.borrow_mut().take_pending_geometry());
    if crate::subsurface::effectively_sync(s) {
        // Sous-surface synchronisée : gardée jusqu'à la prochaine mise à jour du parent.
        let (p, prev, current) = {
            let mut sb = s.borrow_mut();
            let p = core::mem::take(&mut sb.pending);
            let prev = sb.sub.as_mut().and_then(|sub| sub.cached.take());
            (p, prev, sb.buffer.clone())
        };
        let merged = match prev {
            Some(mut old) => {
                old.merge(p, c, current.as_ref());
                old
            }
            None => p,
        };
        if let Some(sub) = s.borrow_mut().sub.as_mut() {
            sub.cached = Some(merged);
        }
        return Ok(());
    }
    {
        let mut sb = s.borrow_mut();
        if sb.queue.len() >= MAX_QUEUED {
            return Err(ProtocolError::new(
                DISPLAY_ID,
                wl_display::error::NO_MEMORY,
                format!("plus de {MAX_QUEUED} mises a jour en attente sur la surface {}", sb.id),
            ));
        }
        let mut p = core::mem::take(&mut sb.pending);
        p.geometry = geometry;
        sb.queue.push_back(p);
    }
    flush(c, s, sh)
}

/// Applique les mises à jour prêtes de `s`, dans l'ordre ; une surface qui en garde en file est
/// inscrite dans `c.waiting` (revue à chaque image et à chaque échéance).
pub(crate) fn flush(c: &mut State, s: &Rc<RefCell<Surface>>, sh: &mut Shared) -> Result<(), ProtocolError> {
    let presented = window_manager::last_presentation().gen;
    let next = window_manager::next_frame_ns(Instant::now());
    loop {
        let update = {
            let mut sb = s.borrow_mut();
            if sb.dead || !sb.front_ready(presented, next) {
                break;
            }
            sb.queue.pop_front()
        };
        let Some(u) = update else { break };
        apply_update(c, s, u, sh)?;
    }
    let waiting = !s.borrow().queue.is_empty() && !s.borrow().dead;
    let listed = c.waiting.iter().any(|w| Rc::ptr_eq(w, s));
    if waiting && !listed {
        c.waiting.push(s.clone());
    } else if !waiting && listed {
        c.waiting.retain(|w| !Rc::ptr_eq(w, s));
    }
    Ok(())
}

/// Mise à jour abandonnée (sous-surface détachée avec un cache) : rappels répondus,
/// retours écartés, tampon rendu s'il n'est pas le courant.
pub(crate) fn abandon(c: &mut State, p: &mut Pending, current: Option<&Rc<Buffer>>) {
    let ms = crate::now_ms(Instant::now());
    for f in p.frames.drain(..) {
        c.fire_callback(f, ms);
    }
    for f in p.feedbacks.drain(..) {
        crate::timing::discard(c, f);
    }
    if let Some(Some(b)) = p.buffer.take() {
        if !current.is_some_and(|cur| Rc::ptr_eq(cur, &b)) {
            c.release(&b);
        }
    }
}

/// Applique une mise à jour : état, enfants, rôle (copie des pixels), puis ce qui attend
/// l'image. Une sous-surface recompose ensuite sa racine.
pub(crate) fn apply_update(c: &mut State, s: &Rc<RefCell<Surface>>, p: Pending, sh: &mut Shared) -> Result<(), ProtocolError> {
    apply_one(c, s, p, sh, false)
}

/// Mise à jour en cache d'une sous-surface appliquée avec son parent (la racine recomposera).
pub(crate) fn apply_nested(c: &mut State, s: &Rc<RefCell<Surface>>, p: Pending, sh: &mut Shared) -> Result<(), ProtocolError> {
    apply_one(c, s, p, sh, true)
}

fn apply_one(c: &mut State, s: &Rc<RefCell<Surface>>, p: Pending, sh: &mut Shared, nested: bool) -> Result<(), ProtocolError> {
    crate::STAT_COMMITS.fetch_add(1, Ordering::Relaxed);
    let set_barrier = p.set_barrier;
    let (done, frames, feedbacks) = s.borrow_mut().apply(p)?;
    let new_buffer = done.new_buffer;
    let content = new_buffer || !done.damage.is_empty();
    // Surface d'un arbre : son tampon reste lisible jusqu'au suivant (le précédent est rendu
    // quand son jeton tombe).
    if new_buffer {
        let keep = s.borrow().sub.is_some() || crate::subsurface::has_children(s);
        let buf = s.borrow().buffer.clone();
        let hold = if keep { buf.and_then(|b| b.plane(c.client, 0, 0)) } else { None };
        s.borrow_mut().hold = hold;
    }
    crate::subsurface::parent_applied(c, s, done.restacked, sh)?;
    let (sid, role, xdg) = {
        let sb = s.borrow();
        (sb.id, sb.role, sb.xdg.upgrade())
    };
    // Affichée ? Lue directement dans le tampon (composition directe) ?
    let (displayed, direct) = if let Some(x) = xdg {
        crate::xdg::commit(c, &x, s, done, sh)?;
        let xs = x.borrow();
        (xs.mapped(), xs.direct())
    } else if role == Role::Cursor {
        crate::seat::cursor_committed(c, sh, s, done.offset);
        (c.seat.cursor.as_ref().is_some_and(|cur| Rc::ptr_eq(&cur.surface, s)), false)
    } else if role == Role::Subsurface {
        match crate::subsurface::child_applied(s, &done.damage) {
            Some(root) => {
                if !nested {
                    crate::subsurface::repaint(c, &root, sh)?;
                }
                (crate::subsurface::root_mapped(&root) && s.borrow().buffer.is_some(), false)
            }
            None => (false, false),
        }
    } else {
        (false, false)
    };
    // Ce qui attend l'image qui contient cette mise à jour.
    if !frames.is_empty() || !feedbacks.is_empty() || set_barrier {
        let gen = window_manager::frame_gen();
        c.wait_frames(frames, gen);
        if set_barrier {
            s.borrow_mut().barrier = Some(gen);
        }
        crate::timing::feedback_applied(c, sid, feedbacks, gen, displayed, content, direct);
    }
    // Contenu copié (ou sans usage) : le tampon est rendu tout de suite. Lu directement : il
    // le sera quand plus aucune fenêtre ne le lira (`shm::ReleaseToken`).
    if new_buffer && !direct && s.borrow().hold.is_none() {
        let buf = s.borrow().buffer.clone();
        if let Some(b) = buf {
            c.release(&b);
        }
    }
    Ok(())
}
