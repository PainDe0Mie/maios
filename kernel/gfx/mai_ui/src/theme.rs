//! Tokyo Night theme colors and layout constants for MaiOS.
//!
//! All MaiOS applications should use these colors for a consistent look.

use color::Color;

// ═══════════════════════════════════════════════════════════════
// Base colors
// ═══════════════════════════════════════════════════════════════

/// Main background — deep dark blue
pub const C_BG: Color = Color::new(0x001A1B26);
/// Panel/card background — slightly lighter
pub const C_PANEL: Color = Color::new(0x0024283A);
/// Header/toolbar background
pub const C_HEADER: Color = Color::new(0x00292E42);
/// Alternating row background
pub const C_ROW_ALT: Color = Color::new(0x001E2030);
/// Subtle background for inputs
pub const C_INPUT_BG: Color = Color::new(0x00111115);

// ═══════════════════════════════════════════════════════════════
// Border & separator
// ═══════════════════════════════════════════════════════════════

/// Default border color
pub const C_BORDER: Color = Color::new(0x00414868);
/// Subtle separator
pub const C_SEPARATOR: Color = Color::new(0x00252535);
/// Focused element border
pub const C_FOCUS: Color = Color::new(0x007AA2F7);

// ═══════════════════════════════════════════════════════════════
// Accent & semantic colors
// ═══════════════════════════════════════════════════════════════

/// Primary accent — bright blue
pub const C_ACCENT: Color = Color::new(0x007AA2F7);
/// Success — green
pub const C_GREEN: Color = Color::new(0x009ECE6A);
/// Warning — yellow/orange
pub const C_YELLOW: Color = Color::new(0x00E0AF68);
/// Error/danger — red/pink
pub const C_RED: Color = Color::new(0x00F7768E);
/// Info/secondary — purple
pub const C_PURPLE: Color = Color::new(0x00BB9AF7);
/// Highlight — cyan
pub const C_CYAN: Color = Color::new(0x007DCFFF);
/// Orange accent
pub const C_ORANGE: Color = Color::new(0x00FF9E64);

// ═══════════════════════════════════════════════════════════════
// Text colors
// ═══════════════════════════════════════════════════════════════

/// Primary foreground text
pub const C_FG: Color = Color::new(0x00C0CAF5);
/// Dimmed/secondary text
pub const C_FG_DIM: Color = Color::new(0x00565F89);
/// Disabled text
pub const C_FG_DISABLED: Color = Color::new(0x003B4261);

// ═══════════════════════════════════════════════════════════════
// Button states
// ═══════════════════════════════════════════════════════════════

/// Button background (normal)
pub const C_BTN: Color = Color::new(0x00343A52);
/// Button background (hovered)
pub const C_BTN_HOVER: Color = Color::new(0x00414868);
/// Button background (pressed)
pub const C_BTN_PRESSED: Color = Color::new(0x007AA2F7);
/// Button text
pub const C_BTN_FG: Color = Color::new(0x00C0CAF5);

// ═══════════════════════════════════════════════════════════════
// Layout constants
// ═══════════════════════════════════════════════════════════════

/// Default padding inside panels
pub const PADDING: isize = 12;
/// Small padding
pub const PADDING_SM: isize = 6;
/// Large padding
pub const PADDING_LG: isize = 20;
/// Default spacing between elements
pub const SPACING: isize = 8;
/// Small spacing
pub const SPACING_SM: isize = 4;
/// Row height for list items
pub const ROW_HEIGHT: usize = 18;
/// Header bar height
pub const HEADER_HEIGHT: usize = 28;
/// Default progress bar height
pub const BAR_HEIGHT: usize = 14;
/// Default button height
pub const BUTTON_HEIGHT: usize = 24;
/// Default text input height
pub const INPUT_HEIGHT: usize = 22;

/// Character dimensions (from bitmap font)
pub const CHAR_W: usize = font::CHARACTER_WIDTH;
pub const CHAR_H: usize = font::CHARACTER_HEIGHT;
