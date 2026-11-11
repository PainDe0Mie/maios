//! `wl_subcompositor`, `wl_subsurface` : arbres de surfaces.
//!
//! Une sous-surface est placée relativement à son parent, au-dessus ou au-dessous de lui et de
//! ses sœurs. Sa position et l'ordre des enfants font partie de l'état **du parent** : ils
//! s'appliquent avec la prochaine mise à jour de celui-ci. En mode synchronisé (le défaut, et
//! dès qu'un ancêtre l'est), un `commit` de l'enfant ne s'applique pas : il est gardé en cache
//! (fusionné avec le précédent) et appliqué avec la prochaine mise à jour du parent, ce qui
//! rend atomique le changement d'un arbre entier (Firefox, GTK : contenu + décorations).
//!
//! Affichage : la racine de l'arbre (fenêtre xdg, menu) compose ses couches dans le tampon de
//! sa fenêtre ([`crate::pixels::compose`]) : pas de composition directe pour un arbre. Chaque
//! surface d'un arbre garde son tampon jusqu'au suivant (`Surface::hold`), pour qu'une couche
//! puisse être recomposée quand une autre change. Le pointeur va à la couche la plus haute sous
//! lui dont la région d'entrée contient le point ([`pick`]).

use alloc::{
    format,
    rc::{Rc, Weak},
    string::String,
    vec::Vec,
};
use core::cell::RefCell;
use region2d::Rect;
use wl_proto::protocols::*;
use wl_proto::{ObjectId, ProtocolError, Request};

use crate::client::{Fd, Obj, State};
use crate::pixels::Layer;
use crate::surface::{Pending, Role, Surface};
use crate::Shared;

/// Profondeur maximale d'un arbre (au-delà, les descendants ne sont pas affichés).
const MAX_DEPTH: usize = 32;
/// Couches affichées au plus par arbre.
const MAX_LAYERS: usize = 256;
/// Zone « tout » : abîme toute la fenêtre de la racine (bornée ensuite à sa géométrie).
const EVERYTHING: Rect = Rect { x0: -(1 << 24), y0: -(1 << 24), x1: 1 << 24, y1: 1 << 24 };

/// Rôle de sous-surface.
pub(crate) struct Sub {
    /// Objet `wl_subsurface`.
    pub id: ObjectId,
    pub parent: Weak<RefCell<Surface>>,
    /// Position dans le parent (coordonnées de sa surface).
    pub pos: (i32, i32),
    /// `set_position`, appliquée avec la prochaine mise à jour du parent.
    pub pending_pos: Option<(i32, i32)>,
    pub sync: bool,
    /// Mise à jour gardée en mode synchronisé.
    pub cached: Option<Pending>,
    /// Rectangle affiché à la dernière application (coordonnées de la racine).
    pub drawn: Rect,
}

/// Ordre des enfants d'une surface (du plus bas au plus haut, de part et d'autre du parent).
#[derive(Clone, Default)]
pub(crate) struct Stack {
    pub below: Vec<Weak<RefCell<Surface>>>,
    pub above: Vec<Weak<RefCell<Surface>>>,
}

impl Stack {
    fn remove(&mut self, s: &Rc<RefCell<Surface>>) {
        let keep = |w: &Weak<RefCell<Surface>>| w.upgrade().is_some_and(|x| !Rc::ptr_eq(&x, s));
        self.below.retain(keep);
        self.above.retain(keep);
    }

    fn position(list: &[Weak<RefCell<Surface>>], s: &Rc<RefCell<Surface>>) -> Option<usize> {
        list.iter().position(|w| w.upgrade().is_some_and(|x| Rc::ptr_eq(&x, s)))
    }
}

fn surface_of(c: &State, id: ObjectId) -> Option<Rc<RefCell<Surface>>> {
    match c.objs.get(id).map(|o| &o.data) {
        Some(Obj::Surface(s)) => Some(s.clone()),
        _ => None,
    }
}

/// `child` a-t-il `parent` pour parent (lien vivant) ?
fn child_of(child: &Rc<RefCell<Surface>>, parent: &Rc<RefCell<Surface>>) -> bool {
    child.borrow().sub.as_ref().and_then(|s| s.parent.upgrade()).is_some_and(|p| Rc::ptr_eq(&p, parent))
}

/// Enfants vivants de `s`, du plus bas au plus haut : `(sous le parent, au-dessus)`.
fn children(s: &Rc<RefCell<Surface>>) -> (Vec<Rc<RefCell<Surface>>>, Vec<Rc<RefCell<Surface>>>) {
    let st = s.borrow().stack.clone();
    let live = |l: &[Weak<RefCell<Surface>>]| -> Vec<Rc<RefCell<Surface>>> {
        l.iter().filter_map(|w| w.upgrade()).filter(|x| child_of(x, s) && !x.borrow().dead).collect()
    };
    (live(&st.below), live(&st.above))
}

/// `s` a-t-il au moins un enfant vivant ?
pub(crate) fn has_children(s: &Rc<RefCell<Surface>>) -> bool {
    let (b, a) = children(s);
    !b.is_empty() || !a.is_empty()
}

/// Mode synchronisé effectif : le sien ou celui d'un ancêtre sous-surface.
pub(crate) fn effectively_sync(s: &Rc<RefCell<Surface>>) -> bool {
    let mut cur = s.clone();
    for _ in 0..MAX_DEPTH {
        let parent = {
            let cb = cur.borrow();
            let Some(sub) = &cb.sub else { return false };
            if sub.sync {
                return true;
            }
            sub.parent.upgrade()
        };
        match parent {
            Some(p) => cur = p,
            None => return false,
        }
    }
    false
}

/// Racine de l'arbre de `s` et position de `s` dans celle-ci ; `None` si `s` est détachée
/// (sous-surface détruite, parent disparu).
pub(crate) fn root_of(s: &Rc<RefCell<Surface>>) -> Option<(Rc<RefCell<Surface>>, (i32, i32))> {
    let mut cur = s.clone();
    let (mut x, mut y) = (0i32, 0i32);
    for _ in 0..MAX_DEPTH {
        let parent = {
            let cb = cur.borrow();
            if cb.dead {
                return None;
            }
            match &cb.sub {
                None if cb.role == Role::Subsurface => return None,
                None => return Some((cur.clone(), (x, y))),
                Some(sub) => {
                    x = x.saturating_add(sub.pos.0);
                    y = y.saturating_add(sub.pos.1);
                    sub.parent.upgrade()?
                }
            }
        };
        cur = parent;
    }
    None
}

/// Surfaces affichées de l'arbre de `root`, de la plus basse à la plus haute, avec leur
/// position dans la racine. Une surface sans tampon n'est pas affichée, ni ses descendants.
pub(crate) fn tree(root: &Rc<RefCell<Surface>>) -> Vec<(Rc<RefCell<Surface>>, i32, i32)> {
    let mut out = Vec::new();
    walk(root, 0, 0, 0, &mut out);
    out
}

fn walk(s: &Rc<RefCell<Surface>>, x: i32, y: i32, depth: usize, out: &mut Vec<(Rc<RefCell<Surface>>, i32, i32)>) {
    {
        let sb = s.borrow();
        if sb.dead || sb.buffer.is_none() || sb.view.is_none() || depth > MAX_DEPTH {
            return;
        }
    }
    let (below, above) = children(s);
    let at = |c: &Rc<RefCell<Surface>>| {
        let p = c.borrow().sub.as_ref().map_or((0, 0), |sub| sub.pos);
        (x.saturating_add(p.0), y.saturating_add(p.1))
    };
    for c in below.iter() {
        let (cx, cy) = at(c);
        walk(c, cx, cy, depth + 1, out);
    }
    if out.len() < MAX_LAYERS {
        out.push((s.clone(), x, y));
    }
    for c in above.iter() {
        let (cx, cy) = at(c);
        walk(c, cx, cy, depth + 1, out);
    }
}

/// Couches de pixels d'un arbre (pour [`crate::pixels::compose`]).
pub(crate) fn layers(t: &[(Rc<RefCell<Surface>>, i32, i32)]) -> Vec<Layer> {
    t.iter()
        .filter_map(|(s, x, y)| {
            let sb = s.borrow();
            Some(Layer { buf: sb.buffer.clone()?, view: sb.view?, x: *x, y: *y })
        })
        .collect()
}

/// Surface de l'arbre de `root` qui reçoit le pointeur au point `(x, y)` de la racine, et le
/// point dans cette surface.
pub(crate) fn pick(root: &Rc<RefCell<Surface>>, x: i32, y: i32) -> Option<(ObjectId, i32, i32)> {
    let t = tree(root);
    if t.len() <= 1 {
        return Some((root.borrow().id, x, y));
    }
    for (s, sx, sy) in t.iter().rev() {
        let sb = s.borrow();
        let Some(v) = sb.view else { continue };
        let (w, h) = v.size();
        let (lx, ly) = (x - sx, y - sy);
        if lx < 0 || ly < 0 || lx >= w as i32 || ly >= h as i32 {
            continue;
        }
        if sb.input.as_ref().map_or(true, |r| r.contains_point(lx, ly)) {
            return Some((sb.id, lx, ly));
        }
    }
    None
}

/// Abîme la zone `r` (coordonnées de la racine) de l'arbre de la racine `root`.
fn damage_root(root: &Rc<RefCell<Surface>>, r: Rect) {
    if !r.is_empty() {
        root.borrow_mut().tree_damage.add(r);
    }
}

/// `wl_subcompositor`.
pub(crate) fn subcompositor_request(c: &mut State, id: ObjectId, op: u16, req: &Request<'_, Fd>) -> Result<(), ProtocolError> {
    if op != wl_subcompositor::req::GET_SUBSURFACE {
        return Ok(());
    }
    let a = &req.args;
    let nid = a.new_id(0);
    let (Some(s), Some(p)) = (surface_of(c, a.object(1)), surface_of(c, a.object(2))) else { return Ok(()) };
    {
        let sb = s.borrow();
        let reusable = sb.role == Role::Subsurface && sb.sub.is_none();
        if (sb.role != Role::None && !reusable) || sb.xdg.upgrade().is_some() {
            return Err(ProtocolError::new(id, wl_subcompositor::error::BAD_SURFACE, format!("la surface {} a deja un role", sb.id)));
        }
    }
    // Pas de boucle : le parent ne peut être ni la surface ni l'un de ses descendants.
    let mut cur = Some(p.clone());
    for _ in 0..=MAX_DEPTH {
        let Some(x) = cur else { break };
        if Rc::ptr_eq(&x, &s) {
            return Err(ProtocolError::new(id, wl_subcompositor::error::BAD_PARENT, String::from("parent egal a la surface ou descendant d'elle")));
        }
        cur = x.borrow().sub.as_ref().and_then(|sub| sub.parent.upgrade());
    }
    {
        let mut sb = s.borrow_mut();
        sb.role = Role::Subsurface;
        sb.sub = Some(Sub { id: nid, parent: Rc::downgrade(&p), pos: (0, 0), pending_pos: None, sync: true, cached: None, drawn: Rect::EMPTY });
    }
    {
        // Au sommet de la pile de ses sœurs et du parent (aussi dans l'ordre en attente).
        let mut pb = p.borrow_mut();
        pb.stack.above.push(Rc::downgrade(&s));
        if let Some(st) = pb.pending.stack.as_mut() {
            st.above.push(Rc::downgrade(&s));
        }
    }
    c.create(nid, &wl_subsurface::INTERFACE, 1, Obj::Subsurface(Rc::downgrade(&s)))
}

/// `wl_subsurface`.
pub(crate) fn request(c: &mut State, id: ObjectId, w: &Weak<RefCell<Surface>>, op: u16, req: &Request<'_, Fd>, sh: &mut Shared) -> Result<(), ProtocolError> {
    let Some(s) = w.upgrade() else { return Ok(()) };
    // Objet d'une sous-surface déjà remplacée (nouvelle `get_subsurface` sur la même surface).
    if s.borrow().sub.as_ref().map_or(true, |sub| sub.id != id) {
        return Ok(());
    }
    let a = &req.args;
    match op {
        wl_subsurface::req::DESTROY => detach(c, &s, sh)?,
        wl_subsurface::req::SET_POSITION => {
            if let Some(sub) = s.borrow_mut().sub.as_mut() {
                sub.pending_pos = Some((a.int(0), a.int(1)));
            }
        }
        wl_subsurface::req::PLACE_ABOVE | wl_subsurface::req::PLACE_BELOW => {
            let above = op == wl_subsurface::req::PLACE_ABOVE;
            let sibling = surface_of(c, a.object(0));
            let parent = s.borrow().sub.as_ref().and_then(|sub| sub.parent.upgrade());
            let (Some(sib), Some(parent)) = (sibling, parent) else { return Ok(()) };
            let valid = !Rc::ptr_eq(&sib, &s) && (Rc::ptr_eq(&sib, &parent) || child_of(&sib, &parent));
            if !valid {
                return Err(ProtocolError::new(id, wl_subsurface::error::BAD_SURFACE, String::from("place_above/below : ni le parent ni une soeur")));
            }
            restack(&parent, &s, &sib, above);
        }
        wl_subsurface::req::SET_SYNC => {
            if let Some(sub) = s.borrow_mut().sub.as_mut() {
                sub.sync = true;
            }
        }
        wl_subsurface::req::SET_DESYNC => {
            if let Some(sub) = s.borrow_mut().sub.as_mut() {
                sub.sync = false;
            }
            // Plus synchronisée : ce qui attendait le parent s'applique maintenant.
            if !effectively_sync(&s) {
                let cached = s.borrow_mut().sub.as_mut().and_then(|sub| sub.cached.take());
                if let Some(p) = cached {
                    crate::surface::apply_update(c, &s, p, sh)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Nouvel ordre en attente du parent : `s` juste au-dessus (ou au-dessous) de `sib`.
fn restack(parent: &Rc<RefCell<Surface>>, s: &Rc<RefCell<Surface>>, sib: &Rc<RefCell<Surface>>, above: bool) {
    let mut pb = parent.borrow_mut();
    let current = pb.stack.clone();
    let st = pb.pending.stack.get_or_insert(current);
    st.remove(s);
    let w = Rc::downgrade(s);
    if Rc::ptr_eq(sib, parent) {
        if above {
            st.above.insert(0, w);
        } else {
            st.below.push(w);
        }
    } else if let Some(i) = Stack::position(&st.below, sib) {
        st.below.insert(if above { i + 1 } else { i }, w);
    } else if let Some(i) = Stack::position(&st.above, sib) {
        st.above.insert(if above { i + 1 } else { i }, w);
    } else {
        st.above.push(w);
    }
}

/// `wl_subsurface.destroy` (ou destruction de la surface) : la surface quitte l'arbre et n'est
/// plus affichée ; sa mise à jour en cache est abandonnée.
pub(crate) fn detach(c: &mut State, s: &Rc<RefCell<Surface>>, sh: &mut Shared) -> Result<(), ProtocolError> {
    let root = root_of(s).map(|(r, _)| r);
    let (parent, cached) = {
        let mut sb = s.borrow_mut();
        let Some(sub) = sb.sub.take() else { return Ok(()) };
        sb.hold = None;
        (sub.parent.upgrade(), sub.cached)
    };
    if let Some(p) = parent {
        let mut pb = p.borrow_mut();
        pb.stack.remove(s);
        if let Some(st) = pb.pending.stack.as_mut() {
            st.remove(s);
        }
    }
    if let Some(mut p) = cached {
        crate::surface::abandon(c, &mut p, s.borrow().buffer.as_ref());
    }
    if let Some(r) = root {
        damage_root(&r, EVERYTHING);
        repaint(c, &r, sh)?;
    }
    Ok(())
}

/// Mise à jour du parent `s` appliquée : positions et ordre de ses enfants, puis leurs mises à
/// jour en cache (mode synchronisé), récursivement.
pub(crate) fn parent_applied(c: &mut State, s: &Rc<RefCell<Surface>>, restacked: bool, sh: &mut Shared) -> Result<(), ProtocolError> {
    let (below, above) = children(s);
    if below.is_empty() && above.is_empty() {
        return Ok(());
    }
    let mut moved = restacked;
    for ch in below.iter().chain(above.iter()) {
        let mut cb = ch.borrow_mut();
        if let Some(sub) = cb.sub.as_mut() {
            if let Some(p) = sub.pending_pos.take() {
                moved |= p != sub.pos;
                sub.pos = p;
            }
        }
    }
    if moved {
        if let Some((root, _)) = root_of(s) {
            damage_root(&root, EVERYTHING);
        }
    }
    for ch in below.iter().chain(above.iter()) {
        let cached = {
            let mut cb = ch.borrow_mut();
            cb.sub.as_mut().and_then(|sub| sub.cached.take())
        };
        if let Some(p) = cached {
            crate::surface::apply_nested(c, ch, p, sh)?;
        } else if moved {
            // Les rectangles affichés des enfants ont bougé avec le parent.
            refresh_drawn(ch);
        }
    }
    Ok(())
}

fn refresh_drawn(s: &Rc<RefCell<Surface>>) {
    let r = shown_rect(s);
    if let Some(sub) = s.borrow_mut().sub.as_mut() {
        sub.drawn = r;
    }
}

/// Rectangle affiché de `s` dans sa racine (vide si non affichée).
fn shown_rect(s: &Rc<RefCell<Surface>>) -> Rect {
    let Some((_, (x, y))) = root_of(s) else { return Rect::EMPTY };
    let sb = s.borrow();
    match (&sb.buffer, sb.view) {
        (Some(_), Some(v)) => {
            let (w, h) = v.size();
            Rect::new(x, y, x.saturating_add(w as i32), y.saturating_add(h as i32))
        }
        _ => Rect::EMPTY,
    }
}

/// Mise à jour d'une sous-surface appliquée : la zone abîmée passe dans sa racine. Rend la
/// racine si la sous-surface est affichée.
pub(crate) fn child_applied(s: &Rc<RefCell<Surface>>, damage: &region2d::Region) -> Option<Rc<RefCell<Surface>>> {
    let (root, (x, y)) = root_of(s)?;
    let now = shown_rect(s);
    let old = s.borrow().sub.as_ref().map_or(Rect::EMPTY, |sub| sub.drawn);
    if now != old {
        damage_root(&root, old);
        damage_root(&root, now);
    }
    for r in damage.iter() {
        damage_root(&root, r.translate(x, y).intersect(&now));
    }
    if let Some(sub) = s.borrow_mut().sub.as_mut() {
        sub.drawn = now;
    }
    Some(root)
}

/// La racine `root` est-elle affichée ?
pub(crate) fn root_mapped(root: &Rc<RefCell<Surface>>) -> bool {
    root.borrow().xdg.upgrade().is_some_and(|x| x.borrow().mapped())
}

/// Recompose la fenêtre de la racine `root` (zone abîmée de son arbre).
pub(crate) fn repaint(c: &mut State, root: &Rc<RefCell<Surface>>, sh: &mut Shared) -> Result<(), ProtocolError> {
    let Some(x) = root.borrow().xdg.upgrade() else {
        root.borrow_mut().tree_damage.clear();
        return Ok(());
    };
    if !x.borrow().mapped() {
        return Ok(());
    }
    crate::xdg::commit(c, &x, root, crate::surface::Committed::repaint(), sh)
}
