//! Préparation d'une image de programme : lecture, scripts `#!`, chargement ELF dans un espace
//! neuf, pile initiale. Partagé par la création d'un processus ([`crate::spawn`]) et `execve`.
//!
//! Tout se fait **avant** le point de non-retour d'`execve` : une erreur (fichier absent, format
//! inconnu, mémoire) laisse le processus appelant intact.

use alloc::{string::String, sync::Arc, vec, vec::Vec};
use kernel_config::memory::PAGE_SIZE;
use uvm::{Mm, PageSource};

use crate::linux::errno::*;
use crate::{elf, stack};

const ENOEXEC: i64 = 8;
/// Profondeur maximale d'interpréteurs `#!` imbriqués (comme Linux).
const MAX_SHEBANG_DEPTH: usize = 4;
/// Taille maximale des arguments et de l'environnement (comme `MAX_ARG_STRLEN` × raisonnable).
pub const MAX_ARGS_BYTES: usize = 2 << 20;

/// Programme prêt à démarrer.
pub struct Prepared {
    pub mm: Arc<Mm>,
    pub entry: usize,
    pub sp: usize,
    /// Chemin de l'exécutable (`/proc/self/exe`).
    pub path: String,
    /// Nom court (`comm`).
    pub comm: [u8; 16],
    /// Arguments et environnement dans la pile (`/proc/<pid>/cmdline`).
    pub args: [usize; 4],
    /// Programme 32 bits (i386) : mode compatible, table d'appels i386, espace sous 4 Gio.
    pub compat: bool,
}

/// Programme résolu : chemin de l'ELF, en-tête (début du fichier), arguments finaux.
pub type Resolved = (String, Arc<[u8]>, Vec<String>);

/// Fichier programme ouvert par son cache de pages : en-tête (assez pour la table des
/// segments) et source des pages.
pub struct Program {
    pub head: Arc<[u8]>,
    pub src: Arc<dyn PageSource>,
}

/// Ouvre `path` par le cache de pages ([`crate::pagecache`]) : rien n'est lu au-delà des
/// en-têtes ; les segments viendront du cache, partagé avec les autres processus.
pub fn open_program(path: &str, root: &crate::mount::Loc) -> Option<Program> {
    let file = match crate::mount::resolve(root, path, true).ok()?.target {
        crate::mount::Target::Node(fs_node::FileOrDir::File(f)) => f,
        _ => return None,
    };
    let cache = crate::pagecache::get(&file);
    let len = PageSource::len(&*cache);
    let mut head = vec![0u8; len.min(PAGE_SIZE)];
    cache.read(0, &mut head);
    if head.len() >= 64 && &head[..4] == b"\x7fELF" {
        if let Some(need) = elf::Image::header_span(&head).filter(|&n| n > head.len() && n <= len.min(1 << 20)) {
            head = vec![0u8; need];
            cache.read(0, &mut head);
        }
    }
    Some(Program { head: Arc::from(head.into_boxed_slice()), src: cache })
}

/// Résout `path` (absolu) en image ELF, en suivant les scripts `#!` : renvoie le chemin de l'ELF,
/// son contenu et la liste d'arguments finale.
pub fn resolve(path: &str, argv: Vec<String>, root: &crate::mount::Loc) -> Result<Resolved, i64> {
    let mut path = String::from(path);
    let mut argv = argv;
    for _ in 0..=MAX_SHEBANG_DEPTH {
        let image = open_program(&path, root).ok_or(ENOENT)?.head;
        if image.len() >= 4 && &image[..4] == b"\x7fELF" {
            return Ok((path, image, argv));
        }
        if image.len() >= 2 && &image[..2] == b"#!" {
            let end = image.iter().take(256).position(|&b| b == b'\n').unwrap_or(image.len().min(256));
            let line = core::str::from_utf8(&image[2..end]).map_err(|_| ENOEXEC)?.trim();
            let (interp, arg) = match line.find(|c: char| c == ' ' || c == '\t') {
                Some(i) => (&line[..i], Some(line[i..].trim())),
                None => (line, None),
            };
            if interp.is_empty() {
                return Err(ENOEXEC);
            }
            let mut next = Vec::with_capacity(argv.len() + 2);
            next.push(String::from(interp));
            if let Some(a) = arg.filter(|a| !a.is_empty()) {
                next.push(String::from(a));
            }
            next.push(path.clone());
            next.extend(argv.into_iter().skip(1));
            argv = next;
            path = String::from(interp);
            continue;
        }
        return Err(ENOEXEC);
    }
    Err(ELOOP)
}

/// Charge l'ELF `image` dans un espace neuf avec sa pile initiale.
pub fn prepare(path: &str, image: Arc<[u8]>, argv: &[String], envp: &[String], root: &crate::mount::Loc) -> Result<Prepared, String> {
    // Pages du programme : son cache s'il est dans le VFS, sinon l'image fournie.
    let src: Arc<dyn PageSource> = match open_program(path, root) {
        Some(p) => p.src,
        None => Arc::new(uvm::MemFile(image.clone())),
    };
    let img = elf::Image::parse(&image, src.len()).map_err(String::from)?;
    let compat = img.is32;
    let layout = stack::Layout::randomized(compat);
    let mm = Mm::new(layout.mmap_top).map_err(String::from)?;
    mm.set_task_size(layout.task_size());

    // Interpréteur (`PT_INTERP`) : chargé lui aussi, il reçoit la main.
    let interp = match img.interp(&*src) {
        Some(ipath) => Some((open_program(&ipath, root).ok_or_else(|| alloc::format!("interpreteur introuvable : {}", ipath))?, ipath)),
        None => None,
    };
    // Noms des zones : chemins physiques (liens résolus), comme `/proc/<pid>/maps` de Linux.
    let phys = |p: &str| -> uvm::VmaName { Arc::<str>::from(crate::exe_path(root, p).as_str()).into() };
    let main = elf::load(&mm, &img, src, elf::Role::Main, phys(path)).map_err(String::from)?;
    let interp_loaded = match &interp {
        Some((p, ipath)) => {
            let ii = elf::Image::parse(&p.head, p.src.len()).map_err(String::from)?;
            if ii.is32 != compat {
                return Err(alloc::format!("interpreteur {} : classe ELF differente du programme", ipath));
            }
            Some(elf::load(&mm, &ii, p.src.clone(), elf::Role::Interpreter, phys(ipath)).map_err(String::from)?)
        }
        None => None,
    };
    mm.set_brk_base(main.brk_base + uvm::rand::random_pages(13));
    let entry = interp_loaded.as_ref().map_or(main.entry, |i| i.entry);
    let (vdso, sysinfo) = if compat {
        match crate::vdso::map32(&mm) {
            Some((base, entry)) => (Some(base), Some(entry)),
            None => (None, None),
        }
    } else {
        (crate::vdso::map(&mm), None)
    };
    let built = stack::build(&mm, &layout, path, argv, envp, stack::Images { main: &main, interp: interp_loaded.as_ref(), vdso, sysinfo })
        .map_err(|e| alloc::format!("pile initiale : {:?}", e))?;
    let (sp, args) = (built.sp, built.args);

    let mut comm = [0u8; 16];
    let base = path.rsplit('/').next().unwrap_or("");
    for (d, s) in comm.iter_mut().zip(base.bytes().take(15)) {
        *d = s;
    }
    Ok(Prepared { mm, entry, sp, path: String::from(path), comm, args, compat })
}
