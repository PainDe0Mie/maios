//! Pixels des tampons des clients vers la convention de MaiCompositor.
//!
//! Même disposition mémoire (BGRA little-endian, `0xAARRGGBB`), mais :
//! * Wayland : alpha `255` = opaque, couleurs **prémultipliées** par l'alpha ;
//! * MaiOS : alpha `0` = opaque, `255` = invisible, couleurs non prémultipliées.
//!
//! `XRGB8888` : l'octet d'alpha est sans signification, on le met à `0` (opaque). `ARGB8888` :
//! les pixels opaques (le cas courant) ne coûtent qu'un masque ; seuls les pixels partiellement
//! transparents (ombres, coins anticrénelés) sont dé-prémultipliés.
//!
//! Deux chemins :
//! * **direct** : un pixel de surface est un pixel du tampon ([`View::is_identity`]) : copie
//!   ligne à ligne depuis le pool, puis conversion en place ;
//! * **échantillonné** (transformation, échelle, `wp_viewport`) : filtre bilinéaire en virgule
//!   fixe dans l'espace prémultiplié (le seul où l'interpolation de l'alpha est juste), sur une
//!   tuile du tampon lue d'un coup par bande de lignes de destination. Les cas exacts (rotation,
//!   recadrage aux pixels près) tombent sur des centres de pixels : poids nuls, pixels inchangés.

use alloc::vec;
use alloc::vec::Vec;
use framebuffer::AlphaPixel;
use region2d::{Rect, Region};

use crate::shm::Buffer;
use crate::view::{floor_px, View, ONE};

/// Lignes de destination traitées par bande (une tuile du tampon lue par bande).
const STRIP: i32 = 16;
/// Pixels au plus dans une tuile (4 Mio) ; au-delà, la bande est coupée, puis lue pixel à pixel.
const MAX_TILE: usize = 1 << 20;

/// Recopie les rectangles de `damage` (coordonnées de la surface, déjà bornés à `geom`) vers
/// `px` (`size` = largeur × hauteur) : le point `(geom.x0, geom.y0)` de la surface va en
/// `origin`. Rend le rectangle englobant copié (coordonnées de la surface) et le nombre de
/// pixels écrits.
pub(crate) fn blit(buf: &Buffer, view: &View, damage: &Region, geom: Rect, px: &mut [AlphaPixel], size: (usize, usize), origin: (usize, usize)) -> (Rect, u64) {
    let mut copied = 0u64;
    let mut bounds = Rect::EMPTY;
    let mut scratch = Vec::new();
    for r in damage.iter() {
        let r = r.intersect(&geom);
        if r.is_empty() {
            continue;
        }
        let dst = Dst { px: &mut *px, size, origin, geom };
        if let Some(c) = buf.solid() {
            fill(dst, &r, c);
        } else if view.is_identity() {
            copy_direct(buf, dst, &r);
        } else {
            sample(buf, view, dst, &r, &mut scratch);
        }
        copied += r.area();
        bounds = if bounds.is_empty() { r } else { bounds.hull(&r) };
    }
    (bounds, copied)
}

/// Destination : tampon d'une fenêtre (ou d'un curseur) et place de la surface dedans.
struct Dst<'a> {
    px: &'a mut [AlphaPixel],
    size: (usize, usize),
    origin: (usize, usize),
    geom: Rect,
}

impl Dst<'_> {
    /// Ligne `y` de la surface, colonnes `[x0, x0 + w)`, en octets ; `None` hors du tampon.
    fn row(&mut self, x0: i32, y: i32, w: usize) -> Option<&mut [u8]> {
        let ((fw, fh), (ox, oy)) = (self.size, self.origin);
        let dx = ox + (x0 - self.geom.x0) as usize;
        let dy = oy + (y - self.geom.y0) as usize;
        let d = dy * fw + dx;
        if dy >= fh || dx + w > fw || d + w > self.px.len() {
            return None;
        }
        let row = &mut self.px[d..d + w];
        // SAFETY : `AlphaPixel` est `repr(C)`, quatre octets sans remplissage.
        Some(unsafe { core::slice::from_raw_parts_mut(row.as_mut_ptr() as *mut u8, w * 4) })
    }
}

/// Tampon uni (`single-pixel-buffer`), à toute échelle : une seule couleur.
fn fill(mut dst: Dst<'_>, r: &Rect, c: [u8; 4]) {
    let mut one = c;
    convert_row(&mut one, c[3] == 255);
    let w = r.width() as usize;
    for y in r.y0..r.y1 {
        let Some(row) = dst.row(r.x0, y, w) else { break };
        for p in row.chunks_exact_mut(4) {
            p.copy_from_slice(&one);
        }
    }
}

/// Pixel de surface = pixel du tampon : copie ligne à ligne.
fn copy_direct(buf: &Buffer, mut dst: Dst<'_>, r: &Rect) {
    let w = r.width() as usize;
    for y in r.y0..r.y1 {
        let Some(row) = dst.row(r.x0, y, w) else { break };
        buf.read_row(r.x0 as usize, y as usize, row);
        convert_row(row, buf.opaque_format());
    }
}

/// Lit `buf` sur `area` (pixels du tampon) en mots ARGB prémultipliés (alpha forcé à 255
/// pour `XRGB8888`).
fn fetch(buf: &Buffer, area: &Rect, out: &mut Vec<u32>) {
    let (w, h) = (area.width() as usize, area.height() as usize);
    out.clear();
    out.resize(w * h, 0);
    let opaque = buf.opaque_format();
    for (i, y) in (area.y0..area.y1).enumerate() {
        let row = &mut out[i * w..(i + 1) * w];
        // SAFETY : `u32` sur quatre octets, alignement plus fort que celui des octets.
        let bytes = unsafe { core::slice::from_raw_parts_mut(row.as_mut_ptr() as *mut u8, w * 4) };
        buf.read_row(area.x0 as usize, y as usize, bytes);
        if opaque {
            for p in row.iter_mut() {
                *p |= 0xFF00_0000;
            }
        }
    }
}

/// Un pixel du tampon (lecture isolée, quand la tuile serait trop grande).
fn fetch_one(buf: &Buffer, x: i32, y: i32) -> u32 {
    let mut b = [0u8; 4];
    buf.read_row(x as usize, y as usize, &mut b);
    let v = u32::from_le_bytes(b);
    if buf.opaque_format() { v | 0xFF00_0000 } else { v }
}

/// Moyenne pondérée de deux pixels prémultipliés (`f` sur 0..=256, poids de `b`), en 16 bits
/// par canal (précision gardée pour la seconde passe).
#[inline(always)]
fn lerp_wide(a: u32, b: u32, f: u32) -> [u32; 4] {
    let g = 256 - f;
    let ch = |s: u32| ((a >> s) & 0xFF) * g + ((b >> s) & 0xFF) * f;
    [ch(0), ch(8), ch(16), ch(24)]
}

/// Filtre bilinéaire : `p00 p10 / p01 p11`, poids `fx`, `fy` sur 0..=255.
#[inline(always)]
fn bilinear(p00: u32, p10: u32, p01: u32, p11: u32, fx: u32, fy: u32) -> u32 {
    if fx == 0 && fy == 0 {
        return p00;
    }
    let top = lerp_wide(p00, p10, fx);
    let bot = lerp_wide(p01, p11, fx);
    let g = 256 - fy;
    let mut out = 0u32;
    for (i, s) in [0u32, 8, 16, 24].iter().enumerate() {
        let v = (top[i] * g + bot[i] * fy + 32768) >> 16;
        out |= v.min(255) << s;
    }
    out
}

/// Chemin échantillonné vers une fenêtre (pixels convertis).
fn sample(buf: &Buffer, view: &View, mut dst: Dst<'_>, r: &Rect, scratch: &mut Vec<u32>) {
    let w = r.width() as usize;
    sample_rows(buf, view, r, scratch, &mut |v, line| {
        if let Some(row) = dst.row(r.x0, v, w) {
            for (d, s) in row.chunks_exact_mut(4).zip(line.iter()) {
                d.copy_from_slice(&s.to_le_bytes());
            }
            convert_row(row, false);
        }
    });
}

/// Une couche d'un arbre de surfaces : tampon, correspondance, coin dans la racine.
pub(crate) struct Layer {
    pub buf: alloc::rc::Rc<Buffer>,
    pub view: View,
    pub x: i32,
    pub y: i32,
}

/// Lignes de la racine composées par passe (borne la mémoire de travail).
const BAND: i32 = 64;

/// Compose les couches (de la plus basse à la plus haute, `src over` prémultiplié) sur les
/// rectangles de `damage` (coordonnées de la racine, bornés à `geom`), puis les écrit
/// converties dans `px`, comme [`blit`]. Rend le nombre de pixels écrits.
pub(crate) fn compose(layers: &[Layer], damage: &Region, geom: Rect, px: &mut [AlphaPixel], size: (usize, usize), origin: (usize, usize)) -> u64 {
    let mut written = 0u64;
    let mut scratch = Vec::new();
    let mut acc: Vec<u32> = Vec::new();
    for r in damage.iter() {
        let r = r.intersect(&geom);
        if r.is_empty() {
            continue;
        }
        let w = r.width() as usize;
        let mut y0 = r.y0;
        while y0 < r.y1 {
            let band = Rect::new(r.x0, y0, r.x1, (y0 + BAND).min(r.y1));
            acc.clear();
            acc.resize(w * band.height() as usize, 0);
            for l in layers {
                let (lw, lh) = l.view.size();
                let lr = Rect::new(l.x, l.y, l.x.saturating_add(lw as i32), l.y.saturating_add(lh as i32)).intersect(&band);
                if lr.is_empty() {
                    continue;
                }
                let local = lr.translate(-l.x, -l.y);
                let dx = (lr.x0 - band.x0) as usize;
                render_rows(&l.buf, &l.view, &local, &mut scratch, &mut |v, line| {
                    let row = (v + l.y - band.y0) as usize * w + dx;
                    over(&mut acc[row..row + line.len()], line);
                });
            }
            let mut dst = Dst { px: &mut *px, size, origin, geom };
            for (i, y) in (band.y0..band.y1).enumerate() {
                let Some(row) = dst.row(band.x0, y, w) else { break };
                for (d, s) in row.chunks_exact_mut(4).zip(acc[i * w..(i + 1) * w].iter()) {
                    d.copy_from_slice(&s.to_le_bytes());
                }
                convert_row(row, false);
            }
            written += band.area();
            y0 = band.y1;
        }
    }
    written
}

/// `dst = src + dst × (1 − αsrc)`, pixels ARGB prémultipliés.
fn over(dst: &mut [u32], src: &[u32]) {
    for (d, &s) in dst.iter_mut().zip(src.iter()) {
        let a = s >> 24;
        if a == 255 {
            *d = s;
        } else if a != 0 {
            let k = 255 - a;
            let mut out = 0u32;
            for sh in [0u32, 8, 16, 24] {
                let v = ((s >> sh) & 0xFF) + (((*d >> sh) & 0xFF) * k + 127) / 255;
                out |= v.min(255) << sh;
            }
            *d = out;
        }
    }
}

/// Lignes de la surface sur `r` (coordonnées de la surface), en ARGB prémultiplié (alpha forcé
/// à 255 pour `XRGB8888`), passées une à une à `sink(y, ligne)`.
fn render_rows(buf: &Buffer, view: &View, r: &Rect, scratch: &mut Vec<u32>, sink: &mut dyn FnMut(i32, &[u32])) {
    let w = r.width() as usize;
    if let Some(c) = buf.solid() {
        let line = vec![u32::from_le_bytes(c); w];
        for y in r.y0..r.y1 {
            sink(y, &line);
        }
    } else if view.is_identity() {
        let mut line = vec![0u32; w];
        let opaque = buf.opaque_format();
        for y in r.y0..r.y1 {
            // SAFETY : `u32` sur quatre octets.
            let bytes = unsafe { core::slice::from_raw_parts_mut(line.as_mut_ptr() as *mut u8, w * 4) };
            buf.read_row(r.x0 as usize, y as usize, bytes);
            if opaque {
                for p in line.iter_mut() {
                    *p |= 0xFF00_0000;
                }
            }
            sink(y, &line);
        }
    } else {
        sample_rows(buf, view, r, scratch, sink);
    }
}

/// Filtre bilinéaire par bandes de [`STRIP`] lignes : chaque ligne de `r` (coordonnées de la
/// surface), en ARGB prémultiplié, va à `sink`.
fn sample_rows(buf: &Buffer, view: &View, r: &Rect, scratch: &mut Vec<u32>, sink: &mut dyn FnMut(i32, &[u32])) {
    let a = view.affine();
    let clamp = view.clamp_box();
    let w = r.width() as usize;
    let mut line = vec![0u32; w];
    let mut y = r.y0;
    let mut strip = STRIP;
    while y < r.y1 {
        let y1 = (y + strip).min(r.y1);
        // Enveloppe dans le tampon des centres de la bande, plus un pixel pour le voisin du
        // filtre, bornée à la zone lisible.
        let corners = [a.at(r.x0, y), a.at(r.x1 - 1, y), a.at(r.x0, y1 - 1), a.at(r.x1 - 1, y1 - 1)];
        let bx0 = corners.iter().map(|c| c.0).min().unwrap_or(0) - ONE / 2;
        let bx1 = corners.iter().map(|c| c.0).max().unwrap_or(0) + ONE / 2;
        let by0 = corners.iter().map(|c| c.1).min().unwrap_or(0) - ONE / 2;
        let by1 = corners.iter().map(|c| c.1).max().unwrap_or(0) + ONE / 2;
        let tile = Rect::new(floor_px(bx0), floor_px(by0), floor_px(bx1) + 2, floor_px(by1) + 2).intersect(&clamp);
        let area = tile.width().max(0) as usize * tile.height().max(0) as usize;
        if area > MAX_TILE && strip > 1 {
            strip = (strip / 2).max(1);
            continue;
        }
        let tiled = area <= MAX_TILE && !tile.is_empty();
        if tiled {
            fetch(buf, &tile, scratch);
        }
        let tw = tile.width().max(1);
        for v in y..y1 {
            let mut p = a.at(r.x0, v);
            for out in line.iter_mut() {
                // Centre du pixel → coin du premier voisin du filtre.
                let (sx, sy) = (p.0 - ONE / 2, p.1 - ONE / 2);
                let (ix, iy) = (floor_px(sx), floor_px(sy));
                let (fx, fy) = (((sx >> 24) & 0xFF) as u32, ((sy >> 24) & 0xFF) as u32);
                let cx = |x: i32| x.clamp(clamp.x0, clamp.x1 - 1);
                let cy = |y: i32| y.clamp(clamp.y0, clamp.y1 - 1);
                let (x0, x1, y0, y1) = (cx(ix), cx(ix + 1), cy(iy), cy(iy + 1));
                let get = |x: i32, y: i32| -> u32 {
                    if tiled && tile.contains_point(x, y) {
                        scratch[((y - tile.y0) * tw + (x - tile.x0)) as usize]
                    } else {
                        fetch_one(buf, x, y)
                    }
                };
                *out = bilinear(get(x0, y0), get(x1, y0), get(x0, y1), get(x1, y1), fx, fy);
                p.0 += a.du.0;
                p.1 += a.du.1;
            }
            sink(v, &line);
        }
        y = y1;
        strip = STRIP;
    }
}

/// Convertit en place une ligne de pixels `wl_shm` (`bytes.len()` multiple de 4).
pub(crate) fn convert_row(bytes: &mut [u8], opaque_format: bool) {
    if opaque_format {
        for px in bytes.chunks_exact_mut(4) {
            px[3] = 0;
        }
        return;
    }
    for px in bytes.chunks_exact_mut(4) {
        let a = px[3] as u32;
        match a {
            255 => px[3] = 0,
            0 => {
                px.copy_from_slice(&[0, 0, 0, 255]);
            }
            _ => {
                for c in &mut px[..3] {
                    *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
                }
                px[3] = (255 - a) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions() {
        let mut row = [10, 20, 30, 255, 1, 2, 3, 0, 64, 32, 16, 128];
        convert_row(&mut row, false);
        assert_eq!(&row[..4], &[10, 20, 30, 0]);
        assert_eq!(&row[4..8], &[0, 0, 0, 255]);
        assert_eq!(&row[8..], &[128, 64, 32, 127]);
        let mut x = [1, 2, 3, 77];
        convert_row(&mut x, true);
        assert_eq!(x, [1, 2, 3, 0]);
    }

    #[test]
    fn filter() {
        let (a, b) = (0xFF00_00FFu32, 0xFF00_FF00u32);
        assert_eq!(bilinear(a, b, a, b, 0, 0), a);
        // À mi-chemin : moitié de chaque composante.
        assert_eq!(bilinear(a, b, a, b, 128, 0), 0xFF00_8080);
        assert_eq!(bilinear(a, a, a, a, 77, 201), a);
    }
}
