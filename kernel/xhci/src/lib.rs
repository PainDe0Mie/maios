//! USB xHCI (eXtensible Host Controller Interface) driver for MaiOS.
//!
//! Supports xHCI 1.0+ controllers. Performs controller initialization,
//! command ring and event ring setup, DCBAA allocation, and port scanning.

#![no_std]

#![allow(unused_imports)]

extern crate alloc;

pub mod regs;
pub mod ring;

use alloc::vec::Vec;
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
    CRCR_RCS, PORTSC_PR, PORTSC_PRC, PORTSC_CHANGE_BITS,
    SPEED_LOW, SPEED_FULL, SPEED_HIGH, SPEED_SUPER,
    hcsparams1_max_slots, hcsparams1_max_ports, hcsparams1_max_intrs,
    speed_to_str,
};
use regs::{
    Trb, TRB_TYPE_ENABLE_SLOT, TRB_TYPE_ADDRESS_DEVICE,
    TRB_TYPE_CMD_COMPLETION, TRB_TYPE_PORT_STATUS_CHANGE,
    TRB_TYPE_SETUP, TRB_TYPE_DATA, TRB_TYPE_STATUS,
    TRB_TYPE_CONFIGURE_ENDPOINT, TRB_TYPE_NOOP,
    TRB_COMP_SUCCESS, TRB_COMP_SHORT_PACKET,
    trb_control,
};
use ring::{TrbRing, EventRing};
use spin::{Mutex, Once};

/// Information about a connected USB device.
pub struct UsbDevice {
    pub slot_id: u8,
    pub port: u8,
    pub speed: u32,
    pub vendor_id: u16,
    pub product_id: u16,
    pub device_class: u8,
    pub device_subclass: u8,
    pub device_protocol: u8,
    pub max_packet_size_ep0: u16,
    /// The device context pages (output context).
    _output_ctx_pages: MappedPages,
    /// The input context pages.
    _input_ctx_pages: MappedPages,
    /// Input context virtual address.
    input_ctx_va: usize,
    /// Transfer ring for endpoint 0 (control).
    ep0_ring: TrbRing,
}

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
    /// DCBAA virtual address (for writing device context pointers).
    dcbaa_va: usize,
    /// Enumerated USB devices.
    pub devices: Vec<UsbDevice>,
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

    /// Write a PORTSC register (preserving RW bits, NOT clearing change bits).
    fn write_portsc(&self, port: u8, val: u32) {
        let portsc_offset = self.op_offset + 0x400 + 0x10 * (port as usize);
        let portsc_va = self.mmio_va + portsc_offset;
        unsafe { core::ptr::write_volatile(portsc_va as *mut u32, val); }
    }

    /// Send a command TRB on the command ring, ring the doorbell,
    /// and poll the event ring for the completion. Returns the completion TRB.
    fn send_command(&mut self, trb: Trb) -> Result<Trb, &'static str> {
        self.cmd_ring.push_trb(trb);
        self.ring_cmd_doorbell();

        // Poll event ring for completion (up to ~100ms).
        for _ in 0..100_000 {
            if let Some(event) = self.event_ring.dequeue_event() {
                // Update ERDP so the controller knows we consumed the event.
                self.update_erdp();
                return Ok(event);
            }
            core::hint::spin_loop();
        }
        Err("XHCI: command timeout — no completion event")
    }

    /// Update the Event Ring Dequeue Pointer in interrupter 0.
    fn update_erdp(&self) {
        let intr0 = self.interrupter_mut(0);
        let erdp_val = self.event_ring.dequeue_phys().value() as u64 | (1 << 3); // EHB bit
        unsafe {
            core::ptr::write_volatile(&mut (*intr0).erdp as *mut _ as *mut u64, erdp_val);
        }
    }

    /// Reset a port and wait for PED (Port Enabled).
    fn reset_port(&self, port: u8) -> Result<u32, &'static str> {
        // Read current PORTSC, set PR (Port Reset), preserve RW bits,
        // clear change bits to avoid accidental acknowledge.
        let portsc = self.read_portsc(port);
        let val = (portsc & !PORTSC_CHANGE_BITS & !PORTSC_PED) | PORTSC_PR;
        self.write_portsc(port, val);

        // Wait for PRC (Port Reset Change) — indicates reset is complete.
        for _ in 0..1_000_000 {
            let ps = self.read_portsc(port);
            if ps & PORTSC_PRC != 0 {
                // Clear PRC (write-1-to-clear).
                self.write_portsc(port, (ps & !PORTSC_CHANGE_BITS) | PORTSC_PRC);
                return Ok(self.read_portsc(port));
            }
            core::hint::spin_loop();
        }
        Err("XHCI: port reset timeout")
    }

    /// Enable a slot for a device. Returns the assigned slot ID.
    fn enable_slot(&mut self) -> Result<u8, &'static str> {
        let trb = Trb {
            parameter: 0,
            status: 0,
            control: trb_control(TRB_TYPE_ENABLE_SLOT, false, 0),
        };
        let event = self.send_command(trb)?;
        let comp = event.completion_code();
        if comp != TRB_COMP_SUCCESS {
            warn!("XHCI: Enable Slot failed, completion code={}", comp);
            return Err("XHCI: Enable Slot command failed");
        }
        let slot_id = event.slot_id();
        warn!("XHCI: slot {} enabled", slot_id);
        Ok(slot_id)
    }

    /// Allocate and initialize Input/Output Device Contexts for a slot,
    /// then send Address Device command.
    fn address_device(
        &mut self,
        slot_id: u8,
        port: u8,
        speed: u32,
    ) -> Result<(MappedPages, MappedPages, usize, TrbRing), &'static str> {
        let kernel_mmi = memory::get_kernel_mmi_ref()
            .ok_or("XHCI: no kernel MMI")?;
        let flags = pte_flags::PteFlags::new().valid(true).writable(true);

        // Allocate Output Device Context (32 entries × 32 bytes = 1024 bytes).
        let out_pages_raw = memory::allocate_pages_by_bytes(4096)
            .ok_or("XHCI: alloc output ctx")?;
        let out_mapped = kernel_mmi.lock().page_table
            .map_allocated_pages(out_pages_raw, flags)?;
        let out_phys = kernel_mmi.lock().page_table
            .translate(out_mapped.start_address())
            .ok_or("XHCI: translate output ctx")?;
        unsafe { core::ptr::write_bytes(out_mapped.start_address().value() as *mut u8, 0, 4096); }

        // Write Output Context pointer into DCBAA[slot_id].
        unsafe {
            let dcbaa_entry = (self.dcbaa_va + slot_id as usize * 8) as *mut u64;
            core::ptr::write_volatile(dcbaa_entry, out_phys.value() as u64);
        }

        // Allocate Input Device Context (33 entries × 32 bytes = 1056 bytes,
        // includes the Input Control Context at offset 0).
        let in_pages_raw = memory::allocate_pages_by_bytes(4096)
            .ok_or("XHCI: alloc input ctx")?;
        let in_mapped = kernel_mmi.lock().page_table
            .map_allocated_pages(in_pages_raw, flags)?;
        let in_phys = kernel_mmi.lock().page_table
            .translate(in_mapped.start_address())
            .ok_or("XHCI: translate input ctx")?;
        let in_va = in_mapped.start_address().value();
        unsafe { core::ptr::write_bytes(in_va as *mut u8, 0, 4096); }

        // Create EP0 Transfer Ring.
        let ep0_ring = TrbRing::new()?;

        // Fill Input Control Context (offset 0, 32 bytes).
        // Add Context flags: bit 0 (Slot) + bit 1 (EP0).
        unsafe {
            let icc = in_va as *mut u32;
            // Add Context Flags at offset 0x04 (Drop=0x00, Add=0x04).
            core::ptr::write_volatile(icc.add(1), (1 << 0) | (1 << 1)); // A0=Slot, A1=EP0
        }

        // Fill Slot Context (offset 32, 32 bytes).
        // Field 0: Route String=0, Speed, Context Entries=1 (just EP0).
        let max_packet_ep0: u16 = match speed {
            SPEED_LOW => 8,
            SPEED_FULL => 8,    // 8, 16, 32, or 64; use 8 initially, update later
            SPEED_HIGH => 64,
            SPEED_SUPER => 512,
            _ => 8,
        };
        unsafe {
            let slot_ctx = (in_va + 32) as *mut u32;
            // DW0: Route String=0 | Speed[23:20] | Context Entries=1[31:27]
            let dw0 = (speed << 20) | (1u32 << 27);
            core::ptr::write_volatile(slot_ctx.add(0), dw0);
            // DW1: Root Hub Port Number (1-based)
            let dw1 = ((port as u32 + 1) << 16);
            core::ptr::write_volatile(slot_ctx.add(1), dw1);
        }

        // Fill EP0 Context (offset 64, 32 bytes — endpoint 0 = DCI 1).
        unsafe {
            let ep0_ctx = (in_va + 64) as *mut u32;
            // DW1: EP Type=4 (Control Bidirectional) | MaxPacketSize | CErr=3
            let ep_type = 4u32; // Control Bidirectional
            let cerr = 3u32;
            let dw1 = (cerr << 1) | (ep_type << 3) | ((max_packet_ep0 as u32) << 16);
            core::ptr::write_volatile(ep0_ctx.add(1), dw1);
            // DW2-3: TR Dequeue Pointer (64-bit) | DCS=1
            let tr_dequeue = ep0_ring.phys_addr().value() as u64 | 1; // DCS=1
            core::ptr::write_volatile(ep0_ctx.add(2) as *mut u64, tr_dequeue);
            // DW4: Average TRB Length = 8 (control transfers are small)
            core::ptr::write_volatile(ep0_ctx.add(4), 8);
        }

        // Send Address Device command.
        let trb = Trb {
            parameter: in_phys.value() as u64,
            status: 0,
            control: trb_control(TRB_TYPE_ADDRESS_DEVICE, false, 0)
                | ((slot_id as u32) << 24),
        };
        let event = self.send_command(trb)?;
        let comp = event.completion_code();
        if comp != TRB_COMP_SUCCESS {
            warn!("XHCI: Address Device slot {} failed, comp={}", slot_id, comp);
            return Err("XHCI: Address Device failed");
        }
        warn!("XHCI: slot {} addressed (speed={}, maxpkt={})",
            slot_id, speed_to_str(speed), max_packet_ep0);

        Ok((out_mapped, in_mapped, in_va, ep0_ring))
    }

    /// Send a USB control transfer (Setup → Data → Status) on EP0 of a slot.
    /// Returns the data read (for IN transfers) or empty vec (for OUT/no-data).
    fn control_transfer(
        &mut self,
        slot_id: u8,
        ep0_ring: &mut TrbRing,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        length: u16,
    ) -> Result<Vec<u8>, &'static str> {
        let is_in = request_type & 0x80 != 0;
        let data_len = length as usize;

        // Allocate a data buffer if needed.
        let data_buf: Option<(MappedPages, PhysicalAddress, usize)> = if data_len > 0 {
            let kernel_mmi = memory::get_kernel_mmi_ref().ok_or("no MMI")?;
            let flags = pte_flags::PteFlags::new().valid(true).writable(true);
            let pages = memory::allocate_pages_by_bytes(4096)
                .ok_or("XHCI: alloc data buf")?;
            let mapped = kernel_mmi.lock().page_table
                .map_allocated_pages(pages, flags)?;
            let phys = kernel_mmi.lock().page_table
                .translate(mapped.start_address())
                .ok_or("XHCI: translate data buf")?;
            let va = mapped.start_address().value();
            unsafe { core::ptr::write_bytes(va as *mut u8, 0, 4096); }
            Some((mapped, phys, va))
        } else {
            None
        };

        // --- Setup Stage TRB ---
        let setup_param = (request_type as u64)
            | ((request as u64) << 8)
            | ((value as u64) << 16)
            | ((index as u64) << 32)
            | ((length as u64) << 48);
        let trt = if data_len == 0 { 0u32 } else if is_in { 3u32 } else { 2u32 };
        let setup_trb = Trb {
            parameter: setup_param,
            status: 8, // Transfer length = 8 (setup packet is always 8 bytes)
            control: trb_control(TRB_TYPE_SETUP, false, 0)
                | (1 << 6)   // IDT (Immediate Data)
                | (trt << 16), // TRT (Transfer Type)
        };
        ep0_ring.push_trb(setup_trb);

        // --- Data Stage TRB (optional) ---
        if let Some((_, ref phys, _)) = data_buf {
            let dir_bit = if is_in { 1u32 << 16 } else { 0u32 };
            let data_trb = Trb {
                parameter: phys.value() as u64,
                status: data_len as u32,
                control: trb_control(TRB_TYPE_DATA, false, 0) | dir_bit,
            };
            ep0_ring.push_trb(data_trb);
        }

        // --- Status Stage TRB ---
        let status_dir = if data_len > 0 && is_in { 0u32 } else { 1u32 << 16 }; // opposite direction
        let status_trb = Trb {
            parameter: 0,
            status: 0,
            control: trb_control(TRB_TYPE_STATUS, false, 0)
                | (1 << 5)   // IOC (Interrupt on Completion)
                | status_dir,
        };
        ep0_ring.push_trb(status_trb);

        // Ring the doorbell for this slot, endpoint 0 (target=1 for EP0 IN/OUT).
        self.ring_doorbell(slot_id, 1);

        // Poll for transfer events (we expect up to 3 events: setup, data, status).
        let mut result_data = Vec::new();
        for _ in 0..500_000 {
            if let Some(event) = self.event_ring.dequeue_event() {
                self.update_erdp();
                let comp = event.completion_code();
                if comp == TRB_COMP_SUCCESS || comp == TRB_COMP_SHORT_PACKET {
                    // Check if this is the status stage completion (IOC).
                    if event.trb_type() == regs::TRB_TYPE_TRANSFER_EVENT {
                        // Peek: did we get all events? The status TRB has IOC.
                        // For simplicity, after any successful transfer event with
                        // the IOC-related completion, we're done.
                        if let Some((_, _, va)) = &data_buf {
                            let actual_len = if comp == TRB_COMP_SHORT_PACKET {
                                // Residual in status field bits 23:0.
                                let residual = event.status & 0x00FF_FFFF;
                                data_len.saturating_sub(residual as usize)
                            } else {
                                data_len
                            };
                            if result_data.is_empty() {
                                let slice = unsafe {
                                    core::slice::from_raw_parts(*va as *const u8, actual_len)
                                };
                                result_data = slice.to_vec();
                            }
                        }
                    }
                } else if comp != 0 {
                    // Non-zero, non-success: might be an error for one of the stages.
                    // Continue draining to not leave stale events.
                }
            } else {
                // No event yet, keep polling.
                core::hint::spin_loop();
            }
            // If we got data, check if the transfer is fully done.
            if !result_data.is_empty() || (data_len == 0 && !result_data.is_empty()) {
                break;
            }
        }

        // Drain remaining events.
        for _ in 0..10 {
            if self.event_ring.dequeue_event().is_some() {
                self.update_erdp();
            } else {
                break;
            }
        }

        if data_len > 0 && result_data.is_empty() {
            // We might not have gotten the data event — read buffer anyway.
            if let Some((_, _, va)) = &data_buf {
                let slice = unsafe {
                    core::slice::from_raw_parts(*va as *const u8, data_len)
                };
                result_data = slice.to_vec();
            }
        }

        Ok(result_data)
    }

    /// Enumerate a connected device on a port: reset, enable slot,
    /// address, and read the device descriptor.
    pub fn enumerate_port(&mut self, port: u8) -> Result<(), &'static str> {
        let portsc = self.read_portsc(port);
        if portsc & PORTSC_CCS == 0 {
            return Ok(()); // Not connected.
        }

        let speed = (portsc & PORTSC_SPEED_MASK) >> PORTSC_SPEED_SHIFT;
        warn!("XHCI: enumerating port {} (speed={})", port + 1, speed_to_str(speed));

        // Step 1: Reset the port.
        let portsc_after = self.reset_port(port)?;
        if portsc_after & PORTSC_PED == 0 {
            warn!("XHCI: port {} not enabled after reset", port + 1);
            return Err("XHCI: port not enabled after reset");
        }

        // Drain any Port Status Change events from the reset.
        for _ in 0..10 {
            if let Some(ev) = self.event_ring.dequeue_event() {
                self.update_erdp();
                if ev.trb_type() != TRB_TYPE_PORT_STATUS_CHANGE {
                    warn!("XHCI: unexpected event during port reset: type={}", ev.trb_type());
                }
            } else {
                break;
            }
        }

        // Step 2: Enable a slot.
        let slot_id = self.enable_slot()?;

        // Step 3: Address the device.
        let (out_ctx, in_ctx, in_va, mut ep0_ring) =
            self.address_device(slot_id, port, speed)?;

        // Step 4: GET_DESCRIPTOR (Device Descriptor, 18 bytes).
        let desc = self.control_transfer(
            slot_id, &mut ep0_ring,
            0x80,  // bmRequestType: Device-to-Host, Standard, Device
            0x06,  // bRequest: GET_DESCRIPTOR
            0x0100, // wValue: Descriptor Type=1 (Device), Index=0
            0x0000, // wIndex: 0
            18,     // wLength: 18 bytes (Device Descriptor)
        )?;

        if desc.len() >= 18 {
            let vendor_id = u16::from_le_bytes([desc[8], desc[9]]);
            let product_id = u16::from_le_bytes([desc[10], desc[11]]);
            let dev_class = desc[4];
            let dev_subclass = desc[5];
            let dev_protocol = desc[6];
            let max_pkt = desc[7] as u16;

            warn!("XHCI: device descriptor: VID={:#06x} PID={:#06x} class={:#04x} sub={:#04x} proto={:#04x} maxpkt={}",
                vendor_id, product_id, dev_class, dev_subclass, dev_protocol, max_pkt);

            let dev = UsbDevice {
                slot_id,
                port,
                speed,
                vendor_id,
                product_id,
                device_class: dev_class,
                device_subclass: dev_subclass,
                device_protocol: dev_protocol,
                max_packet_size_ep0: if max_pkt > 0 { max_pkt } else { 8 },
                _output_ctx_pages: out_ctx,
                _input_ctx_pages: in_ctx,
                input_ctx_va: in_va,
                ep0_ring,
            };
            self.devices.push(dev);
        } else {
            warn!("XHCI: device descriptor too short ({} bytes)", desc.len());
        }

        Ok(())
    }

    /// Enumerate all connected ports.
    pub fn enumerate_all_ports(&mut self) {
        let ports = self.max_ports;
        for port in 0..ports {
            let portsc = self.read_portsc(port);
            if portsc & PORTSC_CCS != 0 {
                if let Err(e) = self.enumerate_port(port) {
                    warn!("XHCI: failed to enumerate port {}: {}", port + 1, e);
                }
            }
        }
        warn!("XHCI: {} device(s) enumerated", self.devices.len());
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
        dcbaa_va: dcbaa_va,
        devices: Vec::new(),
    };

    XHCI_CONTROLLER.call_once(|| Mutex::new(controller));

    // ---- Step 15: Scan ports and enumerate devices ----
    if let Some(xhci_ref) = XHCI_CONTROLLER.get() {
        let mut xhci = xhci_ref.lock();
        xhci.scan_ports();
        xhci.enumerate_all_ports();
    }

    warn!("XHCI: init complete -- xHCI v{}.{}, {} slots, {} ports",
        hciversion >> 8, hciversion & 0xFF, max_slots, max_ports);

    Ok(())
}
