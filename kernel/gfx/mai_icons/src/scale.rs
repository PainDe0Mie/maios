//! Mise à l'échelle d'une image prémultipliée, en virgule fixe (aucun flottant).
//!
//! Réduction : moyenne exacte des aires couvertes (filtre « boîte » à couverture fractionnaire,
//! en 1/256 de pixel), ce qui garde les traits fins des icônes sans crénelage. Agrandissement :
//! interpolation bilinéaire, centres de pixels alignés. Prémultiplié : les bords transparents
//! ne bavent pas de couleur.

use alloc::vec::Vec;

use crate::Image;

/// Poids (1/256) des pixels sources couverts par l'intervalle `[a, b)` (unités 1/256).
fn spans(src: usize, dst: usize) -> Vec<Vec<(usize, u32)>> {
    let mut out = Vec::with_capacity(dst);
    for d in 0..dst {
        let a = d * src * 256 / dst;
        let b = (d + 1) * src * 256 / dst;
        let mut v = Vec::new();
        let mut p = a;
        while p < b {
            let i = p / 256;
            let end = ((i + 1) * 256).min(b);
            v.push((i.min(src - 1), (end - p) as u32));
            p = end;
        }
        out.push(v);
    }
    out
}

fn box_filter(img: &Image, w: usize, h: usize) -> Image {
    let xs = spans(img.width, w);
    let ys = spans(img.height, h);
    let mut pixels = Vec::with_capacity(w * h);
    for yspan in &ys {
        for xspan in &xs {
            let mut acc = [0u64; 4];
            let mut total = 0u64;
            for &(sy, wy) in yspan {
                let row = &img.pixels[sy * img.width..(sy + 1) * img.width];
                for &(sx, wx) in xspan {
                    let wgt = (wy * wx) as u64;
                    let p = row[sx];
                    for c in 0..4 {
                        acc[c] += p[c] as u64 * wgt;
                    }
                    total += wgt;
                }
            }
            let t = total.max(1);
            pixels.push([
                ((acc[0] + t / 2) / t) as u8,
                ((acc[1] + t / 2) / t) as u8,
                ((acc[2] + t / 2) / t) as u8,
                ((acc[3] + t / 2) / t) as u8,
            ]);
        }
    }
    Image { width: w, height: h, pixels }
}

fn bilinear(img: &Image, w: usize, h: usize) -> Image {
    let (sw, sh) = (img.width as i64, img.height as i64);
    // Position source du centre du pixel d, en 1/256 : (d + 0,5) × s / dst − 0,5.
    let coord = |d: usize, s: i64, dst: usize| -> (usize, usize, u32) {
        let p = ((2 * d as i64 + 1) * s * 256 / (2 * dst as i64) - 128).clamp(0, (s - 1) * 256);
        let i = (p / 256) as usize;
        let f = (p % 256) as u32;
        (i, (i + 1).min(s as usize - 1), f)
    };
    let mut pixels = Vec::with_capacity(w * h);
    for y in 0..h {
        let (y0, y1, fy) = coord(y, sh, h);
        for x in 0..w {
            let (x0, x1, fx) = coord(x, sw, w);
            let p = |xx: usize, yy: usize| img.pixels[yy * img.width + xx];
            let (a, b, c, d) = (p(x0, y0), p(x1, y0), p(x0, y1), p(x1, y1));
            let mut out = [0u8; 4];
            for k in 0..4 {
                let top = a[k] as u32 * (256 - fx) + b[k] as u32 * fx;
                let bot = c[k] as u32 * (256 - fx) + d[k] as u32 * fx;
                out[k] = ((top * (256 - fy) + bot * fy + (1 << 15)) >> 16) as u8;
            }
            pixels.push(out);
        }
    }
    Image { width: w, height: h, pixels }
}

impl Image {
    /// Image redimensionnée à `w` × `h` exactement.
    pub fn resized(&self, w: usize, h: usize) -> Image {
        if w == 0 || h == 0 || self.width == 0 || self.height == 0 {
            return Image { width: w, height: h, pixels: alloc::vec![[0; 4]; w * h] };
        }
        if w == self.width && h == self.height {
            return self.clone();
        }
        if w <= self.width && h <= self.height {
            box_filter(self, w, h)
        } else if w >= self.width && h >= self.height {
            bilinear(self, w, h)
        } else {
            // Un axe réduit, l'autre agrandi : en deux temps.
            let mid = box_filter(self, w.min(self.width), h.min(self.height));
            bilinear(&mid, w, h)
        }
    }

    /// Image tenant dans un carré de `size` pixels, proportions gardées, centrée.
    pub fn fit_square(&self, size: usize) -> Image {
        if self.width == 0 || self.height == 0 || size == 0 {
            return Image { width: size, height: size, pixels: alloc::vec![[0; 4]; size * size] };
        }
        let (w, h) = if self.width >= self.height {
            (size, (self.height * size / self.width).max(1))
        } else {
            ((self.width * size / self.height).max(1), size)
        };
        let scaled = self.resized(w, h);
        if w == size && h == size {
            return scaled;
        }
        let mut pixels = alloc::vec![[0u8; 4]; size * size];
        let (ox, oy) = ((size - w) / 2, (size - h) / 2);
        for y in 0..h {
            pixels[(oy + y) * size + ox..(oy + y) * size + ox + w].copy_from_slice(&scaled.pixels[y * w..(y + 1) * w]);
        }
        Image { width: size, height: size, pixels }
    }
}
