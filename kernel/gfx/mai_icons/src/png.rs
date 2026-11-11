//! Décodeur PNG (ISO/IEC 15948) complet : tous les types de couleur et profondeurs
//! (gris 1/2/4/8/16, RVB 8/16, palette 1/2/4/8, gris + alpha 8/16, RVBA 8/16), entrelacement
//! Adam7, les cinq filtres, transparence `tRNS`, CRC de chaque segment vérifié. Sortie : RVBA
//! 8 bits **prémultipliée** ([`crate::Image`]). La gamma et les profils de couleur sont ignorés
//! (les icônes sont en sRGB).

use alloc::vec::Vec;

use crate::inflate;
use crate::Image;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    BadSignature,
    Truncated,
    BadCrc,
    /// `IHDR` absent, mal placé ou incohérent (profondeur, type, méthodes).
    BadHeader,
    /// Image plus grande que la borne de l'appelant.
    TooLarge,
    /// Palette absente, trop longue, ou indice hors de la palette.
    BadPalette,
    /// Filtre de ligne inconnu.
    BadFilter,
    /// Données d'image de mauvaise taille.
    BadData,
    Inflate(inflate::Error),
}

impl From<inflate::Error> for Error {
    fn from(e: inflate::Error) -> Self {
        Error::Inflate(e)
    }
}

const SIGNATURE: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];

/// CRC-32 (polynôme 0xEDB88320), table calculée à la compilation.
const CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[n] = c;
        n += 1;
    }
    t
};

pub fn crc32(parts: &[&[u8]]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for p in parts {
        for &b in *p {
            c = CRC_TABLE[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
        }
    }
    c ^ 0xFFFF_FFFF
}

/// La donnée commence-t-elle comme un PNG ?
pub fn is_png(data: &[u8]) -> bool {
    data.starts_with(&SIGNATURE)
}

struct Header {
    width: usize,
    height: usize,
    depth: u8,
    color: u8,
    interlace: bool,
}

impl Header {
    fn channels(&self) -> usize {
        match self.color {
            0 | 3 => 1,
            4 => 2,
            2 => 3,
            _ => 4,
        }
    }
    /// Octets par ligne filtrée (sans l'octet de filtre) pour `w` pixels.
    fn row_bytes(&self, w: usize) -> usize {
        (w * self.channels() * self.depth as usize + 7) / 8
    }
    /// Pas du filtre : octets d'un pixel, au moins 1.
    fn bpp(&self) -> usize {
        ((self.channels() * self.depth as usize) / 8).max(1)
    }
}

/// Passes d'Adam7 : `(x0, y0, dx, dy)`.
const ADAM7: [(usize, usize, usize, usize); 7] =
    [(0, 0, 8, 8), (4, 0, 8, 8), (0, 4, 4, 8), (2, 0, 4, 4), (0, 2, 2, 4), (1, 0, 2, 2), (0, 1, 1, 2)];

fn pass_size(h: &Header, p: (usize, usize, usize, usize)) -> (usize, usize) {
    let (x0, y0, dx, dy) = p;
    let w = if h.width > x0 { (h.width - x0 + dx - 1) / dx } else { 0 };
    let hh = if h.height > y0 { (h.height - y0 + dy - 1) / dy } else { 0 };
    (w, hh)
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i16 + b as i16 - c as i16;
    let (pa, pb, pc) = ((p - a as i16).abs(), (p - b as i16).abs(), (p - c as i16).abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Défiltre en place `rows` lignes de `stride` octets (chacune précédée de son octet de filtre).
fn unfilter(data: &mut [u8], rows: usize, stride: usize, bpp: usize) -> Result<(), Error> {
    let line = stride + 1;
    for y in 0..rows {
        let (prev, cur) = data.split_at_mut(y * line);
        let prev = if y == 0 { None } else { Some(&prev[(y - 1) * line + 1..y * line]) };
        let cur = &mut cur[..line];
        let filter = cur[0];
        let cur = &mut cur[1..];
        match (filter, prev) {
            (0, _) => {}
            (1, _) => {
                for i in bpp..stride {
                    cur[i] = cur[i].wrapping_add(cur[i - bpp]);
                }
            }
            (2, None) => {}
            (2, Some(p)) => {
                for i in 0..stride {
                    cur[i] = cur[i].wrapping_add(p[i]);
                }
            }
            (3, p) => {
                for i in 0..stride {
                    let a = if i >= bpp { cur[i - bpp] as u16 } else { 0 };
                    let b = p.map_or(0, |p| p[i] as u16);
                    cur[i] = cur[i].wrapping_add(((a + b) / 2) as u8);
                }
            }
            (4, p) => {
                for i in 0..stride {
                    let a = if i >= bpp { cur[i - bpp] } else { 0 };
                    let b = p.map_or(0, |p| p[i]);
                    let c = if i >= bpp { p.map_or(0, |p| p[i - bpp]) } else { 0 };
                    cur[i] = cur[i].wrapping_add(paeth(a, b, c));
                }
            }
            _ => return Err(Error::BadFilter),
        }
    }
    Ok(())
}

/// Échantillon `i` (en unités de `depth` bits) d'une ligne défiltrée.
#[inline]
fn sample(row: &[u8], i: usize, depth: u8) -> u16 {
    match depth {
        8 => row[i] as u16,
        16 => u16::from_be_bytes([row[2 * i], row[2 * i + 1]]),
        d => {
            let bit = i * d as usize;
            let byte = row[bit / 8];
            let shift = 8 - d as usize - (bit % 8);
            ((byte >> shift) & ((1u8 << d) - 1)) as u16
        }
    }
}

/// Ramène un échantillon de `depth` bits à 8 bits.
#[inline]
fn to8(v: u16, depth: u8) -> u8 {
    match depth {
        16 => (v >> 8) as u8,
        8 => v as u8,
        d => (v as u32 * 255 / ((1u32 << d) - 1)) as u8,
    }
}

struct Ctx<'a> {
    h: &'a Header,
    palette: &'a [[u8; 4]],
    /// Couleur transparente (gris : `[g, _, _]`, RVB : `[r, g, b]`), en échantillons bruts.
    key: Option<[u16; 3]>,
}

impl Ctx<'_> {
    /// Pixel `x` d'une ligne défiltrée → RVBA non prémultiplié.
    fn pixel(&self, row: &[u8], x: usize) -> Result<[u8; 4], Error> {
        let d = self.h.depth;
        Ok(match self.h.color {
            0 => {
                let g = sample(row, x, d);
                let a = if self.key.is_some_and(|k| k[0] == g) { 0 } else { 255 };
                let g8 = to8(g, d);
                [g8, g8, g8, a]
            }
            2 => {
                let (r, g, b) = (sample(row, 3 * x, d), sample(row, 3 * x + 1, d), sample(row, 3 * x + 2, d));
                let a = if self.key == Some([r, g, b]) { 0 } else { 255 };
                [to8(r, d), to8(g, d), to8(b, d), a]
            }
            3 => {
                let i = sample(row, x, d) as usize;
                *self.palette.get(i).ok_or(Error::BadPalette)?
            }
            4 => {
                let g = to8(sample(row, 2 * x, d), d);
                [g, g, g, to8(sample(row, 2 * x + 1, d), d)]
            }
            _ => [
                to8(sample(row, 4 * x, d), d),
                to8(sample(row, 4 * x + 1, d), d),
                to8(sample(row, 4 * x + 2, d), d),
                to8(sample(row, 4 * x + 3, d), d),
            ],
        })
    }
}

fn premultiply(p: [u8; 4]) -> [u8; 4] {
    let a = p[3] as u32;
    let m = |c: u8| ((c as u32 * a + 127) / 255) as u8;
    [m(p[0]), m(p[1]), m(p[2]), p[3]]
}

/// Décode un PNG ; `max_pixels` borne la taille acceptée (largeur × hauteur).
pub fn decode(data: &[u8], max_pixels: usize) -> Result<Image, Error> {
    if !is_png(data) {
        return Err(Error::BadSignature);
    }
    let mut pos = 8;
    let mut header: Option<Header> = None;
    let mut palette: Vec<[u8; 4]> = Vec::new();
    let mut trns: Option<Vec<u8>> = None;
    let mut idat: Vec<u8> = Vec::new();
    let mut seen_idat = false;
    loop {
        if data.len() - pos < 12 {
            return Err(Error::Truncated);
        }
        let len = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        if len > 0x7fff_ffff || data.len() - pos - 12 < len {
            return Err(Error::Truncated);
        }
        let kind = &data[pos + 4..pos + 8];
        let body = &data[pos + 8..pos + 8 + len];
        let crc = u32::from_be_bytes([data[pos + 8 + len], data[pos + 9 + len], data[pos + 10 + len], data[pos + 11 + len]]);
        if crc32(&[kind, body]) != crc {
            return Err(Error::BadCrc);
        }
        pos += 12 + len;
        if header.is_none() && kind != b"IHDR" {
            return Err(Error::BadHeader);
        }
        match kind {
            b"IHDR" => {
                if header.is_some() || len != 13 {
                    return Err(Error::BadHeader);
                }
                let w = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
                let h = u32::from_be_bytes([body[4], body[5], body[6], body[7]]) as usize;
                let (depth, color) = (body[8], body[9]);
                let ok = matches!(
                    (color, depth),
                    (0, 1 | 2 | 4 | 8 | 16) | (2, 8 | 16) | (3, 1 | 2 | 4 | 8) | (4, 8 | 16) | (6, 8 | 16)
                );
                if !ok || body[10] != 0 || body[11] != 0 || body[12] > 1 || w == 0 || h == 0 {
                    return Err(Error::BadHeader);
                }
                if w > 1 << 16 || h > 1 << 16 || w * h > max_pixels {
                    return Err(Error::TooLarge);
                }
                header = Some(Header { width: w, height: h, depth, color, interlace: body[12] == 1 });
            }
            b"PLTE" => {
                if len % 3 != 0 || len / 3 > 256 || len == 0 || seen_idat {
                    return Err(Error::BadPalette);
                }
                palette = body.chunks(3).map(|c| [c[0], c[1], c[2], 255]).collect();
            }
            b"tRNS" => trns = Some(body.to_vec()),
            b"IDAT" => {
                seen_idat = true;
                idat.extend_from_slice(body);
            }
            b"IEND" => break,
            _ => {
                // Segment critique inconnu (majuscule en tête) : illisible.
                if kind[0] & 0x20 == 0 {
                    return Err(Error::BadHeader);
                }
            }
        }
    }
    let h = header.ok_or(Error::BadHeader)?;
    if h.color == 3 && palette.is_empty() {
        return Err(Error::BadPalette);
    }
    let mut key = None;
    if let Some(t) = &trns {
        match h.color {
            3 => {
                if t.len() > palette.len() {
                    return Err(Error::BadPalette);
                }
                for (i, &a) in t.iter().enumerate() {
                    palette[i][3] = a;
                }
            }
            0 if t.len() >= 2 => key = Some([u16::from_be_bytes([t[0], t[1]]), 0, 0]),
            2 if t.len() >= 6 => {
                key = Some([
                    u16::from_be_bytes([t[0], t[1]]),
                    u16::from_be_bytes([t[2], t[3]]),
                    u16::from_be_bytes([t[4], t[5]]),
                ])
            }
            _ => {}
        }
    }

    // Taille exacte des données défiltrées attendues.
    let passes: Vec<(usize, usize, usize, usize)> = if h.interlace { ADAM7.to_vec() } else { alloc::vec![(0, 0, 1, 1)] };
    let mut expected = 0usize;
    for &p in &passes {
        let (pw, ph) = pass_size(&h, p);
        if pw > 0 && ph > 0 {
            expected += ph * (h.row_bytes(pw) + 1);
        }
    }
    let mut raw = inflate::zlib_decompress(&idat, expected)?;
    if raw.len() != expected {
        return Err(Error::BadData);
    }

    let ctx = Ctx { h: &h, palette: &palette, key };
    let mut pixels = alloc::vec![[0u8; 4]; h.width * h.height];
    let bpp = h.bpp();
    let mut off = 0usize;
    for &p in &passes {
        let (pw, ph) = pass_size(&h, p);
        if pw == 0 || ph == 0 {
            continue;
        }
        let stride = h.row_bytes(pw);
        let size = ph * (stride + 1);
        let block = &mut raw[off..off + size];
        unfilter(block, ph, stride, bpp)?;
        let (x0, y0, dx, dy) = p;
        for py in 0..ph {
            let row = &block[py * (stride + 1) + 1..(py + 1) * (stride + 1)];
            let y = y0 + py * dy;
            for px in 0..pw {
                let x = x0 + px * dx;
                pixels[y * h.width + x] = premultiply(ctx.pixel(row, px)?);
            }
        }
        off += size;
    }
    Ok(Image { width: h.width, height: h.height, pixels })
}
