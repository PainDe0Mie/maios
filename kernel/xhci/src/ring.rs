//! xHCI TRB ring management.
//!
//! Implements Command Ring (producer) and Event Ring (consumer) structures
//! used for host-controller communication via Transfer Request Blocks.

use crate::regs::{Trb, EventRingSegmentTableEntry, TRB_TYPE_LINK, trb_control};
use log::warn;
use memory::{MappedPages, PhysicalAddress};

/// Number of TRBs per ring segment (256 TRBs = 4096 bytes = 1 page).
const RING_SIZE: usize = 256;

/// Size of one TRB in bytes.
const TRB_SIZE: usize = 16;

// ---- Command / Transfer Ring (Producer) ------------------------------------

/// A TRB ring that the host software enqueues TRBs into.
///
/// Used for the Command Ring and Transfer Rings. The last TRB in the ring
/// is always a Link TRB that wraps back to the start.
pub struct TrbRing {
    /// Backing pages for the ring buffer.
    _pages: MappedPages,
    /// Virtual address of the ring buffer start.
    va: usize,
    /// Physical address of the ring buffer start (for hardware).
    phys: PhysicalAddress,
    /// Current enqueue index (0..RING_SIZE-1).
    enqueue_idx: usize,
    /// Producer Cycle State (PCS). Toggled on each wrap.
    cycle: bool,
}

impl TrbRing {
    /// Allocate a new TRB ring (one page, 256 TRBs).
    ///
    /// The ring is zero-filled and a Link TRB is placed at the last entry
    /// pointing back to the ring start.
    pub fn new() -> Result<Self, &'static str> {
        let kernel_mmi = memory::get_kernel_mmi_ref()
            .ok_or("XHCI ring: no kernel MMI")?;

        let alloc_flags = pte_flags::PteFlags::new()
            .valid(true)
            .writable(true);

        let pages_raw = memory::allocate_pages_by_bytes(RING_SIZE * TRB_SIZE)
            .ok_or("XHCI: failed to allocate ring pages")?;
        let mapped = kernel_mmi.lock().page_table
            .map_allocated_pages(pages_raw, alloc_flags)?;
        let phys = kernel_mmi.lock().page_table
            .translate(mapped.start_address())
            .ok_or("XHCI: failed to translate ring VA->PA")?;
        let va = mapped.start_address().value();

        // Zero-fill the entire ring.
        unsafe {
            core::ptr::write_bytes(va as *mut u8, 0, RING_SIZE * TRB_SIZE);
        }

        let mut ring = Self {
            _pages: mapped,
            va,
            phys,
            enqueue_idx: 0,
            cycle: true,
        };

        // Place a Link TRB at the last slot (index RING_SIZE-1) that wraps
        // back to the start. The Toggle Cycle bit (bit 1) is set so the
        // cycle state inverts on each wrap.
        ring.write_link_trb();

        Ok(ring)
    }

    /// Physical address of the ring start (for writing to CRCR or endpoint ctx).
    pub fn phys_addr(&self) -> PhysicalAddress {
        self.phys
    }

    /// Current Producer Cycle State.
    pub fn cycle_bit(&self) -> bool {
        self.cycle
    }

    /// Enqueue a TRB onto the ring. The cycle bit in the TRB control field
    /// is set automatically. Returns the physical address of the enqueued TRB.
    pub fn push_trb(&mut self, mut trb: Trb) -> PhysicalAddress {
        // Set or clear the cycle bit in the TRB control word.
        if self.cycle {
            trb.control |= 1;
        } else {
            trb.control &= !1;
        }

        let slot_va = self.va + self.enqueue_idx * TRB_SIZE;
        let slot_phys = PhysicalAddress::new(
            self.phys.value() + self.enqueue_idx * TRB_SIZE
        ).unwrap();

        // Write TRB fields in order: parameter, status, then control last
        // (the HC reads on cycle bit match, so control must be written last).
        unsafe {
            let ptr = slot_va as *mut u64;
            core::ptr::write_volatile(ptr, trb.parameter);
            let ptr32 = (slot_va + 8) as *mut u32;
            core::ptr::write_volatile(ptr32, trb.status);
            let ctrl_ptr = (slot_va + 12) as *mut u32;
            core::ptr::write_volatile(ctrl_ptr, trb.control);
        }

        self.enqueue_idx += 1;

        // If we've reached the Link TRB slot, the hardware will follow it
        // back to the ring start. Advance our index and toggle cycle.
        if self.enqueue_idx >= RING_SIZE - 1 {
            // Update the Link TRB cycle bit to match current PCS before
            // the hardware reads it.
            self.update_link_trb_cycle();
            self.enqueue_idx = 0;
            self.cycle = !self.cycle;
            // Re-write the Link TRB with the new cycle state for next wrap.
            self.write_link_trb();
        }

        slot_phys
    }

    /// Write a Link TRB at index RING_SIZE-1 pointing to ring start.
    fn write_link_trb(&self) {
        let link_va = self.va + (RING_SIZE - 1) * TRB_SIZE;
        let link_trb = Trb {
            parameter: self.phys.value() as u64,
            status: 0,
            // TRB type = Link (6), Toggle Cycle = bit 1, cycle bit set if PCS.
            control: trb_control(TRB_TYPE_LINK, self.cycle, 1 << 1),
        };
        unsafe {
            let ptr = link_va as *mut u64;
            core::ptr::write_volatile(ptr, link_trb.parameter);
            let ptr32 = (link_va + 8) as *mut u32;
            core::ptr::write_volatile(ptr32, link_trb.status);
            let ctrl_ptr = (link_va + 12) as *mut u32;
            core::ptr::write_volatile(ctrl_ptr, link_trb.control);
        }
    }

    /// Update just the cycle bit of the Link TRB at the end of the ring.
    fn update_link_trb_cycle(&self) {
        let link_va = self.va + (RING_SIZE - 1) * TRB_SIZE;
        unsafe {
            let ctrl_ptr = (link_va + 12) as *mut u32;
            let mut ctrl = core::ptr::read_volatile(ctrl_ptr);
            if self.cycle {
                ctrl |= 1;
            } else {
                ctrl &= !1;
            }
            core::ptr::write_volatile(ctrl_ptr, ctrl);
        }
    }
}

// ---- Event Ring (Consumer) -------------------------------------------------

/// An Event Ring that the xHC writes events into and the host software reads.
///
/// The host owns the Event Ring Segment Table and the dequeue pointer.
/// The xHC owns the enqueue pointer and writes events with the correct
/// cycle bit.
pub struct EventRing {
    /// Backing pages for the event ring segment.
    _ring_pages: MappedPages,
    /// Virtual address of the ring segment start.
    ring_va: usize,
    /// Physical address of the ring segment start.
    ring_phys: PhysicalAddress,
    /// Backing pages for the Event Ring Segment Table (ERST).
    _erst_pages: MappedPages,
    /// Physical address of the ERST.
    erst_phys: PhysicalAddress,
    /// Current dequeue index.
    dequeue_idx: usize,
    /// Consumer Cycle State (CCS). Matches the cycle bit of new events.
    cycle: bool,
}

impl EventRing {
    /// Allocate a new Event Ring (one segment of 256 TRBs) and its
    /// segment table (one entry).
    pub fn new() -> Result<Self, &'static str> {
        let kernel_mmi = memory::get_kernel_mmi_ref()
            .ok_or("XHCI event ring: no kernel MMI")?;

        let alloc_flags = pte_flags::PteFlags::new()
            .valid(true)
            .writable(true);

        // Allocate ring segment (256 TRBs = 4096 bytes).
        let ring_pages_raw = memory::allocate_pages_by_bytes(RING_SIZE * TRB_SIZE)
            .ok_or("XHCI: failed to allocate event ring pages")?;
        let ring_mapped = kernel_mmi.lock().page_table
            .map_allocated_pages(ring_pages_raw, alloc_flags)?;
        let ring_phys = kernel_mmi.lock().page_table
            .translate(ring_mapped.start_address())
            .ok_or("XHCI: failed to translate event ring VA->PA")?;
        let ring_va = ring_mapped.start_address().value();

        // Zero-fill the ring segment.
        unsafe {
            core::ptr::write_bytes(ring_va as *mut u8, 0, RING_SIZE * TRB_SIZE);
        }

        // Allocate ERST (one entry = 16 bytes, but allocate a full page).
        let erst_pages_raw = memory::allocate_pages_by_bytes(4096)
            .ok_or("XHCI: failed to allocate ERST pages")?;
        let erst_mapped = kernel_mmi.lock().page_table
            .map_allocated_pages(erst_pages_raw, alloc_flags)?;
        let erst_phys = kernel_mmi.lock().page_table
            .translate(erst_mapped.start_address())
            .ok_or("XHCI: failed to translate ERST VA->PA")?;
        let erst_va = erst_mapped.start_address().value();

        // Zero-fill ERST page, then write the single segment table entry.
        unsafe {
            core::ptr::write_bytes(erst_va as *mut u8, 0, 4096);
            let entry = erst_va as *mut EventRingSegmentTableEntry;
            (*entry).ring_base = ring_phys.value() as u64;
            (*entry).ring_size = RING_SIZE as u32;
            (*entry)._rsvd = 0;
        }

        Ok(Self {
            _ring_pages: ring_mapped,
            ring_va,
            ring_phys,
            _erst_pages: erst_mapped,
            erst_phys,
            dequeue_idx: 0,
            cycle: true,
        })
    }

    /// Physical address of the Event Ring Segment Table.
    pub fn erst_phys(&self) -> PhysicalAddress {
        self.erst_phys
    }

    /// Physical address of the first TRB in the event ring segment.
    /// Used to initialize the ERDP register.
    pub fn ring_phys(&self) -> PhysicalAddress {
        self.ring_phys
    }

    /// Number of entries in the segment table (always 1 for now).
    pub fn erst_size(&self) -> u32 {
        1
    }

    /// Current dequeue physical address (for writing to ERDP).
    pub fn dequeue_phys(&self) -> PhysicalAddress {
        PhysicalAddress::new(
            self.ring_phys.value() + self.dequeue_idx * TRB_SIZE
        ).unwrap()
    }

    /// Try to dequeue the next event TRB from the ring.
    ///
    /// Returns `Some(trb)` if a new event is available (cycle bit matches CCS),
    /// or `None` if the ring is empty.
    pub fn dequeue_event(&mut self) -> Option<Trb> {
        let slot_va = self.ring_va + self.dequeue_idx * TRB_SIZE;

        let trb = unsafe {
            let param = core::ptr::read_volatile(slot_va as *const u64);
            let status = core::ptr::read_volatile((slot_va + 8) as *const u32);
            let control = core::ptr::read_volatile((slot_va + 12) as *const u32);
            Trb { parameter: param, status, control }
        };

        // Check if the cycle bit matches our Consumer Cycle State.
        let event_cycle = trb.control & 1 != 0;
        if event_cycle != self.cycle {
            return None;
        }

        // Advance dequeue pointer.
        self.dequeue_idx += 1;
        if self.dequeue_idx >= RING_SIZE {
            self.dequeue_idx = 0;
            self.cycle = !self.cycle;
        }

        Some(trb)
    }

    /// Drain all pending events from the ring, logging each one.
    /// Returns the number of events processed.
    pub fn drain_events(&mut self) -> usize {
        let mut count = 0;
        while let Some(trb) = self.dequeue_event() {
            warn!("XHCI: event TRB type={} comp={} slot={}",
                trb.trb_type(), trb.completion_code(), trb.slot_id());
            count += 1;
        }
        count
    }
}
