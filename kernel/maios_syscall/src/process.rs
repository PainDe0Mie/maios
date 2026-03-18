//! Syscalls de gestion de processus unifiés pour MaiOS.
//!
//! Consolide les implémentations de linux_syscall (exit, getpid, execve)
//! et windows_syscall (NtTerminateProcess) en une seule source.

use alloc::vec;
use log::{debug, warn};

use crate::error::{SyscallResult, SyscallError};
use crate::resource;

/// Lire une chaîne C terminée par null depuis un pointeur.
unsafe fn read_c_string(ptr: u64) -> Option<alloc::string::String> {
    if ptr == 0 {
        return None;
    }
    let mut p = ptr as *const u8;
    let mut len = 0usize;
    while len < 4096 {
        if *p == 0 { break; }
        p = p.add(1);
        len += 1;
    }
    if len == 0 || len >= 4096 {
        return None;
    }
    let slice = core::slice::from_raw_parts(ptr as *const u8, len);
    core::str::from_utf8(slice).ok().map(|s| alloc::string::String::from(s))
}

// =============================================================================
// Implémentations
// =============================================================================

/// sys_exit — terminer la tâche courante.
pub fn sys_exit(status: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    let status = status as i32;
    debug!("sys_exit(status={})", status);

    let tid = task::get_my_current_task_id();

    // Nettoyer la table de ressources (libère les MappedPages, etc.)
    resource::remove_resources(tid);

    // Tuer la tâche courante
    let kill_result = task::with_current_task(|t| {
        t.kill(task::KillReason::Requested)
    });

    match kill_result {
        Ok(Ok(())) => {
            debug!("sys_exit: task killed, scheduling away");
        }
        Ok(Err(state)) => {
            warn!("sys_exit: could not kill task (state: {:?})", state);
        }
        Err(e) => {
            warn!("sys_exit: no current task: {}", e);
        }
    }

    task::scheduler::schedule();
    Ok(0) // unreachable
}

/// sys_getpid — obtenir l'ID de la tâche courante.
pub fn sys_getpid(_: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    match task::with_current_task(|t| t.0.id) {
        Ok(id) => Ok(id as u64),
        Err(_) => Ok(1),
    }
}

/// sys_getppid — obtenir l'ID du parent.
pub fn sys_getppid(_: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    Ok(1) // Toujours retourner init PID
}

/// sys_gettid — obtenir l'ID du thread (= PID en modèle single-thread).
pub fn sys_gettid(_: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    sys_getpid(0, 0, 0, 0, 0, 0)
}

/// sys_execve — exécuter un binaire ELF.
pub fn sys_execve(path_ptr: u64, _argv_ptr: u64, _envp_ptr: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    debug!("sys_execve(path={:#x})", path_ptr);

    let path_str = match unsafe { read_c_string(path_ptr) } {
        Some(s) => s,
        None => return Err(SyscallError::Fault),
    };
    debug!("sys_execve: path = \"{}\"", path_str);

    // Résoudre le fichier dans le VFS MaiOS
    let root_dir = root::get_root();
    let p = path::Path::new(&path_str);
    let file_ref = match p.get_file(root_dir) {
        Some(f) => f,
        None => {
            warn!("sys_execve: not found \"{}\"", path_str);
            return Err(SyscallError::NotFound);
        }
    };

    // Lire le fichier entier
    let file_len = {
        let locked = file_ref.lock();
        io::KnownLength::len(&*locked)
    };
    if file_len == 0 {
        return Err(SyscallError::NotExecutable);
    }

    let mut elf_data = vec![0u8; file_len];
    {
        let mut locked = file_ref.lock();
        match io::ByteReader::read_at(&mut *locked, &mut elf_data, 0) {
            Ok(_) => {}
            Err(_) => return Err(SyscallError::IoError),
        }
    }

    // Valider le header ELF
    if elf_loader::parse_header(&elf_data).is_err() {
        warn!("sys_execve: \"{}\" is not a valid ELF64 binary", path_str);
        return Err(SyscallError::NotExecutable);
    }

    // Spawner une nouvelle tâche qui charge et exécute l'ELF
    let task_name = alloc::format!("elf_{}", path_str);

    let task_result = spawn::new_task_builder(move |_: ()| -> isize {
        match elf_loader::load(&elf_data) {
            Ok(loaded) => {
                let entry = loaded.entry_point.value();
                debug!("sys_execve: jumping to ELF entry point at {:#x}", entry);
                let _segments = loaded.segments;
                let entry_fn: extern "C" fn() -> ! = unsafe {
                    core::mem::transmute(entry)
                };
                entry_fn();
            }
            Err(e) => {
                log::error!("sys_execve: elf_loader::load failed: {}", e);
                -1
            }
        }
    }, ())
    .name(task_name)
    .spawn();

    match task_result {
        Ok(_) => {
            debug!("sys_execve: ELF task spawned, killing current task");
            sys_exit(0, 0, 0, 0, 0, 0)
        }
        Err(e) => {
            warn!("sys_execve: failed to spawn task: {}", e);
            Err(SyscallError::OutOfMemory)
        }
    }
}

/// sys_exit_group — en modèle single-thread, identique à sys_exit.
pub fn sys_exit_group(status: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    debug!("sys_exit_group(status={})", status);
    sys_exit(status, 0, 0, 0, 0, 0)
}

// Stubs d'identité (MaiOS est mono-utilisateur)

pub fn sys_getuid(_: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult { Ok(0) }
pub fn sys_getgid(_: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult { Ok(0) }
pub fn sys_geteuid(_: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult { Ok(0) }
pub fn sys_getegid(_: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult { Ok(0) }
