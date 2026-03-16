//! Theseus-specific memory management features.
//! 
//! * Theseus does not yet support copy-on-write, 
//!   so all mappings will be marked as "shared", not "private".
//! * Theseus doesn't yet support swapping pages out to disk,
//!   so we use the PRESENT bit in a PTE to represent 
//!   a fully-protected region with NONE access rights.
//! * Theseus's page table entries don't use access permissions
//!   to differentiate between a PRESENT page and a READABLE page.
//!   If it is PRESENT, we consider it READABLE.
//! 

use crate::{Error, Protection, Region, Result};
use rust_alloc::vec::Vec;
// use core::cmp::{max, min};
use core2::io;
use theseus_memory::{MappedPages, PteFlags};

/// The set of active mappings created by users of this crate.
/// This is not unified with all other mappings in Theseus,
/// in order to keep those invisible and safe from outside accessors.
/// 
/// TODO: move this to tlibc so we can use regular libc mmap calls
///       to track all external MappedPages instances.
static mut MAPPINGS: Vec<MappedPages> = Vec::new();


pub struct QueryIter {
  region_address: usize,
  upper_bound: usize,
}

impl QueryIter {
  pub fn new(origin: *const (), size: usize) -> Result<QueryIter> {
    let start = origin as usize;
    Ok(QueryIter {
      region_address: start,
      upper_bound: start.saturating_add(size), 
    })
  }

  pub fn upper_bound(&self) -> usize {
    self.upper_bound
  }
}

impl Iterator for QueryIter {
  type Item = Result<Region>;

  fn next(&mut self) -> Option<Self::Item> {
    while self.region_address < self.upper_bound {
      // We search the list of mappings on every iteration in case the mappings have changed.
      if let Some(mp) = find_mapped_pages(self.region_address as *const _).map(|i| unsafe { &MAPPINGS[i] }) {
        // move the next iteration to the end of this MappedPages region.
        self.region_address += mp.size_in_bytes();
        
        let region = Region {
          base:       mp.start_address().value() as *const _,
          reserved:   false, // not relevant in Theseus, only in Windows, see `Region::is_committed()`
          guarded:    false, // not relevant in Theseus, only in Windows and MacOS
          shared:     true,  // see module-level docs
          size:       mp.size_in_bytes(),
          protection: Protection::from_native(mp.flags().into()),
        };
        return Some(Ok(region));
      }
    }

    None
  }
}

/// Returns the index of the MappedPages object in [`MAPPINGS`]
/// that contains the given `base` address, if any.
fn find_mapped_pages(base: *const ()) -> Option<usize> {
  let base = base as usize;
  unsafe {
    for (i, mp) in MAPPINGS.iter().enumerate() {
      if base >= mp.start_address().value() && base < (mp.start_address().value() + mp.size_in_bytes()) {
        return Some(i);
      }
    }
  }
  None
}

pub fn page_size() -> usize {
  theseus_memory::PAGE_SIZE
}

/// This function uses `mmap` to allocate a new anonymous memory region, i.e.,
/// a memory region not backed by a file.
pub unsafe fn alloc(base: *const (), size: usize, protection: Protection) -> Result<*const ()> {
  let flags = protection.to_native();

  let pages = if base.is_null() {
    theseus_memory::allocate_pages_by_bytes(size)
      .ok_or(Error::SystemCall(io::Error::new(io::ErrorKind::Other, "out of virtual memory")))?
  } else {
    let vaddr = theseus_memory::VirtualAddress::new(base as usize)
      .ok_or(Error::InvalidParameter("base address was an invalid virtual address"))?;
    theseus_memory::allocate_pages_by_bytes_at(vaddr, size)
      .map_err(|_| Error::SystemCall(io::ErrorKind::AddrInUse.into()))?
  };

  let kernel_mmi_ref = theseus_memory::get_kernel_mmi_ref().expect("Theseus memory subsystem not yet initialized.");
  let mp = kernel_mmi_ref.lock().page_table.map_allocated_pages(pages, flags).unwrap();

  let start_addr = mp.start_address().value();
  MAPPINGS.push(mp);
  Ok(start_addr as *const _)
}

/// This function uses `munmap` to remove a region previously created by `mmap`.
pub unsafe fn free(base: *const (), _size: usize) -> Result<()> {
  if let Some(mp) = find_mapped_pages(base).map(|i| MAPPINGS.remove(i)) {
    drop(mp); // unmaps this MappedPages
    Ok(())
  } else {
    Err(Error::UnmappedRegion)
  }
}

pub unsafe fn protect(base: *const (), _size: usize, protection: Protection) -> Result<()> {
  if let Some(mp) = find_mapped_pages(base).map(|i| &mut MAPPINGS[i]) {
    let kernel_mmi_ref = theseus_memory::get_kernel_mmi_ref().expect("Theseus memory subsystem not yet initialized.");
    let mut kernel_mmi = kernel_mmi_ref.lock();
    mp.remap(&mut kernel_mmi.page_table, protection.to_native())
      .map_err(|_| Error::UnmappedRegion)
  } else {
    Err(Error::UnmappedRegion)
  }
}

pub fn lock(_base: *const (), _size: usize) -> Result<()> {
  // Theseus currently doesn't swap pages out to disk.
  Ok(())
}

pub fn unlock(_base: *const (), _size: usize) -> Result<()> {
  // Theseus currently doesn't swap pages out to disk.
  Ok(())
}

impl Protection {
  fn from_native(flags: PteFlags) -> Self {
    let mut prot = Protection::empty();
    if flags.is_valid() {
      // Theseus currently treats the PRESENT/VALID flag as readable.
      prot.insert(Protection::READ);
    }
    if flags.is_writable() {
      prot.insert(Protection::WRITE);
    }
    if flags.is_executable() {
      prot.insert(Protection::EXECUTE);
    }
    prot
  }

  pub(crate) fn to_native(self) -> PteFlags {
    PteFlags::new()
      // Theseus currently treats the PRESENT/VALID flag as readable.
      .valid(self.intersects(Protection::READ))
      .writable(self.intersects(Protection::WRITE))
      .executable(self.intersects(Protection::EXECUTE))
  }
}


#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn protection_flags_are_mapped_to_native() {
    let none = PteFlags::NO_EXECUTE;
    let ro   = PteFlags::PRESENT | PteFlags::NO_EXECUTE;
    let rw   = PteFlags::PRESENT | PteFlags::WRITABLE | PteFlags::NO_EXECUTE;
    let rwx  = PteFlags::PRESENT | PteFlags::WRITABLE;
    assert_eq!(Protection::NONE.to_native(), none);
    assert_eq!(Protection::READ.to_native(), ro);
    assert_eq!(Protection::READ_WRITE.to_native(), rw);
    assert_eq!(Protection::READ_WRITE_EXECUTE.to_native(), rwx);
  }
}
