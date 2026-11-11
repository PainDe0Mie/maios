//! Objets noyau désignés par un descripteur sans être des fichiers du VFS : `eventfd`,
//! `timerfd`, `signalfd`, `pidfd`, `epoll`, sockets.
//!
//! Comme les `file_operations` de Linux, chaque objet fournit ses opérations ; la table des
//! descripteurs le porte dans [`crate::fd::Node::Object`]. L'état « prêt » d'un objet se lit par
//! [`FileObject::poll`] et ses changements se signalent par ses files d'attente
//! ([`FileObject::queues`]) : `poll`, `select` et `epoll` s'y inscrivent sans rien savoir de
//! l'objet.

use alloc::{string::String, sync::Arc};
use core::any::Any;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::linux::errno::*;
use crate::linux::SysResult;
use crate::wait::WaitQueue;
use crate::Thread;

/// Événements de `poll`/`epoll` (mêmes valeurs pour les deux).
pub mod ev {
    pub const IN: u32 = 0x1;
    pub const PRI: u32 = 0x2;
    pub const OUT: u32 = 0x4;
    pub const ERR: u32 = 0x8;
    pub const HUP: u32 = 0x10;
    pub const NVAL: u32 = 0x20;
    pub const RDNORM: u32 = 0x40;
    pub const RDBAND: u32 = 0x80;
    pub const WRNORM: u32 = 0x100;
    pub const WRBAND: u32 = 0x200;
    pub const MSG: u32 = 0x400;
    pub const RDHUP: u32 = 0x2000;
    /// Lisible (avec son doublon `RDNORM`).
    pub const READABLE: u32 = IN | RDNORM;
    /// Inscriptible (avec son doublon `WRNORM`).
    pub const WRITABLE: u32 = OUT | WRNORM;
}

/// Type de fichier pour `stat` d'un inode anonyme (Linux : mode 0600 sans type).
pub const S_IFSOCK: u32 = 0o140000;
pub const S_IFCHR: u32 = 0o020000;

static NEXT_INO: AtomicU64 = AtomicU64::new(0x10_0000);

/// Numéro d'inode neuf pour un objet.
pub fn new_ino() -> u64 {
    NEXT_INO.fetch_add(1, Ordering::Relaxed)
}

/// Opérations d'un objet désigné par un descripteur.
pub trait FileObject: Send + Sync + Any {
    /// Pour les conversions vers le type concret (`epoll_ctl`, appels des sockets).
    fn as_any(&self) -> &dyn Any;

    /// Conversion de l'`Arc` (pour garder une référence au type concret).
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync>;

    /// Cible du lien `/proc/<pid>/fd/<n>` (`anon_inode:[eventfd]`, `socket:[1234]`).
    fn link_name(&self) -> String;

    /// `(st_mode, st_ino)`.
    fn stat(&self) -> (u32, u64);

    /// `(st_rdev, st_uid, st_gid)` (périphériques : pseudo-terminaux).
    fn stat_extra(&self) -> (u64, u32, u32) {
        (0, 0, 0)
    }

    /// Lecture (`read`, `recv` sans drapeau).
    fn read(&self, _t: &Thread, _kb: &mut [u8], _nonblock: bool) -> Result<usize, i64> {
        Err(EINVAL)
    }

    /// Écriture (`write`, `send` sans drapeau).
    fn write(&self, _t: &Thread, _kb: &[u8], _nonblock: bool) -> Result<usize, i64> {
        Err(EINVAL)
    }

    /// Événements prêts maintenant ([`ev`]), vus par le fil `t` s'il est donné (un `signalfd`
    /// dépend du fil qui le lit).
    fn poll(&self, t: Option<&Thread>) -> u32;

    /// Files réveillées quand l'état de [`FileObject::poll`] peut avoir changé.
    fn queues<'a>(&'a self, cb: &mut dyn FnMut(&'a WaitQueue));

    fn ioctl(&self, _t: &Thread, _req: u64, _arg: u64) -> SysResult {
        Err(ENOTTY)
    }

    /// Lignes propres à l'objet dans `/proc/<pid>/fdinfo/<n>`.
    fn fdinfo(&self) -> String {
        String::new()
    }

    /// Opérations de socket, si l'objet en est une.
    fn as_socket(&self) -> Option<&dyn crate::socket::SocketOps> {
        None
    }

    /// Mémoire projetée par `mmap` à l'offset `off` (anneaux d'io_uring), si l'objet en a.
    fn mmap_backing(&self, _off: u64) -> Option<Arc<uvm::ShmObject>> {
        None
    }

    /// Octets lisibles sans attendre (`FIONREAD`), si l'objet le sait.
    fn pending_bytes(&self) -> Option<usize> {
        None
    }
}

/// Implémente `as_any` et `into_any` (identiques pour tous les objets).
macro_rules! object_any {
    () => {
        fn as_any(&self) -> &dyn core::any::Any {
            self
        }

        fn into_any(self: alloc::sync::Arc<Self>) -> alloc::sync::Arc<dyn core::any::Any + Send + Sync> {
            self
        }
    };
}
pub(crate) use object_any;

/// Convertit un objet vers son type concret.
pub fn downcast<T: 'static>(o: &dyn FileObject) -> Option<&T> {
    o.as_any().downcast_ref::<T>()
}

/// `Arc` du type concret `T`.
pub fn downcast_arc<T: Send + Sync + 'static>(o: &Arc<dyn FileObject>) -> Option<Arc<T>> {
    o.clone().into_any().downcast::<T>().ok()
}

/// Code d'erreur d'une attente interrompue ou expirée (lecture bloquante d'un objet).
pub fn wait_errno(e: crate::wait::WaitError) -> i64 {
    match e {
        crate::wait::WaitError::Interrupted => crate::signal::ERESTARTSYS,
        crate::wait::WaitError::TimedOut => EAGAIN,
    }
}
