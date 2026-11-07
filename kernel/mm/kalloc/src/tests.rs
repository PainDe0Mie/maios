//! Tests hôte : chaque fil joue un « cœur » ; les segments viennent de l'allocateur de l'hôte.

use crate::class::{self, NCLASS, SIZES};
use crate::*;
use std::alloc::{alloc as host_alloc, dealloc as host_dealloc, Layout};
use std::boxed::Box;
use std::collections::BTreeMap;
use std::sync::{mpsc, Arc, Barrier};
use std::vec::Vec;

fn seg_layout() -> Layout {
    Layout::from_size_align(SEGMENT_SIZE, SEGMENT_SIZE).unwrap()
}

fn new_kalloc() -> &'static Kalloc {
    let k: &'static Kalloc = Box::leak(Box::new(Kalloc::new()));
    k.init(0x1234_5678_9ABC_DEF0);
    k
}

unsafe fn give_segment(k: &Kalloc) {
    let p = host_alloc(seg_layout());
    assert!(!p.is_null());
    k.add_segment(p as usize, [0; OS_WORDS]);
}

unsafe fn trim_all(k: &Kalloc) {
    while let Some(h) = k.trim() {
        host_dealloc(h.base as *mut u8, seg_layout());
    }
}

unsafe fn a(k: &Kalloc, cpu: usize, size: usize, align: usize) -> *mut u8 {
    loop {
        match k.alloc(cpu, size, align) {
            Alloc::Ok(p) => {
                assert_eq!(p as usize % align, 0, "alignement {align} non respecte pour {size} o");
                return p;
            }
            Alloc::NeedSegment => give_segment(k),
            Alloc::Large => return host_alloc(Layout::from_size_align(size, align).unwrap()),
        }
    }
}

unsafe fn f(k: &Kalloc, cpu: usize, p: *mut u8, size: usize, align: usize) {
    match k.free(cpu, p, size, align) {
        Free::Done => {}
        Free::Trim => trim_all(k),
        Free::Large => host_dealloc(p, Layout::from_size_align(size, align).unwrap()),
    }
}

/// Remplit l'objet d'un motif dépendant de son identifiant.
unsafe fn fill(p: *mut u8, size: usize, id: u64) {
    let b = (id as u8) | 1;
    std::ptr::write_bytes(p, b, size);
}

unsafe fn check(p: *mut u8, size: usize, id: u64) {
    let b = (id as u8) | 1;
    for i in 0..size {
        assert_eq!(*p.add(i), b, "objet {id} ecrase a l'octet {i}/{size}");
    }
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
    /// Taille : surtout petite, parfois jusqu'à 160 Kio (au-delà des classes).
    fn size(&mut self) -> usize {
        match self.below(100) {
            0..=69 => 1 + self.below(128),
            70..=94 => 1 + self.below(4096),
            95..=97 => 1 + self.below(MAX_SMALL),
            98 => MAX_SMALL + 1 + self.below(MAX_LARGE - MAX_SMALL),
            _ => MAX_LARGE + 1 + self.below(64 * 1024),
        }
    }
    fn align(&mut self, size: usize) -> usize {
        // Une `Layout` a une taille multiple de son alignement.
        let mut al = 1usize << self.below(13);
        while al > 1 && size % al != 0 {
            al >>= 1;
        }
        al
    }
}

#[test]
fn classes_cover_all_sizes() {
    for c in 1..NCLASS {
        assert!(SIZES[c] > SIZES[c - 1]);
    }
    assert_eq!(SIZES[NCLASS - 1] as usize, MAX_SMALL);
    for size in 1..=MAX_SMALL {
        let c = class::of_size(size);
        let bs = SIZES[c] as usize;
        assert!(bs >= size, "classe trop petite pour {size}");
        if c > 0 {
            assert!((SIZES[c - 1] as usize) < size, "classe non minimale pour {size}");
        }
        // Perte interne bornée (25 % au-delà de 64 o).
        if size > 64 {
            assert!(bs * 4 <= size * 5 + 4, "perte trop forte pour {size} : {bs}");
        }
        let mut al = 1;
        while al <= MAX_ALIGN {
            if size % al == 0 {
                let c = class::lookup(size, al).expect("classe introuvable");
                let bs = SIZES[c] as usize;
                assert!(bs >= size && bs % al.min(8192) == 0, "size {size} align {al} -> {bs}");
            }
            al <<= 1;
        }
    }
    assert!(class::lookup(MAX_SMALL + 1, 8).is_none());
    assert!(class::lookup(64, 8192).is_none());
}

/// Un seul cœur, 300 000 opérations aléatoires, motifs vérifiés, absence de chevauchement.
#[test]
fn single_cpu_model() {
    let k = new_kalloc();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut live: Vec<(*mut u8, usize, usize, u64)> = Vec::new();
    let mut ranges: BTreeMap<usize, usize> = BTreeMap::new();
    unsafe {
        for id in 0..300_000u64 {
            if live.is_empty() || rng.below(100) < 55 {
                let size = rng.size();
                let al = rng.align(size);
                let p = a(k, 0, size, al);
                let start = p as usize;
                if let Some((&s, &l)) = ranges.range(..=start).next_back() {
                    assert!(s + l <= start, "chevauchement");
                }
                if let Some((&s, _)) = ranges.range(start..).next() {
                    assert!(start + size <= s, "chevauchement");
                }
                ranges.insert(start, size);
                fill(p, size, id);
                live.push((p, size, al, id));
            } else {
                let i = rng.below(live.len());
                let (p, size, al, id) = live.swap_remove(i);
                check(p, size, id);
                ranges.remove(&(p as usize));
                f(k, 0, p, size, al);
            }
        }
        for (p, size, al, id) in live.drain(..) {
            check(p, size, id);
            f(k, 0, p, size, al);
        }
        k.collect(0);
        trim_all(k);
    }
    let st = k.stats();
    assert_eq!(st.allocs, st.frees);
    assert_eq!(st.pages_fresh, st.pages_retired, "pages non rendues apres collecte");
    // Seul le segment « maison » reste engagé (plus la réserve plafonnée).
    assert!(st.segments <= 1 + 8, "segments engages : {}", st.segments);
}

/// 8 cœurs : chacun alloue, envoie la moitié de ses objets au voisin qui les libère (libérations
/// distantes), puis tous font la maintenance : rien ne doit rester.
#[test]
fn cross_cpu_frees() {
    const CPUS: usize = 8;
    const N: usize = 40_000;
    let k = new_kalloc();
    let barrier = Arc::new(Barrier::new(CPUS));
    let mut txs = Vec::new();
    let mut rxs = Vec::new();
    for _ in 0..CPUS {
        let (tx, rx) = mpsc::channel::<Vec<(usize, usize, u64)>>();
        txs.push(tx);
        rxs.push(Some(rx));
    }
    let mut handles = Vec::new();
    for cpu in 0..CPUS {
        let tx = txs[(cpu + 1) % CPUS].clone();
        let rx = rxs[cpu].take().unwrap();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || unsafe {
            let mut rng = Rng(0xABCD_0000 + cpu as u64 * 7919);
            let mut mine = Vec::new();
            let mut batch = Vec::new();
            for i in 0..N {
                let size = 1 + rng.below(if i % 50 == 0 { 20_000 } else { 512 });
                let id = (cpu * N + i) as u64;
                let p = a(k, cpu, size, 8);
                fill(p, size, id);
                if rng.below(2) == 0 {
                    batch.push((p as usize, size, id));
                    if batch.len() == 256 {
                        tx.send(std::mem::take(&mut batch)).unwrap();
                    }
                } else {
                    mine.push((p as usize, size, id));
                }
                // Libérations entrelacées : les pages pleines passent par le chemin différé.
                if let Ok(v) = rx.try_recv() {
                    for (p, size, id) in v {
                        check(p as *mut u8, size, id);
                        f(k, cpu, p as *mut u8, size, 8);
                    }
                }
            }
            tx.send(batch).unwrap();
            drop(tx);
            for (p, size, id) in mine {
                check(p as *mut u8, size, id);
                f(k, cpu, p as *mut u8, size, 8);
            }
            barrier.wait();
            while let Ok(v) = rx.try_recv() {
                for (p, size, id) in v {
                    check(p as *mut u8, size, id);
                    f(k, cpu, p as *mut u8, size, 8);
                }
            }
            barrier.wait();
            k.collect(cpu);
        }));
    }
    drop(txs);
    for h in handles {
        h.join().unwrap();
    }
    let st = k.stats();
    assert_eq!(st.allocs, (CPUS * N) as u64);
    assert_eq!(st.frees + st.remote_frees, st.allocs);
    assert!(st.remote_frees > 0);
    assert_eq!(st.pages_fresh, st.pages_retired, "pages non rendues : {st:?}");
}

/// Pages pleines libérées par un autre cœur : le protocole différé doit les remettre en service
/// sans prendre de nouveau segment.
#[test]
fn delayed_free_recycles_full_pages() {
    let k = new_kalloc();
    const N: usize = 20_000; // 64 o : ~20 pages pleines
    let ptrs: Vec<usize> = (0..N).map(|_| unsafe { a(k, 0, 64, 8) as usize }).collect();
    let before = k.heap_stats(0).unwrap();
    let ptrs2 = ptrs.clone();
    std::thread::spawn(move || unsafe {
        // Le cœur 1 a son propre tas (les libérations distantes sont comptées par le libérateur).
        let own = a(k, 1, 16, 8);
        f(k, 1, own, 16, 8);
        for p in ptrs2 {
            f(k, 1, p as *mut u8, 64, 8);
        }
    })
    .join()
    .unwrap();
    let mid = k.stats();
    assert!(mid.remote_frees as usize >= N);
    let again: Vec<usize> = (0..N).map(|_| unsafe { a(k, 0, 64, 8) as usize }).collect();
    let after = k.heap_stats(0).unwrap();
    assert!(after.delayed_frees > 0, "le chemin differe n'a pas servi : {after:?}");
    assert_eq!(after.segments_taken, before.segments_taken, "nouveau segment pris au lieu de recycler");
    assert_eq!(after.pages_fresh, before.pages_fresh, "nouvelles pages au lieu de recycler");
    unsafe {
        for p in again {
            f(k, 0, p as *mut u8, 64, 8);
        }
        k.collect(0);
    }
}

/// Cycles alloue-tout / libère-tout : la mémoire engagée reste bornée et revient au système.
#[test]
fn memory_is_returned() {
    let k = new_kalloc();
    k.set_pool_cap(2);
    let mut rng = Rng(42);
    let mut peak = 0;
    for round in 0..20 {
        let v: Vec<(usize, usize)> = (0..30_000)
            .map(|_| {
                let s = 1 + rng.below(2048);
                (unsafe { a(k, 0, s, 8) } as usize, s)
            })
            .collect();
        peak = peak.max(k.stats().segments);
        for (p, s) in v {
            unsafe { f(k, 0, p as *mut u8, s, 8) };
        }
        unsafe {
            k.collect(0);
            trim_all(k);
        }
        let st = k.stats();
        assert!(st.segments <= 1 + 2, "tour {round} : {} segments engages", st.segments);
    }
    assert!(peak >= 10, "le test n'a pas sollicite assez de memoire ({peak})");
}

/// Pages larges (128 Kio à 1 Mio) : libérées localement et à distance, la mémoire revient.
#[test]
fn large_pages_local_and_remote() {
    let k = new_kalloc();
    k.set_pool_cap(1);
    unsafe {
        let sizes = [MAX_SMALL + 1, 200_000, 512 * 1024, MAX_LARGE];
        let mut v = Vec::new();
        for (i, &s) in sizes.iter().cycle().take(40).enumerate() {
            let p = a(k, 0, s, 8);
            assert!(k.block_size(p) >= s);
            fill(p, s, i as u64);
            v.push((p as usize, s, i as u64));
        }
        // La moitié libérée par un autre cœur.
        let remote: Vec<(usize, usize, u64)> = v.drain(..20).collect();
        std::thread::spawn(move || {
            for (p, s, id) in remote {
                check(p as *mut u8, s, id);
                f(k, 1, p as *mut u8, s, 8);
            }
        })
        .join()
        .unwrap();
        for (p, s, id) in v {
            check(p as *mut u8, s, id);
            f(k, 0, p as *mut u8, s, 8);
        }
        k.collect(0);
        trim_all(k);
    }
    let st = k.heap_stats(0).unwrap();
    assert_eq!(st.pages_fresh, st.pages_retired, "pages larges non rendues : {st:?}");
    assert!(k.stats().segments <= 1 + 1, "segments engages : {}", k.stats().segments);
}

#[test]
fn block_size_and_realloc_in_place() {
    let k = new_kalloc();
    unsafe {
        for size in [1usize, 24, 100, 1000, 5000, 70_000] {
            let p = a(k, 0, size, 8);
            let bs = k.block_size(p);
            assert!(bs >= size && bs == SIZES[class::lookup(size, 8).unwrap()] as usize);
            f(k, 0, p, size, 8);
        }
    }
}

#[test]
#[should_panic(expected = "double liberation")]
fn double_free_is_detected() {
    let k = new_kalloc();
    unsafe {
        let p = a(k, 0, 48, 8);
        let _q = a(k, 0, 48, 8);
        f(k, 0, p, 48, 8);
        f(k, 0, p, 48, 8);
    }
}

#[test]
#[should_panic(expected = "corrompue")]
fn overflow_into_free_block_is_detected() {
    let k = new_kalloc();
    unsafe {
        let v: Vec<*mut u8> = (0..8).map(|_| a(k, 0, 32, 8)).collect();
        // Les blocs d'une page neuve sont chaînés dans l'ordre des adresses : un débordement du
        // dernier objet alloué écrase le lien du premier bloc libre.
        std::ptr::write_bytes(v[7], 0x41, 32 + 8);
        for _ in 0..10_000 {
            a(k, 0, 32, 8);
        }
    }
}

#[test]
#[should_panic(expected = "liberation invalide")]
fn foreign_pointer_is_rejected() {
    let k = new_kalloc();
    unsafe {
        let _ = a(k, 0, 16, 8);
        let foreign = host_alloc(seg_layout());
        std::ptr::write_bytes(foreign, 0, 64);
        f(k, 0, foreign.add(HEADER_SIZE), 16, 8);
    }
}
