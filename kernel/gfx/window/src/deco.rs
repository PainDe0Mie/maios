//! Rendu anticrénelé des décorations de fenêtre MaiOS (thème Tokyo Night).
//!
//! - Barre de titre en dégradé vertical, liseré lumineux interne, séparateur sombre.
//! - Bordure fine de 1 px tout autour, avec les quatre coins arrondis.
//! - Boutons « feux tricolores » à gauche (fermer, réduire, agrandir), anneau
//!   sombre et pictogrammes vectoriels affichés au survol du groupe.
//! - Titre centré, tronqué avec « ... » s'il ne tient pas.
//!
//! Les coins sont lissés en écrivant une transparence partielle dans le canal
//! alpha (`0` = opaque, `255` = invisible) : le compositeur les fond ensuite
//! sur le fond d'écran. Aucun flottant : couverture en 1/16 de pixel.

use framebuffer::AlphaPixel;

// ── Couleurs ──────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    r: u8,
    g: u8,
    b: u8,
}

const fn rgb(hex: u32) -> Rgb {
    Rgb { r: (hex >> 16) as u8, g: (hex >> 8) as u8, b: hex as u8 }
}

const WHITE: Rgb = rgb(0xFFFFFF);
const BLACK: Rgb = rgb(0x000000);

impl Rgb {
    /// Mélange vers `o` : `t` va de 0 (self) à 256 (o).
    fn mix(self, o: Rgb, t: u32) -> Rgb {
        let t = t.min(256);
        let f = |a: u8, b: u8| ((a as u32 * (256 - t) + b as u32 * t) >> 8) as u8;
        Rgb { r: f(self.r, o.r), g: f(self.g, o.g), b: f(self.b, o.b) }
    }
    fn lighten(self, t: u32) -> Rgb { self.mix(WHITE, t) }
    fn darken(self, t: u32) -> Rgb { self.mix(BLACK, t) }
}

struct Palette {
    bar_top:    Rgb,
    bar_bottom: Rgb,
    highlight:  Rgb,
    separator:  Rgb,
    border:     Rgb,
    title:      Rgb,
}

const ACTIVE: Palette = Palette {
    bar_top:    rgb(0x262A3F),
    bar_bottom: rgb(0x1F2335),
    highlight:  rgb(0x363C59),
    separator:  rgb(0x14151D),
    border:     rgb(0x4560A8),
    title:      rgb(0xC0CAF5),
};

const INACTIVE: Palette = Palette {
    bar_top:    rgb(0x1E2030),
    bar_bottom: rgb(0x1A1B26),
    highlight:  rgb(0x272A3B),
    separator:  rgb(0x121318),
    border:     rgb(0x2C3147),
    title:      rgb(0x5A6390),
};

fn palette(active: bool) -> &'static Palette {
    if active { &ACTIVE } else { &INACTIVE }
}

/// Couleurs des boutons, de gauche à droite : fermer, réduire, agrandir.
const BTN_COLORS: [Rgb; 3] = [rgb(0xF7768E), rgb(0xE0AF68), rgb(0x9ECE6A)];
/// Boutons d'une fenêtre inactive (non survolée).
const BTN_IDLE_INACTIVE: Rgb = rgb(0x3B4261);
/// Couleur des pictogrammes (×, −, +) dessinés sur les boutons.
const BTN_GLYPH: Rgb = rgb(0x16161E);

// ── Géométrie ─────────────────────────────────────────────────────────────────

/// Diamètre d'un bouton.
pub const BTN_DIAMETER: isize = 12;
/// Espace entre deux boutons.
pub const BTN_GAP: isize = 8;
/// Marge entre le bord gauche de la fenêtre et le premier bouton.
pub const BTN_LEFT: isize = 14;
/// Nombre de boutons.
pub const BTN_COUNT: usize = 3;
/// Demi-largeur / demi-hauteur de la zone cliquable d'un bouton.
const BTN_HIT_HALF: isize = 9;

/// Zone horizontale `[début, fin)` occupée par le groupe de boutons.
pub const BUTTON_ZONE: (isize, isize) = (
    BTN_LEFT - 6,
    BTN_LEFT + BTN_COUNT as isize * BTN_DIAMETER + (BTN_COUNT as isize - 1) * BTN_GAP + 6,
);

/// Marge à droite du titre.
const TITLE_MARGIN_RIGHT: isize = 14;

const CHAR_W: isize = font::CHARACTER_WIDTH as isize;
const CHAR_H: isize = font::CHARACTER_HEIGHT as isize;

/// Centre horizontal du bouton `index` (0 = fermer).
pub fn button_center_x(index: usize) -> isize {
    BTN_LEFT + index as isize * (BTN_DIAMETER + BTN_GAP) + BTN_DIAMETER / 2
}

/// Retourne l'index du bouton sous `(x, y)` (coordonnées relatives à la fenêtre).
pub fn button_at(x: isize, y: isize, title_bar_height: isize) -> Option<usize> {
    let cy = title_bar_height / 2;
    if (y - cy).abs() > BTN_HIT_HALF + 1 {
        return None;
    }
    (0..BTN_COUNT).find(|&i| (x - button_center_x(i)).abs() <= BTN_HIT_HALF)
}

/// Paramètres de rendu d'une fenêtre.
#[derive(Clone, Copy)]
pub struct Style {
    pub title_bar_height: isize,
    /// Rayon des coins (0 quand la fenêtre est agrandie).
    pub radius: isize,
    pub active: bool,
}

// ── Surface de dessin ─────────────────────────────────────────────────────────

const CLEAR: AlphaPixel = AlphaPixel { alpha: 255, red: 0, green: 0, blue: 0 };

/// Pixel de couleur `c` dont l'opacité vaut `cov` / 16.
#[inline]
fn px(c: Rgb, cov: u32) -> AlphaPixel {
    let cov = cov.min(16);
    AlphaPixel { alpha: (255 - cov * 255 / 16) as u8, red: c.r, green: c.g, blue: c.b }
}

/// Vue mutable sur le framebuffer d'une fenêtre.
pub struct Surface<'a> {
    buf: &'a mut [AlphaPixel],
    w: isize,
    h: isize,
}

impl<'a> Surface<'a> {
    pub fn new(buf: &'a mut [AlphaPixel], w: usize, h: usize) -> Self {
        debug_assert!(buf.len() >= w * h);
        Surface { buf, w: w as isize, h: h as isize }
    }

    #[inline]
    fn set(&mut self, x: isize, y: isize, p: AlphaPixel) {
        if x >= 0 && y >= 0 && x < self.w && y < self.h {
            self.buf[(y * self.w + x) as usize] = p;
        }
    }

    /// Mélange `c` sur le pixel existant (opacité `a` sur 255) sans toucher à son alpha.
    #[inline]
    fn blend(&mut self, x: isize, y: isize, c: Rgb, a: u32) {
        if a == 0 || x < 0 || y < 0 || x >= self.w || y >= self.h {
            return;
        }
        let a = a.min(255);
        let ia = 255 - a;
        let p = &mut self.buf[(y * self.w + x) as usize];
        p.red   = ((c.r as u32 * a + p.red   as u32 * ia + 127) / 255) as u8;
        p.green = ((c.g as u32 * a + p.green as u32 * ia + 127) / 255) as u8;
        p.blue  = ((c.b as u32 * a + p.blue  as u32 * ia + 127) / 255) as u8;
    }
}

// ── Cadre ─────────────────────────────────────────────────────────────────────

/// Pixel du cadre en `(x, y)` : `bg` à l'intérieur, bordure sur le contour,
/// transparence partielle à l'extérieur des coins.
#[inline]
fn frame_pixel(s: &Surface, st: &Style, x: isize, y: isize, bg: Rgb, border: Rgb) -> AlphaPixel {
    let r = st.radius;
    let outer = rr_cov(x, y, 0, 0, s.w, s.h, r);
    if outer == 0 {
        return CLEAR;
    }
    let inner = rr_cov(x, y, 1, 1, s.w - 2, s.h - 2, (r - 1).max(0));
    px(border.mix(bg, inner * 16), outer)
}

/// Repeint les colonnes `[x0, x1)` de la barre de titre (fond, liseré, séparateur,
/// bordure supérieure et coins du haut).
pub fn paint_title_bar(s: &mut Surface, st: &Style, x0: isize, x1: isize) {
    let pal = palette(st.active);
    let tbh = st.title_bar_height.min(s.h);
    let r = st.radius;
    let (x0, x1) = (x0.max(0), x1.min(s.w));

    for y in 0..tbh {
        let bg = if y == tbh - 1 {
            pal.separator
        } else if y == 1 {
            pal.highlight
        } else {
            pal.bar_top.mix(pal.bar_bottom, (y * 256 / tbh.max(1)) as u32)
        };
        let solid = px(bg, 16);
        let edge_row = y <= r;
        for x in x0..x1 {
            let p = if edge_row || x <= r || x >= s.w - 1 - r {
                frame_pixel(s, st, x, y, bg, pal.border)
            } else {
                solid
            };
            s.set(x, y, p);
        }
    }
}

/// Dessine les bordures gauche, droite et basse puis les coins du bas.
pub fn paint_side_borders(s: &mut Surface, st: &Style) {
    let pal = palette(st.active);
    let b = px(pal.border, 16);
    for y in st.title_bar_height..s.h {
        s.set(0, y, b);
        s.set(s.w - 1, y, b);
    }
    for x in 0..s.w {
        s.set(x, s.h - 1, b);
    }
    paint_bottom_corners(s, st);
}

/// Arrondit les coins du bas.
///
/// Ces coins débordent sur la zone de contenu que l'application redessine : cette
/// fonction est donc rappelée à chaque `Window::render`. Elle est idempotente —
/// elle écrase les pixels du contour au lieu de les mélanger — pour ne pas
/// s'accumuler d'une image à l'autre.
pub fn paint_bottom_corners(s: &mut Surface, st: &Style) {
    let r = st.radius;
    if r <= 0 {
        return;
    }
    let border = palette(st.active).border;
    let y0 = (s.h - r - 1).max(st.title_bar_height);
    for y in y0..s.h {
        for x in (0..=r).chain((s.w - 1 - r)..s.w) {
            let inner = rr_cov(x, y, 1, 1, s.w - 2, s.h - 2, r - 1);
            if inner == 16 {
                continue;
            }
            let outer = rr_cov(x, y, 0, 0, s.w, s.h, r);
            let p = if outer == 0 { CLEAR } else { px(border, outer) };
            s.set(x, y, p);
        }
    }
}

// ── Titre ─────────────────────────────────────────────────────────────────────

/// Convertit un caractère Unicode vers son index dans la police CP437.
fn glyph_index(ch: char) -> usize {
    match ch {
        'Ç' => 0x80, 'ü' => 0x81, 'é' => 0x82, 'â' => 0x83, 'ä' => 0x84,
        'à' => 0x85, 'ç' => 0x87, 'ê' => 0x88, 'ë' => 0x89, 'è' => 0x8A,
        'ï' => 0x8B, 'î' => 0x8C, 'É' => 0x90, 'ô' => 0x93, 'ö' => 0x94,
        'û' => 0x96, 'ù' => 0x97,
        c if (c as u32) < 128 => c as usize,
        _ => b'?' as usize,
    }
}

fn draw_char(s: &mut Surface, x: isize, y: isize, ch: char, c: Rgb) {
    let bitmap = &font::FONT_BASIC[glyph_index(ch)];
    for (row, bits) in bitmap.iter().enumerate() {
        for col in 0..8isize {
            if bits & (0x80 >> col) != 0 {
                s.blend(x + col, y + row as isize, c, 255);
            }
        }
    }
}

/// Dessine le titre centré sur la largeur de la fenêtre, sans jamais chevaucher
/// les boutons, et tronqué avec « ... » s'il est trop long.
pub fn paint_title(s: &mut Surface, st: &Style, title: &str) {
    let color = palette(st.active).title;
    let x_min = BUTTON_ZONE.1 + 6;
    let x_max = s.w - TITLE_MARGIN_RIGHT;
    let max_chars = (x_max - x_min) / CHAR_W;
    if max_chars < 4 {
        return;
    }

    let n = title.chars().count() as isize;
    let (count, ellipsis) = if n <= max_chars { (n, false) } else { (max_chars - 3, true) };
    let shown = count + if ellipsis { 3 } else { 0 };
    let tw = shown * CHAR_W;

    let x = ((s.w - tw) / 2).max(x_min).min(x_max - tw);
    // Les majuscules occupent les lignes 2..12 de la police : on recentre d'1 px.
    let y = (st.title_bar_height - CHAR_H) / 2 + 1;

    let mut cx = x;
    for ch in title.chars().take(count as usize) {
        draw_char(s, cx, y, ch, color);
        cx += CHAR_W;
    }
    if ellipsis {
        for _ in 0..3 {
            draw_char(s, cx, y, '.', color);
            cx += CHAR_W;
        }
    }
}

// ── Boutons ───────────────────────────────────────────────────────────────────

/// Segments des pictogrammes, en 1/16 px relatifs au centre du bouton.
const GLYPH_CLOSE: [(isize, isize, isize, isize); 2] = [(-34, -34, 34, 34), (34, -34, -34, 34)];
const GLYPH_MINIMIZE: [(isize, isize, isize, isize); 1] = [(-42, 0, 42, 0)];
const GLYPH_MAXIMIZE: [(isize, isize, isize, isize); 2] = [(-40, 0, 40, 0), (0, -40, 0, 40)];
/// Demi-épaisseur des traits des pictogrammes (1/16 px).
const GLYPH_HALF_THICKNESS: isize = 13;

/// Repeint tout le groupe de boutons.
///
/// `hovered` : bouton survolé ; `pressed` : bouton gauche maintenu dessus.
/// Les pictogrammes apparaissent sur les trois boutons dès que l'un est survolé.
pub fn paint_buttons(s: &mut Surface, st: &Style, hovered: Option<usize>, pressed: bool) {
    // Repartir d'un fond propre : le lissage des cercles se mélange au fond.
    paint_title_bar(s, st, BUTTON_ZONE.0, BUTTON_ZONE.1);

    let show_glyphs = hovered.is_some();
    let cy16 = st.title_bar_height * 8;
    let r16 = BTN_DIAMETER * 8;
    let reach = BTN_DIAMETER / 2 + 1;

    for i in 0..BTN_COUNT {
        let cx16 = button_center_x(i) * 16;
        let base = if st.active || show_glyphs { BTN_COLORS[i] } else { BTN_IDLE_INACTIVE };
        let fill = match hovered {
            Some(h) if h == i && pressed => base.darken(64),
            Some(h) if h == i => base.lighten(36),
            _ => base,
        };
        let ring = fill.darken(56);
        let glyph: &[(isize, isize, isize, isize)] = match i {
            0 => &GLYPH_CLOSE,
            1 => &GLYPH_MINIMIZE,
            _ => &GLYPH_MAXIMIZE,
        };

        let (cx, cy) = (cx16 / 16, cy16 / 16);
        for py in (cy - reach)..(cy + reach) {
            for pxl in (cx - reach)..(cx + reach) {
                let sx = pxl * 16 + 8 - cx16;
                let sy = py * 16 + 8 - cy16;
                let d = isqrt((sx * sx + sy * sy) as i64) as isize;
                let outer = (r16 + 8 - d).clamp(0, 16) as u32;
                if outer == 0 {
                    continue;
                }
                let inner = (r16 - 16 + 8 - d).clamp(0, 16) as u32;
                s.blend(pxl, py, ring.mix(fill, inner * 16), outer * 255 / 16);

                if show_glyphs {
                    let cov = glyph
                        .iter()
                        .map(|&(ax, ay, bx, by)| seg_cov(sx, sy, ax, ay, bx, by, GLYPH_HALF_THICKNESS))
                        .max()
                        .unwrap_or(0);
                    s.blend(pxl, py, BTN_GLYPH, cov * 200 / 16);
                }
            }
        }
    }
}

// ── Maths entières ────────────────────────────────────────────────────────────

fn isqrt(n: i64) -> i64 {
    if n <= 0 {
        return 0;
    }
    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

/// Couverture (0..=16) d'un pixel par un rectangle arrondi.
fn rr_cov(px: isize, py: isize, x: isize, y: isize, w: isize, h: isize, r: isize) -> u32 {
    if w <= 0 || h <= 0 || px < x || py < y || px >= x + w || py >= y + h {
        return 0;
    }
    let sx = px * 16 + 8;
    let sy = py * 16 + 8;
    let qx = if px < x + r {
        (x + r) * 16 - sx
    } else if px >= x + w - r {
        sx - (x + w - r) * 16
    } else {
        0
    }
    .max(0);
    let qy = if py < y + r {
        (y + r) * 16 - sy
    } else if py >= y + h - r {
        sy - (y + h - r) * 16
    } else {
        0
    }
    .max(0);
    if qx == 0 && qy == 0 {
        return 16;
    }
    let d = if qx == 0 {
        qy
    } else if qy == 0 {
        qx
    } else {
        isqrt((qx * qx + qy * qy) as i64) as isize
    };
    (r * 16 + 8 - d).clamp(0, 16) as u32
}

/// Couverture (0..=16) du point `(sx, sy)` par un segment à bouts ronds
/// d'épaisseur `2 * half` (tout en 1/16 px).
fn seg_cov(sx: isize, sy: isize, ax: isize, ay: isize, bx: isize, by: isize, half: isize) -> u32 {
    let (vx, vy) = ((bx - ax) as i64, (by - ay) as i64);
    let (wx, wy) = ((sx - ax) as i64, (sy - ay) as i64);
    let l2 = vx * vx + vy * vy;
    let (qx, qy) = if l2 == 0 {
        (wx, wy)
    } else {
        let t = (wx * vx + wy * vy).clamp(0, l2);
        (wx - vx * t / l2, wy - vy * t / l2)
    };
    let d = isqrt(qx * qx + qy * qy) as isize;
    (half + 8 - d).clamp(0, 16) as u32
}
