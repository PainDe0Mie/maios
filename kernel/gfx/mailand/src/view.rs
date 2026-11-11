//! Correspondance entre une surface et son tampon : `wl_surface.set_buffer_transform`,
//! `set_buffer_scale` et `wp_viewport` (recadrage et mise à l'échelle).
//!
//! Le modèle est celui de Weston (`weston_surface_to_buffer_float`) :
//!
//! ```text
//!  point de la surface ──viewport──► point « issu du tampon » ──transformation, × échelle──► tampon
//!  (taille : destination)            (taille : tampon transformé ÷ échelle)
//! ```
//!
//! La correspondance est affine. Elle est calculée en virgule fixe Q32 (1/2³² de pixel), sans
//! flottants (le noyau est compilé sans SSE) : une ligne de destination se parcourt par simples
//! additions. Les cas exacts (rotation pure, échelle entière, recadrage aux pixels près) tombent
//! sur des centres de pixels, si bien que le filtre bilinéaire y rend les pixels tels quels.
//!
//! Module sans dépendance hors de `region2d`, testé sur l'hôte (voir les tests en fin de fichier).

use region2d::Rect;

/// Un pixel en Q32.
pub const ONE: i64 = 1 << 32;

/// Rectangle source de `wp_viewport.set_source`, en `wl_fixed` (1/256 de pixel de surface).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SrcRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Erreurs constatées à l'application de l'état d'une surface.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ViewError {
    /// Source non entière sans destination (`wp_viewport.bad_size`).
    BadSize,
    /// Source qui déborde du tampon (`wp_viewport.out_of_buffer`).
    OutOfBuffer,
    /// Tampon dont la taille n'est pas un multiple de l'échelle (`wl_surface.invalid_size`).
    InvalidSize,
}

/// Tout ce qui relie la surface à son tampon.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct View {
    /// Taille du tampon, en pixels.
    pub buf: (u32, u32),
    /// `wl_output.transform` (0 à 7).
    pub transform: u32,
    /// Échelle du tampon (≥ 1).
    pub scale: u32,
    pub src: Option<SrcRect>,
    pub dst: Option<(u32, u32)>,
}

/// Correspondance affine : point du tampon (Q32) = `origin + u·du + v·dv` pour le **centre** du
/// pixel `(u, v)` de la surface.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Affine {
    pub origin: (i64, i64),
    pub du: (i64, i64),
    pub dv: (i64, i64),
}

impl Affine {
    /// Centre du pixel `(u, v)` de la surface, dans le tampon (Q32).
    #[inline]
    pub fn at(&self, u: i32, v: i32) -> (i64, i64) {
        let (u, v) = (u as i64, v as i64);
        (self.origin.0 + u * self.du.0 + v * self.dv.0, self.origin.1 + u * self.du.1 + v * self.dv.1)
    }
}

/// Transformations qui échangent largeur et hauteur.
fn swaps(transform: u32) -> bool {
    transform & 1 == 1
}

impl View {
    /// Taille du tampon une fois transformé, en pixels du tampon.
    fn transformed(&self) -> (u32, u32) {
        if swaps(self.transform) { (self.buf.1, self.buf.0) } else { self.buf }
    }

    /// Taille de la surface « issue du tampon » (avant le viewport), en Q32.
    fn from_buffer_q32(&self) -> (i64, i64) {
        let (tw, th) = self.transformed();
        let k = self.scale.max(1) as i64;
        ((tw as i64) * ONE / k, (th as i64) * ONE / k)
    }

    /// Vérifie l'état (à l'application d'un `commit`).
    pub fn validate(&self) -> Result<(), ViewError> {
        let k = self.scale.max(1);
        if let Some(s) = self.src {
            if self.dst.is_none() && (s.w & 0xFF != 0 || s.h & 0xFF != 0) {
                return Err(ViewError::BadSize);
            }
            // En 1/256 de pixel de surface : comparaison exacte sur 64 bits.
            let (tw, th) = self.transformed();
            let (fw, fh) = (tw as i64 * 256, th as i64 * 256);
            let (k, x, y, w, h) = (k as i64, s.x as i64, s.y as i64, s.w as i64, s.h as i64);
            if (x + w) * k > fw || (y + h) * k > fh {
                return Err(ViewError::OutOfBuffer);
            }
        } else if self.buf.0 % k != 0 || self.buf.1 % k != 0 {
            return Err(ViewError::InvalidSize);
        }
        Ok(())
    }

    /// Taille de la surface, en pixels de surface (au moins 1 × 1).
    pub fn size(&self) -> (u32, u32) {
        if let Some((w, h)) = self.dst {
            return (w.max(1), h.max(1));
        }
        if let Some(s) = self.src {
            return (((s.w as u32) >> 8).max(1), ((s.h as u32) >> 8).max(1));
        }
        let (tw, th) = self.transformed();
        let k = self.scale.max(1);
        ((tw / k).max(1), (th / k).max(1))
    }

    /// Un pixel de surface est exactement un pixel du tampon, à la même place.
    pub fn is_identity(&self) -> bool {
        if self.transform != 0 || self.scale > 1 {
            return false;
        }
        let whole = SrcRect { x: 0, y: 0, w: (self.buf.0 as i32) << 8, h: (self.buf.1 as i32) << 8 };
        self.src.map_or(true, |s| s == whole) && self.dst.map_or(true, |d| d == self.buf)
    }

    /// Rectangle source, en Q32 de pixels de la surface « issue du tampon ».
    fn src_q32(&self) -> (i64, i64, i64, i64) {
        match self.src {
            Some(s) => ((s.x as i64) << 24, (s.y as i64) << 24, (s.w as i64) << 24, (s.h as i64) << 24),
            None => {
                let (fw, fh) = self.from_buffer_q32();
                (0, 0, fw, fh)
            }
        }
    }

    /// Correspondance affine de la surface vers le tampon.
    pub fn affine(&self) -> Affine {
        let (dw, dh) = self.size();
        let (sx, sy, sw, sh) = self.src_q32();
        // Point issu du tampon pour le point de surface (x, y) : q = s0 + x·sw/dw.
        // Pas par pixel de surface, et centre du premier pixel.
        let (qx_step, qy_step) = (sw / dw as i64, sh / dh as i64);
        let (qx0, qy0) = (sx + sw / (2 * dw as i64), sy + sh / (2 * dh as i64));
        let (fw, fh) = self.from_buffer_q32();
        let k = self.scale.max(1) as i64;
        // Transformation (weston_transformed_coord), puis × échelle :
        // b = M·q + c, avec M une matrice de signes/permutation.
        let (m, c): ([[i64; 2]; 2], (i64, i64)) = match self.transform {
            1 => ([[0, 1], [-1, 0]], (0, fw)),     // 90
            2 => ([[-1, 0], [0, -1]], (fw, fh)),   // 180
            3 => ([[0, -1], [1, 0]], (fh, 0)),     // 270
            4 => ([[-1, 0], [0, 1]], (fw, 0)),     // retourné
            5 => ([[0, 1], [1, 0]], (0, 0)),       // retourné 90
            6 => ([[1, 0], [0, -1]], (0, fh)),     // retourné 180
            7 => ([[0, -1], [-1, 0]], (fh, fw)),   // retourné 270
            _ => ([[1, 0], [0, 1]], (0, 0)),
        };
        let apply = |qx: i64, qy: i64| -> (i64, i64) { ((m[0][0] * qx + m[0][1] * qy) * k, (m[1][0] * qx + m[1][1] * qy) * k) };
        let o = apply(qx0, qy0);
        let du = apply(qx_step, 0);
        let dv = apply(0, qy_step);
        Affine { origin: (o.0 + c.0 * k, o.1 + c.1 * k), du, dv }
    }

    /// Zone du tampon (pixels) que l'échantillonnage peut lire : le rectangle source, arrondi
    /// vers l'extérieur et borné au tampon. Le filtre ne lit jamais au-delà (pas de fuite de
    /// couleur depuis l'extérieur d'un recadrage).
    pub fn clamp_box(&self) -> Rect {
        let full = Rect::new(0, 0, self.buf.0 as i32, self.buf.1 as i32);
        if self.src.is_none() {
            return full;
        }
        let a = self.affine();
        let (w, h) = self.size();
        // Coins de la surface : centres des pixels extrêmes, élargis d'un demi-pas.
        let corners = [a.at(0, 0), a.at(w as i32 - 1, 0), a.at(0, h as i32 - 1), a.at(w as i32 - 1, h as i32 - 1)];
        let half = |d: (i64, i64)| (d.0.abs() / 2, d.1.abs() / 2);
        let (hu, hv) = (half(a.du), half(a.dv));
        let ex = hu.0 + hv.0;
        let ey = hu.1 + hv.1;
        let x0 = corners.iter().map(|c| c.0).min().unwrap_or(0) - ex;
        let x1 = corners.iter().map(|c| c.0).max().unwrap_or(0) + ex;
        let y0 = corners.iter().map(|c| c.1).min().unwrap_or(0) - ey;
        let y1 = corners.iter().map(|c| c.1).max().unwrap_or(0) + ey;
        let r = Rect::new(floor_px(x0), floor_px(y0), ceil_px(x1), ceil_px(y1));
        let r = r.intersect(&full);
        if r.is_empty() { full } else { r }
    }

    /// Rectangle de la surface touché par un rectangle `r` du tampon (pixels), arrondi vers
    /// l'extérieur, élargi d'un pixel quand le filtre mélange des voisins, borné à la surface.
    pub fn buffer_rect_to_surface(&self, r: Rect) -> Rect {
        let (w, h) = self.size();
        let surface = Rect::new(0, 0, w as i32, h as i32);
        if r.is_empty() {
            return Rect::EMPTY;
        }
        if self.is_identity() {
            return r.intersect(&surface);
        }
        // Inverse de la correspondance affine (matrice de signes/permutation et échelles) :
        // on passe par les quatre coins et l'on prend l'enveloppe.
        let a = self.affine();
        // Pour chaque axe du tampon, l'axe de la surface qui le parcourt.
        let inv = |bx: i64, by: i64| -> (i64, i64) {
            // b = o' + u·du + v·dv, où o' est le coin (0, 0) de la surface (pas le centre).
            let o = (a.origin.0 - (a.du.0 + a.dv.0) / 2, a.origin.1 - (a.du.1 + a.dv.1) / 2);
            let (px, py) = (bx - o.0, by - o.1);
            // du et dv sont alignés sur les axes : l'un a sa composante x nulle, l'autre y.
            let (u, v) = if a.du.1 == 0 && a.dv.0 == 0 {
                (div_q(px, a.du.0), div_q(py, a.dv.1))
            } else {
                (div_q(py, a.du.1), div_q(px, a.dv.0))
            };
            (u, v)
        };
        let (bx0, by0, bx1, by1) = ((r.x0 as i64) << 32, (r.y0 as i64) << 32, (r.x1 as i64) << 32, (r.y1 as i64) << 32);
        let pts = [inv(bx0, by0), inv(bx1, by0), inv(bx0, by1), inv(bx1, by1)];
        let u0 = pts.iter().map(|p| p.0).min().unwrap_or(0);
        let u1 = pts.iter().map(|p| p.0).max().unwrap_or(0);
        let v0 = pts.iter().map(|p| p.1).min().unwrap_or(0);
        let v1 = pts.iter().map(|p| p.1).max().unwrap_or(0);
        let out = Rect::new(floor_px(u0) - 1, floor_px(v0) - 1, ceil_px(u1) + 1, ceil_px(v1) + 1);
        out.intersect(&surface)
    }
}

/// `n / d` où `n` est en Q32 de pixels du tampon et `d` le pas (Q32 de tampon par pixel de
/// surface) : rend des pixels de surface en Q32. `d` nul (surface dégénérée) : 0.
fn div_q(n: i64, d: i64) -> i64 {
    if d == 0 {
        return 0;
    }
    ((n as i128) * (ONE as i128) / (d as i128)) as i64
}

/// Partie entière (vers −∞) d'un Q32, bornée aux `i32`.
pub fn floor_px(q: i64) -> i32 {
    (q >> 32).clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

/// Partie entière (vers +∞) d'un Q32, bornée aux `i32`.
pub fn ceil_px(q: i64) -> i32 {
    ((q + ONE - 1) >> 32).clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(buf: (u32, u32), transform: u32, scale: u32) -> View {
        View { buf, transform, scale, src: None, dst: None }
    }

    /// Pixel du tampon lu pour le pixel `(u, v)` de la surface (échantillonnage au plus proche).
    fn px(v: &View, u: i32, w: i32) -> (i32, i32) {
        let (x, y) = v.affine().at(u, w);
        (floor_px(x), floor_px(y))
    }

    #[test]
    fn identity() {
        let v = view((4, 3), 0, 1);
        assert!(v.is_identity());
        assert_eq!(v.size(), (4, 3));
        assert_eq!(v.affine().at(0, 0), (ONE / 2, ONE / 2));
        assert_eq!(px(&v, 3, 2), (3, 2));
    }

    #[test]
    fn transforms_match_weston() {
        // Tampon 4 × 2 : pixel (u, v) de la surface → pixel du tampon (convention Weston).
        let (bw, bh) = (4i32, 2i32);
        for t in 0..8u32 {
            let v = view((bw as u32, bh as u32), t, 1);
            let (sw, sh) = v.size();
            assert_eq!((sw, sh), if t & 1 == 1 { (2, 4) } else { (4, 2) });
            let (w, h) = (sw as i32, sh as i32);
            for sy in 0..h {
                for sx in 0..w {
                    // weston_transformed_coord sur le centre (sx + ½, sy + ½), puis plancher.
                    let (cx, cy) = (2 * sx + 1, 2 * sy + 1); // en demi-pixels
                    let (w2, h2) = (2 * w, 2 * h);
                    let (bx, by) = match t {
                        0 => (cx, cy),
                        4 => (w2 - cx, cy),
                        1 => (cy, w2 - cx),
                        5 => (cy, cx),
                        2 => (w2 - cx, h2 - cy),
                        6 => (cx, h2 - cy),
                        3 => (h2 - cy, cx),
                        _ => (h2 - cy, w2 - cx),
                    };
                    assert_eq!(px(&v, sx, sy), (bx / 2, by / 2), "transformation {t}, pixel ({sx}, {sy})");
                    let (x, y) = v.affine().at(sx, sy);
                    assert_eq!((x & (ONE - 1), y & (ONE - 1)), (ONE / 2, ONE / 2), "centres exacts");
                }
            }
            assert!(bw * bh == w * h);
        }
    }

    #[test]
    fn scale_and_viewport() {
        let v = view((8, 6), 0, 2);
        assert_eq!(v.size(), (4, 3));
        assert_eq!(v.validate(), Ok(()));
        // Centre du pixel (1, 1) de la surface → (3, 3) du tampon : entre 2 et 3, à mi-chemin.
        assert_eq!(v.affine().at(1, 1), (3 * ONE, 3 * ONE));
        assert_eq!(view((7, 6), 0, 2).validate(), Err(ViewError::InvalidSize));

        // Recadrage (2, 1, 4, 2) mis à l'échelle sur 8 × 4 : pas d'un demi-pixel.
        let mut v = view((10, 10), 0, 1);
        v.src = Some(SrcRect { x: 2 << 8, y: 1 << 8, w: 4 << 8, h: 2 << 8 });
        v.dst = Some((8, 4));
        assert_eq!(v.size(), (8, 4));
        let a = v.affine();
        assert_eq!(a.du, (ONE / 2, 0));
        assert_eq!(a.at(0, 0), (2 * ONE + ONE / 4, ONE + ONE / 4));
        assert_eq!(v.clamp_box(), Rect::new(2, 1, 6, 3));
        assert!(!v.is_identity());

        // Source non entière sans destination ; source hors du tampon.
        v.dst = None;
        v.src = Some(SrcRect { x: 0, y: 0, w: 384, h: 256 });
        assert_eq!(v.validate(), Err(ViewError::BadSize));
        v.src = Some(SrcRect { x: 8 << 8, y: 0, w: 4 << 8, h: 1 << 8 });
        assert_eq!(v.validate(), Err(ViewError::OutOfBuffer));

        // Tampon d'un pixel étiré (single-pixel-buffer).
        let mut v = view((1, 1), 0, 1);
        v.dst = Some((300, 200));
        assert_eq!(v.size(), (300, 200));
        assert_eq!(px(&v, 299, 199), (0, 0));
        assert_eq!(px(&v, 0, 0), (0, 0));
    }

    #[test]
    fn damage_mapping() {
        let v = view((8, 6), 0, 2);
        // Pixel (2, 2) du tampon : surface (1, 1), élargi d'un pixel pour le filtre.
        assert_eq!(v.buffer_rect_to_surface(Rect::new(2, 2, 3, 3)), Rect::new(0, 0, 3, 3));
        let id = view((8, 6), 0, 1);
        assert_eq!(id.buffer_rect_to_surface(Rect::new(2, 2, 3, 3)), Rect::new(2, 2, 3, 3));
        // Rotation de 90 (x du tampon = y de la surface) : la colonne x = 0 du tampon est la
        // ligne du haut de la surface.
        let r = view((4, 2), 1, 1);
        let s = r.buffer_rect_to_surface(Rect::new(0, 0, 1, 2));
        assert!(s.contains(&Rect::new(0, 0, 2, 1)), "{s:?}");
        for u in 0..2 {
            assert_eq!(px(&r, u, 0).0, 0);
        }
        // Damage de tout le tampon : toute la surface.
        assert_eq!(r.buffer_rect_to_surface(Rect::new(0, 0, 4, 2)), Rect::new(0, 0, 2, 4));
    }
}
