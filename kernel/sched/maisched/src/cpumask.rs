//! Masque de cœurs atomique (256 cœurs), sans verrou.
//!
//! Sert à publier l'état des cœurs (inactif, en attente active…) pour que le chemin de réveil
//! trouve un cœur libre en lisant quatre mots au lieu de parcourir toutes les files
//! (Linux `idle_cpus_mask` / `sched_domain_shared::has_idle_cores`).

use core::sync::atomic::{AtomicU64, Ordering};

pub const MASK_CPUS: usize = 256;
const WORDS: usize = MASK_CPUS / 64;

pub struct AtomicCpuMask {
    words: [AtomicU64; WORDS],
}

impl AtomicCpuMask {
    pub const fn new() -> AtomicCpuMask {
        AtomicCpuMask { words: [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)] }
    }

    #[inline]
    pub fn set(&self, cpu: usize) {
        if cpu < MASK_CPUS {
            self.words[cpu / 64].fetch_or(1 << (cpu % 64), Ordering::AcqRel);
        }
    }

    #[inline]
    pub fn clear(&self, cpu: usize) {
        if cpu < MASK_CPUS {
            self.words[cpu / 64].fetch_and(!(1 << (cpu % 64)), Ordering::AcqRel);
        }
    }

    #[inline]
    pub fn test(&self, cpu: usize) -> bool {
        cpu < MASK_CPUS && self.words[cpu / 64].load(Ordering::Acquire) & (1 << (cpu % 64)) != 0
    }

    /// Prend atomiquement le bit (`true` si on l'a effacé nous-mêmes) : deux réveils simultanés ne
    /// choisissent pas le même cœur inactif.
    #[inline]
    pub fn test_and_clear(&self, cpu: usize) -> bool {
        if cpu >= MASK_CPUS {
            return false;
        }
        let bit = 1u64 << (cpu % 64);
        self.words[cpu / 64].fetch_and(!bit, Ordering::AcqRel) & bit != 0
    }

    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|w| w.load(Ordering::Acquire) == 0)
    }

    pub fn count(&self) -> u32 {
        self.words.iter().map(|w| w.load(Ordering::Relaxed).count_ones()).sum()
    }
}
