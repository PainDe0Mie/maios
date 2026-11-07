//! Classes de taille.
//!
//! 8 classes exactes de 8 à 64 octets (les objets d'un noyau sont surtout de quelques mots),
//! puis 4 classes par puissance de deux jusqu'à 128 Kio, comme mimalloc et jemalloc : la perte
//! interne reste sous 25 % (sous 12,5 % en moyenne).

use crate::{MAX_ALIGN, SLICE_SIZE};

/// Nombre de classes.
pub const NCLASS: usize = 52;

/// Plus grand objet servi par les classes ; au-delà, l'objet est mappé directement.
pub const MAX_SMALL: usize = 128 * 1024;

/// Nombre minimal d'objets par page : fixe la taille des pages des grandes classes.
const MIN_OBJECTS_PER_PAGE: usize = 8;

/// Taille des blocs de chaque classe.
pub static SIZES: [u32; NCLASS] = build();

const fn build() -> [u32; NCLASS] {
    let mut t = [0u32; NCLASS];
    let mut i = 0;
    while i < 8 {
        t[i] = ((i + 1) * 8) as u32;
        i += 1;
    }
    let mut b = 6;
    while b < 17 {
        let base = 1u32 << b;
        let mut k = 1;
        while k <= 4 {
            t[i] = base + k * (base / 4);
            i += 1;
            k += 1;
        }
        b += 1;
    }
    t
}

/// Classe de la taille `size` (1 ≤ `size` ≤ [`MAX_SMALL`]), sans contrainte d'alignement.
#[inline(always)]
pub fn of_size(size: usize) -> usize {
    if size <= 64 {
        return (size.max(1) + 7) / 8 - 1;
    }
    let w = size - 1;
    let b = (usize::BITS - 1 - w.leading_zeros()) as usize; // ⌊log2 w⌋ ≥ 6
    let k = (w - (1 << b)) >> (b - 2);
    8 + (b - 6) * 4 + k
}

/// Classe servant `size` octets alignés sur `align`, ou `None` si l'objet est trop grand ou trop
/// aligné (il passe alors par le mappage direct).
///
/// Un bloc de taille `s` commence à un multiple de `s` depuis un début de page aligné sur
/// 8 Kio : il est donc aligné sur le plus grand diviseur puissance de deux de `s` (plafonné à
/// 8 Kio). On remonte les classes jusqu'à en trouver une assez alignée ; comme une `Layout` a
/// toujours une taille multiple de son alignement, la dernière classe (128 Kio) convient
/// toujours.
#[inline(always)]
pub fn lookup(size: usize, align: usize) -> Option<usize> {
    if size > MAX_SMALL || align > MAX_ALIGN {
        return None;
    }
    let mut c = of_size(size);
    if align > 8 {
        while (SIZES[c] as usize) & (align - 1) != 0 {
            c += 1;
            if c >= NCLASS {
                return None;
            }
        }
    }
    Some(c)
}

/// Vrai si l'objet passe par les classes (et donc par les segments).
#[inline(always)]
pub fn is_small(size: usize, align: usize) -> bool {
    size <= MAX_SMALL && align <= MAX_ALIGN
}

/// Nombre de tranches de 64 Kio d'une page de la classe `c` : au moins 8 objets par page.
#[inline]
pub fn slices(c: usize) -> usize {
    let bs = SIZES[c] as usize;
    ((MIN_OBJECTS_PER_PAGE * bs + SLICE_SIZE - 1) / SLICE_SIZE).max(1)
}
