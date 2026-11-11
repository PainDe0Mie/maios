//! `mkdir [-p] DOSSIER...` : crée des dossiers (chemins relatifs ou absolus). `-p` crée aussi
//! les parents manquants et ne se plaint pas d'un dossier existant. Création par la fabrique
//! du système de fichiers du parent (`Directory::create_dir`), sinon un dossier virtuel
//! (`vfs_node`), comme MaiProc.
#![no_std]
#[macro_use]
extern crate app_io;
extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use fs_node::{DirRef, FileOrDir};
use path::{Component, Path};
use vfs_node::VFSDirectory;

pub fn main(args: Vec<String>) -> isize {
    let mut opts = getopts::Options::new();
    opts.optflag("p", "parents", "cree les parents manquants, sans erreur si le dossier existe");
    opts.optflag("h", "help", "affiche cette aide");
    let m = match opts.parse(args) {
        Ok(m) => m,
        Err(e) => {
            println!("mkdir: {e}");
            return -1;
        }
    };
    if m.opt_present("h") || m.free.is_empty() {
        println!("{}", opts.usage("Usage: mkdir [-p] DOSSIER..."));
        return if m.free.is_empty() && !m.opt_present("h") { -1 } else { 0 };
    }
    let Ok(cwd) = task::with_current_task(|t| t.env.lock().working_dir.clone()) else {
        println!("mkdir: tache courante introuvable");
        return -1;
    };
    let parents = m.opt_present("p");
    let mut ret = 0;
    for arg in &m.free {
        if let Err(e) = mkdir(&cwd, arg, parents) {
            println!("mkdir: '{arg}' : {e}");
            ret = -1;
        }
    }
    ret
}

/// Sous-dossier `name` de `dir`, créé.
fn create(dir: &DirRef, name: &str) -> Result<DirRef, &'static str> {
    let made = dir.lock().create_dir(name, 0o755);
    match made {
        Some(r) => r,
        None => VFSDirectory::create(name.to_string(), dir),
    }
}

fn mkdir(cwd: &DirRef, arg: &str, parents: bool) -> Result<(), &'static str> {
    let p: &Path = arg.as_ref();
    let mut cur = if p.is_absolute() { root::get_root().clone() } else { cwd.clone() };
    let comps: Vec<Component> = p.components().collect();
    let last = comps.iter().rposition(|c| matches!(c, Component::Normal(_))).ok_or("nom invalide")?;
    for (i, c) in comps.iter().enumerate() {
        let name = match c {
            Component::Normal(n) => *n,
            Component::ParentDir => {
                let up = cur.lock().get_parent_dir();
                if let Some(up) = up {
                    cur = up;
                }
                continue;
            }
            _ => continue,
        };
        let existing = cur.lock().get(name);
        cur = match existing {
            Some(FileOrDir::Dir(d)) => {
                if i == last && !parents {
                    return Err("existe deja");
                }
                d
            }
            Some(FileOrDir::File(_)) => return Err("un fichier porte deja ce nom"),
            None if i == last || parents => create(&cur, name)?,
            None => return Err("dossier parent introuvable (essayer -p)"),
        };
    }
    Ok(())
}
