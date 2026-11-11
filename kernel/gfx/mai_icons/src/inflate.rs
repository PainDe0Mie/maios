//! Décompression DEFLATE (RFC 1951) et enveloppe zlib (RFC 1950).
//!
//! Codes de Huffman canoniques décodés par une table directe de `FAST_BITS` bits (un seul
//! accès pour presque tous les symboles), puis, pour les codes plus longs, par le parcours
//! canonique de `puff` (zlib) : compte des codes par longueur, sans arbre. Tout est vérifié :
//! codes sur-souscrits, distances avant le début, longueurs réservées, bloc stocké dont `NLEN`
//! ne complète pas `LEN`, sortie bornée par l'appelant (une image ne peut pas faire allouer
//! plus que sa taille annoncée).

use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Flux coupé avant la fin.
    Truncated,
    /// Type de bloc 3 (réservé).
    BadBlockType,
    /// Bloc stocké : `NLEN` n'est pas le complément de `LEN`.
    BadStoredLength,
    /// Code de Huffman impossible (sur-souscrit, sans fin de bloc, symbole réservé).
    BadCode,
    /// Distance qui remonte avant le début de la sortie.
    BadDistance,
    /// La sortie dépasserait la borne donnée.
    TooLarge,
    /// En-tête zlib invalide (méthode, fenêtre, somme de contrôle, dictionnaire).
    BadZlibHeader,
    /// Adler-32 différente.
    BadChecksum,
}

const FAST_BITS: u32 = 9;
const FAST_MASK: u64 = (1 << FAST_BITS) - 1;
const MAX_BITS: usize = 15;

/// Lecteur de bits, poids faible d'abord.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u64,
    cnt: u32,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Bits { data, pos: 0, buf: 0, cnt: 0 }
    }

    /// Remplit le tampon autant que possible (jusqu'à 56 bits), sans erreur en fin de flux.
    #[inline]
    fn refill(&mut self) {
        while self.cnt <= 48 && self.pos < self.data.len() {
            self.buf |= (self.data[self.pos] as u64) << self.cnt;
            self.pos += 1;
            self.cnt += 8;
        }
    }

    #[inline]
    fn bits(&mut self, n: u32) -> Result<u32, Error> {
        if n == 0 {
            return Ok(0);
        }
        if self.cnt < n {
            self.refill();
            if self.cnt < n {
                return Err(Error::Truncated);
            }
        }
        let v = (self.buf & ((1u64 << n) - 1)) as u32;
        self.buf >>= n;
        self.cnt -= n;
        Ok(v)
    }

    /// Saute jusqu'à la prochaine frontière d'octet.
    fn align(&mut self) {
        let r = self.cnt % 8;
        self.buf >>= r;
        self.cnt -= r;
    }

    /// Octets entiers restants, tampon compris (après `align`).
    fn take_bytes(&mut self, n: usize, out: &mut Vec<u8>) -> Result<(), Error> {
        let mut n = n;
        while n > 0 && self.cnt >= 8 {
            out.push(self.buf as u8);
            self.buf >>= 8;
            self.cnt -= 8;
            n -= 1;
        }
        if self.data.len() - self.pos < n {
            return Err(Error::Truncated);
        }
        out.extend_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(())
    }

    /// Position dans les données à la frontière d'octet courante (après le flux).
    fn byte_pos(&self) -> usize {
        self.pos - (self.cnt / 8) as usize
    }
}

/// Code de Huffman canonique.
struct Huffman {
    /// Nombre de codes de chaque longueur (0 inutilisé).
    counts: [u16; MAX_BITS + 1],
    /// Symboles rangés par longueur de code puis par valeur.
    symbols: Vec<u16>,
    /// Table directe : `(symbole << 4) | longueur`, 0 = code plus long que `FAST_BITS`.
    fast: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Huffman, Error> {
        let mut counts = [0u16; MAX_BITS + 1];
        for &l in lengths {
            counts[l as usize] += 1;
        }
        counts[0] = 0;
        // Sur-souscription interdite ; un code incomplet est permis (un seul code de distance,
        // par exemple) : ses trous ne seront jamais lus par un flux valide.
        let mut left: i32 = 1;
        for len in 1..=MAX_BITS {
            left <<= 1;
            left -= counts[len] as i32;
            if left < 0 {
                return Err(Error::BadCode);
            }
        }
        let mut offs = [0u16; MAX_BITS + 2];
        for len in 1..=MAX_BITS {
            offs[len + 1] = offs[len] + counts[len];
        }
        let mut symbols = alloc::vec![0u16; offs[MAX_BITS + 1] as usize];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[offs[l as usize] as usize] = sym as u16;
                offs[l as usize] += 1;
            }
        }
        // Codes canoniques des longueurs ≤ FAST_BITS, retournés (le flux est lu poids faible
        // d'abord, le code poids fort d'abord).
        let mut fast = alloc::vec![0u16; 1 << FAST_BITS];
        let mut code: u32 = 0;
        let mut index = 0usize;
        for len in 1..=MAX_BITS as u32 {
            let n = counts[len as usize] as usize;
            if len <= FAST_BITS {
                for k in 0..n {
                    let c = code + k as u32;
                    let rev = c.reverse_bits() >> (32 - len);
                    let entry = (symbols[index + k] << 4) | len as u16;
                    let mut i = rev as usize;
                    while i < fast.len() {
                        fast[i] = entry;
                        i += 1 << len;
                    }
                }
            }
            index += n;
            code = (code + n as u32) << 1;
        }
        Ok(Huffman { counts, symbols, fast })
    }

    fn decode(&self, b: &mut Bits) -> Result<u16, Error> {
        b.refill();
        let e = self.fast[(b.buf & FAST_MASK) as usize];
        if e != 0 {
            let len = (e & 15) as u32;
            if len > b.cnt {
                return Err(Error::Truncated);
            }
            b.buf >>= len;
            b.cnt -= len;
            return Ok(e >> 4);
        }
        // Code long : parcours canonique, un bit à la fois.
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..=MAX_BITS {
            code |= b.bits(1)? as i32;
            let count = self.counts[len] as i32;
            if code - count < first {
                return Ok(self.symbols[(index + code - first) as usize]);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err(Error::BadCode)
    }
}

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145,
    8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];
/// Ordre des longueurs du code des longueurs (bloc dynamique).
const CL_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

fn fixed_codes() -> (Huffman, Huffman) {
    let mut l = [0u8; 288];
    l[..144].fill(8);
    l[144..256].fill(9);
    l[256..280].fill(7);
    l[280..].fill(8);
    // Les tables fixes sont toujours valides.
    let lit = Huffman::new(&l).unwrap_or_else(|_| unreachable!());
    let dist = Huffman::new(&[5u8; 30]).unwrap_or_else(|_| unreachable!());
    (lit, dist)
}

fn dynamic_codes(b: &mut Bits) -> Result<(Huffman, Huffman), Error> {
    let nlen = b.bits(5)? as usize + 257;
    let ndist = b.bits(5)? as usize + 1;
    let ncode = b.bits(4)? as usize + 4;
    if nlen > 286 || ndist > 30 {
        return Err(Error::BadCode);
    }
    let mut cl = [0u8; 19];
    for &i in CL_ORDER.iter().take(ncode) {
        cl[i] = b.bits(3)? as u8;
    }
    let clh = Huffman::new(&cl)?;
    let mut lengths = [0u8; 286 + 30];
    let mut i = 0;
    while i < nlen + ndist {
        let sym = clh.decode(b)?;
        if sym < 16 {
            lengths[i] = sym as u8;
            i += 1;
            continue;
        }
        let (val, rep) = match sym {
            16 => {
                if i == 0 {
                    return Err(Error::BadCode);
                }
                (lengths[i - 1], 3 + b.bits(2)? as usize)
            }
            17 => (0, 3 + b.bits(3)? as usize),
            _ => (0, 11 + b.bits(7)? as usize),
        };
        if i + rep > nlen + ndist {
            return Err(Error::BadCode);
        }
        lengths[i..i + rep].fill(val);
        i += rep;
    }
    if lengths[256] == 0 {
        return Err(Error::BadCode); // pas de fin de bloc
    }
    Ok((Huffman::new(&lengths[..nlen])?, Huffman::new(&lengths[nlen..nlen + ndist])?))
}

fn codes(b: &mut Bits, out: &mut Vec<u8>, lit: &Huffman, dist: &Huffman, limit: usize) -> Result<(), Error> {
    loop {
        let sym = lit.decode(b)? as usize;
        if sym < 256 {
            if out.len() >= limit {
                return Err(Error::TooLarge);
            }
            out.push(sym as u8);
            continue;
        }
        if sym == 256 {
            return Ok(());
        }
        let s = sym - 257;
        if s >= 29 {
            return Err(Error::BadCode);
        }
        let len = LEN_BASE[s] as usize + b.bits(LEN_EXTRA[s] as u32)? as usize;
        let d = dist.decode(b)? as usize;
        if d >= 30 {
            return Err(Error::BadCode);
        }
        let dd = DIST_BASE[d] as usize + b.bits(DIST_EXTRA[d] as u32)? as usize;
        if dd > out.len() {
            return Err(Error::BadDistance);
        }
        if out.len() + len > limit {
            return Err(Error::TooLarge);
        }
        let start = out.len() - dd;
        if dd >= len {
            out.extend_from_within(start..start + len);
        } else {
            for k in 0..len {
                let v = out[start + k];
                out.push(v);
            }
        }
    }
}

/// Décompresse un flux DEFLATE brut ; renvoie la sortie et le nombre d'octets lus.
pub fn inflate(data: &[u8], limit: usize) -> Result<(Vec<u8>, usize), Error> {
    let mut b = Bits::new(data);
    let mut out = Vec::new();
    let mut fixed: Option<(Huffman, Huffman)> = None;
    loop {
        let last = b.bits(1)?;
        match b.bits(2)? {
            0 => {
                b.align();
                let len = b.bits(16)?;
                let nlen = b.bits(16)?;
                if len != !nlen & 0xffff {
                    return Err(Error::BadStoredLength);
                }
                if out.len() + len as usize > limit {
                    return Err(Error::TooLarge);
                }
                b.take_bytes(len as usize, &mut out)?;
            }
            1 => {
                let (lit, dist) = fixed.get_or_insert_with(fixed_codes);
                codes(&mut b, &mut out, lit, dist, limit)?;
            }
            2 => {
                let (lit, dist) = dynamic_codes(&mut b)?;
                codes(&mut b, &mut out, &lit, &dist, limit)?;
            }
            _ => return Err(Error::BadBlockType),
        }
        if last == 1 {
            b.align();
            return Ok((out, b.byte_pos()));
        }
    }
}

/// Adler-32 (RFC 1950).
pub fn adler32(data: &[u8]) -> u32 {
    const M: u32 = 65521;
    let (mut a, mut b) = (1u32, 0u32);
    // 5552 octets au plus entre deux réductions : pas de débordement de `b`.
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= M;
        b %= M;
    }
    (b << 16) | a
}

/// Décompresse un flux zlib (en-tête, DEFLATE, Adler-32 vérifiée), sortie bornée à `limit`.
pub fn zlib_decompress(data: &[u8], limit: usize) -> Result<Vec<u8>, Error> {
    if data.len() < 2 {
        return Err(Error::Truncated);
    }
    let (cmf, flg) = (data[0], data[1]);
    if cmf & 15 != 8 || cmf >> 4 > 7 || ((cmf as u16) << 8 | flg as u16) % 31 != 0 || flg & 0x20 != 0 {
        return Err(Error::BadZlibHeader);
    }
    let (out, used) = inflate(&data[2..], limit)?;
    let tail = &data[2 + used..];
    if tail.len() < 4 {
        return Err(Error::Truncated);
    }
    if u32::from_be_bytes([tail[0], tail[1], tail[2], tail[3]]) != adler32(&out) {
        return Err(Error::BadChecksum);
    }
    Ok(out)
}
