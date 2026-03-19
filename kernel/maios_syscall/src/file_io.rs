//! Syscalls d'I/O fichiers unifiés pour MaiOS.
//!
//! Implémente read, write, open, close, stat, fstat, lseek, ioctl,
//! openat, fcntl, writev, readv, pread64, access, getcwd, dup, dup2, dup3, pipe, pipe2.
//! Utilise la `ResourceTable` unifiée au lieu des fd_table/HandleTable séparées.

use alloc::vec;
use alloc::string::String;
#[allow(unused_imports)]
use log::debug;
use crate::error::{SyscallResult, SyscallError};
use crate::resource::{self, Resource};
use fs_node;

/// Linux `struct iovec` layout (x86_64).
#[repr(C)]
struct IoVec {
    iov_base: u64, // pointer to buffer
    iov_len: u64,  // length
}

/// AT_FDCWD: special dirfd meaning "current working directory".
const AT_FDCWD: i32 = -100;

/// Obtenir l'ID de la tâche courante.
fn current_task_id() -> usize {
    task::get_my_current_task_id()
}

/// Lire une chaîne C terminée par null depuis un pointeur userspace.
///
/// # Safety
/// Le pointeur doit pointer vers de la mémoire lisible contenant
/// une séquence d'octets terminée par null.
unsafe fn read_c_string(ptr: u64) -> Option<alloc::string::String> {
    if ptr == 0 {
        return None;
    }
    let mut p = ptr as *const u8;
    let mut len = 0usize;
    while len < 4096 {
        if *p == 0 {
            break;
        }
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
// Structure stat Linux x86_64
// =============================================================================

/// Layout de `struct stat` Linux x86_64 (144 octets).
#[repr(C)]
struct LinuxStat {
    st_dev: u64,
    st_ino: u64,
    st_nlink: u64,
    st_mode: u32,
    st_uid: u32,
    st_gid: u32,
    __pad0: u32,
    st_rdev: u64,
    st_size: i64,
    st_blksize: i64,
    st_blocks: i64,
    st_atime: i64,
    st_atime_nsec: i64,
    st_mtime: i64,
    st_mtime_nsec: i64,
    st_ctime: i64,
    st_ctime_nsec: i64,
    __unused: [i64; 3],
}

/// Remplir un buffer stat pour un fichier régulier ou répertoire.
fn fill_stat_buf(stat_ptr: u64, size: usize, is_dir: bool) {
    let mode: u32 = if is_dir { 0o040755 } else { 0o100644 };
    let stat = unsafe { &mut *(stat_ptr as *mut LinuxStat) };
    unsafe { core::ptr::write_bytes(stat as *mut LinuxStat, 0, 1); }
    stat.st_dev = 1;
    stat.st_ino = 1;
    stat.st_nlink = 1;
    stat.st_mode = mode;
    stat.st_size = size as i64;
    stat.st_blksize = 4096;
    stat.st_blocks = ((size + 511) / 512) as i64;
}

/// Remplir un buffer stat pour un périphérique caractère (stdin/stdout/stderr).
fn fill_stat_buf_chardev(stat_ptr: u64) {
    let stat = unsafe { &mut *(stat_ptr as *mut LinuxStat) };
    unsafe { core::ptr::write_bytes(stat as *mut LinuxStat, 0, 1); }
    stat.st_dev = 1;
    stat.st_ino = 1;
    stat.st_nlink = 1;
    stat.st_mode = 0o020666;
    stat.st_rdev = 0x0501;
    stat.st_blksize = 1024;
}

// =============================================================================
// Implémentations des syscalls
// =============================================================================

pub fn sys_read(fd: u64, buf_ptr: u64, count: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    if buf_ptr == 0 || count == 0 {
        return if count == 0 { Ok(0) } else { Err(SyscallError::Fault) };
    }

    let tid = current_task_id();

    // Check what kind of resource this is without holding the lock during I/O.
    enum ReadTarget {
        Stdin,
        File,
        BadFd,
    }

    let target = resource::with_resources(tid, |table| {
        match table.get(fd) {
            Some(Resource::Stdin) => ReadTarget::Stdin,
            Some(Resource::File { .. }) => ReadTarget::File,
            Some(Resource::Stdout) | Some(Resource::Stderr) => ReadTarget::BadFd,
            Some(Resource::Memory { .. }) => ReadTarget::BadFd,
            None => ReadTarget::BadFd,
        }
    });

    match target {
        ReadTarget::Stdin => {
            match app_io::stdin() {
                Ok(reader) => {
                    let buf = unsafe {
                        core::slice::from_raw_parts_mut(buf_ptr as *mut u8, count as usize)
                    };
                    match reader.read(buf) {
                        Ok(n) => Ok(n as u64),
                        Err(_) => Err(SyscallError::WouldBlock),
                    }
                }
                Err(_) => Err(SyscallError::WouldBlock),
            }
        }
        ReadTarget::File => {
            // For files, we need mutable access to update the offset
            resource::with_resources_mut(tid, |table| {
                if let Some(Resource::File { file, offset }) = table.get_mut(fd) {
                    let mut buf = vec![0u8; count as usize];
                    let mut locked = file.lock();
                    match locked.read_at(&mut buf, *offset) {
                        Ok(bytes_read) => {
                            unsafe {
                                core::ptr::copy_nonoverlapping(
                                    buf.as_ptr(),
                                    buf_ptr as *mut u8,
                                    bytes_read,
                                );
                            }
                            *offset += bytes_read;
                            Ok(bytes_read as u64)
                        }
                        Err(_) => Err(SyscallError::IoError),
                    }
                } else {
                    Err(SyscallError::BadFileDescriptor)
                }
            })
        }
        ReadTarget::BadFd => Err(SyscallError::BadFileDescriptor),
    }
}

pub fn sys_write(fd: u64, buf_ptr: u64, count: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    if buf_ptr == 0 && count > 0 {
        return Err(SyscallError::Fault);
    }
    if count == 0 {
        return Ok(0);
    }

    let slice = unsafe {
        core::slice::from_raw_parts(buf_ptr as *const u8, count as usize)
    };

    let tid = current_task_id();

    // Determine what kind of resource this fd/handle refers to,
    // but do NOT hold the RESOURCE_TABLES lock during I/O.
    // Holding a spinlock while writing to a pipe can deadlock if the
    // pipe blocks (waiting for the reader to drain it).
    enum WriteTarget {
        Stdout,
        Stderr,
        BadFd,
        File,
        Other,
    }

    let target = resource::with_resources(tid, |table| {
        match table.get(fd) {
            Some(Resource::Stdout) => WriteTarget::Stdout,
            Some(Resource::Stderr) => WriteTarget::Stderr,
            Some(Resource::Stdin) => WriteTarget::BadFd,
            Some(Resource::File { .. }) => WriteTarget::File,
            Some(Resource::Memory { .. }) => WriteTarget::Other,
            None => WriteTarget::BadFd,
        }
    });

    match target {
        WriteTarget::Stdout => {
            if let Ok(w) = app_io::stdout() {
                if let Ok(n) = w.write(slice) {
                    return Ok(n as u64);
                }
            }
            if let Ok(s) = core::str::from_utf8(slice) {
                log::info!("[userspace] {}", s);
            }
            Ok(count)
        }
        WriteTarget::Stderr => {
            if let Ok(w) = app_io::stderr() {
                if let Ok(n) = w.write(slice) {
                    return Ok(n as u64);
                }
            }
            if let Ok(s) = core::str::from_utf8(slice) {
                log::info!("[userspace] {}", s);
            }
            Ok(count)
        }
        WriteTarget::File => {
            // For files, we need mutable access to update the offset
            resource::with_resources_mut(tid, |table| {
                if let Some(Resource::File { file, offset }) = table.get_mut(fd) {
                    let mut locked = file.lock();
                    match locked.write_at(slice, *offset) {
                        Ok(bytes_written) => {
                            *offset += bytes_written;
                            Ok(bytes_written as u64)
                        }
                        Err(_) => Err(SyscallError::IoError),
                    }
                } else {
                    Err(SyscallError::BadFileDescriptor)
                }
            })
        }
        WriteTarget::BadFd => Err(SyscallError::BadFileDescriptor),
        WriteTarget::Other => Err(SyscallError::BadFileDescriptor),
    }
}

// Linux open(2) flags
const O_WRONLY: u64  = 0x0001;
const O_RDWR: u64    = 0x0002;
const O_CREAT: u64   = 0x0040;
const O_EXCL: u64    = 0x0080;
const O_TRUNC: u64   = 0x0200;
const O_APPEND: u64  = 0x0400;

pub fn sys_open(path_ptr: u64, flags: u64, _mode: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    debug!("sys_open(path={:#x}, flags={:#x}, mode={:#o})", path_ptr, flags, _mode);

    let path_str = match unsafe { read_c_string(path_ptr) } {
        Some(s) => s,
        None => return Err(SyscallError::Fault),
    };
    debug!("sys_open: path = \"{}\"", path_str);

    let root_dir = root::get_root();
    let p = path::Path::new(&path_str);

    // Try to find existing file/dir
    let existing = p.get(&root_dir);

    let file_ref = match existing {
        Some(fs_node::FileOrDir::File(f)) => {
            // File exists
            if (flags & O_CREAT != 0) && (flags & O_EXCL != 0) {
                // O_CREAT | O_EXCL: fail if file already exists
                debug!("sys_open: O_EXCL and file exists \"{}\"", path_str);
                return Err(SyscallError::FileExists);
            }
            // O_TRUNC: truncate the file to zero length
            if flags & O_TRUNC != 0 {
                let mut locked = f.lock();
                // Truncate by writing empty at offset 0 and setting length
                // HeapFile stores a Vec<u8>, so we use write_at with empty to signal truncation
                // Actually we need to access the internal vec — use the trait method
                if let Err(_) = locked.write_at(&[], 0) {
                    debug!("sys_open: truncate failed for \"{}\"", path_str);
                }
                // For HeapFile, truncation means replacing the content.
                // Since we can't directly truncate, we'll handle this at the resource level.
            }
            f
        }
        Some(fs_node::FileOrDir::Dir(_)) => {
            // Opening a directory as a file — return IsADirectory for write attempts
            if flags & (O_WRONLY | O_RDWR) != 0 {
                return Err(SyscallError::IsADirectory);
            }
            // For read-only directory opens (used by some Linux programs), return NotFound
            // since we don't support directory fds yet
            return Err(SyscallError::IsADirectory);
        }
        None => {
            // File not found
            if flags & O_CREAT == 0 {
                debug!("sys_open: not found \"{}\"", path_str);
                return Err(SyscallError::NotFound);
            }

            // O_CREAT: create the file
            debug!("sys_open: creating \"{}\"", path_str);

            // Split path into parent directory and filename
            let (parent_dir, filename) = resolve_parent_and_name(&path_str, &root_dir)?;

            match heapfile::HeapFile::create(filename, &parent_dir) {
                Ok(f) => f,
                Err(e) => {
                    debug!("sys_open: create failed for \"{}\": {}", path_str, e);
                    return Err(SyscallError::IoError);
                }
            }
        }
    };

    let offset = if flags & O_APPEND != 0 {
        file_ref.lock().len()
    } else {
        0
    };

    let tid = current_task_id();
    let fd = resource::with_resources_mut(tid, |table| {
        table.alloc_fd(Resource::File { file: file_ref, offset })
    });
    debug!("sys_open: \"{}\" -> fd {}", path_str, fd);
    Ok(fd)
}

/// Split a path string into (parent_directory_ref, filename).
fn resolve_parent_and_name(path_str: &str, root: &fs_node::DirRef) -> Result<(fs_node::DirRef, String), SyscallError> {
    // Find the last '/' to split parent path and filename
    if let Some(last_slash) = path_str.rfind('/') {
        let parent_path = if last_slash == 0 { "/" } else { &path_str[..last_slash] };
        let filename = &path_str[last_slash + 1..];
        if filename.is_empty() {
            return Err(SyscallError::InvalidArgument);
        }
        let p = path::Path::new(parent_path);
        match p.get(root) {
            Some(fs_node::FileOrDir::Dir(d)) => Ok((d, String::from(filename))),
            _ => Err(SyscallError::NotFound),
        }
    } else {
        // No slash — file is in the current working directory
        let cwd = task::with_current_task(|t| t.env.lock().working_dir.clone())
            .map_err(|_| SyscallError::NotFound)?;
        Ok((cwd, String::from(path_str)))
    }
}

pub fn sys_close(handle: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    debug!("sys_close(handle={})", handle);

    // Les handles stdio sont protégés — retourner succès silencieusement
    // (comportement Windows : NtClose sur un handle console retourne SUCCESS)
    if resource::ResourceTable::is_stdio(handle) {
        return Ok(0);
    }

    let tid = current_task_id();
    let closed = resource::with_resources_mut(tid, |table| table.close(handle));

    match closed {
        Some(_) => Ok(0),
        None => Err(SyscallError::BadFileDescriptor),
    }
}

pub fn sys_stat(path_ptr: u64, stat_buf: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    debug!("sys_stat(path={:#x}, buf={:#x})", path_ptr, stat_buf);

    if stat_buf == 0 {
        return Err(SyscallError::Fault);
    }

    let path_str = match unsafe { read_c_string(path_ptr) } {
        Some(s) => s,
        None => return Err(SyscallError::Fault),
    };

    let root_dir = root::get_root();
    let p = path::Path::new(&path_str);
    match p.get(root_dir) {
        Some(fs_node::FileOrDir::File(f)) => {
            let size = f.lock().len();
            fill_stat_buf(stat_buf, size, false);
            Ok(0)
        }
        Some(fs_node::FileOrDir::Dir(_)) => {
            fill_stat_buf(stat_buf, 0, true);
            Ok(0)
        }
        None => Err(SyscallError::NotFound),
    }
}

pub fn sys_fstat(fd: u64, stat_buf: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    debug!("sys_fstat(fd={}, buf={:#x})", fd, stat_buf);

    if stat_buf == 0 {
        return Err(SyscallError::Fault);
    }

    let tid = current_task_id();

    resource::with_resources(tid, |table| {
        let entry = match table.get(fd) {
            Some(e) => e,
            None => return Err(SyscallError::BadFileDescriptor),
        };

        match entry {
            Resource::Stdin | Resource::Stdout | Resource::Stderr => {
                fill_stat_buf_chardev(stat_buf);
                Ok(0)
            }
            Resource::File { file, .. } => {
                let size = file.lock().len();
                fill_stat_buf(stat_buf, size, false);
                Ok(0)
            }
            Resource::Memory { .. } => Err(SyscallError::BadFileDescriptor),
        }
    })
}

pub fn sys_lseek(fd: u64, offset: u64, whence: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    let offset = offset as i64;
    let whence = whence as i32;
    debug!("sys_lseek(fd={}, offset={}, whence={})", fd, offset, whence);

    const SEEK_SET: i32 = 0;
    const SEEK_CUR: i32 = 1;
    const SEEK_END: i32 = 2;

    let tid = current_task_id();

    resource::with_resources_mut(tid, |table| {
        let entry = match table.get_mut(fd) {
            Some(e) => e,
            None => return Err(SyscallError::BadFileDescriptor),
        };

        match entry {
            Resource::File { file, offset: cur_off } => {
                let file_len = file.lock().len() as i64;
                let new_offset = match whence {
                    SEEK_SET => offset,
                    SEEK_CUR => *cur_off as i64 + offset,
                    SEEK_END => file_len + offset,
                    _ => return Err(SyscallError::InvalidArgument),
                };
                if new_offset < 0 {
                    return Err(SyscallError::InvalidArgument);
                }
                *cur_off = new_offset as usize;
                Ok(new_offset as u64)
            }
            _ => Err(SyscallError::IllegalSeek),
        }
    })
}

pub fn sys_ioctl(fd: u64, request: u64, arg: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    debug!("sys_ioctl(fd={}, request={:#x}, arg={:#x})", fd, request, arg);

    const TIOCGWINSZ: u64 = 0x5413;

    if request == TIOCGWINSZ && arg != 0 {
        let ws = unsafe { &mut *(arg as *mut [u16; 4]) };
        ws[0] = 25;  // rows
        ws[1] = 80;  // cols
        ws[2] = 0;
        ws[3] = 0;
        return Ok(0);
    }

    Err(SyscallError::NotATerminal)
}

/// sys_dup — duplicate a file descriptor.
///
/// Returns the new fd (lowest available number).
pub fn sys_dup(fd: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    let tid = current_task_id();
    resource::with_resources_mut(tid, |table| {
        match table.dup(fd) {
            Some(new_fd) => Ok(new_fd),
            None => Err(SyscallError::BadFileDescriptor),
        }
    })
}

/// sys_dup2 — duplicate a file descriptor onto a specific target.
///
/// If newfd is already open, it is silently closed first.
pub fn sys_dup2(oldfd: u64, newfd: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    let tid = current_task_id();
    resource::with_resources_mut(tid, |table| {
        match table.dup2(oldfd, newfd) {
            Some(fd) => Ok(fd),
            None => Err(SyscallError::BadFileDescriptor),
        }
    })
}

/// sys_pipe — create a unidirectional pipe.
///
/// Creates a pair of file descriptors: pipefd[0] for reading, pipefd[1] for writing.
/// The pipe uses a MaiOS `Stdio` ring buffer internally.
///
/// # Arguments
/// - `pipefd_ptr`: pointer to `[i32; 2]` where the fd pair is written
pub fn sys_pipe(pipefd_ptr: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    if pipefd_ptr == 0 {
        return Err(SyscallError::Fault);
    }

    // For now, pipe is not fully supported — we'd need a shared Stdio buffer
    // between two fds. Return NotImplemented until we have a proper pipe mechanism.
    // TODO: Create a Stdio buffer, wrap reader as Resource::PipeRead and writer as Resource::PipeWrite
    Err(SyscallError::NotImplemented)
}

// =============================================================================
// Phase 1A: openat, fcntl, writev, readv, pread64, access, getcwd, pipe2, dup3
// =============================================================================

/// sys_openat — open a file relative to a directory fd.
///
/// When dirfd == AT_FDCWD (-100), behaves exactly like open().
/// This is what musl/glibc actually calls instead of open().
pub fn sys_openat(dirfd: u64, path_ptr: u64, flags: u64, mode: u64, _: u64, _: u64) -> SyscallResult {
    let dirfd_i = dirfd as i32;

    // For now, we only support AT_FDCWD (current directory = root).
    // TODO: support real dirfd-relative paths when we have per-task cwd.
    if dirfd_i != AT_FDCWD && dirfd_i >= 0 {
        // Could resolve relative to the directory referred to by dirfd,
        // but for now treat all paths as absolute from root.
    }

    // Delegate to sys_open (same logic)
    sys_open(path_ptr, flags, mode, 0, 0, 0)
}

/// sys_fcntl — file descriptor control.
///
/// Supports: F_GETFL, F_SETFL, F_GETFD, F_SETFD, F_DUPFD, F_DUPFD_CLOEXEC.
pub fn sys_fcntl(fd: u64, cmd: u64, arg: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    const F_DUPFD: u64 = 0;
    const F_GETFD: u64 = 1;
    const F_SETFD: u64 = 2;
    const F_GETFL: u64 = 3;
    const F_SETFL: u64 = 4;
    const F_DUPFD_CLOEXEC: u64 = 1030;

    let tid = current_task_id();

    match cmd {
        F_DUPFD | F_DUPFD_CLOEXEC => {
            // Duplicate fd to the lowest available fd >= arg
            // Simplified: just dup (ignore the >= arg constraint for now)
            resource::with_resources_mut(tid, |table| {
                match table.dup(fd) {
                    Some(new_fd) => Ok(new_fd),
                    None => Err(SyscallError::BadFileDescriptor),
                }
            })
        }
        F_GETFD => {
            // Return close-on-exec flag. We don't track this yet, return 0.
            let tid = current_task_id();
            resource::with_resources(tid, |table| {
                if table.get(fd).is_some() { Ok(0) } else { Err(SyscallError::BadFileDescriptor) }
            })
        }
        F_SETFD => {
            // Set close-on-exec flag. We don't track this yet, just succeed.
            let tid = current_task_id();
            resource::with_resources(tid, |table| {
                if table.get(fd).is_some() { Ok(0) } else { Err(SyscallError::BadFileDescriptor) }
            })
        }
        F_GETFL => {
            // Return file status flags. We don't track flags yet.
            // Return O_RDWR (2) as default.
            let tid = current_task_id();
            resource::with_resources(tid, |table| {
                if table.get(fd).is_some() { Ok(2) } else { Err(SyscallError::BadFileDescriptor) }
            })
        }
        F_SETFL => {
            // Set file status flags. We don't track flags yet, just succeed.
            let _ = arg;
            let tid = current_task_id();
            resource::with_resources(tid, |table| {
                if table.get(fd).is_some() { Ok(0) } else { Err(SyscallError::BadFileDescriptor) }
            })
        }
        _ => Err(SyscallError::InvalidArgument),
    }
}

/// sys_writev — write data from multiple buffers (scatter/gather).
///
/// Used by printf/glibc to write header + data in one syscall.
pub fn sys_writev(fd: u64, iov_ptr: u64, iovcnt: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    if iov_ptr == 0 || iovcnt == 0 {
        return if iovcnt == 0 { Ok(0) } else { Err(SyscallError::Fault) };
    }
    if iovcnt > 1024 {
        return Err(SyscallError::InvalidArgument); // UIO_MAXIOV
    }

    let iovecs = unsafe {
        core::slice::from_raw_parts(iov_ptr as *const IoVec, iovcnt as usize)
    };

    let mut total: u64 = 0;
    for iov in iovecs {
        if iov.iov_len == 0 {
            continue;
        }
        if iov.iov_base == 0 {
            return Err(SyscallError::Fault);
        }
        match sys_write(fd, iov.iov_base, iov.iov_len, 0, 0, 0) {
            Ok(n) => total += n,
            Err(e) => {
                if total > 0 { return Ok(total); } // partial write
                return Err(e);
            }
        }
    }
    Ok(total)
}

/// sys_readv — read data into multiple buffers (scatter/gather).
pub fn sys_readv(fd: u64, iov_ptr: u64, iovcnt: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    if iov_ptr == 0 || iovcnt == 0 {
        return if iovcnt == 0 { Ok(0) } else { Err(SyscallError::Fault) };
    }
    if iovcnt > 1024 {
        return Err(SyscallError::InvalidArgument);
    }

    let iovecs = unsafe {
        core::slice::from_raw_parts(iov_ptr as *const IoVec, iovcnt as usize)
    };

    let mut total: u64 = 0;
    for iov in iovecs {
        if iov.iov_len == 0 {
            continue;
        }
        if iov.iov_base == 0 {
            return Err(SyscallError::Fault);
        }
        match sys_read(fd, iov.iov_base, iov.iov_len, 0, 0, 0) {
            Ok(0) => break, // EOF
            Ok(n) => {
                total += n;
                if n < iov.iov_len { break; } // short read
            }
            Err(e) => {
                if total > 0 { return Ok(total); }
                return Err(e);
            }
        }
    }
    Ok(total)
}

/// sys_pread64 — read from a file at a specific offset without changing the cursor.
///
/// Used by the dynamic linker to read ELF headers.
pub fn sys_pread64(fd: u64, buf_ptr: u64, count: u64, offset: u64, _: u64, _: u64) -> SyscallResult {
    if buf_ptr == 0 || count == 0 {
        return if count == 0 { Ok(0) } else { Err(SyscallError::Fault) };
    }

    let tid = current_task_id();

    resource::with_resources(tid, |table| {
        match table.get(fd) {
            Some(Resource::File { file, .. }) => {
                let mut buf = vec![0u8; count as usize];
                let mut locked = file.lock();
                match locked.read_at(&mut buf, offset as usize) {
                    Ok(n) => {
                        unsafe {
                            core::ptr::copy_nonoverlapping(buf.as_ptr(), buf_ptr as *mut u8, n);
                        }
                        Ok(n as u64)
                    }
                    Err(_) => Err(SyscallError::IoError),
                }
            }
            Some(_) => Err(SyscallError::IllegalSeek), // can't pread on stdin/stdout
            None => Err(SyscallError::BadFileDescriptor),
        }
    })
}

/// sys_access — check file permissions.
///
/// Simplified: just checks if the file exists (ignores mode bits).
pub fn sys_access(path_ptr: u64, _mode: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    let path_str = match unsafe { read_c_string(path_ptr) } {
        Some(s) => s,
        None => return Err(SyscallError::Fault),
    };

    let root_dir = root::get_root();
    let p = path::Path::new(&path_str);
    match p.get(root_dir) {
        Some(_) => Ok(0),
        None => Err(SyscallError::NotFound),
    }
}

/// sys_getcwd — get current working directory.
///
/// MaiOS doesn't have per-task cwd yet, always returns "/".
pub fn sys_getcwd(buf_ptr: u64, size: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    if buf_ptr == 0 {
        return Err(SyscallError::Fault);
    }
    if size < 2 {
        return Err(SyscallError::BufferTooSmall);
    }

    let cwd = b"/\0";
    unsafe {
        core::ptr::copy_nonoverlapping(cwd.as_ptr(), buf_ptr as *mut u8, 2);
    }
    Ok(buf_ptr) // Linux getcwd returns the buffer pointer on success
}

/// sys_pipe2 — create a pipe with flags (O_CLOEXEC, O_NONBLOCK).
///
/// For now, same as pipe (stub), flags are ignored.
pub fn sys_pipe2(pipefd_ptr: u64, _flags: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    sys_pipe(pipefd_ptr, 0, 0, 0, 0, 0)
}

/// sys_dup3 — duplicate fd with flags (O_CLOEXEC).
///
/// Like dup2 but with flags. We ignore flags for now.
pub fn sys_dup3(oldfd: u64, newfd: u64, _flags: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    if oldfd == newfd {
        return Err(SyscallError::InvalidArgument); // dup3 returns EINVAL if oldfd == newfd
    }
    sys_dup2(oldfd, newfd, 0, 0, 0, 0)
}

// =============================================================================
// Phase 2: Filesystem operations
// =============================================================================

/// Linux `struct linux_dirent64` layout.
#[repr(C)]
struct LinuxDirent64 {
    d_ino: u64,
    d_off: i64,
    d_reclen: u16,
    d_type: u8,
    // d_name follows (variable length, null-terminated)
}

/// sys_getdents64 — read directory entries.
///
/// Reads entries from an open directory fd into a buffer.
/// Returns the number of bytes written, or 0 for end-of-directory.
pub fn sys_getdents64(fd: u64, buf_ptr: u64, buf_size: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    if buf_ptr == 0 || buf_size == 0 {
        return Err(SyscallError::Fault);
    }

    // For now, we don't support opening directories as fds.
    // TODO: add Resource::Directory variant and implement directory reading
    let _ = fd;
    Err(SyscallError::NotImplemented)
}

/// sys_chdir — change current working directory.
///
/// MaiOS doesn't have per-task cwd yet. Stub: validate path exists, return Ok.
pub fn sys_chdir(path_ptr: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    let path_str = match unsafe { read_c_string(path_ptr) } {
        Some(s) => s,
        None => return Err(SyscallError::Fault),
    };

    // Validate path exists
    let root_dir = root::get_root();
    let p = path::Path::new(&path_str);
    match p.get(root_dir) {
        Some(fs_node::FileOrDir::Dir(_)) => Ok(0),
        Some(fs_node::FileOrDir::File(_)) => Err(SyscallError::NotADirectory),
        None => Err(SyscallError::NotFound),
    }
}

/// sys_mkdir — create a directory.
///
/// Stub: MaiOS VFS doesn't support directory creation yet.
/// Returns EROFS (read-only filesystem) to indicate the limitation.
pub fn sys_mkdir(_path_ptr: u64, _mode: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    // TODO: implement when VFS supports directory creation
    Err(SyscallError::ReadOnlyFs)
}

/// sys_unlink — delete a file.
///
/// Stub: MaiOS VFS doesn't support file deletion yet.
pub fn sys_unlink(_path_ptr: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    // TODO: implement when VFS supports file deletion
    Err(SyscallError::ReadOnlyFs)
}

/// sys_readlink — read the target of a symbolic link.
///
/// MaiOS doesn't have symlinks. Special-case /proc/self/exe.
pub fn sys_readlink(path_ptr: u64, buf_ptr: u64, buf_size: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    if buf_ptr == 0 || buf_size == 0 {
        return Err(SyscallError::Fault);
    }

    let path_str = match unsafe { read_c_string(path_ptr) } {
        Some(s) => s,
        None => return Err(SyscallError::Fault),
    };

    // Special case: /proc/self/exe — dynamic linker reads this
    if path_str == "/proc/self/exe" {
        let exe = b"/app";
        let len = core::cmp::min(exe.len(), buf_size as usize);
        unsafe {
            core::ptr::copy_nonoverlapping(exe.as_ptr(), buf_ptr as *mut u8, len);
        }
        return Ok(len as u64);
    }

    // No symlinks in MaiOS
    Err(SyscallError::InvalidArgument)
}

/// sys_newfstatat — stat relative to a directory fd.
///
/// Modern replacement for stat, used by musl/glibc.
pub fn sys_newfstatat(dirfd: u64, path_ptr: u64, stat_buf: u64, _flags: u64, _: u64, _: u64) -> SyscallResult {
    // If path is empty and AT_EMPTY_PATH flag is set, fstat the dirfd
    let path_str = unsafe { read_c_string(path_ptr) };
    match path_str {
        Some(ref s) if s.is_empty() => {
            // AT_EMPTY_PATH: stat the fd itself
            return sys_fstat(dirfd, stat_buf, 0, 0, 0, 0);
        }
        None => return Err(SyscallError::Fault),
        _ => {}
    }

    // Otherwise delegate to stat (ignoring dirfd for now, treating as AT_FDCWD)
    sys_stat(path_ptr, stat_buf, 0, 0, 0, 0)
}

/// sys_faccessat — check file permissions relative to dirfd.
///
/// Modern replacement for access, used by musl/glibc.
pub fn sys_faccessat(_dirfd: u64, path_ptr: u64, mode: u64, _flags: u64, _: u64, _: u64) -> SyscallResult {
    sys_access(path_ptr, mode, 0, 0, 0, 0)
}

/// sys_pwrite64 — write to a file at an offset without changing the cursor.
pub fn sys_pwrite64(fd: u64, buf_ptr: u64, count: u64, offset: u64, _: u64, _: u64) -> SyscallResult {
    if buf_ptr == 0 || count == 0 {
        return if count == 0 { Ok(0) } else { Err(SyscallError::Fault) };
    }

    let slice = unsafe {
        core::slice::from_raw_parts(buf_ptr as *const u8, count as usize)
    };

    let tid = current_task_id();

    resource::with_resources(tid, |table| {
        match table.get(fd) {
            Some(Resource::File { file, .. }) => {
                let mut locked = file.lock();
                match locked.write_at(slice, offset as usize) {
                    Ok(n) => Ok(n as u64),
                    Err(_) => Err(SyscallError::IoError),
                }
            }
            Some(_) => Err(SyscallError::IllegalSeek),
            None => Err(SyscallError::BadFileDescriptor),
        }
    })
}

/// sys_ftruncate — truncate a file to a given length. Stub.
pub fn sys_ftruncate(_fd: u64, _length: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    Err(SyscallError::NotImplemented)
}

/// sys_rename — rename a file. Stub: VFS doesn't support renaming.
pub fn sys_rename(_oldpath: u64, _newpath: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    Err(SyscallError::ReadOnlyFs)
}
