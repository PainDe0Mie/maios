//! Sommes de contrôle d'ext4.
//!
//! * **CRC32C** (Castagnoli, polynôme réfléchi `0x82F63B78`) pour `metadata_csum` et JBD2 v3.
//!   Comme `crc32c_le()` de Linux, [`crc32c`] est la mise à jour *brute* : ni inversion
//!   initiale ni finale (ext4 part de `~0` ou d'une graine et compare le résultat tel quel).
//!   Chemin matériel : instruction `crc32` de SSE4.2, qui ne touche qu'aux registres généraux et
//!   s'emploie donc dans un noyau compilé sans SSE ; repli logiciel « slicing-by-8 » (Intel,
//!   Kounavis et Berry 2005), 8 octets par tour avec 8 tables de 1 Kio.
//! * **CRC16** (polynôme `0x8005` réfléchi) pour l'ancienne somme des descripteurs de groupes
//!   (`gdt_csum`, `uninit_bg`).

const POLY32C: u32 = 0x82F6_3B78;

const fn make_tables() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ POLY32C } else { c >> 1 };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut s = 1;
    while s < 8 {
        let mut i = 0;
        while i < 256 {
            let prev = t[s - 1][i];
            t[s][i] = (prev >> 8) ^ t[0][(prev & 0xFF) as usize];
            i += 1;
        }
        s += 1;
    }
    t
}

static T32C: [[u32; 256]; 8] = make_tables();

fn crc32c_soft(mut crc: u32, data: &[u8]) -> u32 {
    let mut chunks = data.chunks_exact(8);
    for c in &mut chunks {
        let lo = u32::from_le_bytes([c[0], c[1], c[2], c[3]]) ^ crc;
        let hi = u32::from_le_bytes([c[4], c[5], c[6], c[7]]);
        crc = T32C[7][(lo & 0xFF) as usize]
            ^ T32C[6][((lo >> 8) & 0xFF) as usize]
            ^ T32C[5][((lo >> 16) & 0xFF) as usize]
            ^ T32C[4][(lo >> 24) as usize]
            ^ T32C[3][(hi & 0xFF) as usize]
            ^ T32C[2][((hi >> 8) & 0xFF) as usize]
            ^ T32C[1][((hi >> 16) & 0xFF) as usize]
            ^ T32C[0][(hi >> 24) as usize];
    }
    for &b in chunks.remainder() {
        crc = (crc >> 8) ^ T32C[0][((crc ^ b as u32) & 0xFF) as usize];
    }
    crc
}

#[cfg(target_arch = "x86_64")]
mod hw {
    use core::sync::atomic::{AtomicU8, Ordering};

    /// 0 : pas encore sondé ; 1 : absent ; 2 : présent.
    static STATE: AtomicU8 = AtomicU8::new(0);

    #[inline]
    pub fn available() -> bool {
        match STATE.load(Ordering::Relaxed) {
            2 => true,
            1 => false,
            _ => {
                // SAFETY : `cpuid` existe sur tout processeur x86_64.
                #[allow(unused_unsafe)]
                let ecx = unsafe { core::arch::x86_64::__cpuid(1) }.ecx;
                let ok = ecx & (1 << 20) != 0; // SSE4.2
                STATE.store(if ok { 2 } else { 1 }, Ordering::Relaxed);
                ok
            }
        }
    }

    /// SAFETY : le processeur doit avoir SSE4.2 ([`available`]).
    pub unsafe fn crc32c(crc: u32, data: &[u8]) -> u32 {
        let mut c = crc as u64;
        let mut chunks = data.chunks_exact(8);
        for ch in &mut chunks {
            let v = u64::from_le_bytes([ch[0], ch[1], ch[2], ch[3], ch[4], ch[5], ch[6], ch[7]]);
            core::arch::asm!("crc32 {c}, {v}", c = inout(reg) c, v = in(reg) v, options(pure, nomem, nostack));
        }
        let mut c = c as u32;
        for &b in chunks.remainder() {
            core::arch::asm!("crc32 {c:e}, {b}", c = inout(reg) c, b = in(reg_byte) b, options(pure, nomem, nostack));
        }
        c
    }
}

/// Mise à jour brute du CRC32C (convention de `crc32c_le()` de Linux).
#[inline]
pub fn crc32c(crc: u32, data: &[u8]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    if hw::available() {
        // SAFETY : SSE4.2 vérifié juste au-dessus.
        return unsafe { hw::crc32c(crc, data) };
    }
    crc32c_soft(crc, data)
}

/// CRC32C d'un `u32` petit-boutiste (numéro d'inode, de groupe, génération).
#[inline]
pub fn crc32c_u32(crc: u32, v: u32) -> u32 {
    crc32c(crc, &v.to_le_bytes())
}

/// CRC16 de `lib/crc16.c` de Linux (polynôme `0x8005` réfléchi, sans inversion finale).
pub fn crc16(mut crc: u16, data: &[u8]) -> u16 {
    for &b in data {
        crc ^= b as u16;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xA001 } else { crc >> 1 };
        }
    }
    crc
}

#[cfg(test)]
pub(crate) fn crc32c_soft_for_tests(crc: u32, data: &[u8]) -> u32 {
    crc32c_soft(crc, data)
}
