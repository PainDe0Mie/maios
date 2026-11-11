//! Remplissage du pool de frames pré-zéroées (MaiMM M1) pendant que les cœurs sont inactifs.
//!
//! La tâche idle de chaque cœur appelle [`refill_zeroed_frames`] avant de s'endormir (`hlt`).
//! Par lot de 64 frames (256 Kio), la fonction alloue 64 frames contiguës, les met à zéro et
//! les dépose dans le pool de `frame_allocator`.
//!
//! C'est le pendant de la *zeroed page list* que Windows remplit avec un thread de priorité
//! zéro. Le travail est fait quand le CPU n'a rien d'autre à faire et sort du chemin critique.
//!
//! x86_64 : les frames sont écrites par la physmap (toute la RAM projetée en pages de 2 Mio) :
//! ni table de pages à créer, ni invalidation de TLB, donc rien à allouer d'autre que les frames
//! elles-mêmes — une tâche inactive ne doit jamais échouer faute de mémoire. Les écritures sont
//! non temporelles (`movnti`, comme `clear_page_nocache` de Linux) : 256 Kio de zéros ne
//! chassent pas du cache les données de la tâche qui va reprendre le cœur.
//! Ailleurs : projection temporaire des frames, comme avant la physmap.
//!
//! Réserve : le pool n'est rempli que s'il reste au moins [`RESERVE_FRAMES`] frames libres. Sous
//! pression, il ne doit pas immobiliser les dernières frames (l'allocateur le vide alors,
//! `zeroed::drain`, mais une tâche inactive le remplirait de nouveau aussitôt).

use frame_allocator::{allocate_frames, free_general_frames, push_zeroed_frames, zeroed_pool_deficit};

use crate::PAGE_SIZE;

/// Frames zéroées par lot.
const BATCH: usize = 64;

/// Réserve système : frames libres en dessous desquelles le pool n'est plus rempli et tmpfs ne
/// prend plus de page (16 Mio).
pub const RESERVE_FRAMES: usize = 4096;

/// Zéroe au plus `max_batches` lots de 64 frames et les dépose dans le pool de frames
/// pré-zéroées. Renvoie le nombre de frames ajoutées : 0 si le pool est plein, si la mémoire
/// manque ou si le système mémoire n'est pas encore initialisé.
pub fn refill_zeroed_frames(max_batches: usize) -> usize {
    let mut added = 0;
    for _ in 0..max_batches {
        if zeroed_pool_deficit() < BATCH {
            break;
        }
        if free_general_frames().map_or(false, |free| free < RESERVE_FRAMES + BATCH) {
            break;
        }
        if !zero_one_batch() {
            break;
        }
        added += BATCH;
    }
    added
}

#[cfg(target_arch = "x86_64")]
fn zero_one_batch() -> bool {
    use crate::paging::kernel_half::{is_sealed, phys_to_virt, physmap_end};
    if !is_sealed() {
        return zero_one_batch_mapped();
    }
    let Some(frames) = allocate_frames(BATCH) else { return false };
    let pa = frames.start_address().value();
    if pa + BATCH * PAGE_SIZE > physmap_end() {
        return false; // hors de la RAM projetée : les frames sont rendues en sortant
    }
    // SAFETY : les frames viennent d'être allouées, ne sont mappées nulle part ailleurs et la
    // physmap les couvre en lecture/écriture.
    unsafe { zero_nontemporal(phys_to_virt(pa) as *mut u64, BATCH * PAGE_SIZE / 8) };
    push_zeroed_frames(frames);
    true
}

/// Met à zéro `words` mots de 64 bits à `dst` (aligné sur 64 octets) par des écritures non
/// temporelles, puis `sfence` : les zéros sont visibles avant le dépôt dans le pool.
#[cfg(target_arch = "x86_64")]
unsafe fn zero_nontemporal(dst: *mut u64, words: usize) {
    debug_assert!(dst as usize % 64 == 0 && words % 8 == 0);
    let end = dst.add(words);
    core::arch::asm!(
        "xor {z:e}, {z:e}",
        "2:",
        "movnti [{p}], {z}",
        "movnti [{p} + 8], {z}",
        "movnti [{p} + 16], {z}",
        "movnti [{p} + 24], {z}",
        "movnti [{p} + 32], {z}",
        "movnti [{p} + 40], {z}",
        "movnti [{p} + 48], {z}",
        "movnti [{p} + 56], {z}",
        "add {p}, 64",
        "cmp {p}, {e}",
        "jb 2b",
        "sfence",
        p = inout(reg) dst => _,
        e = in(reg) end,
        z = out(reg) _,
        options(nostack),
    );
}

#[cfg(not(target_arch = "x86_64"))]
fn zero_one_batch() -> bool {
    zero_one_batch_mapped()
}

/// Lot zéroé par une projection temporaire (avant la physmap, ou sans physmap).
fn zero_one_batch_mapped() -> bool {
    use pte_flags::PteFlags;
    let Some(kernel_mmi) = crate::get_kernel_mmi_ref() else { return false };
    let Some(frames) = allocate_frames(BATCH) else { return false };
    let Some(pages) = crate::allocate_pages(BATCH) else { return false };
    let flags = PteFlags::new().valid(true).writable(true);
    let mapped = kernel_mmi.lock().page_table.map_allocated_pages_to(pages, frames, flags);
    let Ok(mut mp) = mapped else { return false };
    match mp.as_slice_mut::<u8>(0, BATCH * PAGE_SIZE) {
        Ok(bytes) => bytes.fill(0),
        Err(_) => return false, // `mp` est démappé et ses frames libérées en sortant
    }
    let unmapped = mp.unmap_into_parts(&mut kernel_mmi.lock().page_table);
    match unmapped {
        Ok((_pages, Some(frames))) if frames.size_in_frames() == BATCH => {
            push_zeroed_frames(frames);
            true
        }
        // Cas imprévus : les frames (s'il y en a) sont libérées normalement en sortant.
        Ok(_) | Err(_) => false,
    }
}
