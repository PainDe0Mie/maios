//! Lecture d'un bloc EDID 1.3/1.4 (VESA E-EDID, § 3) : fabricant, produit, nom du moniteur et
//! mode préféré (premier descripteur de timing détaillé).

use alloc::string::String;

#[derive(Clone, Debug, Default)]
pub struct EdidInfo {
    /// Code PNP du fabricant (trois lettres).
    pub manufacturer: [u8; 3],
    pub product: u16,
    pub version: (u8, u8),
    /// Nom du moniteur (descripteur 0xFC), vide s'il n'y en a pas.
    pub name: String,
    /// Mode préféré `(largeur, hauteur, fréquence en mHz)`, si un timing détaillé est présent.
    pub preferred: Option<(u32, u32, u32)>,
}

impl EdidInfo {
    pub fn manufacturer_str(&self) -> &str {
        core::str::from_utf8(&self.manufacturer).unwrap_or("???")
    }
}

/// Analyse le bloc de base (128 octets) ; `None` si l'en-tête ou la somme de contrôle est faux.
pub fn parse(b: &[u8]) -> Option<EdidInfo> {
    if b.len() < 128 || b[0..8] != [0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0] {
        return None;
    }
    if b[..128].iter().fold(0u8, |a, &x| a.wrapping_add(x)) != 0 {
        return None;
    }
    let m = u16::from_be_bytes([b[8], b[9]]);
    let letter = |v: u16| (b'A' - 1).wrapping_add((v & 0x1F) as u8);
    let mut info = EdidInfo {
        manufacturer: [letter(m >> 10), letter(m >> 5), letter(m)],
        product: u16::from_le_bytes([b[10], b[11]]),
        version: (b[18], b[19]),
        ..Default::default()
    };
    for d in (54..126).step_by(18) {
        let desc = &b[d..d + 18];
        let clock = u16::from_le_bytes([desc[0], desc[1]]) as u64; // en 10 kHz
        if clock != 0 {
            if info.preferred.is_none() {
                let ha = desc[2] as u32 | ((desc[4] as u32 & 0xF0) << 4);
                let hb = desc[3] as u32 | ((desc[4] as u32 & 0x0F) << 8);
                let va = desc[5] as u32 | ((desc[7] as u32 & 0xF0) << 4);
                let vb = desc[6] as u32 | ((desc[7] as u32 & 0x0F) << 8);
                let total = (ha + hb) as u64 * (va + vb) as u64;
                let mhz = if total == 0 { 0 } else { (clock * 10_000 * 1000 / total) as u32 };
                info.preferred = Some((ha, va, mhz));
            }
        } else if desc[3] == 0xFC {
            let text = &desc[5..18];
            let end = text.iter().position(|&c| c == 0x0A).unwrap_or(text.len());
            info.name = text[..end].iter().map(|&c| if c.is_ascii_graphic() || c == b' ' { c as char } else { '?' }).collect();
            let trimmed = info.name.trim_end().len();
            info.name.truncate(trimmed);
        }
    }
    Some(info)
}
