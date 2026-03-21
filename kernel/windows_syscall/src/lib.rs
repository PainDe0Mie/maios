//! Couche de traduction Windows NT → MaiOS.
//!
//! Ce crate est un mapper mince qui traduit les numéros de syscall NT
//! en numéros MaiOS natifs, avec des fonctions d'adaptation pour les
//! particularités de l'ABI NT (pointeur-vers-valeur, NTSTATUS, etc.).
//!
//! ## Convention NT x86_64
//!
//! - RAX = numéro de syscall (bits 12..13 = index table service)
//! - Arguments : R10 (original RCX), RDX, R8, R9
//! - Retour : NTSTATUS dans RAX

#![no_std]

extern crate alloc;

use log::warn;
use maios_syscall::error::{SyscallResult, SyscallError, result_to_ntstatus};

pub mod nt_threading;

/// Codes NTSTATUS Windows.
pub mod ntstatus {
    pub const STATUS_SUCCESS: i64 = 0x0000_0000;
    pub const STATUS_NOT_IMPLEMENTED: i64 = 0xC000_0002_u32 as i32 as i64;
    pub const STATUS_INVALID_PARAMETER: i64 = 0xC000_000D_u32 as i32 as i64;
    pub const STATUS_INVALID_HANDLE: i64 = 0xC000_0008_u32 as i32 as i64;
    pub const STATUS_NO_MEMORY: i64 = 0xC000_0017_u32 as i32 as i64;
    #[allow(dead_code)]
    pub const STATUS_ACCESS_DENIED: i64 = 0xC000_0022_u32 as i32 as i64;
    #[allow(dead_code)]
    pub const STATUS_OBJECT_NAME_NOT_FOUND: i64 = 0xC000_0034_u32 as i32 as i64;
    #[allow(dead_code)]
    pub const STATUS_BUFFER_TOO_SMALL: i64 = 0xC000_0023_u32 as i32 as i64;
}

/// Numéros de syscall Windows NT (Windows 10 21H2+ / build 19044+).
pub mod nr {
    pub const NT_CLOSE: u64 = 0x000F;
    pub const NT_TERMINATE_PROCESS: u64 = 0x002C;
    pub const NT_ALLOCATE_VIRTUAL_MEMORY: u64 = 0x0018;
    pub const NT_FREE_VIRTUAL_MEMORY: u64 = 0x001E;
    pub const NT_PROTECT_VIRTUAL_MEMORY: u64 = 0x0050;
    pub const NT_READ_FILE: u64 = 0x0006;
    pub const NT_WRITE_FILE: u64 = 0x0008;
    pub const NT_CREATE_FILE: u64 = 0x0055;
    pub const NT_QUERY_INFORMATION_FILE: u64 = 0x0011;
    pub const NT_QUERY_SYSTEM_INFORMATION: u64 = 0x0036;
    pub const NT_QUERY_PERFORMANCE_COUNTER: u64 = 0x0031;
    pub const NT_QUERY_INFORMATION_PROCESS: u64 = 0x0019;

    // Threading & synchronization
    pub const NT_WAIT_FOR_SINGLE_OBJECT: u64 = 0x0004;
    pub const NT_WAIT_FOR_MULTIPLE_OBJECTS: u64 = 0x000B;
    pub const NT_SET_EVENT: u64 = 0x000E;
    pub const NT_RELEASE_MUTANT: u64 = 0x001B;
    pub const NT_RESET_EVENT: u64 = 0x0028;
    pub const NT_CREATE_EVENT: u64 = 0x0048;
    pub const NT_CREATE_MUTANT: u64 = 0x004B;
    pub const NT_CREATE_THREAD_EX: u64 = 0x00C2;
}

// =============================================================================
// Table de traduction NT → MaiOS
// =============================================================================

const UNMAPPED: u16 = 0xFFFF;

/// Table de correspondance : index = numéro NT service, valeur = numéro MaiOS.
///
/// Taille : 256 entrées × 2 octets = 512 octets. Lookup O(1).
static NT_TO_MAIOS: [u16; 256] = {
    let mut table = [UNMAPPED; 256];

    // File I/O
    table[0x06] = maios_syscall::nr::SYS_READ;       // NtReadFile
    table[0x08] = maios_syscall::nr::SYS_WRITE;      // NtWriteFile
    table[0x0F] = maios_syscall::nr::SYS_CLOSE;      // NtClose

    // Memory (avec adaptateurs — pas de dispatch direct)
    // 0x18 et 0x1E sont traités séparément via adaptateurs
    table[0x50] = maios_syscall::nr::SYS_MPROTECT;   // NtProtectVirtualMemory

    // Process
    table[0x2C] = maios_syscall::nr::SYS_EXIT;       // NtTerminateProcess

    // System info
    table[0x31] = maios_syscall::nr::SYS_PERF_COUNTER; // NtQueryPerformanceCounter

    table
};

// =============================================================================
// Adaptateurs NT spécifiques
// =============================================================================

/// Adaptateur pour NtAllocateVirtualMemory.
///
/// NT passe des pointeurs-vers-valeurs (*BaseAddress, *RegionSize)
/// alors que MaiOS sys_alloc_vm prend des valeurs directes.
fn adapt_nt_allocate_vm(
    _process_handle: u64,
    base_addr_ptr: u64,
    _zero_bits: u64,
    region_size_ptr: u64,
    alloc_type: u64,
    protect: u64,
) -> i64 {
    if region_size_ptr == 0 {
        return ntstatus::STATUS_INVALID_PARAMETER;
    }

    let requested_size = unsafe { *(region_size_ptr as *const u64) };

    if requested_size == 0 {
        return ntstatus::STATUS_INVALID_PARAMETER;
    }

    let result = maios_syscall::dispatch(
        maios_syscall::nr::SYS_ALLOC_VM,
        requested_size,
        protect,
        alloc_type,
        0, 0, 0,
    );

    match result {
        Ok(base_addr) => {
            // Écrire l'adresse de base dans le pointeur du caller
            if base_addr_ptr != 0 {
                unsafe { *(base_addr_ptr as *mut u64) = base_addr; }
            }
            // Écrire la taille réelle (arrondie aux pages)
            let page_size = kernel_config::memory::PAGE_SIZE as u64;
            let actual_size = (requested_size + page_size - 1) & !(page_size - 1);
            unsafe { *(region_size_ptr as *mut u64) = actual_size; }
            ntstatus::STATUS_SUCCESS
        }
        Err(e) => e.to_ntstatus(),
    }
}

/// Adaptateur pour NtFreeVirtualMemory.
///
/// NT passe *BaseAddress (pointeur-vers-pointeur).
fn adapt_nt_free_vm(
    _process_handle: u64,
    base_addr_ptr: u64,
    _region_size_ptr: u64,
    free_type: u64,
) -> i64 {
    if base_addr_ptr == 0 {
        return ntstatus::STATUS_INVALID_PARAMETER;
    }

    let base_addr = unsafe { *(base_addr_ptr as *const u64) };

    let result = maios_syscall::dispatch(
        maios_syscall::nr::SYS_FREE_VM,
        base_addr,
        free_type,
        0, 0, 0, 0,
    );

    result_to_ntstatus(result)
}

/// Adaptateur pour NtWriteFile avec handles console.
///
/// NT utilise des handles spéciaux (0x07 stdout, 0x0B stderr).
/// Le dispatch vers sys_write fonctionne directement car la ResourceTable
/// mappe ces handles aux mêmes ressources Stdout/Stderr.
fn adapt_nt_write_file(handle: u64, _event: u64, buffer: u64, length: u64) -> i64 {
    if buffer == 0 && length > 0 {
        return ntstatus::STATUS_INVALID_PARAMETER;
    }

    let result = maios_syscall::dispatch(
        maios_syscall::nr::SYS_WRITE,
        handle, buffer, length,
        0, 0, 0,
    );

    result_to_ntstatus(result)
}

/// Adaptateur pour NtQueryPerformanceCounter.
///
/// Dispatch directement — les arguments (counter_ptr, freq_ptr) sont
/// déjà dans le bon format pour sys_perf_counter.
fn adapt_nt_perf_counter(counter_out: u64, frequency_out: u64) -> i64 {
    let result = maios_syscall::dispatch(
        maios_syscall::nr::SYS_PERF_COUNTER,
        counter_out, frequency_out,
        0, 0, 0, 0,
    );

    result_to_ntstatus(result)
}

/// Adaptateur pour NtQueryInformationFile.
///
/// NT prototype:
///   NTSTATUS NtQueryInformationFile(
///     HANDLE FileHandle,                     // arg0
///     PIO_STATUS_BLOCK IoStatusBlock,        // arg1
///     PVOID FileInformation,                 // arg2
///     ULONG Length,                          // arg3
///     FILE_INFORMATION_CLASS FileInfoClass   // arg4
///   );
fn adapt_nt_query_information_file(
    handle: u64,
    io_status_block: u64,
    buffer: u64,
    length: u64,
    info_class: u64,
) -> i64 {
    use maios_syscall::resource::{self, Resource};

    if buffer == 0 || length == 0 {
        return ntstatus::STATUS_INVALID_PARAMETER;
    }

    let tid = task::get_my_current_task_id();

    let result = resource::with_resources(tid, |table| {
        let resource = match table.get(handle) {
            Some(r) => r,
            None => return ntstatus::STATUS_INVALID_HANDLE,
        };

        match resource {
            Resource::File { file, offset } => {
                let locked = file.lock();
                let file_size = locked.len() as u64;
                let file_name = locked.get_name();
                let current_offset = *offset as u64;
                drop(locked);

                match info_class {
                    // FileStandardInformation (5)
                    5 => {
                        // struct FILE_STANDARD_INFORMATION {
                        //   LARGE_INTEGER AllocationSize;   // +0
                        //   LARGE_INTEGER EndOfFile;        // +8
                        //   ULONG         NumberOfLinks;    // +16
                        //   BOOLEAN       DeletePending;    // +20
                        //   BOOLEAN       Directory;        // +21
                        // } // size = 24
                        if length < 24 {
                            return ntstatus::STATUS_BUFFER_TOO_SMALL;
                        }
                        unsafe {
                            let p = buffer as *mut u8;
                            // AllocationSize (round up to 4K)
                            let alloc_size = (file_size + 4095) & !4095;
                            *(p as *mut u64) = alloc_size;
                            // EndOfFile
                            *(p.add(8) as *mut u64) = file_size;
                            // NumberOfLinks
                            *(p.add(16) as *mut u32) = 1;
                            // DeletePending
                            *p.add(20) = 0;
                            // Directory
                            *p.add(21) = 0;
                        }
                        write_io_status(io_status_block, 0, 24);
                        ntstatus::STATUS_SUCCESS
                    }
                    // FileBasicInformation (4)
                    4 => {
                        // struct FILE_BASIC_INFORMATION {
                        //   LARGE_INTEGER CreationTime;     // +0
                        //   LARGE_INTEGER LastAccessTime;   // +8
                        //   LARGE_INTEGER LastWriteTime;    // +16
                        //   LARGE_INTEGER ChangeTime;       // +24
                        //   ULONG         FileAttributes;   // +32
                        // } // size = 40
                        if length < 40 {
                            return ntstatus::STATUS_BUFFER_TOO_SMALL;
                        }
                        unsafe {
                            let p = buffer as *mut u8;
                            // All timestamps = 0 (not tracked yet)
                            for i in 0..4 {
                                *(p.add(i * 8) as *mut u64) = 0;
                            }
                            // FILE_ATTRIBUTE_NORMAL = 0x80
                            *(p.add(32) as *mut u32) = 0x80;
                        }
                        write_io_status(io_status_block, 0, 40);
                        ntstatus::STATUS_SUCCESS
                    }
                    // FileNameInformation (9)
                    9 => {
                        // struct FILE_NAME_INFORMATION {
                        //   ULONG FileNameLength;  // +0  (bytes, not chars)
                        //   WCHAR FileName[1];     // +4  (UTF-16LE)
                        // }
                        let utf16: alloc::vec::Vec<u16> = file_name.encode_utf16().collect();
                        let name_bytes = utf16.len() * 2;
                        let needed = 4 + name_bytes;
                        if (length as usize) < needed {
                            return ntstatus::STATUS_BUFFER_TOO_SMALL;
                        }
                        unsafe {
                            let p = buffer as *mut u8;
                            *(p as *mut u32) = name_bytes as u32;
                            core::ptr::copy_nonoverlapping(
                                utf16.as_ptr() as *const u8,
                                p.add(4),
                                name_bytes,
                            );
                        }
                        write_io_status(io_status_block, 0, needed as u64);
                        ntstatus::STATUS_SUCCESS
                    }
                    // FilePositionInformation (14)
                    14 => {
                        if length < 8 {
                            return ntstatus::STATUS_BUFFER_TOO_SMALL;
                        }
                        unsafe {
                            *(buffer as *mut u64) = current_offset;
                        }
                        write_io_status(io_status_block, 0, 8);
                        ntstatus::STATUS_SUCCESS
                    }
                    other => {
                        warn!("NtQueryInformationFile: unhandled info class {}", other);
                        ntstatus::STATUS_NOT_IMPLEMENTED
                    }
                }
            }
            Resource::Stdin | Resource::Stdout | Resource::Stderr => {
                // Console handles: return minimal info
                match info_class {
                    5 => {
                        if length < 24 {
                            return ntstatus::STATUS_BUFFER_TOO_SMALL;
                        }
                        unsafe {
                            let p = buffer as *mut u8;
                            core::ptr::write_bytes(p, 0, 24);
                        }
                        write_io_status(io_status_block, 0, 24);
                        ntstatus::STATUS_SUCCESS
                    }
                    _ => ntstatus::STATUS_NOT_IMPLEMENTED,
                }
            }
            _ => ntstatus::STATUS_INVALID_HANDLE,
        }
    });

    result
}

/// Write an IO_STATUS_BLOCK: { NTSTATUS Status; ULONG_PTR Information; }
fn write_io_status(io_status_block: u64, status: i64, info: u64) {
    if io_status_block != 0 {
        unsafe {
            let p = io_status_block as *mut u8;
            *(p as *mut i64) = status;
            *(p.add(8) as *mut u64) = info;
        }
    }
}

// =============================================================================
// Point d'entrée
// =============================================================================

/// Point d'entrée pour le handling des syscalls Windows NT.
///
/// Extrait l'index de table service (bits 12..13), puis traduit
/// le numéro de service en numéro MaiOS via lookup O(1) ou
/// adaptateur spécifique.
pub fn handle_syscall(
    num: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
) -> i64 {
    let table_index = (num >> 12) & 0x3;
    let service_num = (num & 0xFFF) as usize;

    if table_index != 0 {
        warn!(
            "windows_syscall: Win32k table not implemented (table={}, service={})",
            table_index, service_num
        );
        return ntstatus::STATUS_NOT_IMPLEMENTED;
    }

    // Syscalls nécessitant des adaptateurs spécifiques (argument conversion)
    match service_num as u64 {
        nr::NT_ALLOCATE_VIRTUAL_MEMORY => {
            return adapt_nt_allocate_vm(arg0, arg1, arg2, arg3, arg4, arg5);
        }
        nr::NT_FREE_VIRTUAL_MEMORY => {
            return adapt_nt_free_vm(arg0, arg1, arg2, arg3);
        }
        nr::NT_WRITE_FILE => {
            return adapt_nt_write_file(arg0, arg1, arg2, arg3);
        }
        nr::NT_QUERY_PERFORMANCE_COUNTER => {
            return adapt_nt_perf_counter(arg0, arg1);
        }
        nr::NT_TERMINATE_PROCESS => {
            // NtTerminateProcess : handle -1 = processus courant
            if arg0 == 0xFFFF_FFFF_FFFF_FFFF || arg0 == 0 {
                let result = maios_syscall::dispatch(
                    maios_syscall::nr::SYS_EXIT,
                    arg1, 0, 0, 0, 0, 0,
                );
                return result_to_ntstatus(result);
            }
            return ntstatus::STATUS_NOT_IMPLEMENTED;
        }

        // --- Threading & Synchronization ---
        nr::NT_CREATE_THREAD_EX => {
            return nt_threading::adapt_nt_create_thread_ex(arg0, arg1, arg2, arg3, arg4, arg5);
        }
        nr::NT_CREATE_EVENT => {
            return nt_threading::adapt_nt_create_event(arg0, arg1, arg2, arg3, arg4);
        }
        nr::NT_SET_EVENT => {
            return nt_threading::adapt_nt_set_event(arg0, arg1);
        }
        nr::NT_RESET_EVENT => {
            return nt_threading::adapt_nt_reset_event(arg0, arg1);
        }
        nr::NT_CREATE_MUTANT => {
            return nt_threading::adapt_nt_create_mutant(arg0, arg1, arg2, arg3);
        }
        nr::NT_RELEASE_MUTANT => {
            return nt_threading::adapt_nt_release_mutant(arg0, arg1);
        }
        nr::NT_WAIT_FOR_SINGLE_OBJECT => {
            return nt_threading::adapt_nt_wait_for_single_object(arg0, arg1, arg2);
        }
        nr::NT_WAIT_FOR_MULTIPLE_OBJECTS => {
            return nt_threading::adapt_nt_wait_for_multiple_objects(arg0, arg1, arg2, arg3, arg4);
        }

        nr::NT_QUERY_INFORMATION_FILE => {
            return adapt_nt_query_information_file(arg0, arg1, arg2, arg3, arg4);
        }
        nr::NT_QUERY_SYSTEM_INFORMATION | nr::NT_QUERY_INFORMATION_PROCESS => {
            // Stub: zero-fill the buffer and return success
            if arg1 != 0 && arg2 > 0 {
                unsafe { core::ptr::write_bytes(arg1 as *mut u8, 0, arg2 as usize); }
            }
            return ntstatus::STATUS_SUCCESS;
        }

        _ => {}
    }

    // NtClose (0x0F) : essayer d'abord de fermer un objet kernel NT.
    // Si c'est un handle kernel (Event/Mutant/Thread), on le traite ici.
    // Sinon, on le laisse passer au dispatch normal (fichier/resource).
    if service_num == 0x0F {
        if nt_threading::try_close_kernel_object(arg0) {
            return ntstatus::STATUS_SUCCESS;
        }
        // Pas un objet kernel → continuer vers SYS_CLOSE normal
    }

    // Lookup dans la table de traduction pour les syscalls sans adaptation
    let maios_nr = if service_num < NT_TO_MAIOS.len() {
        NT_TO_MAIOS[service_num]
    } else {
        UNMAPPED
    };

    if maios_nr == UNMAPPED {
        warn!(
            "windows_syscall: unmapped NT syscall {:#x} (args: {:#x}, {:#x}, {:#x}, {:#x})",
            service_num, arg0, arg1, arg2, arg3
        );
        return ntstatus::STATUS_NOT_IMPLEMENTED;
    }

    let result = maios_syscall::dispatch(maios_nr, arg0, arg1, arg2, arg3, arg4, arg5);
    result_to_ntstatus(result)
}
