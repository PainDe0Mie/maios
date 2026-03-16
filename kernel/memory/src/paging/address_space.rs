#![allow(unused_imports)]

use super::{PageTable, Mapper}; 
use memory_structs::Frame;      
use x86_64::PhysAddr;           
use x86_64::structures::paging::{
    PhysFrame, FrameAllocator, PageTable as X86PageTable, PageTableFlags
};
use x86_64::registers::control::Cr3;

/// Cette structure représente la mémoire d'un processus isolé.
pub struct AddressSpace {
    /// La frame physique de la table PML4 (la racine des pages)
    pml4_frame: Frame,
}

impl AddressSpace {
    /// Crée un nouvel espace d'adressage
    /// Copie automatiquement le Kernel de la table actuelle vers la nouvelle.
    pub fn new(allocator: &mut dyn FrameAllocator<x86_64::structures::paging::Size4KiB>) -> Result<Self, &'static str> {
        // 1. Allouer une page physique pour la nouvelle PML4
        let phys_frame = allocator.allocate_frame().ok_or("Out of memory for PML4")?;
        
        // 2. Récupérer l'adresse virtuelle pour écrire dedans
        // ATTENTION: On suppose ici que Theseus utilise l'Identity Mapping (Phys 0x1000 = Virt 0x1000)
        // C'est le cas au démarrage. Si ça change, il faudra ajouter un offset ici.
        let new_table_ptr = phys_frame.start_address().as_u64() as *mut X86PageTable;
        let new_table = unsafe { &mut *new_table_ptr };

        // 3. Initialiser la nouvelle table à zéro
        new_table.zero();

        // 4. Copier le Kernel (Entrées 256 à 512)
        // On récupère la table active actuelle via CR3
        let active_table = unsafe { get_active_pml4() };
        
        // On clone la moitié haute (Kernel Space)
        for i in 256..512 {
            new_table[i] = active_table[i].clone();
        }

        // 5. Création de l'objet final
        let addr_val = phys_frame.start_address().as_u64() as usize;
        let physical_address = memory_structs::PhysicalAddress::new(addr_val)
            .ok_or("Invalid physical address")?;
        let frame = Frame::containing_address(physical_address);

        Ok(AddressSpace { pml4_frame: frame })
    }

    /// Active cet espace d'adressage (Switch CR3)
    pub unsafe fn switch_to(&self) {
        let addr_val = self.pml4_frame.start_address().value() as u64;
        let phys_addr = PhysAddr::new(addr_val);
        let phys_frame = PhysFrame::from_start_address(phys_addr).unwrap();
        
        let (_, flags) = Cr3::read();
        Cr3::write(phys_frame, flags);
    }
}

/// Helper pour lire la table PML4 active actuellement sur le CPU
unsafe fn get_active_pml4() -> &'static X86PageTable {
    let (frame, _) = Cr3::read();
    let phys_addr = frame.start_address().as_u64();
    let virt_addr = phys_addr; // Assumption: Identity Mapping
    &*(virt_addr as *const X86PageTable)
}
