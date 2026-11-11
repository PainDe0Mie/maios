//! `xdg_activation_v1` : passage du focus d'une application à une autre.
//!
//! Un client obtient un jeton (`get_activation_token`), le transmet à un autre (variable
//! `XDG_ACTIVATION_TOKEN` d'un programme lancé, D-Bus…), qui demande avec lui l'activation de
//! sa fenêtre. Contre le vol du focus (comme Mutter et KWin), un jeton ne donne le focus que
//! s'il a été émis pour un client qui avait le focus du clavier, avec le numéro d'une de ses
//! entrées récentes ; sinon l'activation est ignorée. Les jetons sont communs à tous les
//! clients, aléatoires (128 bits), à usage unique, et expirent.

use alloc::{format, rc::Rc, string::String, vec::Vec};
use core::cell::RefCell;
use time::{Duration, Instant};
use wl_proto::protocols::*;
use wl_proto::{Arg, ObjectId, ProtocolError, Request};

use crate::client::{Fd, Obj, State};
use crate::Shared;

/// Durée de validité d'un jeton.
const LIFETIME: Duration = Duration::from_secs(10);
/// Jetons gardés au plus (les plus anciens tombent).
const MAX_TOKENS: usize = 64;

/// Jeton en cours de construction (`xdg_activation_token_v1`).
#[derive(Default)]
pub(crate) struct Builder {
    serial: Option<u32>,
    surface: Option<ObjectId>,
    committed: bool,
}

/// Jeton émis.
pub(crate) struct Token {
    text: String,
    issued: Instant,
    /// Émis pour le client qui avait le focus, sur une de ses entrées : il peut activer.
    privileged: bool,
}

/// `xdg_activation_v1`.
pub(crate) fn manager_request(c: &mut State, op: u16, req: &Request<'_, Fd>, sh: &mut Shared) -> Result<(), ProtocolError> {
    match op {
        xdg_activation_v1::req::GET_ACTIVATION_TOKEN => {
            let id = req.args.new_id(0);
            c.create(id, &xdg_activation_token_v1::INTERFACE, 1, Obj::ActivationToken(Rc::new(RefCell::new(Builder::default()))))
        }
        xdg_activation_v1::req::ACTIVATE => {
            let text = String::from_utf8_lossy(req.args.str(0).unwrap_or(&[])).into_owned();
            let surface = req.args.object(1);
            activate(c, sh, &text, surface);
            Ok(())
        }
        _ => Ok(()),
    }
}

/// `xdg_activation_token_v1`.
pub(crate) fn token_request(
    c: &mut State,
    id: ObjectId,
    b: &Rc<RefCell<Builder>>,
    op: u16,
    req: &Request<'_, Fd>,
    sh: &mut Shared,
) -> Result<(), ProtocolError> {
    let a = &req.args;
    if b.borrow().committed && op != xdg_activation_token_v1::req::DESTROY {
        return Err(ProtocolError::new(id, xdg_activation_token_v1::error::ALREADY_USED, String::from("jeton deja valide")));
    }
    match op {
        xdg_activation_token_v1::req::SET_SERIAL => b.borrow_mut().serial = Some(a.uint(0)),
        xdg_activation_token_v1::req::SET_SURFACE => b.borrow_mut().surface = Some(a.object(0)),
        xdg_activation_token_v1::req::COMMIT => {
            b.borrow_mut().committed = true;
            let (serial, surface) = {
                let bb = b.borrow();
                (bb.serial, bb.surface)
            };
            // Privilégié : le client a le focus du clavier, et le numéro est celui de sa
            // dernière entrée (touche, clic) ou d'un événement envoyé depuis (relâchement).
            // La surface, si donnée, doit être celle du focus.
            let focus = crate::xdg::focused_toplevel(c);
            let last = c.seat.input_serial;
            let privileged = focus.is_some()
                && last != 0
                && serial.is_some_and(|s| s.wrapping_sub(last) <= sh.current_serial().wrapping_sub(last))
                && surface.map_or(true, |s| Some(s) == focus);
            let text = format!("maios-{:016x}{:016x}", uvm::rand::random_u64(), uvm::rand::random_u64());
            let now = Instant::now();
            sh.tokens.retain(|t| now.duration_since(t.issued) < LIFETIME);
            if sh.tokens.len() >= MAX_TOKENS {
                sh.tokens.remove(0);
            }
            sh.tokens.push(Token { text: text.clone(), issued: now, privileged });
            c.send(id, xdg_activation_token_v1::evt::DONE, [Arg::Str(Some(text.as_bytes()))]);
        }
        _ => {}
    }
    Ok(())
}

/// `activate(jeton, surface)` : donne le focus à la fenêtre de `surface` si le jeton le permet.
fn activate(c: &mut State, sh: &mut Shared, text: &str, surface: ObjectId) {
    let now = Instant::now();
    let Some(i) = sh.tokens.iter().position(|t| t.text == text) else { return };
    let t = sh.tokens.remove(i);
    if !t.privileged || now.duration_since(t.issued) >= LIFETIME {
        return;
    }
    let s = match c.objs.get(surface).map(|o| &o.data) {
        Some(Obj::Surface(s)) => s.clone(),
        _ => return,
    };
    let x = s.borrow().xdg.upgrade();
    if let Some(x) = x {
        crate::xdg::raise(&x);
    }
}

/// Jetons communs à tous les clients.
pub(crate) type Tokens = Vec<Token>;
