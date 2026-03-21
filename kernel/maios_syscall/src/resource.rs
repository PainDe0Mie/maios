//! Unified resource table for MaiOS syscalls.
//!
//! Replaces both the Linux `fd_table` (file descriptors 0, 1, 2, 3, ...)
//! and the Windows `HandleTable` (handles 0x03, 0x07, 0x0B, 0x100, ...).
//!
//! A single `ResourceTable` per task manages all kernel object references
//! regardless of which ABI the task uses. Linux tasks allocate sequential
//! fds via `alloc_fd()`; Windows tasks allocate 4-aligned handles via
//! `alloc_handle()`.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use spin::Mutex;

#[cfg(target_arch = "x86_64")]
use fs_node::FileRef;
#[cfg(target_arch = "x86_64")]
use memory::MappedPages;
#[cfg(target_arch = "x86_64")]
use smoltcp::iface::SocketHandle;
#[cfg(target_arch = "x86_64")]
use net::NetworkInterface;

// ---------------------------------------------------------------------------
// Resource enum — what a handle/fd points to
// ---------------------------------------------------------------------------

/// The types of kernel resources that a task can hold.
#[cfg(target_arch = "x86_64")]
pub enum Resource {
    /// Standard input stream.
    Stdin,
    /// Standard output stream.
    Stdout,
    /// Standard error stream.
    Stderr,
    /// A VFS file with a current read/write offset.
    File {
        file: FileRef,
        offset: usize,
    },
    /// An anonymous memory mapping (from mmap or NtAllocateVirtualMemory).
    /// Dropping the `MappedPages` unmaps the memory region.
    Memory {
        pages: MappedPages,
        base: usize,
        size: usize,
    },
    /// A network socket (TCP or UDP) backed by smoltcp.
    Socket {
        handle: SocketHandle,
        interface: Arc<NetworkInterface>,
        sock_type: SocketKind,
    },
}

/// The kind of network socket.
#[cfg(target_arch = "x86_64")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketKind {
    Tcp,
    Udp,
}

/// A handle/descriptor number visible to userspace.
pub type ResourceHandle = u64;

// ---------------------------------------------------------------------------
// ResourceTable — per-task resource management
// ---------------------------------------------------------------------------

/// Per-task resource table.
///
/// Manages the mapping from userspace handles to kernel resources.
/// Supports both Linux-style sequential fds (0, 1, 2, 3, ...)
/// and Windows-style 4-aligned handles (0x03, 0x07, 0x0B, 0x100, ...).
///
/// Uses `BTreeMap` instead of a flat array because Windows handles are
/// sparse. For Linux-only tasks, BTreeMap handles sequential keys
/// efficiently (cache-friendly node layout).
#[cfg(target_arch = "x86_64")]
pub struct ResourceTable {
    resources: BTreeMap<ResourceHandle, Resource>,
    /// Next Linux-style fd to allocate (starts at 3, after stdin/stdout/stderr).
    next_fd: u64,
    /// Next Windows-style handle to allocate (starts at 0x100, 4-byte aligned).
    next_handle: u64,
}

#[cfg(target_arch = "x86_64")]
impl ResourceTable {
    /// Create a new table with stdin/stdout/stderr pre-installed.
    ///
    /// The standard streams are dual-mapped:
    /// - Linux fds: 0 (stdin), 1 (stdout), 2 (stderr)
    /// - Windows handles: 0x03, 0x07, 0x0B
    ///
    /// This costs 6 BTreeMap entries but avoids indirection complexity.
    pub fn new() -> Self {
        let mut resources = BTreeMap::new();
        // Linux fd mappings
        resources.insert(0, Resource::Stdin);
        resources.insert(1, Resource::Stdout);
        resources.insert(2, Resource::Stderr);
        // Windows handle mappings
        resources.insert(0x03, Resource::Stdin);
        resources.insert(0x07, Resource::Stdout);
        resources.insert(0x0B, Resource::Stderr);

        ResourceTable {
            resources,
            next_fd: 3,
            next_handle: 0x100,
        }
    }

    /// Allocate a Linux-style file descriptor (small sequential integer).
    ///
    /// Returns the new fd number. FDs are never reused within a task
    /// to prevent use-after-close races.
    pub fn alloc_fd(&mut self, resource: Resource) -> u64 {
        let fd = self.next_fd;
        self.next_fd += 1;
        self.resources.insert(fd, resource);
        fd
    }

    /// Allocate a Windows-style handle (4-byte aligned, starting at 0x100).
    ///
    /// Returns the new handle value.
    pub fn alloc_handle(&mut self, resource: Resource) -> u64 {
        let handle = self.next_handle;
        self.next_handle += 4;
        self.resources.insert(handle, resource);
        handle
    }

    /// Close a resource by handle/fd.
    ///
    /// Returns the removed resource if it existed. Refuses to close
    /// standard stdio entries (Linux fds 0-2, Windows handles 0x03/0x07/0x0B).
    pub fn close(&mut self, handle: u64) -> Option<Resource> {
        match handle {
            0 | 1 | 2 | 0x03 | 0x07 | 0x0B => None,
            _ => self.resources.remove(&handle),
        }
    }

    /// Check if a handle is a protected stdio entry (cannot be closed).
    pub fn is_stdio(handle: u64) -> bool {
        matches!(handle, 0 | 1 | 2 | 0x03 | 0x07 | 0x0B)
    }

    /// Get an immutable reference to a resource.
    pub fn get(&self, handle: u64) -> Option<&Resource> {
        self.resources.get(&handle)
    }

    /// Get a mutable reference to a resource (for updating file offset, etc.).
    pub fn get_mut(&mut self, handle: u64) -> Option<&mut Resource> {
        self.resources.get_mut(&handle)
    }

    /// Duplicate a resource entry (for dup/dup2 syscalls).
    ///
    /// Returns Some(new_fd) on success, None if the source handle doesn't
    /// exist or isn't duplicable (e.g., Memory resources).
    pub fn dup(&mut self, src_handle: u64) -> Option<u64> {
        let dup_resource = match self.resources.get(&src_handle)? {
            Resource::Stdin => Some(Resource::Stdin),
            Resource::Stdout => Some(Resource::Stdout),
            Resource::Stderr => Some(Resource::Stderr),
            Resource::File { file, offset } => Some(Resource::File {
                file: file.clone(),
                offset: *offset,
            }),
            Resource::Memory { .. } => None, // Cannot dup memory mappings
            Resource::Socket { handle, ref interface, sock_type } => Some(Resource::Socket {
                handle: *handle,
                interface: interface.clone(),
                sock_type: *sock_type,
            }),
        }?;
        Some(self.alloc_fd(dup_resource))
    }

    /// Duplicate a resource entry onto a specific target fd (for dup2).
    ///
    /// If `target_fd` already exists, it is closed first.
    /// Returns the target fd on success, None if source isn't duplicable.
    pub fn dup2(&mut self, src_handle: u64, target_fd: u64) -> Option<u64> {
        if src_handle == target_fd {
            // dup2(fd, fd) is a no-op if fd is valid
            return if self.resources.contains_key(&src_handle) { Some(target_fd) } else { None };
        }
        let dup_resource = match self.resources.get(&src_handle)? {
            Resource::Stdin => Some(Resource::Stdin),
            Resource::Stdout => Some(Resource::Stdout),
            Resource::Stderr => Some(Resource::Stderr),
            Resource::File { file, offset } => Some(Resource::File {
                file: file.clone(),
                offset: *offset,
            }),
            Resource::Memory { .. } => None,
            Resource::Socket { handle, ref interface, sock_type } => Some(Resource::Socket {
                handle: *handle,
                interface: interface.clone(),
                sock_type: *sock_type,
            }),
        }?;
        // Close target if it exists (ignore if it's stdio)
        if !Self::is_stdio(target_fd) {
            self.resources.remove(&target_fd);
        }
        self.resources.insert(target_fd, dup_resource);
        Some(target_fd)
    }

    /// Find a handle by matching a predicate on the resource.
    ///
    /// Used for operations like NtFreeVirtualMemory where we need to find
    /// the handle that owns a specific memory region by base address.
    pub fn find_handle<F>(&self, predicate: F) -> Option<u64>
    where
        F: Fn(&Resource) -> bool,
    {
        self.resources
            .iter()
            .find(|(_, v)| predicate(v))
            .map(|(&k, _)| k)
    }
}

// ---------------------------------------------------------------------------
// Global per-task resource tables
// ---------------------------------------------------------------------------

/// Global map: task_id → ResourceTable.
///
/// Protected by `spin::Mutex` for use from syscall context.
/// Tables are lazily created on first access and cleaned up on task exit.
#[cfg(target_arch = "x86_64")]
static RESOURCE_TABLES: Mutex<BTreeMap<usize, ResourceTable>> =
    Mutex::new(BTreeMap::new());

/// Execute a closure with mutable access to a task's `ResourceTable`.
///
/// If the task has no table yet, one is created automatically with
/// standard stdio entries pre-installed.
#[cfg(target_arch = "x86_64")]
pub fn with_resources_mut<F, R>(task_id: usize, f: F) -> R
where
    F: FnOnce(&mut ResourceTable) -> R,
{
    let mut map = RESOURCE_TABLES.lock();
    let table = map.entry(task_id).or_insert_with(ResourceTable::new);
    f(table)
}

/// Execute a closure with shared access to a task's `ResourceTable`.
///
/// If the task has no table yet, one is created automatically.
#[cfg(target_arch = "x86_64")]
pub fn with_resources<F, R>(task_id: usize, f: F) -> R
where
    F: FnOnce(&ResourceTable) -> R,
{
    let mut map = RESOURCE_TABLES.lock();
    let table = map.entry(task_id).or_insert_with(ResourceTable::new);
    f(table)
}

/// Remove and drop a task's entire resource table (called on task exit).
///
/// All owned resources (MappedPages, FileRef, etc.) are dropped,
/// which unmaps memory and releases file locks automatically.
#[cfg(target_arch = "x86_64")]
pub fn remove_resources(task_id: usize) {
    RESOURCE_TABLES.lock().remove(&task_id);
}
