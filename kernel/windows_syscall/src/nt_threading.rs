//! NT threading & synchronization primitives.
//!
//! Implémente les objets kernel Windows (Event, Mutant) et les syscalls
//! de threading (NtCreateThreadEx, NtWaitForSingleObject, etc.).
//!
//! ## Architecture
//!
//! Les objets kernel sont stockés dans une table globale `KERNEL_OBJECTS`
//! indexée par handle NT (multiples de 4, à partir de 0x1000 pour éviter
//! les collisions avec les handles stdio/fichier).
//!
//! Chaque objet peut être :
//! - **Event** : signaled/nonsignaled, auto-reset ou manual-reset
//! - **Mutant** : compteur de récursion, propriétaire = task ID
//! - **Thread** : référence vers un TaskRef pour join

extern crate alloc;

use alloc::collections::BTreeMap;
use spin::Mutex;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::ntstatus;

/// Handle de départ pour les objets kernel NT (évite les collisions avec
/// les handles fichier qui commencent à 0x03).
const KERNEL_HANDLE_BASE: u64 = 0x1000;

/// Pas entre handles (multiple de 4, convention NT).
const KERNEL_HANDLE_STEP: u64 = 4;

/// Prochain handle à allouer.
static NEXT_KERNEL_HANDLE: AtomicU64 = AtomicU64::new(KERNEL_HANDLE_BASE);

/// Alloue un nouveau handle kernel.
fn alloc_kernel_handle() -> u64 {
    NEXT_KERNEL_HANDLE.fetch_add(KERNEL_HANDLE_STEP, Ordering::Relaxed)
}

// =============================================================================
// Objets kernel
// =============================================================================

/// Type d'événement NT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    /// NotificationEvent — reste signalé jusqu'à NtResetEvent.
    ManualReset,
    /// SynchronizationEvent — auto-reset après un seul waiter réveillé.
    AutoReset,
}

/// Objet Event NT.
#[derive(Debug)]
pub struct NtEvent {
    pub event_type: EventType,
    pub signaled: bool,
}

/// Objet Mutant (mutex récursif) NT.
#[derive(Debug)]
pub struct NtMutant {
    /// Task ID du propriétaire actuel, ou 0 si libre.
    pub owner: usize,
    /// Compteur de récursion (>0 si verrouillé).
    pub recursion_count: u32,
    /// Si true, le mutant a été abandonné (propriétaire mort).
    pub abandoned: bool,
}

/// Objet Thread NT — wrapping d'un task ID pour le join.
#[derive(Debug)]
pub struct NtThread {
    pub task_id: usize,
}

/// Union de tous les types d'objets kernel NT.
#[derive(Debug)]
pub enum KernelObject {
    Event(NtEvent),
    Mutant(NtMutant),
    Thread(NtThread),
}

/// Table globale des objets kernel NT.
static KERNEL_OBJECTS: Mutex<BTreeMap<u64, KernelObject>> = Mutex::new(BTreeMap::new());

// =============================================================================
// NtCreateThreadEx (0x00B7 sur Win10+)
// =============================================================================

/// NT syscall number pour NtCreateThreadEx.
pub const NT_CREATE_THREAD_EX: u64 = 0x00C2;

/// Adaptateur NtCreateThreadEx.
///
/// Signature NT simplifiée :
///   NtCreateThreadEx(
///     OUT PHANDLE ThreadHandle,          // arg0 (R10)
///     ACCESS_MASK DesiredAccess,          // arg1 (RDX)
///     POBJECT_ATTRIBUTES ObjectAttributes,// arg2 (R8)
///     HANDLE ProcessHandle,              // arg3 (R9)
///     PVOID StartRoutine,                // arg4 (stack)
///     PVOID Argument,                    // arg5 (stack)
///   )
///
/// On crée un nouveau thread MaiOS qui exécute `StartRoutine(Argument)`.
pub fn adapt_nt_create_thread_ex(
    thread_handle_ptr: u64,
    _desired_access: u64,
    _object_attributes: u64,
    _process_handle: u64,
    start_routine: u64,
    argument: u64,
) -> i64 {
    if thread_handle_ptr == 0 || start_routine == 0 {
        return ntstatus::STATUS_INVALID_PARAMETER;
    }

    // Fonction wrapper qui exécute le StartRoutine NT.
    // Le start_routine est un pointeur de fonction cdecl/x64 qui prend un PVOID.
    let entry = start_routine;
    let arg = argument;

    let spawn_result = spawn::new_task_builder(
        move |_task_id: usize| -> isize {
            // Appel direct de la routine de démarrage NT.
            // On utilise un appel indirect via un pointeur de fonction.
            let func: extern "C" fn(u64) -> u64 = unsafe {
                core::mem::transmute(entry as usize)
            };
            let ret = func(arg);
            ret as isize
        },
        0usize, // task_id placeholder, sera écrasé
    )
    .name(alloc::string::String::from("nt_thread"))
    .spawn();

    match spawn_result {
        Ok(joinable) => {
            let task_id = joinable.0.id;

            // Créer un objet Thread dans la table kernel
            let handle = alloc_kernel_handle();
            {
                let mut objects = KERNEL_OBJECTS.lock();
                objects.insert(handle, KernelObject::Thread(NtThread { task_id }));
            }

            // Écrire le handle dans le pointeur de sortie
            unsafe {
                *(thread_handle_ptr as *mut u64) = handle;
            }

            // On ne garde pas le JoinableTaskRef — le join sera fait via
            // NtWaitForSingleObject en polled mode sur le RunState.
            core::mem::forget(joinable);

            ntstatus::STATUS_SUCCESS
        }
        Err(_e) => {
            ntstatus::STATUS_NO_MEMORY
        }
    }
}

// =============================================================================
// NtCreateEvent (0x0048)
// =============================================================================

pub const NT_CREATE_EVENT: u64 = 0x0048;

/// Adaptateur NtCreateEvent.
///
///   NtCreateEvent(
///     OUT PHANDLE EventHandle,            // arg0
///     ACCESS_MASK DesiredAccess,           // arg1
///     POBJECT_ATTRIBUTES ObjectAttributes, // arg2
///     EVENT_TYPE EventType,               // arg3  (0=NotificationEvent, 1=SynchronizationEvent)
///     BOOLEAN InitialState,               // arg4  (TRUE=signaled)
///   )
pub fn adapt_nt_create_event(
    event_handle_ptr: u64,
    _desired_access: u64,
    _object_attributes: u64,
    event_type: u64,
    initial_state: u64,
) -> i64 {
    if event_handle_ptr == 0 {
        return ntstatus::STATUS_INVALID_PARAMETER;
    }

    let etype = if event_type == 0 {
        EventType::ManualReset
    } else {
        EventType::AutoReset
    };

    let handle = alloc_kernel_handle();
    {
        let mut objects = KERNEL_OBJECTS.lock();
        objects.insert(handle, KernelObject::Event(NtEvent {
            event_type: etype,
            signaled: initial_state != 0,
        }));
    }

    unsafe { *(event_handle_ptr as *mut u64) = handle; }

    ntstatus::STATUS_SUCCESS
}

// =============================================================================
// NtSetEvent (0x000E) / NtResetEvent (0x0028)
// =============================================================================

pub const NT_SET_EVENT: u64 = 0x000E;
pub const NT_RESET_EVENT: u64 = 0x0028;

/// NtSetEvent — met un événement en état signalé.
///
///   NtSetEvent(HANDLE EventHandle, OUT PLONG PreviousState)
pub fn adapt_nt_set_event(event_handle: u64, previous_state_ptr: u64) -> i64 {
    let mut objects = KERNEL_OBJECTS.lock();
    match objects.get_mut(&event_handle) {
        Some(KernelObject::Event(ev)) => {
            let prev = ev.signaled as i64;
            ev.signaled = true;
            if previous_state_ptr != 0 {
                unsafe { *(previous_state_ptr as *mut i64) = prev; }
            }
            ntstatus::STATUS_SUCCESS
        }
        _ => ntstatus::STATUS_INVALID_HANDLE,
    }
}

/// NtResetEvent — remet un événement en état non-signalé.
///
///   NtResetEvent(HANDLE EventHandle, OUT PLONG PreviousState)
pub fn adapt_nt_reset_event(event_handle: u64, previous_state_ptr: u64) -> i64 {
    let mut objects = KERNEL_OBJECTS.lock();
    match objects.get_mut(&event_handle) {
        Some(KernelObject::Event(ev)) => {
            let prev = ev.signaled as i64;
            ev.signaled = false;
            if previous_state_ptr != 0 {
                unsafe { *(previous_state_ptr as *mut i64) = prev; }
            }
            ntstatus::STATUS_SUCCESS
        }
        _ => ntstatus::STATUS_INVALID_HANDLE,
    }
}

// =============================================================================
// NtCreateMutant (0x004B)
// =============================================================================

pub const NT_CREATE_MUTANT: u64 = 0x004B;

/// NtCreateMutant — crée un mutex récursif.
///
///   NtCreateMutant(
///     OUT PHANDLE MutantHandle,
///     ACCESS_MASK DesiredAccess,
///     POBJECT_ATTRIBUTES ObjectAttributes,
///     BOOLEAN InitialOwner,               // arg3: si TRUE, le thread courant est propriétaire
///   )
pub fn adapt_nt_create_mutant(
    mutant_handle_ptr: u64,
    _desired_access: u64,
    _object_attributes: u64,
    initial_owner: u64,
) -> i64 {
    if mutant_handle_ptr == 0 {
        return ntstatus::STATUS_INVALID_PARAMETER;
    }

    let owner = if initial_owner != 0 {
        task::get_my_current_task_id()
    } else {
        0
    };

    let handle = alloc_kernel_handle();
    {
        let mut objects = KERNEL_OBJECTS.lock();
        objects.insert(handle, KernelObject::Mutant(NtMutant {
            owner,
            recursion_count: if owner != 0 { 1 } else { 0 },
            abandoned: false,
        }));
    }

    unsafe { *(mutant_handle_ptr as *mut u64) = handle; }

    ntstatus::STATUS_SUCCESS
}

// =============================================================================
// NtReleaseMutant (0x001B)
// =============================================================================

pub const NT_RELEASE_MUTANT: u64 = 0x001B;

/// NtReleaseMutant — libère un mutex.
///
///   NtReleaseMutant(HANDLE MutantHandle, OUT PLONG PreviousCount)
pub fn adapt_nt_release_mutant(mutant_handle: u64, previous_count_ptr: u64) -> i64 {
    let tid = task::get_my_current_task_id();

    let mut objects = KERNEL_OBJECTS.lock();
    match objects.get_mut(&mutant_handle) {
        Some(KernelObject::Mutant(m)) => {
            if m.owner != tid {
                return ntstatus::STATUS_INVALID_HANDLE; // STATUS_MUTANT_NOT_OWNED
            }
            let prev = m.recursion_count as i64;
            m.recursion_count -= 1;
            if m.recursion_count == 0 {
                m.owner = 0;
            }
            if previous_count_ptr != 0 {
                unsafe { *(previous_count_ptr as *mut i64) = prev; }
            }
            ntstatus::STATUS_SUCCESS
        }
        _ => ntstatus::STATUS_INVALID_HANDLE,
    }
}

// =============================================================================
// NtWaitForSingleObject (0x0004)
// =============================================================================

pub const NT_WAIT_FOR_SINGLE_OBJECT: u64 = 0x0004;

/// NtWaitForSingleObject — attend qu'un objet devienne signalé.
///
///   NtWaitForSingleObject(
///     HANDLE Handle,              // arg0
///     BOOLEAN Alertable,          // arg1
///     PLARGE_INTEGER Timeout,     // arg2 (NULL=infini, négatif=relatif en 100ns units)
///   )
pub fn adapt_nt_wait_for_single_object(
    handle: u64,
    _alertable: u64,
    timeout_ptr: u64,
) -> i64 {
    // Calculer le timeout en millisecondes
    let timeout_ms: Option<u64> = if timeout_ptr != 0 {
        let raw = unsafe { *(timeout_ptr as *const i64) };
        if raw < 0 {
            // Timeout relatif en unités de 100ns (négatif = relatif dans NT)
            let hundreds_ns = (-raw) as u64;
            Some(hundreds_ns / 10_000) // convertir en ms
        } else if raw == 0 {
            Some(0) // test instantané
        } else {
            // Timeout absolu — on le traite comme relatif (simplification)
            Some(raw as u64 / 10_000)
        }
    } else {
        None // attente infinie
    };

    // Cap à 30 secondes pour éviter les deadlocks
    let max_ms = timeout_ms.unwrap_or(30_000).min(30_000);
    let poll_interval = sleep::Duration::from_millis(1);
    let mut elapsed_ms: u64 = 0;

    loop {
        // Vérifier l'état de l'objet
        let status = check_and_acquire(handle);
        match status {
            WaitResult::Satisfied => return ntstatus::STATUS_SUCCESS,
            WaitResult::Abandoned => return 0x0000_0080, // STATUS_ABANDONED_WAIT_0
            WaitResult::InvalidHandle => return ntstatus::STATUS_INVALID_HANDLE,
            WaitResult::NotReady => {}
        }

        if elapsed_ms >= max_ms {
            return 0x0000_0102_u32 as i32 as i64; // STATUS_TIMEOUT
        }

        let _ = sleep::sleep(poll_interval);
        elapsed_ms += 1;
    }
}

/// Résultat d'une tentative d'acquisition d'objet.
enum WaitResult {
    Satisfied,
    Abandoned,
    InvalidHandle,
    NotReady,
}

/// Vérifie si un objet est signalé et l'acquiert si possible.
fn check_and_acquire(handle: u64) -> WaitResult {
    let mut objects = KERNEL_OBJECTS.lock();
    match objects.get_mut(&handle) {
        Some(KernelObject::Event(ev)) => {
            if ev.signaled {
                if ev.event_type == EventType::AutoReset {
                    ev.signaled = false; // auto-reset
                }
                WaitResult::Satisfied
            } else {
                WaitResult::NotReady
            }
        }
        Some(KernelObject::Mutant(m)) => {
            let tid = task::get_my_current_task_id();
            if m.owner == 0 {
                // Libre → acquérir
                m.owner = tid;
                m.recursion_count = 1;
                if m.abandoned {
                    m.abandoned = false;
                    WaitResult::Abandoned
                } else {
                    WaitResult::Satisfied
                }
            } else if m.owner == tid {
                // Récursion
                m.recursion_count += 1;
                WaitResult::Satisfied
            } else {
                WaitResult::NotReady
            }
        }
        Some(KernelObject::Thread(t)) => {
            // Vérifier si le thread a terminé
            if let Some(task_ref) = task::get_task(t.task_id) {
                match task_ref.runstate.load() {
                    task::RunState::Exited(_) => WaitResult::Satisfied,
                    _ => WaitResult::NotReady,
                }
            } else {
                // Task introuvable — probablement déjà nettoyée
                WaitResult::Satisfied
            }
        }
        None => WaitResult::InvalidHandle,
    }
}

// =============================================================================
// NtWaitForMultipleObjects (0x000B)
// =============================================================================

pub const NT_WAIT_FOR_MULTIPLE_OBJECTS: u64 = 0x000B;

/// NtWaitForMultipleObjects — attend qu'un ou tous les objets deviennent signalés.
///
///   NtWaitForMultipleObjects(
///     ULONG Count,                 // arg0
///     HANDLE* Handles,             // arg1
///     WAIT_TYPE WaitType,          // arg2 (0=WaitAll, 1=WaitAny)
///     BOOLEAN Alertable,           // arg3
///     PLARGE_INTEGER Timeout,      // arg4
///   )
pub fn adapt_nt_wait_for_multiple_objects(
    count: u64,
    handles_ptr: u64,
    wait_type: u64,
    _alertable: u64,
    timeout_ptr: u64,
) -> i64 {
    if count == 0 || count > 64 || handles_ptr == 0 {
        return ntstatus::STATUS_INVALID_PARAMETER;
    }

    let n = count as usize;
    let handles = unsafe {
        core::slice::from_raw_parts(handles_ptr as *const u64, n)
    };

    let timeout_ms: Option<u64> = if timeout_ptr != 0 {
        let raw = unsafe { *(timeout_ptr as *const i64) };
        if raw < 0 {
            Some((-raw) as u64 / 10_000)
        } else if raw == 0 {
            Some(0)
        } else {
            Some(raw as u64 / 10_000)
        }
    } else {
        None
    };

    let max_ms = timeout_ms.unwrap_or(30_000).min(30_000);
    let poll_interval = sleep::Duration::from_millis(1);
    let mut elapsed_ms: u64 = 0;
    let wait_all = wait_type == 0;

    loop {
        if wait_all {
            // WaitAll: tous doivent être prêts
            let all_ready = handles.iter().all(|&h| {
                matches!(check_and_acquire(h), WaitResult::Satisfied | WaitResult::Abandoned)
            });
            if all_ready {
                return ntstatus::STATUS_SUCCESS;
            }
        } else {
            // WaitAny: le premier prêt gagne
            for (i, &h) in handles.iter().enumerate() {
                match check_and_acquire(h) {
                    WaitResult::Satisfied => return i as i64, // STATUS_WAIT_0 + index
                    WaitResult::Abandoned => return 0x80 + i as i64, // STATUS_ABANDONED_WAIT_0 + index
                    _ => {}
                }
            }
        }

        if elapsed_ms >= max_ms {
            return 0x0000_0102_u32 as i32 as i64; // STATUS_TIMEOUT
        }

        let _ = sleep::sleep(poll_interval);
        elapsed_ms += 1;
    }
}

// =============================================================================
// NtClose pour objets kernel (extension)
// =============================================================================

/// Ferme un handle kernel. Retourne true si c'était un objet kernel (traité),
/// false si ce n'est pas un handle kernel (le caller doit essayer d'autres tables).
pub fn try_close_kernel_object(handle: u64) -> bool {
    let mut objects = KERNEL_OBJECTS.lock();
    objects.remove(&handle).is_some()
}
