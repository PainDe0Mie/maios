//! Intel HDA register definitions.
//!
//! Memory-mapped register structs matching the Intel HD Audio specification.
//! All fields use `Volatile` or `ReadOnly` to ensure proper MMIO access.

use volatile::{Volatile, ReadOnly};
use zerocopy::FromBytes;

// ─── Global Registers (0x00..0x3F) ──────────────────────────────────────────

/// HDA controller global registers at BAR0 offset 0x00.
#[derive(FromBytes)]
#[repr(C)]
pub struct HdaRegisters {
    pub gcap:       ReadOnly<u16>,      // 0x00 Global Capabilities
    pub vmin:       ReadOnly<u8>,       // 0x02 Minor Version
    pub vmaj:       ReadOnly<u8>,       // 0x03 Major Version
    pub outpay:     ReadOnly<u16>,      // 0x04 Output Payload Capability
    pub inpay:      ReadOnly<u16>,      // 0x06 Input Payload Capability
    pub gctl:       Volatile<u32>,      // 0x08 Global Control
    pub wakeen:     Volatile<u16>,      // 0x0C Wake Enable
    pub statests:   Volatile<u16>,      // 0x0E State Change Status
    pub gsts:       ReadOnly<u16>,      // 0x10 Global Status
    _pad0:          [u8; 6],            // 0x12..0x17
    pub outstrmpay: ReadOnly<u16>,      // 0x18
    pub instrmpay:  ReadOnly<u16>,      // 0x1A
    _pad1:          [u8; 4],            // 0x1C..0x1F
    pub intctl:     Volatile<u32>,      // 0x20 Interrupt Control
    pub intsts:     ReadOnly<u32>,      // 0x24 Interrupt Status
    _pad2:          [u8; 8],            // 0x28..0x2F
    pub walclk:     ReadOnly<u32>,      // 0x30 Wall Clock Counter
    _pad3:          [u8; 4],            // 0x34..0x37
    pub ssync:      Volatile<u32>,      // 0x38 Stream Synchronization
    _pad4:          [u8; 4],            // 0x3C..0x3F

    // ── CORB registers (0x40..0x4F) ──
    pub corblbase:  Volatile<u32>,      // 0x40 CORB Lower Base Address
    pub corbubase:  Volatile<u32>,      // 0x44 CORB Upper Base Address
    pub corbwp:     Volatile<u16>,      // 0x48 CORB Write Pointer
    pub corbrp:     Volatile<u16>,      // 0x4A CORB Read Pointer
    pub corbctl:    Volatile<u8>,       // 0x4C CORB Control
    pub corbsts:    Volatile<u8>,       // 0x4D CORB Status
    pub corbsize:   Volatile<u8>,       // 0x4E CORB Size
    _pad5:          u8,                 // 0x4F

    // ── RIRB registers (0x50..0x5F) ──
    pub rirblbase:  Volatile<u32>,      // 0x50 RIRB Lower Base Address
    pub rirbubase:  Volatile<u32>,      // 0x54 RIRB Upper Base Address
    pub rirbwp:     Volatile<u16>,      // 0x58 RIRB Write Pointer
    pub rintcnt:    Volatile<u16>,      // 0x5A Response Interrupt Count
    pub rirbctl:    Volatile<u8>,       // 0x5C RIRB Control
    pub rirbsts:    Volatile<u8>,       // 0x5D RIRB Status
    pub rirbsize:   Volatile<u8>,       // 0x5E RIRB Size
    _pad6:          u8,                 // 0x5F

    // ── Immediate Command Interface (0x60..0x6B) ──
    pub icoi:       Volatile<u32>,      // 0x60 Immediate Command Output
    pub irii:       ReadOnly<u32>,      // 0x64 Immediate Response Input
    pub ics:        Volatile<u16>,      // 0x68 Immediate Command Status
    _pad7:          [u8; 0x16],         // 0x6A..0x7F

    // ── Stream Descriptor 0 (first output stream, at 0x80) ──
    pub sd0:        HdaStreamDesc,      // 0x80..0x9F
}

/// Stream Descriptor registers (0x20 bytes each).
#[derive(FromBytes)]
#[repr(C)]
pub struct HdaStreamDesc {
    pub ctl_lo:     Volatile<u16>,      // +0x00 Control low (bit 1=RUN, bit 0=SRST)
    pub ctl_hi:     Volatile<u8>,       // +0x02 Control high (bits 7:4 = stream number)
    pub sts:        Volatile<u8>,       // +0x03 Status (bit 2=BCIS, bit 3=FIFOE, bit 4=DESE)
    pub lpib:       ReadOnly<u32>,      // +0x04 Link Position In Buffer
    pub cbl:        Volatile<u32>,      // +0x08 Cyclic Buffer Length
    pub lvi:        Volatile<u16>,      // +0x0C Last Valid Index
    _pad0:          [u8; 2],            // +0x0E
    pub fifod:      ReadOnly<u16>,      // +0x10 FIFO Size
    pub fmt:        Volatile<u16>,      // +0x12 Format
    _pad1:          [u8; 4],            // +0x14
    pub bdlpl:      Volatile<u32>,      // +0x18 BDL Pointer Lower
    pub bdlpu:      Volatile<u32>,      // +0x1C BDL Pointer Upper
}

// ─── BDL Entry ──────────────────────────────────────────────────────────────

/// Buffer Descriptor List entry (16 bytes).
#[derive(Clone, Copy)]
#[repr(C)]
pub struct BdlEntry {
    /// Physical address of the audio buffer.
    pub address: u64,
    /// Length of the buffer in bytes.
    pub length: u32,
    /// Flags. Bit 0 = IOC (Interrupt On Completion).
    pub flags: u32,
}

// ─── GCTL bit definitions ───────────────────────────────────────────────────

/// Controller Reset bit in GCTL.
pub const GCTL_CRST: u32 = 1 << 0;

// ─── CORB/RIRB control bits ────────────────────────────────────────────────

/// CORB DMA Run bit.
pub const CORBCTL_MEIE: u8 = 1 << 0;
pub const CORBCTL_RUN:  u8 = 1 << 1;

/// CORB Read Pointer Reset bit.
pub const CORBRP_RST: u16 = 1 << 15;

/// RIRB DMA Run bit and interrupt control.
pub const RIRBCTL_INTCTL: u8 = 1 << 0;
pub const RIRBCTL_RUN:    u8 = 1 << 1;

/// RIRB Write Pointer Reset bit.
pub const RIRBWP_RST: u16 = 1 << 15;

// ─── Immediate Command Status bits ──────────────────────────────────────────

/// Immediate Command Busy.
pub const ICS_BUSY: u16 = 1 << 0;
/// Immediate Result Valid.
pub const ICS_VALID: u16 = 1 << 1;

// ─── Stream descriptor bits ────────────────────────────────────────────────

/// Stream run bit.
pub const SD_CTL_RUN:   u16 = 1 << 1;
/// Stream reset bit.
pub const SD_CTL_SRST:  u16 = 1 << 0;
/// Interrupt on completion enable.
pub const SD_CTL_IOCE:  u16 = 1 << 2;
/// FIFO error interrupt enable.
pub const SD_CTL_FEIE:  u16 = 1 << 3;
/// Descriptor error interrupt enable.
pub const SD_CTL_DEIE:  u16 = 1 << 4;

/// Buffer Completion Interrupt Status.
pub const SD_STS_BCIS: u8 = 1 << 2;

// ─── Stream format encoding ────────────────────────────────────────────────

/// 48 kHz, 16-bit, stereo PCM.
///
/// Encoding (Intel HDA spec 3.7.1):
/// - Bits 15:14 = 00 : base rate 48 kHz
/// - Bits 13:11 = 000 : multiply by 1
/// - Bits 10:8  = 000 : divide by 1
/// - Bits  7:4  = 0001 : 16 bits per sample
/// - Bits  3:0  = 0001 : 2 channels (channels - 1)
pub const FMT_48KHZ_16BIT_STEREO: u16 = 0x0011;

// ─── GCAP field extraction ─────────────────────────────────────────────────

/// Number of Output Streams Supported (bits 15:12 of GCAP).
pub fn gcap_oss(gcap: u16) -> u8 {
    ((gcap >> 12) & 0xF) as u8
}

/// Number of Input Streams Supported (bits 11:8 of GCAP).
pub fn gcap_iss(gcap: u16) -> u8 {
    ((gcap >> 8) & 0xF) as u8
}
