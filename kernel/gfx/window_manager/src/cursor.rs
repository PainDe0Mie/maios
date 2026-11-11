//! Images du curseur : formes intégrées (flèche, texte, main, redimensionnements…) et images
//! fournies par une application (curseur d'une surface Wayland, `wl_pointer.set_cursor`).
//!
//! Le curseur reste un plan à part (voir `compose.rs`) : l'image courante est publiée ici, sous
//! un verrou bref, et le compositeur la redessine quand elle change ou que le pointeur bouge.
//! Les formes intégrées couvrent celles de `cursor-shape-v1` (noms CSS) : les plus courantes
//! ont leur dessin, les autres prennent la plus proche.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use framebuffer::AlphaPixel;
use region2d::Rect;
use spin::Once;

use crate::pixels::blend;

/// Côté maximal d'une image de curseur (au-delà, elle est rognée).
pub const MAX_CURSOR_SIDE: usize = 256;

/// Pixel absent (alpha de MaiOS : 255 = invisible).
const CLEAR: AlphaPixel = AlphaPixel { blue: 0, green: 0, red: 0, alpha: 255 };
const OUTLINE: AlphaPixel = AlphaPixel { blue: 0x1E, green: 0x16, red: 0x15, alpha: 0 };
const FILL: AlphaPixel = AlphaPixel { blue: 0xFF, green: 0xF6, red: 0xF4, alpha: 0 };
const SHADOW_NEAR: AlphaPixel = AlphaPixel { blue: 0, green: 0, red: 0, alpha: 0x98 };
const SHADOW_FAR: AlphaPixel = AlphaPixel { blue: 0, green: 0, red: 0, alpha: 0xD0 };

/// Image de curseur : pixels de MaiOS (`alpha == 255` : absent) et point chaud.
pub struct CursorImage {
    pub width: usize,
    pub height: usize,
    /// Point chaud (le pixel qui désigne), relatif au coin haut gauche de l'image.
    pub hot_x: i32,
    pub hot_y: i32,
    pub px: Vec<AlphaPixel>,
}

impl CursorImage {
    /// Image de `width` × `height` (bornée à [`MAX_CURSOR_SIDE`]), entièrement transparente.
    pub fn new(width: usize, height: usize, hot_x: i32, hot_y: i32) -> CursorImage {
        let (width, height) = (width.min(MAX_CURSOR_SIDE), height.min(MAX_CURSOR_SIDE));
        CursorImage { width, height, hot_x, hot_y, px: vec![CLEAR; width * height] }
    }

    /// Dessin en caractères : `X` contour, `o` remplissage, espace transparent ; l'ombre portée
    /// (vers le bas à droite) est ajoutée.
    fn from_grid(g: &Grid, hot_x: i32, hot_y: i32) -> CursorImage {
        let mut img = CursorImage::new(g.w + 2, g.h + 2, hot_x, hot_y);
        for row in 0..img.height {
            for col in 0..img.width {
                let (c, r) = (col as isize, row as isize);
                img.px[row * img.width + col] = match g.at(c, r) {
                    b'X' => OUTLINE,
                    b'o' => FILL,
                    _ if g.at(c - 1, r - 1) != b' ' => SHADOW_NEAR,
                    _ if g.at(c - 2, r - 2) != b' ' || g.at(c - 1, r - 2) != b' ' => SHADOW_FAR,
                    _ => CLEAR,
                };
            }
        }
        img
    }

    /// Rectangle (écran) couvert quand le point chaud est en `(x, y)`.
    pub fn rect_at(&self, x: i32, y: i32) -> Rect {
        Rect::from_xywh(x - self.hot_x, y - self.hot_y, self.width as i32, self.height as i32)
    }

    /// Écrit le curseur (point chaud en `(x, y)`) dans `front`, mélangé au contenu de `back` :
    /// seuls les pixels du dessin sont écrits, le reste de `front` doit déjà égaler `back`.
    pub fn draw(&self, front: &mut [AlphaPixel], back: &[AlphaPixel], w: usize, h: usize, x: i32, y: i32) {
        let (x0, y0) = (x - self.hot_x, y - self.hot_y);
        for row in 0..self.height {
            let sy = y0 + row as i32;
            if sy < 0 || sy >= h as i32 {
                continue;
            }
            for col in 0..self.width {
                let sx = x0 + col as i32;
                if sx < 0 || sx >= w as i32 {
                    continue;
                }
                let p = self.px[row * self.width + col];
                if p.alpha == 255 {
                    continue;
                }
                let o = sy as usize * w + sx as usize;
                front[o] = blend(p, back[o]);
            }
        }
    }
}

/// Formes intégrées (noms de `cursor-shape-v1`, ceux des curseurs CSS).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorShape {
    Default,
    Text,
    Pointer,
    Crosshair,
    Move,
    EwResize,
    NsResize,
    NwseResize,
    NeswResize,
    NotAllowed,
    Wait,
}

impl CursorShape {
    /// Forme pour une valeur de `wp_cursor_shape_device_v1.shape` (`None` : valeur invalide).
    pub fn from_wayland(v: u32) -> Option<CursorShape> {
        use CursorShape::*;
        Some(match v {
            1 | 2 | 3 | 7 | 11 | 12 | 33 | 34 | 35 => Default, // default, context-menu, help, cell, alias, copy, zoom, dnd-ask
            4 | 16 | 17 => Pointer,                           // pointer, grab, grabbing
            5 | 6 => Wait,                                    // progress, wait
            8 => Crosshair,
            9 | 10 => Text,
            13 | 32 | 36 => Move, // move, all-scroll, all-resize
            14 | 15 => NotAllowed,
            18 | 25 | 26 | 30 => EwResize, // e, w, ew, col
            19 | 22 | 27 | 31 => NsResize, // n, s, ns, row
            21 | 23 | 29 => NwseResize,    // nw, se, nwse
            20 | 24 | 28 => NeswResize,    // ne, sw, nesw
            _ => return None,
        })
    }
}

/// Grille de caractères d'un dessin.
struct Grid {
    w: usize,
    h: usize,
    c: Vec<u8>,
}

impl Grid {
    fn blank(w: usize, h: usize) -> Grid {
        Grid { w, h, c: vec![b' '; w * h] }
    }

    fn from_rows(rows: &[&[u8]]) -> Grid {
        let w = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        let mut g = Grid::blank(w, rows.len());
        for (y, r) in rows.iter().enumerate() {
            for (x, &ch) in r.iter().enumerate() {
                g.c[y * w + x] = ch;
            }
        }
        g
    }

    fn at(&self, x: isize, y: isize) -> u8 {
        if x < 0 || y < 0 || x >= self.w as isize || y >= self.h as isize {
            return b' ';
        }
        self.c[y as usize * self.w + x as usize]
    }

    /// Pose `ch` en `(x, y)` : le remplissage l'emporte sur le contour, qui l'emporte sur le vide.
    fn put(&mut self, x: isize, y: isize, ch: u8) {
        if x < 0 || y < 0 || x >= self.w as isize || y >= self.h as isize {
            return;
        }
        let rank = |c: u8| match c {
            b'o' => 2,
            b'X' => 1,
            _ => 0,
        };
        let cell = &mut self.c[y as usize * self.w + x as usize];
        if rank(ch) > rank(*cell) {
            *cell = ch;
        }
    }

    /// Superpose `g` avec son coin en `(x, y)`.
    fn overlay(&mut self, g: &Grid, x: isize, y: isize) {
        for gy in 0..g.h {
            for gx in 0..g.w {
                self.put(x + gx as isize, y + gy as isize, g.c[gy * g.w + gx]);
            }
        }
    }

    fn transpose(&self) -> Grid {
        let mut t = Grid::blank(self.h, self.w);
        for y in 0..self.h {
            for x in 0..self.w {
                t.c[x * t.w + y] = self.c[y * self.w + x];
            }
        }
        t
    }

    fn mirror_x(&self) -> Grid {
        let mut m = Grid::blank(self.w, self.h);
        for y in 0..self.h {
            for x in 0..self.w {
                m.c[y * self.w + (self.w - 1 - x)] = self.c[y * self.w + x];
            }
        }
        m
    }
}

const ARROW: [&[u8]; 19] = [
    b"X           ",
    b"XX          ",
    b"XoX         ",
    b"XooX        ",
    b"XoooX       ",
    b"XooooX      ",
    b"XoooooX     ",
    b"XooooooX    ",
    b"XoooooooX   ",
    b"XooooooooX  ",
    b"XoooooooooX ",
    b"XooooooXXXXX",
    b"XoooXooX    ",
    b"XooXXooX    ",
    b"XoX  XooX   ",
    b"XX   XooX   ",
    b"      XooX  ",
    b"      XooX  ",
    b"       XX   ",
];

const IBEAM: [&[u8]; 19] = [
    b"XXX XXX",
    b"XooXooX",
    b"XXXoXXX",
    b"  XoX  ",
    b"  XoX  ",
    b"  XoX  ",
    b"  XoX  ",
    b"  XoX  ",
    b"  XoX  ",
    b"  XoX  ",
    b"  XoX  ",
    b"  XoX  ",
    b"  XoX  ",
    b"  XoX  ",
    b"  XoX  ",
    b"  XoX  ",
    b"XXXoXXX",
    b"XooXooX",
    b"XXX XXX",
];

const HAND: [&[u8]; 18] = [
    b"     XX          ",
    b"    XooX         ",
    b"    XooX         ",
    b"    XooX         ",
    b"    XooXXX       ",
    b"    XooXooXXX    ",
    b"    XooXooXooXX  ",
    b"    XooXooXooXoX ",
    b" XX XooooooooooX ",
    b"XooXXooooooooooX ",
    b"XoooXooooooooooX ",
    b" XooooooooooooX  ",
    b"  XoooooooooooX  ",
    b"  XooooooooooX   ",
    b"   XoooooooooX   ",
    b"   XooooooooX    ",
    b"    XoooooooX    ",
    b"    XXXXXXXXX    ",
];

const CROSS: [&[u8]; 15] = [
    b"      XXX      ",
    b"      XoX      ",
    b"      XoX      ",
    b"      XoX      ",
    b"      XoX      ",
    b"      XoX      ",
    b"XXXXXX   XXXXXX",
    b"Xooooo o oooooX",
    b"XXXXXX   XXXXXX",
    b"      XoX      ",
    b"      XoX      ",
    b"      XoX      ",
    b"      XoX      ",
    b"      XoX      ",
    b"      XXX      ",
];

/// Double flèche verticale.
const NS: [&[u8]; 19] = [
    b"   X   ",
    b"  XoX  ",
    b" XoooX ",
    b"XoooooX",
    b"XXXoXXX",
    b"  XoX  ",
    b"  XoX  ",
    b"  XoX  ",
    b"  XoX  ",
    b"  XoX  ",
    b"  XoX  ",
    b"  XoX  ",
    b"  XoX  ",
    b"  XoX  ",
    b"XXXoXXX",
    b"XoooooX",
    b" XoooX ",
    b"  XoX  ",
    b"   X   ",
];

const HOURGLASS: [&[u8]; 17] = [
    b"XXXXXXXXXXXXX",
    b"XoooooooooooX",
    b" XoooooooooX ",
    b" XoooooooooX ",
    b"  XoooooooX  ",
    b"   XoooooX   ",
    b"    XoooX    ",
    b"     XoX     ",
    b"     XoX     ",
    b"     XoX     ",
    b"    XoooX    ",
    b"   XoooooX   ",
    b"  XoooooooX  ",
    b" XoooooooooX ",
    b" XoooooooooX ",
    b"XoooooooooooX",
    b"XXXXXXXXXXXXX",
];

/// Double flèche diagonale, du haut gauche au bas droit.
fn diagonal() -> Grid {
    const N: isize = 15;
    let mut g = Grid::blank(N as usize, N as usize);
    for y in 0..N {
        for x in 0..N {
            let (tl, br) = (x + y, (N - 1 - x) + (N - 1 - y));
            for d in [tl, br] {
                if d <= 6 {
                    let edge = d == 6 || x == 0 || y == 0 || x == N - 1 || y == N - 1;
                    g.put(x, y, if edge { b'X' } else { b'o' });
                }
            }
            match (x - y).abs() {
                0 => g.put(x, y, b'o'),
                1 => g.put(x, y, b'X'),
                _ => {}
            }
        }
    }
    g
}

/// Cercle barré (action interdite).
fn forbidden() -> Grid {
    const N: isize = 19;
    let c = N / 2;
    let mut g = Grid::blank(N as usize, N as usize);
    for y in 0..N {
        for x in 0..N {
            let d2 = (x - c) * (x - c) + (y - c) * (y - c);
            if (36..=64).contains(&d2) {
                g.put(x, y, b'o');
            } else if (25..36).contains(&d2) || (65..=81).contains(&d2) {
                g.put(x, y, b'X');
            }
            if d2 < 36 {
                match (x - y).abs() {
                    0 | 1 => g.put(x, y, b'o'),
                    2 => g.put(x, y, b'X'),
                    _ => {}
                }
            }
        }
    }
    g
}

fn build(shape: CursorShape) -> CursorImage {
    use CursorShape::*;
    match shape {
        Default => CursorImage::from_grid(&Grid::from_rows(&ARROW), 0, 0),
        Text => CursorImage::from_grid(&Grid::from_rows(&IBEAM), 3, 9),
        Pointer => CursorImage::from_grid(&Grid::from_rows(&HAND), 5, 0),
        Crosshair => CursorImage::from_grid(&Grid::from_rows(&CROSS), 7, 7),
        NsResize => CursorImage::from_grid(&Grid::from_rows(&NS), 3, 9),
        EwResize => CursorImage::from_grid(&Grid::from_rows(&NS).transpose(), 9, 3),
        Move => {
            let ns = Grid::from_rows(&NS);
            let mut g = Grid::blank(19, 19);
            g.overlay(&ns, 6, 0);
            g.overlay(&ns.transpose(), 0, 6);
            CursorImage::from_grid(&g, 9, 9)
        }
        NwseResize => CursorImage::from_grid(&diagonal(), 7, 7),
        NeswResize => CursorImage::from_grid(&diagonal().mirror_x(), 7, 7),
        NotAllowed => CursorImage::from_grid(&forbidden(), 9, 9),
        Wait => CursorImage::from_grid(&Grid::from_rows(&HOURGLASS), 6, 8),
    }
}

const SHAPES: usize = 11;
static IMAGES: Once<[Arc<CursorImage>; SHAPES]> = Once::new();

/// Image (partagée) d'une forme intégrée.
pub fn shape_image(shape: CursorShape) -> Arc<CursorImage> {
    use CursorShape::*;
    let all = IMAGES.call_once(|| {
        [Default, Text, Pointer, Crosshair, Move, EwResize, NsResize, NwseResize, NeswResize, NotAllowed, Wait].map(|s| Arc::new(build(s)))
    });
    all[shape as usize].clone()
}
