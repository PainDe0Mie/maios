//! Résolution d'un chemin avec liens symboliques (`namei` / `link_path_walk` de Linux).
//!
//! Le parcours garde la pile des dossiers traversés : `..` remonte dans cette pile, donc du
//! côté *physique* d'un lien (un lien `lib -> usr/lib` suivi de `..` mène à `/usr`, pas à `/`).
//! Un lien rencontré au milieu du chemin est toujours suivi ; le dernier composant l'est selon
//! `follow_last` (`stat` contre `lstat`, `O_NOFOLLOW`). Au plus [`MAX_SYMLINKS`] liens par
//! résolution (`ELOOP`), comme `MAXSYMLINKS` de Linux.
//!
//! Seul le verrou du dossier traversé est pris, le temps de [`fs_node::Directory::get_with_link`] :
//! jamais celui d'un fichier (un appelant peut le tenir pendant qu'il résout un autre chemin).

use alloc::{string::String, vec::Vec};

use fs_node::{DirRef, FileOrDir};

pub const MAX_SYMLINKS: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveError {
    /// `ENOENT`.
    NotFound,
    /// `ENOTDIR` : un composant du milieu n'est pas un dossier.
    NotDir,
    /// `ELOOP`.
    Loop,
}

pub struct Resolved {
    pub node: FileOrDir,
    /// Chemin physique (liens résolus) depuis le point de départ ; absolu si le chemin l'était.
    pub path: String,
}

/// Résout `path`, absolu ou relatif à `cwd`.
pub fn resolve(cwd: &DirRef, path: &str, follow_last: bool) -> Result<Resolved, ResolveError> {
    let root = root::get_root().clone();
    let absolute = path.starts_with('/');
    // Pile des dossiers traversés : (dossier, nom sous lequel on y est entré).
    let mut stack: Vec<(DirRef, String)> = Vec::new();
    stack.push((if absolute { root.clone() } else { cwd.clone() }, String::new()));
    // Composants restant à traiter, le prochain en dernier.
    let mut pending: Vec<String> = path.split('/').rev().filter(|c| !c.is_empty() && *c != ".").map(String::from).collect();
    let mut links = 0usize;
    let mut rooted = absolute;

    while let Some(comp) = pending.pop() {
        if comp == ".." {
            if stack.len() > 1 {
                stack.pop();
            } else if !rooted {
                // Au-dessus du dossier de départ d'un chemin relatif : son parent dans le VFS.
                let top = stack[0].0.clone();
                let parent = top.lock().get_parent_dir();
                if let Some(p) = parent {
                    stack[0] = (p, String::from(".."));
                }
            }
            continue;
        }
        let dir = stack.last().unwrap().0.clone();
        let (child, link) = dir.lock().get_with_link(&comp).ok_or(ResolveError::NotFound)?;
        let last = pending.is_empty();
        if let FileOrDir::File(_) = &child {
            if !last || follow_last {
                if let Some(target) = link {
                    links += 1;
                    if links > MAX_SYMLINKS {
                        return Err(ResolveError::Loop);
                    }
                    if target.starts_with('/') {
                        stack.clear();
                        stack.push((root.clone(), String::new()));
                        rooted = true;
                    }
                    pending.extend(target.split('/').rev().filter(|c| !c.is_empty() && *c != ".").map(String::from));
                    continue;
                }
            }
        }
        if last {
            let mut p = canonical(&stack, rooted);
            if !p.ends_with('/') {
                p.push('/');
            }
            p.push_str(&comp);
            return Ok(Resolved { node: child, path: p });
        }
        match child {
            FileOrDir::Dir(d) => stack.push((d, comp)),
            FileOrDir::File(_) => return Err(ResolveError::NotDir),
        }
    }
    let path = canonical(&stack, rooted);
    let (dir, _) = stack.pop().unwrap();
    Ok(Resolved { node: FileOrDir::Dir(dir), path })
}

fn canonical(stack: &[(DirRef, String)], rooted: bool) -> String {
    let mut p = String::new();
    if rooted {
        p.push('/');
    }
    for (i, (_, name)) in stack.iter().enumerate().skip(1) {
        if i > 1 {
            p.push('/');
        }
        p.push_str(name);
    }
    if p.is_empty() {
        p.push('.');
    }
    p
}
