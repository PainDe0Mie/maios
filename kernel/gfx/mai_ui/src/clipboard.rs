//! Presse-papiers du système, partagé par toutes les applications : `mai_ui` est une crate du
//! noyau, chargée une seule fois dans le namespace du noyau, donc son état est commun.
//!
//! Il porte soit du texte, soit une liste de chemins (copier ou couper des fichiers), comme
//! les cibles `text/plain` et `text/uri-list` (avec l'opération `copy`/`cut`) de X11 et Wayland.

use alloc::string::String;
use alloc::vec::Vec;
use spin::Mutex;

/// Contenu du presse-papiers.
#[derive(Clone, Debug, Default)]
pub enum Clip {
    #[default]
    Empty,
    Text(String),
    /// Chemins absolus ; `cut` : déplacer au collage plutôt que copier.
    Files { paths: Vec<String>, cut: bool },
}

static CLIP: Mutex<Clip> = Mutex::new(Clip::Empty);
static GENERATION: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn set(c: Clip) {
    *CLIP.lock() = c;
    GENERATION.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

pub fn get() -> Clip { CLIP.lock().clone() }

pub fn clear() { set(Clip::Empty) }

/// Change à chaque écriture (pour rafraîchir un affichage).
pub fn generation() -> u64 { GENERATION.load(core::sync::atomic::Ordering::Relaxed) }

pub fn set_text(s: &str) { set(Clip::Text(String::from(s))) }

pub fn set_files(paths: Vec<String>, cut: bool) { set(Clip::Files { paths, cut }) }
