//! Rendu logiciel du bureau MaiOS.
//!
//! - `Canvas` : dessin avec opacité réelle (alpha blending) et bords anticrénelés,
//!   directement dans un buffer `AlphaPixel`. Tous les pixels écrits restent opaques
//!   (`alpha == 0`) pour que le compositeur ne les re-mélange pas.
//! - `wallpaper()` : fond d'écran procédural « aurore Tokyo Night » (dégradé tramé,
//!   halos, aurore boréale, étoiles, collines en couches, vignettage).
//! - Chiffres vectoriels pour la grande horloge et pictogrammes d'applications.
//!
//! Aucun flottant : tout est en arithmétique entière à virgule fixe.

#![allow(clippy::too_many_arguments)]

use alloc::vec::Vec;
use framebuffer::AlphaPixel;

// ================================================================
// COULEURS
// ================================================================

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

pub const fn rgb(hex: u32) -> Rgb {
    Rgb { r: (hex >> 16) as u8, g: (hex >> 8) as u8, b: hex as u8 }
}

pub const WHITE: Rgb = rgb(0xFFFFFF);
pub const BLACK: Rgb = rgb(0x000000);

impl Rgb {
    /// Mélange vers `o` : `t` va de 0 (self) à 256 (o).
    pub fn mix(self, o: Rgb, t: u32) -> Rgb {
        let t = t.min(256);
        let f = |a: u8, b: u8| ((a as u32 * (256 - t) + b as u32 * t) >> 8) as u8;
        Rgb { r: f(self.r, o.r), g: f(self.g, o.g), b: f(self.b, o.b) }
    }
    pub fn lighten(self, t: u32) -> Rgb { self.mix(WHITE, t) }
    pub fn darken(self, t: u32) -> Rgb { self.mix(BLACK, t) }
}

impl From<color::Color> for Rgb {
    fn from(c: color::Color) -> Self {
        Rgb { r: c.red(), g: c.green(), b: c.blue() }
    }
}

/// Pose `c` d'opacité `a` (0..=255) sur `p` (opérateur « over »).
///
/// Convention de MaiOS : `alpha` 0 = opaque, 255 = invisible, couleurs non prémultipliées. Sur
/// une destination opaque (le bureau), le résultat reste opaque ; sur une destination
/// transparente (barre des tâches, menu : fenêtres `translucent`), l'opacité s'accumule et le
/// compositeur mélange ensuite avec ce qui est dessous.
#[inline]
fn blend_into(p: &mut AlphaPixel, c: Rgb, a: u32) {
    let a = a.min(255);
    if a == 0 {
        return;
    }
    let da = 255 - p.alpha as u32; // opacité de la destination
    if da == 255 || a == 255 {
        let ia = 255 - a;
        p.red = ((c.r as u32 * a + p.red as u32 * ia + 127) / 255) as u8;
        p.green = ((c.g as u32 * a + p.green as u32 * ia + 127) / 255) as u8;
        p.blue = ((c.b as u32 * a + p.blue as u32 * ia + 127) / 255) as u8;
        p.alpha = if a == 255 { 0 } else { p.alpha };
        return;
    }
    // Poids (×255) de la source et de la destination ; opacité finale ×255.
    let sw = a * 255;
    let dw = da * (255 - a);
    let ow = sw + dw;
    p.red = ((c.r as u32 * sw + p.red as u32 * dw + ow / 2) / ow) as u8;
    p.green = ((c.g as u32 * sw + p.green as u32 * dw + ow / 2) / ow) as u8;
    p.blue = ((c.b as u32 * sw + p.blue as u32 * dw + ow / 2) / ow) as u8;
    p.alpha = (255 - (ow + 127) / 255) as u8;
}

/// Pixel entièrement transparent.
pub const CLEAR: AlphaPixel = AlphaPixel { alpha: 255, red: 0, green: 0, blue: 0 };

/// Copie un tampon de MaiOS en image RGBA prémultipliée (`mai_icons`), pour la remettre à
/// l'échelle ou la poser ailleurs.
pub fn to_image(buf: &[AlphaPixel], w: usize, h: usize) -> mai_icons::Image {
    let pixels = buf[..w * h]
        .iter()
        .map(|p| {
            let a = 255 - p.alpha as u32;
            let m = |c: u8| ((c as u32 * a + 127) / 255) as u8;
            [m(p.red), m(p.green), m(p.blue), a as u8]
        })
        .collect();
    mai_icons::Image { width: w, height: h, pixels }
}

// ================================================================
// MATHS ENTIÈRES
// ================================================================

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

/// Sinus entier (approximation de Bhaskara I).
/// `t` : 4096 unités = une période complète. Retour dans [-1024, 1024].
fn isin(t: i64) -> i64 {
    let t = t.rem_euclid(4096);
    let (half, sign) = if t < 2048 { (t, 1) } else { (t - 2048, -1) };
    let p = half * (2048 - half);
    sign * (16 * p * 1024) / (5 * 2048 * 2048 - 4 * p)
}

/// Générateur pseudo-aléatoire déterministe (xorshift).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
}

// ================================================================
// TEXTE (police bitmap CP437)
// ================================================================

pub const CHAR_W: isize = font::CHARACTER_WIDTH as isize;
pub const CHAR_H: isize = font::CHARACTER_HEIGHT as isize;

/// Convertit un caractère Unicode vers son index dans la police CP437.
fn glyph_index(ch: char) -> usize {
    match ch {
        'Ç' => 0x80, 'ü' => 0x81, 'é' => 0x82, 'â' => 0x83, 'ä' => 0x84,
        'à' => 0x85, 'ç' => 0x87, 'ê' => 0x88, 'ë' => 0x89, 'è' => 0x8A,
        'ï' => 0x8B, 'î' => 0x8C, 'É' => 0x90, 'ô' => 0x93, 'ö' => 0x94,
        'û' => 0x96, 'ù' => 0x97,
        'å' => 0x86, 'ì' => 0x8D, 'Ä' => 0x8E, 'Å' => 0x8F, 'æ' => 0x91, 'Æ' => 0x92,
        'ò' => 0x95, 'ÿ' => 0x98, 'Ö' => 0x99, 'Ü' => 0x9A, 'á' => 0xA0, 'í' => 0xA1,
        'ó' => 0xA2, 'ú' => 0xA3, 'ñ' => 0xA4, 'Ñ' => 0xA5, '«' => 0xAE, '»' => 0xAF,
        '°' => 0xF8, '·' | '•' => 0xFA,
        // Titres venus des applications : ponctuation typographique ramenée à l'ASCII.
        '—' | '–' | '‐' | '−' => b'-' as usize,
        '‘' | '’' | '′' => b'\'' as usize,
        '“' | '”' | '„' => b'"' as usize,
        '…' => 0xFA,
        '\u{a0}' | '\u{202f}' | '\u{2009}' => b' ' as usize,
        'À' | 'Â' => b'A' as usize, 'È' | 'Ê' | 'Ë' => b'E' as usize, 'Î' | 'Ï' => b'I' as usize,
        'Ô' => b'O' as usize, 'Ù' | 'Û' => b'U' as usize, 'œ' => b'o' as usize, 'Œ' => b'O' as usize,
        c if (c as u32) < 128 => c as usize,
        _ => b'?' as usize,
    }
}

/// Coupe `s` pour qu'il tienne en `max_w` pixels (échelle 1), avec « .. » s'il a été coupé.
pub fn ellipsize(s: &str, max_w: isize) -> alloc::string::String {
    let max = (max_w / CHAR_W).max(0) as usize;
    let n = s.chars().count();
    if n <= max {
        return s.into();
    }
    if max <= 2 {
        return s.chars().take(max).collect();
    }
    let mut out: alloc::string::String = s.chars().take(max - 2).collect();
    while out.ends_with(' ') {
        out.pop();
    }
    out.push_str("..");
    out
}

/// Largeur en pixels d'un texte (échelle et espacement inter-lettres).
/// Coupe `s` en deux lignes au plus de `max_w` pixels, aux espaces (ou après `.`, `-`, `_`)
/// si possible, la seconde ligne raccourcie au besoin (`ellipsize`).
pub fn wrap2(s: &str, max_w: isize) -> Vec<alloc::string::String> {
    use alloc::string::String;
    let max = (max_w / CHAR_W).max(1) as usize;
    if s.chars().count() <= max {
        return alloc::vec![s.into()];
    }
    // Dernière coupure possible qui tient sur la première ligne.
    let mut cut: Option<(usize, usize)> = None; // (fin de la 1re ligne, début de la 2de)
    for (n, (i, ch)) in s.char_indices().enumerate() {
        if n > max {
            break;
        }
        if ch == ' ' {
            cut = Some((i, i + 1));
        } else if matches!(ch, '.' | '-' | '_') && n > 0 && n < max {
            cut = Some((i + ch.len_utf8(), i + ch.len_utf8()));
        }
    }
    let (end, start) = cut.unwrap_or_else(|| {
        let i = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
        (i, i)
    });
    let first: String = s[..end].into();
    alloc::vec![first, ellipsize(s[start..].trim_start(), max_w)]
}

pub fn text_width(s: &str, scale: isize, spacing: isize) -> isize {
    let n = s.chars().count() as isize;
    if n == 0 { 0 } else { n * CHAR_W * scale + (n - 1) * spacing }
}

// ================================================================
// CANVAS
// ================================================================

pub struct Canvas<'a> {
    buf: &'a mut [AlphaPixel],
    w: usize,
    h: usize,
}

impl<'a> Canvas<'a> {
    pub fn new(buf: &'a mut [AlphaPixel], w: usize, h: usize) -> Self {
        debug_assert!(buf.len() >= w * h);
        Canvas { buf, w, h }
    }

    pub fn width(&self) -> isize { self.w as isize }

    /// Tampon et taille, pour y dessiner avec un autre canevas (`mai_ui`) : destination opaque
    /// seulement (le bureau).
    pub fn raw(&mut self) -> (&mut [AlphaPixel], usize, usize) { (&mut *self.buf, self.w, self.h) }

    /// Rend tout le tampon transparent (surimpressions).
    pub fn clear(&mut self) {
        let n = self.w * self.h;
        self.buf[..n].fill(CLEAR);
    }

    /// Pose une image RGBA prémultipliée, coin en `(x, y)`, opacité globale `opacity`.
    pub fn image(&mut self, x: isize, y: isize, img: &mai_icons::Image, opacity: u32) {
        let opacity = opacity.min(255);
        for iy in 0..img.height as isize {
            let py = y + iy;
            if py < 0 || py >= self.h as isize {
                continue;
            }
            for ix in 0..img.width as isize {
                let px = x + ix;
                if px < 0 || px >= self.w as isize {
                    continue;
                }
                let [r, g, b, a] = img.pixels[iy as usize * img.width + ix as usize];
                if a == 0 {
                    continue;
                }
                let a32 = a as u32;
                let un = |c: u8| ((c as u32 * 255 + a32 / 2) / a32).min(255) as u8;
                let c = Rgb { r: un(r), g: un(g), b: un(b) };
                blend_into(&mut self.buf[py as usize * self.w + px as usize], c, a32 * opacity / 255);
            }
        }
    }

    /// Trait anticrénelé à bouts et jointures ronds le long de `pts` (coordonnées et rayon en
    /// seizièmes de pixel). La couverture d'un pixel est celle du segment le plus proche :
    /// les jointures ne sont pas peintes deux fois.
    pub fn stroke(&mut self, pts: &[(i64, i64)], r16: i64, c: Rgb, a: u32) {
        if pts.is_empty() || a == 0 {
            return;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (i64::MAX, i64::MAX, i64::MIN, i64::MIN);
        for &(x, y) in pts {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
        let pad = r16 + 16;
        let (px0, py0) = (((x0 - pad) >> 4).max(0), ((y0 - pad) >> 4).max(0));
        let (px1, py1) = (((x1 + pad) >> 4).min(self.w as i64 - 1), ((y1 + pad) >> 4).min(self.h as i64 - 1));
        for py in py0..=py1 {
            for px in px0..=px1 {
                let (sx, sy) = (px * 16 + 8, py * 16 + 8);
                let mut d2 = i64::MAX;
                if pts.len() == 1 {
                    let (dx, dy) = (sx - pts[0].0, sy - pts[0].1);
                    d2 = dx * dx + dy * dy;
                }
                for s in pts.windows(2) {
                    d2 = d2.min(seg_dist2((sx, sy), s[0], s[1]));
                }
                let cov = (r16 + 8 - isqrt(d2)).clamp(0, 16) as u32;
                if cov > 0 {
                    blend_into(&mut self.buf[py as usize * self.w + px as usize], c, a * cov / 16);
                }
            }
        }
    }

    /// Anneau anticrénelé (centre, rayon et demi-épaisseur en seizièmes de pixel) ; `skip`
    /// reçoit `(dx, dy)` depuis le centre et coupe l'anneau là où il rend `true`.
    pub fn ring(&mut self, cx: i64, cy: i64, r16: i64, hw16: i64, c: Rgb, a: u32, skip: impl Fn(i64, i64) -> bool) {
        let pad = r16 + hw16 + 16;
        let (px0, py0) = (((cx - pad) >> 4).max(0), ((cy - pad) >> 4).max(0));
        let (px1, py1) = (((cx + pad) >> 4).min(self.w as i64 - 1), ((cy + pad) >> 4).min(self.h as i64 - 1));
        for py in py0..=py1 {
            for px in px0..=px1 {
                let (dx, dy) = (px * 16 + 8 - cx, py * 16 + 8 - cy);
                if skip(dx, dy) {
                    continue;
                }
                let d = isqrt(dx * dx + dy * dy);
                let cov = (hw16 + 8 - (d - r16).abs()).clamp(0, 16) as u32;
                if cov > 0 {
                    blend_into(&mut self.buf[py as usize * self.w + px as usize], c, a * cov / 16);
                }
            }
        }
    }
    /// Mélange un pixel avec l'opacité `a` (0 = invisible, 255 = opaque).
    #[inline]
    pub fn put(&mut self, x: isize, y: isize, c: Rgb, a: u32) {
        if a == 0 || x < 0 || y < 0 || x >= self.w as isize || y >= self.h as isize {
            return;
        }
        blend_into(&mut self.buf[y as usize * self.w + x as usize], c, a);
    }

    /// Rectangle plein semi-transparent.
    pub fn fill(&mut self, x: isize, y: isize, w: isize, h: isize, c: Rgb, a: u32) {
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w).min(self.w as isize);
        let y1 = (y + h).min(self.h as isize);
        if a == 0 || x0 >= x1 || y0 >= y1 {
            return;
        }
        for py in y0..y1 {
            let row = py as usize * self.w;
            for p in &mut self.buf[row + x0 as usize..row + x1 as usize] {
                blend_into(p, c, a);
            }
        }
    }

    /// Rectangle arrondi anticrénelé dont la couleur dépend de la ligne (dégradés).
    fn rrect_with<F: Fn(isize) -> Rgb>(
        &mut self, x: isize, y: isize, w: isize, h: isize, r: isize, a: u32, color_at: F,
    ) {
        if w <= 0 || h <= 0 || a == 0 {
            return;
        }
        let r = r.min(w / 2).min(h / 2).max(0);
        let y0 = y.max(0);
        let y1 = (y + h).min(self.h as isize);
        let x0 = x.max(0);
        let x1 = (x + w).min(self.w as isize);
        for py in y0..y1 {
            let c = color_at(py - y);
            let corner_row = py < y + r || py >= y + h - r;
            for px in x0..x1 {
                let cov = if corner_row { rr_cov(px, py, x, y, w, h, r) } else { 16 };
                if cov > 0 {
                    blend_into(&mut self.buf[py as usize * self.w + px as usize], c, a * cov / 16);
                }
            }
        }
    }

    /// Rectangle arrondi plein.
    pub fn rrect(&mut self, x: isize, y: isize, w: isize, h: isize, r: isize, c: Rgb, a: u32) {
        self.rrect_with(x, y, w, h, r, a, |_| c);
    }

    /// Rectangle arrondi avec dégradé vertical.
    pub fn rrect_grad(
        &mut self, x: isize, y: isize, w: isize, h: isize, r: isize, top: Rgb, bottom: Rgb, a: u32,
    ) {
        let hh = h.max(1) as u32;
        self.rrect_with(x, y, w, h, r, a, |row| top.mix(bottom, row.max(0) as u32 * 256 / hh));
    }

    /// Contour (1 px) d'un rectangle arrondi.
    pub fn rrect_outline(&mut self, x: isize, y: isize, w: isize, h: isize, r: isize, c: Rgb, a: u32) {
        if w <= 2 || h <= 2 {
            return;
        }
        let r = r.min(w / 2).min(h / 2).max(0);
        let ri = (r - 1).max(0);
        for py in y.max(0)..(y + h).min(self.h as isize) {
            for px in x.max(0)..(x + w).min(self.w as isize) {
                // L'intérieur strict n'est jamais touché : on saute vite.
                if py > y + r && py < y + h - 1 - r && px > x && px < x + w - 1 {
                    continue;
                }
                let outer = rr_cov(px, py, x, y, w, h, r);
                let inner = rr_cov(px, py, x + 1, y + 1, w - 2, h - 2, ri);
                let cov = outer.saturating_sub(inner);
                if cov > 0 {
                    blend_into(&mut self.buf[py as usize * self.w + px as usize], c, a * cov / 16);
                }
            }
        }
    }

    /// Ombre portée douce sous un rectangle arrondi.
    pub fn shadow(&mut self, x: isize, y: isize, w: isize, h: isize, r: isize, a: u32) {
        const LAYERS: isize = 6;
        let total: u32 = (1..=LAYERS as u32).sum();
        for i in (1..=LAYERS).rev() {
            let la = a * (LAYERS - i + 1) as u32 / total;
            self.rrect(x - i, y - i + 3, w + 2 * i, h + 2 * i, r + i, BLACK, la);
        }
    }

    /// Texte bitmap avec facteur d'échelle et espacement entre lettres.
    pub fn text_ex(
        &mut self, x: isize, y: isize, s: &str, scale: isize, spacing: isize, c: Rgb, a: u32,
    ) {
        let mut cx = x;
        for ch in s.chars() {
            let bitmap = &font::FONT_BASIC[glyph_index(ch)];
            for (row, bits) in bitmap.iter().enumerate() {
                for col in 0..8isize {
                    if bits & (0x80 >> col) != 0 {
                        let px = cx + col * scale;
                        let py = y + row as isize * scale;
                        if scale == 1 {
                            self.put(px, py, c, a);
                        } else {
                            self.fill(px, py, scale, scale, c, a);
                        }
                    }
                }
            }
            cx += CHAR_W * scale + spacing;
        }
    }

    pub fn text(&mut self, x: isize, y: isize, s: &str, c: Rgb) {
        self.text_ex(x, y, s, 1, 0, c, 255);
    }

    /// Texte avec une ombre discrète (lisible sur le fond d'écran).
    pub fn text_shadow(&mut self, x: isize, y: isize, s: &str, c: Rgb) {
        self.text_ex(x + 1, y + 1, s, 1, 0, BLACK, 160);
        self.text_ex(x, y, s, 1, 0, c, 255);
    }

    // ------------------------------------------------------------
    // Grande horloge : chiffres vectoriels à segments arrondis
    // ------------------------------------------------------------

    /// Dessine un chiffre (0-9) à segments arrondis.
    pub fn digit(&mut self, x: isize, y: isize, d: u8, st: SegStyle, c: Rgb, a: u32) {
        // bits : a b c d e f g (haut, haut-droit, bas-droit, bas, bas-gauche, haut-gauche, milieu)
        const SEGS: [u8; 10] = [0x3F, 0x06, 0x5B, 0x4F, 0x66, 0x6D, 0x7D, 0x07, 0x7F, 0x6F];
        let bits = SEGS[(d % 10) as usize];
        let SegStyle { w, h, t } = st;
        let g = (t / 3).max(1);
        let r = t / 2;
        let hx = x + r + g;
        let hw = w - t - 2 * g;
        let up0 = y + r + g;
        let up1 = y + h / 2 - g;
        let lo0 = y + h / 2 + g;
        let lo1 = y + h - r - g;
        let segs: [(isize, isize, isize, isize); 7] = [
            (hx, y, hw, t),                        // a
            (x + w - t, up0, t, up1 - up0),        // b
            (x + w - t, lo0, t, lo1 - lo0),        // c
            (hx, y + h - t, hw, t),                // d
            (x, lo0, t, lo1 - lo0),                // e
            (x, up0, t, up1 - up0),                // f
            (hx, y + h / 2 - t / 2, hw, t),        // g
        ];
        for (i, &(sx, sy, sw, sh)) in segs.iter().enumerate() {
            if bits & (1 << i) != 0 {
                self.rrect(sx, sy, sw, sh, r, c, a);
            }
        }
    }

    /// Dessine "HH:MM" ; retourne la largeur occupée.
    pub fn clock_digits(&mut self, x: isize, y: isize, hh: u8, mm: u8, st: SegStyle, c: Rgb, a: u32) -> isize {
        let gap = st.w / 3;
        let colon_gap = st.w / 3 + 2;
        let mut cx = x;
        self.digit(cx, y, hh / 10, st, c, a);
        cx += st.w + gap;
        self.digit(cx, y, hh % 10, st, c, a);
        cx += st.w + colon_gap;
        let dot = st.t;
        self.rrect(cx, y + st.h * 3 / 10 - dot / 2, dot, dot, dot / 2, c, a);
        self.rrect(cx, y + st.h * 7 / 10 - dot / 2, dot, dot, dot / 2, c, a);
        cx += dot + colon_gap;
        self.digit(cx, y, mm / 10, st, c, a);
        cx += st.w + gap;
        self.digit(cx, y, mm % 10, st, c, a);
        cx + st.w - x
    }

    // ------------------------------------------------------------
    // Pictogrammes d'applications (tuile de 48 px)
    // ------------------------------------------------------------

    pub fn glyph(&mut self, kind: Glyph, x: isize, y: isize, size: isize, tint: Rgb) {
        match kind {
            Glyph::Terminal => {
                // Écran sombre + invite « >_ »
                self.rrect(x + 8, y + 11, size - 16, size - 22, 5, BLACK, 120);
                self.rrect_outline(x + 8, y + 11, size - 16, size - 22, 5, WHITE, 40);
                let (px, py) = (x + 14, y + 17);
                for i in 0..5 {
                    self.fill(px + i, py + i, 3, 2, WHITE, 240);
                    self.fill(px + i, py + 9 - i, 3, 2, WHITE, 240);
                }
                self.fill(px + 11, py + 9, 9, 2, rgb(0x9ECE6A), 255);
            }
            Glyph::Monitor => {
                // Histogramme d'activité
                let heights = [12, 22, 15, 27];
                let (bw, gap) = (5, 3);
                let total = 4 * bw + 3 * gap;
                let bx = x + (size - total) / 2;
                let base = y + size - 11;
                for (i, &bh) in heights.iter().enumerate() {
                    let bxi = bx + i as isize * (bw + gap);
                    self.rrect(bxi, base - bh, bw, bh, 2, WHITE, 235);
                }
                self.fill(x + 9, base + 2, size - 18, 2, WHITE, 120);
            }
            Glyph::Folder => {
                // Dossier : onglet + corps + rabat
                self.rrect(x + 10, y + 13, 14, 8, 3, WHITE, 190);
                self.rrect(x + 10, y + 16, size - 20, size - 28, 4, WHITE, 245);
                self.rrect(x + 10, y + 21, size - 20, size - 33, 4, tint.lighten(150), 255);
                self.fill(x + 12, y + 21, size - 24, 1, WHITE, 200);
            }
            Glyph::Globe => {
                // Disque cerclé, équateur, parallèles et méridiens
                let d = size - 20;
                let (gx, gy) = (x + 10, y + 10);
                self.rrect(gx, gy, d, d, d / 2, WHITE, 240);
                self.rrect(gx + 2, gy + 2, d - 4, d - 4, (d - 4) / 2, tint, 255);
                self.rrect_outline(gx + d / 4, gy + 2, d / 2, d - 4, d / 4, WHITE, 230);
                self.fill(gx + d / 2 - 1, gy + 2, 2, d - 4, WHITE, 230);
                self.fill(gx + 2, gy + d / 2 - 1, d - 4, 2, WHITE, 230);
                self.fill(gx + 5, gy + d / 4, d - 10, 1, WHITE, 200);
                self.fill(gx + 5, gy + 3 * d / 4, d - 10, 1, WHITE, 200);
            }
            Glyph::Letter(ch) => {
                // Les majuscules occupent les lignes 2..12 de la police :
                // on centre la partie visible (30 px à l'échelle 3).
                let lx = x + (size - 8 * 3) / 2;
                let ly = y + (size - 30) / 2 - 6;
                let mut buf = [0u8; 4];
                let s = ch.encode_utf8(&mut buf);
                self.text_ex(lx + 1, ly + 2, s, 3, 0, BLACK, 70);
                self.text_ex(lx, ly, s, 3, 0, WHITE, 245);
            }
        }
    }
}

/// Carré de la distance de `p` au segment `[a, b]`.
fn seg_dist2(p: (i64, i64), a: (i64, i64), b: (i64, i64)) -> i64 {
    let (vx, vy) = (b.0 - a.0, b.1 - a.1);
    let (wx, wy) = (p.0 - a.0, p.1 - a.1);
    let l = vx * vx + vy * vy;
    let (cx, cy) = if l == 0 {
        a
    } else {
        let t = (wx * vx + wy * vy).clamp(0, l);
        (a.0 + vx * t / l, a.1 + vy * t / l)
    };
    let (dx, dy) = (p.0 - cx, p.1 - cy);
    dx * dx + dy * dy
}

// ================================================================
// LOGO ET PICTOGRAMMES VECTORIELS
// ================================================================

/// Le « M » de MaiOS : quatre traits ronds d'une seule ligne brisée et une petite étincelle,
/// dans une boîte de `size` pixels dont le coin est `(x, y)`. Dessiné en vectoriel (net à
/// toute taille), avec une ombre douce.
pub fn logo(c: &mut Canvas, x: isize, y: isize, size: isize, ink: Rgb) {
    // Plan de 36 unités, coordonnées en seizièmes de pixel.
    let k = |u: i64| u * size as i64 * 16 / 360;
    let at = |ux: i64, uy: i64| (x as i64 * 16 + k(ux), y as i64 * 16 + k(uy));
    let m = [at(105, 255), at(105, 115), at(180, 200), at(255, 115), at(255, 255)];
    let r = k(23);
    let shadow: [(i64, i64); 5] = core::array::from_fn(|i| (m[i].0 + k(6), m[i].1 + k(10)));
    c.stroke(&shadow, r + 4, BLACK, 70);
    c.stroke(&m, r, ink, 255);
    // Étincelle à quatre branches en haut à droite.
    let (sx, sy) = at(296, 70);
    let l = k(30);
    c.stroke(&[(sx, sy - l), (sx, sy + l)], k(8), ink, 235);
    c.stroke(&[(sx - l, sy), (sx + l, sy)], k(8), ink, 235);
}

/// Symbole d'alimentation (cercle ouvert en haut et barre) dans une boîte de `size` pixels.
pub fn power_icon(c: &mut Canvas, x: isize, y: isize, size: isize, ink: Rgb, a: u32) {
    let s = size as i64 * 16;
    let (cx, cy) = (x as i64 * 16 + s / 2, y as i64 * 16 + s / 2 + s / 24);
    let r = s * 30 / 100;
    let hw = (s / 22).max(12);
    c.ring(cx, cy, r, hw, ink, a, |dx, dy| dy < 0 && dx.abs() * 100 < -dy * 62);
    c.stroke(&[(cx, cy - r - s / 12), (cx, cy - r / 6)], hw, ink, a);
}

/// Symbole de redémarrage (cercle ouvert en haut à droite, flèche) dans une boîte de `size`.
pub fn restart_icon(c: &mut Canvas, x: isize, y: isize, size: isize, ink: Rgb, a: u32) {
    let s = size as i64 * 16;
    let (cx, cy) = (x as i64 * 16 + s / 2, y as i64 * 16 + s / 2);
    let r = s * 30 / 100;
    let hw = (s / 22).max(12);
    c.ring(cx, cy, r, hw, ink, a, |dx, dy| dy < 0 && dx > 0 && dx * 10 < -dy * 17);
    let (tx, ty) = (cx, cy - r);
    let h = s * 11 / 100;
    c.stroke(&[(tx - h, ty - h), (tx, ty), (tx - h, ty + h)], hw, ink, a);
}

/// Loupe (champ de recherche) dans une boîte de `size` pixels.
pub fn search_icon(c: &mut Canvas, x: isize, y: isize, size: isize, ink: Rgb, a: u32) {
    let s = size as i64 * 16;
    let (cx, cy) = (x as i64 * 16 + s * 42 / 100, y as i64 * 16 + s * 42 / 100);
    let r = s * 26 / 100;
    let hw = (s / 20).max(12);
    c.ring(cx, cy, r, hw, ink, a, |_, _| false);
    let d = r * 72 / 100;
    c.stroke(&[(cx + d + hw, cy + d + hw), (x as i64 * 16 + s * 88 / 100, y as i64 * 16 + s * 88 / 100)], hw + 4, ink, a);
}

/// Croix de fermeture dans une boîte de `size` pixels.
pub fn close_icon(c: &mut Canvas, x: isize, y: isize, size: isize, ink: Rgb, a: u32) {
    let s = size as i64 * 16;
    let (x0, y0) = (x as i64 * 16 + s * 30 / 100, y as i64 * 16 + s * 30 / 100);
    let (x1, y1) = (x as i64 * 16 + s * 70 / 100, y as i64 * 16 + s * 70 / 100);
    let hw = (s / 18).max(10);
    c.stroke(&[(x0, y0), (x1, y1)], hw, ink, a);
    c.stroke(&[(x0, y1), (x1, y0)], hw, ink, a);
}

/// Couverture (0..=16) d'un pixel par un rectangle arrondi (anticrénelage).
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

/// Dimensions des chiffres à segments.
#[derive(Clone, Copy)]
pub struct SegStyle {
    pub w: isize,
    pub h: isize,
    pub t: isize,
}

/// Pictogramme dessiné sur une tuile d'application.
#[derive(Clone, Copy)]
pub enum Glyph {
    Terminal,
    Monitor,
    Folder,
    /// Globe (navigateur).
    Globe,
    Letter(char),
}

// ================================================================
// FOND D'ÉCRAN PROCÉDURAL
// ================================================================

/// Couleur en virgule fixe (×256) pour un dégradé sans perte de précision.
type Fx = [i64; 3];

fn fx(c: Rgb) -> Fx {
    [(c.r as i64) << 8, (c.g as i64) << 8, (c.b as i64) << 8]
}

/// Mélange `a` vers `b`, `t` dans 0..=1024.
#[inline]
fn fx_mix(a: Fx, b: Fx, t: i64) -> Fx {
    [
        a[0] + (b[0] - a[0]) * t / 1024,
        a[1] + (b[1] - a[1]) * t / 1024,
        a[2] + (b[2] - a[2]) * t / 1024,
    ]
}

struct Glow {
    cx: i64,
    cy: i64,
    rx: i64,
    ry: i64,
    color: Fx,
    strength: i64, // 0..1024
}

/// Matrice de Bayer 4×4 pour un tramage ordonné (supprime les bandes du dégradé).
const BAYER: [[i64; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];

/// Génère le fond d'écran « aurore Tokyo Night » à la résolution donnée.
pub fn wallpaper(w: usize, h: usize) -> Vec<AlphaPixel> {
    let mut out = alloc::vec![AlphaPixel::default(); w * h];
    if w == 0 || h == 0 {
        return out;
    }
    let (wi, hi) = (w as i64, h as i64);

    // -- Dégradé du ciel : nuit profonde → crépuscule violet --
    let sky_top = fx(rgb(0x0B0C16));
    let sky_mid = fx(rgb(0x16172A));
    let sky_bot = fx(rgb(0x2B2142));

    // -- Halos colorés --
    let glows = [
        Glow { cx: wi * 18 / 100, cy: hi * 20 / 100, rx: wi * 48 / 100, ry: hi * 58 / 100,
               color: fx(rgb(0x7AA2F7)), strength: 230 },
        Glow { cx: wi * 90 / 100, cy: hi * 60 / 100, rx: wi * 50 / 100, ry: hi * 55 / 100,
               color: fx(rgb(0xBB9AF7)), strength: 250 },
        Glow { cx: wi * 55 / 100, cy: hi, rx: wi * 40 / 100, ry: hi * 30 / 100,
               color: fx(rgb(0xF7768E)), strength: 150 },
    ];
    // Précalcul des distances normalisées par colonne / ligne (×1024).
    let gdx: Vec<Vec<i64>> = glows.iter()
        .map(|g| (0..wi).map(|x| { let d = (x - g.cx) * 1024 / g.rx; d * d }).collect())
        .collect();
    let gdy: Vec<Vec<i64>> = glows.iter()
        .map(|g| (0..hi).map(|y| { let d = (y - g.cy) * 1024 / g.ry; d * d }).collect())
        .collect();

    // -- Aurore boréale : un ruban ondulant, par colonne --
    let aurora_c0 = fx(rgb(0x73DACA));
    let aurora_c1 = fx(rgb(0x7AA2F7));
    let aurora_c2 = fx(rgb(0xBB9AF7));
    let mut a_center = Vec::with_capacity(w);
    let mut a_half = Vec::with_capacity(w);
    let mut a_int = Vec::with_capacity(w);
    let mut a_col = Vec::with_capacity(w);
    for x in 0..wi {
        let s1 = isin(x * 4096 / (wi * 13 / 10) + 300);
        let s2 = isin(x * 4096 / (wi * 4 / 10) + 1500);
        let s3 = isin(x * 4096 / (wi * 7 / 10) + 2600);
        a_center.push(hi * 30 / 100 + hi * 7 * s1 / 100 / 1024 + hi * 2 * s2 / 100 / 1024);
        a_half.push((hi * 6 / 100 + hi * 3 * s3 / 100 / 1024).max(8));
        // Intensité : enveloppe lente × fins « rideaux » verticaux.
        let env = 620 + 400 * isin(x * 4096 / (wi * 9 / 10) + 3300) / 1024; // 220..1020
        let rays = 760 + 264 * isin(x * 4096 / 37) / 1024 + 120 * isin(x * 4096 / 13 + 700) / 1024;
        a_int.push((env * rays / 1024).clamp(0, 1024) * 300 / 1024);
        let t = x * 2048 / wi.max(1);
        a_col.push(if t < 1024 { fx_mix(aurora_c0, aurora_c1, t) } else { fx_mix(aurora_c1, aurora_c2, t - 1024) });
    }

    // -- Collines en couches (×16 pour l'anticrénelage) --
    struct Layer { base: Fx, rim: Fx, rim_strength: i64, curve: Vec<i64> }
    let layer_defs: [(i64, i64, i64, i64, u32, u32, i64); 3] = [
        // base%, amplitude‰, période(%w), phase, couleur, liseré, force du liseré
        (64, 55, 70, 400, 0x24264A, 0xBB9AF7, 260),
        (72, 45, 55, 2200, 0x1B1C34, 0x7AA2F7, 240),
        (80, 35, 85, 1100, 0x111220, 0x7DCFFF, 200),
    ];
    let layers: Vec<Layer> = layer_defs.iter().map(|&(bp, amp, per, ph, col, rim, rs)| {
        let curve = (0..wi).map(|x| {
            let s = isin(x * 4096 / (wi * per / 100).max(1) + ph);
            let s2 = isin(x * 4096 / (wi * per / 260).max(1) + ph * 3);
            (hi * bp * 16) / 100 + (hi * amp * 16) * (s * 2 + s2) / 3 / 1024 / 1000
        }).collect();
        Layer { base: fx(rgb(col)), rim: fx(rgb(rim)), rim_strength: rs, curve }
    }).collect();

    let black = fx(BLACK);
    let half_w = (wi / 2).max(1);
    let half_h = (hi / 2).max(1);

    for y in 0..hi {
        // Dégradé vertical en deux segments.
        let gy = y * 1024 / hi;
        let sky = if gy < 560 {
            fx_mix(sky_top, sky_mid, gy * 1024 / 560)
        } else {
            fx_mix(sky_mid, sky_bot, (gy - 560) * 1024 / 464)
        };
        let ny = (y - half_h) * 1024 / half_h;
        let row = y as usize * w;

        for x in 0..wi {
            let mut c = sky;
            let xu = x as usize;

            // Halos
            for (gi, g) in glows.iter().enumerate() {
                let d2 = gdx[gi][xu] + gdy[gi][y as usize];
                if d2 < 1024 * 1024 {
                    let t = 1024 - d2 / 1024;
                    let f = t * t / 1024 * g.strength / 1024;
                    c = fx_mix(c, g.color, f);
                }
            }

            // Aurore : fondu long au-dessus du ruban, court en dessous.
            let d = y - a_center[xu];
            let range = if d < 0 { a_half[xu] * 2 } else { a_half[xu] / 2 + 2 };
            if d.abs() < range {
                let t = 1024 - d * d * 1024 / (range * range);
                let f = t * t / 1024 * a_int[xu] / 1024;
                c = fx_mix(c, a_col[xu], f);
            }

            // Collines (de l'arrière vers l'avant)
            let py16 = y * 16;
            for l in &layers {
                let edge = l.curve[xu];
                let cov = (py16 + 16 - edge).clamp(0, 16);
                if cov == 0 {
                    continue;
                }
                let depth = (py16 - edge).max(0) / 16;
                let body = fx_mix(l.base, black, (depth * 3).min(300));
                c = fx_mix(c, body, cov * 64);
                if depth < 4 {
                    let rf = (4 - depth) * l.rim_strength / 4 * cov / 16;
                    c = fx_mix(c, l.rim, rf);
                }
            }

            // Vignettage
            let nx = (x - half_w) * 1024 / half_w;
            let v = (nx * nx + ny * ny) / 1024; // 0..2048
            let vf = v.min(2048) * v.min(2048) * 150 / (2048 * 2048);
            c = fx_mix(c, black, vf);

            // Tramage + écriture
            let th = BAYER[(y & 3) as usize][(x & 3) as usize] * 16;
            let q = |v: i64| ((v + th) >> 8).clamp(0, 255) as u8;
            out[row + xu] = AlphaPixel { alpha: 0, red: q(c[0]), green: q(c[1]), blue: q(c[2]) };
        }
    }

    // -- Étoiles (seulement dans le ciel) --
    {
        let horizon: Vec<i64> = layers[0].curve.iter().map(|e| e / 16 - 6).collect();
        let mut canvas = Canvas::new(&mut out, w, h);
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let star = rgb(0xE6ECFF);
        let count = (w * h / 1700) as u64;
        for i in 0..count {
            let x = rng.below(wi as u64) as i64;
            let y = rng.below((hi * 62 / 100) as u64) as i64;
            if y >= horizon[x as usize] {
                continue;
            }
            // Les étoiles s'estompent vers l'horizon.
            let fade = 1024 - (y * 1024 / horizon[x as usize].max(1)) * 6 / 10;
            let b = (40 + rng.below(170) as i64) * fade / 1024;
            let (xs, ys) = (x as isize, y as isize);
            if i % 14 == 0 {
                // Étoile brillante en croix
                canvas.put(xs, ys, star, 255);
                for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
                    canvas.put(xs + dx, ys + dy, star, (b / 2) as u32);
                }
                for (dx, dy) in [(-2, 0), (2, 0), (0, -2), (0, 2)] {
                    canvas.put(xs + dx, ys + dy, star, (b / 6) as u32);
                }
            } else {
                canvas.put(xs, ys, star, b as u32);
            }
        }
    }

    out
}
