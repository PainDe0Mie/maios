//! Syscalls d'I/O fichiers unifiés pour MaiOS.
//!
//! Implémente read, write, open, close, stat, fstat, lseek, ioctl.
//! Utilise la `ResourceTable` unifiée au lieu des fd_table/HandleTable séparées.

use alloc::vec;
#[allow(unused_imports)]
use log::debug;
use crate::error::{SyscallResult, SyscallError};
use crate::resource::{self, Resource};

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

pub fn sys_open(path_ptr: u64, _flags: u64, _mode: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    debug!("sys_open(path={:#x}, flags={:#x}, mode={:#o})", path_ptr, _flags, _mode);

    let path_str = match unsafe { read_c_string(path_ptr) } {
        Some(s) => s,
        None => return Err(SyscallError::Fault),
    };
    debug!("sys_open: path = \"{}\"", path_str);

    let root_dir = root::get_root();
    let p = path::Path::new(&path_str);
    let file_ref = match p.get_file(root_dir) {
        Some(f) => f,
        None => {
            debug!("sys_open: not found \"{}\"", path_str);
            return Err(SyscallError::NotFound);
        }
    };

    let tid = current_task_id();
    let fd = resource::with_resources_mut(tid, |table| {
        table.alloc_fd(Resource::File { file: file_ref, offset: 0 })
    });
    debug!("sys_open: \"{}\" -> fd {}", path_str, fd);
    Ok(fd)
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
