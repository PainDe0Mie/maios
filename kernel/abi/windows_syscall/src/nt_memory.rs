//! NT memory management syscalls.
//!
//! Implements NtCreateSection, NtMapViewOfSection, and NtQueryVirtualMemory
//! as thin adapters over MaiOS native syscalls (SYS_MMAP).

use crate::ntstatus;
use crate::nt_threading::{self, KernelObject, NtSection};

// =============================================================================
// NtCreateSection (0x004A)
// =============================================================================

/// NtCreateSection — crée un objet section (mémoire partageable).
///
///   NtCreateSection(
///     OUT PHANDLE SectionHandle,             // arg0
///     ACCESS_MASK DesiredAccess,              // arg1
///     POBJECT_ATTRIBUTES ObjectAttributes,   // arg2 (ignoré)
///     PLARGE_INTEGER MaximumSize,             // arg3
///     ULONG SectionPageProtection,           // arg4 (PAGE_READWRITE etc.)
///     ULONG AllocationAttributes,            // arg5 (SEC_COMMIT etc.)
///   )
///
/// Implémentation simplifiée : sections anonymes (pagefile-backed) uniquement.
/// La taille est stockée dans l'objet kernel ; le mapping réel se fait dans
/// NtMapViewOfSection via SYS_MMAP.
pub fn adapt_nt_create_section(
    section_handle_ptr: u64,
    _desired_access: u64,
    _object_attributes: u64,
    max_size_ptr: u64,
    protection: u64,
    _allocation_attributes: u64,
) -> i64 {
    if section_handle_ptr == 0 {
        return ntstatus::STATUS_INVALID_PARAMETER;
    }

    // Lire la taille maximale (si fournie)
    let max_size = if max_size_ptr != 0 {
        let raw = unsafe { *(max_size_ptr as *const u64) };
        if raw == 0 { 4096 } else { raw }
    } else {
        4096 // Taille par défaut : 1 page
    };

    let handle = nt_threading::alloc_kernel_handle();
    {
        let mut objects = nt_threading::kernel_objects().lock();
        objects.insert(handle, KernelObject::Section(NtSection {
            max_size,
            protection: protection as u32,
        }));
    }

    unsafe { *(section_handle_ptr as *mut u64) = handle; }

    ntstatus::STATUS_SUCCESS
}

// =============================================================================
// NtMapViewOfSection (0x0028)
// =============================================================================

/// NtMapViewOfSection — mappe une section dans l'espace d'adressage.
///
///   NtMapViewOfSection(
///     HANDLE SectionHandle,                  // arg0
///     HANDLE ProcessHandle,                  // arg1 (-1 = current)
///     PVOID *BaseAddress,                    // arg2 (in/out)
///     ULONG_PTR ZeroBits,                    // arg3
///     SIZE_T CommitSize,                     // arg4
///     PLARGE_INTEGER SectionOffset,          // arg5
///   )
///
/// Note: ViewSize serait arg6 (stack) mais on utilise section.max_size.
///
/// Dispatch vers SYS_MMAP anonyme (MAP_ANONYMOUS | MAP_PRIVATE).
pub fn adapt_nt_map_view_of_section(
    section_handle: u64,
    _process_handle: u64,
    base_address_ptr: u64,
    _zero_bits: u64,
    _commit_size: u64,
    _section_offset: u64,
) -> i64 {
    if base_address_ptr == 0 {
        return ntstatus::STATUS_INVALID_PARAMETER;
    }

    // Récupérer la taille de la section
    let section_size = {
        let objects = nt_threading::kernel_objects().lock();
        match objects.get(&section_handle) {
            Some(KernelObject::Section(s)) => s.max_size,
            _ => return ntstatus::STATUS_INVALID_HANDLE,
        }
    };

    // Arrondir à la page
    let page_size = kernel_config::memory::PAGE_SIZE as u64;
    let aligned_size = (section_size + page_size - 1) & !(page_size - 1);

    // SYS_MMAP(addr, length, prot, flags, fd, offset)
    // PROT_READ | PROT_WRITE = 3, MAP_ANONYMOUS | MAP_PRIVATE = 0x22
    let result = maios_syscall::dispatch(
        maios_syscall::nr::SYS_MMAP,
        0,              // addr = NULL (let kernel choose)
        aligned_size,   // length
        3,              // prot = PROT_READ | PROT_WRITE
        0x22,           // flags = MAP_ANONYMOUS | MAP_PRIVATE
        u64::MAX,       // fd = -1
        0,              // offset = 0
    );

    match result {
        Ok(mapped_addr) => {
            unsafe { *(base_address_ptr as *mut u64) = mapped_addr; }
            ntstatus::STATUS_SUCCESS
        }
        Err(_) => ntstatus::STATUS_NO_MEMORY,
    }
}

// =============================================================================
// NtQueryVirtualMemory (0x0023)
// =============================================================================

/// NtQueryVirtualMemory — retourne des informations sur une région mémoire.
///
///   NtQueryVirtualMemory(
///     HANDLE ProcessHandle,                       // arg0
///     PVOID BaseAddress,                          // arg1
///     MEMORY_INFORMATION_CLASS MemoryInfoClass,  // arg2
///     PVOID MemoryInformation,                   // arg3
///     SIZE_T MemoryInformationLength,            // arg4
///     PSIZE_T ReturnLength,                      // arg5
///   )
///
/// Seul MemoryBasicInformation (class 0) est supporté.
/// Retourne des valeurs par défaut : MEM_COMMIT, PAGE_READWRITE, MEM_PRIVATE.
pub fn adapt_nt_query_virtual_memory(
    _process_handle: u64,
    base_address: u64,
    info_class: u64,
    buffer: u64,
    length: u64,
    return_length_ptr: u64,
) -> i64 {
    // Seul MemoryBasicInformation (0) est supporté
    if info_class != 0 {
        return ntstatus::STATUS_NOT_IMPLEMENTED;
    }

    // MEMORY_BASIC_INFORMATION = 48 bytes sur x64
    const MBI_SIZE: u64 = 48;

    if buffer == 0 || length < MBI_SIZE {
        return ntstatus::STATUS_INVALID_PARAMETER;
    }

    let page_size = kernel_config::memory::PAGE_SIZE as u64;
    let aligned_base = base_address & !(page_size - 1);

    // struct MEMORY_BASIC_INFORMATION {
    //   PVOID  BaseAddress;           // +0
    //   PVOID  AllocationBase;        // +8
    //   DWORD  AllocationProtect;     // +16
    //   WORD   PartitionId;           // +20
    //   SIZE_T RegionSize;            // +24 (aligned to +24 on x64 with padding)
    //   DWORD  State;                 // +32
    //   DWORD  Protect;               // +36
    //   DWORD  Type;                  // +40
    // }
    unsafe {
        let p = buffer as *mut u8;
        // Zero-fill first
        core::ptr::write_bytes(p, 0, MBI_SIZE as usize);
        // BaseAddress
        *(p as *mut u64) = aligned_base;
        // AllocationBase
        *(p.add(8) as *mut u64) = aligned_base;
        // AllocationProtect = PAGE_READWRITE (0x04)
        *(p.add(16) as *mut u32) = 0x04;
        // PartitionId = 0 (already zeroed)
        // RegionSize = 1 page (default)
        *(p.add(24) as *mut u64) = page_size;
        // State = MEM_COMMIT (0x1000)
        *(p.add(32) as *mut u32) = 0x1000;
        // Protect = PAGE_READWRITE (0x04)
        *(p.add(36) as *mut u32) = 0x04;
        // Type = MEM_PRIVATE (0x20000)
        *(p.add(40) as *mut u32) = 0x20000;
    }

    if return_length_ptr != 0 {
        unsafe { *(return_length_ptr as *mut u64) = MBI_SIZE; }
    }

    ntstatus::STATUS_SUCCESS
}
