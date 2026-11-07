//! Reusable UI widgets for MaiOS applications.
//!
//! All widgets follow the immediate-mode pattern: they draw directly
//! to a `DrawContext` at the specified position.

extern crate alloc;

use crate::draw::DrawContext;
use crate::theme;
use color::Color;

// ═══════════════════════════════════════════════════════════════
// Label
// ═══════════════════════════════════════════════════════════════

/// A simple text label.
pub struct Label<'a> {
    text: &'a str,
    color: Color,
}

impl<'a> Label<'a> {
    pub fn new(text: &'a str) -> Self {
        Self { text, color: theme::C_FG }
    }

    pub fn color(mut self, c: Color) -> Self {
        self.color = c;
        self
    }

    /// Draw at (x, y). Returns the height consumed.
    pub fn draw(&self, ctx: &mut DrawContext, x: isize, y: isize) -> usize {
        ctx.text(x, y, self.text, self.color);
        theme::CHAR_H
    }

    /// Draw centered within a given width.
    pub fn draw_centered(&self, ctx: &mut DrawContext, x: isize, y: isize, w: usize) -> usize {
        ctx.text_centered(x, y, w, self.text, self.color);
        theme::CHAR_H
    }
}

// ═══════════════════════════════════════════════════════════════
// Header
// ═══════════════════════════════════════════════════════════════

/// A title bar / header strip with text and optional right-side info.
pub struct Header<'a> {
    title: &'a str,
    right_text: Option<&'a str>,
    bg: Color,
    fg: Color,
    accent: Color,
    height: usize,
}

impl<'a> Header<'a> {
    pub fn new(title: &'a str) -> Self {
        Self {
            title,
            right_text: None,
            bg: theme::C_PANEL,
            fg: theme::C_ACCENT,
            accent: theme::C_ACCENT,
            height: theme::HEADER_HEIGHT,
        }
    }

    pub fn right_text(mut self, text: &'a str) -> Self {
        self.right_text = Some(text);
        self
    }

    pub fn bg(mut self, c: Color) -> Self { self.bg = c; self }
    pub fn fg(mut self, c: Color) -> Self { self.fg = c; self }
    pub fn height(mut self, h: usize) -> Self { self.height = h; self }

    /// Draw at (x, y) spanning full `width`. Returns height consumed.
    pub fn draw(&self, ctx: &mut DrawContext, x: isize, y: isize, width: usize) -> usize {
        ctx.fill_rect(x, y, width, self.height, self.bg);
        ctx.hline(x, x + width as isize, y + self.height as isize - 1, self.accent);
        ctx.text(x + theme::PADDING, y + (self.height as isize - theme::CHAR_H as isize) / 2, self.title, self.fg);
        if let Some(rt) = self.right_text {
            ctx.text_right(x + width as isize - theme::PADDING, y + (self.height as isize - theme::CHAR_H as isize) / 2, rt, theme::C_FG_DIM);
        }
        self.height
    }
}

// ═══════════════════════════════════════════════════════════════
// Button
// ═══════════════════════════════════════════════════════════════

/// A clickable button.
#[derive(Clone, Copy, PartialEq)]
pub enum ButtonState {
    Normal,
    Hovered,
    Pressed,
}

pub struct Button<'a> {
    text: &'a str,
    state: ButtonState,
    width: Option<usize>,
}

impl<'a> Button<'a> {
    pub fn new(text: &'a str) -> Self {
        Self { text, state: ButtonState::Normal, width: None }
    }

    pub fn state(mut self, s: ButtonState) -> Self {
        self.state = s;
        self
    }

    pub fn width(mut self, w: usize) -> Self {
        self.width = Some(w);
        self
    }

    /// Draw at (x, y). Returns (width, height) of the button.
    pub fn draw(&self, ctx: &mut DrawContext, x: isize, y: isize) -> (usize, usize) {
        let w = self.width.unwrap_or(self.text.len() * theme::CHAR_W + 16);
        let h = theme::BUTTON_HEIGHT;
        let (bg, fg) = match self.state {
            ButtonState::Normal  => (theme::C_BTN, theme::C_BTN_FG),
            ButtonState::Hovered => (theme::C_BTN_HOVER, theme::C_BTN_FG),
            ButtonState::Pressed => (theme::C_BTN_PRESSED, theme::C_BG),
        };
        ctx.rounded_rect(x, y, w, h, 3, bg);
        ctx.text_centered(x, y + (h as isize - theme::CHAR_H as isize) / 2, w, self.text, fg);
        (w, h)
    }

    /// Check if a point (px, py) is within this button drawn at (x, y).
    pub fn hit_test(&self, x: isize, y: isize, px: isize, py: isize) -> bool {
        let w = self.width.unwrap_or(self.text.len() * theme::CHAR_W + 16) as isize;
        let h = theme::BUTTON_HEIGHT as isize;
        px >= x && px < x + w && py >= y && py < y + h
    }
}

// ═══════════════════════════════════════════════════════════════
// ProgressBar
// ═══════════════════════════════════════════════════════════════

/// A horizontal progress bar (0–100%).
pub struct ProgressBar {
    percent: usize,
    color: Color,
    height: usize,
    show_text: bool,
}

impl ProgressBar {
    pub fn new(percent: usize) -> Self {
        Self {
            percent: percent.min(100),
            color: theme::C_ACCENT,
            height: theme::BAR_HEIGHT,
            show_text: false,
        }
    }

    pub fn color(mut self, c: Color) -> Self { self.color = c; self }
    pub fn height(mut self, h: usize) -> Self { self.height = h; self }
    pub fn show_text(mut self, show: bool) -> Self { self.show_text = show; self }

    /// Draw at (x, y) with given width. Returns height consumed.
    pub fn draw(&self, ctx: &mut DrawContext, x: isize, y: isize, width: usize) -> usize {
        // Background
        ctx.fill_rect(x, y, width, self.height, theme::C_INPUT_BG);
        // Filled portion
        let filled = (width * self.percent) / 100;
        if filled > 0 {
            ctx.fill_rect(x, y, filled, self.height, self.color);
        }
        // Border
        ctx.border_rect(x, y, width, self.height, theme::C_BORDER);
        // Optional percentage text
        if self.show_text {
            use alloc::format;
            let text = format!("{}%", self.percent);
            ctx.text_centered(x, y + (self.height as isize - theme::CHAR_H as isize) / 2,
                            width, &text, theme::C_FG);
        }
        self.height
    }
}

// ═══════════════════════════════════════════════════════════════
// Panel
// ═══════════════════════════════════════════════════════════════

/// A panel (card) with background, optional border and padding.
pub struct Panel {
    bg: Color,
    border: Option<Color>,
    padding: isize,
    radius: usize,
}

impl Panel {
    pub fn new() -> Self {
        Self {
            bg: theme::C_PANEL,
            border: Some(theme::C_BORDER),
            padding: theme::PADDING,
            radius: 0,
        }
    }

    pub fn bg(mut self, c: Color) -> Self { self.bg = c; self }
    pub fn border(mut self, c: Option<Color>) -> Self { self.border = c; self }
    pub fn padding(mut self, p: isize) -> Self { self.padding = p; self }
    pub fn radius(mut self, r: usize) -> Self { self.radius = r; self }

    /// Draw the panel background at (x, y) with given size.
    /// Returns a sub-context offset by the panel's padding.
    pub fn draw_bg(&self, ctx: &mut DrawContext, x: isize, y: isize, w: usize, h: usize) {
        if self.radius > 0 {
            ctx.rounded_rect(x, y, w, h, self.radius, self.bg);
        } else {
            ctx.fill_rect(x, y, w, h, self.bg);
        }
        if let Some(bc) = self.border {
            ctx.border_rect(x, y, w, h, bc);
        }
    }

    /// The inset (padding) to apply to content inside this panel.
    pub fn inset(&self) -> isize {
        self.padding
    }
}

// ═══════════════════════════════════════════════════════════════
// Separator
// ═══════════════════════════════════════════════════════════════

/// A horizontal separator line.
pub struct Separator {
    color: Color,
}

impl Separator {
    pub fn new() -> Self {
        Self { color: theme::C_SEPARATOR }
    }

    pub fn color(mut self, c: Color) -> Self { self.color = c; self }

    /// Draw at y across the given width. Returns height (1).
    pub fn draw(&self, ctx: &mut DrawContext, x: isize, y: isize, width: usize) -> usize {
        ctx.hline(x, x + width as isize, y, self.color);
        1
    }
}

// ═══════════════════════════════════════════════════════════════
// ListRow
// ═══════════════════════════════════════════════════════════════

/// A single row in a list, with alternating background and selection support.
pub struct ListRow {
    index: usize,
    selected: bool,
    height: usize,
}

impl ListRow {
    pub fn new(index: usize) -> Self {
        Self { index, selected: false, height: theme::ROW_HEIGHT }
    }

    pub fn selected(mut self, sel: bool) -> Self { self.selected = sel; self }
    pub fn height(mut self, h: usize) -> Self { self.height = h; self }

    /// Draw the row background at (x, y) with given width.
    /// Returns the row height.
    pub fn draw_bg(&self, ctx: &mut DrawContext, x: isize, y: isize, width: usize) -> usize {
        let bg = if self.selected {
            theme::C_ACCENT
        } else if self.index % 2 == 0 {
            theme::C_BG
        } else {
            theme::C_ROW_ALT
        };
        ctx.fill_rect(x, y, width, self.height, bg);
        ctx.hline(x, x + width as isize, y + self.height as isize - 1, theme::C_SEPARATOR);
        self.height
    }
}

// ═══════════════════════════════════════════════════════════════
// ScrollBar
// ═══════════════════════════════════════════════════════════════

/// A vertical scrollbar indicator.
pub struct ScrollBar {
    total_items: usize,
    visible_items: usize,
    scroll_offset: usize,
}

impl ScrollBar {
    pub fn new(total: usize, visible: usize, offset: usize) -> Self {
        Self {
            total_items: total,
            visible_items: visible,
            scroll_offset: offset,
        }
    }

    /// Draw at (x, y) with given height. Returns width (6px).
    pub fn draw(&self, ctx: &mut DrawContext, x: isize, y: isize, height: usize) -> usize {
        if self.total_items <= self.visible_items {
            return 0; // No scrollbar needed
        }
        let sb_w: usize = 6;
        // Track
        ctx.fill_rect(x, y, sb_w, height, theme::C_INPUT_BG);
        // Thumb
        let thumb_h = ((self.visible_items * height) / self.total_items).max(20);
        let thumb_y = if self.total_items > 0 {
            (self.scroll_offset * height) / self.total_items
        } else {
            0
        };
        ctx.fill_rect(x, y + thumb_y as isize, sb_w, thumb_h, theme::C_BORDER);
        sb_w
    }
}

// ═══════════════════════════════════════════════════════════════
// TextInput (display only — cursor + text)
// ═══════════════════════════════════════════════════════════════

/// A simple text input field (visual only — state management is up to the app).
pub struct TextInput<'a> {
    text: &'a str,
    cursor_pos: usize,
    focused: bool,
    placeholder: Option<&'a str>,
}

impl<'a> TextInput<'a> {
    pub fn new(text: &'a str) -> Self {
        Self {
            text,
            cursor_pos: text.len(),
            focused: false,
            placeholder: None,
        }
    }

    pub fn cursor(mut self, pos: usize) -> Self { self.cursor_pos = pos; self }
    pub fn focused(mut self, f: bool) -> Self { self.focused = f; self }
    pub fn placeholder(mut self, p: &'a str) -> Self { self.placeholder = Some(p); self }

    /// Draw at (x, y) with given width. Returns height consumed.
    pub fn draw(&self, ctx: &mut DrawContext, x: isize, y: isize, width: usize) -> usize {
        let h = theme::INPUT_HEIGHT;
        let border = if self.focused { theme::C_FOCUS } else { theme::C_BORDER };

        ctx.fill_rect(x, y, width, h, theme::C_INPUT_BG);
        ctx.border_rect(x, y, width, h, border);

        let text_y = y + (h as isize - theme::CHAR_H as isize) / 2;
        let text_x = x + 4;

        if self.text.is_empty() {
            if let Some(ph) = self.placeholder {
                ctx.text(text_x, text_y, ph, theme::C_FG_DISABLED);
            }
        } else {
            let max_chars = (width - 8) / theme::CHAR_W;
            ctx.text_clipped(text_x, text_y, self.text, max_chars, theme::C_FG);
        }

        // Cursor
        if self.focused {
            let cursor_x = text_x + self.cursor_pos as isize * theme::CHAR_W as isize;
            ctx.vline(cursor_x, y + 2, y + h as isize - 2, theme::C_ACCENT);
        }

        h
    }
}

// ═══════════════════════════════════════════════════════════════
// Badge / Tag
// ═══════════════════════════════════════════════════════════════

/// A small colored badge/tag for status indicators.
pub struct Badge<'a> {
    text: &'a str,
    bg: Color,
    fg: Color,
}

impl<'a> Badge<'a> {
    pub fn new(text: &'a str) -> Self {
        Self { text, bg: theme::C_ACCENT, fg: theme::C_BG }
    }

    pub fn success(text: &'a str) -> Self {
        Self { text, bg: theme::C_GREEN, fg: theme::C_BG }
    }

    pub fn warning(text: &'a str) -> Self {
        Self { text, bg: theme::C_YELLOW, fg: theme::C_BG }
    }

    pub fn error(text: &'a str) -> Self {
        Self { text, bg: theme::C_RED, fg: theme::C_BG }
    }

    pub fn bg(mut self, c: Color) -> Self { self.bg = c; self }
    pub fn fg(mut self, c: Color) -> Self { self.fg = c; self }

    /// Draw at (x, y). Returns (width, height).
    pub fn draw(&self, ctx: &mut DrawContext, x: isize, y: isize) -> (usize, usize) {
        let w = self.text.len() * theme::CHAR_W + 8;
        let h = theme::CHAR_H + 4;
        ctx.rounded_rect(x, y, w, h, 2, self.bg);
        ctx.text(x + 4, y + 2, self.text, self.fg);
        (w, h)
    }
}

// ═══════════════════════════════════════════════════════════════
// Table header
// ═══════════════════════════════════════════════════════════════

/// A table column header strip.
pub struct TableHeader<'a> {
    columns: &'a [(&'a str, isize)],
    bg: Color,
    fg: Color,
}

impl<'a> TableHeader<'a> {
    /// Create with column definitions: `(name, x_offset)` pairs.
    pub fn new(columns: &'a [(&'a str, isize)]) -> Self {
        Self {
            columns,
            bg: theme::C_HEADER,
            fg: theme::C_FG_DIM,
        }
    }

    /// Draw at (y) full width. Returns height consumed.
    pub fn draw(&self, ctx: &mut DrawContext, y: isize, width: usize) -> usize {
        let h = theme::CHAR_H + 8;
        ctx.fill_rect(0, y, width, h, self.bg);
        ctx.hline(0, width as isize, y + h as isize - 1, theme::C_ACCENT);
        for &(name, x_off) in self.columns {
            ctx.text(x_off, y + 4, name, self.fg);
        }
        h
    }
}

// ═══════════════════════════════════════════════════════════════
// Footer / StatusBar
// ═══════════════════════════════════════════════════════════════

/// A status bar / footer with key hints.
pub struct StatusBar<'a> {
    text: &'a str,
}

impl<'a> StatusBar<'a> {
    pub fn new(text: &'a str) -> Self {
        Self { text }
    }

    /// Draw at the bottom of the given area. Returns height consumed.
    pub fn draw(&self, ctx: &mut DrawContext, y: isize, width: usize) -> usize {
        let h = theme::ROW_HEIGHT + 2;
        ctx.fill_rect(0, y, width, h, theme::C_PANEL);
        ctx.hline(0, width as isize, y, theme::C_BORDER);
        ctx.text(theme::PADDING, y + 2, self.text, theme::C_FG_DIM);
        h
    }
}

// ═══════════════════════════════════════════════════════════════
// Checkbox
// ═══════════════════════════════════════════════════════════════

/// A simple checkbox.
pub struct Checkbox<'a> {
    label: &'a str,
    checked: bool,
}

impl<'a> Checkbox<'a> {
    pub fn new(label: &'a str, checked: bool) -> Self {
        Self { label, checked }
    }

    /// Draw at (x, y). Returns (width, height).
    pub fn draw(&self, ctx: &mut DrawContext, x: isize, y: isize) -> (usize, usize) {
        let box_size: usize = 12;
        let h = box_size.max(theme::CHAR_H);

        // Box
        ctx.fill_rect(x, y + 2, box_size, box_size, theme::C_INPUT_BG);
        ctx.border_rect(x, y + 2, box_size, box_size, theme::C_BORDER);

        // Check mark
        if self.checked {
            let inner = box_size - 4;
            ctx.fill_rect(x + 2, y + 4, inner, inner, theme::C_ACCENT);
        }

        // Label
        let text_x = x + box_size as isize + 6;
        ctx.text(text_x, y + (h as isize - theme::CHAR_H as isize) / 2, self.label, theme::C_FG);

        let w = box_size + 6 + self.label.len() * theme::CHAR_W;
        (w, h)
    }
}

// ═══════════════════════════════════════════════════════════════
// Tooltip
// ═══════════════════════════════════════════════════════════════

/// A floating tooltip (drawn on top of everything).
pub struct Tooltip<'a> {
    text: &'a str,
}

impl<'a> Tooltip<'a> {
    pub fn new(text: &'a str) -> Self {
        Self { text }
    }

    /// Draw near position (px, py), offset slightly.
    pub fn draw(&self, ctx: &mut DrawContext, px: isize, py: isize) -> (usize, usize) {
        let w = self.text.len() * theme::CHAR_W + 8;
        let h = theme::CHAR_H + 6;
        let x = px + 12;
        let y = py + 12;

        ctx.fill_rect(x, y, w, h, Color::new(0x00111115));
        ctx.border_rect(x, y, w, h, theme::C_BORDER);
        ctx.text(x + 4, y + 3, self.text, theme::C_FG);
        (w, h)
    }
}
