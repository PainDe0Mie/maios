//! Intel HD Audio (HDA) PCI driver for MaiOS.
//!
//! Supports basic PCM playback at 48 kHz / 16-bit / stereo through QEMU's
//! `intel-hda` controller and `hda-output` codec.

#![no_std]

#![allow(unused_imports)]

extern crate alloc;

pub mod codec;
pub mod regs;
pub mod stream;

use log::warn;
use memory::{MappedPages, PhysicalAddress};
use pci::PciDevice;
use regs::{HdaRegisters, GCTL_CRST};
use spin::{Mutex, Once};

/// PCI class for multimedia devices.
pub const HDA_PCI_CLASS: u8 = 0x04;
/// PCI subclass for HD Audio controllers.
pub const HDA_PCI_SUBCLASS: u8 = 0x03;

/// MMIO region size to map (16 KB covers all registers + stream descriptors).
const MMIO_SIZE: usize = 0x4000;

/// Global HDA controller singleton.
///
/// Uses a regular `spin::Mutex` instead of `IrqSafeMutex` because:
/// - The audio pump is a normal kernel task, not an interrupt handler.
/// - `IrqSafeMutex` masks the LAPIC timer on every lock/try_lock, which
///   starves the scheduler when called frequently (every 10 ms).
static HDA_CONTROLLER: Once<Mutex<HdaController>> = Once::new();

/// Get a reference to the HDA controller.
pub fn get_hda() -> Option<&'static Mutex<HdaController>> {
    HDA_CONTROLLER.get()
}

/// The HDA controller state.
pub struct HdaController {
    /// MMIO mapped registers.
    _mmio_pages: MappedPages,
    /// Pointer to the register block (valid for the lifetime of _mmio_pages).
    regs_ptr: *mut HdaRegisters,
    /// Output stream (if initialized).
    output_stream: Option<stream::OutputStream>,
}

// Safety: HdaController is only accessed through the spin::Mutex.
unsafe impl Send for HdaController {}

impl HdaController {
    fn regs(&mut self) -> &mut HdaRegisters {
        unsafe { &mut *self.regs_ptr }
    }
}

/// Initialize the Intel HDA controller from a PCI device.
///
/// Performs PCI config, controller reset, codec discovery, and stream setup
/// synchronously. Spawns a background pump task (10ms interval) that
/// refills the DMA buffer from the audio mixer.
pub fn init_from_pci(pci_dev: &PciDevice) -> Result<(), &'static str> {
    if HDA_CONTROLLER.get().is_some() {
        return Ok(());
    }

    warn!("HDA: init PCI {:?} vendor={:#06x} device={:#06x}",
        pci_dev.location, pci_dev.vendor_id, pci_dev.device_id);
    let mem_base = pci_dev.determine_mem_base(0)?;
    pci_dev.pci_set_command_bus_master_bit();
    pci_dev.pci_set_intx_disable_bit(true);
    warn!("HDA: BAR0 phys={:#010x}, bus master enabled, INTx disabled", mem_base.value());

    hda_init_inner(mem_base)?;
    Ok(())
}

/// Kill all HDA interrupt sources on the controller.
///
/// Must be called after every controller state change that could arm an
/// interrupt (reset, stream start, etc.). Without a registered IRQ handler,
/// any asserted HDA interrupt causes an IRQ storm that freezes the system.
fn kill_hda_interrupts(regs: &mut HdaRegisters) {
    regs.intctl.write(0);      // Disable global + per-stream interrupts
    regs.wakeen.write(0);      // Disable wake events (STATESTS -> IRQ)
    // Clear pending status (write-1-to-clear)
    let sts = regs.statests.read();
    if sts != 0 { regs.statests.write(sts); }
    // Clear stream 0 status bits (BCIS, FIFOE, DESE)
    regs.sd0.sts.write(0x1C);
}

fn hda_init_inner(mem_base: PhysicalAddress) -> Result<(), &'static str> {

    // ── Step 1: Map MMIO ──
    let kernel_mmi = memory::get_kernel_mmi_ref()
        .ok_or("HDA: no kernel MMI")?;
    let mmio_flags = pte_flags::PteFlags::new()
        .valid(true)
        .writable(true)
        .device_memory(true);
    let mmio_pages_raw = memory::allocate_pages_by_bytes(MMIO_SIZE)
        .ok_or("HDA: failed to allocate MMIO pages")?;
    let mmio_frames = memory::allocate_frames_by_bytes_at(mem_base, MMIO_SIZE)
        .map_err(|_| "HDA: failed to allocate MMIO frames at BAR0")?;
    let mmio_pages = kernel_mmi.lock().page_table
        .map_allocated_pages_to(mmio_pages_raw, mmio_frames, mmio_flags)?;
    let regs_ptr = mmio_pages.start_address().value() as *mut HdaRegisters;
    let regs = unsafe { &mut *regs_ptr };

    warn!("HDA: GCAP={:#06x} v{}.{} OSS={} ISS={}",
        regs.gcap.read(),
        (regs.vmaj.read()), (regs.vmin.read()),
        (regs.gcap.read() >> 12) & 0xF, (regs.gcap.read() >> 8) & 0xF);

    // ── Step 2: Reset controller ──
    warn!("HDA: resetting controller...");
    regs.gctl.write(0);
    for _ in 0..100_000 {
        if regs.gctl.read() & GCTL_CRST == 0 { break; }
        core::hint::spin_loop();
    }
    regs.gctl.write(GCTL_CRST);
    for _ in 0..1_000_000 {
        if regs.gctl.read() & GCTL_CRST != 0 { break; }
        core::hint::spin_loop();
    }

    // CRITICAL: kill interrupt enables immediately, but do NOT clear STATESTS yet —
    // we need it to detect which codecs were found after enumeration.
    regs.intctl.write(0);
    regs.wakeen.write(0);
    regs.sd0.sts.write(0x1C);
    warn!("HDA: reset done, GCTL={:#x}, IRQ enables killed (STATESTS preserved)",
        regs.gctl.read());

    // Wait for codec enumeration (spec: up to 521 us after CRST).
    // Spin-wait ~1ms instead of sleep() to avoid scheduler deadlock.
    for _ in 0..1_000_000 { core::hint::spin_loop(); }
    let statests = regs.statests.read();
    warn!("HDA: STATESTS={:#06x}", statests);
    if statests == 0 {
        return Err("HDA: no codecs found");
    }
    // NOW clear STATESTS (write-1-to-clear)
    regs.statests.write(statests);

    // ── Step 3: Discover codec ──
    let codec_id = (0u8..15).find(|i| statests & (1 << i) != 0)
        .ok_or("HDA: no codec bit set")?;
    warn!("HDA: using codec {}", codec_id);
    warn!("HDA: discovering codec...");
    let output_path = codec::find_output_path(regs, codec_id)?;

    // ── Step 4: Configure output ──
    warn!("HDA: configuring output...");
    let stream_tag: u8 = 1;
    codec::configure_output(regs, codec_id, &output_path, stream_tag)?;

    // Kill interrupts again (ICI verb exchanges may have side effects)
    kill_hda_interrupts(regs);

    // ── Step 5: Initialize mixer ──
    warn!("HDA: initializing mixer...");
    audio_mixer::init()?;

    // ── Step 6: Set up stream ──
    warn!("HDA: setting up stream...");
    let mut output_stream = stream::OutputStream::new(stream_tag)?;
    output_stream.configure(&mut regs.sd0);

    // Kill interrupts after stream configure (stream reset can re-arm bits)
    kill_hda_interrupts(regs);

    // Stream is configured but NOT started — it starts on-demand when
    // audio data is written to the mixer. This avoids a QEMU HDA DMA issue
    // where a continuously-running stream causes system freezes.

    // ── Step 7: Store controller & spawn pump ──
    let controller = HdaController {
        _mmio_pages: mmio_pages,
        regs_ptr,
        output_stream: Some(output_stream),
    };
    HDA_CONTROLLER.call_once(|| Mutex::new(controller));

    // Register pump callback — called from:
    // 1. sys_audio_write (immediate response when app writes audio)
    // 2. Timer tick handler (periodic refill every ~10ms tick)
    //
    // No background task: Theseus's scheduler has lock contention issues
    // when tasks call sleep() in tight loops. The timer-driven pump avoids
    // this by running inside the existing timer interrupt — no sleep/wake
    // cycles, no extra scheduler load.
    audio_mixer::register_hw_pump(pump);

    warn!("HDA: init complete — pump from timer tick + inline syscall");
    Ok(())
}

/// Pump audio from the mixer to the DMA buffer.
///
/// Called from two paths:
/// 1. Timer tick handler (CPU 0, every ~10ms) for periodic DMA refill.
/// 2. Inline from `sys_audio_write` via registered callback for immediate response.
pub fn pump() {
    if let Some(hda_ref) = get_hda() {
        if let Some(mut hda) = hda_ref.try_lock() {
            let regs_ptr = hda.regs_ptr;
            if let Some(ref mut stream) = hda.output_stream {
                let regs = unsafe { &mut *regs_ptr };
                stream.refill_from_mixer(&mut regs.sd0);
            }
        }
    }
}
