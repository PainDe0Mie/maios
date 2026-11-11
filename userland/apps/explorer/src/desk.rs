//! Contenu du bureau : **tout** `root:/desktop`, pas seulement les raccourcis.
//!
//! - `*.desk` (raccourcis natifs, `catalog::desk_app`) et `*.desktop` (lanceurs freedesktop,
//!   `catalog::desktop_entry`) : une application, rangée au catalogue (même identifiant →
//!   même entrée, mise à jour) ; un raccourci invalide reste visible comme simple fichier.
//! - dossiers, puis fichiers, triés par nom sans tenir compte de la casse ; type reconnu par
//!   `mai_ui::files::classify` (octets magiques puis extension), comme dans Fichiers.
//! - les noms cachés (`.xxx`) ne sont pas montrés.
//!
//! Les applis natives n'émettent pas d'événements du VFS : l'explorateur compare chaque
//! seconde une empreinte du dossier (noms, types, tailles) et relit tout quand elle change.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use fs_node::FileOrDir;
use mai_ui::files::{self, FileKind};

use crate::catalog::{self, App};

/// Dossier du bureau.
pub const DIR: &str = "/desktop";
/// Octets lus pour reconnaître le type d'un fichier.
const HEAD: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    /// Application du catalogue (indice).
    App(usize),
    Folder,
    File(FileKind),
}

pub struct Entry {
    /// Libellé : nom de l'application, ou nom du fichier.
    pub label: String,
    pub path: String,
    pub len: usize,
    pub target: Target,
}

fn node(path: &str) -> Option<FileOrDir> {
    let p: &path::Path = path.as_ref();
    p.get(root::get_root())
}

/// Empreinte du dossier du bureau : noms, types et tailles.
pub fn fingerprint() -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |b: &[u8]| {
        for &x in b {
            h = (h ^ x as u64).wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for name in catalog::list_dir(DIR) {
        eat(name.as_bytes());
        match node(&format!("{DIR}/{name}")) {
            Some(FileOrDir::Dir(_)) => eat(b"\0/"),
            Some(FileOrDir::File(f)) => {
                let len = f.lock().len() as u64;
                eat(b"\0");
                eat(&len.to_le_bytes());
            }
            None => eat(b"\0?"),
        }
    }
    h
}

/// Range `app` au catalogue : même identifiant → remplace l'entrée (raccourci modifié), une
/// appli déjà connue sous une de ses clés (Firefox natif et `firefox-esr.desktop`) → la
/// garde ; sinon l'ajoute. Les indices ne changent jamais (lancements en cours, menu).
fn intern(apps: &mut Vec<App>, app: App) -> usize {
    if let Some(i) = apps.iter().position(|a| a.id == app.id) {
        apps[i] = app;
        return i;
    }
    if let Some(i) = apps.iter().position(|a| app.keys.iter().any(|k| a.matches(k))) {
        return i;
    }
    apps.push(app);
    apps.len() - 1
}

/// Lit le bureau : raccourcis (ordre des noms de fichiers), dossiers, puis fichiers.
pub fn scan(apps: &mut Vec<App>) -> Vec<Entry> {
    let mut shortcuts = Vec::new();
    let mut folders = Vec::new();
    let mut others = Vec::new();
    // Les noms d'abord (verrou du dossier relâché), puis chaque enfant.
    for name in catalog::list_dir(DIR) {
        if name.starts_with('.') {
            continue;
        }
        let path = format!("{DIR}/{name}");
        let f = match node(&path) {
            Some(FileOrDir::Dir(_)) => {
                folders.push(Entry { label: name, path, len: 0, target: Target::Folder });
                continue;
            }
            Some(FileOrDir::File(f)) => f,
            None => continue,
        };
        let len = f.lock().len();
        // Raccourci : Some(Some(appli)), masqué : Some(None), fichier ordinaire : None.
        let launcher = if name.ends_with(".desk") {
            catalog::desk_app(&name).ok()
        } else if name.ends_with(".desktop") {
            catalog::desktop_entry(&path).map(Some)
        } else {
            None
        };
        match launcher {
            Some(Some(app)) => {
                let i = intern(apps, app);
                shortcuts.push(Entry { label: apps[i].name.clone(), path, len, target: Target::App(i) });
            }
            Some(None) => {}
            None => {
                let mut head = [0u8; HEAD];
                let n = f.lock().read_at(&mut head, 0).unwrap_or(0).min(len);
                let kind = files::classify(&name, &path, &head[..n], len);
                others.push(Entry { label: name, path, len, target: Target::File(kind) });
            }
        }
    }
    let by_name = |a: &Entry, b: &Entry| a.label.to_lowercase().cmp(&b.label.to_lowercase()).then_with(|| a.label.cmp(&b.label));
    folders.sort_by(by_name);
    others.sort_by(by_name);
    shortcuts.extend(folders);
    shortcuts.extend(others);
    shortcuts
}
