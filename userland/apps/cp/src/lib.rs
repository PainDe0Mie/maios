//! `cp [-r] SOURCE DEST` ou `cp [-r] SOURCE... DOSSIER` : copie des fichiers (et des dossiers
//! avec `-r`). Une destination qui est un dossier reçoit la source sous son nom ; un fichier
//! existant est remplacé (tronqué) ; un fichier absent est créé par la fabrique du système de
//! fichiers du dossier (`Directory::create_file`), sinon en mémoire (`memfs`), comme MaiProc.
//! Copie par morceaux de 1 Mio : jamais tout le fichier en mémoire.
#![no_std]
#[macro_use]
extern crate app_io;
extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use fs_node::{DirRef, FileOrDir, FileRef};
use path::Path;
use vfs_node::VFSDirectory;

/// Taille des morceaux copiés.
const CHUNK: usize = 1 << 20;

pub fn main(args: Vec<String>) -> isize {
    let mut opts = getopts::Options::new();
    opts.optflag("r", "recursive", "copie les dossiers et leur contenu");
    opts.optflag("R", "", "comme -r");
    opts.optflag("h", "help", "affiche cette aide");
    let m = match opts.parse(args) {
        Ok(m) => m,
        Err(e) => {
            println!("cp: {e}");
            return -1;
        }
    };
    if m.opt_present("h") {
        println!("{}", opts.usage("Usage: cp [-r] SOURCE DEST\n       cp [-r] SOURCE... DOSSIER"));
        return 0;
    }
    if m.free.len() < 2 {
        println!("cp: operande manquante (cp SOURCE DEST)");
        return -1;
    }
    let Ok(cwd) = task::with_current_task(|t| t.env.lock().working_dir.clone()) else {
        println!("cp: tache courante introuvable");
        return -1;
    };
    let recursive = m.opt_present("r") || m.opt_present("R");
    let (dest, sources) = m.free.split_last().unwrap();
    let dest_p: &Path = dest.as_ref();
    let dest_dir = match dest_p.get(&cwd) {
        Some(FileOrDir::Dir(d)) => Some(d),
        _ => None,
    };
    if sources.len() > 1 && dest_dir.is_none() {
        println!("cp: la cible '{dest}' n'est pas un dossier");
        return -1;
    }
    let mut ret = 0;
    for src in sources {
        let src_p: &Path = src.as_ref();
        let Some(node) = src_p.get(&cwd) else {
            println!("cp: '{src}' introuvable");
            ret = -1;
            continue;
        };
        if matches!(node, FileOrDir::Dir(_)) && !recursive {
            println!("cp: '{src}' est un dossier (utiliser -r)");
            ret = -1;
            continue;
        }
        // Dossier de destination et nom de la copie.
        let target = match &dest_dir {
            Some(d) => src_p.file_name().map(|n| (d.clone(), n.to_string())).ok_or("nom de source invalide"),
            None => split(&cwd, dest),
        };
        let r = target.map_err(String::from).and_then(|(dir, name)| {
            if let FileOrDir::Dir(s) = &node {
                // Pas de copie d'un dossier dans lui-même (ou dans un descendant).
                let mut cur = Some(dir.clone());
                while let Some(c) = cur {
                    if alloc::sync::Arc::ptr_eq(&c, s) {
                        return Err("copie d'un dossier dans lui-meme".to_string());
                    }
                    cur = c.lock().get_parent_dir();
                }
            }
            copy_node(&node, &dir, &name)
        });
        if let Err(e) = r {
            println!("cp: '{src}' -> '{dest}' : {e}");
            ret = -1;
        }
    }
    ret
}

/// Dossier parent et nom de `arg`, relatif à `cwd`.
fn split(cwd: &DirRef, arg: &str) -> Result<(DirRef, String), &'static str> {
    let p: &Path = arg.as_ref();
    let name = p.file_name().ok_or("nom invalide")?;
    let parent = match p.parent() {
        Some(d) if d.components().next().is_some() => d.get_dir(cwd).ok_or("dossier parent introuvable")?,
        _ if arg.starts_with('/') => root::get_root().clone(),
        _ => cwd.clone(),
    };
    Ok((parent, name.to_string()))
}

fn copy_node(node: &FileOrDir, dir: &DirRef, name: &str) -> Result<(), String> {
    match node {
        FileOrDir::File(f) => copy_file(f, dir, name),
        FileOrDir::Dir(src) => {
            let existing = dir.lock().get(name);
            let dst = match existing {
                Some(FileOrDir::Dir(d)) => d,
                Some(FileOrDir::File(_)) => return Err(format!("'{name}' existe et n'est pas un dossier")),
                None => {
                    let made = dir.lock().create_dir(name, 0o755);
                    match made {
                        Some(r) => r,
                        None => VFSDirectory::create(name.to_string(), dir),
                    }
                    .map_err(String::from)?
                }
            };
            // Les noms d'abord, verrou relâché, puis chaque enfant.
            let names = src.lock().list();
            for n in names {
                let child = src.lock().get(&n);
                if let Some(c) = child {
                    copy_node(&c, &dst, &n)?;
                }
            }
            Ok(())
        }
    }
}

fn copy_file(src: &FileRef, dir: &DirRef, name: &str) -> Result<(), String> {
    let existing = dir.lock().get(name);
    let dst = match existing {
        Some(FileOrDir::File(f)) => {
            if alloc::sync::Arc::ptr_eq(&f, src) {
                return Err("source et destination identiques".into());
            }
            f.lock().set_len(0).map_err(|e| format!("remplacement impossible : {e}"))?;
            f
        }
        Some(FileOrDir::Dir(_)) => return Err(format!("'{name}' est un dossier")),
        None => {
            let made = dir.lock().create_file(name, 0o644);
            match made {
                Some(r) => r,
                None => memfs::MemFile::create(name.to_string(), dir),
            }
            .map_err(|e| format!("creation impossible : {e}"))?
        }
    };
    let len = src.lock().len();
    let mut buf = alloc::vec![0u8; len.min(CHUNK)];
    let mut off = 0;
    while off < len {
        let n = src.lock().read_at(&mut buf, off).map_err(|e| format!("lecture : {e:?}"))?;
        if n == 0 {
            break;
        }
        dst.lock().write_at(&buf[..n], off).map_err(|e| format!("ecriture : {e:?}"))?;
        off += n;
    }
    Ok(())
}
