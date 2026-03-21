//! HDA output stream management.
//!
//! Manages a single output stream descriptor, its Buffer Descriptor List (BDL),
//! and the DMA playback buffer. Uses double-buffering (2 x 4KB pages).

use crate::regs::{self, BdlEntry, HdaStreamDesc};
use log::debug;
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
                flags: 1, // IOC
            },
            BdlEntry {
                address: dma_phys_val + DMA_HALF_SIZE as u64,
                length: DMA_HALF_SIZE as u32,
                flags: 1, // IOC
            },
        ];
        unsafe {
            let bdl_ptr = bdl_va as *mut BdlEntry;
            core::ptr::copy_nonoverlapping(entries.as_ptr(), bdl_ptr, BDL_ENTRIES);
        }

        Ok(OutputStream {
            stream_tag,
            _bdl_pages: bdl_mapped,
            bdl_phys,
            _dma_pages: dma_mapped,
            dma_va,
            dma_phys,
            last_refilled_half: 1, // Pretend half 1 was just refilled so we start with half 0.
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

    /// Start the output stream (set RUN bit + interrupt enables).
    pub fn start(&self, sd: &mut HdaStreamDesc) {
        let ctl = sd.ctl_lo.read();
        sd.ctl_lo.write(ctl | regs::SD_CTL_RUN | regs::SD_CTL_IOCE);
    }

    /// Stop the output stream.
    pub fn stop(&self, sd: &mut HdaStreamDesc) {
        let ctl = sd.ctl_lo.read();
        sd.ctl_lo.write(ctl & !regs::SD_CTL_RUN);
    }

    /// Refill the DMA buffer from the audio mixer.
    ///
    /// Reads LPIB to determine which half has been consumed by the controller,
    /// then refills the consumed half with data from the mixer.
    pub fn refill_from_mixer(&mut self, sd: &mut HdaStreamDesc) {
        let lpib = sd.lpib.read() as usize;
        // Determine which half the controller is currently playing.
        let current_half = if lpib < DMA_HALF_SIZE { 0u8 } else { 1u8 };

        // Refill the OTHER half (the one not being played).
        let refill_half = 1 - current_half;
        if refill_half == self.last_refilled_half {
            return; // Already refilled, nothing to do.
        }

        let offset = refill_half as usize * DMA_HALF_SIZE;
        let dst = unsafe {
            core::slice::from_raw_parts_mut(
                (self.dma_va + offset) as *mut u8,
                DMA_HALF_SIZE,
            )
        };

        // Read from the global audio mixer.
        if let Some(mixer) = audio_mixer::get_mixer() {
            mixer.lock().read_pcm_into(dst);
        } else {
            // No mixer — fill with silence.
            unsafe { core::ptr::write_bytes(dst.as_mut_ptr(), 0, DMA_HALF_SIZE); }
        }

        self.last_refilled_half = refill_half;
    }
}
