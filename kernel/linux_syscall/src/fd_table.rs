//! Per-task file descriptor table for Linux syscall compatibility.
//!
//! Each task running a Linux binary gets its own `FdTable`, which maps
//! integer file descriptors (0, 1, 2, 3, ...) to MaiOS VFS files or
//! special I/O streams (stdin, stdout, stderr).
//!
//! The global `FD_TABLES` map is keyed by MaiOS task ID and protected
//! by a `spin::Mutex` so it can be used from interrupt / syscall context
//! without blocking.

use alloc::collections::BTreeMap;
use fs_node::FileRef;
use spin::Mutex;
use lazy_static::lazy_static;

// ---------------------------------------------------------------------------
// FdEntry -- what a single file descriptor points to
// ---------------------------------------------------------------------------

/// A file descriptor entry.
pub enum FdEntry {
    /// Regular file backed by MaiOS VFS.
    File {
        file: FileRef,
        offset: usize,
    },
    /// stdout -- routes to kernel log (and app_io when available).
    Stdout,
    /// stderr -- routes to kernel log (and app_io when available).
    Stderr,
    /// stdin -- routes to app_io (returns EAGAIN when unavailable).
    Stdin,
}

#[allow(dead_code)]
impl FdEntry {
    /// If this is a `File` entry, return its current length via `KnownLength`.
    pub fn file_len(&self) -> Option<usize> {
        match self {
            FdEntry::File { file, .. } => Some(file.lock().len()),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// FdTable -- per-task descriptor table
// ---------------------------------------------------------------------------

/// Per-task file descriptor table.
///
/// File descriptors 0/1/2 are pre-populated as stdin/stdout/stderr.
/// New descriptors are allocated starting from 3 upward, and closed
/// descriptors are **not** reused (simple monotonic allocator) to avoid
/// use-after-close races in naive userspace code.
pub struct FdTable {
    fds: BTreeMap<u64, FdEntry>,
    next_fd: u64,
}

impl FdTable {
    /// Create a new table with the standard three descriptors pre-installed.
    pub fn new() -> Self {
        let mut fds = BTreeMap::new();
        fds.insert(0, FdEntry::Stdin);
        fds.insert(1, FdEntry::Stdout);
        fds.insert(2, FdEntry::Stderr);
        FdTable { fds, next_fd: 3 }
    }

    /// Open a VFS file and return the new file descriptor number.
    pub fn open(&mut self, file: FileRef) -> u64 {
        let fd = self.next_fd;
        self.next_fd += 1;
        self.fds.insert(fd, FdEntry::File { file, offset: 0 });
        fd
    }

    /// Close a file descriptor. Returns `true` if it existed.
    pub fn close(&mut self, fd: u64) -> bool {
        self.fds.remove(&fd).is_some()
    }

    /// Immutable access to an entry.
    pub fn get(&self, fd: u64) -> Option<&FdEntry> {
        self.fds.get(&fd)
    }

    /// Mutable access to an entry (needed to update offset on read/write).
    pub fn get_mut(&mut self, fd: u64) -> Option<&mut FdEntry> {
        self.fds.get_mut(&fd)
    }
}

// ---------------------------------------------------------------------------
// Global table: task-id -> FdTable
// ---------------------------------------------------------------------------

lazy_static! {
    /// Global map of task ID to its file descriptor table.
    ///
    /// A table is lazily created the first time a task issues a file I/O
    /// syscall.  It is **not** automatically cleaned up when the task exits;
    /// the exit handler should call `remove_table()`.
    static ref FD_TABLES: Mutex<BTreeMap<usize, FdTable>> = Mutex::new(BTreeMap::new());
}

/// Return a lock guard to the global map.  All public helpers below use
/// this so that the lock is held for the shortest possible duration.
fn tables() -> spin::MutexGuard<'static, BTreeMap<usize, FdTable>> {
    FD_TABLES.lock()
}

/// Ensure the given task has an `FdTable`, creating one if needed.
#[allow(dead_code)]
pub fn ensure_table(task_id: usize) {
    let mut map = tables();
    if !map.contains_key(&task_id) {
        map.insert(task_id, FdTable::new());
    }
}

/// Remove a task's table entirely (call on task exit).
pub fn remove_table(task_id: usize) {
    tables().remove(&task_id);
}

/// Execute a closure with shared (`&`) access to a task's `FdTable`.
///
/// If the task has no table yet, one is created automatically.
pub fn with_table<F, R>(task_id: usize, f: F) -> R
where
    F: FnOnce(&FdTable) -> R,
{
    let mut map = tables();
    let table = map.entry(task_id).or_insert_with(FdTable::new);
    f(table)
}

/// Execute a closure with exclusive (`&mut`) access to a task's `FdTable`.
///
/// If the task has no table yet, one is created automatically.
pub fn with_table_mut<F, R>(task_id: usize, f: F) -> R
where
    F: FnOnce(&mut FdTable) -> R,
{
    let mut map = tables();
    let table = map.entry(task_id).or_insert_with(FdTable::new);
    f(table)
}
