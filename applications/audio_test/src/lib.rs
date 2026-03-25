//! Audio test application for MaiOS.
//!
//! Generates a 440 Hz sine wave and writes it to the audio mixer
//! for 2 seconds of playback.

#![no_std]

extern crate alloc;
#[macro_use]
extern crate app_io;

use alloc::string::String;
use alloc::vec::Vec;

/// Fast sine approximation using a 3rd-order polynomial (Bhaskara I).
///
/// Input: angle in radians.
/// Output: approximate sin(angle) in range [-1.0, 1.0].
/// Accuracy: max error ~0.001, good enough for audio.
fn fast_sin(x: f64) -> f64 {
    use core::f64::consts::PI;

    // Normalize to [0, 2*PI)
    let mut t = x % (2.0 * PI);
    if t < 0.0 { t += 2.0 * PI; }

    // Map to [0, PI] and track sign
    let sign = if t > PI { t -= PI; -1.0 } else { 1.0 };

    // Bhaskara I approximation: sin(x) ≈ 16x(π-x) / (5π² - 4x(π-x))
    let pmx = PI - t;
    let num = 16.0 * t * pmx;
    let den = 5.0 * PI * PI - 4.0 * t * pmx;
    sign * num / den
}

pub fn main(_args: Vec<String>) -> isize {
    println!("=== Audio Test: 440 Hz Sine Wave ===");

    let mixer = match audio_mixer::get_mixer() {
        Some(m) => m,
        None => {
            println!("ERROR: audio mixer not initialized (no HDA device?)");
            return -1;
        }
    };

    let sample_rate = audio_mixer::SAMPLE_RATE as f64;
    let freq = 440.0f64;
    let duration_secs = 2;
    let total_frames = audio_mixer::SAMPLE_RATE as usize * duration_secs;
    let amplitude = 24_000i16; // ~75% of max to avoid clipping

    // Write in chunks of 1024 frames (4096 bytes).
    let chunk_frames = 1024usize;
    let mut buf = [0i16; 1024 * 2]; // stereo
    let mut frames_written = 0usize;

    println!("Playing {} Hz for {} seconds ({} frames)...", freq as u32, duration_secs, total_frames);

    while frames_written < total_frames {
        let remaining = total_frames - frames_written;
        let this_chunk = chunk_frames.min(remaining);

        for i in 0..this_chunk {
            let t = (frames_written + i) as f64 / sample_rate;
            let sample = (fast_sin(t * freq * 2.0 * core::f64::consts::PI) * amplitude as f64) as i16;
            buf[i * 2] = sample;     // left
            buf[i * 2 + 1] = sample; // right
        }

        let data = unsafe {
            core::slice::from_raw_parts(buf.as_ptr() as *const u8, this_chunk * 4)
        };

        // Write to mixer; if buffer is full, yield and retry.
        let mut offset = 0;
        while offset < data.len() {
            let written = mixer.lock().write_pcm(&data[offset..]);
            offset += written;
            if offset < data.len() {
                // Buffer full — wait a bit for the driver to drain.
                let _ = sleep::sleep(core::time::Duration::from_millis(10));
            }
        }

        frames_written += this_chunk;
    }

    // Wait for the mixer to drain.
    for _ in 0..200 {
        if mixer.lock().available_frames() == 0 {
            break;
        }
        let _ = sleep::sleep(core::time::Duration::from_millis(10));
    }

    println!("Audio test complete.");
    0
}
