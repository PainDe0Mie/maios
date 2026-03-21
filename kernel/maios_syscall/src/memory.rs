//! Syscalls mémoire unifiés pour MaiOS.
//!
//! Consolide les implémentations de linux_syscall (mmap, munmap, brk)
//! et windows_syscall (NtAllocateVirtualMemory, NtFreeVirtualMemory)
//! en une seule source de vérité.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use log::{debug, warn};
use memory::MappedPages;
use pte_flags::PteFlags;
use spin::Mutex;

use crate::error::{SyscallResult, SyscallError};
use crate::resource::{self, Resource};

// =============================================================================
// État global pour le tracking mémoire
// =============================================================================

/// Régions mmap allouées. Empêche le drop prématuré des MappedPages.
/// Clé = adresse virtuelle de début.
static MMAP_REGIONS: Mutex<BTreeMap<usize, MappedPages>> = Mutex::new(BTreeMap::new());

/// État du programme break pour sys_brk.
static BRK_STATE: Mutex<BrkState> = Mutex::new(BrkState {
    current_brk: 0x6000_0000,
    initial_brk: 0x6000_0000,
});

/// Pages allouées par brk (empêche leur drop).
static BRK_PAGES: Mutex<Vec<MappedPages>> = Mutex::new(Vec::new());

struct BrkState {
    current_brk: usize,
    initial_brk: usize,
}

// =============================================================================
// Constantes de protection Linux
// =============================================================================

mod mmap_flags {
    pub const MAP_ANONYMOUS: u64 = 0x20;
    #[allow(dead_code)]
    pub const MAP_PRIVATE: u64 = 0x02;
}

mod mmap_prot {
    #[allow(dead_code)]
    pub const PROT_READ: u64 = 0x1;
    pub const PROT_WRITE: u64 = 0x2;
    pub const PROT_EXEC: u64 = 0x4;
}

/// Convertir les flags PROT_* Linux en PteFlags MaiOS.
fn linux_prot_to_pte_flags(prot: u64) -> PteFlags {
    let mut flags = PteFlags::new().valid(true);
    // NOT_EXECUTABLE est set par défaut dans new() — on doit l'effacer explicitement
    if prot & mmap_prot::PROT_EXEC != 0 {
        flags = flags.executable(true); // efface NOT_EXECUTABLE
    }
    if prot & mmap_prot::PROT_WRITE != 0 {
        flags = flags.writable(true);
    }
    flags
}

// =============================================================================
// Constantes de protection Windows
// =============================================================================

mod win_protect {
    #[allow(dead_code)]
    pub const PAGE_NOACCESS: u64 = 0x01;
    #[allow(dead_code)]
    pub const PAGE_READONLY: u64 = 0x02;
    pub const PAGE_READWRITE: u64 = 0x04;
    pub const PAGE_WRITECOPY: u64 = 0x08;
    pub const PAGE_EXECUTE: u64 = 0x10;
    pub const PAGE_EXECUTE_READ: u64 = 0x20;
    pub const PAGE_EXECUTE_READWRITE: u64 = 0x40;
}

mod win_mem_type {
    pub const MEM_COMMIT: u64 = 0x1000;
    pub const MEM_RESERVE: u64 = 0x2000;
    pub const MEM_DECOMMIT: u64 = 0x4000;
    pub const MEM_RELEASE: u64 = 0x8000;
}

/// Convertir les flags PAGE_* Windows en PteFlags MaiOS.
fn win_protect_to_pte_flags(protect: u64) -> PteFlags {
    let mut flags = PteFlags::new().valid(true); // ← idem
    match protect {
        win_protect::PAGE_READWRITE | win_protect::PAGE_WRITECOPY => {
            flags = flags.writable(true);
        }
        win_protect::PAGE_EXECUTE_READ => {
            flags = flags.executable(true);
        }
        win_protect::PAGE_EXECUTE_READWRITE => {
            flags = flags.writable(true).executable(true);
        }
        win_protect::PAGE_EXECUTE => {
            flags = flags.executable(true);
        }
        _ => {}
    }
    flags
}


// =============================================================================
// Implémentations des syscalls
// =============================================================================

/// sys_mmap — allocation de mémoire anonyme (Linux ABI).
///
/// Arguments : addr, length, prot, flags, fd, offset
pub fn sys_mmap(addr: u64, length: u64, prot: u64, flags: u64, _fd: u64, _offset: u64) -> SyscallResult {
    warn!("sys_mmap(addr={:#x}, len={}, prot={:#x}, flags={:#x})", addr, length, prot, flags);

    if flags & mmap_flags::MAP_ANONYMOUS == 0 {
        warn!("sys_mmap: non-anonymous mapping not supported (flags={:#x})", flags);
        return Err(SyscallError::NotImplemented);
    }
    if length == 0 {
        return Err(SyscallError::InvalidArgument);
    }

    let pte_flags = linux_prot_to_pte_flags(prot);

    match memory::create_mapping(length as usize, pte_flags) {
        Ok(mp) => {
            let vaddr = mp.start_address().value();
            let size = mp.size_in_bytes();
            unsafe { core::ptr::write_bytes(vaddr as *mut u8, 0, size); }
            warn!("sys_mmap: mapped {} bytes at {:#x}", size, vaddr);
            MMAP_REGIONS.lock().insert(vaddr, mp);
            Ok(vaddr as u64)
        }
        Err(e) => {
            warn!("sys_mmap: memory::create_mapping failed: {}", e);
            Err(SyscallError::OutOfMemory)
        }
    }
}

/// sys_munmap — libération de mémoire anonyme (Linux ABI).
pub fn sys_munmap(addr: u64, _length: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    warn!("sys_munmap(addr={:#x}, len={})", addr, _length);

    let addr = addr as usize;
    if MMAP_REGIONS.lock().remove(&addr).is_some() {
        warn!("sys_munmap: unmapped region at {:#x}", addr);
        return Ok(0);
    }

    warn!("sys_munmap: no mapping at {:#x}, returning success", addr);
    Ok(0)
}

/// sys_mprotect — changement de protection mémoire (stub).
pub fn sys_mprotect(addr: u64, length: u64, prot: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    debug!("sys_mprotect(addr={:#x}, len={}, prot={:#x})", addr, length, prot);

    let new_flags = linux_prot_to_pte_flags(prot);
    let addr_usize = addr as usize;

    // Cherche la région dans MMAP_REGIONS et applique remap()
    let mut regions = MMAP_REGIONS.lock();
    if let Some(mp) = regions.get_mut(&addr_usize) {
        mp.remap(
            &mut memory::get_kernel_mmi_ref()
                .ok_or(SyscallError::InternalError)?
                .lock()
                .page_table,
            new_flags,
        ).map_err(|_| SyscallError::InvalidArgument)?;
        return Ok(0);
    }

    // Cherche aussi dans les Memory resources de la tâche courante
    let tid = task::get_my_current_task_id();
    let found = resource::with_resources_mut(tid, |table| {
        table.remap_memory(addr_usize, new_flags)
    });

    if found {
        Ok(0)
    } else {
        warn!("sys_mprotect: no tracked region at {:#x}, ignoring", addr_usize);
        Ok(0)
    }
}

/// sys_brk — gestion du programme break.
pub fn sys_brk(addr: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    warn!("sys_brk(addr={:#x})", addr);

    let mut state = BRK_STATE.lock();
    let addr = addr as usize;

    if addr == 0 || addr < state.initial_brk {
        return Ok(state.current_brk as u64);
    }

    if addr <= state.current_brk {
        state.current_brk = addr;
        return Ok(addr as u64);
    }

    let growth = addr - state.current_brk;
    let pte_flags = PteFlags::new().valid(true).writable(true);
    match memory::create_mapping(growth, pte_flags) {
        Ok(mp) => {
            let vaddr = mp.start_address().value();
            let size = mp.size_in_bytes();
            unsafe { core::ptr::write_bytes(vaddr as *mut u8, 0, size); }
            BRK_PAGES.lock().push(mp);
            state.current_brk = addr;
            Ok(addr as u64)
        }
        Err(e) => {
            warn!("sys_brk: memory::create_mapping failed: {}", e);
            Ok(state.current_brk as u64)
        }
    }
}

/// sys_alloc_vm — allocation de mémoire virtuelle (style NT).
///
/// Appelé par l'adaptateur NT NtAllocateVirtualMemory après
/// déréférencement des pointeurs.
///
/// Arguments : size, protect, alloc_type (les 3 autres ignorés)
/// Retourne : Ok(base_address) en cas de succès
pub fn sys_alloc_vm(size: u64, protect: u64, alloc_type: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    warn!("sys_alloc_vm(size={:#x}, protect={:#x}, type={:#x})", size, protect, alloc_type);

    let requested_size = size as usize;
    if requested_size == 0 {
        return Err(SyscallError::InvalidArgument);
    }

    if alloc_type & win_mem_type::MEM_COMMIT == 0 && alloc_type & win_mem_type::MEM_RESERVE == 0 {
        return Err(SyscallError::InvalidArgument);
    }
    
    let effective_protect = if protect == 0 { 
        win_protect::PAGE_READWRITE 
    } else { 
        protect 
    };
    let pte_flags = win_protect_to_pte_flags(effective_protect);

    match memory::create_mapping(requested_size, pte_flags) {
        Ok(mp) => {
            let vaddr = mp.start_address().value();
            let actual_size = mp.size_in_bytes();
            unsafe { core::ptr::write_bytes(vaddr as *mut u8, 0, actual_size); }

            warn!("sys_alloc_vm: mapped {} bytes at {:#x}", actual_size, vaddr);

            // Tracker dans la resource table
            let tid = task::get_my_current_task_id();
            resource::with_resources_mut(tid, |table| {
                table.alloc_handle(Resource::Memory {
                    pages: mp,
                    base: vaddr,
                    size: actual_size,
                });
            });

            // Retourner l'adresse de base — l'adaptateur NT écrira dans les pointeurs
            Ok(vaddr as u64)
        }
        Err(e) => {
            warn!("sys_alloc_vm: memory::create_mapping failed: {}", e);
            Err(SyscallError::OutOfMemory)
        }
    }
}

/// sys_free_vm — libération de mémoire virtuelle (style NT).
///
/// Arguments : base_address, free_type
pub fn sys_free_vm(base: u64, free_type: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    let base_addr = base as usize;
    warn!("sys_free_vm(base={:#x}, type={:#x})", base_addr, free_type);

    if free_type & win_mem_type::MEM_RELEASE != 0 {
        let tid = task::get_my_current_task_id();

        // Trouver et fermer le handle associé à cette région
        let handle = resource::with_resources(tid, |table| {
            table.find_handle(|r| {
                matches!(r, Resource::Memory { base, .. } if *base == base_addr)
            })
        });

        if let Some(h) = handle {
            resource::with_resources_mut(tid, |table| {
                table.close(h); // Le drop de MappedPages libère la mémoire
            });
            warn!("sys_free_vm: released region at {:#x}", base_addr);
            Ok(0)
        } else {
            warn!("sys_free_vm: no region found at {:#x}", base_addr);
            Err(SyscallError::InvalidArgument)
        }
    } else if free_type & win_mem_type::MEM_DECOMMIT != 0 {
        warn!("sys_free_vm: MEM_DECOMMIT treated as no-op");
        Ok(0)
    } else {
        Err(SyscallError::InvalidArgument)
    }
}

/// sys_mremap — remap a memory region (grow/shrink/move).
///
/// Stub: returns NotImplemented. A real implementation would need to
/// relocate MappedPages, which the MaiOS memory subsystem doesn't support yet.
pub fn sys_mremap(_old_addr: u64, _old_size: u64, _new_size: u64, _flags: u64, _: u64, _: u64) -> SyscallResult {
    Err(SyscallError::NotImplemented)
}
