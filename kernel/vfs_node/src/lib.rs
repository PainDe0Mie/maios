//! Implémentation concrète générique d'un répertoire VFS.
//!
//! Utilise `BTreeMap` (disponible dans `alloc` sans dépendance externe)
//! pour le stockage des enfants.  L'index de préfixes est conservé pour
//! accélérer `get_file_starting_with`, qui est la méthode la plus appelée
//! par les namespaces de crates.

#![no_std]

extern crate alloc;
extern crate spin;
extern crate fs_node;
extern crate memory;

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::sync::{Arc, Weak};
use spin::Mutex;
use fs_node::{DirRef, WeakDirRef, Directory, FileOrDir, FsNode, FileRef};

// ────────────────────────────────────────────────────────────────────────────
// VFSDirectory
// ────────────────────────────────────────────────────────────────────────────

/// Nœud répertoire générique du VFS de mai_os.
pub struct VFSDirectory {
    name:         String,
    /// Enfants : nom → FileOrDir.
    children:     BTreeMap<String, FileOrDir>,
    /// Référence faible au répertoire parent (évite les cycles Arc).
    parent:       WeakDirRef,
    /// Index préfixe → nom complet, pour `get_file_starting_with` rapide.
    prefix_index: BTreeMap<String, String>,
}

impl VFSDirectory {
    // ── Constructeurs ─────────────────────────────────────────────────────────

    /// Crée un nouveau répertoire et l'insère dans `parent`.
    pub fn create(name: String, parent: &DirRef) -> Result<DirRef, &'static str> {
        let dir = VFSDirectory {
            name,
            children:     BTreeMap::new(),
            parent:       Arc::downgrade(parent),
            prefix_index: BTreeMap::new(),
        };
        let dir_ref = Arc::new(Mutex::new(dir)) as DirRef;
        parent.lock().insert(FileOrDir::Dir(dir_ref.clone()))?;
        Ok(dir_ref)
    }

    /// Crée un répertoire racine (sans parent).
    pub fn create_root(name: String) -> DirRef {
        let dir = VFSDirectory {
            name,
            children:     BTreeMap::new(),
            parent:       Weak::<Mutex<VFSDirectory>>::new(),
            prefix_index: BTreeMap::new(),
        };
        Arc::new(Mutex::new(dir)) as DirRef
    }

    // ── Accès ─────────────────────────────────────────────────────────────────

    #[inline]
    pub fn entry_count(&self) -> usize { self.children.len() }

    #[inline]
    pub fn is_empty(&self) -> bool { self.children.is_empty() }

    /// Retourne le premier fichier dont le nom commence par `prefix`.
    ///
    /// Utilise l'index de préfixes pour répondre en O(log n) après le premier
    /// appel, plutôt que de scanner toutes les entrées en O(n).
    ///
    /// Note : cette méthode n'est PAS dans le trait `Directory` (qui est
    /// défini dans `fs_node` et ne peut pas être modifié ici).
    /// Les namespaces de crates l'appellent via downcast ou directement
    /// quand le type `VFSDirectory` est connu.
    pub fn get_file_starting_with(&self, prefix: &str) -> Option<FileRef> {
        // Fast path : index chaud
        if let Some(full_name) = self.prefix_index.get(prefix) {
            if let Some(FileOrDir::File(f)) = self.children.get(full_name.as_str()) {
                return Some(f.clone());
            }
        }
        // Slow path : scan linéaire (première utilisation d'un préfixe)
        self.children.iter()
            .find_map(|(name, node)| {
                if name.starts_with(prefix) {
                    if let FileOrDir::File(f) = node { Some(f.clone()) } else { None }
                } else {
                    None
                }
            })
    }

    // ── Index de préfixes (interne) ───────────────────────────────────────────

    fn index_insert(&mut self, name: &str) {
        let prefix_end = name.find('-').map(|i| i + 1).unwrap_or(name.len());
        let prefix     = &name[..prefix_end];
        let should_insert = self.prefix_index
            .get(prefix)
            .map(|existing| name > existing.as_str())
            .unwrap_or(true);
        if should_insert {
            self.prefix_index.insert(prefix.to_string(), name.to_string());
        }
    }

    fn index_remove(&mut self, name: &str) {
        let prefix_end = name.find('-').map(|i| i + 1).unwrap_or(name.len());
        let prefix     = &name[..prefix_end];
        if let Some(indexed) = self.prefix_index.get(prefix) {
            if indexed.as_str() == name {
                let best = self.children.keys()
                    .filter(|k| k.starts_with(prefix) && k.as_str() != name)
                    .last()
                    .cloned();
                match best {
                    Some(b) => { self.prefix_index.insert(prefix.to_string(), b); }
                    None    => { self.prefix_index.remove(prefix); }
                }
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// impl Directory
// ────────────────────────────────────────────────────────────────────────────

impl Directory for VFSDirectory {
    fn insert(&mut self, node: FileOrDir) -> Result<Option<FileOrDir>, &'static str> {
        let name = node.get_name();
        self.index_insert(&name);
        if let Some(mut old) = self.children.insert(name, node) {
            old.set_parent_dir(Weak::<Mutex<VFSDirectory>>::new());
            Ok(Some(old))
        } else {
            Ok(None)
        }
    }

    fn get(&self, name: &str) -> Option<FileOrDir> {
        self.children.get(name).cloned()
    }

    fn list(&self) -> Vec<String> {
        self.children.keys().cloned().collect()
    }

    fn remove(&mut self, node: &FileOrDir) -> Option<FileOrDir> {
        let name = node.get_name();
        self.index_remove(&name);
        if let Some(mut old) = self.children.remove(&name) {
            old.set_parent_dir(Weak::<Mutex<VFSDirectory>>::new());
            Some(old)
        } else {
            None
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// impl FsNode
// ────────────────────────────────────────────────────────────────────────────

impl FsNode for VFSDirectory {
    fn get_name(&self) -> String { self.name.clone() }

    fn get_parent_dir(&self) -> Option<DirRef> { self.parent.upgrade() }

    fn set_parent_dir(&mut self, new_parent: WeakDirRef) { self.parent = new_parent; }
}

// ────────────────────────────────────────────────────────────────────────────
// impl Debug
// ────────────────────────────────────────────────────────────────────────────

impl core::fmt::Debug for VFSDirectory {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VFSDirectory")
            .field("name", &self.name)
            .field("children", &self.children.len())
            .finish()
    }
}