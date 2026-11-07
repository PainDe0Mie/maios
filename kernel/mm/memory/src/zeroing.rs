//! Remplissage du pool de frames pré-zéroées (MaiMM M1) pendant que les cœurs sont inactifs.
//!
//! La tâche idle de chaque cœur appelle [`refill_zeroed_frames`] avant de s'endormir (`hlt`).
//! Par lot de 64 frames (256 Kio), la fonction :
//! 1. alloue 64 frames contiguës ;
//! 2. les mappe sur des pages virtuelles temporaires ;
//! 3. les met à zéro ;
//! 4. les démappe (un seul TLB shootdown pour tout le lot) ;
//! 5. les dépose dans le pool de `frame_allocator`.
//!
//! C'est le pendant de la *zeroed page list* que Windows remplit avec un thread de priorité
//! zéro. Le travail est fait quand le CPU n'a rien d'autre à faire et sort du chemin critique.

use frame_allocator::{allocate_frames, push_zeroed_frames, zeroed_pool_deficit};
use pte_flags::PteFlags;

use crate::{allocate_pages, get_kernel_mmi_ref, PAGE_SIZE};

/// Frames zéroées par lot.
const BATCH: usize = 64;

/// Zéroe au plus `max_batches` lots de 64 frames et les dépose dans le pool de frames
/// pré-zéroées. Renvoie le nombre de frames ajoutées : 0 si le pool est plein, si la mémoire
/// manque ou si le système mémoire n'est pas encore initialisé.
pub fn refill_zeroed_frames(max_batches: usize) -> usize {
    let Some(kernel_mmi) = get_kernel_mmi_ref() else { return 0 };
    let mut added = 0;
    for _ in 0..max_batches {
        if zeroed_pool_deficit() < BATCH {
            break;
        }
        let Some(frames) = allocate_frames(BATCH) else { break };
        let Some(pages) = allocate_pages(BATCH) else { break };
        let flags = PteFlags::new().valid(true).writable(true);
        let mapped = kernel_mmi.lock().page_table.map_allocated_pages_to(pages, frames, flags);
        let Ok(mut mp) = mapped else { break };
        match mp.as_slice_mut::<u8>(0, BATCH * PAGE_SIZE) {
            Ok(bytes) => bytes.fill(0),
            Err(_) => break, // `mp` est démappé et ses frames libérées en sortant
        }
        let unmapped = mp.unmap_into_parts(&mut kernel_mmi.lock().page_table);
        match unmapped {
            Ok((_pages, Some(frames))) if frames.size_in_frames() == BATCH => {
                push_zeroed_frames(frames);
                added += BATCH;
            }
            // Cas imprévus : les frames (s'il y en a) sont libérées normalement en sortant.
            Ok(_) | Err(_) => break,
        }
    }
    added
}
