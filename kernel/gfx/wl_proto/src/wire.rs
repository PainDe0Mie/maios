//! Format filaire : en-tête, décodage vérifié et encodage des arguments.
//!
//! Message : `[objet: u32][taille << 16 | opcode: u32][arguments…]`, mots de 32 bits dans
//! l'ordre de la machine, taille (en-tête compris) multiple de 4. Chaînes et tableaux : longueur
//! sur 32 bits puis octets complétés à 4 ; une chaîne compte son `NUL` final (longueur 0 = chaîne
//! absente). Les descripteurs voyagent hors bande, dans l'ordre des messages.
//!
//! Le décodeur ne fait confiance à rien : toute longueur est comparée à ce qui reste, en
//! arithmétique sur 64 bits (le débordement de `wl_connection_demarshal` signalé par Jann Horn
//! en 2018 venait d'une addition 32 bits), un message doit être consommé exactement, une chaîne
//! se termine par son seul `NUL`.

use alloc::vec::Vec;
use core::fmt;

use crate::protocols::MAX_ARGS;
use crate::types::{ArgDesc, ArgKind, Fixed, MessageDesc, ObjectId};

/// Taille de l'en-tête.
pub const HEADER: usize = 8;
/// Plus grand message (champ de taille sur 16 bits, multiple de 4).
pub const MAX_MESSAGE: usize = 0xfffc;

/// Argument décodé (les octets empruntent le tampon de réception).
#[derive(PartialEq, Eq, Debug)]
pub enum Arg<'a, F> {
    Int(i32),
    Uint(u32),
    Fixed(Fixed),
    /// Chaîne sans son `NUL` ; `None` : absente (`allow-null`).
    Str(Option<&'a [u8]>),
    /// Objet ; 0 : aucun (`allow-null`).
    Object(ObjectId),
    NewId(ObjectId),
    /// `new_id` sans interface fixée (`wl_registry.bind`).
    NewIdDyn { interface: &'a [u8], version: u32, id: ObjectId },
    Array(&'a [u8]),
    Fd(F),
}

impl<'a, F> Arg<'a, F> {
    fn kind(&self) -> ArgKind {
        match self {
            Arg::Int(_) => ArgKind::Int,
            Arg::Uint(_) => ArgKind::Uint,
            Arg::Fixed(_) => ArgKind::Fixed,
            Arg::Str(_) => ArgKind::String,
            Arg::Object(_) => ArgKind::Object,
            Arg::NewId(_) | Arg::NewIdDyn { .. } => ArgKind::NewId,
            Arg::Array(_) => ArgKind::Array,
            Arg::Fd(_) => ArgKind::Fd,
        }
    }
}

/// Erreur de format.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WireError {
    /// Taille d'en-tête < 8 ou non multiple de 4.
    BadSize,
    /// Un argument dépasse la fin du message.
    Truncated,
    /// Octets en trop après le dernier argument.
    TrailingBytes,
    /// Chaîne absente alors qu'elle est obligatoire.
    NullString,
    /// Chaîne sans `NUL` final, ou avec un `NUL` intérieur.
    BadString,
    /// Objet absent alors qu'il est obligatoire.
    NullObject,
    /// `new_id` nul.
    NullNewId,
    /// Le message porte plus de descripteurs qu'il n'en est arrivé.
    MissingFd,
    /// Type d'un argument différent de la signature (encodage).
    Signature,
    /// Message trop grand pour le champ de taille (encodage).
    TooLarge,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            WireError::BadSize => "taille de message invalide",
            WireError::Truncated => "argument tronqué",
            WireError::TrailingBytes => "octets en trop",
            WireError::NullString => "chaîne obligatoire absente",
            WireError::BadString => "chaîne mal terminée",
            WireError::NullObject => "objet obligatoire absent",
            WireError::NullNewId => "nouvel identifiant nul",
            WireError::MissingFd => "descripteur manquant",
            WireError::Signature => "argument hors signature",
            WireError::TooLarge => "message trop grand",
        };
        f.write_str(s)
    }
}

/// En-tête d'un message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Header {
    pub object: ObjectId,
    pub opcode: u16,
    /// Taille totale, en-tête compris.
    pub size: usize,
}

/// Lit l'en-tête au début de `buf` ; `Ok(None)` s'il n'y a pas encore 8 octets.
pub fn read_header(buf: &[u8]) -> Result<Option<Header>, WireError> {
    if buf.len() < HEADER {
        return Ok(None);
    }
    let object = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let w = u32::from_ne_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let size = (w >> 16) as usize;
    if size < HEADER || size % 4 != 0 {
        return Err(WireError::BadSize);
    }
    Ok(Some(Header { object, opcode: (w & 0xffff) as u16, size }))
}

/// Arguments d'un message, dans l'ordre de la signature.
pub struct Args<'a, F> {
    items: [Option<Arg<'a, F>>; MAX_ARGS],
    len: usize,
}

impl<'a, F> Args<'a, F> {
    fn new() -> Self {
        Args { items: core::array::from_fn(|_| None), len: 0 }
    }

    fn push(&mut self, a: Arg<'a, F>) {
        // La signature générée a au plus `MAX_ARGS` arguments (vérifié par build.rs).
        self.items[self.len] = Some(a);
        self.len += 1;
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, i: usize) -> Option<&Arg<'a, F>> {
        self.items.get(i).and_then(|a| a.as_ref())
    }

    pub(crate) fn get_mut(&mut self, i: usize) -> Option<&mut Arg<'a, F>> {
        self.items.get_mut(i).and_then(|a| a.as_mut())
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arg<'a, F>> {
        self.items[..self.len].iter().filter_map(|a| a.as_ref())
    }

    // Accesseurs typés : le décodage garantit la signature, un mauvais indice est une erreur du
    // serveur ; ils rendent alors une valeur neutre plutôt que de paniquer dans le noyau.

    pub fn int(&self, i: usize) -> i32 {
        match self.get(i) {
            Some(Arg::Int(v)) => *v,
            _ => 0,
        }
    }

    pub fn uint(&self, i: usize) -> u32 {
        match self.get(i) {
            Some(Arg::Uint(v)) => *v,
            _ => 0,
        }
    }

    pub fn fixed(&self, i: usize) -> Fixed {
        match self.get(i) {
            Some(Arg::Fixed(v)) => *v,
            _ => Fixed(0),
        }
    }

    pub fn str(&self, i: usize) -> Option<&'a [u8]> {
        match self.get(i) {
            Some(Arg::Str(s)) => *s,
            _ => None,
        }
    }

    /// Objet (0 : aucun).
    pub fn object(&self, i: usize) -> ObjectId {
        match self.get(i) {
            Some(Arg::Object(v)) => *v,
            _ => 0,
        }
    }

    pub fn new_id(&self, i: usize) -> ObjectId {
        match self.get(i) {
            Some(Arg::NewId(v)) => *v,
            Some(Arg::NewIdDyn { id, .. }) => *id,
            _ => 0,
        }
    }

    pub fn array(&self, i: usize) -> &'a [u8] {
        match self.get(i) {
            Some(Arg::Array(a)) => a,
            _ => &[],
        }
    }

    /// Prend le descripteur `i` (une seule fois).
    pub fn take_fd(&mut self, i: usize) -> Option<F> {
        match self.items.get_mut(i) {
            Some(slot @ Some(Arg::Fd(_))) => match slot.take() {
                Some(Arg::Fd(f)) => {
                    *slot = Some(Arg::Uint(u32::MAX));
                    Some(f)
                }
                _ => None,
            },
            _ => None,
        }
    }
}

impl<'a, F: fmt::Debug> fmt::Debug for Args<'a, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

struct Reader<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    fn u32(&mut self) -> Result<u32, WireError> {
        let s = self.b.get(self.p..self.p + 4).ok_or(WireError::Truncated)?;
        self.p += 4;
        Ok(u32::from_ne_bytes([s[0], s[1], s[2], s[3]]))
    }

    /// `len` octets puis le bourrage jusqu'au mot suivant.
    fn bytes(&mut self, len: u32) -> Result<&'a [u8], WireError> {
        let padded = (u64::from(len) + 3) & !3;
        let rest = (self.b.len() - self.p) as u64;
        if padded > rest {
            return Err(WireError::Truncated);
        }
        let s = &self.b[self.p..self.p + len as usize];
        self.p += padded as usize;
        Ok(s)
    }

    fn string(&mut self, nullable: bool) -> Result<Option<&'a [u8]>, WireError> {
        let len = self.u32()?;
        if len == 0 {
            return if nullable { Ok(None) } else { Err(WireError::NullString) };
        }
        let s = self.bytes(len)?;
        let (last, body) = s.split_last().ok_or(WireError::BadString)?;
        if *last != 0 || body.contains(&0) {
            return Err(WireError::BadString);
        }
        Ok(Some(body))
    }
}

/// Décode le corps d'un message (sans l'en-tête) selon `desc` ; `fd` fournit les descripteurs
/// arrivés, dans l'ordre.
pub fn decode<'a, F>(
    body: &'a [u8],
    desc: &MessageDesc,
    fd: &mut dyn FnMut() -> Option<F>,
) -> Result<Args<'a, F>, WireError> {
    let mut r = Reader { b: body, p: 0 };
    let mut out = Args::new();
    for a in desc.args {
        let v = decode_arg(&mut r, a, fd)?;
        out.push(v);
    }
    if r.p != body.len() {
        return Err(WireError::TrailingBytes);
    }
    Ok(out)
}

fn decode_arg<'a, F>(r: &mut Reader<'a>, a: &ArgDesc, fd: &mut dyn FnMut() -> Option<F>) -> Result<Arg<'a, F>, WireError> {
    Ok(match a.kind {
        ArgKind::Int => Arg::Int(r.u32()? as i32),
        ArgKind::Uint => Arg::Uint(r.u32()?),
        ArgKind::Fixed => Arg::Fixed(Fixed(r.u32()? as i32)),
        ArgKind::String => Arg::Str(r.string(a.nullable)?),
        ArgKind::Object => {
            let id = r.u32()?;
            if id == 0 && !a.nullable {
                return Err(WireError::NullObject);
            }
            Arg::Object(id)
        }
        ArgKind::NewId if a.interface_name.is_none() => {
            let interface = r.string(false)?.ok_or(WireError::NullString)?;
            let version = r.u32()?;
            let id = r.u32()?;
            if id == 0 {
                return Err(WireError::NullNewId);
            }
            Arg::NewIdDyn { interface, version, id }
        }
        ArgKind::NewId => {
            let id = r.u32()?;
            if id == 0 {
                return Err(WireError::NullNewId);
            }
            Arg::NewId(id)
        }
        ArgKind::Array => {
            let len = r.u32()?;
            Arg::Array(r.bytes(len)?)
        }
        ArgKind::Fd => Arg::Fd(fd().ok_or(WireError::MissingFd)?),
    })
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_ne_bytes());
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8], nul: bool) {
    let len = b.len() + nul as usize;
    put_u32(out, len as u32);
    out.extend_from_slice(b);
    if nul {
        out.push(0);
    }
    let pad = (4 - len % 4) % 4;
    out.extend(core::iter::repeat(0).take(pad));
}

/// Encode un message à la fin de `out` (sans rien laisser en cas d'erreur) ; les descripteurs
/// passent à `fd` dans l'ordre.
pub fn encode<'a, F>(
    out: &mut Vec<u8>,
    object: ObjectId,
    opcode: u16,
    desc: &MessageDesc,
    args: impl IntoIterator<Item = Arg<'a, F>>,
    fd: &mut dyn FnMut(F),
) -> Result<(), WireError> {
    let start = out.len();
    put_u32(out, object);
    put_u32(out, 0);
    let mut fds = Vec::new();
    let mut n = 0;
    let r = (|| {
        for v in args {
            let a = desc.args.get(n).ok_or(WireError::Signature)?;
            n += 1;
            if v.kind() != a.kind {
                return Err(WireError::Signature);
            }
            match v {
                Arg::Int(x) => put_u32(out, x as u32),
                Arg::Uint(x) => put_u32(out, x),
                Arg::Fixed(x) => put_u32(out, x.0 as u32),
                Arg::Str(None) if a.nullable => put_u32(out, 0),
                Arg::Str(None) => return Err(WireError::NullString),
                Arg::Str(Some(s)) => {
                    if s.contains(&0) {
                        return Err(WireError::BadString);
                    }
                    put_bytes(out, s, true)
                }
                Arg::Object(0) if !a.nullable => return Err(WireError::NullObject),
                Arg::Object(id) => put_u32(out, id),
                Arg::NewId(0) | Arg::NewIdDyn { id: 0, .. } => return Err(WireError::NullNewId),
                Arg::NewId(id) if a.interface_name.is_some() => put_u32(out, id),
                Arg::NewIdDyn { interface, version, id } if a.interface_name.is_none() => {
                    if interface.contains(&0) {
                        return Err(WireError::BadString);
                    }
                    put_bytes(out, interface, true);
                    put_u32(out, version);
                    put_u32(out, id);
                }
                Arg::NewId(_) | Arg::NewIdDyn { .. } => return Err(WireError::Signature),
                Arg::Array(b) => put_bytes(out, b, false),
                Arg::Fd(f) => fds.push(f),
            }
        }
        if n != desc.args.len() {
            return Err(WireError::Signature);
        }
        let size = out.len() - start;
        if size > MAX_MESSAGE {
            return Err(WireError::TooLarge);
        }
        let w = (size as u32) << 16 | u32::from(opcode);
        out[start + 4..start + 8].copy_from_slice(&w.to_ne_bytes());
        Ok(())
    })();
    match r {
        Ok(()) => {
            for f in fds {
                fd(f);
            }
            Ok(())
        }
        Err(e) => {
            out.truncate(start);
            Err(e)
        }
    }
}
