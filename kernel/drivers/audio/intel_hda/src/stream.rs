//! HDA output stream management.
//!
//! Manages a single output stream descriptor, its Buffer Descriptor List (BDL),
//! and the DMA playback buffer. Uses double-buffering (2 x 4KB pages).

use crate::regs::{self, BdlEntry, HdaStreamDesc};
use log::{debug, warn};
use memory::{MappedPages, PhysicalAddress};

/// Size of each DMA buffer half (4 KB = 1024 stereo 16-bit frames).
const DMA_HALF_SIZE: usize = 4096;
/// Total DMA buffer size (both halves).
const DMA_TOTAL_SIZE: usize = DMA_HALF_SIZE * 2;
/// Number of BDL entries.
const BDL_ENTRIES: usize = 2;

/// An output stream with its BDL and DMA buffers.
pub struct OutputStream {
    /// Stream tag (1-15, assigned to the codec's converter).
    pub stream_tag: u8,
    /// BDL backing pages.
    _bdl_pages: MappedPages,
    /// BDL physical address (for the stream descriptor register).
    bdl_phys: PhysicalAddress,
    /// DMA playback buffer backing pages.
    _dma_pages: MappedPages,
    /// DMA buffer virtual address.
    dma_va: usize,
    /// Physical address of DMA buffer start.
    dma_phys: PhysicalAddress,
    /// Tracks which half was last refilled to avoid redundant copies.
    last_refilled_half: u8,
    /// Whether the DMA stream is currently running.
    pub running: bool,
    /// Number of consecutive pump cycles with no data from the mixer.
    silence_cycles: u32,
}

impl OutputStream {
    /// Allocate BDL and DMA buffers, set up the stream descriptor.
    pub fn new(stream_tag: u8) -> Result<Self, &'static str> {
        let kernel_mmi = memory::get_kernel_mmi_ref()
            .ok_or("HDA stream: no kernel MMI")?;

        let flags = pte_flags::PteFlags::new()
            .valid(true)
            .writable(true)
            .device_memory(true);

        // Allocate BDL (256 bytes needed, but allocate a full page).
        let bdl_pages_raw = memory::allocate_pages_by_bytes(4096)
            .ok_or("HDA: failed to allocate BDL pages")?;
        let bdl_mapped = kernel_mmi.lock().page_table
            .map_allocated_pages(bdl_pages_raw, flags)?;
        let bdl_phys = kernel_mmi.lock().page_table
            .translate(bdl_mapped.start_address())
            .ok_or("HDA: failed to translate BDL VA→PA")?;

        // Allocate DMA buffer (2 pages = 8KB).
        let dma_pages_raw = memory::allocate_pages_by_bytes(DMA_TOTAL_SIZE)
            .ok_or("HDA: failed to allocate DMA buffer pages")?;
        let dma_mapped = kernel_mmi.lock().page_table
            .map_allocated_pages(dma_pages_raw, flags)?;
        let dma_phys = kernel_mmi.lock().page_table
            .translate(dma_mapped.start_address())
            .ok_or("HDA: failed to translate DMA VA→PA")?;
        let dma_va = dma_mapped.start_address().value();

        // Zero-fill both buffers.
        unsafe {
            core::ptr::write_bytes(bdl_mapped.start_address().value() as *mut u8, 0, 4096);
            core::ptr::write_bytes(dma_va as *mut u8, 0, DMA_TOTAL_SIZE);
        }

        // Set up BDL entries pointing to the two DMA halves.
        let bdl_va = bdl_mapped.start_address().value();
        let dma_phys_val = dma_phys.value() as u64;
        let entries = [
            BdlEntry {
                address: dma_phys_val,
                length: DMA_HALF_SIZE as u32,
                flags: 0, // No IOC — we poll LPIB, no interrupts needed
            },
            BdlEntry {
                address: dma_phys_val + DMA_HALF_SIZE as u64,
                length: DMA_HALF_SIZE as u32,
                flags: 0, // No IOC
            },
        ];
        unsafe {
            let bdl_ptr = bdl_va as *mut BdlEntry;
            core::ptr::copy_nonoverlapping(entries.as_ptr(), bdl_ptr, BDL_ENTRIES);
        }

        warn!("HDA stream: BDL phys={:#010x}, DMA phys={:#010x} ({})",
            bdl_phys.value(), dma_phys.value(),
            if dma_phys.value() > 0xFFFF_FFFF { "ABOVE 4GB!" } else { "below 4GB" });

        Ok(OutputStream {
            stream_tag,
            _bdl_pages: bdl_mapped,
            bdl_phys,
            _dma_pages: dma_mapped,
            dma_va,
            dma_phys,
            last_refilled_half: 1,
            running: false,
            silence_cycles: 0,
        })
    }

    /// Configure the stream descriptor registers. Must be called before `start`.
    pub fn configure(&self, sd: &mut HdaStreamDesc) {
        // Reset the stream.
        sd.ctl_lo.write(regs::SD_CTL_SRST);
        for _ in 0..1000 {
            if sd.ctl_lo.read() & regs::SD_CTL_SRST != 0 {
                break;
            }
            core::hint::spin_loop();
        }
        // Clear reset.
        sd.ctl_lo.write(0);
        for _ in 0..1000 {
            if sd.ctl_lo.read() & regs::SD_CTL_SRST == 0 {
                break;
            }
            core::hint::spin_loop();
        }

        // Clear any pending status bits.
        sd.sts.write(0x1C); // Write 1 to clear BCIS, FIFOE, DESE.

        // Set BDL pointer.
        sd.bdlpl.write(self.bdl_phys.value() as u32);
        sd.bdlpu.write((self.bdl_phys.value() >> 32) as u32);

        // Set cyclic buffer length.
        sd.cbl.write(DMA_TOTAL_SIZE as u32);

        // Set last valid index (0-based: 2 entries → LVI = 1).
        sd.lvi.write((BDL_ENTRIES - 1) as u16);

        // Set format: 48kHz, 16-bit, stereo.
        sd.fmt.write(regs::FMT_48KHZ_16BIT_STEREO);

        // Set stream number in CTL high byte (bits 7:4 = stream tag).
        sd.ctl_hi.write(self.stream_tag << 4);

        debug!("HDA stream {}: configured, CBL={}, LVI={}, FMT={:#06x}",
            self.stream_tag, DMA_TOTAL_SIZE, BDL_ENTRIES - 1, regs::FMT_48KHZ_16BIT_STEREO);
    }

    /// Start the output stream (set RUN bit only, no interrupt enables).
    ///
    /// We poll LPIB from the audio pump instead of using interrupts,
    /// so IOCE (Interrupt On Completion Enable) is deliberately not set.
    pub fn start(&self, sd: &mut HdaStreamDesc) {
        let ctl = sd.ctl_lo.read();
        sd.ctl_lo.write(ctl | regs::SD_CTL_RUN);
    }

    /// Stop the output stream.
    pub fn stop(&self, sd: &mut HdaStreamDesc) {
        let ctl = sd.ctl_lo.read();
        sd.ctl_lo.write(ctl & !regs::SD_CTL_RUN);
    }

    /// Number of silence cycles before stopping the stream (~500ms at 10ms pump).
    const SILENCE_STOP_THRESHOLD: u32 = 50;

    /// Refill the DMA buffer from the audio mixer.
    ///
    /// On-demand streaming: the DMA stream only runs when the mixer has audio
    /// data. This avoids a QEMU DMA issue where a continuously-running HDA
    /// stream causes system freezes.
    pub fn refill_from_mixer(&mut self, sd: &mut HdaStreamDesc) {
        // Check if the mixer has data.
        let has_data = audio_mixer::get_mixer()
            .and_then(|m| m.try_lock())
            .map(|m| m.available_frames() > 0)
            .unwrap_or(false);

        if !has_data {
            self.silence_cycles += 1;
            if self.running && self.silence_cycles > Self::SILENCE_STOP_THRESHOLD {
                self.stop(sd);
                self.running = false;
                warn!("HDA: stream stopped (no audio data)");
            }
            return;
        }

        // We have data — reset silence counter.
        self.silence_cycles = 0;

        // Start stream on-demand if not running.
        if !self.running {
            // Pre-fill both halves before starting.
            for half in 0..2u8 {
                let offset = half as usize * DMA_HALF_SIZE;
                let dst = unsafe {
                    core::slice::from_raw_parts_mut(
                        (self.dma_va + offset) as *mut u8,
                        DMA_HALF_SIZE,
                    )
                };
                if let Some(mixer) = audio_mixer::get_mixer() {
                    if let Some(mut m) = mixer.try_lock() {
                        m.read_pcm_into(dst);
                    }
                }
            }
            self.last_refilled_half = 1;
            self.start(sd);
            self.running = true;
            warn!("HDA: stream started (audio data available)");
            return;
        }

        // Stream is running — refill the half not being played.
        let lpib = sd.lpib.read() as usize;
        let current_half = if lpib < DMA_HALF_SIZE { 0u8 } else { 1u8 };
        let refill_half = 1 - current_half;
        if refill_half == self.last_refilled_half {
            return;
        }

        let offset = refill_half as usize * DMA_HALF_SIZE;
        let dst = unsafe {
            core::slice::from_raw_parts_mut(
                (self.dma_va + offset) as *mut u8,
                DMA_HALF_SIZE,
            )
        };

        if let Some(mixer) = audio_mixer::get_mixer() {
            if let Some(mut m) = mixer.try_lock() {
                m.read_pcm_into(dst);
            }
        }

        self.last_refilled_half = refill_half;
    }
}
