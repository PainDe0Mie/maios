//! # Swap — RAM virtuelle sur disque pour mai_os
//!
//! Utilise un StorageDevice (ATA) comme extension de RAM.
//! L'accès au disque passe par les traits BlockReader/BlockWriter
//! de storage_device — pas besoin de downcast vers AtaDrive.
//!
//! ## Encodage PTE quand present=0
//! ```text
//! bit  0   = 0  (not present)
//! bit  1   = 1  (SWAP_MAGIC — distingue d'une PTE vide)
//! bits 2..51 = slot index
//! ```

#![no_std]
extern crate alloc;
extern crate spin;
#[macro_use] extern crate log;

extern crate memory_structs;
extern crate frame_allocator;
extern crate page_allocator;
extern crate memory;
extern crate pte_flags;
extern crate storage_device;
extern crate io;

use alloc::vec::Vec;
use spin::{Mutex, Once};
use memory_structs::PhysicalAddress;
use storage_device::StorageDeviceRef;

// ================================================================
// CONSTANTES
// ================================================================

pub const PAGE_SIZE:          usize = 4096;
const  SECTORS_PER_PAGE:      usize = PAGE_SIZE / 512; // 8 secteurs ATA
const  SWAP_DISK_OFFSET_SECS: usize = 2048;            // 1 MB de marge
const  MAX_SWAP_PAGES:        usize = 131072;           // 512 MB max

/// Bit 1 de la PTE : identifie une page swappée (≠ PTE vide)
pub const SWAP_MAGIC_BIT: u64 = 1 << 1;

// ================================================================
// INSTANCE GLOBALE
// ================================================================

pub static SWAP: Once<Mutex<SwapManager>> = Once::new();

// ================================================================
// SWAP MANAGER
// ================================================================

pub struct SwapManager {
    free_slots:  Vec<bool>,
    used_slots:  usize,
    total_slots: usize,
    /// Le device de stockage (Arc<Mutex<dyn StorageDevice>>)
    device:      StorageDeviceRef,
}

impl SwapManager {
    fn new(device: StorageDeviceRef, swap_size_mb: usize) -> Self {
        let total = ((swap_size_mb * 1024 * 1024) / PAGE_SIZE).min(MAX_SWAP_PAGES);
        let mut free_slots = Vec::with_capacity(total);
        free_slots.resize(total, true);
        info!("[swap] {} MB = {} slots", swap_size_mb, total);
        SwapManager { free_slots, used_slots: 0, total_slots: total, device }
    }

    fn alloc_slot(&mut self) -> Option<usize> {
        for (i, free) in self.free_slots.iter_mut().enumerate() {
            if *free { *free = false; self.used_slots += 1; return Some(i); }
        }
        None
    }

    fn free_slot(&mut self, slot: usize) {
        if slot < self.total_slots {
            self.free_slots[slot] = true;
            self.used_slots = self.used_slots.saturating_sub(1);
        }
    }

    fn sector_of(&self, slot: usize) -> usize {
        SWAP_DISK_OFFSET_SECS + slot * SECTORS_PER_PAGE
    }

    fn write(&self, slot: usize, data: &[u8; PAGE_SIZE]) -> Result<(), &'static str> {
        self.device.lock()
            .write_blocks(data, self.sector_of(slot))
            .map(|_| ())
            .map_err(|_e| "swap: write_blocks échoué")
    }

    fn read(&self, slot: usize, buf: &mut [u8; PAGE_SIZE]) -> Result<(), &'static str> {
        self.device.lock()
            .read_blocks(buf, self.sector_of(slot))
            .map(|_| ())
            .map_err(|_e| "swap: read_blocks échoué")
    }

    pub fn usage(&self) -> (usize, usize) { (self.used_slots, self.total_slots) }
}

// ================================================================
// ENCODAGE / DÉCODAGE PTE
// ================================================================

#[inline]
pub fn encode_swap_pte(slot: usize) -> u64 {
    ((slot as u64) << 2) | SWAP_MAGIC_BIT
}

#[inline]
pub fn decode_swap_slot(pte_raw: u64) -> usize {
    (pte_raw >> 2) as usize
}

/// Vérifie si une PTE brute représente une page swappée :
/// bit 0 = 0 (not present) ET bit 1 = 1 (magic)
#[inline]
pub fn is_swap_pte(pte_raw: u64) -> bool {
    (pte_raw & 0b11) == SWAP_MAGIC_BIT
}

// ================================================================
// API PUBLIQUE
// ================================================================

/// Initialise le swap.
/// 
/// `device` : un StorageDeviceRef depuis storage_manager::storage_devices()
/// `swap_size_mb` : taille max du swap en MB
pub fn init(device: StorageDeviceRef, swap_size_mb: usize) {
    SWAP.call_once(|| Mutex::new(SwapManager::new(device, swap_size_mb)));
}

/// Swapper une page vers le disque.
///
/// # Safety
/// `vaddr` doit être aligné sur PAGE_SIZE et pointer une page RAM valide.
///
/// # Returns
/// La valeur à écrire dans la PTE (present=0, slot encodé)
pub unsafe fn swap_out(vaddr: usize) -> Result<u64, &'static str> {
    debug_assert!(vaddr % PAGE_SIZE == 0, "vaddr non aligné");

    let swap = SWAP.get().ok_or("swap: non initialisé")?;
    let mut sw = swap.lock();

    let slot = sw.alloc_slot().ok_or("swap: plus de slots libres")?;

    // Lit la page depuis la RAM dans un buffer temporaire
    let mut buf = [0u8; PAGE_SIZE];
    core::ptr::copy_nonoverlapping(vaddr as *const u8, buf.as_mut_ptr(), PAGE_SIZE);

    // Écrit sur disque
    sw.write(slot, &buf)?;

    let (used, total) = sw.usage();
    debug!("[swap] OUT 0x{:x} → slot {} ({}/{})", vaddr, slot, used, total);

    Ok(encode_swap_pte(slot))
}

/// Recharge une page swappée depuis le disque.
/// Appelé depuis le page fault handler.
///
/// # Returns
/// L'adresse physique du frame rechargé — à écrire dans la PTE.
pub fn swap_in(pte_raw: u64) -> Result<PhysicalAddress, &'static str> {
    if !is_swap_pte(pte_raw) {
        return Err("swap: pas une swap PTE");
    }

    let slot = decode_swap_slot(pte_raw);

    // 1. Lit depuis le disque dans un buffer stack
    let mut buf = [0u8; PAGE_SIZE];
    {
        let swap = SWAP.get().ok_or("swap: non initialisé")?;
        swap.lock().read(slot, &mut buf)?;
    }

    // 2. Alloue frame physique + page virtuelle temporaire
    let frames = frame_allocator::allocate_frames(1)
        .ok_or("swap: plus de frames physiques")?;
    let paddr = frames.start_address();

    let temp_pages = page_allocator::allocate_pages(1)
        .ok_or("swap: plus de pages virtuelles")?;

    // 3. Mappe temporairement le frame pour pouvoir y écrire
    let flags = pte_flags::PteFlagsArch::new()
        .valid(true)
        .writable(true);

    let kernel_mmi = memory::get_kernel_mmi_ref()
        .ok_or("swap: kernel MMI non disponible")?;

    let mapped = {
        let mut mmi = kernel_mmi.lock();
        mmi.page_table.map_allocated_pages_to(temp_pages, frames, flags)?
    };

    // 4. Copie les données dans le frame via le vaddr temporaire
    unsafe {
        core::ptr::copy_nonoverlapping(
            buf.as_ptr(),
            mapped.start_address().value() as *mut u8,
            PAGE_SIZE,
        );
    }

    // 5. forget(mapped) : empêche le drop de libérer le frame.
    //    Fuite de 4KB de vaddr kernel — négligeable.
    core::mem::forget(mapped);

    // 6. Libère le slot
    {
        let swap = SWAP.get().unwrap();
        let mut sw = swap.lock();
        sw.free_slot(slot);
        let (used, total) = sw.usage();
        debug!("[swap] IN slot {} → paddr 0x{:x} ({}/{})", slot, paddr.value(), used, total);
    }

    Ok(paddr)
}

/// Stats : (slots utilisés, total)
pub fn usage() -> Option<(usize, usize)> {
    SWAP.get().map(|s| s.lock().usage())
}