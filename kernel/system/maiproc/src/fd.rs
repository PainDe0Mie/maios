//! Descripteurs de fichiers d'un processus.
//!
//! Comme sous Linux, un descripteur désigne une *description de fichier ouverte* ([`OpenFile`],
//! partagée par `dup` : position et drapeaux communs) ; `FD_CLOEXEC` appartient au descripteur.

use alloc::{string::String, sync::Arc, vec::Vec};
use app_io::{ImmutableRead, ImmutableWrite, IoStreams};
use core::sync::atomic::{AtomicU32, Ordering};
use fs_node::{DirRef, FileRef};
use spin::Mutex;
use tty::LineDiscipline;

/// Nombre maximal de descripteurs (`RLIMIT_NOFILE`).
pub const NOFILE: usize = 1024;

pub const O_ACCMODE: u32 = 3;
pub const O_WRONLY: u32 = 1;
pub const O_RDWR: u32 = 2;
pub const O_APPEND: u32 = 0x400;
pub const O_NONBLOCK: u32 = 0x800;

/// Objet désigné.
pub enum Node {
    /// Terminal (entrées/sorties standard de l'appelant).
    Console {
        input: Option<Arc<dyn ImmutableRead>>,
        output: Option<Arc<dyn ImmutableWrite>>,
        discipline: Option<Arc<LineDiscipline>>,
    },
    File { file: FileRef, path: String },
    Dir { dir: DirRef, path: String },
    /// `/dev/null`.
    Null,
    /// `/dev/zero`.
    Zero,
    /// `/dev/random`, `/dev/urandom`.
    Random,
    /// Contenu produit à l'ouverture (`/proc/...`), en lecture seule.
    Generated { data: Arc<[u8]>, path: String },
}

/// Description de fichier ouverte.
pub struct OpenFile {
    pub node: Node,
    /// Position (octets ; rang d'entrée pour un répertoire).
    pub pos: Mutex<usize>,
    /// Mode d'accès et drapeaux d'état (`O_APPEND`, `O_NONBLOCK`…).
    pub flags: AtomicU32,
}

impl OpenFile {
    pub fn new(node: Node, flags: u32) -> Arc<OpenFile> {
        Arc::new(OpenFile { node, pos: Mutex::new(0), flags: AtomicU32::new(flags) })
    }

    pub fn flags(&self) -> u32 { self.flags.load(Ordering::Relaxed) }

    pub fn readable(&self) -> bool { self.flags() & O_ACCMODE != O_WRONLY }

    pub fn writable(&self) -> bool {
        let m = self.flags() & O_ACCMODE;
        m == O_WRONLY || m == O_RDWR
    }

    pub fn is_tty(&self) -> bool { matches!(self.node, Node::Console { .. }) }

    /// Écrit sur un terminal (messages du noyau au processus) ; sans effet sinon.
    pub fn write_console(&self, buf: &[u8]) {
        if let Node::Console { output: Some(w), .. } = &self.node {
            let _ = w.write_all(buf);
        }
    }
}

struct Slot {
    file: Arc<OpenFile>,
    cloexec: bool,
}

/// Table des descripteurs.
pub struct FdTable {
    slots: Vec<Option<Slot>>,
}

impl FdTable {
    pub fn new() -> FdTable { FdTable { slots: Vec::new() } }

    /// Table avec 0, 1, 2 sur le terminal donné (ou `/dev/null`).
    pub fn with_stdio(stdio: Option<&IoStreams>) -> FdTable {
        let mut t = FdTable::new();
        match stdio {
            Some(s) => {
                let d = s.discipline.clone();
                let con = |input: Option<Arc<dyn ImmutableRead>>, output: Option<Arc<dyn ImmutableWrite>>, flags| {
                    OpenFile::new(Node::Console { input, output, discipline: d.clone() }, flags)
                };
                t.slots.push(Some(Slot { file: con(Some(s.stdin.clone()), None, 0), cloexec: false }));
                t.slots.push(Some(Slot { file: con(None, Some(s.stdout.clone()), O_WRONLY), cloexec: false }));
                t.slots.push(Some(Slot { file: con(None, Some(s.stderr.clone()), O_WRONLY), cloexec: false }));
            }
            None => {
                for flags in [0, O_WRONLY, O_WRONLY] {
                    t.slots.push(Some(Slot { file: OpenFile::new(Node::Null, flags), cloexec: false }));
                }
            }
        }
        t
    }

    pub fn get(&self, fd: usize) -> Option<Arc<OpenFile>> {
        self.slots.get(fd)?.as_ref().map(|s| s.file.clone())
    }

    /// Plus petit descripteur libre `>= min`.
    pub fn alloc(&mut self, file: Arc<OpenFile>, min: usize, cloexec: bool) -> Option<usize> {
        let fd = (min..NOFILE).find(|&i| self.slots.get(i).map_or(true, |s| s.is_none()))?;
        self.install(fd, file, cloexec);
        Some(fd)
    }

    /// Pose `file` en `fd` (fermant l'ancien), comme `dup2`.
    pub fn install(&mut self, fd: usize, file: Arc<OpenFile>, cloexec: bool) {
        if self.slots.len() <= fd {
            self.slots.resize_with(fd + 1, || None);
        }
        self.slots[fd] = Some(Slot { file, cloexec });
    }

    pub fn close(&mut self, fd: usize) -> bool {
        let closed = matches!(self.slots.get_mut(fd).map(|s| s.take()), Some(Some(_)));
        while matches!(self.slots.last(), Some(None)) {
            self.slots.pop();
        }
        closed
    }

    pub fn cloexec(&self, fd: usize) -> Option<bool> {
        self.slots.get(fd)?.as_ref().map(|s| s.cloexec)
    }

    pub fn set_cloexec(&mut self, fd: usize, on: bool) -> bool {
        match self.slots.get_mut(fd).and_then(|s| s.as_mut()) {
            Some(s) => {
                s.cloexec = on;
                true
            }
            None => false,
        }
    }

    pub fn close_all(&mut self) {
        self.slots.clear();
    }
}

impl Default for FdTable {
    fn default() -> Self { Self::new() }
}
