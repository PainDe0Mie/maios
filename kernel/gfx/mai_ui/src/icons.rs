//! Icônes vectorielles dessinées à la volée (aucune image embarquée) : types de fichiers,
//! volumes, actions de barre d'outils. Toutes se dessinent dans un carré de côté `s` (16 à 64
//! pixels) et restent nettes à toute taille grâce aux primitives anticrénelées du canevas.

use crate::canvas::{rgb, Canvas, Rect, Rgb, CHAR_H};
use crate::kit::pal;

/// Type d'icône.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    Folder,
    FolderOpen,
    File,
    Text,
    Code,
    Image,
    Audio,
    Archive,
    /// Exécutable ELF (Linux ou programme C).
    Binary,
    /// Application MaiOS (crate du namespace).
    App,
    /// Bibliothèque / module du noyau.
    Library,
    Disk,
    Trash,
    Home,
    Desktop,
    Search,
    Back,
    Forward,
    Up,
    Refresh,
    Grid,
    List,
    Cpu,
    /// Carte graphique (GPU).
    Gpu,
    Memory,
    Process,
    Thread,
    Gear,
    Plus,
    Star,
    Clock,
}

/// Couleur d'accent d'une icône de fichier.
pub fn icon_color(i: Icon) -> Rgb {
    match i {
        Icon::Folder | Icon::FolderOpen => rgb(0xE0AF68),
        Icon::Text => rgb(0xA9B1D6),
        Icon::Code => rgb(0x7DCFFF),
        Icon::Image => rgb(0xBB9AF7),
        Icon::Audio => rgb(0xFF9E64),
        Icon::Archive => rgb(0xD19A66),
        Icon::Binary => rgb(0x9ECE6A),
        Icon::App => rgb(0x73DACA),
        Icon::Library => rgb(0x7AA2F7),
        Icon::Disk => rgb(0x89DDFF),
        Icon::Gpu => rgb(0x9ECE6A),
        Icon::Trash => rgb(0xF7768E),
        _ => pal::TEXT_SOFT,
    }
}

/// Dessine l'icône `i` dans le carré `(x, y, s)`.
pub fn draw(c: &mut Canvas, i: Icon, x: isize, y: isize, s: isize) {
    let col = icon_color(i);
    let u = |n: isize| n * s / 16; // unité : 1/16 du côté
    match i {
        Icon::Folder | Icon::FolderOpen => {
            let back = col.darken(70);
            // Onglet + dos du dossier.
            c.round_rect(Rect::new(x + u(1), y + u(3), u(6), u(3)), u(1).max(1), back);
            c.round_rect(Rect::new(x + u(1), y + u(4), u(14), u(10)), u(1).max(1), back);
            if i == Icon::FolderOpen {
                c.round_rect(Rect::new(x + u(2), y + u(6), u(14), u(8)), u(1).max(1), col);
            } else {
                c.round_rect(Rect::new(x + u(1), y + u(6), u(14), u(8)), u(1).max(1), col);
                c.fill_alpha(Rect::new(x + u(1), y + u(6), u(14), 1), rgb(0xFFFFFF), 70);
            }
        }
        Icon::File | Icon::Text | Icon::Code | Icon::Image | Icon::Audio | Icon::Archive | Icon::Binary | Icon::Library => {
            // Feuille à coin replié.
            let page = Rect::new(x + u(3), y + u(1), u(10), u(14));
            c.round_rect(page, u(1).max(1), rgb(0xDCE3FF).mix(col, 60));
            let fold = u(4);
            c.fill(Rect::new(page.right() - fold, page.y, fold, fold), pal::BG);
            c.round_rect(Rect::new(page.right() - fold, page.y, fold, fold), 1, col.darken(40));
            let ink = col.darken(110);
            match i {
                Icon::Text | Icon::File => {
                    for k in 0..4 {
                        let w = if k == 3 { u(4) } else { u(6) };
                        c.fill(Rect::new(page.x + u(2), page.y + u(6) + k * u(2), w, u(1).max(1)), ink);
                    }
                }
                Icon::Code => {
                    let cy = page.y + u(9);
                    c.line(page.x + u(4), cy - u(2), page.x + u(2), cy, ink);
                    c.line(page.x + u(2), cy, page.x + u(4), cy + u(2), ink);
                    c.line(page.x + u(6), cy - u(2), page.x + u(8), cy, ink);
                    c.line(page.x + u(8), cy, page.x + u(6), cy + u(2), ink);
                }
                Icon::Image => {
                    c.disc(page.x + u(3), page.y + u(7), u(1).max(1), ink, 255);
                    c.line(page.x + u(1), page.bottom() - u(2), page.x + u(4), page.y + u(9), ink);
                    c.line(page.x + u(4), page.y + u(9), page.x + u(6), page.bottom() - u(3), ink);
                    c.line(page.x + u(6), page.bottom() - u(3), page.x + u(9), page.y + u(8), ink);
                }
                Icon::Audio => {
                    c.disc(page.x + u(4), page.bottom() - u(3), u(1) + 1, ink, 255);
                    c.fill(Rect::new(page.x + u(5), page.y + u(6), u(1).max(1), u(5)), ink);
                    c.line(page.x + u(5), page.y + u(6), page.x + u(8), page.y + u(5), ink);
                }
                Icon::Archive => {
                    for k in 0..5 {
                        c.fill(Rect::new(page.x + u(4) + (k % 2) * u(1), page.y + u(1) + k * u(2), u(1).max(1), u(1).max(1)), ink);
                    }
                    c.round_rect(Rect::new(page.x + u(3), page.y + u(11), u(3), u(2)), 1, ink);
                }
                Icon::Binary => {
                    // Invite de commande « >_ ».
                    let cy = page.y + u(8);
                    c.line(page.x + u(2), cy - u(2), page.x + u(4), cy, ink);
                    c.line(page.x + u(4), cy, page.x + u(2), cy + u(2), ink);
                    c.fill(Rect::new(page.x + u(5), cy + u(2), u(3), u(1).max(1)), ink);
                }
                Icon::Library => {
                    for k in 0..3 {
                        c.round_rect(Rect::new(page.x + u(2), page.y + u(5) + k * u(3), u(6), u(2)), 1, ink);
                    }
                }
                _ => {}
            }
        }
        Icon::App => {
            let r = Rect::new(x + u(1), y + u(1), u(14), u(14));
            c.round_rect(r, u(4), col.darken(60));
            c.round_rect(r.inset(u(1)), u(3), col);
            let inner = r.inset(u(4));
            c.round_rect(Rect::new(inner.x, inner.y, inner.w / 2 - 1, inner.h / 2 - 1), 2, rgb(0x1A1B26));
            c.round_rect(Rect::new(inner.x + inner.w / 2 + 1, inner.y + inner.h / 2 + 1, inner.w / 2 - 1, inner.h / 2 - 1), 2, rgb(0x1A1B26));
        }
        Icon::Disk => {
            let r = Rect::new(x + u(1), y + u(4), u(14), u(9));
            c.round_rect(r, u(2), col.darken(80));
            c.round_rect(r.inset(1), u(2), col.darken(20));
            c.fill(Rect::new(r.x + u(2), r.bottom() - u(3), u(7), u(1).max(1)), col.darken(120));
            c.disc(r.right() - u(3), r.bottom() - u(3), u(1).max(1), pal::GREEN, 255);
        }
        Icon::Trash => {
            c.round_rect(Rect::new(x + u(2), y + u(3), u(12), u(2)), 1, col);
            c.round_rect(Rect::new(x + u(6), y + u(1), u(4), u(2)), 1, col);
            c.round_rect(Rect::new(x + u(3), y + u(5), u(10), u(10)), u(1).max(1), col.darken(40));
            for k in 0..3 {
                c.fill(Rect::new(x + u(5) + k * u(2) + k / 2, y + u(7), u(1).max(1), u(6)), col.darken(130));
            }
        }
        Icon::Home => {
            let cx = x + u(8);
            c.line(x + u(1), y + u(8), cx, y + u(2), col);
            c.line(cx, y + u(2), x + u(15), y + u(8), col);
            c.round_rect(Rect::new(x + u(3), y + u(8), u(10), u(7)), 1, col);
            c.fill(Rect::new(x + u(7), y + u(10), u(2), u(5)), pal::BG);
        }
        Icon::Desktop => {
            c.round_rect(Rect::new(x + u(1), y + u(2), u(14), u(10)), u(1).max(1), col);
            c.fill(Rect::new(x + u(2), y + u(3), u(12), u(8)), pal::ACCENT_SOFT);
            c.fill(Rect::new(x + u(7), y + u(12), u(2), u(2)), col);
            c.round_rect(Rect::new(x + u(4), y + u(14), u(8), u(1).max(1)), 1, col);
        }
        Icon::Search => {
            let r = u(5);
            c.disc(x + u(7), y + u(7), r, col, 255);
            c.disc(x + u(7), y + u(7), r - u(1).max(2), pal::SURFACE0, 255);
            for k in 0..2 {
                c.line(x + u(10) + k, y + u(10), x + u(14) + k, y + u(14), col);
            }
        }
        Icon::Back | Icon::Forward | Icon::Up => {
            let (ax, ay, bx, by, cx, cy) = match i {
                Icon::Back => (u(10), u(3), u(5), u(8), u(10), u(13)),
                Icon::Forward => (u(6), u(3), u(11), u(8), u(6), u(13)),
                _ => (u(3), u(10), u(8), u(5), u(13), u(10)),
            };
            for k in 0..2 {
                let d = if i == Icon::Up { (0, k) } else { (k, 0) };
                c.line(x + ax + d.0, y + ay + d.1, x + bx + d.0, y + by + d.1, col);
                c.line(x + bx + d.0, y + by + d.1, x + cx + d.0, y + cy + d.1, col);
            }
        }
        Icon::Refresh => {
            // Arc approché par segments + pointe de flèche.
            let (cx, cy, r) = (x + u(8), y + u(8), u(5));
            let pts: [(isize, isize); 9] = [(0, -16), (11, -11), (16, 0), (11, 11), (0, 16), (-11, 11), (-16, 0), (-11, -11), (-4, -15)];
            for w in pts.windows(2) {
                c.line(cx + w[0].0 * r / 16, cy + w[0].1 * r / 16, cx + w[1].0 * r / 16, cy + w[1].1 * r / 16, col);
            }
            c.line(cx, cy - r, cx - u(3), cy - r - u(2), col);
            c.line(cx, cy - r, cx - u(2), cy - r + u(3), col);
        }
        Icon::Grid => {
            for k in 0..4 {
                let (gx, gy) = (k % 2, k / 2);
                c.round_rect(Rect::new(x + u(2) + gx * u(7), y + u(2) + gy * u(7), u(5), u(5)), 2, col);
            }
        }
        Icon::List => {
            for k in 0..3 {
                c.round_rect(Rect::new(x + u(2), y + u(3) + k * u(4), u(2), u(2)), 1, col);
                c.round_rect(Rect::new(x + u(5), y + u(3) + k * u(4), u(9), u(2)), 1, col);
            }
        }
        Icon::Cpu => {
            let r = Rect::new(x + u(3), y + u(3), u(10), u(10));
            for k in 0..4 {
                let o = u(4) + k * u(2);
                c.fill(Rect::new(x + o, y + u(1), 1, u(2)), col);
                c.fill(Rect::new(x + o, y + u(13), 1, u(2)), col);
                c.fill(Rect::new(x + u(1), y + o, u(2), 1), col);
                c.fill(Rect::new(x + u(13), y + o, u(2), 1), col);
            }
            c.round_rect(r, 2, col);
            c.round_rect(r.inset(u(2)), 1, pal::SURFACE0);
        }
        Icon::Gpu => {
            // Carte : équerre à gauche, ventilateur, contacts du connecteur en bas.
            c.fill(Rect::new(x, y + u(2), 1.max(u(1)), u(12)), col);
            let board = Rect::new(x + u(2), y + u(3), u(14), u(9));
            c.round_rect(board, 2, col);
            let (cx, cy, rad) = (x + u(7), y + u(7) + u(1) / 2, u(3));
            c.disc(cx, cy, rad, pal::SURFACE0, 255);
            c.disc(cx, cy, rad.saturating_sub(u(2)).max(1), col, 255);
            c.round_rect(Rect::new(x + u(11), y + u(5), u(3), u(5)), 1, pal::SURFACE0);
            for k in 0..5 {
                c.fill(Rect::new(x + u(4) + k * u(2), y + u(12), 1.max(u(1)), u(2)), col);
            }
        }
        Icon::Memory => {
            c.round_rect(Rect::new(x + u(1), y + u(4), u(14), u(7)), 1, col);
            for k in 0..4 {
                c.fill(Rect::new(x + u(2) + k * u(3), y + u(5), u(2), u(4)), pal::SURFACE0);
            }
            for k in 0..6 {
                c.fill(Rect::new(x + u(2) + k * u(2), y + u(11), 1, u(2)), col);
            }
        }
        Icon::Process | Icon::Thread => {
            c.round_rect(Rect::new(x + u(1), y + u(2), u(14), u(12)), u(2), col.darken(60));
            c.fill(Rect::new(x + u(1), y + u(5), u(14), 1), col);
            if i == Icon::Thread {
                c.line(x + u(4), y + u(8), x + u(12), y + u(8), col);
                c.line(x + u(4), y + u(11), x + u(10), y + u(11), col);
            } else {
                c.round_rect(Rect::new(x + u(3), y + u(7), u(4), u(5)), 1, col);
                c.round_rect(Rect::new(x + u(8), y + u(7), u(5), u(2)), 1, col);
            }
        }
        Icon::Gear => {
            let (cx, cy) = (x + u(8), y + u(8));
            const D: [(isize, isize); 8] = [(0, -6), (4, -4), (6, 0), (4, 4), (0, 6), (-4, 4), (-6, 0), (-4, -4)];
            for &(dx, dy) in &D {
                c.disc(cx + dx * s / 16, cy + dy * s / 16, u(1) + 1, col, 255);
            }
            c.disc(cx, cy, u(5), col, 255);
            c.disc(cx, cy, u(2), pal::SURFACE0, 255);
        }
        Icon::Plus => {
            c.round_rect(Rect::new(x + u(7), y + u(2), u(2), u(12)), 1, col);
            c.round_rect(Rect::new(x + u(2), y + u(7), u(12), u(2)), 1, col);
        }
        Icon::Star => {
            let (cx, cy) = (x + u(8), y + u(8));
            const P: [(isize, isize); 10] = [(0, -7), (2, -2), (7, -2), (3, 1), (5, 7), (0, 3), (-5, 7), (-3, 1), (-7, -2), (-2, -2)];
            for k in 0..10 {
                let a = P[k];
                let b = P[(k + 1) % 10];
                c.line(cx + a.0 * s / 16, cy + a.1 * s / 16, cx + b.0 * s / 16, cy + b.1 * s / 16, pal::YELLOW);
            }
        }
        Icon::Clock => {
            let (cx, cy) = (x + u(8), y + u(8));
            c.disc(cx, cy, u(7), col, 255);
            c.disc(cx, cy, u(6), pal::SURFACE0, 255);
            c.line(cx, cy, cx, cy - u(4), col);
            c.line(cx, cy, cx + u(3), cy + u(1), col);
        }
    }
}

/// Icône centrée verticalement dans `r` à partir de `x`.
pub fn draw_in(c: &mut Canvas, i: Icon, x: isize, r: Rect, s: isize) {
    draw(c, i, x, r.y + (r.h - s) / 2, s);
}

/// Hauteur de ligne de texte, pour aligner icônes et libellés.
pub const LINE_H: isize = CHAR_H;
