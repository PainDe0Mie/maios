//! `touch FICHIER...` : crée les fichiers absents (vides), met à jour la date de modification
//! des autres. Création par la fabrique du système de fichiers du dossier
//! (`Directory::create_file`, ext4 en lecture seule refuse), sinon un fichier en mémoire
//! (`memfs`), comme MaiProc.
#![no_std]
#[macro_use]
extern crate app_io;
extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use fs_node::{DirRef, FsNode, MetaChange, TimeSet};
use path::Path;

pub fn main(args: Vec<String>) -> isize {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("Usage: touch FICHIER...");
        println!("Cree les fichiers absents (vides) et met a jour la date des autres.");
        return 0;
    }
    if args.is_empty() {
        println!("touch: operande manquante (touch FICHIER...)");
        return -1;
    }
    let Ok(cwd) = task::with_current_task(|t| t.env.lock().working_dir.clone()) else {
        println!("touch: tache courante introuvable");
        return -1;
    };
    let mut ret = 0;
    for arg in &args {
        let p: &Path = arg.as_ref();
        match p.get(&cwd) {
            Some(mut node) => {
                // Pas de date modifiable : le système de fichiers ne garde pas de dates.
                let _ = node.set_metadata(MetaChange::Times { atime: TimeSet::Now, mtime: TimeSet::Now });
            }
            None => {
                if let Err(e) = create(&cwd, arg) {
                    println!("touch: impossible de creer '{arg}' : {e}");
                    ret = -1;
                }
            }
        }
    }
    ret
}

/// Dossier parent et nom de `arg`, relatif à `cwd`.
fn split<'a>(cwd: &DirRef, arg: &'a str) -> Result<(DirRef, &'a str), &'static str> {
    let p: &Path = arg.as_ref();
    let name = p.file_name().ok_or("nom invalide")?;
    let parent = match p.parent() {
        Some(d) if d.components().next().is_some() => d.get_dir(cwd).ok_or("dossier parent introuvable")?,
        _ if arg.starts_with('/') => root::get_root().clone(),
        _ => cwd.clone(),
    };
    Ok((parent, name))
}

fn create(cwd: &DirRef, arg: &str) -> Result<(), &'static str> {
    let (dir, name) = split(cwd, arg)?;
    let made = dir.lock().create_file(name, 0o644);
    match made {
        Some(r) => r.map(|_| ()),
        None => memfs::MemFile::create(name.to_string(), &dir).map(|_| ()),
    }
}
