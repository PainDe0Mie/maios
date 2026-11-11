//! Icônes d'applications pour le bureau de MaiOS, sans dépendance (testé sur l'hôte comme
//! `llfree` : `cp -r kernel/gfx/mai_icons ~/t && cd ~/t && cargo test`).
//!
//! - [`inflate`] : DEFLATE et zlib ;
//! - [`png`] : PNG complet (Adam7, tous types et profondeurs, `tRNS`) ;
//! - [`xpm`] : XPM 3 (icônes historiques de `/usr/share/pixmaps`) ;
//! - [`desktop`] : fichiers `.desktop` et découpage de `Exec` ;
//! - mise à l'échelle prémultipliée ([`Image::resized`], [`Image::fit_square`]) ;
//! - [`lookup_order`] : chemins candidats d'une icône nommée (thème `hicolor`, `pixmaps`).
//!
//! Les pixels sont en RVBA 8 bits **prémultipliés**.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod desktop;
pub mod inflate;
pub mod png;
mod scale;
pub mod xpm;

#[cfg(test)]
mod tests;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Image RVBA prémultipliée.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<[u8; 4]>,
}

/// Décode une image PNG ou XPM (reconnue au contenu).
pub fn decode(data: &[u8], max_pixels: usize) -> Option<Image> {
    if png::is_png(data) {
        return png::decode(data, max_pixels).ok();
    }
    let text = core::str::from_utf8(data).ok()?;
    if text.trim_start().starts_with("/* XPM */") {
        return xpm::decode(text, max_pixels).ok();
    }
    None
}

/// Tailles des dossiers `hicolor` essayées pour une icône de `size` pixels : d'abord la plus
/// petite qui n'est pas plus petite (réduire est net), puis les plus petites en descendant.
pub fn hicolor_sizes(size: usize) -> Vec<usize> {
    const SIZES: [usize; 13] = [16, 22, 24, 32, 36, 48, 64, 72, 96, 128, 192, 256, 512];
    let mut out: Vec<usize> = SIZES.iter().copied().filter(|&s| s >= size).collect();
    out.extend(SIZES.iter().rev().copied().filter(|&s| s < size));
    out
}

/// Chemins candidats, dans l'ordre, pour l'icône nommée `name` à `size` pixels (spécification
/// des thèmes d'icônes, thème `hicolor` puis `/usr/share/pixmaps`, avec la convention Debian
/// `nom_NxN.xpm`). Un nom absolu est essayé tel quel.
pub fn lookup_order(name: &str, size: usize) -> Vec<String> {
    let mut out = Vec::new();
    if name.is_empty() || name.contains("..") {
        return out;
    }
    if name.starts_with('/') {
        out.push(String::from(name));
        return out;
    }
    if name.contains('/') {
        return out;
    }
    for s in hicolor_sizes(size) {
        out.push(format!("/usr/share/icons/hicolor/{s}x{s}/apps/{name}.png"));
    }
    out.push(format!("/usr/share/pixmaps/{name}.png"));
    for s in [48usize, 32, 64, 16] {
        out.push(format!("/usr/share/pixmaps/{name}_{s}x{s}.xpm"));
    }
    out.push(format!("/usr/share/pixmaps/{name}.xpm"));
    out
}
