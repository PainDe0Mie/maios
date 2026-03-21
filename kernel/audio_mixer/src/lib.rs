//! Kernel audio mixer for MaiOS.
//!
//! Provides a global PCM ring buffer where applications write audio data
//! and the audio driver reads from it to fill DMA buffers.
//!
//! Fixed format: 48 kHz, 16-bit signed LE, stereo (4 bytes per sample frame).

#![no_std]

extern crate alloc;

use log::info;
use spin::Once;

#[cfg(target_arch = "x86_64")]
use memory::MappedPages;

/// Sample rate in Hz.
pub const SAMPLE_RATE: u32 = 48_000;
/// Number of audio channels (stereo).
pub const CHANNELS: u16 = 2;
/// Bits per sample.
pub const BITS_PER_SAMPLE: u16 = 16;
/// Bytes per sample frame (2 channels * 2 bytes).
pub const FRAME_SIZE: usize = (CHANNELS as usize) * (BITS_PER_SAMPLE as usize / 8);

/// Ring buffer capacity in sample frames (~1.36 seconds at 48 kHz).
const RING_FRAMES: usize = 65_536;
/// Ring buffer capacity in bytes.
const RING_BYTES: usize = RING_FRAMES * FRAME_SIZE;

/// The global audio mixer singleton.
static AUDIO_MIXER: Once<spin::Mutex<AudioMixer>> = Once::new();

/// Get a reference to the global audio mixer, if initialized.
pub fn get_mixer() -> Option<&'static spin::Mutex<AudioMixer>> {
    AUDIO_MIXER.get()
}

/// Initialize the global audio mixer. Call once during boot.
///
/// Returns a reference to the mixer, or an error if allocation fails.
#[cfg(target_arch = "x86_64")]
pub fn init() -> Result<&'static spin::Mutex<AudioMixer>, &'static str> {
    if let Some(m) = AUDIO_MIXER.get() {
        return Ok(m);
    }

    let kernel_mmi = memory::get_kernel_mmi_ref()
        .ok_or("audio_mixer: no kernel MMI")?;

    // Allocate ring buffer pages (256 KB).
    let pages = memory::allocate_pages_by_bytes(RING_BYTES)
        .ok_or("audio_mixer: failed to allocate ring buffer pages")?;

    let flags = pte_flags::PteFlags::new()
        .valid(true)
        .writable(true);

    let mapped = kernel_mmi.lock().page_table
        .map_allocated_pages(pages, flags)?;

    // Zero-initialize the buffer.
    let va = mapped.start_address().value();
    unsafe { core::ptr::write_bytes(va as *mut u8, 0, RING_BYTES); }

    let mixer = AudioMixer {
        _pages: mapped,
        buf: va,
        write_pos: 0,
        read_pos: 0,
    };

    let mixer_ref = AUDIO_MIXER.call_once(|| spin::Mutex::new(mixer));
    info!("Audio mixer initialized: {}Hz {}ch {}bit, ring={}KB",
        SAMPLE_RATE, CHANNELS, BITS_PER_SAMPLE, RING_BYTES / 1024);
    Ok(mixer_ref)
}

/// The kernel audio mixer.
///
/// Uses a single-producer/single-consumer ring buffer. Applications write
/// PCM data via [`write_pcm`], and the audio driver reads it via
/// [`read_pcm_into`].
pub struct AudioMixer {
    /// Backing pages — kept alive so the buffer is not unmapped.
    #[cfg(target_arch = "x86_64")]
    _pages: MappedPages,
    /// Virtual address of the ring buffer start.
    buf: usize,
    /// Write cursor (byte offset into the ring buffer). Advanced by producers.
    write_pos: usize,
    /// Read cursor (byte offset into the ring buffer). Advanced by the audio driver.
    read_pos: usize,
}

// Safety: the mixer is only accessed through the Mutex wrapper.
unsafe impl Send for AudioMixer {}

impl AudioMixer {
    /// Number of bytes available for reading (buffered audio data).
    #[inline]
    fn buffered_bytes(&self) -> usize {
        self.write_pos.wrapping_sub(self.read_pos) % RING_BYTES
    }

    /// Number of bytes of free space for writing.
    #[inline]
    fn free_bytes(&self) -> usize {
        // Leave 1 frame unused to distinguish full from empty.
        RING_BYTES - FRAME_SIZE - self.buffered_bytes()
    }

    /// Number of complete sample frames available for reading.
    pub fn available_frames(&self) -> usize {
        self.buffered_bytes() / FRAME_SIZE
    }

    /// Write PCM sample data into the ring buffer.
    ///
    /// `data` must contain interleaved 16-bit signed LE stereo samples.
    /// The length must be a multiple of [`FRAME_SIZE`] (4 bytes).
    /// Returns the number of bytes actually written (may be less than
    /// `data.len()` if the buffer is nearly full).
    pub fn write_pcm(&mut self, data: &[u8]) -> usize {
        let avail = self.free_bytes();
        let to_write = data.len().min(avail);
        // Round down to frame boundary.
        let to_write = to_write & !(FRAME_SIZE - 1);
        if to_write == 0 {
            return 0;
        }

        let offset = self.write_pos % RING_BYTES;
        let first = (RING_BYTES - offset).min(to_write);
        unsafe {
            core::ptr::copy_nonoverlapping(
                data.as_ptr(),
                (self.buf + offset) as *mut u8,
                first,
            );
            if first < to_write {
                core::ptr::copy_nonoverlapping(
                    data.as_ptr().add(first),
                    self.buf as *mut u8,
                    to_write - first,
                );
            }
        }
        self.write_pos = (self.write_pos + to_write) % RING_BYTES;
        to_write
    }

    /// Read and consume PCM data from the ring buffer into `dst`.
    ///
    /// Copies up to `dst.len()` bytes of audio data. If less data is
    /// available than requested, the remainder of `dst` is filled with
    /// silence (zeros). Returns the number of bytes of real audio copied.
    pub fn read_pcm_into(&mut self, dst: &mut [u8]) -> usize {
        let avail = self.buffered_bytes();
        let to_read = dst.len().min(avail);
        let to_read = to_read & !(FRAME_SIZE - 1);

        if to_read > 0 {
            let offset = self.read_pos % RING_BYTES;
            let first = (RING_BYTES - offset).min(to_read);
            unsafe {
                core::ptr::copy_nonoverlapping(
                    (self.buf + offset) as *const u8,
                    dst.as_mut_ptr(),
                    first,
                );
                if first < to_read {
                    core::ptr::copy_nonoverlapping(
                        self.buf as *const u8,
                        dst.as_mut_ptr().add(first),
                        to_read - first,
                    );
                }
            }
            self.read_pos = (self.read_pos + to_read) % RING_BYTES;
        }

        // Fill remainder with silence.
        if to_read < dst.len() {
            unsafe {
                core::ptr::write_bytes(
                    dst.as_mut_ptr().add(to_read),
                    0,
                    dst.len() - to_read,
                );
            }
        }

        to_read
    }
}
