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

// =============================================================================
// Threading stubs (Phase 1B)
// =============================================================================

/// sys_set_tid_address — register the clear_child_tid pointer.
///
/// Called by glibc/musl at thread startup. The kernel writes 0 to this address
/// and does a FUTEX_WAKE when the thread dies (for pthread_join).
/// Stub: just return the current tid.
pub fn sys_set_tid_address(_tidptr: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    // TODO: store tidptr in task struct for cleanup notification
    let tid = task::get_my_current_task_id();
    Ok(tid as u64)
}

/// sys_set_robust_list — register the robust futex list head.
///
/// Called once per thread by glibc. If not implemented, glibc may crash.
/// Stub: accept and ignore.
pub fn sys_set_robust_list(_head: u64, _len: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    // len must be 24 (sizeof(struct robust_list_head))
    Ok(0)
}

/// sys_prlimit64 — get/set resource limits.
///
/// glibc calls this at startup for RLIMIT_STACK.
/// Stub: return sensible defaults, ignore set operations.
pub fn sys_prlimit64(_pid: u64, resource: u64, _new_limit: u64, old_limit: u64, _: u64, _: u64) -> SyscallResult {
    const RLIMIT_STACK: u64 = 3;
    const RLIMIT_NOFILE: u64 = 7;
    const RLIMIT_AS: u64 = 9;
    const RLIM_INFINITY: u64 = u64::MAX;

    if old_limit != 0 {
        // struct rlimit { rlim_cur: u64, rlim_max: u64 }
        let (cur, max) = match resource {
            RLIMIT_STACK => (8 * 1024 * 1024, 64 * 1024 * 1024), // 8MB / 64MB
            RLIMIT_NOFILE => (1024, 4096),
            RLIMIT_AS => (RLIM_INFINITY, RLIM_INFINITY),
            _ => (RLIM_INFINITY, RLIM_INFINITY),
        };
        unsafe {
            let ptr = old_limit as *mut [u64; 2];
            (*ptr)[0] = cur;
            (*ptr)[1] = max;
        }
    }

    // Ignore new_limit (we don't enforce resource limits)
    Ok(0)
}

/// sys_sched_yield — voluntarily give up the CPU.
pub fn sys_sched_yield(_: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    scheduler::schedule();
    Ok(0)
}

/// sys_gettimeofday — legacy time syscall.
///
/// Many older programs use this instead of clock_gettime.
/// struct timeval { tv_sec: i64, tv_usec: i64 }
pub fn sys_gettimeofday(tv: u64, _tz: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    if tv != 0 {
        let elapsed = time::Instant::now().duration_since(time::Instant::ZERO);
        // For a proper implementation we'd add the RTC epoch like clock_gettime does,
        // but for now just return monotonic time as a reasonable approximation.
        unsafe {
            let ptr = tv as *mut [i64; 2];
            (*ptr)[0] = elapsed.as_secs() as i64;
            (*ptr)[1] = (elapsed.subsec_nanos() / 1000) as i64; // microseconds
        }
    }
    Ok(0)
}

// =============================================================================
// Process management (Phase 2)
// =============================================================================

/// sys_wait4 — wait for a child process to change state.
///
/// Simplified: MaiOS doesn't have real parent-child tracking yet.
/// Returns ECHILD (no child processes) since we don't track children.
pub fn sys_wait4(_pid: u64, _wstatus: u64, options: u64, _rusage: u64, _: u64, _: u64) -> SyscallResult {
    let options = options as i32;
    const WNOHANG: i32 = 1;

    if options & WNOHANG != 0 {
        // Non-blocking: no children, return 0 (nothing happened)
        return Ok(0);
    }

    // Blocking wait: we have no children to wait for
    Err(SyscallError::NoChild)
}

/// sys_kill — send a signal to a process.
///
/// Stub: since we don't deliver signals, just validate the target exists.
/// Signal 0 is used to check if a process exists.
pub fn sys_kill(pid: u64, sig: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    let pid = pid as i64;

    if sig > 64 {
        return Err(SyscallError::InvalidArgument);
    }

    // Signal 0: check if process exists — always succeed
    if sig == 0 {
        return Ok(0);
    }

    // SIGKILL/SIGTERM to self: exit
    if (sig == 9 || sig == 15) && (pid <= 0 || pid == task::get_my_current_task_id() as i64) {
        return sys_exit(0, 0, 0, 0, 0, 0);
    }

    // For other signals: accept silently (we don't deliver signals yet)
    Ok(0)
}

/// sys_tgkill — send a signal to a specific thread.
///
/// Stub: same as kill but with thread group awareness.
pub fn sys_tgkill(_tgid: u64, _tid: u64, sig: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    if sig > 64 {
        return Err(SyscallError::InvalidArgument);
    }
    Ok(0)
}
