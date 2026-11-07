//! Appels système de mémoire : `mmap` et compagnie, sur [`uvm::Mm`].

use super::errno::*;
use super::{fault, SysResult};
use super::fs::get_fd;
use crate::fd::Node;
use crate::Thread;
use alloc::{sync::Arc, vec};
use kernel_config::memory::PAGE_SIZE;
use log::warn;
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
    let mm = &t.process.mm;
    let fixed = flags & (MAP_FIXED | MAP_FIXED_NOREPLACE) != 0;
    if fixed && addr as usize % PAGE_SIZE != 0 {
        return Err(EINVAL);
    }
    if flags & MAP_FIXED_NOREPLACE != 0 && flags & MAP_FIXED == 0 && !mm.is_free_range(addr as usize, len) {
        return Err(EEXIST);
    }
    let hint = if !fixed && addr == 0 && flags & MAP_32BIT != 0 { 0x4000_0000 } else { addr as usize };

    let (backing, name) = if flags & MAP_ANONYMOUS != 0 {
        (Backing::Anon, "[anon]")
    } else {
        let f = get_fd(t, fd)?;
        if !f.readable() {
            return Err(EACCES);
        }
        let off = off as usize;
        let data: Arc<[u8]> = match &f.node {
            Node::Zero => Arc::from(vec![].into_boxed_slice()),
            Node::File { file, .. } => {
                let mut l = file.lock();
                let flen = l.len();
                let n = len.min(flen.saturating_sub(off));
                let mut buf = vec![0u8; n];
                if n > 0 {
                    let got = l.read_at(&mut buf, off).map_err(|_| EIO)?;
                    buf.truncate(got);
                }
                Arc::from(buf.into_boxed_slice())
            }
            Node::Generated { data, .. } => {
                let s = off.min(data.len());
                let e = (off + len).min(data.len());
                Arc::from(data[s..e].to_vec().into_boxed_slice())
            }
            Node::Dir { .. } => return Err(ENODEV),
            _ => return Err(ENODEV),
        };
        if kind != MAP_PRIVATE && prot & 2 != 0 && !matches!(f.node, Node::Zero) {
            warn!("maiproc : pid {} projection partagee inscriptible traitee en prive (ecriture non reportee)", t.process.pid);
        }
        if matches!(f.node, Node::Zero) {
            (Backing::Anon, "/dev/zero")
        } else {
            let filesz = data.len();
            let src: Arc<dyn PageSource> = Arc::new(MemFile(data));
            (Backing::File { src, offset: 0, file_va: 0, filesz }, "file")
        }
    };

    let start = mm.mmap(hint, len, prot_of(prot), backing, fixed, name).map_err(vm_err)?;
    if flags & MAP_POPULATE != 0 {
        let _ = mm.populate(start, len, prot & 2 != 0);
    }
    Ok(start as i64)
}

pub fn munmap(t: &Thread, addr: u64, len: u64) -> SysResult {
    if len == 0 {
        return Err(EINVAL);
    }
    t.process.mm.munmap(addr as usize, len as usize).map(|_| 0).map_err(fault)
}

pub fn mprotect(t: &Thread, addr: u64, len: u64, prot: u64) -> SysResult {
    t.process.mm.mprotect(addr as usize, len as usize, prot_of(prot)).map(|_| 0).map_err(vm_err)
}

pub fn mremap(t: &Thread, old: u64, old_len: u64, new_len: u64, flags: u64, new_addr: u64) -> SysResult {
    if flags & !(MREMAP_MAYMOVE | MREMAP_FIXED) != 0 {
        return Err(EINVAL);
    }
    let fixed = if flags & MREMAP_FIXED != 0 { Some(new_addr as usize) } else { None };
    t.process
        .mm
        .mremap(old as usize, old_len as usize, new_len as usize, flags & MREMAP_MAYMOVE != 0, fixed)
        .map(|a| a as i64)
        .map_err(|e| match e {
            VmError::NoMapping => EFAULT,
            e => vm_err(e),
        })
}

pub fn mincore(t: &Thread, addr: u64, len: u64, vec_ptr: u64) -> SysResult {
    let v = t.process.mm.mincore(addr as usize, len as usize).map_err(vm_err)?;
    t.process.mm.copy_to_user(vec_ptr as usize, &v).map_err(fault)?;
    Ok(0)
}

pub fn madvise(t: &Thread, addr: u64, len: u64, advice: u64) -> SysResult {
    if addr as usize % PAGE_SIZE != 0 {
        return Err(EINVAL);
    }
    match advice {
        MADV_DONTNEED | MADV_FREE | MADV_REMOVE => {
            t.process.mm.discard(addr as usize, len as usize).map(|_| 0).map_err(fault)
        }
        _ => Ok(0),
    }
}
