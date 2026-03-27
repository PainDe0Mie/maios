//! xHCI register definitions.
//!
//! Memory-mapped register structs matching the xHCI specification (rev 1.2).
//! All fields use `Volatile` or `ReadOnly` to ensure proper MMIO access.

use volatile::{Volatile, ReadOnly};
use zerocopy::FromBytes;

// ---- Capability Registers (BAR0 + 0x00) ------------------------------------

/// xHCI Host Controller Capability Registers at BAR0 offset 0x00.
#[derive(FromBytes)]
#[repr(C)]
pub struct XhciCapRegs {
    /// Capability Register Length (offset to Operational Registers).
    pub caplength:  ReadOnly<u8>,       // 0x00
    _rsvd0:         u8,                 // 0x01
    /// Host Controller Interface Version Number (BCD).
    pub hciversion: ReadOnly<u16>,      // 0x02
    /// Structural Parameters 1 (MaxSlots, MaxIntrs, MaxPorts).
    pub hcsparams1: ReadOnly<u32>,      // 0x04
    /// Structural Parameters 2 (IST, ERST Max, SPB Max Hi/Lo).
    pub hcsparams2: ReadOnly<u32>,      // 0x08
    /// Structural Parameters 3 (U1/U2 latencies).
    pub hcsparams3: ReadOnly<u32>,      // 0x0C
    /// Capability Parameters 1 (AC64, BNC, CSZ, PPC, PIND, LHRC, LTC, NSS,
    /// PAE, SPC, SEC, CFC, MaxPSASize, xECP).
    pub hccparams1: ReadOnly<u32>,      // 0x10
    /// Doorbell Offset (relative to BAR0).
    pub dboff:      ReadOnly<u32>,      // 0x14
    /// Runtime Register Space Offset (relative to BAR0).
    pub rtsoff:     ReadOnly<u32>,      // 0x18
    /// Capability Parameters 2 (U3C, CMC, FSC, CTC, LEC, CIC, ETC, ETC_TSC,
    /// GSC, VTC).
    pub hccparams2: ReadOnly<u32>,      // 0x1C
}

// ---- Operational Registers (BAR0 + CAPLENGTH) ------------------------------

/// xHCI Host Controller Operational Registers.
///
/// Located at BAR0 + CAPLENGTH. PORTSC registers are accessed separately
/// at offset 0x400 + 0x10 * port_index from the operational register base.
#[derive(FromBytes)]
#[repr(C)]
pub struct XhciOpRegs {
    /// USB Command Register.
    pub usbcmd:     Volatile<u32>,      // +0x00
    /// USB Status Register.
    pub usbsts:     Volatile<u32>,      // +0x04
    /// Page Size Register.
    pub pagesize:   ReadOnly<u32>,      // +0x08
    _rsvd0:         [u8; 8],            // +0x0C..0x13
    /// Device Notification Control Register.
    pub dnctrl:     Volatile<u32>,      // +0x14
    /// Command Ring Control Register (64-bit).
    pub crcr:       Volatile<u64>,      // +0x18
    _rsvd1:         [u8; 16],           // +0x20..0x2F
    /// Device Context Base Address Array Pointer (64-bit).
    pub dcbaap:     Volatile<u64>,      // +0x30
    /// Configure Register (MaxSlotsEn in bits 7:0).
    pub config:     Volatile<u32>,      // +0x38
}

// ---- Port Status and Control Register --------------------------------------

/// xHCI Port Status and Control Register (PORTSC).
///
/// Located at operational base + 0x400 + 0x10 * port_index.
#[derive(FromBytes)]
#[repr(C)]
pub struct XhciPortsc {
    /// Port Status and Control.
    pub portsc:     Volatile<u32>,      // +0x00
    /// Port PM Status and Control.
    pub portpmsc:   Volatile<u32>,      // +0x04
    /// Port Link Info.
    pub portli:     Volatile<u32>,      // +0x08
    /// Port Hardware LPM Control.
    pub porthlpmc:  Volatile<u32>,      // +0x0C
}

// ---- Interrupter Register Set ----------------------------------------------

/// xHCI Interrupter Register Set.
///
/// Located at runtime_base + 0x20 + 32 * interrupter_index.
#[derive(FromBytes)]
#[repr(C)]
pub struct XhciInterrupter {
    /// Interrupter Management Register.
    pub iman:       Volatile<u32>,      // +0x00
    /// Interrupter Moderation Register.
    pub imod:       Volatile<u32>,      // +0x04
    /// Event Ring Segment Table Size.
    pub erstsz:     Volatile<u32>,      // +0x08
    _rsvd:          u32,                // +0x0C
    /// Event Ring Segment Table Base Address (64-bit).
    pub erstba:     Volatile<u64>,      // +0x10
    /// Event Ring Dequeue Pointer (64-bit).
    pub erdp:       Volatile<u64>,      // +0x18
}

// ---- Transfer Request Block (TRB) ------------------------------------------

/// A Transfer Request Block (TRB) -- 16 bytes.
///
/// Used for command ring, transfer ring, and event ring entries.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Trb {
    /// TRB-type-specific parameter (varies by TRB type).
    pub parameter:  u64,
    /// Status field (completion code, transfer length, etc.).
    pub status:     u32,
    /// Control field (cycle bit, TRB type, flags).
    pub control:    u32,
}

impl Trb {
    /// Create a zeroed TRB.
    pub const fn zeroed() -> Self {
        Self { parameter: 0, status: 0, control: 0 }
    }

    /// Get the TRB type from bits 15:10 of the control field.
    pub fn trb_type(&self) -> u32 {
        (self.control >> 10) & 0x3F
    }

    /// Get the cycle bit (bit 0 of control).
    pub fn cycle_bit(&self) -> bool {
        self.control & 1 != 0
    }

    /// Get the completion code from bits 31:24 of the status field.
    pub fn completion_code(&self) -> u8 {
        (self.status >> 24) as u8
    }

    /// Get the slot ID from bits 31:24 of the control field.
    pub fn slot_id(&self) -> u8 {
        (self.control >> 24) as u8
    }
}

// ---- Event Ring Segment Table Entry ----------------------------------------

/// Event Ring Segment Table Entry (16 bytes).
#[derive(Clone, Copy)]
#[repr(C)]
pub struct EventRingSegmentTableEntry {
    /// Ring Segment Base Address (64-bit, 64-byte aligned).
    pub ring_base:  u64,
    /// Ring Segment Size (number of TRBs in this segment).
    pub ring_size:  u32,
    /// Reserved.
    pub _rsvd:      u32,
}

// ---- USBCMD bit definitions ------------------------------------------------

/// Run/Stop -- Setting this to 1 starts the xHC.
pub const USBCMD_RS:    u32 = 1 << 0;
/// Host Controller Reset.
pub const USBCMD_HCRST: u32 = 1 << 1;
/// Interrupter Enable.
pub const USBCMD_INTE:  u32 = 1 << 2;
/// Host System Error Enable.
pub const USBCMD_HSEE:  u32 = 1 << 3;

// ---- USBSTS bit definitions ------------------------------------------------

/// HCHalted -- 1 when the xHC has stopped running.
pub const USBSTS_HCH:   u32 = 1 << 0;
/// Host System Error.
pub const USBSTS_HSE:    u32 = 1 << 2;
/// Event Interrupt (EINT).
pub const USBSTS_EINT:   u32 = 1 << 3;
/// Port Change Detect.
pub const USBSTS_PCD:    u32 = 1 << 4;
/// Controller Not Ready.
pub const USBSTS_CNR:    u32 = 1 << 11;

// ---- PORTSC bit definitions ------------------------------------------------

/// Current Connect Status.
pub const PORTSC_CCS:          u32 = 1 << 0;
/// Port Enabled/Disabled.
pub const PORTSC_PED:          u32 = 1 << 1;
/// Port Reset.
pub const PORTSC_PR:           u32 = 1 << 4;
/// Port Link State mask (bits 8:5).
pub const PORTSC_PLS_MASK:     u32 = 0xF << 5;
/// Port Link State shift.
pub const PORTSC_PLS_SHIFT:    u32 = 5;
/// Port Power.
pub const PORTSC_PP:           u32 = 1 << 9;
/// Port Speed mask (bits 13:10).
pub const PORTSC_SPEED_MASK:   u32 = 0xF << 10;
/// Port Speed shift.
pub const PORTSC_SPEED_SHIFT:  u32 = 10;
/// Connect Status Change (write-1-to-clear).
pub const PORTSC_CSC:          u32 = 1 << 17;
/// Port Enabled/Disabled Change (write-1-to-clear).
pub const PORTSC_PEC:          u32 = 1 << 18;
/// Port Reset Change (write-1-to-clear).
pub const PORTSC_PRC:          u32 = 1 << 21;

/// Bitmask of all write-1-to-clear status change bits in PORTSC.
/// When writing PORTSC, preserve RW bits but do NOT accidentally clear
/// change bits by writing 1 to them.
pub const PORTSC_CHANGE_BITS:  u32 = PORTSC_CSC | PORTSC_PEC | PORTSC_PRC
    | (1 << 19) | (1 << 20) | (1 << 22) | (1 << 23);

// ---- PORTSC speed values ---------------------------------------------------

/// Full Speed (12 Mb/s).
pub const SPEED_FULL:  u32 = 1;
/// Low Speed (1.5 Mb/s).
pub const SPEED_LOW:   u32 = 2;
/// High Speed (480 Mb/s).
pub const SPEED_HIGH:  u32 = 3;
/// Super Speed (5 Gb/s).
pub const SPEED_SUPER: u32 = 4;

// ---- IMAN bit definitions --------------------------------------------------

/// Interrupt Pending (IP) -- write 1 to clear.
pub const IMAN_IP: u32 = 1 << 0;
/// Interrupt Enable (IE).
pub const IMAN_IE: u32 = 1 << 1;

// ---- CRCR bit definitions --------------------------------------------------

/// Ring Cycle State.
pub const CRCR_RCS:   u64 = 1 << 0;
/// Command Stop.
pub const CRCR_CS:    u64 = 1 << 1;
/// Command Abort.
pub const CRCR_CA:    u64 = 1 << 2;
/// Command Ring Running.
pub const CRCR_CRR:   u64 = 1 << 3;

// ---- TRB Type values -------------------------------------------------------

/// Normal TRB.
pub const TRB_TYPE_NORMAL:              u32 = 1;
/// Setup Stage TRB.
pub const TRB_TYPE_SETUP:               u32 = 2;
/// Data Stage TRB.
pub const TRB_TYPE_DATA:                u32 = 3;
/// Status Stage TRB.
pub const TRB_TYPE_STATUS:              u32 = 4;
/// Link TRB.
pub const TRB_TYPE_LINK:                u32 = 6;
/// No Op TRB (command ring).
pub const TRB_TYPE_NOOP:                u32 = 8;
/// Enable Slot Command TRB.
pub const TRB_TYPE_ENABLE_SLOT:         u32 = 9;
/// Disable Slot Command TRB.
pub const TRB_TYPE_DISABLE_SLOT:        u32 = 10;
/// Address Device Command TRB.
pub const TRB_TYPE_ADDRESS_DEVICE:      u32 = 11;
/// Configure Endpoint Command TRB.
pub const TRB_TYPE_CONFIGURE_ENDPOINT:  u32 = 12;
/// Evaluate Context Command TRB.
pub const TRB_TYPE_EVALUATE_CONTEXT:    u32 = 13;
/// Reset Endpoint Command TRB.
pub const TRB_TYPE_RESET_ENDPOINT:      u32 = 14;
/// Stop Endpoint Command TRB.
pub const TRB_TYPE_STOP_ENDPOINT:       u32 = 15;
/// Set TR Dequeue Pointer Command TRB.
pub const TRB_TYPE_SET_TR_DEQUEUE:      u32 = 16;
/// Reset Device Command TRB.
pub const TRB_TYPE_RESET_DEVICE:        u32 = 17;
/// No Op Command TRB.
pub const TRB_TYPE_NOOP_CMD:            u32 = 23;

// ---- Event TRB types -------------------------------------------------------

/// Transfer Event TRB.
pub const TRB_TYPE_TRANSFER_EVENT:      u32 = 32;
/// Command Completion Event TRB.
pub const TRB_TYPE_CMD_COMPLETION:      u32 = 33;
/// Port Status Change Event TRB.
pub const TRB_TYPE_PORT_STATUS_CHANGE:  u32 = 34;

// ---- TRB Completion Codes --------------------------------------------------

/// Success.
pub const TRB_COMP_SUCCESS:         u8 = 1;
/// Short Packet.
pub const TRB_COMP_SHORT_PACKET:    u8 = 13;

// ---- HCSPARAMS1 field extraction -------------------------------------------

/// Extract MaxSlots from HCSPARAMS1 (bits 7:0).
pub fn hcsparams1_max_slots(val: u32) -> u8 {
    (val & 0xFF) as u8
}

/// Extract MaxIntrs from HCSPARAMS1 (bits 18:8).
pub fn hcsparams1_max_intrs(val: u32) -> u16 {
    ((val >> 8) & 0x7FF) as u16
}

/// Extract MaxPorts from HCSPARAMS1 (bits 31:24).
pub fn hcsparams1_max_ports(val: u32) -> u8 {
    ((val >> 24) & 0xFF) as u8
}

// ---- Helper: speed to human-readable string --------------------------------

/// Convert a PORTSC speed value to a human-readable string.
pub fn speed_to_str(speed: u32) -> &'static str {
    match speed {
        SPEED_LOW   => "Low (1.5 Mb/s)",
        SPEED_FULL  => "Full (12 Mb/s)",
        SPEED_HIGH  => "High (480 Mb/s)",
        SPEED_SUPER => "Super (5 Gb/s)",
        _           => "Unknown",
    }
}

/// Build a TRB control word from a TRB type, cycle bit, and extra flags.
pub fn trb_control(trb_type: u32, cycle: bool, flags: u32) -> u32 {
    (trb_type << 10) | (if cycle { 1 } else { 0 }) | flags
}
