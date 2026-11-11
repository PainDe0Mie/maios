//! `wl_shm`, `wl_shm_pool`, `wl_buffer` : tampons en mémoire partagée.
//!
//! Un pool garde le fichier reçu (un `memfd` en pratique) et l'objet partagé de ses projections
//! `MAP_SHARED` (`maiproc::pagecache::shared`) : les pages que le client a écrites par sa
//! projection sont lues dans leurs frames, les autres dans le fichier, et rien au-delà de sa fin
//! (zéros). Aucune projection côté serveur, donc ni `SIGBUS` ni pool tronqué dangereux.
//!
//! **Composition directe** : un tampon affiché tel quel ([`crate::view::View::is_identity`])
//! n'est pas copié ; MaiCompositor le lit au moment de composer par un [`ShmPlane`]. Le tampon
//! est alors gardé jusqu'à ce que plus aucune fenêtre ne le lise : le dernier plan qui le
//! désigne libère son [`ReleaseToken`], qui demande le `wl_buffer.release` à la tâche
//! `mailand` (le plan peut disparaître dans la tâche du compositeur).

use alloc::{
    format,
    rc::Rc,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::cell::{Cell, RefCell};
use core::sync::atomic::{AtomicU64, Ordering};
use framebuffer::AlphaPixel;
use window_inner::ContentPlane;
use fs_node::FileRef;
use maiproc::fd::Node;
use uvm::ShmObject;
use wl_proto::protocols::*;
use wl_proto::{ObjectId, ProtocolError, Request};

use crate::client::{Fd, Obj, State};

/// Formats annoncés (et seuls acceptés) : ceux que tout client sait produire, et qui ont la
/// disposition mémoire des pixels de MaiCompositor (BGRA little-endian).
pub(crate) const FORMATS: [u32; 2] = [wl_shm::format::ARGB8888, wl_shm::format::XRGB8888];

/// Plus grand côté d'un tampon (comme le `INT16_MAX` des tampons de Mesa).
const MAX_SIDE: i32 = 1 << 15;

pub(crate) struct Pool {
    /// Fichier gardé ouvert tant que des tampons s'en servent.
    _file: FileRef,
    shm: Arc<ShmObject>,
    size: Cell<usize>,
}

/// Contenu d'un tampon.
pub(crate) enum Content {
    /// Pixels dans un pool `wl_shm`.
    Shm { pool: Rc<Pool>, offset: usize, stride: usize, format: u32 },
    /// `wp_single_pixel_buffer_manager_v1` : une couleur, octets BGRA prémultipliés (comme
    /// `ARGB8888` en mémoire).
    Solid([u8; 4]),
}

pub(crate) struct Buffer {
    pub id: ObjectId,
    /// Numéro unique (l'identifiant du client peut être réutilisé après sa destruction).
    pub serial: u64,
    pub width: usize,
    pub height: usize,
    pub content: Content,
    /// Jeton des plans qui lisent ce tampon (composition directe).
    token: RefCell<Weak<ReleaseToken>>,
}

static NEXT_SERIAL: AtomicU64 = AtomicU64::new(1);

/// `wl_buffer.release` à envoyer : plus aucun plan ne lit le tampon.
pub(crate) struct Release {
    pub client: u64,
    pub buffer: ObjectId,
    pub serial: u64,
}

/// Libérations demandées par les plans détruits (dans n'importe quelle tâche).
static RELEASES: spin::Mutex<Vec<Release>> = spin::Mutex::new(Vec::new());

/// Prend les libérations en attente.
pub(crate) fn take_releases() -> Vec<Release> {
    core::mem::take(&mut *RELEASES.lock())
}

/// Partagé par les plans d'un même tampon ; sa destruction demande le `release`.
pub(crate) struct ReleaseToken {
    client: u64,
    buffer: ObjectId,
    serial: u64,
}

impl Drop for ReleaseToken {
    fn drop(&mut self) {
        RELEASES.lock().push(Release { client: self.client, buffer: self.buffer, serial: self.serial });
        crate::wake();
    }
}

/// Lecture directe d'un tampon `wl_shm` par le compositeur.
pub(crate) struct ShmPlane {
    shm: Arc<ShmObject>,
    /// Octet du pixel `(0, 0)` du contenu affiché (coin de la géométrie de la fenêtre).
    base: usize,
    stride: usize,
    opaque: bool,
    _token: Arc<ReleaseToken>,
}

impl ContentPlane for ShmPlane {
    fn read_row(&self, x: usize, y: usize, out: &mut [AlphaPixel]) {
        // SAFETY : `AlphaPixel` est `repr(C)`, quatre octets sans remplissage.
        let bytes = unsafe { core::slice::from_raw_parts_mut(out.as_mut_ptr() as *mut u8, out.len() * 4) };
        self.shm.read_at(self.base + y * self.stride + x * 4, bytes);
        crate::pixels::convert_row(bytes, self.opaque);
    }
}

impl Buffer {
    fn new(id: ObjectId, width: usize, height: usize, content: Content) -> Buffer {
        let serial = NEXT_SERIAL.fetch_add(1, Ordering::Relaxed);
        Buffer { id, serial, width, height, content, token: RefCell::new(Weak::new()) }
    }

    /// Lit la ligne `y`, colonnes `[x, x + dst.len() / 4)`, telle que le client l'a écrite.
    pub fn read_row(&self, x: usize, y: usize, dst: &mut [u8]) {
        match &self.content {
            Content::Shm { pool, offset, stride, .. } => {
                let off = offset + y * stride + x * 4;
                // Le pool a pu grandir depuis la création du tampon, jamais rétrécir : la ligne
                // est dans le pool (vérifié à la création). Le fichier, lui, a pu être tronqué :
                // zéros.
                pool.shm.read_at(off, dst);
            }
            Content::Solid(c) => {
                for px in dst.chunks_exact_mut(4) {
                    px.copy_from_slice(c);
                }
            }
        }
    }

    /// L'octet d'alpha est sans signification (`XRGB8888`), ou la couleur unie est opaque.
    pub fn opaque_format(&self) -> bool {
        match &self.content {
            Content::Shm { format, .. } => *format == wl_shm::format::XRGB8888,
            Content::Solid(c) => c[3] == 255,
        }
    }

    /// Plan de lecture directe du contenu dont le coin est le pixel `(x, y)` du tampon
    /// (`None` : tampon uni, toujours copié). Le tampon reste gardé tant qu'un plan existe.
    pub fn plane(&self, client: u64, x: usize, y: usize) -> Option<Arc<dyn ContentPlane>> {
        let Content::Shm { pool, offset, stride, .. } = &self.content else { return None };
        let live = self.token.borrow().upgrade();
        let token = match live {
            Some(t) => t,
            None => {
                let t = Arc::new(ReleaseToken { client, buffer: self.id, serial: self.serial });
                *self.token.borrow_mut() = Arc::downgrade(&t);
                t
            }
        };
        Some(Arc::new(ShmPlane {
            shm: pool.shm.clone(),
            base: offset + y * stride + x * 4,
            stride: *stride,
            opaque: self.opaque_format(),
            _token: token,
        }))
    }

    /// Couleur d'un tampon uni.
    pub fn solid(&self) -> Option<[u8; 4]> {
        match &self.content {
            Content::Solid(c) => Some(*c),
            Content::Shm { .. } => None,
        }
    }
}

/// Composante `u32` de `create_u32_rgba_buffer` (0 = 0 %, `u32::MAX` = 100 %) → octet, arrondi.
fn channel(v: u32) -> u8 {
    ((v as u64 * 255 + (u32::MAX as u64 / 2)) / u32::MAX as u64) as u8
}

/// `wp_single_pixel_buffer_manager_v1.create_u32_rgba_buffer` : un `wl_buffer` d'un pixel.
pub(crate) fn single_pixel(c: &mut State, req: &Request<'_, Fd>) -> Result<(), ProtocolError> {
    let a = &req.args;
    let (r, g, b, al) = (channel(a.uint(1)), channel(a.uint(2)), channel(a.uint(3)), channel(a.uint(4)));
    // Prémultiplié : une composante ne dépasse pas l'alpha (sinon la couleur n'a pas de sens ;
    // on la borne, comme le ferait le mélange d'un GPU qui sature).
    let px = [b.min(al), g.min(al), r.min(al), al];
    let buf = Buffer::new(a.new_id(0), 1, 1, Content::Solid(px));
    c.create(a.new_id(0), &wl_buffer::INTERFACE, 1, Obj::Buffer(Rc::new(buf)))
}

fn shm_error(id: ObjectId, code: u32, msg: alloc::string::String) -> ProtocolError {
    ProtocolError::new(id, code, msg)
}

pub(crate) fn shm_request(c: &mut State, id: ObjectId, op: u16, req: &mut Request<'_, Fd>) -> Result<(), ProtocolError> {
    if op != wl_shm::req::CREATE_POOL {
        return Ok(());
    }
    let pid = req.args.new_id(0);
    let size = req.args.int(2);
    let fd = req.args.take_fd(1);
    if size <= 0 {
        return Err(shm_error(id, wl_shm::error::INVALID_STRIDE, format!("taille de pool invalide ({size})")));
    }
    let file = match fd.as_deref().map(|f| &f.node) {
        Some(Node::File { file, .. }) => file.clone(),
        _ => return Err(shm_error(id, wl_shm::error::INVALID_FD, alloc::string::String::from("descripteur de pool qui n'est pas un fichier"))),
    };
    let shm = maiproc::pagecache::shared(&file);
    let pool = Pool { _file: file, shm, size: Cell::new(size as usize) };
    c.create(pid, &wl_shm_pool::INTERFACE, req.version, Obj::Pool(Rc::new(pool)))
}

pub(crate) fn pool_request(c: &mut State, id: ObjectId, pool: &Rc<Pool>, op: u16, req: &Request<'_, Fd>) -> Result<(), ProtocolError> {
    let a = &req.args;
    match op {
        wl_shm_pool::req::CREATE_BUFFER => {
            let bid = a.new_id(0);
            let (offset, width, height, stride, format) = (a.int(1), a.int(2), a.int(3), a.int(4), a.uint(5));
            if !FORMATS.contains(&format) {
                return Err(shm_error(id, wl_shm::error::INVALID_FORMAT, format!("format {format:#x} non pris en charge")));
            }
            // Arithmétique sur 64 bits : aucune combinaison d'entiers 32 bits ne déborde.
            let (o, w, h, s) = (offset as i64, width as i64, height as i64, stride as i64);
            let end = o + s * (h - 1) + w * 4;
            if o < 0 || w <= 0 || h <= 0 || w > MAX_SIDE as i64 || h > MAX_SIDE as i64 || s < w * 4 || end > pool.size.get() as i64 {
                return Err(shm_error(
                    id,
                    wl_shm::error::INVALID_STRIDE,
                    format!("tampon invalide : decalage {offset}, {width}x{height}, pas {stride}, pool de {} octets", pool.size.get()),
                ));
            }
            let content = Content::Shm { pool: pool.clone(), offset: offset as usize, stride: stride as usize, format };
            let buf = Buffer::new(bid, width as usize, height as usize, content);
            c.create(bid, &wl_buffer::INTERFACE, 1, Obj::Buffer(Rc::new(buf)))
        }
        wl_shm_pool::req::RESIZE => {
            let size = a.int(0);
            if size <= 0 || (size as usize) < pool.size.get() {
                return Err(shm_error(id, wl_shm::error::INVALID_STRIDE, format!("un pool ne peut pas retrecir ({size})")));
            }
            pool.size.set(size as usize);
            Ok(())
        }
        _ => Ok(()),
    }
}
