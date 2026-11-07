//! Entropie pour l'ASLR : RDRAND si présent, sinon TSC mélangé (splitmix64).

use core::sync::atomic::{AtomicU64, Ordering};

static STATE: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);

fn has_rdrand() -> bool {
    // SAFETY : CPUID toujours disponible en mode long.
    unsafe { core::arch::x86_64::__cpuid(1).ecx & (1 << 30) != 0 }
}

/// 64 bits aléatoires.
pub fn random_u64() -> u64 {
    if has_rdrand() {
        for _ in 0..10 {
            let mut v = 0u64;
            // SAFETY : RDRAND est présent.
            if unsafe { core::arch::x86_64::_rdrand64_step(&mut v) } == 1 {
                return v;
            }
        }
    }
    // SAFETY : lecture du TSC.
    let tsc = unsafe { core::arch::x86_64::_rdtsc() };
    let mut z = STATE.fetch_add(0x9E37_79B9_7F4A_7C15 ^ tsc, Ordering::Relaxed);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Décalage aléatoire de `bits` bits, en pages.
pub fn random_pages(bits: u32) -> usize {
    (random_u64() as usize & ((1usize << bits) - 1)) * kernel_config::memory::PAGE_SIZE
}
