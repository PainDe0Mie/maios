//! `wp_viewporter` (recadrage et mise à l'échelle), `wp_fractional_scale_manager_v1` et
//! `wp_single_pixel_buffer_manager_v1`.
//!
//! L'état d'un `wp_viewport` est en double tampon comme celui de la surface : il est porté par
//! sa mise à jour de contenu et vérifié à son application ([`crate::view::View::validate`]).
//! L'échelle préférée est celle de l'écran (1, soit 120/120) : un client qui suit
//! `fractional-scale` dessine donc à la taille de ses pixels de surface.

use alloc::{rc::Rc, string::String};
use core::cell::RefCell;
use wl_proto::protocols::*;
use wl_proto::{Arg, ObjectId, ProtocolError, Request};

use crate::client::{Fd, Obj, State};
use crate::surface::Surface;
use crate::view::SrcRect;

/// Échelle préférée annoncée, en 120ᵉ (`wp_fractional_scale_v1.preferred_scale`).
const PREFERRED_SCALE: u32 = 120;
/// Plus grand côté d'une destination (comme celui d'un tampon).
const MAX_SIDE: i32 = 1 << 15;

fn surface_arg(c: &State, id: ObjectId) -> Option<Rc<RefCell<Surface>>> {
    match c.objs.get(id).map(|o| &o.data) {
        Some(Obj::Surface(s)) => Some(s.clone()),
        _ => None,
    }
}

/// `wp_viewporter.get_viewport`.
pub(crate) fn viewporter_request(c: &mut State, id: ObjectId, op: u16, req: &Request<'_, Fd>) -> Result<(), ProtocolError> {
    if op != wp_viewporter::req::GET_VIEWPORT {
        return Ok(());
    }
    let (vid, sid) = (req.args.new_id(0), req.args.object(1));
    let Some(s) = surface_arg(c, sid) else { return Ok(()) };
    if s.borrow().viewport.is_some() {
        return Err(ProtocolError::new(id, wp_viewporter::error::VIEWPORT_EXISTS, alloc::format!("la surface {sid} a deja un viewport")));
    }
    c.create(vid, &wp_viewport::INTERFACE, req.version, Obj::Viewport(Rc::downgrade(&s)))?;
    s.borrow_mut().viewport = Some(vid);
    Ok(())
}

/// `wp_viewport`.
pub(crate) fn viewport_request(id: ObjectId, w: &alloc::rc::Weak<RefCell<Surface>>, op: u16, req: &Request<'_, Fd>) -> Result<(), ProtocolError> {
    let a = &req.args;
    let live = || match w.upgrade() {
        Some(s) if !s.borrow().dead => Ok(s),
        _ => Err(ProtocolError::new(id, wp_viewport::error::NO_SURFACE, String::from("la surface du viewport n'existe plus"))),
    };
    let bad = |msg: String| ProtocolError::new(id, wp_viewport::error::BAD_VALUE, msg);
    match op {
        wp_viewport::req::DESTROY => {
            // Recadrage et échelle retirés au prochain `commit`.
            if let Some(s) = w.upgrade() {
                let mut sb = s.borrow_mut();
                sb.viewport = None;
                sb.pending.src = Some(None);
                sb.pending.dst = Some(None);
            }
        }
        wp_viewport::req::SET_SOURCE => {
            let s = live()?;
            let (x, y, ww, hh) = (a.fixed(0).raw(), a.fixed(1).raw(), a.fixed(2).raw(), a.fixed(3).raw());
            let unset = -256; // -1.0
            let src = if (x, y, ww, hh) == (unset, unset, unset, unset) {
                None
            } else if x < 0 || y < 0 || ww <= 0 || hh <= 0 {
                return Err(bad(alloc::format!("source ({x}, {y}, {ww}, {hh})/256 invalide")));
            } else {
                Some(SrcRect { x, y, w: ww, h: hh })
            };
            s.borrow_mut().pending.src = Some(src);
        }
        wp_viewport::req::SET_DESTINATION => {
            let s = live()?;
            let (ww, hh) = (a.int(0), a.int(1));
            let dst = if (ww, hh) == (-1, -1) {
                None
            } else if ww <= 0 || hh <= 0 || ww > MAX_SIDE || hh > MAX_SIDE {
                return Err(bad(alloc::format!("destination {ww}x{hh} invalide")));
            } else {
                Some((ww as u32, hh as u32))
            };
            s.borrow_mut().pending.dst = Some(dst);
        }
        _ => {}
    }
    Ok(())
}

/// `wp_fractional_scale_manager_v1.get_fractional_scale`.
pub(crate) fn fractional_manager_request(c: &mut State, id: ObjectId, op: u16, req: &Request<'_, Fd>) -> Result<(), ProtocolError> {
    if op != wp_fractional_scale_manager_v1::req::GET_FRACTIONAL_SCALE {
        return Ok(());
    }
    let (fid, sid) = (req.args.new_id(0), req.args.object(1));
    let Some(s) = surface_arg(c, sid) else { return Ok(()) };
    if s.borrow().has_fraction {
        return Err(ProtocolError::new(
            id,
            wp_fractional_scale_manager_v1::error::FRACTIONAL_SCALE_EXISTS,
            alloc::format!("la surface {sid} a deja un objet d'echelle fractionnaire"),
        ));
    }
    c.create(fid, &wp_fractional_scale_v1::INTERFACE, req.version, Obj::Fractional(Rc::downgrade(&s)))?;
    s.borrow_mut().has_fraction = true;
    c.send(fid, wp_fractional_scale_v1::evt::PREFERRED_SCALE, [Arg::Uint(PREFERRED_SCALE)]);
    Ok(())
}

/// `wp_fractional_scale_v1.destroy`.
pub(crate) fn fractional_request(w: &alloc::rc::Weak<RefCell<Surface>>, op: u16) {
    if op == wp_fractional_scale_v1::req::DESTROY {
        if let Some(s) = w.upgrade() {
            s.borrow_mut().has_fraction = false;
        }
    }
}

/// `wp_single_pixel_buffer_manager_v1`.
pub(crate) fn single_pixel_request(c: &mut State, op: u16, req: &Request<'_, Fd>) -> Result<(), ProtocolError> {
    if op != wp_single_pixel_buffer_manager_v1::req::CREATE_U32_RGBA_BUFFER {
        return Ok(());
    }
    crate::shm::single_pixel(c, req)
}
