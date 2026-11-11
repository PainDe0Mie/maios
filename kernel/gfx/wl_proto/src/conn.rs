//! Tampons d'une connexion : réception validée contre la table d'objets, émission découpée sur
//! les frontières de messages avec ses descripteurs.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::objmap::{IdError, Lookup, ObjectMap};
use crate::protocols::wl_display;
use crate::types::{ArgKind, Interface, MessageDesc, ObjectId, DISPLAY_ID};
use crate::wire::{self, Arg, Args, WireError, HEADER};

/// Erreur fatale de protocole, à envoyer par `wl_display.error` avant de couper la connexion.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ProtocolError {
    pub object: ObjectId,
    pub code: u32,
    pub message: String,
}

impl ProtocolError {
    pub fn new(object: ObjectId, code: u32, message: String) -> Self {
        ProtocolError { object, code, message }
    }
    fn display(code: u32, message: String) -> Self {
        Self::new(DISPLAY_ID, code, message)
    }
}

/// Requête décodée et validée.
#[derive(Debug)]
pub struct Request<'a, F> {
    pub object: ObjectId,
    pub interface: &'static Interface,
    /// Version de l'objet (celle de ses nouveaux objets à interface fixée).
    pub version: u32,
    pub opcode: u16,
    pub desc: &'static MessageDesc,
    pub args: Args<'a, F>,
}

/// Limites d'une connexion.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Octets reçus et pas encore décodés.
    pub in_bytes: usize,
    /// Descripteurs reçus et pas encore pris par un message.
    pub in_fds: usize,
    /// Octets à émettre (Mutter et KWin : 1 Mio, contre 4 Kio par défaut dans libwayland).
    pub out_bytes: usize,
    /// Descripteurs à émettre.
    pub out_fds: usize,
    /// Places de la table d'objets.
    pub objects: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { in_bytes: 1 << 20, in_fds: 1024, out_bytes: 1 << 20, out_fds: 1024, objects: 1 << 16 }
    }
}

/// Le tampon est plein (le client envoie plus vite qu'on ne lit, ou ne lit plus).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Full;

/// Octets et descripteurs reçus d'un client.
#[derive(Debug)]
pub struct InBuf<F> {
    buf: Vec<u8>,
    head: usize,
    fds: VecDeque<F>,
    max_bytes: usize,
    max_fds: usize,
}

impl<F> InBuf<F> {
    pub fn new(l: &Limits) -> Self {
        InBuf { buf: Vec::new(), head: 0, fds: VecDeque::new(), max_bytes: l.in_bytes, max_fds: l.in_fds }
    }

    /// Octets pas encore décodés.
    pub fn pending(&self) -> usize {
        self.buf.len() - self.head
    }

    /// Place restante (pour ne prendre dans la socket que ce qui tient).
    pub fn room(&self) -> usize {
        self.max_bytes.saturating_sub(self.pending())
    }

    pub fn push_bytes(&mut self, b: &[u8]) -> Result<(), Full> {
        if b.len() > self.room() {
            return Err(Full);
        }
        if self.head > 0 && (self.head == self.buf.len() || self.head >= self.buf.len() / 2) {
            self.buf.drain(..self.head);
            self.head = 0;
        }
        self.buf.extend_from_slice(b);
        Ok(())
    }

    pub fn push_fd(&mut self, f: F) -> Result<(), Full> {
        if self.fds.len() >= self.max_fds {
            return Err(Full);
        }
        self.fds.push_back(f);
        Ok(())
    }

    /// Requête suivante, ou `None` s'il faut attendre d'autres octets. Les requêtes vers un
    /// objet zombie sont décodées (leurs descripteurs consommés) puis sautées.
    pub fn next<D>(&mut self, map: &ObjectMap<D>) -> Result<Option<Request<'_, F>>, ProtocolError> {
        // Deux temps (en-tête sans emprunt, puis décodage) : le vérificateur d'emprunts refuse
        // une boucle qui rend un emprunt dans une branche et continue dans l'autre.
        loop {
            let Some(p) = self.peek(map)? else {
                return Ok(None);
            };
            if p.zombie {
                self.decode(p)?;
                continue;
            }
            let mut args = self.decode(p)?;
            check_objects(map, p.h.object, p.interface, p.desc, &mut args)?;
            let Peeked { h, interface, version, desc, .. } = p;
            return Ok(Some(Request { object: h.object, interface, version, opcode: h.opcode, desc, args }));
        }
    }

    /// En-tête du message suivant s'il est entier, avec sa cible et sa signature.
    fn peek<D>(&self, map: &ObjectMap<D>) -> Result<Option<Peeked>, ProtocolError> {
        let Some(h) = wire::read_header(&self.buf[self.head..])
            .map_err(|e| ProtocolError::display(wl_display::error::INVALID_METHOD, format!("{e}")))?
        else {
            return Ok(None);
        };
        if self.pending() < h.size {
            if h.size > self.max_bytes {
                return Err(ProtocolError::display(wl_display::error::NO_MEMORY, format!("message de {} octets", h.size)));
            }
            return Ok(None);
        }
        let (interface, version, zombie) = match map.lookup(h.object) {
            Lookup::Live(o) => (o.interface, o.version, false),
            Lookup::Zombie(i) => (i, u32::MAX, true),
            Lookup::Unknown => {
                return Err(ProtocolError::display(wl_display::error::INVALID_OBJECT, format!("objet {} inconnu", h.object)))
            }
        };
        let Some(desc) = interface.requests.get(h.opcode as usize) else {
            return Err(ProtocolError::new(
                h.object,
                wl_display::error::INVALID_METHOD,
                format!("{}@{} : requête {} inconnue", interface.name, h.object, h.opcode),
            ));
        };
        if desc.since > version {
            return Err(ProtocolError::new(
                h.object,
                wl_display::error::INVALID_METHOD,
                format!("{}@{}.{} : version {} < {}", interface.name, h.object, desc.name, version, desc.since),
            ));
        }
        Ok(Some(Peeked { h, interface, version, desc, zombie }))
    }

    /// Décode le message annoncé par `peek` et le retire du tampon.
    fn decode(&mut self, p: Peeked) -> Result<Args<'_, F>, ProtocolError> {
        let start = self.head;
        self.head += p.h.size;
        let InBuf { buf, fds, .. } = self;
        let body = &buf[start + HEADER..start + p.h.size];
        wire::decode(body, p.desc, &mut || fds.pop_front()).map_err(|e| {
            let code = if e == WireError::MissingFd { wl_display::error::IMPLEMENTATION } else { wl_display::error::INVALID_METHOD };
            ProtocolError::new(p.h.object, code, format!("{}@{}.{} : {e}", p.interface.name, p.h.object, p.desc.name))
        })
    }
}

#[derive(Clone, Copy)]
struct Peeked {
    h: wire::Header,
    interface: &'static Interface,
    version: u32,
    desc: &'static MessageDesc,
    zombie: bool,
}

/// Objets désignés et nouveaux identifiants : existence, type, plage.
fn check_objects<D, F>(
    map: &ObjectMap<D>,
    target: ObjectId,
    iface: &Interface,
    desc: &MessageDesc,
    args: &mut Args<'_, F>,
) -> Result<(), ProtocolError> {
    let ctx = |a: &str| format!("{}@{}.{} ({a})", iface.name, target, desc.name);
    for (i, a) in desc.args.iter().enumerate() {
        match a.kind {
            ArgKind::Object => {
                let id = args.object(i);
                if id == 0 {
                    continue;
                }
                match map.lookup(id) {
                    Lookup::Live(o) => {
                        if let Some(want) = a.interface_name {
                            if o.interface.name != want {
                                return Err(ProtocolError::display(
                                    wl_display::error::INVALID_OBJECT,
                                    format!("{} : objet {id} de type {} au lieu de {want}", ctx(a.name), o.interface.name),
                                ));
                            }
                        }
                    }
                    // Détruit par le serveur, le client ne le sait pas encore : comme absent
                    // s'il peut l'être (libwayland fait de même côté client).
                    Lookup::Zombie(_) if a.nullable => {
                        if let Some(Arg::Object(v)) = args.get_mut(i) {
                            *v = 0;
                        }
                    }
                    _ => {
                        return Err(ProtocolError::display(
                            wl_display::error::INVALID_OBJECT,
                            format!("{} : objet {id} inconnu", ctx(a.name)),
                        ))
                    }
                }
            }
            ArgKind::NewId => {
                let id = args.new_id(i);
                if let Err(e) = map.check_client_id(id) {
                    let why = match e {
                        IdError::OutOfRange => "hors plage",
                        IdError::InUse => "déjà pris",
                        IdError::TooMany => "trop d'objets",
                    };
                    let code = if e == IdError::TooMany { wl_display::error::NO_MEMORY } else { wl_display::error::INVALID_OBJECT };
                    return Err(ProtocolError::display(code, format!("{} : nouvel identifiant {id} {why}", ctx(a.name))));
                }
                if a.interface_name.is_some() && a.interface.is_none() {
                    return Err(ProtocolError::display(
                        wl_display::error::IMPLEMENTATION,
                        format!("{} : interface {} non prise en charge", ctx(a.name), a.interface_name.unwrap_or("?")),
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Descripteurs au plus par envoi : `MAX_FDS_OUT` de libwayland (28). Au-delà, le `recvmsg` du
/// client tronque les données annexes (`MSG_CTRUNC`) et les descripteurs en trop sont perdus.
pub const MAX_FDS_PER_SEND: usize = 28;

/// Événements à émettre vers un client.
#[derive(Debug)]
pub struct OutBuf<F> {
    buf: Vec<u8>,
    /// Fin (relative à `buf`) de chaque message.
    ends: VecDeque<usize>,
    /// Descripteurs, avec le début de leur message.
    fds: VecDeque<(usize, F)>,
    max_bytes: usize,
    max_fds: usize,
}

/// Erreur d'émission.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SendError {
    /// Le client ne lit plus assez vite.
    Full,
    /// Événement inconnu de la version de l'objet (erreur du serveur).
    Version,
    Wire(WireError),
}

impl<F> OutBuf<F> {
    pub fn new(l: &Limits) -> Self {
        OutBuf { buf: Vec::new(), ends: VecDeque::new(), fds: VecDeque::new(), max_bytes: l.out_bytes, max_fds: l.out_fds }
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Ajoute l'événement `opcode` de `interface` sur `object` (de version `version`).
    pub fn event<'a>(
        &mut self,
        object: ObjectId,
        interface: &'static Interface,
        version: u32,
        opcode: u16,
        args: impl IntoIterator<Item = Arg<'a, F>>,
    ) -> Result<(), SendError> {
        let desc = interface.events.get(opcode as usize).ok_or(SendError::Wire(WireError::Signature))?;
        if desc.since > version {
            return Err(SendError::Version);
        }
        if self.fds.len() + desc.fd_count() > self.max_fds {
            return Err(SendError::Full);
        }
        let start = self.buf.len();
        let mut fds = Vec::new();
        wire::encode(&mut self.buf, object, opcode, desc, args, &mut |f| fds.push(f)).map_err(SendError::Wire)?;
        if self.buf.len() > self.max_bytes {
            self.buf.truncate(start);
            return Err(SendError::Full);
        }
        self.ends.push_back(self.buf.len());
        self.fds.extend(fds.into_iter().map(|f| (start, f)));
        Ok(())
    }

    /// Plus long préfixe de messages entiers tenant dans `max` octets et portant au plus
    /// [`MAX_FDS_PER_SEND`] descripteurs (au moins un message, même plus grand), et le nombre
    /// de descripteurs qu'il porte.
    pub fn chunk(&self, max: usize) -> (&[u8], usize) {
        let mut end = 0;
        let mut nfds = 0;
        let mut fds = self.fds.iter().peekable();
        for &e in &self.ends {
            if e > max && end > 0 {
                break;
            }
            // Descripteurs du message qui finit en `e` (ils sont rangés par début de message).
            let mut more = 0;
            let mut probe = fds.clone();
            while probe.next_if(|(s, _)| *s < e).is_some() {
                more += 1;
            }
            if nfds + more > MAX_FDS_PER_SEND && end > 0 {
                break;
            }
            for _ in 0..more {
                fds.next();
            }
            nfds += more;
            end = e;
            if e >= max {
                break;
            }
        }
        (&self.buf[..end], nfds)
    }

    /// Octets en attente (tests).
    #[cfg(test)]
    pub(crate) fn raw(&self) -> &[u8] {
        &self.buf
    }

    /// Retire les `bytes` premiers octets (une fin de message) et rend leurs descripteurs.
    pub fn consume(&mut self, bytes: usize) -> Vec<F> {
        let bytes = bytes.min(self.buf.len());
        self.buf.drain(..bytes);
        while self.ends.front().is_some_and(|&e| e <= bytes) {
            self.ends.pop_front();
        }
        for e in self.ends.iter_mut() {
            *e -= bytes;
        }
        let mut out = Vec::new();
        while self.fds.front().is_some_and(|(s, _)| *s < bytes) {
            out.push(self.fds.pop_front().unwrap().1);
        }
        for (s, _) in self.fds.iter_mut() {
            *s -= bytes;
        }
        out
    }
}
