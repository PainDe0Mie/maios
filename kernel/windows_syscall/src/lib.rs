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

#[macro_use] extern crate alloc;

use alloc::string::{String, ToString};
use log::{warn, debug};
use maios_syscall::error::{SyscallResult, SyscallError, result_to_ntstatus};

/// Codes NTSTATUS Windows.
pub mod ntstatus {
    pub const STATUS_SUCCESS: i64 = 0x0000_0000;
    pub const STATUS_NOT_IMPLEMENTED: i64 = 0xC000_0002_u32 as i32 as i64;
    pub const STATUS_INVALID_PARAMETER: i64 = 0xC000_000D_u32 as i32 as i64;
    pub const STATUS_INVALID_HANDLE: i64 = 0xC000_0008_u32 as i32 as i64;
    pub const STATUS_NO_MEMORY: i64 = 0xC000_0017_u32 as i32 as i64;
    #[allow(dead_code)]
    pub const STATUS_ACCESS_DENIED: i64 = 0xC000_0022_u32 as i32 as i64;
    pub const STATUS_OBJECT_NAME_NOT_FOUND: i64 = 0xC000_0034_u32 as i32 as i64;
    pub const STATUS_OBJECT_NAME_COLLISION: i64 = 0xC000_0035_u32 as i32 as i64;
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

// =============================================================================
// Utilitaires NT → VFS
// =============================================================================

/// Convertit un chemin NT en chemin VFS MaiOS.
///
/// `\Device\HarddiskVolumeN\foo` → `/foo`
/// `\??\C:\foo` → `/foo`
/// `\\.\foo` → `/foo`
/// Normalise `\` → `/`.
fn nt_path_to_vfs(nt_path: &str) -> String {
    let mut path = nt_path.replace('\\', "/");

    // Strip common NT prefixes
    if let Some(rest) = path.strip_prefix("/Device/HarddiskVolume") {
        // Skip the volume number digit(s) and the following slash
        if let Some(pos) = rest.find('/') {
            path = rest[pos..].to_string();
        } else {
            path = String::from("/");
        }
    } else if let Some(rest) = path.strip_prefix("/??/") {
        // \??\C:\foo → strip drive letter too
        if rest.len() >= 2 && rest.as_bytes()[1] == b':' {
            path = rest[2..].to_string();
        } else {
            path = format!("/{}", rest);
        }
    } else if let Some(rest) = path.strip_prefix("//./") {
        path = format!("/{}", rest);
    }

    if path.is_empty() {
        path = String::from("/");
    }
    path
}

/// Lire une UNICODE_STRING NT depuis la mémoire du processus.
///
/// NT UNICODE_STRING: { Length: u16, MaxLength: u16, Buffer: *u16 }
unsafe fn read_nt_unicode_string(ptr: u64) -> Option<String> {
    if ptr == 0 {
        return None;
    }
    let length = *(ptr as *const u16) as usize; // Length in bytes
    let _max_length = *((ptr + 2) as *const u16);
    let buffer_ptr = *((ptr + 8) as *const u64); // offset 8 on x86_64 (padding)

    if buffer_ptr == 0 || length == 0 {
        return None;
    }

    let char_count = length / 2;
    let mut result = String::with_capacity(char_count);
    for i in 0..char_count {
        let wchar = *((buffer_ptr as *const u16).add(i));
        if wchar < 0x80 {
            result.push(wchar as u8 as char);
        } else {
            result.push('?'); // Non-ASCII: placeholder
        }
    }
    Some(result)
}

/// NT CreateDisposition values
mod create_disposition {
    pub const FILE_SUPERSEDE: u64 = 0;
    pub const FILE_OPEN: u64 = 1;
    pub const FILE_CREATE: u64 = 2;
    pub const FILE_OPEN_IF: u64 = 3;
    pub const FILE_OVERWRITE: u64 = 4;
    pub const FILE_OVERWRITE_IF: u64 = 5;
}

/// NT IO_STATUS_BLOCK information values
mod io_information {
    pub const FILE_SUPERSEDED: u64 = 0;
    pub const FILE_OPENED: u64 = 1;
    pub const FILE_CREATED: u64 = 2;
    pub const FILE_OVERWRITTEN: u64 = 3;
}

/// Adaptateur pour NtCreateFile.
///
/// a0 = FileHandle*          (out)
/// a1 = DesiredAccess
/// a2 = ObjectAttributes*    { Length, RootDir, ObjectName*, ... }
/// a3 = IoStatusBlock*       (out)
/// a4 = AllocationSize*      (optional)
/// a5 = FileAttributes | (ShareAccess << 32)  — packed due to 6-arg limit
///
/// Note: CreateDisposition et CreateOptions sont passés via les bits supérieurs
/// de a1 (DesiredAccess) pour contourner la limite de 6 arguments.
/// Bits [63:40] de a1 = CreateDisposition (3 bits) | CreateOptions (21 bits restants)
///
/// Alternative simple : on encode CreateDisposition dans les bits [35:32] de a5.
fn adapt_nt_create_file(
    file_handle_ptr: u64,
    desired_access: u64,
    object_attributes_ptr: u64,
    io_status_block_ptr: u64,
    _allocation_size_ptr: u64,
    extra: u64, // bits [2:0] = CreateDisposition, bits [31:3] = FileAttributes
) -> i64 {
    if file_handle_ptr == 0 || object_attributes_ptr == 0 {
        return ntstatus::STATUS_INVALID_PARAMETER;
    }

    // Lire ObjectAttributes.ObjectName (offset 16 sur x86_64)
    let object_name_ptr = unsafe { *((object_attributes_ptr + 16) as *const u64) };
    let nt_path = match unsafe { read_nt_unicode_string(object_name_ptr) } {
        Some(p) => p,
        None => return ntstatus::STATUS_INVALID_PARAMETER,
    };

    let vfs_path = nt_path_to_vfs(&nt_path);
    debug!("NtCreateFile: NT path=\"{}\" → VFS path=\"{}\"", nt_path, vfs_path);

    let disposition = extra & 0x7;
    let _ = desired_access; // TODO: map access flags

    // Construire un chemin C-string pour sys_open
    let mut path_bytes: alloc::vec::Vec<u8> = vfs_path.into_bytes();
    path_bytes.push(0); // null terminator
    let path_ptr = path_bytes.as_ptr() as u64;

    // Flags pour sys_open : 0 = read-only open existing
    // On mappe le disposition pour décider si on crée ou ouvre
    let flags: u64 = match disposition {
        create_disposition::FILE_OPEN => 0,           // O_RDONLY, must exist
        create_disposition::FILE_CREATE => 0x0241,    // O_WRONLY | O_CREAT | O_EXCL
        create_disposition::FILE_OPEN_IF => 0x0042,   // O_RDWR | O_CREAT
        create_disposition::FILE_OVERWRITE => 0x0201, // O_WRONLY | O_TRUNC
        create_disposition::FILE_OVERWRITE_IF => 0x0242, // O_WRONLY | O_CREAT | O_TRUNC
        create_disposition::FILE_SUPERSEDE => 0x0242, // same as OVERWRITE_IF
        _ => 0,
    };

    let result = maios_syscall::dispatch(
        maios_syscall::nr::SYS_OPEN,
        path_ptr,
        flags,
        0, 0, 0, 0,
    );

    // Ensure path_bytes lives until after dispatch reads the pointer
    let _ = &path_bytes;

    match result {
        Ok(handle) => {
            unsafe { *(file_handle_ptr as *mut u64) = handle; }
            if io_status_block_ptr != 0 {
                unsafe {
                    *(io_status_block_ptr as *mut i64) = ntstatus::STATUS_SUCCESS;
                    let info = match disposition {
                        create_disposition::FILE_CREATE => io_information::FILE_CREATED,
                        create_disposition::FILE_OPEN => io_information::FILE_OPENED,
                        _ => io_information::FILE_OPENED,
                    };
                    *((io_status_block_ptr + 8) as *mut u64) = info;
                }
            }
            ntstatus::STATUS_SUCCESS
        }
        Err(SyscallError::NotFound) => {
            ntstatus::STATUS_OBJECT_NAME_NOT_FOUND
        }
        Err(SyscallError::FileExists) => {
            ntstatus::STATUS_OBJECT_NAME_COLLISION
        }
        Err(_) => {
            ntstatus::STATUS_INVALID_PARAMETER
        }
    }
}

/// Stub minimal pour NtQueryInformationFile.
///
/// Retourne STATUS_SUCCESS avec des données vides pour ne pas crasher
/// les appels type GetFileSize() / GetFileType().
fn adapt_nt_query_information_file(
    _file_handle: u64,
    io_status_block_ptr: u64,
    file_info_ptr: u64,
    length: u64,
    _info_class: u64,
) -> i64 {
    // Zéro-fill le buffer de sortie
    if file_info_ptr != 0 && length > 0 {
        let len = length as usize;
        unsafe {
            core::ptr::write_bytes(file_info_ptr as *mut u8, 0, len);
        }
    }
    if io_status_block_ptr != 0 {
        unsafe {
            *(io_status_block_ptr as *mut i64) = ntstatus::STATUS_SUCCESS;
            *((io_status_block_ptr + 8) as *mut u64) = 0;
        }
    }
    ntstatus::STATUS_SUCCESS
}

/// Stub pour NtQuerySystemInformation.
///
/// Retourne STATUS_SUCCESS avec buffer zéro pour ne pas crasher
/// les appels basiques (SystemBasicInformation, etc.).
fn adapt_nt_query_system_information(
    info_class: u64,
    buffer_ptr: u64,
    buffer_length: u64,
    return_length_ptr: u64,
) -> i64 {
    debug!("NtQuerySystemInformation: class={}", info_class);

    if buffer_ptr != 0 && buffer_length > 0 {
        unsafe {
            core::ptr::write_bytes(buffer_ptr as *mut u8, 0, buffer_length as usize);
        }
    }
    if return_length_ptr != 0 {
        unsafe { *(return_length_ptr as *mut u32) = 0; }
    }
    ntstatus::STATUS_SUCCESS
}

/// Stub pour NtQueryInformationProcess.
///
/// Supporte ProcessBasicInformation (class 0) avec des valeurs par défaut.
fn adapt_nt_query_information_process(
    _process_handle: u64,
    info_class: u64,
    buffer_ptr: u64,
    buffer_length: u64,
    return_length_ptr: u64,
) -> i64 {
    debug!("NtQueryInformationProcess: class={}", info_class);

    if buffer_ptr != 0 && buffer_length > 0 {
        unsafe {
            core::ptr::write_bytes(buffer_ptr as *mut u8, 0, buffer_length as usize);
        }
    }
    if return_length_ptr != 0 {
        unsafe { *(return_length_ptr as *mut u32) = buffer_length as u32; }
    }
    ntstatus::STATUS_SUCCESS
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
        nr::NT_CREATE_FILE => {
            return adapt_nt_create_file(arg0, arg1, arg2, arg3, arg4, arg5);
        }
        nr::NT_QUERY_INFORMATION_FILE => {
            return adapt_nt_query_information_file(arg0, arg1, arg2, arg3, arg4);
        }
        nr::NT_QUERY_SYSTEM_INFORMATION => {
            return adapt_nt_query_system_information(arg0, arg1, arg2, arg3);
        }
        nr::NT_QUERY_INFORMATION_PROCESS => {
            return adapt_nt_query_information_process(arg0, arg1, arg2, arg3, arg4);
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
        _ => {}
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
