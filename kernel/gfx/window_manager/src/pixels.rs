//! Routines de pixels du compositeur : copies de lignes, mélange alpha SWAR, contour d'aperçu.
//!
//! Le noyau est compilé sans SSE (`soft-float`) : pas de SIMD vectoriel ici sans sauvegarde de
//! l'état FPU. Le mélange traite donc deux canaux à la fois dans un mot de 32 bits (SWAR,
//! *SIMD within a register*) : rouge et bleu dans les voies 0–15 et 16–31, puis vert. La
//! division par 255 est exacte et sans division (`(x + 128 + ((x + 128) >> 8)) >> 8`, Blinn,
//! *Three Wrongs Make a Right*, IEEE CG&A 1995), ce qui donne le même arrondi que `x / 255`
//! arrondi au plus proche.
//!
//! Convention d'alpha de MaiOS : `0` = opaque, `255` = invisible.

use framebuffer::AlphaPixel;
use region2d::Rect;

#[inline(always)]
fn to_u32(p: AlphaPixel) -> u32 {
    (p.blue as u32) | ((p.green as u32) << 8) | ((p.red as u32) << 16) | ((p.alpha as u32) << 24)
}

#[inline(always)]
fn from_u32(v: u32) -> AlphaPixel {
    AlphaPixel { blue: v as u8, green: (v >> 8) as u8, red: (v >> 16) as u8, alpha: (v >> 24) as u8 }
}

/// `src` (opacité `255 - src.alpha`) par-dessus `dst` ; le résultat est opaque.
#[inline(always)]
pub fn blend(src: AlphaPixel, dst: AlphaPixel) -> AlphaPixel {
    let t = src.alpha as u32; // poids de `dst`
    if t == 0 {
        return AlphaPixel { alpha: 0, ..src };
    }
    if t == 255 {
        return AlphaPixel { alpha: 0, ..dst };
    }
    let o = 255 - t; // poids de `src`
    let (s, d) = (to_u32(src), to_u32(dst));
    let mut rb = (s & 0x00FF_00FF) * o + (d & 0x00FF_00FF) * t + 0x0080_0080;
    rb = ((rb + ((rb >> 8) & 0x00FF_00FF)) >> 8) & 0x00FF_00FF;
    let mut g = ((s >> 8) & 0xFF) * o + ((d >> 8) & 0xFF) * t + 0x80;
    g = ((g + (g >> 8)) >> 8) & 0xFF;
    from_u32(rb | (g << 8))
}

/// Mélange une ligne `src` sur `dst` (même longueur), avec saut des pixels invisibles et
/// copie en bloc des suites opaques.
#[inline]
pub fn blend_row(src: &[AlphaPixel], dst: &mut [AlphaPixel]) {
    let n = src.len().min(dst.len());
    let mut i = 0;
    while i < n {
        let a = src[i].alpha;
        if a == 0 {
            let start = i;
            while i < n && src[i].alpha == 0 {
                i += 1;
            }
            dst[start..i].copy_from_slice(&src[start..i]);
        } else if a == 255 {
            i += 1;
        } else {
            dst[i] = blend(src[i], dst[i]);
            i += 1;
        }
    }
}

/// Copie (ou mélange si `blend`) la partie `r` (coordonnées écran) d'une surface `src` de
/// largeur `src_w`, placée en `(ox, oy)`, dans `dst` de largeur `dst_w`.
///
/// `r` doit être contenu à la fois dans la surface et dans `dst` (l'appelant découpe).
#[inline]
#[allow(clippy::too_many_arguments)]
pub fn put_rect(
    dst: &mut [AlphaPixel],
    dst_w: usize,
    src: &[AlphaPixel],
    src_w: usize,
    ox: i32,
    oy: i32,
    r: &Rect,
    blend: bool,
) {
    if r.is_empty() {
        return;
    }
    let w = r.width() as usize;
    let sx = (r.x0 - ox) as usize;
    for y in r.y0..r.y1 {
        let s = (y - oy) as usize * src_w + sx;
        let d = y as usize * dst_w + r.x0 as usize;
        if s + w > src.len() || d + w > dst.len() {
            break;
        }
        if blend {
            blend_row(&src[s..s + w], &mut dst[d..d + w]);
        } else {
            dst[d..d + w].copy_from_slice(&src[s..s + w]);
        }
    }
}

/// Remplit `r` d'une couleur (fond quand le bureau ne couvre pas tout l'écran).
pub fn fill_rect(dst: &mut [AlphaPixel], dst_w: usize, r: &Rect, px: AlphaPixel) {
    if r.is_empty() {
        return;
    }
    let w = r.width() as usize;
    for y in r.y0..r.y1 {
        let d = y as usize * dst_w + r.x0 as usize;
        if d + w > dst.len() {
            break;
        }
        dst[d..d + w].fill(px);
    }
}

/// Copie `r` du tampon arrière vers le tampon d'affichage (mémoire vidéo en écriture
/// combinée : des lignes longues et contiguës, sans relecture).
#[inline]
pub fn present_rect(front: &mut [AlphaPixel], back: &[AlphaPixel], w: usize, r: &Rect) {
    if r.is_empty() {
        return;
    }
    let n = r.width() as usize;
    for y in r.y0..r.y1 {
        let o = y as usize * w + r.x0 as usize;
        if o + n > back.len() || o + n > front.len() {
            break;
        }
        front[o..o + n].copy_from_slice(&back[o..o + n]);
    }
}

// ── Contour d'aperçu (déplacement / redimensionnement) ───────────────────────

/// Épaisseur du contour.
pub const BORDER_PX: i32 = 2;
/// Même rayon que les coins des fenêtres.
pub const BORDER_RADIUS: i32 = 10;
const BORDER_RGB: (u8, u8, u8) = (0x7A, 0xA2, 0xF7);
const BORDER_OPACITY: u32 = 220;

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

/// Couverture (0..=16) d'un pixel par un rectangle arrondi (anticrénelage 16 niveaux).
fn rr_cov(px: i32, py: i32, x: i32, y: i32, w: i32, h: i32, r: i32) -> u32 {
    if w <= 0 || h <= 0 || px < x || py < y || px >= x + w || py >= y + h {
        return 0;
    }
    let r = r.min(w / 2).min(h / 2).max(0);
    let sx = px * 16 + 8;
    let sy = py * 16 + 8;
    let qx = if px < x + r { (x + r) * 16 - sx } else if px >= x + w - r { sx - (x + w - r) * 16 } else { 0 }.max(0);
    let qy = if py < y + r { (y + r) * 16 - sy } else if py >= y + h - r { sy - (y + h - r) * 16 } else { 0 }.max(0);
    if qx == 0 && qy == 0 {
        return 16;
    }
    let d = if qx == 0 { qy } else if qy == 0 { qx } else { isqrt(qx as i64 * qx as i64 + qy as i64 * qy as i64) as i32 };
    (r * 16 + 8 - d).clamp(0, 16) as u32
}

/// Les quatre bandes qui contiennent le contour de `b` : tout ce qu'il peut toucher.
pub fn border_bands(b: &Rect) -> [Rect; 4] {
    let band = BORDER_RADIUS + BORDER_PX;
    let (x0, y0, x1, y1) = (b.x0, b.y0, b.x1, b.y1);
    let ym0 = (y0 + band).min(y1);
    let ym1 = (y1 - band).max(ym0);
    [
        Rect::new(x0, y0, x1, ym0),
        Rect::new(x0, ym1, x1, y1),
        Rect::new(x0, ym0, (x0 + band).min(x1), ym1),
        Rect::new((x1 - band).max(x0), ym0, x1, ym1),
    ]
}

/// Dessine la partie `clip` du contour de `b` dans `dst` (par-dessus la scène composée).
pub fn draw_border(dst: &mut [AlphaPixel], w: usize, b: &Rect, clip: &Rect) {
    let (x, y, bw, bh) = (b.x0, b.y0, b.width(), b.height());
    let t = BORDER_PX;
    let r = BORDER_RADIUS;
    if bw <= 2 * t || bh <= 2 * t {
        return;
    }
    let (cr, cg, cb) = BORDER_RGB;
    for py in clip.y0..clip.y1 {
        for px in clip.x0..clip.x1 {
            let outer = rr_cov(px, py, x, y, bw, bh, r);
            if outer == 0 {
                continue;
            }
            let inner = rr_cov(px, py, x + t, y + t, bw - 2 * t, bh - 2 * t, (r - t).max(0));
            let cov = outer.saturating_sub(inner);
            if cov == 0 {
                continue;
            }
            let o = py as usize * w + px as usize;
            if o >= dst.len() {
                continue;
            }
            let p = AlphaPixel { alpha: (255 - cov * BORDER_OPACITY / 16) as u8, red: cr, green: cg, blue: cb };
            dst[o] = blend(p, dst[o]);
        }
    }
}
