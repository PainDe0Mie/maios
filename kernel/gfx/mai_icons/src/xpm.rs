//! Décodeur XPM 3 (X PixMap) : le fichier est du C, l'image est dans ses chaînes littérales
//! (« largeur hauteur couleurs caractères_par_pixel », la table des couleurs, puis les lignes).
//! Couleurs : clé `c` (à défaut `g`, `g4`, `m`), `None` = transparent, `#RGB` à
//! `#RRRRGGGGBBBB`, noms X11 courants. Debian range encore beaucoup d'icônes ainsi
//! (`/usr/share/pixmaps`).

use alloc::string::String;
use alloc::vec::Vec;

use crate::Image;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    BadHeader,
    TooLarge,
    BadColor,
    Truncated,
    UnknownPixel,
}

/// Chaînes littérales du fichier (commentaires sautés, échappements simples).
fn strings(text: &str) -> Vec<String> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b'"' => {
                i += 1;
                let mut s = Vec::new();
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' && i + 1 < b.len() {
                        i += 1;
                    }
                    s.push(b[i]);
                    i += 1;
                }
                i += 1;
                out.push(String::from_utf8_lossy(&s).into_owned());
            }
            _ => i += 1,
        }
    }
    out
}

fn hex(s: &str) -> Option<u32> {
    u32::from_str_radix(s, 16).ok()
}

/// Couleur X11 → RVBA (`None` : transparent).
fn parse_color(v: &str) -> Option<[u8; 4]> {
    let v = v.trim();
    if v.eq_ignore_ascii_case("none") {
        return Some([0, 0, 0, 0]);
    }
    if let Some(h) = v.strip_prefix('#') {
        let n = h.len();
        if n % 3 != 0 || n == 0 || n > 12 {
            return None;
        }
        let k = n / 3;
        let c = |i: usize| -> Option<u8> {
            let x = hex(&h[i * k..(i + 1) * k])?;
            // Ramène k chiffres hexadécimaux à 8 bits.
            Some(match k {
                1 => (x * 17) as u8,
                2 => x as u8,
                _ => (x >> (4 * (k - 2))) as u8,
            })
        };
        return Some([c(0)?, c(1)?, c(2)?, 255]);
    }
    let name: String = v.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
    // Gris numérotés : gray0 … gray100.
    for p in ["gray", "grey"] {
        if let Some(n) = name.strip_prefix(p).and_then(|r| r.parse::<u32>().ok()) {
            if n <= 100 {
                let g = ((n * 255 + 50) / 100) as u8;
                return Some([g, g, g, 255]);
            }
        }
    }
    let rgb = match name.as_str() {
        "black" => 0x000000,
        "white" => 0xFFFFFF,
        "red" => 0xFF0000,
        "green" => 0x00FF00,
        "blue" => 0x0000FF,
        "yellow" => 0xFFFF00,
        "cyan" => 0x00FFFF,
        "magenta" => 0xFF00FF,
        "gray" | "grey" => 0xBEBEBE,
        "darkgray" | "darkgrey" => 0xA9A9A9,
        "lightgray" | "lightgrey" => 0xD3D3D3,
        "dimgray" | "dimgrey" => 0x696969,
        "darkslategray" | "darkslategrey" => 0x2F4F4F,
        "slategray" | "slategrey" => 0x708090,
        "navy" | "navyblue" => 0x000080,
        "orange" => 0xFFA500,
        "brown" => 0xA52A2A,
        "darkgreen" => 0x006400,
        "darkred" => 0x8B0000,
        "darkblue" => 0x00008B,
        "gold" => 0xFFD700,
        "pink" => 0xFFC0CB,
        "purple" => 0xA020F0,
        "violet" => 0xEE82EE,
        "beige" => 0xF5F5DC,
        "tan" => 0xD2B48C,
        "khaki" => 0xF0E68C,
        "maroon" => 0xB03060,
        "steelblue" => 0x4682B4,
        "skyblue" => 0x87CEEB,
        "lightblue" => 0xADD8E6,
        "lightyellow" => 0xFFFFE0,
        "wheat" => 0xF5DEB3,
        "salmon" => 0xFA8072,
        "seagreen" => 0x2E8B57,
        "forestgreen" => 0x228B22,
        "limegreen" => 0x32CD32,
        "royalblue" => 0x4169E1,
        "midnightblue" => 0x191970,
        "lightslategray" | "lightslategrey" => 0x778899,
        "gainsboro" => 0xDCDCDC,
        "whitesmoke" => 0xF5F5F5,
        "ivory" => 0xFFFFF0,
        _ => return None,
    };
    Some([(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8, 255])
}

/// Valeur d'une clé de couleur (`c`, `m`, `g`, `g4`, `s`) dans la fin d'une entrée.
fn color_value(spec: &str) -> Option<[u8; 4]> {
    let toks: Vec<&str> = spec.split_whitespace().collect();
    let keys = ["c", "m", "g", "g4", "s"];
    let mut found: Vec<(&str, String)> = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        if keys.contains(&toks[i]) {
            let key = toks[i];
            let mut v = String::new();
            i += 1;
            while i < toks.len() && !keys.contains(&toks[i]) {
                if !v.is_empty() {
                    v.push(' ');
                }
                v.push_str(toks[i]);
                i += 1;
            }
            found.push((key, v));
        } else {
            i += 1;
        }
    }
    for want in ["c", "g", "g4", "m"] {
        if let Some((_, v)) = found.iter().find(|(k, _)| *k == want) {
            if let Some(c) = parse_color(v) {
                return Some(c);
            }
        }
    }
    None
}

pub fn decode(text: &str, max_pixels: usize) -> Result<Image, Error> {
    let s = strings(text);
    let head: Vec<usize> = s.first().ok_or(Error::BadHeader)?.split_whitespace().take(4).map(|t| t.parse().unwrap_or(0)).collect();
    if head.len() < 4 {
        return Err(Error::BadHeader);
    }
    let (w, h, n, cpp) = (head[0], head[1], head[2], head[3]);
    if w == 0 || h == 0 || n == 0 || cpp == 0 || cpp > 4 {
        return Err(Error::BadHeader);
    }
    if w > 4096 || h > 4096 || w * h > max_pixels || n > 1 << 16 {
        return Err(Error::TooLarge);
    }
    if s.len() < 1 + n + h {
        return Err(Error::Truncated);
    }
    let mut table: Vec<(&[u8], [u8; 4])> = Vec::with_capacity(n);
    for e in &s[1..1 + n] {
        let b = e.as_bytes();
        if b.len() < cpp {
            return Err(Error::BadColor);
        }
        let c = e.get(cpp..).and_then(color_value).ok_or(Error::BadColor)?;
        let a = c[3] as u32;
        let m = |v: u8| ((v as u32 * a + 127) / 255) as u8;
        table.push((&b[..cpp], [m(c[0]), m(c[1]), m(c[2]), c[3]]));
    }
    // Recherche directe pour un caractère par pixel (le cas courant).
    let mut direct = [None::<[u8; 4]>; 256];
    if cpp == 1 {
        for (k, c) in &table {
            direct[k[0] as usize] = Some(*c);
        }
    }
    let mut pixels = Vec::with_capacity(w * h);
    for line in &s[1 + n..1 + n + h] {
        let b = line.as_bytes();
        if b.len() < w * cpp {
            return Err(Error::Truncated);
        }
        for x in 0..w {
            let key = &b[x * cpp..(x + 1) * cpp];
            let c = if cpp == 1 {
                direct[key[0] as usize]
            } else {
                table.iter().find(|(k, _)| *k == key).map(|(_, c)| *c)
            };
            pixels.push(c.ok_or(Error::UnknownPixel)?);
        }
    }
    Ok(Image { width: w, height: h, pixels })
}
