//! Tests hôte : `cargo test` dans `kernel/mm/llfree` (cible hôte, `std`, vrais threads).

use super::*;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering::SeqCst};
use std::sync::Arc;
use std::vec::Vec;

/// Alloue une zone de métadonnées alignée et la fait fuir (durée de vie `'static`).
fn meta(frames: usize, locals: usize) -> &'static mut [u8] {
    let size = LLFree::metadata_size(frames, locals);
    let layout = std::alloc::Layout::from_size_align(size, LLFree::METADATA_ALIGN).unwrap();
    unsafe { core::slice::from_raw_parts_mut(std::alloc::alloc_zeroed(layout), size) }
}

fn new_all_free(base: usize, frames: usize, locals: usize) -> LLFree<'static> {
    let ll = LLFree::new(meta(frames, locals), base, frames, locals).unwrap();
    ll.free(base, frames).unwrap();
    ll
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

#[test]
fn metadata_is_tiny() {
    // 64 Gio de RAM, 256 CPU : ~2 Mio de bitfield + quelques dizaines de Kio.
    let frames = 64 << 18;
    let size = LLFree::metadata_size(frames, 256);
    assert!(size < (2 << 20) + (256 << 10), "taille {size}");
}

#[test]
fn starts_fully_allocated_then_freed() {
    let base = 512 * 4;
    let frames = FRAMES_PER_TREE + 777;
    let ll = LLFree::new(meta(frames, 4), base, frames, 4).unwrap();
    assert_eq!(ll.free_frames(), 0);
    assert_eq!(ll.alloc(0, 1), Err(Error::NoMemory));
    ll.free(base, frames).unwrap();
    assert_eq!(ll.free_frames(), frames);
    ll.validate().unwrap();
}

#[test]
fn exhaust_single_frames() {
    let base = 512;
    let frames = 3 * FRAMES_PER_CHILD + 100;
    let ll = new_all_free(base, frames, 2);
    let mut seen = std::collections::HashSet::new();
    for i in 0..frames {
        let f = ll.alloc(i % 2, 1).unwrap();
        assert!(f >= base && f < base + frames);
        assert!(seen.insert(f), "frame {f:#x} allouée deux fois");
    }
    assert_eq!(ll.alloc(0, 1), Err(Error::NoMemory));
    assert_eq!(ll.free_frames(), 0);
    for f in seen {
        ll.free(f, 1).unwrap();
    }
    assert_eq!(ll.free_frames(), frames);
    ll.validate().unwrap();
}

#[test]
fn small_power_of_two_are_naturally_aligned() {
    let ll = new_all_free(0, FRAMES_PER_TREE, 1);
    for n in [2usize, 4, 8, 16, 32, 64] {
        let f = ll.alloc(0, n).unwrap();
        assert_eq!(f % n, 0, "allocation de {n} non alignée : {f:#x}");
    }
    ll.validate().unwrap();
}

#[test]
fn odd_sizes_cross_words() {
    let ll = new_all_free(0, FRAMES_PER_CHILD * 2, 1);
    // Fragmente le premier mot pour forcer une recherche non alignée multi-mots.
    let a = ll.alloc(0, 1).unwrap();
    let b = ll.alloc(0, 100).unwrap();
    assert!(b > a);
    let c = ll.alloc(0, 300).unwrap();
    assert!(c >= b + 100 || c + 300 <= b);
    ll.free(b, 100).unwrap();
    ll.free(a, 1).unwrap();
    ll.free(c, 300).unwrap();
    assert_eq!(ll.free_frames(), FRAMES_PER_CHILD * 2);
    ll.validate().unwrap();
}

#[test]
fn large_allocations_are_2m_aligned_and_tail_is_returned() {
    let base = 512 * 10;
    let frames = 8 * FRAMES_PER_CHILD;
    let ll = new_all_free(base, frames, 1);
    let f = ll.alloc(0, 513).unwrap();
    assert_eq!(f % FRAMES_PER_CHILD, 0);
    assert_eq!(ll.free_frames(), frames - 513);
    let h = ll.alloc_huge(2).unwrap();
    assert_eq!(h % FRAMES_PER_CHILD, 0);
    assert_eq!(ll.free_frames(), frames - 513 - 1024);
    ll.free(f, 513).unwrap();
    ll.free(h, 1024).unwrap();
    assert_eq!(ll.free_huge(), 8);
    ll.validate().unwrap();
}

#[test]
fn small_allocs_preserve_huge_frames() {
    let ll = new_all_free(0, 4 * FRAMES_PER_CHILD, 1);
    for _ in 0..600 {
        ll.alloc(0, 1).unwrap();
    }
    // 600 frames tiennent dans 2 blocs : il doit rester 2 blocs de 2 Mio intacts.
    assert_eq!(ll.free_huge(), 2);
}

#[test]
fn alloc_at_and_conflicts() {
    let ll = new_all_free(0, 4 * FRAMES_PER_CHILD, 1);
    ll.alloc_at(500, 600).unwrap(); // traverse deux enfants, dont un en entier
    assert!(!ll.is_free(500) && !ll.is_free(1099) && ll.is_free(499) && ll.is_free(1100));
    // Conflit partiel : rien ne doit être pris.
    let before = ll.free_frames();
    assert_eq!(ll.alloc_at(1000, 200), Err(Error::NotFree));
    assert_eq!(ll.free_frames(), before);
    assert!(ll.is_free(1100) && ll.is_free(1199));
    assert_eq!(ll.alloc_at(4 * FRAMES_PER_CHILD - 1, 2), Err(Error::OutOfRange));
    ll.free(500, 600).unwrap();
    ll.validate().unwrap();
}

#[test]
fn double_free_is_detected_and_counters_stay_exact() {
    let ll = new_all_free(0, FRAMES_PER_CHILD, 1);
    let f = ll.alloc(0, 4).unwrap();
    ll.free(f, 2).unwrap();
    assert_eq!(ll.free(f, 4), Err(Error::DoubleFree { frame: f, count: 2 }));
    assert_eq!(ll.free_frames(), FRAMES_PER_CHILD);
    ll.validate().unwrap();
}

#[test]
fn holes_stay_allocated() {
    let ll = LLFree::new(meta(FRAMES_PER_TREE, 1), 0, FRAMES_PER_TREE, 1).unwrap();
    ll.free(0, 1000).unwrap();
    ll.free(2000, 1000).unwrap();
    let mut runs = Vec::new();
    ll.for_each_free_run(|s, l| {
        runs.push((s, l));
        true
    });
    assert_eq!(runs, [(0, 1000), (2000, 1000)]);
    for _ in 0..2000 {
        let f = ll.alloc(0, 1).unwrap();
        assert!(f < 1000 || (2000..3000).contains(&f), "frame {f} prise dans un trou");
    }
    assert_eq!(ll.alloc(0, 1), Err(Error::NoMemory));
}

/// Stress multi-threads : tailles mélangées, allocations et libérations concurrentes.
/// Une table d'ombre vérifie qu'aucune frame n'est jamais donnée à deux propriétaires.
#[test]
fn concurrent_stress_no_overlap() {
    const THREADS: usize = 8;
    const ITERS: usize = 30_000;
    let base = 512 * 7;
    let frames = 4 * FRAMES_PER_TREE + 333;
    let ll = Arc::new(new_all_free(base, frames, THREADS));
    let owner: Arc<Vec<AtomicU8>> = Arc::new((0..frames).map(|_| AtomicU8::new(0)).collect());
    let failures = Arc::new(AtomicUsize::new(0));
    let sizes = [1usize, 1, 1, 1, 1, 2, 3, 4, 7, 8, 33, 64, 100, 300, 512, 700, 1500];

    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let ll = ll.clone();
            let owner = owner.clone();
            let failures = failures.clone();
            std::thread::spawn(move || {
                let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ (t as u64 + 1).wrapping_mul(0x2545_F491_4F6C_DD1D));
                let mut held: Vec<(usize, usize)> = Vec::new();
                for _ in 0..ITERS {
                    if held.len() < 16 && rng.below(3) != 0 {
                        let n = sizes[rng.below(sizes.len())];
                        match ll.alloc(t, n) {
                            Ok(f) => {
                                for i in f..f + n {
                                    assert_eq!(owner[i - base].swap(1, SeqCst), 0, "frame {i:#x} déjà possédée");
                                }
                                held.push((f, n));
                            }
                            Err(Error::NoMemory) if n > 64 => {
                                failures.fetch_add(1, SeqCst);
                            }
                            Err(Error::NoMemory) => panic!("échec d'une petite allocation ({n}) avec de la mémoire libre"),
                            Err(e) => panic!("{e}"),
                        }
                    } else if !held.is_empty() {
                        let (f, n) = held.swap_remove(rng.below(held.len()));
                        for i in f..f + n {
                            owner[i - base].store(0, SeqCst);
                        }
                        ll.free(f, n).unwrap();
                    }
                }
                for (f, n) in held {
                    for i in f..f + n {
                        owner[i - base].store(0, SeqCst);
                    }
                    ll.free(f, n).unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(ll.free_frames(), frames);
    ll.validate().unwrap();
    // Au plus 8 x 16 allocations détenues (~40 % de la mémoire en moyenne) : seules quelques
    // grandes allocations contiguës peuvent échouer par fragmentation.
    let f = failures.load(SeqCst);
    std::println!("échecs de grandes allocations (fragmentation) : {f}");
    assert!(f < THREADS * ITERS / 100, "trop d'échecs : {f}");
}

/// Stress de contention sur une seule frame logique : tous les threads se battent pour
/// très peu de mémoire. Vérifie l'absence de perte (compteurs exacts à la fin).
#[test]
fn concurrent_scarce_memory() {
    const THREADS: usize = 8;
    let frames = FRAMES_PER_CHILD;
    let ll = Arc::new(new_all_free(0, frames, THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let ll = ll.clone();
            std::thread::spawn(move || {
                let mut rng = Rng(t as u64 * 7919 + 1);
                let mut held: Vec<(usize, usize)> = Vec::new();
                let mut got = 0usize;
                for _ in 0..50_000 {
                    if held.len() < 40 && rng.below(2) == 0 {
                        let n = 1 + rng.below(4);
                        if let Ok(f) = ll.alloc(t, n) {
                            held.push((f, n));
                            got += 1;
                        }
                    } else if let Some((f, n)) = held.pop() {
                        ll.free(f, n).unwrap();
                    }
                }
                for (f, n) in held {
                    ll.free(f, n).unwrap();
                }
                got
            })
        })
        .collect();
    let total: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert!(total > 0);
    assert_eq!(ll.free_frames(), frames);
    ll.validate().unwrap();
}
