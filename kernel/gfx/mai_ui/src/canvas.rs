//! Canevas logiciel des applications MaiOS : écriture directe dans le tampon d'une fenêtre,
//! avec découpage (*clipping*), opacité réelle et bords anticrénelés.
//!
//! - Toutes les primitives passent par un rectangle de découpage : un composant ne déborde
//!   jamais de la zone qu'on lui donne (listes défilantes, colonnes, panneaux).
//! - Les coins arrondis et les traits sont anticrénelés par couverture analytique (distance au
//!   bord en virgule fixe 8 bits, sans flottant) ; les courbes des graphes par l'algorithme de
//!   Xiaolin Wu (1991).
//! - Les pixels écrits restent opaques pour le compositeur (`alpha == 0`) : le mélange est fait
//!   ici, une seule fois.
//! - Texte : police bitmap 9×16 (CP437), avec les lettres accentuées du français.

use framebuffer::{AlphaPixel, Framebuffer};

// ================================================================
// COULEURS
// ================================================================

/// Couleur 24 bits.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// Couleur depuis `0xRRGGBB`.
pub const fn rgb(hex: u32) -> Rgb {
    Rgb { r: (hex >> 16) as u8, g: (hex >> 8) as u8, b: hex as u8 }
}

pub const WHITE: Rgb = rgb(0xFFFFFF);
pub const BLACK: Rgb = rgb(0x000000);

impl Rgb {
    /// Mélange vers `o` : `t` va de 0 (`self`) à 256 (`o`).
    pub fn mix(self, o: Rgb, t: u32) -> Rgb {
        let t = t.min(256);
        let f = |a: u8, b: u8| ((a as u32 * (256 - t) + b as u32 * t) >> 8) as u8;
        Rgb { r: f(self.r, o.r), g: f(self.g, o.g), b: f(self.b, o.b) }
    }
    pub fn lighten(self, t: u32) -> Rgb { self.mix(WHITE, t) }
    pub fn darken(self, t: u32) -> Rgb { self.mix(BLACK, t) }

    /// Luminance perçue (0–255), pour choisir un texte clair ou foncé sur un fond.
    pub fn luma(self) -> u32 { (self.r as u32 * 54 + self.g as u32 * 183 + self.b as u32 * 19) >> 8 }
}

impl From<color::Color> for Rgb {
    fn from(c: color::Color) -> Self { Rgb { r: c.red(), g: c.green(), b: c.blue() } }
}

#[inline(always)]
fn blend(p: &mut AlphaPixel, c: Rgb, a: u32) {
    if a >= 255 {
        p.red = c.r;
        p.green = c.g;
        p.blue = c.b;
    } else if a > 0 {
        let ia = 255 - a;
        p.red = ((c.r as u32 * a + p.red as u32 * ia + 127) / 255) as u8;
        p.green = ((c.g as u32 * a + p.green as u32 * ia + 127) / 255) as u8;
        p.blue = ((c.b as u32 * a + p.blue as u32 * ia + 127) / 255) as u8;
    }
    p.alpha = 0;
}

// ================================================================
// RECTANGLES
// ================================================================

/// Rectangle entier (coordonnées du tampon de la fenêtre).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rect {
    pub x: isize,
    pub y: isize,
    pub w: isize,
    pub h: isize,
}

impl Rect {
    pub const fn new(x: isize, y: isize, w: isize, h: isize) -> Rect { Rect { x, y, w, h } }
    pub const fn right(&self) -> isize { self.x + self.w }
    pub const fn bottom(&self) -> isize { self.y + self.h }
    pub fn is_empty(&self) -> bool { self.w <= 0 || self.h <= 0 }

    pub fn contains(&self, px: isize, py: isize) -> bool {
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }

    pub fn intersect(&self, o: &Rect) -> Rect {
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = self.right().min(o.right());
        let y1 = self.bottom().min(o.bottom());
        Rect::new(x0, y0, (x1 - x0).max(0), (y1 - y0).max(0))
    }

    /// Rétrécit de `d` pixels de chaque côté.
    pub fn inset(&self, d: isize) -> Rect { self.inset_xy(d, d) }
    pub fn inset_xy(&self, dx: isize, dy: isize) -> Rect {
        Rect::new(self.x + dx, self.y + dy, (self.w - 2 * dx).max(0), (self.h - 2 * dy).max(0))
    }

    /// Coupe une bande de `h` pixels en haut : `(bande, reste)`.
    pub fn split_top(&self, h: isize) -> (Rect, Rect) {
        let h = h.clamp(0, self.h);
        (Rect::new(self.x, self.y, self.w, h), Rect::new(self.x, self.y + h, self.w, self.h - h))
    }
    pub fn split_bottom(&self, h: isize) -> (Rect, Rect) {
        let h = h.clamp(0, self.h);
        (Rect::new(self.x, self.y, self.w, self.h - h), Rect::new(self.x, self.bottom() - h, self.w, h))
    }
    pub fn split_left(&self, w: isize) -> (Rect, Rect) {
        let w = w.clamp(0, self.w);
        (Rect::new(self.x, self.y, w, self.h), Rect::new(self.x + w, self.y, self.w - w, self.h))
    }
    pub fn split_right(&self, w: isize) -> (Rect, Rect) {
        let w = w.clamp(0, self.w);
        (Rect::new(self.x, self.y, self.w - w, self.h), Rect::new(self.right() - w, self.y, w, self.h))
    }

    /// Centre `(w, h)` dans ce rectangle.
    pub fn center(&self, w: isize, h: isize) -> Rect {
        Rect::new(self.x + (self.w - w) / 2, self.y + (self.h - h) / 2, w, h)
    }
}

// ================================================================
// MATHS ENTIÈRES
// ================================================================

pub fn isqrt(n: i64) -> i64 {
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

// ================================================================
// TEXTE
// ================================================================

pub const CHAR_W: isize = font::CHARACTER_WIDTH as isize;
pub const CHAR_H: isize = font::CHARACTER_HEIGHT as isize;

/// Index d'un caractère Unicode dans la police CP437 (lettres accentuées et symboles utiles).
pub fn glyph_index(ch: char) -> usize {
    match ch {
        'Ç' => 0x80, 'ü' => 0x81, 'é' => 0x82, 'â' => 0x83, 'ä' => 0x84, 'à' => 0x85, 'å' => 0x86,
        'ç' => 0x87, 'ê' => 0x88, 'ë' => 0x89, 'è' => 0x8A, 'ï' => 0x8B, 'î' => 0x8C, 'ì' => 0x8D,
        'Ä' => 0x8E, 'Å' => 0x8F, 'É' => 0x90, 'æ' => 0x91, 'Æ' => 0x92, 'ô' => 0x93, 'ö' => 0x94,
        'ò' => 0x95, 'û' => 0x96, 'ù' => 0x97, 'ÿ' => 0x98, 'Ö' => 0x99, 'Ü' => 0x9A, '£' => 0x9C,
        'á' => 0xA0, 'í' => 0xA1, 'ó' => 0xA2, 'ú' => 0xA3, 'ñ' => 0xA4, 'Ñ' => 0xA5, '¿' => 0xA8,
        '½' => 0xAB, '¼' => 0xAC, '¡' => 0xAD, '«' => 0xAE, '»' => 0xAF, '░' => 0xB0, '▒' => 0xB1,
        '▓' => 0xB2, 'µ' => 0xE6, '±' => 0xF1, '≥' => 0xF2, '≤' => 0xF3, '÷' => 0xF6, '°' => 0xF8,
        '·' => 0xFA, '²' => 0xFD, '■' => 0xFE, '▲' => 0x1E, '▼' => 0x1F, '►' => 0x10, '◄' => 0x11,
        '↑' => 0x18, '↓' => 0x19, '→' => 0x1A, '←' => 0x1B, '♦' => 0x04, '•' => 0x07,
        // Majuscules accentuées absentes de CP437 : lettre de base.
        'À' | 'Â' => b'A' as usize, 'È' | 'Ê' | 'Ë' => b'E' as usize, 'Î' | 'Ï' => b'I' as usize,
        'Ô' => b'O' as usize, 'Ù' | 'Û' => b'U' as usize,
        '’' | '‘' => b'\'' as usize, '“' | '”' => b'"' as usize, '–' | '—' => b'-' as usize,
        '…' => 0xFA,
        c if (c as u32) < 128 => c as usize,
        _ => b'?' as usize,
    }
}

/// Ligatures absentes de la police CP437, écrites en deux lettres (« cœur » → « coeur »).
fn expand(s: &str) -> alloc::borrow::Cow<'_, str> {
    if s.contains(['œ', 'Œ']) {
        alloc::borrow::Cow::Owned(s.replace('œ', "oe").replace('Œ', "OE"))
    } else {
        alloc::borrow::Cow::Borrowed(s)
    }
}

/// Largeur en pixels d'un texte (police à chasse fixe).
pub fn text_width(s: &str) -> isize { expand(s).chars().count() as isize * CHAR_W }

// ================================================================
// CANEVAS
// ================================================================

/// Canevas sur le tampon d'une fenêtre.
pub struct Canvas<'a> {
    buf: &'a mut [AlphaPixel],
    stride: usize,
    w: isize,
    h: isize,
    clip: Rect,
}

impl<'a> Canvas<'a> {
    pub fn new(buf: &'a mut [AlphaPixel], width: usize, height: usize) -> Canvas<'a> {
        let (w, h) = (width as isize, height as isize);
        Canvas { buf, stride: width, w, h, clip: Rect::new(0, 0, w, h) }
    }

    pub fn from_framebuffer(fb: &'a mut Framebuffer<AlphaPixel>) -> Canvas<'a> {
        let (w, h) = (fb.width(), fb.height());
        Canvas::new(fb.buffer_mut(), w, h)
    }

    pub fn width(&self) -> isize { self.w }
    pub fn height(&self) -> isize { self.h }

    /// Zone de découpage courante.
    pub fn clip(&self) -> Rect { self.clip }

    /// Restreint le découpage à `r` (intersection) ; renvoie l'ancienne zone, à rétablir par
    /// [`Canvas::set_clip`].
    pub fn push_clip(&mut self, r: Rect) -> Rect {
        let old = self.clip;
        self.clip = old.intersect(&r);
        old
    }

    pub fn set_clip(&mut self, r: Rect) { self.clip = r.intersect(&Rect::new(0, 0, self.w, self.h)); }

    #[inline(always)]
    fn px(&mut self, x: isize, y: isize, c: Rgb, a: u32) {
        if self.clip.contains(x, y) {
            let i = y as usize * self.stride + x as usize;
            blend(&mut self.buf[i], c, a);
        }
    }

    /// Pixel avec opacité `a` (0–255).
    pub fn pixel(&mut self, x: isize, y: isize, c: Rgb, a: u32) { self.px(x, y, c, a) }

    /// Remplit un rectangle.
    pub fn fill(&mut self, r: Rect, c: Rgb) { self.fill_alpha(r, c, 255) }

    /// Remplit un rectangle avec l'opacité `a`.
    pub fn fill_alpha(&mut self, r: Rect, c: Rgb, a: u32) {
        let r = r.intersect(&self.clip);
        if r.is_empty() || a == 0 {
            return;
        }
        for y in r.y..r.bottom() {
            let row = y as usize * self.stride;
            let line = &mut self.buf[row + r.x as usize..row + r.right() as usize];
            if a >= 255 {
                let p = AlphaPixel { blue: c.b, green: c.g, red: c.r, alpha: 0 };
                line.fill(p);
            } else {
                for p in line.iter_mut() {
                    blend(p, c, a);
                }
            }
        }
    }

    /// Dégradé vertical.
    pub fn vgradient(&mut self, r: Rect, top: Rgb, bottom: Rgb) {
        if r.h <= 0 {
            return;
        }
        for i in 0..r.h {
            let t = if r.h > 1 { (i * 256 / (r.h - 1)) as u32 } else { 0 };
            self.fill(Rect::new(r.x, r.y + i, r.w, 1), top.mix(bottom, t));
        }
    }

    /// Dégradé horizontal.
    pub fn hgradient(&mut self, r: Rect, left: Rgb, right: Rgb) {
        if r.w <= 0 {
            return;
        }
        for i in 0..r.w {
            let t = if r.w > 1 { (i * 256 / (r.w - 1)) as u32 } else { 0 };
            self.fill(Rect::new(r.x + i, r.y, 1, r.h), left.mix(right, t));
        }
    }

    pub fn hline(&mut self, x0: isize, x1: isize, y: isize, c: Rgb) {
        self.fill(Rect::new(x0, y, x1 - x0, 1), c);
    }
    pub fn vline(&mut self, x: isize, y0: isize, y1: isize, c: Rgb) {
        self.fill(Rect::new(x, y0, 1, y1 - y0), c);
    }

    /// Contour d'un pixel.
    pub fn stroke(&mut self, r: Rect, c: Rgb) {
        if r.is_empty() {
            return;
        }
        self.hline(r.x, r.right(), r.y, c);
        self.hline(r.x, r.right(), r.bottom() - 1, c);
        self.vline(r.x, r.y, r.bottom(), c);
        self.vline(r.right() - 1, r.y, r.bottom(), c);
    }

    /// Couverture (0–255) d'un pixel à `d16` seizièmes de pixel du bord d'un disque de rayon `rad`.
    #[inline]
    fn coverage(dist16: i64, rad16: i64) -> u32 {
        // Le centre du pixel est couvert si `dist <= rad` ; transition linéaire sur un pixel.
        let edge = rad16 + 8 - dist16; // en seizièmes
        if edge <= 0 {
            0
        } else if edge >= 16 {
            255
        } else {
            (edge * 255 / 16) as u32
        }
    }

    /// Rectangle plein à coins arrondis anticrénelés, opacité `a`.
    pub fn round_rect_alpha(&mut self, r: Rect, rad: isize, c: Rgb, a: u32) {
        if r.is_empty() {
            return;
        }
        let rad = rad.min(r.w / 2).min(r.h / 2).max(0);
        if rad == 0 {
            self.fill_alpha(r, c, a);
            return;
        }
        // Corps sans les coins.
        self.fill_alpha(Rect::new(r.x + rad, r.y, r.w - 2 * rad, r.h), c, a);
        self.fill_alpha(Rect::new(r.x, r.y + rad, rad, r.h - 2 * rad), c, a);
        self.fill_alpha(Rect::new(r.right() - rad, r.y + rad, rad, r.h - 2 * rad), c, a);
        // Coins : couverture par la distance au centre de l'arc (centres de pixels à +½).
        let rad16 = rad as i64 * 16;
        for dy in 0..rad {
            for dx in 0..rad {
                // Distance du centre du pixel (dx+½, dy+½) au centre du coin (rad, rad).
                let fx = (rad as i64 * 16) - (dx as i64 * 16 + 8);
                let fy = (rad as i64 * 16) - (dy as i64 * 16 + 8);
                let d = isqrt(fx * fx + fy * fy);
                let cov = Self::coverage(d, rad16 - 8);
                if cov == 0 {
                    continue;
                }
                let aa = cov * a / 255;
                self.px(r.x + dx, r.y + dy, c, aa);
                self.px(r.right() - 1 - dx, r.y + dy, c, aa);
                self.px(r.x + dx, r.bottom() - 1 - dy, c, aa);
                self.px(r.right() - 1 - dx, r.bottom() - 1 - dy, c, aa);
            }
        }
    }

    pub fn round_rect(&mut self, r: Rect, rad: isize, c: Rgb) { self.round_rect_alpha(r, rad, c, 255) }

    /// Contour d'un pixel à coins arrondis, anticrénelé.
    pub fn round_stroke(&mut self, r: Rect, rad: isize, c: Rgb, a: u32) {
        if r.is_empty() {
            return;
        }
        let rad = rad.min(r.w / 2).min(r.h / 2).max(0);
        self.fill_alpha(Rect::new(r.x + rad, r.y, r.w - 2 * rad, 1), c, a);
        self.fill_alpha(Rect::new(r.x + rad, r.bottom() - 1, r.w - 2 * rad, 1), c, a);
        self.fill_alpha(Rect::new(r.x, r.y + rad, 1, r.h - 2 * rad), c, a);
        self.fill_alpha(Rect::new(r.right() - 1, r.y + rad, 1, r.h - 2 * rad), c, a);
        if rad == 0 {
            return;
        }
        let rad16 = rad as i64 * 16 - 8;
        for dy in 0..rad {
            for dx in 0..rad {
                let fx = (rad as i64 * 16) - (dx as i64 * 16 + 8);
                let fy = (rad as i64 * 16) - (dy as i64 * 16 + 8);
                let d = isqrt(fx * fx + fy * fy);
                // Anneau d'un pixel : couvert entre rad-1 et rad.
                let outer = Self::coverage(d, rad16);
                let inner = Self::coverage(d, rad16 - 16);
                let cov = outer.saturating_sub(inner);
                if cov == 0 {
                    continue;
                }
                let aa = cov * a / 255;
                self.px(r.x + dx, r.y + dy, c, aa);
                self.px(r.right() - 1 - dx, r.y + dy, c, aa);
                self.px(r.x + dx, r.bottom() - 1 - dy, c, aa);
                self.px(r.right() - 1 - dx, r.bottom() - 1 - dy, c, aa);
            }
        }
    }

    /// Ombre portée douce sous un rectangle arrondi (couches d'opacité décroissante).
    pub fn shadow(&mut self, r: Rect, rad: isize, spread: isize, strength: u32) {
        for i in (1..=spread).rev() {
            let a = strength * (spread - i + 1) as u32 / (spread as u32 * 3).max(1);
            self.round_rect_alpha(Rect::new(r.x - i, r.y - i + spread / 2, r.w + 2 * i, r.h + 2 * i), rad + i, BLACK, a);
        }
    }

    /// Disque plein anticrénelé de centre `(cx, cy)` et de rayon `rad`.
    pub fn disc(&mut self, cx: isize, cy: isize, rad: isize, c: Rgb, a: u32) {
        let rad16 = rad as i64 * 16;
        for y in -rad - 1..=rad {
            for x in -rad - 1..=rad {
                let fx = x as i64 * 16 + 8;
                let fy = y as i64 * 16 + 8;
                let cov = Self::coverage(isqrt(fx * fx + fy * fy), rad16 - 8);
                if cov > 0 {
                    self.px(cx + x, cy + y, c, cov * a / 255);
                }
            }
        }
    }

    /// Segment anticrénelé (Xiaolin Wu), extrémités en 1/256 de pixel.
    pub fn line_fx(&mut self, x0: i64, y0: i64, x1: i64, y1: i64, c: Rgb, a: u32) {
        let (mut x0, mut y0, mut x1, mut y1) = (x0, y0, x1, y1);
        let steep = (y1 - y0).abs() > (x1 - x0).abs();
        if steep {
            core::mem::swap(&mut x0, &mut y0);
            core::mem::swap(&mut x1, &mut y1);
        }
        if x0 > x1 {
            core::mem::swap(&mut x0, &mut x1);
            core::mem::swap(&mut y0, &mut y1);
        }
        let dx = x1 - x0;
        let dy = y1 - y0;
        let grad = if dx == 0 { 256 } else { dy * 256 / dx }; // pente en 1/256
        let xs = x0 >> 8;
        let xe = (x1 + 255) >> 8;
        let mut y = y0 + ((xs << 8) + 128 - x0) * grad / 256;
        for x in xs..=xe {
            let yi = y >> 8;
            let frac = (y & 255) as u32;
            let (p0, p1) = (255 - frac, frac);
            if steep {
                self.px(yi as isize, x as isize, c, p0 * a / 255);
                self.px(yi as isize + 1, x as isize, c, p1 * a / 255);
            } else {
                self.px(x as isize, yi as isize, c, p0 * a / 255);
                self.px(x as isize, yi as isize + 1, c, p1 * a / 255);
            }
            y += grad;
        }
    }

    /// Segment anticrénelé entre deux points entiers.
    pub fn line(&mut self, x0: isize, y0: isize, x1: isize, y1: isize, c: Rgb) {
        self.line_fx(x0 as i64 * 256 + 128, y0 as i64 * 256 + 128, x1 as i64 * 256 + 128, y1 as i64 * 256 + 128, c, 255);
    }

    /// Graphe en aires : `values` (0–`max`) répartis sur la largeur de `r`, du plus ancien au
    /// plus récent (le dernier à droite). Aire remplie en dégradé, courbe anticrénelée.
    pub fn area_chart(&mut self, r: Rect, values: &[u32], max: u32, line: Rgb, fill: Rgb, fill_alpha: u32) {
        if r.w < 2 || r.h < 2 || values.len() < 2 || max == 0 {
            return;
        }
        let n = values.len() as i64;
        let wfx = (r.w as i64 - 1) * 256;
        let hfx = (r.h as i64 - 1) * 256;
        let pt = |i: i64| -> (i64, i64) {
            let v = values[i as usize].min(max) as i64;
            let x = r.x as i64 * 256 + i * wfx / (n - 1);
            let y = r.y as i64 * 256 + hfx - v * hfx / max as i64;
            (x, y)
        };
        // Aire : pour chaque colonne, hauteur interpolée linéairement entre deux échantillons.
        for col in 0..r.w {
            let xf = col as i64 * 256 * (n - 1) / (r.w as i64 - 1).max(1); // position en échantillons ×256
            let i = (xf >> 8).min(n - 2);
            let t = xf - i * 256;
            let v0 = values[i as usize].min(max) as i64;
            let v1 = values[i as usize + 1].min(max) as i64;
            let v = v0 * (256 - t) / 256 + v1 * t / 256;
            let top = r.bottom() - 1 - (v * (r.h as i64 - 1) / max as i64) as isize;
            let x = r.x + col;
            let height = r.bottom() - top;
            if height <= 0 {
                continue;
            }
            // Dégradé vertical de l'aire : plus dense en haut.
            for yy in top..r.bottom() {
                let k = ((r.bottom() - yy) * 256 / r.h.max(1)) as u32; // 0 en bas, 256 en haut
                let a = fill_alpha * (64 + k * 3 / 4) / 256;
                self.px(x, yy, fill, a);
            }
        }
        // Courbe.
        let mut prev = pt(0);
        for i in 1..n {
            let p = pt(i);
            self.line_fx(prev.0 + 128, prev.1 + 128, p.0 + 128, p.1 + 128, line, 255);
            prev = p;
        }
    }

    // ── Texte ────────────────────────────────────────────────────

    fn glyph(&mut self, x: isize, y: isize, ch: char, c: Rgb, a: u32) {
        let bitmap = &font::FONT_BASIC[glyph_index(ch)];
        // Rejet rapide hors découpage.
        let g = Rect::new(x, y, CHAR_W, CHAR_H);
        if g.intersect(&self.clip).is_empty() {
            return;
        }
        for (row, bits) in bitmap.iter().enumerate() {
            if *bits == 0 {
                continue;
            }
            for col in 0..8isize {
                if bits & (0x80 >> col) != 0 {
                    self.px(x + col, y + row as isize, c, a);
                }
            }
        }
    }

    /// Texte ; renvoie la largeur dessinée.
    pub fn text(&mut self, x: isize, y: isize, s: &str, c: Rgb) -> isize {
        let s = expand(s);
        let mut cx = x;
        for ch in s.chars() {
            self.glyph(cx, y, ch, c, 255);
            cx += CHAR_W;
        }
        cx - x
    }

    /// Texte avec opacité.
    pub fn text_alpha(&mut self, x: isize, y: isize, s: &str, c: Rgb, a: u32) -> isize {
        let s = expand(s);
        let mut cx = x;
        for ch in s.chars() {
            self.glyph(cx, y, ch, c, a);
            cx += CHAR_W;
        }
        cx - x
    }

    /// Texte gras (double frappe décalée d'un pixel).
    pub fn text_bold(&mut self, x: isize, y: isize, s: &str, c: Rgb) -> isize {
        self.text(x + 1, y, s, c);
        self.text(x, y, s, c)
    }

    /// Texte tronqué à `max_w` pixels, avec « .. » s'il est coupé.
    pub fn text_fit(&mut self, x: isize, y: isize, max_w: isize, s: &str, c: Rgb) -> isize {
        let s = expand(s);
        let s = s.as_ref();
        let max_chars = (max_w / CHAR_W).max(0) as usize;
        let n = s.chars().count();
        if n <= max_chars {
            return self.text(x, y, s, c);
        }
        if max_chars < 3 {
            let t: alloc::string::String = s.chars().take(max_chars).collect();
            return self.text(x, y, &t, c);
        }
        let t: alloc::string::String = s.chars().take(max_chars - 2).collect();
        let w = self.text(x, y, &t, c);
        w + self.text_alpha(x + w, y, "..", c, 160)
    }

    /// Texte aligné à droite sur `x_right`.
    pub fn text_right(&mut self, x_right: isize, y: isize, s: &str, c: Rgb) -> isize {
        self.text(x_right - text_width(s), y, s, c)
    }

    /// Texte centré horizontalement et verticalement dans `r`.
    pub fn text_center(&mut self, r: Rect, s: &str, c: Rgb) -> isize {
        let w = text_width(s).min(r.w);
        self.text_fit(r.x + (r.w - w) / 2, r.y + (r.h - CHAR_H) / 2, r.w, s, c)
    }

    /// Texte centré verticalement dans `r`, à gauche avec la marge `pad`, tronqué.
    pub fn text_in(&mut self, r: Rect, pad: isize, s: &str, c: Rgb) -> isize {
        self.text_fit(r.x + pad, r.y + (r.h - CHAR_H) / 2, r.w - 2 * pad, s, c)
    }

    /// Texte aligné à droite dans `r`, centré verticalement.
    pub fn text_in_right(&mut self, r: Rect, pad: isize, s: &str, c: Rgb) -> isize {
        let w = text_width(s);
        if w > r.w - 2 * pad {
            return self.text_in(r, pad, s, c);
        }
        self.text(r.right() - pad - w, r.y + (r.h - CHAR_H) / 2, s, c)
    }
}
