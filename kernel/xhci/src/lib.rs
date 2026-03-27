//! USB xHCI (eXtensible Host Controller Interface) driver for MaiOS.
//!
//! Supports xHCI 1.0+ controllers. Performs controller initialization,
//! command ring and event ring setup, DCBAA allocation, and port scanning.

#![no_std]

#![allow(unused_imports)]

extern crate alloc;

pub mod regs;
pub mod ring;

use log::warn;
use memory::{MappedPages, PhysicalAddress};
use pci::PciDevice;
use regs::{
    XhciCapRegs, XhciOpRegs, XhciInterrupter, XhciPortsc,
    USBCMD_RS, USBCMD_HCRST, USBCMD_INTE,
    USBSTS_HCH, USBSTS_CNR,
    PORTSC_CCS, PORTSC_PED, PORTSC_PP, PORTSC_SPEED_MASK, PORTSC_SPEED_SHIFT,
    PORTSC_PLS_MASK, PORTSC_PLS_SHIFT,
    IMAN_IE, IMAN_IP,
    CRCR_RCS,
    hcsparams1_max_slots, hcsparams1_max_ports, hcsparams1_max_intrs,
    speed_to_str,
};
use ring::{TrbRing, EventRing};
use spin::{Mutex, Once};

/// PCI class for Serial Bus Controller.
pub const XHCI_PCI_CLASS: u8 = 0x0C;
/// PCI subclass for USB controller.
pub const XHCI_PCI_SUBCLASS: u8 = 0x03;
/// PCI programming interface for xHCI.
pub const XHCI_PCI_PROG_IF: u8 = 0x30;

/// Global xHCI controller singleton.
static XHCI_CONTROLLER: Once<Mutex<XhciController>> = Once::new();

/// Get a reference to the xHCI controller.
pub fn get_xhci() -> Option<&'static Mutex<XhciController>> {
    XHCI_CONTROLLER.get()
}

/// The xHCI controller state.
pub struct XhciController {
    /// MMIO mapped pages (kept alive to maintain the mapping).
    _mmio_pages: MappedPages,
    /// Pointer to Capability Registers (read-only).
    cap_regs: *const XhciCapRegs,
    /// Pointer to Operational Registers (read-write).
    op_regs: *mut XhciOpRegs,
    /// Pointer to Doorbell Register Array base.
    doorbell_base: *mut u32,
    /// Pointer to Runtime Register Space base.
    runtime_base: *mut u8,
    /// Command Ring.
    cmd_ring: TrbRing,
    /// Event Ring (interrupter 0).
    event_ring: EventRing,
    /// DCBAA backing pages.
    _dcbaa_pages: MappedPages,
    /// DCBAA physical address.
    dcbaa_phys: PhysicalAddress,
    /// Maximum Device Slots supported by the controller.
    max_slots: u8,
    /// Maximum Root Hub Ports on the controller.
    max_ports: u8,
    /// MMIO virtual base address (for port register access).
    mmio_va: usize,
    /// Operational registers offset from MMIO base.
    op_offset: usize,
}

// Safety: XhciController is only accessed through the spin::Mutex.
unsafe impl Send for XhciController {}

impl XhciController {
    /// Read a PORTSC register for the given port (0-indexed).
    ///
    /// PORTSC registers start at operational base + 0x400 + 0x10 * port_index.
    fn read_portsc(&self, port: u8) -> u32 {
        let portsc_offset = self.op_offset + 0x400 + 0x10 * (port as usize);
        let portsc_va = self.mmio_va + portsc_offset;
        unsafe { core::ptr::read_volatile(portsc_va as *const u32) }
    }

    /// Scan all root hub ports and log their status.
    pub fn scan_ports(&self) {
        warn!("XHCI: scanning {} root hub ports...", self.max_ports);
        for port in 0..self.max_ports {
            let portsc = self.read_portsc(port);
            let connected = portsc & PORTSC_CCS != 0;
            let enabled = portsc & PORTSC_PED != 0;
            let powered = portsc & PORTSC_PP != 0;
            let speed = (portsc & PORTSC_SPEED_MASK) >> PORTSC_SPEED_SHIFT;
            let pls = (portsc & PORTSC_PLS_MASK) >> PORTSC_PLS_SHIFT;

            if connected {
                warn!("XHCI:   port {}: CONNECTED speed={} enabled={} powered={} PLS={}",
                    port + 1, speed_to_str(speed), enabled, powered, pls);
            } else {
                warn!("XHCI:   port {}: not connected (PORTSC={:#010x})",
                    port + 1, portsc);
            }
        }
    }

    /// Ring the host controller doorbell for a given slot.
    /// Slot 0 = Command Ring doorbell. Slot N = device slot N.
    pub fn ring_doorbell(&self, slot: u8, target: u32) {
        let db_ptr = unsafe { self.doorbell_base.add(slot as usize) };
        unsafe { core::ptr::write_volatile(db_ptr, target); }
    }

    /// Ring the Command Ring doorbell (slot 0, target 0).
    pub fn ring_cmd_doorbell(&self) {
        self.ring_doorbell(0, 0);
    }

    /// Get a mutable pointer to interrupter N.
    fn interrupter_mut(&self, index: u32) -> *mut XhciInterrupter {
        // Interrupter 0 is at runtime_base + 0x20.
        // Each interrupter is 32 bytes.
        unsafe {
            self.runtime_base.add(0x20 + 32 * index as usize) as *mut XhciInterrupter
        }
    }
}

/// Initialize the xHCI controller from a PCI device.
///
/// Performs full controller initialization: MMIO mapping, reset, DCBAA setup,
/// command ring, event ring, interrupter configuration, and port scan.
pub fn init_from_pci(pci_dev: &PciDevice) -> Result<(), &'static str> {
    if XHCI_CONTROLLER.get().is_some() {
        return Ok(());
    }

    warn!("XHCI: init PCI {:?} vendor={:#06x} device={:#06x}",
        pci_dev.location, pci_dev.vendor_id, pci_dev.device_id);

    let mem_base = pci_dev.determine_mem_base(0)?;
    let mmio_size = pci_dev.determine_mem_size(0) as usize;
    pci_dev.pci_set_command_bus_master_bit();
    warn!("XHCI: BAR0 phys={:#010x} size={:#x}, bus master enabled", mem_base.value(), mmio_size);

    xhci_init_inner(mem_base, mmio_size)
}

fn xhci_init_inner(mem_base: PhysicalAddress, mmio_size: usize) -> Result<(), &'static str> {
    // Clamp to a reasonable range
    let mmio_size = mmio_size.max(0x1000).min(0x10000);

    // ---- Step 1: Map MMIO ----
    let kernel_mmi = memory::get_kernel_mmi_ref()
        .ok_or("XHCI: no kernel MMI")?;
    let mmio_flags = pte_flags::PteFlags::new()
        .valid(true)
        .writable(true)
        .device_memory(true);
    let mmio_pages_raw = memory::allocate_pages_by_bytes(mmio_size)
        .ok_or("XHCI: failed to allocate MMIO pages")?;
    let mmio_frames = memory::allocate_frames_by_bytes_at(mem_base, mmio_size)
        .map_err(|_| "XHCI: failed to allocate MMIO frames at BAR0")?;
    let mmio_pages = kernel_mmi.lock().page_table
        .map_allocated_pages_to(mmio_pages_raw, mmio_frames, mmio_flags)?;
    let mmio_va = mmio_pages.start_address().value();

    // ---- Step 2: Read Capability Registers ----
    let cap_regs = mmio_va as *const XhciCapRegs;
    let caplength: u8;
    let hciversion: u16;
    let hcsparams1: u32;
    let hccparams1: u32;
    let dboff: u32;
    let rtsoff: u32;

    unsafe {
        caplength = core::ptr::read_volatile(&(*cap_regs).caplength as *const _ as *const u8);
        hciversion = core::ptr::read_volatile(&(*cap_regs).hciversion as *const _ as *const u16);
        hcsparams1 = core::ptr::read_volatile(&(*cap_regs).hcsparams1 as *const _ as *const u32);
        hccparams1 = core::ptr::read_volatile(&(*cap_regs).hccparams1 as *const _ as *const u32);
        dboff = core::ptr::read_volatile(&(*cap_regs).dboff as *const _ as *const u32);
        rtsoff = core::ptr::read_volatile(&(*cap_regs).rtsoff as *const _ as *const u32);
    }

    let max_slots = hcsparams1_max_slots(hcsparams1);
    let max_ports = hcsparams1_max_ports(hcsparams1);
    let max_intrs = hcsparams1_max_intrs(hcsparams1);

    warn!("XHCI: xHCI v{}.{} CAPLENGTH={} MaxSlots={} MaxPorts={} MaxIntrs={}",
        hciversion >> 8, hciversion & 0xFF,
        caplength, max_slots, max_ports, max_intrs);
    warn!("XHCI: HCCPARAMS1={:#010x} DBOFF={:#x} RTSOFF={:#x}",
        hccparams1, dboff, rtsoff);

    // ---- Step 3: Calculate register base pointers ----
    let op_offset = caplength as usize;
    let op_regs = (mmio_va + op_offset) as *mut XhciOpRegs;
    let doorbell_base = (mmio_va + (dboff & !0x3) as usize) as *mut u32;
    let runtime_base = (mmio_va + (rtsoff & !0x1F) as usize) as *mut u8;

    // ---- Step 4: Halt the controller if running ----
    let usbsts = unsafe { core::ptr::read_volatile(&(*op_regs).usbsts as *const _ as *const u32) };
    if usbsts & USBSTS_HCH == 0 {
        warn!("XHCI: controller running, halting...");
        unsafe {
            let cmd = core::ptr::read_volatile(&(*op_regs).usbcmd as *const _ as *const u32);
            core::ptr::write_volatile(&mut (*op_regs).usbcmd as *mut _ as *mut u32, cmd & !USBCMD_RS);
        }
        // Spin until HCH=1 (halted).
        for _ in 0..1_000_000 {
            let sts = unsafe { core::ptr::read_volatile(&(*op_regs).usbsts as *const _ as *const u32) };
            if sts & USBSTS_HCH != 0 { break; }
            core::hint::spin_loop();
        }
    }

    // ---- Step 5: Reset the controller ----
    warn!("XHCI: resetting controller...");
    unsafe {
        core::ptr::write_volatile(&mut (*op_regs).usbcmd as *mut _ as *mut u32, USBCMD_HCRST);
    }

    // Spin until HCRST clears (controller finished internal reset).
    for i in 0..2_000_000u32 {
        let cmd = unsafe { core::ptr::read_volatile(&(*op_regs).usbcmd as *const _ as *const u32) };
        if cmd & USBCMD_HCRST == 0 { break; }
        if i == 1_999_999 {
            return Err("XHCI: controller reset timeout (HCRST stuck)");
        }
        core::hint::spin_loop();
    }

    // Spin until CNR clears (Controller Not Ready -> Ready).
    for i in 0..2_000_000u32 {
        let sts = unsafe { core::ptr::read_volatile(&(*op_regs).usbsts as *const _ as *const u32) };
        if sts & USBSTS_CNR == 0 { break; }
        if i == 1_999_999 {
            return Err("XHCI: controller not ready timeout (CNR stuck)");
        }
        core::hint::spin_loop();
    }
    warn!("XHCI: reset complete");

    // ---- Step 6: Allocate DCBAA ----
    // DCBAA has (MaxSlots + 1) entries, each 8 bytes (64-bit pointers).
    // Entry 0 is the Scratchpad Buffer Array pointer (or 0 if not used).
    let dcbaa_size = (max_slots as usize + 1) * 8;
    let dcbaa_alloc_size = if dcbaa_size < 4096 { 4096 } else { dcbaa_size };

    let dcbaa_flags = pte_flags::PteFlags::new()
        .valid(true)
        .writable(true);

    let dcbaa_pages_raw = memory::allocate_pages_by_bytes(dcbaa_alloc_size)
        .ok_or("XHCI: failed to allocate DCBAA pages")?;
    let dcbaa_mapped = kernel_mmi.lock().page_table
        .map_allocated_pages(dcbaa_pages_raw, dcbaa_flags)?;
    let dcbaa_phys = kernel_mmi.lock().page_table
        .translate(dcbaa_mapped.start_address())
        .ok_or("XHCI: failed to translate DCBAA VA->PA")?;
    let dcbaa_va = dcbaa_mapped.start_address().value();

    // Zero-fill DCBAA.
    unsafe {
        core::ptr::write_bytes(dcbaa_va as *mut u8, 0, dcbaa_alloc_size);
    }
    warn!("XHCI: DCBAA at phys={:#010x} ({} slots)", dcbaa_phys.value(), max_slots);

    // ---- Step 7: Create Command Ring ----
    let cmd_ring = TrbRing::new()?;
    warn!("XHCI: command ring at phys={:#010x}", cmd_ring.phys_addr().value());

    // ---- Step 8: Create Event Ring + Segment Table ----
    let event_ring = EventRing::new()?;
    warn!("XHCI: event ring at phys={:#010x}, ERST at phys={:#010x}",
        event_ring.ring_phys().value(), event_ring.erst_phys().value());

    // ---- Step 9: Configure Interrupter 0 ----
    let intr0 = unsafe {
        &mut *(runtime_base.add(0x20) as *mut XhciInterrupter)
    };
    unsafe {
        // Set Event Ring Segment Table Size = 1.
        core::ptr::write_volatile(&mut intr0.erstsz as *mut _ as *mut u32, event_ring.erst_size());
        // Set Event Ring Dequeue Pointer (with EHB bit 3 cleared).
        core::ptr::write_volatile(&mut intr0.erdp as *mut _ as *mut u64, event_ring.ring_phys().value() as u64);
        // Set Event Ring Segment Table Base Address.
        core::ptr::write_volatile(&mut intr0.erstba as *mut _ as *mut u64, event_ring.erst_phys().value() as u64);
        // Enable interrupts on interrupter 0: set IE bit.
        core::ptr::write_volatile(&mut intr0.iman as *mut _ as *mut u32, IMAN_IE);
        // Set interrupt moderation (0 = no throttle).
        core::ptr::write_volatile(&mut intr0.imod as *mut _ as *mut u32, 0);
    }
    warn!("XHCI: interrupter 0 configured");

    // ---- Step 10: Write DCBAAP ----
    unsafe {
        core::ptr::write_volatile(&mut (*op_regs).dcbaap as *mut _ as *mut u64, dcbaa_phys.value() as u64);
    }

    // ---- Step 11: Write CRCR (Command Ring Control Register) ----
    // Physical address of the command ring | Ring Cycle State bit.
    let crcr_val = cmd_ring.phys_addr().value() as u64 | if cmd_ring.cycle_bit() { CRCR_RCS } else { 0 };
    unsafe {
        core::ptr::write_volatile(&mut (*op_regs).crcr as *mut _ as *mut u64, crcr_val);
    }

    // ---- Step 12: Configure MaxSlotsEn ----
    unsafe {
        core::ptr::write_volatile(&mut (*op_regs).config as *mut _ as *mut u32, max_slots as u32);
    }
    warn!("XHCI: MaxSlotsEn={}", max_slots);

    // ---- Step 13: Start the controller ----
    unsafe {
        core::ptr::write_volatile(
            &mut (*op_regs).usbcmd as *mut _ as *mut u32,
            USBCMD_RS | USBCMD_INTE,
        );
    }

    // Verify the controller is running (HCH should clear).
    for _ in 0..1_000_000 {
        let sts = unsafe { core::ptr::read_volatile(&(*op_regs).usbsts as *const _ as *const u32) };
        if sts & USBSTS_HCH == 0 { break; }
        core::hint::spin_loop();
    }

    let final_sts = unsafe { core::ptr::read_volatile(&(*op_regs).usbsts as *const _ as *const u32) };
    if final_sts & USBSTS_HCH != 0 {
        return Err("XHCI: controller failed to start (HCH still set)");
    }

    warn!("XHCI: controller running (USBSTS={:#010x})", final_sts);

    // ---- Step 14: Build controller struct and store singleton ----
    let controller = XhciController {
        _mmio_pages: mmio_pages,
        cap_regs,
        op_regs,
        doorbell_base,
        runtime_base,
        cmd_ring,
        event_ring,
        _dcbaa_pages: dcbaa_mapped,
        dcbaa_phys,
        max_slots,
        max_ports,
        mmio_va,
        op_offset,
    };

    XHCI_CONTROLLER.call_once(|| Mutex::new(controller));

    // ---- Step 15: Scan ports ----
    if let Some(xhci_ref) = XHCI_CONTROLLER.get() {
        let xhci = xhci_ref.lock();
        xhci.scan_ports();
    }

    warn!("XHCI: init complete -- xHCI v{}.{}, {} slots, {} ports",
        hciversion >> 8, hciversion & 0xFF, max_slots, max_ports);

    Ok(())
}
