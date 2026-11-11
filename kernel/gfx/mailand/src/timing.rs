//! Horloge de présentation côté clients : `wp_presentation` (presentation-time),
//! `wp_fifo_v1` (fifo-v1) et `wp_commit_timer_v1` (commit-timing-v1).
//!
//! * Un `wp_presentation_feedback` suit une mise à jour de contenu : à son application, il
//!   attend la génération de MaiCompositor qui la couvre ; quand l'image correspondante est
//!   présentée, il reçoit `sync_output` puis `presented` (instant réel de l'image, numéro,
//!   période). Il reçoit `discarded` si la mise à jour n'est jamais montrée : surface non
//!   affichée, remplacée par une autre avant l'image, ou détruite.
//! * L'horloge est celle de `CLOCK_MONOTONIC` des processus (même base que
//!   `time::Instant`, voir `maiproc::linux::sys::monotonic_ns`).
//! * Pas de drapeau `vsync` : MaiCompositor présente par recopie dans la mémoire vidéo sur une
//!   horloge logicielle à 60 Hz, sans signal de retour de balayage du matériel. `zero_copy` :
//!   le contenu a été lu directement dans le tampon du client (composition directe).

use alloc::{rc::Rc, string::String};
use core::cell::RefCell;
use wl_proto::protocols::*;
use wl_proto::{Arg, ObjectId, ProtocolError, Request};

use crate::client::{Fd, Obj, State};
use crate::surface::Surface;

/// `CLOCK_MONOTONIC` de Linux.
const CLOCK_MONOTONIC: u32 = 1;
/// Retours de présentation en attente au plus par client.
const MAX_FEEDBACKS: usize = 4096;

/// Retour de présentation d'une mise à jour appliquée, en attente de son image.
pub(crate) struct Feedback {
    pub id: ObjectId,
    pub surface: ObjectId,
    pub gen: u64,
    /// Montrée par composition directe (drapeau `zero_copy`).
    pub zero_copy: bool,
}

/// La globale vient d'être liée : horloge des instants.
pub(crate) fn presentation_bound(c: &mut State, id: ObjectId) {
    c.send(id, wp_presentation::evt::CLOCK_ID, [Arg::Uint(CLOCK_MONOTONIC)]);
}

/// Surface désignée par un argument, vivante.
fn surface_arg(c: &State, id: ObjectId) -> Option<Rc<RefCell<Surface>>> {
    match c.objs.get(id).map(|o| &o.data) {
        Some(Obj::Surface(s)) => Some(s.clone()),
        _ => None,
    }
}

/// `wp_presentation.feedback`.
pub(crate) fn presentation_request(c: &mut State, op: u16, req: &Request<'_, Fd>) -> Result<(), ProtocolError> {
    if op != wp_presentation::req::FEEDBACK {
        return Ok(());
    }
    let (sid, fid) = (req.args.object(0), req.args.new_id(1));
    c.create(fid, &wp_presentation_feedback::INTERFACE, req.version, Obj::Feedback)?;
    match surface_arg(c, sid) {
        Some(s) => s.borrow_mut().pending.feedbacks.push(fid),
        None => discard(c, fid),
    }
    Ok(())
}

/// `discarded`, puis destruction par le serveur.
pub(crate) fn discard(c: &mut State, id: ObjectId) {
    if !matches!(c.objs.get(id).map(|o| &o.data), Some(Obj::Feedback)) {
        return;
    }
    c.send(id, wp_presentation_feedback::evt::DISCARDED, []);
    c.server_destroyed(id);
}

/// Une mise à jour de `sid` vient d'être appliquée, couverte par la génération `gen`.
/// `displayed` : la surface est à l'écran ; `content` : la mise à jour change ses pixels.
#[allow(clippy::too_many_arguments)]
pub(crate) fn feedback_applied(
    c: &mut State,
    sid: ObjectId,
    ids: alloc::vec::Vec<ObjectId>,
    gen: u64,
    displayed: bool,
    content: bool,
    zero_copy: bool,
) {
    if content {
        // Ce qu'une image a déjà montré est présenté, pas écarté (l'image a pu partir pendant
        // que la tâche appliquait la mise à jour).
        presented(c, &window_manager::last_presentation());
        // Le contenu précédent, pas encore présenté, ne le sera jamais.
        let old: alloc::vec::Vec<ObjectId> = c.feedbacks.iter().filter(|f| f.surface == sid).map(|f| f.id).collect();
        c.feedbacks.retain(|f| f.surface != sid);
        for id in old {
            discard(c, id);
        }
    }
    for id in ids {
        if !displayed || c.feedbacks.len() >= MAX_FEEDBACKS {
            discard(c, id);
        } else {
            c.feedbacks.push(Feedback { id, surface: sid, gen, zero_copy });
        }
    }
}

/// La surface `sid` disparaît : ses retours en attente sont écartés.
pub(crate) fn surface_gone(c: &mut State, sid: ObjectId) {
    let old: alloc::vec::Vec<ObjectId> = c.feedbacks.iter().filter(|f| f.surface == sid).map(|f| f.id).collect();
    c.feedbacks.retain(|f| f.surface != sid);
    for id in old {
        discard(c, id);
    }
}

/// Image présentée : `sync_output` puis `presented` pour chaque retour qu'elle couvre.
pub(crate) fn presented(c: &mut State, p: &window_manager::Presentation) {
    if c.feedbacks.iter().all(|f| f.gen > p.gen) {
        return;
    }
    let mut due = alloc::vec::Vec::new();
    c.feedbacks.retain(|f| {
        if f.gen <= p.gen {
            due.push((f.id, f.zero_copy));
            false
        } else {
            true
        }
    });
    let secs = p.time_ns / 1_000_000_000;
    let nsec = (p.time_ns % 1_000_000_000) as u32;
    let refresh = window_manager::FRAME_PERIOD.as_nanos() as u32;
    let outputs = c.outputs.clone();
    for (id, zero_copy) in due {
        if !matches!(c.objs.get(id).map(|o| &o.data), Some(Obj::Feedback)) {
            continue;
        }
        for &o in &outputs {
            c.send(id, wp_presentation_feedback::evt::SYNC_OUTPUT, [Arg::Object(o)]);
        }
        c.send(
            id,
            wp_presentation_feedback::evt::PRESENTED,
            [
                Arg::Uint((secs >> 32) as u32),
                Arg::Uint(secs as u32),
                Arg::Uint(nsec),
                Arg::Uint(refresh),
                Arg::Uint((p.seq >> 32) as u32),
                Arg::Uint(p.seq as u32),
                Arg::Uint(if zero_copy { wp_presentation_feedback::kind::ZERO_COPY } else { 0 }),
            ],
        );
        c.server_destroyed(id);
    }
}

/// Surface d'un objet d'extension : vivante, ou l'erreur `code` sur `id`.
fn alive(id: ObjectId, w: &alloc::rc::Weak<RefCell<Surface>>, code: u32) -> Result<Rc<RefCell<Surface>>, ProtocolError> {
    match w.upgrade() {
        Some(s) if !s.borrow().dead => Ok(s),
        _ => Err(ProtocolError::new(id, code, String::from("la surface n'existe plus"))),
    }
}

/// `wp_fifo_manager_v1.get_fifo`.
pub(crate) fn fifo_manager_request(c: &mut State, id: ObjectId, op: u16, req: &Request<'_, Fd>) -> Result<(), ProtocolError> {
    if op != wp_fifo_manager_v1::req::GET_FIFO {
        return Ok(());
    }
    let (fid, sid) = (req.args.new_id(0), req.args.object(1));
    let Some(s) = surface_arg(c, sid) else { return Ok(()) };
    if s.borrow().has_fifo {
        return Err(ProtocolError::new(id, wp_fifo_manager_v1::error::ALREADY_EXISTS, alloc::format!("la surface {sid} a deja un objet fifo")));
    }
    c.create(fid, &wp_fifo_v1::INTERFACE, req.version, Obj::Fifo(Rc::downgrade(&s)))?;
    s.borrow_mut().has_fifo = true;
    Ok(())
}

/// `wp_fifo_v1`.
pub(crate) fn fifo_request(id: ObjectId, w: &alloc::rc::Weak<RefCell<Surface>>, op: u16) -> Result<(), ProtocolError> {
    match op {
        wp_fifo_v1::req::DESTROY => {
            if let Some(s) = w.upgrade() {
                s.borrow_mut().has_fifo = false;
            }
        }
        wp_fifo_v1::req::SET_BARRIER => alive(id, w, wp_fifo_v1::error::SURFACE_DESTROYED)?.borrow_mut().pending.set_barrier = true,
        wp_fifo_v1::req::WAIT_BARRIER => alive(id, w, wp_fifo_v1::error::SURFACE_DESTROYED)?.borrow_mut().pending.wait_barrier = true,
        _ => {}
    }
    Ok(())
}

/// `wp_commit_timing_manager_v1.get_timer`.
pub(crate) fn timing_manager_request(c: &mut State, id: ObjectId, op: u16, req: &Request<'_, Fd>) -> Result<(), ProtocolError> {
    if op != wp_commit_timing_manager_v1::req::GET_TIMER {
        return Ok(());
    }
    let (tid, sid) = (req.args.new_id(0), req.args.object(1));
    let Some(s) = surface_arg(c, sid) else { return Ok(()) };
    if s.borrow().has_timer {
        return Err(ProtocolError::new(
            id,
            wp_commit_timing_manager_v1::error::COMMIT_TIMER_EXISTS,
            alloc::format!("la surface {sid} a deja un minuteur"),
        ));
    }
    c.create(tid, &wp_commit_timer_v1::INTERFACE, req.version, Obj::Timer(Rc::downgrade(&s)))?;
    s.borrow_mut().has_timer = true;
    Ok(())
}

/// `wp_commit_timer_v1`.
pub(crate) fn timer_request(id: ObjectId, w: &alloc::rc::Weak<RefCell<Surface>>, op: u16, req: &Request<'_, Fd>) -> Result<(), ProtocolError> {
    match op {
        wp_commit_timer_v1::req::DESTROY => {
            if let Some(s) = w.upgrade() {
                s.borrow_mut().has_timer = false;
            }
        }
        wp_commit_timer_v1::req::SET_TIMESTAMP => {
            let a = &req.args;
            let secs = ((a.uint(0) as u64) << 32) | a.uint(1) as u64;
            let nsec = a.uint(2);
            if nsec >= 1_000_000_000 {
                return Err(ProtocolError::new(id, wp_commit_timer_v1::error::INVALID_TIMESTAMP, alloc::format!("tv_nsec {nsec} invalide")));
            }
            let s = alive(id, w, wp_commit_timer_v1::error::SURFACE_DESTROYED)?;
            let mut sb = s.borrow_mut();
            if sb.pending.timestamp.is_some() {
                return Err(ProtocolError::new(id, wp_commit_timer_v1::error::TIMESTAMP_EXISTS, String::from("instant deja donne pour ce commit")));
            }
            sb.pending.timestamp = Some(secs.saturating_mul(1_000_000_000).saturating_add(nsec as u64));
        }
        _ => {}
    }
    Ok(())
}
