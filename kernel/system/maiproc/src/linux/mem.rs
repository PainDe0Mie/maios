//! Appels système de mémoire : `mmap` et compagnie, sur [`uvm::Mm`].

use super::errno::*;
use super::{fault, SysResult};
use super::fs::get_fd;
use crate::fd::Node;
use crate::Thread;
use alloc::sync::Arc;
use kernel_config::memory::PAGE_SIZE;
use uvm::{page_up, Backing, MemFile, PageSource, Prot, VmError};

const MAP_SHARED: u64 = 0x1;
const MAP_PRIVATE: u64 = 0x2;
const MAP_SHARED_VALIDATE: u64 = 0x3;
const MAP_FIXED: u64 = 0x10;
const MAP_ANONYMOUS: u64 = 0x20;
const MAP_32BIT: u64 = 0x40;
const MAP_POPULATE: u64 = 0x8000;
const MAP_FIXED_NOREPLACE: u64 = 0x10_0000;

const MREMAP_MAYMOVE: u64 = 1;
const MREMAP_FIXED: u64 = 2;

const MADV_DONTNEED: u64 = 4;
const MADV_FREE: u64 = 8;
const MADV_REMOVE: u64 = 9;

fn vm_err(e: VmError) -> i64 {
    match e {
        VmError::NoMapping | VmError::Access => ENOMEM,
        e => fault(e),
    }
}

fn prot_of(p: u64) -> Prot {
    Prot((p & 7) as u32)
}

pub fn mmap(t: &Thread, addr: u64, len: u64, prot: u64, flags: u64, fd: u64, off: u64) -> SysResult {
    if len == 0 || off as usize % PAGE_SIZE != 0 {
        return Err(EINVAL);
    }
    let len = len as usize;
    if page_up(len) == 0 || len > uvm::USER_END {
        return Err(ENOMEM);
    }
    let kind = flags & 0xf;
    if kind != MAP_SHARED && kind != MAP_PRIVATE && kind != MAP_SHARED_VALIDATE {
        return Err(EINVAL);
    }
    let mm = t.mm();
    let fixed = flags & (MAP_FIXED | MAP_FIXED_NOREPLACE) != 0;
    if fixed && addr as usize % PAGE_SIZE != 0 {
        return Err(EINVAL);
    }
    if flags & MAP_FIXED_NOREPLACE != 0 && flags & MAP_FIXED == 0 && !mm.is_free_range(addr as usize, len) {
        return Err(EEXIST);
    }
    let hint = if !fixed && addr == 0 && flags & MAP_32BIT != 0 { 0x4000_0000 } else { addr as usize };

    let shared = kind == MAP_SHARED || kind == MAP_SHARED_VALIDATE;
    // Projection partagée qui ne pourra pas devenir inscriptible (`VM_MAYWRITE` absent).
    let mut deny_write = false;
    let (backing, name): (Backing, uvm::VmaName) = if flags & MAP_ANONYMOUS != 0 {
        if shared {
            // Anonyme partagé : un objet commun, que `fork` transmet sans copie à l'écriture.
            (Backing::Shared { obj: uvm::ShmObject::new(), pgoff: 0 }, "[anon]".into())
        } else {
            (Backing::Anon, "[anon]".into())
        }
    } else {
        let f = get_fd(t, fd)?;
        if !f.readable() {
            return Err(EACCES);
        }
        let off = off as usize;
        match &f.node {
            Node::Zero if shared => (Backing::Shared { obj: uvm::ShmObject::new(), pgoff: 0 }, "/dev/zero".into()),
            Node::Zero => (Backing::Anon, "/dev/zero".into()),
            Node::File { file, path } if shared => {
                // MaiSS : objet partagé du fichier, commun à toutes ses projections partagées ;
                // les écritures vont au fichier (msync, fsync, vidage périodique, dernière
                // projection retirée).
                if prot & 2 != 0 && !f.writable() {
                    return Err(EACCES);
                }
                if prot & 2 != 0 {
                    crate::seals::check_shared_write_map(file)?;
                }
                deny_write = !f.writable() || crate::seals::check_shared_write_map(file).is_err();
                (Backing::Shared { obj: crate::pagecache::shared(file), pgoff: off / PAGE_SIZE }, path_name(path))
            }
            Node::File { file, path } => {
                // Pages du cache du fichier, partagées par tous les processus qui le projettent.
                let cache = crate::pagecache::get(file);
                let filesz = len.min(PageSource::len(&*cache).saturating_sub(off));
                (Backing::File { src: cache, offset: off, file_va: 0, filesz }, path_name(path))
            }
            Node::Generated { data, path } => {
                let s = off.min(data.len());
                let e = (off + len).min(data.len());
                let bytes: Arc<[u8]> = Arc::from(data[s..e].to_vec().into_boxed_slice());
                let filesz = bytes.len();
                let src: Arc<dyn PageSource> = Arc::new(MemFile(bytes));
                (Backing::File { src, offset: 0, file_va: 0, filesz }, path_name(path))
            }
            Node::Object(o) => match o.mmap_backing(off as u64) {
                Some(obj) if shared => (Backing::Shared { obj, pgoff: 0 }, "anon_inode:[io_uring]".into()),
                Some(_) => return Err(EINVAL),
                None => return Err(ENODEV),
            },
            _ => return Err(ENODEV),
        }
    };

    let start = mm.mmap(hint, len, prot_of(prot), backing, fixed, name).map_err(vm_err)?;
    if deny_write {
        mm.deny_write(start);
    }
    if flags & MAP_POPULATE != 0 {
        let _ = mm.populate(start, len, prot & 2 != 0);
    }
    Ok(start as i64)
}

/// Nom d'une projection de fichier dans `/proc/<pid>/maps` : son chemin.
fn path_name(path: &str) -> uvm::VmaName {
    uvm::VmaName::Path(Arc::from(path))
}

/// `msync` : réécrit dans leurs fichiers les pages des projections partagées de la plage
/// (`MS_ASYNC` aussi, de façon synchrone : la réécriture est rapide et l'appel reste correct).
pub fn msync(t: &Thread, addr: u64, len: u64, flags: u64) -> SysResult {
    const MS_ASYNC: u64 = 1;
    const MS_INVALIDATE: u64 = 2;
    const MS_SYNC: u64 = 4;
    if addr as usize % PAGE_SIZE != 0 || flags & !(MS_ASYNC | MS_INVALIDATE | MS_SYNC) != 0 || (flags & MS_ASYNC != 0 && flags & MS_SYNC != 0) {
        return Err(EINVAL);
    }
    if len == 0 {
        return Ok(0);
    }
    let objs = t.mm().shared_files_in(addr as usize, len as usize).map_err(|e| match e {
        VmError::NoMapping => ENOMEM,
        e => vm_err(e),
    })?;
    for (obj, first, last) in objs {
        obj.writeback(first, last);
    }
    Ok(0)
}

pub fn munmap(t: &Thread, addr: u64, len: u64) -> SysResult {
    if len == 0 {
        return Err(EINVAL);
    }
    t.mm().munmap(addr as usize, len as usize).map(|_| 0).map_err(fault)
}

pub fn mprotect(t: &Thread, addr: u64, len: u64, prot: u64) -> SysResult {
    t.mm().mprotect(addr as usize, len as usize, prot_of(prot)).map(|_| 0).map_err(|e| match e {
        VmError::Access => EACCES,
        e => vm_err(e),
    })
}

pub fn mremap(t: &Thread, old: u64, old_len: u64, new_len: u64, flags: u64, new_addr: u64) -> SysResult {
    if flags & !(MREMAP_MAYMOVE | MREMAP_FIXED) != 0 {
        return Err(EINVAL);
    }
    let fixed = if flags & MREMAP_FIXED != 0 { Some(new_addr as usize) } else { None };
    t.mm()
        .mremap(old as usize, old_len as usize, new_len as usize, flags & MREMAP_MAYMOVE != 0, fixed)
        .map(|a| a as i64)
        .map_err(|e| match e {
            VmError::NoMapping => EFAULT,
            e => vm_err(e),
        })
}

pub fn mincore(t: &Thread, addr: u64, len: u64, vec_ptr: u64) -> SysResult {
    let v = t.mm().mincore(addr as usize, len as usize).map_err(vm_err)?;
    t.mm().copy_to_user(vec_ptr as usize, &v).map_err(fault)?;
    Ok(0)
}

pub fn madvise(t: &Thread, addr: u64, len: u64, advice: u64) -> SysResult {
    if addr as usize % PAGE_SIZE != 0 {
        return Err(EINVAL);
    }
    match advice {
        MADV_DONTNEED | MADV_FREE | MADV_REMOVE => {
            t.mm().discard(addr as usize, len as usize).map(|_| 0).map_err(fault)
        }
        _ => Ok(0),
    }
}
