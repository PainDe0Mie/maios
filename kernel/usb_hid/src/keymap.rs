//! USB HID Usage ID to keycodes_ascii::Keycode translation.
//!
//! Maps USB HID keyboard usage IDs (page 0x07) to the PS/2 scan code set 1
//! values used by `keycodes_ascii::Keycode`.
//!
//! Also provides translation from the USB HID modifier bitfield (byte 0 of a
//! boot protocol keyboard report) to `keycodes_ascii::KeyboardModifiers`.

use keycodes_ascii::{Keycode, KeyboardModifiers};

/// Convert a USB HID keyboard usage ID (page 0x07) to the corresponding
/// `keycodes_ascii::Keycode`.
///
/// Returns `None` for unmapped or reserved usage IDs.
pub fn hid_usage_to_keycode(usage: u8) -> Option<Keycode> {
    match usage {
        // Letters: HID 0x04..=0x1D  ->  A(30)..Z(44+)
        // HID is alphabetical (A=0x04, B=0x05, ..., Z=0x1D).
        // PS/2 set 1 keycodes follow QWERTY physical layout, not alphabetical.
        0x04 => Some(Keycode::A),         // 30
        0x05 => Some(Keycode::B),         // 48
        0x06 => Some(Keycode::C),         // 46
        0x07 => Some(Keycode::D),         // 32
        0x08 => Some(Keycode::E),         // 18
        0x09 => Some(Keycode::F),         // 33
        0x0A => Some(Keycode::G),         // 34
        0x0B => Some(Keycode::H),         // 35
        0x0C => Some(Keycode::I),         // 23
        0x0D => Some(Keycode::J),         // 36
        0x0E => Some(Keycode::K),         // 37
        0x0F => Some(Keycode::L),         // 38
        0x10 => Some(Keycode::M),         // 50
        0x11 => Some(Keycode::N),         // 49
        0x12 => Some(Keycode::O),         // 24
        0x13 => Some(Keycode::P),         // 25
        0x14 => Some(Keycode::Q),         // 16
        0x15 => Some(Keycode::R),         // 19
        0x16 => Some(Keycode::S),         // 31
        0x17 => Some(Keycode::T),         // 20
        0x18 => Some(Keycode::U),         // 22
        0x19 => Some(Keycode::V),         // 47
        0x1A => Some(Keycode::W),         // 17
        0x1B => Some(Keycode::X),         // 45
        0x1C => Some(Keycode::Y),         // 21
        0x1D => Some(Keycode::Z),         // 44

        // Number row: HID 0x1E..=0x27  ->  1(2)..0(11)
        0x1E => Some(Keycode::Num1),      // 2
        0x1F => Some(Keycode::Num2),      // 3
        0x20 => Some(Keycode::Num3),      // 4
        0x21 => Some(Keycode::Num4),      // 5
        0x22 => Some(Keycode::Num5),      // 6
        0x23 => Some(Keycode::Num6),      // 7
        0x24 => Some(Keycode::Num7),      // 8
        0x25 => Some(Keycode::Num8),      // 9
        0x26 => Some(Keycode::Num9),      // 10
        0x27 => Some(Keycode::Num0),      // 11

        // Special keys
        0x28 => Some(Keycode::Enter),     // 28
        0x29 => Some(Keycode::Escape),    // 1
        0x2A => Some(Keycode::Backspace), // 14
        0x2B => Some(Keycode::Tab),       // 15
        0x2C => Some(Keycode::Space),     // 57

        // Symbols
        0x2D => Some(Keycode::Minus),        // 12
        0x2E => Some(Keycode::Equals),       // 13
        0x2F => Some(Keycode::LeftBracket),  // 26
        0x30 => Some(Keycode::RightBracket), // 27
        0x31 => Some(Keycode::Backslash),    // 43
        // 0x32 = Non-US # and ~ (skip)
        0x33 => Some(Keycode::Semicolon),    // 39
        0x34 => Some(Keycode::Quote),        // 40
        0x35 => Some(Keycode::Backtick),     // 41
        0x36 => Some(Keycode::Comma),        // 51
        0x37 => Some(Keycode::Period),       // 52
        0x38 => Some(Keycode::Slash),        // 53

        // Lock keys
        0x39 => Some(Keycode::CapsLock),     // 58

        // Function keys: HID 0x3A..=0x45  ->  F1(59)..F12(70)
        0x3A => Some(Keycode::F1),        // 59
        0x3B => Some(Keycode::F2),        // 60
        0x3C => Some(Keycode::F3),        // 61
        0x3D => Some(Keycode::F4),        // 62
        0x3E => Some(Keycode::F5),        // 63
        0x3F => Some(Keycode::F6),        // 64
        0x40 => Some(Keycode::F7),        // 65
        0x41 => Some(Keycode::F8),        // 66
        0x42 => Some(Keycode::F9),        // 67
        0x43 => Some(Keycode::F10),       // 68
        0x44 => Some(Keycode::F11),       // 88
        0x45 => Some(Keycode::F12),       // 89

        // Navigation / editing keys
        0x46 => Some(Keycode::PadMultiply),  // PrintScreen shares this scancode
        0x47 => Some(Keycode::ScrollLock),   // 70
        0x48 => Some(Keycode::Pause),        // 90
        0x49 => Some(Keycode::Insert),       // 83
        0x4A => Some(Keycode::Home),         // 72
        0x4B => Some(Keycode::PageUp),       // 73
        0x4C => Some(Keycode::Delete),       // 84
        0x4D => Some(Keycode::End),          // 80
        0x4E => Some(Keycode::PageDown),     // 82
        0x4F => Some(Keycode::Right),        // 77
        0x50 => Some(Keycode::Left),         // 76
        0x51 => Some(Keycode::Down),         // 81
        0x52 => Some(Keycode::Up),           // 73

        // Numpad
        0x53 => Some(Keycode::NumLock),      // 69
        0x54 => Some(Keycode::Slash),        // Pad Divide (reuse Slash)
        0x55 => Some(Keycode::PadMultiply),  // 55
        0x56 => Some(Keycode::PadMinus),     // 74
        0x57 => Some(Keycode::PadPlus),      // 78
        // 0x58 = Keypad Enter (reuse Enter)
        0x58 => Some(Keycode::Enter),

        // Modifier keycodes (left side)
        0xE0 => Some(Keycode::Control),      // Left Control
        0xE1 => Some(Keycode::LeftShift),    // Left Shift
        0xE2 => Some(Keycode::Alt),          // Left Alt
        0xE3 => Some(Keycode::SuperKeyLeft), // Left GUI
        // Modifier keycodes (right side)
        0xE4 => Some(Keycode::Control),      // Right Control
        0xE5 => Some(Keycode::RightShift),   // Right Shift
        0xE6 => Some(Keycode::Alt),          // Right Alt (AltGr)
        0xE7 => Some(Keycode::SuperKeyRight),// Right GUI

        // Non-US backslash
        0x64 => Some(Keycode::NonUsBackslash),

        // Menu / Application key
        0x65 => Some(Keycode::Menu),

        _ => None,
    }
}

/// Convert the USB HID boot protocol modifier byte (byte 0 of the report)
/// to `keycodes_ascii::KeyboardModifiers`.
///
/// Modifier byte bit layout:
/// - bit 0: Left Control
/// - bit 1: Left Shift
/// - bit 2: Left Alt
/// - bit 3: Left GUI (Super)
/// - bit 4: Right Control
/// - bit 5: Right Shift
/// - bit 6: Right Alt (AltGr)
/// - bit 7: Right GUI (Super)
pub fn hid_modifiers_to_keyboard_modifiers(mods: u8) -> KeyboardModifiers {
    let mut result = KeyboardModifiers::empty();

    if mods & (1 << 0) != 0 {
        result |= KeyboardModifiers::CONTROL_LEFT;
    }
    if mods & (1 << 1) != 0 {
        result |= KeyboardModifiers::SHIFT_LEFT;
    }
    if mods & (1 << 2) != 0 {
        result |= KeyboardModifiers::ALT;
    }
    if mods & (1 << 3) != 0 {
        result |= KeyboardModifiers::SUPER_KEY_LEFT;
    }
    if mods & (1 << 4) != 0 {
        result |= KeyboardModifiers::CONTROL_RIGHT;
    }
    if mods & (1 << 5) != 0 {
        result |= KeyboardModifiers::SHIFT_RIGHT;
    }
    if mods & (1 << 6) != 0 {
        result |= KeyboardModifiers::ALT_GR;
    }
    if mods & (1 << 7) != 0 {
        result |= KeyboardModifiers::SUPER_KEY_RIGHT;
    }

    result
}
