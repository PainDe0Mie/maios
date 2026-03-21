//! HDA codec discovery and verb communication.
//!
//! Implements CORB/RIRB verb send/receive and simplified codec enumeration
//! targeting QEMU's `hda-output` codec topology.

use crate::regs::{self, HdaRegisters};
use log::{info, warn};

// ─── Verb encoding ──────────────────────────────────────────────────────────

/// Encode a 12-bit verb (4-bit payload) addressed to a codec/node.
///
/// Format: `[codec(4) | NID(8) | verb_id(12) | payload(8)]` = 32 bits.
fn make_verb_short(codec: u8, nid: u8, verb: u16, payload: u8) -> u32 {
    ((codec as u32) << 28)
        | ((nid as u32) << 20)
        | ((verb as u32 & 0xFFF) << 8)
        | (payload as u32)
}

/// Encode a 4-bit verb (16-bit payload).
///
/// Format: `[codec(4) | NID(8) | verb_id(4) | payload(16)]` = 32 bits.
fn make_verb_long(codec: u8, nid: u8, verb: u8, payload: u16) -> u32 {
    ((codec as u32) << 28)
        | ((nid as u32) << 20)
        | ((verb as u32 & 0xF) << 16)
        | (payload as u32)
}

// ─── Verb constants ─────────────────────────────────────────────────────────

// GET verbs (12-bit, short form)
const GET_PARAMETER:           u16 = 0xF00;
const GET_CONN_LIST:           u16 = 0xF02;
const GET_PIN_WIDGET_CONTROL:  u16 = 0xF07;

// SET verbs (12-bit, short form)
const SET_PIN_WIDGET_CONTROL:  u16 = 0x707;
const SET_POWER_STATE:         u16 = 0x705;
const SET_STREAM_CHANNEL:      u16 = 0x706;
const SET_EAPD_ENABLE:         u16 = 0x70C;

// SET verbs (4-bit, long form)
const SET_CONVERTER_FORMAT:    u8 = 0x2;
const SET_AMP_GAIN_MUTE:       u8 = 0x3;

// Parameter IDs for GET_PARAMETER
const PARAM_VENDOR_ID:         u8 = 0x00;
const PARAM_NODE_COUNT:        u8 = 0x04;
const PARAM_AUDIO_WIDGET_CAP:  u8 = 0x09;
const PARAM_CONN_LIST_LEN:     u8 = 0x0E;

// Widget types (from Audio Widget Capabilities, bits 23:20)
const WIDGET_AUDIO_OUTPUT:     u8 = 0x0;
const WIDGET_AUDIO_INPUT:      u8 = 0x1;
const WIDGET_AUDIO_MIXER:      u8 = 0x2;
const WIDGET_AUDIO_SELECTOR:   u8 = 0x3;
const WIDGET_PIN_COMPLEX:      u8 = 0x4;

/// CORB/RIRB state for verb communication.
pub struct VerbState {
    /// Virtual address of CORB buffer (256 entries of u32).
    pub corb_va: usize,
    /// Virtual address of RIRB buffer (256 entries of u64).
    pub rirb_va: usize,
    /// Next CORB write index.
    pub corb_wp: u16,
    /// Next RIRB read index.
    pub rirb_rp: u16,
}

impl VerbState {
    /// Send a verb via CORB and poll RIRB for the response.
    ///
    /// This is a blocking operation with a timeout.
    pub fn send_verb(&mut self, regs: &mut HdaRegisters, verb: u32) -> Result<u32, &'static str> {
        // Advance write pointer.
        self.corb_wp = (self.corb_wp + 1) % 256;

        // Write verb to CORB entry.
        let corb_entry = (self.corb_va + self.corb_wp as usize * 4) as *mut u32;
        unsafe { corb_entry.write_volatile(verb); }

        // Tell the controller.
        regs.corbwp.write(self.corb_wp);

        // Poll RIRB for a response (timeout after ~100k iterations).
        for _ in 0..100_000 {
            let hw_wp = regs.rirbwp.read();
            if hw_wp != self.rirb_rp {
                self.rirb_rp = (self.rirb_rp + 1) % 256;
                // Each RIRB entry is 8 bytes: [response(4), solicited+codec(4)]
                let rirb_entry = (self.rirb_va + self.rirb_rp as usize * 8) as *const u32;
                let response = unsafe { rirb_entry.read_volatile() };
                return Ok(response);
            }
            core::hint::spin_loop();
        }

        Err("HDA: verb timeout waiting for RIRB response")
    }

    /// Send GET_PARAMETER verb.
    fn get_param(&mut self, regs: &mut HdaRegisters, codec: u8, nid: u8, param: u8) -> Result<u32, &'static str> {
        let verb = make_verb_short(codec, nid, GET_PARAMETER, param);
        self.send_verb(regs, verb)
    }
}

/// Output path: the DAC node ID and the output pin node ID.
pub struct OutputPath {
    pub dac_nid: u8,
    pub pin_nid: u8,
}

/// Discover the first output path in a codec.
///
/// Walks the widget tree to find a Pin Complex configured as a line-out
/// or headphone, then traces its connection to a DAC (Audio Output widget).
pub fn find_output_path(
    verb: &mut VerbState,
    regs: &mut HdaRegisters,
    codec: u8,
) -> Result<OutputPath, &'static str> {
    // Read vendor ID.
    let vendor = verb.get_param(regs, codec, 0, PARAM_VENDOR_ID)?;
    info!("HDA codec {}: vendor={:#010x}", codec, vendor);

    // Get subordinate node count from root (NID 0).
    let node_count = verb.get_param(regs, codec, 0, PARAM_NODE_COUNT)?;
    let start_nid = ((node_count >> 16) & 0xFF) as u8;
    let num_nodes = (node_count & 0xFF) as u8;
    info!("HDA codec {}: root has {} sub-nodes starting at NID {}", codec, num_nodes, start_nid);

    if num_nodes == 0 {
        return Err("HDA: no sub-nodes in root");
    }

    // The first sub-node is typically the Audio Function Group (AFG).
    let afg_nid = start_nid;

    // Power up the AFG.
    let power_verb = make_verb_short(codec, afg_nid, SET_POWER_STATE, 0x00); // D0
    let _ = verb.send_verb(regs, power_verb);

    // Get AFG's child widgets.
    let afg_count = verb.get_param(regs, codec, afg_nid, PARAM_NODE_COUNT)?;
    let widget_start = ((afg_count >> 16) & 0xFF) as u8;
    let widget_num = (afg_count & 0xFF) as u8;
    info!("HDA AFG NID {}: {} widgets starting at NID {}", afg_nid, widget_num, widget_start);

    // Scan widgets to find DACs and output pins.
    let mut dac_nid: Option<u8> = None;
    let mut pin_nid: Option<u8> = None;

    for i in 0..widget_num {
        let nid = widget_start + i;
        let cap = verb.get_param(regs, codec, nid, PARAM_AUDIO_WIDGET_CAP)?;
        let widget_type = ((cap >> 20) & 0xF) as u8;

        match widget_type {
            WIDGET_AUDIO_OUTPUT => {
                if dac_nid.is_none() {
                    info!("HDA: found DAC at NID {}", nid);
                    dac_nid = Some(nid);
                }
            }
            WIDGET_PIN_COMPLEX => {
                // Accept the first pin as output.
                if pin_nid.is_none() {
                    info!("HDA: found pin at NID {}", nid);
                    pin_nid = Some(nid);
                }
            }
            _ => {}
        }
    }

    let dac = dac_nid.ok_or("HDA: no DAC found")?;
    let pin = pin_nid.ok_or("HDA: no output pin found")?;

    Ok(OutputPath { dac_nid: dac, pin_nid: pin })
}

/// Configure the output path for PCM playback.
///
/// - Sets the DAC's stream/channel assignment.
/// - Sets the DAC's converter format to 48kHz/16-bit/stereo.
/// - Enables the output pin.
/// - Unmutes amplifiers.
pub fn configure_output(
    verb: &mut VerbState,
    regs: &mut HdaRegisters,
    codec: u8,
    path: &OutputPath,
    stream_id: u8,
) -> Result<(), &'static str> {
    // Assign stream 1 (stream_id), channel 0 to the DAC.
    let stream_chan = make_verb_short(codec, path.dac_nid, SET_STREAM_CHANNEL,
        (stream_id << 4) | 0x00);
    verb.send_verb(regs, stream_chan)?;

    // Set converter format: 48kHz, 16-bit, stereo.
    let format = make_verb_long(codec, path.dac_nid, SET_CONVERTER_FORMAT,
        regs::FMT_48KHZ_16BIT_STEREO);
    verb.send_verb(regs, format)?;

    // Power up the DAC.
    let power = make_verb_short(codec, path.dac_nid, SET_POWER_STATE, 0x00);
    verb.send_verb(regs, power)?;

    // Enable the output pin (OUT_ENABLE = 0x40).
    let pin_ctl = make_verb_short(codec, path.pin_nid, SET_PIN_WIDGET_CONTROL, 0xC0);
    verb.send_verb(regs, pin_ctl)?;

    // Power up the pin.
    let pin_power = make_verb_short(codec, path.pin_nid, SET_POWER_STATE, 0x00);
    verb.send_verb(regs, pin_power)?;

    // Unmute output amplifier on the DAC (set gain, output, left+right).
    // Bit 15 = output, bit 13 = left, bit 12 = right, bits 6:0 = gain (max)
    let amp = make_verb_long(codec, path.dac_nid, SET_AMP_GAIN_MUTE,
        0xB000 | 0x7F); // output + left + right + gain=127
    verb.send_verb(regs, amp)?;

    // Try EAPD on the pin (some codecs need it).
    let eapd = make_verb_short(codec, path.pin_nid, SET_EAPD_ENABLE, 0x02);
    let _ = verb.send_verb(regs, eapd); // Ignore error; not all pins have EAPD.

    info!("HDA: output configured — DAC NID {}, pin NID {}, stream {}",
        path.dac_nid, path.pin_nid, stream_id);

    Ok(())
}
