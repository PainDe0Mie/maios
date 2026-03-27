//! Gamepad/joystick data types — XInput-style layout.
//!
//! Pure data crate, no kernel dependencies. Mirrors `mouse_data`.

#![no_std]

/// Analog stick axes and triggers.
///
/// Stick values are in [-32768, 32767] (XInput convention).
/// Trigger values are in [0, 255].
#[derive(Debug, Clone, Copy, Default)]
pub struct GamepadAxes {
    pub left_stick_x: i16,
    pub left_stick_y: i16,
    pub right_stick_x: i16,
    pub right_stick_y: i16,
    pub left_trigger: u8,
    pub right_trigger: u8,
}

/// Button state as a bitfield (XInput layout).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GamepadButtons {
    bits: u16,
}

impl GamepadButtons {
    pub const A: u16          = 1 << 0;
    pub const B: u16          = 1 << 1;
    pub const X: u16          = 1 << 2;
    pub const Y: u16          = 1 << 3;
    pub const LB: u16         = 1 << 4;
    pub const RB: u16         = 1 << 5;
    pub const BACK: u16       = 1 << 6;
    pub const START: u16      = 1 << 7;
    pub const L_THUMB: u16    = 1 << 8;
    pub const R_THUMB: u16    = 1 << 9;
    pub const DPAD_UP: u16    = 1 << 10;
    pub const DPAD_DOWN: u16  = 1 << 11;
    pub const DPAD_LEFT: u16  = 1 << 12;
    pub const DPAD_RIGHT: u16 = 1 << 13;
    pub const GUIDE: u16      = 1 << 14;

    pub fn pressed(&self, mask: u16) -> bool { self.bits & mask != 0 }
    pub fn any_pressed(&self) -> bool { self.bits != 0 }
    pub fn raw(&self) -> u16 { self.bits }
    pub fn from_raw(bits: u16) -> Self { Self { bits } }
}

/// Full gamepad state snapshot.
#[derive(Debug, Clone, Default)]
pub struct GamepadState {
    pub gamepad_id: u8,
    pub buttons: GamepadButtons,
    pub axes: GamepadAxes,
}

impl GamepadState {
    pub fn new(gamepad_id: u8) -> Self {
        Self { gamepad_id, ..Default::default() }
    }
}
