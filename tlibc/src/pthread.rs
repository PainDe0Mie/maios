//! POSIX threads (`<pthread.h>`) implementation for MaiOS.
//!
//! Uses Theseus's `spawn` and `task` kernel APIs since we're in kernel space.

use libc::{c_int, c_void};
use errno::*;
use core::ptr;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use spin::Mutex;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;

// ---------------------------------------------------------------------------
// Types matching POSIX
// ---------------------------------------------------------------------------

pub type pthread_t = u64;
pub type pthread_key_t = u32;

#[repr(C)]
pub struct pthread_attr_t {
    _detach_state: c_int,
    _stack_size: usize,
}

#[repr(C)]
pub struct pthread_mutex_t {
    lock: AtomicU64,  // 0 = unlocked, tid = locked
}

#[repr(C)]
pub struct pthread_mutexattr_t {
    _kind: c_int,
}

#[repr(C)]
pub struct pthread_cond_t {
    _seq: AtomicU64,
}

#[repr(C)]
pub struct pthread_condattr_t {
    _dummy: c_int,
}

#[repr(C)]
pub struct pthread_once_t {
    state: AtomicU64, // 0 = not called, 1 = in progress, 2 = done
}

pub const PTHREAD_MUTEX_INITIALIZER: pthread_mutex_t = pthread_mutex_t { lock: AtomicU64::new(0) };
pub const PTHREAD_COND_INITIALIZER: pthread_cond_t = pthread_cond_t { _seq: AtomicU64::new(0) };
pub const PTHREAD_ONCE_INIT: pthread_once_t = pthread_once_t { state: AtomicU64::new(0) };

// ---------------------------------------------------------------------------
// Thread-local storage (TLS keys)
// ---------------------------------------------------------------------------

static NEXT_KEY: AtomicUsize = AtomicUsize::new(1);
static TLS_DESTRUCTORS: Mutex<BTreeMap<u32, Option<unsafe extern "C" fn(*mut c_void)>>> =
    Mutex::new(BTreeMap::new());

// Per-thread TLS values (simplified: global map keyed by (tid, key))
static TLS_VALUES: Mutex<BTreeMap<(u64, u32), *mut c_void>> =
    Mutex::new(BTreeMap::new());

fn current_tid() -> u64 {
    task::get_my_current_task()
        .map(|t| u64::from(usize::from(t.id)))
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Thread creation / join
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pthread_create(
    thread: *mut pthread_t,
    _attr: *const pthread_attr_t,
    start_routine: extern "C" fn(*mut c_void) -> *mut c_void,
    arg: *mut c_void,
) -> c_int {
    if thread.is_null() {
        return EINVAL;
    }

    // We wrap the C start routine in a Rust closure for Theseus's spawn API.
    // The arg pointer is sent as a raw u64.
    let arg_val = arg as u64;
    let func = start_routine;

    let task_result = spawn::new_task_builder(
        move |_: ()| -> c_int {
            let ret = func(arg_val as *mut c_void);
            ret as c_int  // discard the void* return for now
        },
        ()
    )
    .and_then(|builder| builder.spawn());

    match task_result {
        Ok(task_ref) => {
            *thread = u64::from(usize::from(task_ref.id));
            0
        }
        Err(_e) => {
            error!("pthread_create failed: {}", _e);
            EAGAIN
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn pthread_join(thread: pthread_t, _retval: *mut *mut c_void) -> c_int {
    // Theseus doesn't have a direct join API.
    // We busy-wait on the task's exit status (simple approach).
    let tid = thread as usize;
    loop {
        if let Some(task_ref) = task::get_task(tid.into()) {
            if task_ref.has_exited() {
                break;
            }
        } else {
            return ESRCH;
        }
        // Yield to avoid busy-spin
        scheduler::schedule();
    }
    0
}

#[no_mangle]
pub extern "C" fn pthread_self() -> pthread_t {
    current_tid()
}

#[no_mangle]
pub extern "C" fn pthread_equal(t1: pthread_t, t2: pthread_t) -> c_int {
    (t1 == t2) as c_int
}

#[no_mangle]
pub unsafe extern "C" fn pthread_detach(_thread: pthread_t) -> c_int {
    // Theseus tasks are always "detached" in a sense
    0
}

// ---------------------------------------------------------------------------
// Mutex
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_init(
    mutex: *mut pthread_mutex_t,
    _attr: *const pthread_mutexattr_t,
) -> c_int {
    if mutex.is_null() { return EINVAL; }
    (*mutex).lock.store(0, Ordering::Release);
    0
}

#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_lock(mutex: *mut pthread_mutex_t) -> c_int {
    if mutex.is_null() { return EINVAL; }
    let tid = current_tid();
    // Simple spinlock with yield
    loop {
        match (*mutex).lock.compare_exchange(0, tid, Ordering::Acquire, Ordering::Relaxed) {
            Ok(_) => return 0,
            Err(_) => { core::hint::spin_loop(); }
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_trylock(mutex: *mut pthread_mutex_t) -> c_int {
    if mutex.is_null() { return EINVAL; }
    let tid = current_tid();
    match (*mutex).lock.compare_exchange(0, tid, Ordering::Acquire, Ordering::Relaxed) {
        Ok(_) => 0,
        Err(_) => EBUSY,
    }
}

#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_unlock(mutex: *mut pthread_mutex_t) -> c_int {
    if mutex.is_null() { return EINVAL; }
    (*mutex).lock.store(0, Ordering::Release);
    0
}

#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_destroy(_mutex: *mut pthread_mutex_t) -> c_int {
    0
}

// ---------------------------------------------------------------------------
// Condition variables (simplified spin-based implementation)
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pthread_cond_init(
    cond: *mut pthread_cond_t,
    _attr: *const pthread_condattr_t,
) -> c_int {
    if cond.is_null() { return EINVAL; }
    (*cond)._seq.store(0, Ordering::Release);
    0
}

#[no_mangle]
pub unsafe extern "C" fn pthread_cond_wait(
    cond: *mut pthread_cond_t,
    mutex: *mut pthread_mutex_t,
) -> c_int {
    if cond.is_null() || mutex.is_null() { return EINVAL; }
    let seq = (*cond)._seq.load(Ordering::Acquire);
    pthread_mutex_unlock(mutex);
    // Wait for signal (sequence number change)
    while (*cond)._seq.load(Ordering::Acquire) == seq {
        core::hint::spin_loop();
    }
    pthread_mutex_lock(mutex);
    0
}

#[no_mangle]
pub unsafe extern "C" fn pthread_cond_signal(cond: *mut pthread_cond_t) -> c_int {
    if cond.is_null() { return EINVAL; }
    (*cond)._seq.fetch_add(1, Ordering::Release);
    0
}

#[no_mangle]
pub unsafe extern "C" fn pthread_cond_broadcast(cond: *mut pthread_cond_t) -> c_int {
    pthread_cond_signal(cond)
}

#[no_mangle]
pub unsafe extern "C" fn pthread_cond_destroy(_cond: *mut pthread_cond_t) -> c_int {
    0
}

// ---------------------------------------------------------------------------
// Thread-local storage keys
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pthread_key_create(
    key: *mut pthread_key_t,
    destructor: Option<unsafe extern "C" fn(*mut c_void)>,
) -> c_int {
    if key.is_null() { return EINVAL; }
    let k = NEXT_KEY.fetch_add(1, Ordering::Relaxed) as u32;
    TLS_DESTRUCTORS.lock().insert(k, destructor);
    *key = k;
    0
}

#[no_mangle]
pub unsafe extern "C" fn pthread_key_delete(key: pthread_key_t) -> c_int {
    TLS_DESTRUCTORS.lock().remove(&key);
    0
}

#[no_mangle]
pub unsafe extern "C" fn pthread_getspecific(key: pthread_key_t) -> *mut c_void {
    let tid = current_tid();
    TLS_VALUES.lock().get(&(tid, key)).copied().unwrap_or(ptr::null_mut())
}

#[no_mangle]
pub unsafe extern "C" fn pthread_setspecific(key: pthread_key_t, value: *const c_void) -> c_int {
    let tid = current_tid();
    TLS_VALUES.lock().insert((tid, key), value as *mut c_void);
    0
}

// ---------------------------------------------------------------------------
// Once
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pthread_once(
    once_control: *mut pthread_once_t,
    init_routine: extern "C" fn(),
) -> c_int {
    if once_control.is_null() { return EINVAL; }
    // Try to transition from 0 (not called) to 1 (in progress)
    if (*once_control).state.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_ok() {
        init_routine();
        (*once_control).state.store(2, Ordering::Release);
    } else {
        // Wait for completion
        while (*once_control).state.load(Ordering::Acquire) != 2 {
            core::hint::spin_loop();
        }
    }
    0
}

// ---------------------------------------------------------------------------
// Mutex/cond attributes (stubs)
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pthread_mutexattr_init(_attr: *mut pthread_mutexattr_t) -> c_int { 0 }
#[no_mangle]
pub unsafe extern "C" fn pthread_mutexattr_destroy(_attr: *mut pthread_mutexattr_t) -> c_int { 0 }
#[no_mangle]
pub unsafe extern "C" fn pthread_mutexattr_settype(_attr: *mut pthread_mutexattr_t, _kind: c_int) -> c_int { 0 }
#[no_mangle]
pub unsafe extern "C" fn pthread_condattr_init(_attr: *mut pthread_condattr_t) -> c_int { 0 }
#[no_mangle]
pub unsafe extern "C" fn pthread_condattr_destroy(_attr: *mut pthread_condattr_t) -> c_int { 0 }
#[no_mangle]
pub unsafe extern "C" fn pthread_attr_init(_attr: *mut pthread_attr_t) -> c_int { 0 }
#[no_mangle]
pub unsafe extern "C" fn pthread_attr_destroy(_attr: *mut pthread_attr_t) -> c_int { 0 }
#[no_mangle]
pub unsafe extern "C" fn pthread_attr_setstacksize(attr: *mut pthread_attr_t, stacksize: usize) -> c_int {
    if attr.is_null() { return EINVAL; }
    (*attr)._stack_size = stacksize;
    0
}
