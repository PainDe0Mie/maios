//! Intel HD Audio (HDA) PCI driver for MaiOS.
//!
//! Supports basic PCM playback at 48 kHz / 16-bit / stereo through QEMU's
//! `intel-hda` controller and `hda-output` codec.

#![no_std]

extern crate alloc;

pub mod codec;
pub mod regs;
pub mod stream;

use alloc::boxed::Box;
use log::{info, warn, error};
use memory::{MappedPages, PhysicalAddress};
use pci::PciDevice;
use regs::{HdaRegisters, GCTL_CRST};
use spin::Once;
use sync_irq::IrqSafeMutex;

/// PCI class for multimedia devices.
pub const HDA_PCI_CLASS: u8 = 0x04;
/// PCI subclass for HD Audio controllers.
pub const HDA_PCI_SUBCLASS: u8 = 0x03;

/// MMIO region size to map (16 KB covers all registers + stream descriptors).
const MMIO_SIZE: usize = 0x4000;

/// CORB size: 256 entries of 4 bytes = 1 KB.
const CORB_ENTRIES: usize = 256;
const CORB_SIZE: usize = CORB_ENTRIES * 4;

/// RIRB size: 256 entries of 8 bytes = 2 KB.
const RIRB_ENTRIES: usize = 256;
const RIRB_SIZE: usize = RIRB_ENTRIES * 8;

/// Global HDA controller singleton.
static HDA_CONTROLLER: Once<IrqSafeMutex<HdaController>> = Once::new();

/// Get a reference to the HDA controller.
pub fn get_hda() -> Option<&'static IrqSafeMutex<HdaController>> {
    HDA_CONTROLLER.get()
}

/// The HDA controller state.
pub struct HdaController {
    /// MMIO mapped registers.
    _mmio_pages: MappedPages,
    /// Pointer to the register block (valid for the lifetime of _mmio_pages).
    regs_ptr: *mut HdaRegisters,
    /// CORB DMA buffer.
    _corb_pages: MappedPages,
    /// RIRB DMA buffer.
    _rirb_pages: MappedPages,
    /// Verb send/receive state.
    verb_state: codec::VerbState,
    /// Output stream (if initialized).
    output_stream: Option<stream::OutputStream>,
}

// Safety: HdaController is only accessed through the IrqSafeMutex.
unsafe impl Send for HdaController {}

impl HdaController {
    fn regs(&mut self) -> &mut HdaRegisters {
        unsafe { &mut *self.regs_ptr }
    }
}

/// Initialize the Intel HDA controller from a PCI device.
///
/// This function:
/// 1. Maps MMIO registers from BAR0
/// 2. Resets the controller
/// 3. Sets up CORB/RIRB for verb communication
/// 4. Discovers the codec and configures the output path
/// 5. Starts the output stream
/// 6. Spawns the audio pump task
pub fn init_from_pci(pci_dev: &PciDevice) -> Result<(), &'static str> {
    if HDA_CONTROLLER.get().is_some() {
        return Ok(()); // Already initialized.
    }

    info!("HDA: initializing PCI device at {:?} (vendor={:#06x} device={:#06x})",
        pci_dev.location, pci_dev.vendor_id, pci_dev.device_id);

    // ── PCI setup ───────────────────────────────────────────────────────
    let mem_base = pci_dev.determine_mem_base(0)?;
    pci_dev.pci_set_command_bus_master_bit();

    // ── Map MMIO ────────────────────────────────────────────────────────
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

    // ── Read capabilities ───────────────────────────────────────────────
    let gcap = regs.gcap.read();
    let oss = regs::gcap_oss(gcap);
    let iss = regs::gcap_iss(gcap);
    info!("HDA: GCAP={:#06x}, version={}.{}, OSS={}, ISS={}",
        gcap, regs.vmaj.read(), regs.vmin.read(), oss, iss);

    if oss == 0 {
        return Err("HDA: no output streams supported");
    }

    // ── Reset controller ────────────────────────────────────────────────
    // Clear CRST to enter reset.
    regs.gctl.write(regs.gctl.read() & !GCTL_CRST);
    for _ in 0..10_000 {
        if regs.gctl.read() & GCTL_CRST == 0 { break; }
        core::hint::spin_loop();
    }

    // Set CRST to exit reset.
    regs.gctl.write(regs.gctl.read() | GCTL_CRST);
    for _ in 0..100_000 {
        if regs.gctl.read() & GCTL_CRST != 0 { break; }
        core::hint::spin_loop();
    }

    // Wait for codecs to enumerate (spec says up to 521 us after CRST=1).
    for _ in 0..1_000_000 { core::hint::spin_loop(); }

    // ── Check for codecs ────────────────────────────────────────────────
    let statests = regs.statests.read();
    info!("HDA: STATESTS={:#06x} (codec presence)", statests);
    if statests == 0 {
        return Err("HDA: no codecs detected");
    }
    // Clear status bits by writing 1s.
    regs.statests.write(statests);

    let codec_id = statests.trailing_zeros() as u8;

    // ── Allocate CORB ───────────────────────────────────────────────────
    let corb_pages_raw = memory::allocate_pages_by_bytes(4096)
        .ok_or("HDA: failed to allocate CORB")?;
    let corb_mapped = kernel_mmi.lock().page_table
        .map_allocated_pages(corb_pages_raw, mmio_flags)?;
    let corb_phys = kernel_mmi.lock().page_table
        .translate(corb_mapped.start_address())
        .ok_or("HDA: CORB translate failed")?;
    let corb_va = corb_mapped.start_address().value();
    unsafe { core::ptr::write_bytes(corb_va as *mut u8, 0, 4096); }

    // ── Allocate RIRB ───────────────────────────────────────────────────
    let rirb_pages_raw = memory::allocate_pages_by_bytes(4096)
        .ok_or("HDA: failed to allocate RIRB")?;
    let rirb_mapped = kernel_mmi.lock().page_table
        .map_allocated_pages(rirb_pages_raw, mmio_flags)?;
    let rirb_phys = kernel_mmi.lock().page_table
        .translate(rirb_mapped.start_address())
        .ok_or("HDA: RIRB translate failed")?;
    let rirb_va = rirb_mapped.start_address().value();
    unsafe { core::ptr::write_bytes(rirb_va as *mut u8, 0, 4096); }

    // ── Configure CORB ──────────────────────────────────────────────────
    // Stop CORB DMA.
    regs.corbctl.write(0);
    for _ in 0..1000 {
        if regs.corbctl.read() & regs::CORBCTL_RUN == 0 { break; }
        core::hint::spin_loop();
    }

    // Set size to 256 entries.
    regs.corbsize.write(0x02); // 256 entries

    // Set CORB base address.
    regs.corblbase.write(corb_phys.value() as u32);
    regs.corbubase.write((corb_phys.value() >> 32) as u32);

    // Reset read pointer.
    regs.corbrp.write(regs::CORBRP_RST);
    for _ in 0..1000 {
        if regs.corbrp.read() & regs::CORBRP_RST != 0 { break; }
        core::hint::spin_loop();
    }
    regs.corbrp.write(0);
    for _ in 0..1000 {
        if regs.corbrp.read() & regs::CORBRP_RST == 0 { break; }
        core::hint::spin_loop();
    }

    // Reset write pointer.
    regs.corbwp.write(0);

    // Start CORB DMA.
    regs.corbctl.write(regs::CORBCTL_RUN);

    // ── Configure RIRB ──────────────────────────────────────────────────
    // Stop RIRB DMA.
    regs.rirbctl.write(0);
    for _ in 0..1000 {
        if regs.rirbctl.read() & regs::RIRBCTL_RUN == 0 { break; }
        core::hint::spin_loop();
    }

    // Set size to 256 entries.
    regs.rirbsize.write(0x02);

    // Set RIRB base address.
    regs.rirblbase.write(rirb_phys.value() as u32);
    regs.rirbubase.write((rirb_phys.value() >> 32) as u32);

    // Reset write pointer.
    regs.rirbwp.write(regs::RIRBWP_RST);

    // Start RIRB DMA.
    regs.rirbctl.write(regs::RIRBCTL_RUN);

    info!("HDA: CORB/RIRB configured");

    // ── Codec discovery ─────────────────────────────────────────────────
    let mut verb_state = codec::VerbState {
        corb_va,
        rirb_va,
        corb_wp: 0,
        rirb_rp: 0,
    };

    let output_path = codec::find_output_path(&mut verb_state, regs, codec_id)?;

    // ── Configure output stream ─────────────────────────────────────────
    let stream_tag = 1u8; // Stream tags are 1-based.
    codec::configure_output(&mut verb_state, regs, codec_id, &output_path, stream_tag)?;

    // Initialize the audio mixer (if not already done).
    audio_mixer::init()?;

    let mut output_stream = stream::OutputStream::new(stream_tag)?;
    output_stream.configure(&mut regs.sd0);

    // Pre-fill the DMA buffer with silence.
    output_stream.refill_from_mixer(&mut regs.sd0);

    // Start the stream.
    output_stream.start(&mut regs.sd0);
    info!("HDA: output stream started");

    // ── Enable global interrupts ────────────────────────────────────────
    // Enable interrupt for stream 0 (bit 0) + global interrupt enable (bit 31).
    regs.intctl.write((1 << 31) | (1 << 0));

    // ── Store controller ────────────────────────────────────────────────
    let controller = HdaController {
        _mmio_pages: mmio_pages,
        regs_ptr,
        _corb_pages: corb_mapped,
        _rirb_pages: rirb_mapped,
        verb_state,
        output_stream: Some(output_stream),
    };

    HDA_CONTROLLER.call_once(|| IrqSafeMutex::new(controller));

    // ── Spawn audio pump task ───────────────────────────────────────────
    spawn_audio_pump();

    info!("HDA: initialization complete");
    Ok(())
}

/// Spawn a kernel task that periodically refills the DMA buffer from the mixer.
fn spawn_audio_pump() {
    let builder = spawn::new_task_builder(audio_pump_entry, ())
        .name(alloc::string::String::from("hda_audio_pump"));
    match builder.spawn() {
        Ok(_) => info!("HDA: audio pump task spawned"),
        Err(e) => error!("HDA: failed to spawn audio pump: {}", e),
    }
}

/// Entry point for the audio pump kernel task.
fn audio_pump_entry(_: ()) -> ! {
    loop {
        if let Some(hda) = get_hda() {
            let mut hda = hda.lock();
            let regs_ptr = hda.regs_ptr;
            if let Some(ref mut stream) = hda.output_stream {
                let regs = unsafe { &mut *regs_ptr };
                stream.refill_from_mixer(&mut regs.sd0);
            }
        }
        // Sleep ~5 ms (well under one DMA half-buffer of ~21 ms at 48kHz).
        sleep::sleep(core::time::Duration::from_millis(5)).unwrap_or(());
    }
}
