//! Hachage des noms des répertoires indexés (`fs/ext4/hash.c` de Linux), bit pour bit.

pub const DX_HASH_LEGACY: u8 = 0;
pub const DX_HASH_HALF_MD4: u8 = 1;
pub const DX_HASH_TEA: u8 = 2;
pub const DX_HASH_LEGACY_UNSIGNED: u8 = 3;
pub const DX_HASH_HALF_MD4_UNSIGNED: u8 = 4;
pub const DX_HASH_TEA_UNSIGNED: u8 = 5;

fn dx_hack_hash(name: &[u8], signed: bool) -> u32 {
    let (mut hash0, mut hash1): (u32, u32) = (0x12a3_fe2d, 0x37ab_e8f9);
    for &c in name {
        let v = if signed { c as i8 as i32 } else { c as i32 };
        let mut hash = hash1.wrapping_add(hash0 ^ (v.wrapping_mul(7_152_373) as u32));
        if hash & 0x8000_0000 != 0 {
            hash = hash.wrapping_sub(0x7fff_ffff);
        }
        hash1 = hash0;
        hash0 = hash;
    }
    hash0 << 1
}

/// `str2hashbuf_signed` / `str2hashbuf_unsigned` : remplit `out` (`num` mots) avec le nom.
fn str2hashbuf(msg: &[u8], out: &mut [u32], num: usize, signed: bool) {
    let len = msg.len();
    let mut pad = (len as u32) | ((len as u32) << 8);
    pad |= pad << 16;
    let mut val = pad;
    let take = len.min(num * 4);
    let mut o = 0usize;
    let mut left = num as isize;
    for (i, &c) in msg[..take].iter().enumerate() {
        let v = if signed { c as i8 as i32 as u32 } else { c as u32 };
        val = v.wrapping_add(val << 8);
        if i % 4 == 3 {
            out[o] = val;
            o += 1;
            val = pad;
            left -= 1;
        }
    }
    left -= 1;
    if left >= 0 {
        out[o] = val;
        o += 1;
    }
    while left > 0 {
        left -= 1;
        out[o] = pad;
        o += 1;
    }
}

fn tea_transform(buf: &mut [u32; 4], inp: &[u32]) {
    const DELTA: u32 = 0x9E37_79B9;
    let mut sum: u32 = 0;
    let (mut b0, mut b1) = (buf[0], buf[1]);
    let (a, b, c, d) = (inp[0], inp[1], inp[2], inp[3]);
    for _ in 0..16 {
        sum = sum.wrapping_add(DELTA);
        b0 = b0.wrapping_add(
            ((b1 << 4).wrapping_add(a)) ^ (b1.wrapping_add(sum)) ^ ((b1 >> 5).wrapping_add(b)),
        );
        b1 = b1.wrapping_add(
            ((b0 << 4).wrapping_add(c)) ^ (b0.wrapping_add(sum)) ^ ((b0 >> 5).wrapping_add(d)),
        );
    }
    buf[0] = buf[0].wrapping_add(b0);
    buf[1] = buf[1].wrapping_add(b1);
}

fn half_md4_transform(buf: &mut [u32; 4], x: &[u32]) {
    #[inline]
    fn f(x: u32, y: u32, z: u32) -> u32 {
        z ^ (x & (y ^ z))
    }
    #[inline]
    fn g(x: u32, y: u32, z: u32) -> u32 {
        (x & y).wrapping_add((x ^ y) & z)
    }
    #[inline]
    fn h(x: u32, y: u32, z: u32) -> u32 {
        x ^ y ^ z
    }
    const K2: u32 = 0x5A82_7999;
    const K3: u32 = 0x6ED9_EBA1;
    let (mut a, mut b, mut c, mut d) = (buf[0], buf[1], buf[2], buf[3]);
    macro_rules! round {
        ($f:ident, $a:ident, $b:ident, $c:ident, $d:ident, $x:expr, $s:expr) => {
            $a = $a.wrapping_add($f($b, $c, $d)).wrapping_add($x);
            $a = $a.rotate_left($s);
        };
    }
    round!(f, a, b, c, d, x[0], 3);
    round!(f, d, a, b, c, x[1], 7);
    round!(f, c, d, a, b, x[2], 11);
    round!(f, b, c, d, a, x[3], 19);
    round!(f, a, b, c, d, x[4], 3);
    round!(f, d, a, b, c, x[5], 7);
    round!(f, c, d, a, b, x[6], 11);
    round!(f, b, c, d, a, x[7], 19);

    round!(g, a, b, c, d, x[1].wrapping_add(K2), 3);
    round!(g, d, a, b, c, x[3].wrapping_add(K2), 5);
    round!(g, c, d, a, b, x[5].wrapping_add(K2), 9);
    round!(g, b, c, d, a, x[7].wrapping_add(K2), 13);
    round!(g, a, b, c, d, x[0].wrapping_add(K2), 3);
    round!(g, d, a, b, c, x[2].wrapping_add(K2), 5);
    round!(g, c, d, a, b, x[4].wrapping_add(K2), 9);
    round!(g, b, c, d, a, x[6].wrapping_add(K2), 13);

    round!(h, a, b, c, d, x[3].wrapping_add(K3), 3);
    round!(h, d, a, b, c, x[7].wrapping_add(K3), 9);
    round!(h, c, d, a, b, x[2].wrapping_add(K3), 11);
    round!(h, b, c, d, a, x[6].wrapping_add(K3), 15);
    round!(h, a, b, c, d, x[1].wrapping_add(K3), 3);
    round!(h, d, a, b, c, x[5].wrapping_add(K3), 9);
    round!(h, c, d, a, b, x[0].wrapping_add(K3), 11);
    round!(h, b, c, d, a, x[4].wrapping_add(K3), 15);

    buf[0] = buf[0].wrapping_add(a);
    buf[1] = buf[1].wrapping_add(b);
    buf[2] = buf[2].wrapping_add(c);
    buf[3] = buf[3].wrapping_add(d);
}

/// Hachage principal (bit de poids faible à 0) d'un nom pour la version `version`.
/// `None` si la version est inconnue (SipHash des dossiers chiffrés et insensibles à la casse).
pub fn dx_hash(name: &[u8], version: u8, seed: &[u32; 4]) -> Option<u32> {
    let mut buf: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    if seed.iter().any(|&s| s != 0) {
        buf = *seed;
    }
    let mut inp = [0u32; 8];
    let hash = match version {
        DX_HASH_LEGACY => dx_hack_hash(name, true),
        DX_HASH_LEGACY_UNSIGNED => dx_hack_hash(name, false),
        DX_HASH_HALF_MD4 | DX_HASH_HALF_MD4_UNSIGNED => {
            let signed = version == DX_HASH_HALF_MD4;
            for p in name.chunks(32).enumerate().map(|(i, _)| &name[i * 32..]) {
                str2hashbuf(p, &mut inp, 8, signed);
                half_md4_transform(&mut buf, &inp);
            }
            buf[1]
        }
        DX_HASH_TEA | DX_HASH_TEA_UNSIGNED => {
            let signed = version == DX_HASH_TEA;
            for p in name.chunks(16).enumerate().map(|(i, _)| &name[i * 16..]) {
                str2hashbuf(p, &mut inp, 4, signed);
                tea_transform(&mut buf, &inp[..4]);
            }
            buf[0]
        }
        _ => return None,
    };
    let mut h = hash & !1;
    if h == 0x7fff_ffff << 1 {
        h = (0x7fff_ffff - 1) << 1;
    }
    Some(h)
}
