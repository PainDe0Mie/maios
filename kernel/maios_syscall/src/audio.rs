//! Audio syscalls for MaiOS.
//!
//! Provides SYS_AUDIO_WRITE to let applications write PCM data to the
//! kernel audio mixer for playback.

use crate::error::{SyscallResult, SyscallError};

/// Write PCM audio data to the kernel mixer.
///
/// # Arguments
/// - `buf_ptr` (arg0): pointer to PCM data (interleaved i16 LE stereo, 48 kHz)
/// - `buf_len` (arg1): length in bytes (must be a multiple of 4)
///
/// # Returns
/// Number of bytes actually written, or an error.
pub fn sys_audio_write(buf_ptr: u64, buf_len: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    if buf_ptr == 0 || buf_len == 0 {
        return Ok(0);
    }
    if buf_len % 4 != 0 {
        return Err(SyscallError::InvalidArgument);
    }

    let mixer = audio_mixer::get_mixer()
        .ok_or(SyscallError::NoDevice)?;

    let data = unsafe {
        core::slice::from_raw_parts(buf_ptr as *const u8, buf_len as usize)
    };

    let written = mixer.lock().write_pcm(data);

    // Pump audio to the HDA DMA buffer via callback.
    // No background pump task — Theseus's scheduler deadlocks on sleep loops.
    audio_mixer::pump_hardware();

    Ok(written as u64)
}
