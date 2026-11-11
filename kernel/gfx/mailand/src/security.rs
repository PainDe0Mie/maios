//! `wp_security_context_manager_v1` : connexions venues d'un bac à sable (Flatpak, …).
//!
//! Le gestionnaire du bac à sable crée une socket d'écoute, la passe au serveur avec un
//! descripteur de fin (`create_listener(listen_fd, close_fd)`), décrit le bac à sable
//! (`set_sandbox_engine`, `set_app_id`, `set_instance_id`) puis `commit`. Dès lors, le serveur
//! accepte lui-même les connexions de cette socket (l'application enfermée s'y connecte) et
//! les marque : un client marqué ne voit pas les globales privilégiées (ici le gestionnaire
//! lui-même : pas de bac à sable imbriqué, comme le demande le protocole). Le serveur cesse
//! d'écouter quand `close_fd` devient lisible ou est raccroché (fin du bac à sable).

use alloc::{format, rc::Rc, string::String, sync::Arc, vec::Vec};
use core::cell::RefCell;
use log::warn;
use maiproc::object::ev;
use maiproc::unix::KernelListener;
use maiproc::wait::Watcher;
use wl_proto::protocols::*;
use wl_proto::{Interface, ObjectId, ProtocolError, Request};

use crate::client::{Fd, Obj, State};
use crate::{Bell, Shared};

/// Écoutes ouvertes au plus (tous clients) et par client.
const MAX_LISTENERS: usize = 64;
const MAX_PER_CLIENT: usize = 8;
/// Longueur d'une métadonnée.
const MAX_META: usize = 512;

/// Description du bac à sable d'une connexion.
#[derive(Default, Debug)]
pub(crate) struct Sandbox {
    pub engine: Option<String>,
    pub app_id: Option<String>,
    pub instance_id: Option<String>,
}

/// Contexte en construction (`wp_security_context_v1`).
pub(crate) struct Context {
    listen: Option<KernelListener>,
    close: Option<Fd>,
    meta: Sandbox,
    committed: bool,
}

/// Socket d'écoute adoptée.
pub(crate) struct Listener {
    pub owner: u64,
    pub listener: KernelListener,
    close: Fd,
    pub sandbox: Rc<Sandbox>,
    bell: Arc<Bell>,
    watcher: Arc<dyn Watcher>,
}

impl Listener {
    /// Connexions en attente ou fin signalée ?
    pub fn rung(&self) -> bool {
        self.bell.take()
    }

    /// Le bac à sable est fini (`close_fd` lisible, raccroché ou en erreur).
    pub fn closed(&self) -> bool {
        maiproc::poll::poll_file(&self.close, None) & (ev::IN | ev::HUP | ev::ERR) != 0
    }

    /// Désinscription des files observées.
    pub fn release(&self) {
        self.listener.unwatch(&self.watcher);
        maiproc::poll::for_each_queue(&self.close, &mut |q| q.unwatch(&self.watcher));
    }
}

/// Globale interdite aux connexions d'un bac à sable.
pub(crate) fn privileged(iface: &Interface) -> bool {
    iface.is(&wp_security_context_manager_v1::INTERFACE)
}

/// `wp_security_context_manager_v1`.
pub(crate) fn manager_request(c: &mut State, mid: ObjectId, op: u16, req: &mut Request<'_, Fd>) -> Result<(), ProtocolError> {
    if op != wp_security_context_manager_v1::req::CREATE_LISTENER {
        return Ok(()); // destroy
    }
    let id = req.args.new_id(0);
    let listen = req.args.take_fd(1);
    let close = req.args.take_fd(2);
    if c.sandbox.is_some() {
        return Err(ProtocolError::new(mid, wp_security_context_manager_v1::error::NESTED, String::from("connexion deja dans un bac a sable")));
    }
    let Some(listener) = listen.as_deref().and_then(KernelListener::adopt) else {
        return Err(ProtocolError::new(
            mid,
            wp_security_context_manager_v1::error::INVALID_LISTEN_FD,
            String::from("listen_fd n'est pas une socket Unix de flux qui ecoute"),
        ));
    };
    let ctx = Context { listen: Some(listener), close, meta: Sandbox::default(), committed: false };
    c.create(id, &wp_security_context_v1::INTERFACE, 1, Obj::SecurityContext(Rc::new(RefCell::new(ctx))))
}

/// `wp_security_context_v1`.
pub(crate) fn context_request(
    c: &mut State,
    id: ObjectId,
    ctx: &Rc<RefCell<Context>>,
    op: u16,
    req: &Request<'_, Fd>,
    sh: &mut Shared,
) -> Result<(), ProtocolError> {
    use wp_security_context_v1::{error, req as r};
    let mut x = ctx.borrow_mut();
    if op == r::DESTROY {
        return Ok(()); // avant `commit`, la socket est simplement abandonnée
    }
    if x.committed {
        return Err(ProtocolError::new(id, error::ALREADY_USED, String::from("contexte deja valide")));
    }
    let text = |what: &str| -> Result<String, ProtocolError> {
        let raw = req.args.str(0).unwrap_or(&[]);
        if raw.len() > MAX_META {
            return Err(ProtocolError::new(id, error::INVALID_METADATA, format!("{what} trop long")));
        }
        core::str::from_utf8(raw).map(String::from).map_err(|_| ProtocolError::new(id, error::INVALID_METADATA, format!("{what} n'est pas en UTF-8")))
    };
    let set = |slot: &mut Option<String>, v: String, what: &str| -> Result<(), ProtocolError> {
        if slot.is_some() {
            return Err(ProtocolError::new(id, error::ALREADY_SET, format!("{what} deja pose")));
        }
        *slot = Some(v);
        Ok(())
    };
    match op {
        r::SET_SANDBOX_ENGINE => {
            let v = text("moteur")?;
            set(&mut x.meta.engine, v, "moteur")
        }
        r::SET_APP_ID => {
            let v = text("app_id")?;
            set(&mut x.meta.app_id, v, "app_id")
        }
        r::SET_INSTANCE_ID => {
            let v = text("instance_id")?;
            set(&mut x.meta.instance_id, v, "instance_id")
        }
        r::COMMIT => {
            x.committed = true;
            let (Some(listener), Some(close)) = (x.listen.take(), x.close.take()) else { return Ok(()) };
            let mine = sh.security.iter().filter(|l| l.owner == c.client).count();
            if sh.security.len() >= MAX_LISTENERS || mine >= MAX_PER_CLIENT {
                warn!("MaiLand : trop de contextes de securite, socket abandonnee");
                return Ok(());
            }
            let bell = Bell::new();
            let watcher: Arc<dyn Watcher> = bell.clone();
            listener.watch(watcher.clone());
            maiproc::poll::for_each_queue(&close, &mut |q| q.watch(watcher.clone()));
            let sandbox = Rc::new(core::mem::take(&mut x.meta));
            sh.security.push(Listener { owner: c.client, listener, close, sandbox, bell, watcher });
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Retire les écoutes dont le bac à sable est fini ; rend les connexions acceptées, chacune
/// avec la description de son bac à sable.
pub(crate) fn poll(sh: &mut Shared, accepted: &mut Vec<(maiproc::unix::KernelConn, Rc<Sandbox>)>) {
    sh.security.retain(|l| {
        if !l.rung() {
            return true;
        }
        if l.closed() {
            l.release();
            return false;
        }
        while let Some(conn) = l.listener.accept() {
            accepted.push((conn, l.sandbox.clone()));
        }
        true
    });
}
