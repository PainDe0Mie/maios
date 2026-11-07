//! `free` : état de la mémoire physique (MaiMM).

#![no_std]
#[macro_use] extern crate app_io;
extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

const FRAME_KIB: usize = 4;

fn fmt_size(frames: usize, human: bool) -> String {
    let kib = frames * FRAME_KIB;
    if !human {
        return alloc::format!("{kib}");
    }
    // Arithmétique entière en dixièmes : le noyau est compilé sans SSE (soft-float).
    if kib >= 1 << 20 {
        let tenths = kib * 10 / (1 << 20);
        alloc::format!("{}.{}G", tenths / 10, tenths % 10)
    } else if kib >= 1 << 10 {
        let tenths = kib * 10 / (1 << 10);
        alloc::format!("{}.{}M", tenths / 10, tenths % 10)
    } else {
        alloc::format!("{kib}K")
    }
}

// Sorties en ASCII : la police du terminal MaiOS n'a pas encore les accents.
pub fn main(args: Vec<String>) -> isize {
    let mut human = false;
    let mut verbose = false;
    for arg in &args {
        match arg.as_str() {
            "-h" | "--human" => human = true,
            "-v" | "--verbose" => verbose = true,
            "--help" => {
                println!("Usage : free [-h] [-v]");
                println!("Affiche l'utilisation de la memoire physique.");
                println!("  -h, --human    tailles lisibles (K, M, G) au lieu de Kio");
                println!("  -v, --verbose  compteurs de l'allocateur de frames");
                return 0;
            }
            other => {
                println!("free : option inconnue '{other}' (voir --help)");
                return 1;
            }
        }
    }

    let s = frame_allocator::stats();
    let used = s.total_frames.saturating_sub(s.free_frames);
    let unit = if human { "" } else { " (Kio)" };
    println!("{:<8}{:>12}{:>12}{:>12}{:>14}", unit, "total", "utilisee", "libre", "pages 2 Mio");
    println!(
        "{:<8}{:>12}{:>12}{:>12}{:>14}",
        "Mem :",
        fmt_size(s.total_frames, human),
        fmt_size(used, human),
        fmt_size(s.free_frames, human),
        s.free_huge_2m,
    );
    println!("Swap :   indisponible");

    if verbose {
        let e = s.events;
        println!();
        println!("Allocateur de frames : {}", s.allocator);
        println!("  pool pre-zeroe       : {} frames", s.zeroed_pool_frames);
        println!("  reserve libre (MMIO) : {}", fmt_size(s.free_reserved_frames, true));
        println!("  allocations          : {} ({} frames)", e.allocations, e.allocated_frames);
        println!("  liberations          : {} ({} frames)", e.deallocations, e.freed_frames);
        println!("  echecs               : {}", e.failures);
        println!("  frames zeroees       : {} servies par le pool, {} a la volee", e.zeroed_hits, e.zeroed_misses);
        let h = heap::stats();
        let k = h.kalloc;
        println!();
        println!("Tas noyau : {}", if h.enabled { "kalloc" } else { "amorcage" });
        println!("  segments 2 Mio mappes : {} ({} en reserve), {} tas de coeur", h.segments_mapped, k.pool, k.heaps);
        println!("  allocations           : {} ({} par le chemin lent)", k.allocs, k.slow_allocs);
        println!("  liberations           : {} locales, {} distantes ({} differees)", k.frees, k.remote_frees, k.delayed_frees);
        println!("  pages                 : {} creees, {} rendues", k.pages_fresh, k.pages_retired);
        println!("  gros objets mappes    : {} ({})", h.large_count, fmt_size(h.large_bytes / 4096, true));
    }
    0
}
