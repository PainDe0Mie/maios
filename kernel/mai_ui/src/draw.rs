//! Low-level drawing context that wraps framebuffer operations.
//!
//! `DrawContext` provides a clean API over the raw framebuffer, handling
//! coordinate transforms, clipping, and text rendering.

use color::Color;
use shapes::Coord;
use framebuffer::{Framebuffer, AlphaPixel};

/// A drawing context wrapping a mutable framebuffer reference.
///
/// All widget drawing goes through this to ensure consistent coordinate
/// handling and future support for clip regions and transforms.
pub struct DrawContext<'a> {
    fb: &'a mut Framebuffer<AlphaPixel>,
    /// Offset applied to all drawing operations (for nested layouts)
    origin_x: isize,
    origin_y: isize,
}

impl<'a> DrawContext<'a> {
    /// Create a new draw context from a framebuffer.
    pub fn new(fb: &'a mut Framebuffer<AlphaPixel>) -> Self {
        Self { fb, origin_x: 0, origin_y: 0 }
    }

    /// Create a sub-context with an offset origin.
    /// Useful for drawing inside panels or scroll regions.
    pub fn with_offset(&mut self, dx: isize, dy: isize) -> DrawContext<'_> {
        DrawContext {
            fb: self.fb,
            origin_x: self.origin_x + dx,
            origin_y: self.origin_y + dy,
        }
    }

    /// Width of the underlying framebuffer.
    #[inline]
    pub fn width(&self) -> usize {
        self.fb.width()
    }

    /// Height of the underlying framebuffer.
    #[inline]
    pub fn height(&self) -> usize {
        self.fb.height()
    }

    /// Raw framebuffer access (for advanced use).
    pub fn framebuffer_mut(&mut self) -> &mut Framebuffer<AlphaPixel> {
        self.fb
    }

    // ───────────────────────────────────────────────────────────
    // Primitives
    // ───────────────────────────────────────────────────────────

    /// Fill a rectangle with a solid color.
    #[inline]
    pub fn fill_rect(&mut self, x: isize, y: isize, w: usize, h: usize, color: Color) {
        framebuffer_drawer::fill_rectangle(
            self.fb,
            Coord::new(self.origin_x + x, self.origin_y + y),
            w, h,
            color.into(),
        );
    }

    /// Draw a single pixel.
    #[inline]
    pub fn pixel(&mut self, x: isize, y: isize, color: Color) {
        self.fb.draw_pixel(
            Coord::new(self.origin_x + x, self.origin_y + y),
            color.into(),
        );
    }

    /// Draw a horizontal line from x0 to x1 (exclusive) at the given y.
    pub fn hline(&mut self, x0: isize, x1: isize, y: isize, color: Color) {
        let ay = self.origin_y + y;
        let ax0 = self.origin_x + x0;
        let ax1 = self.origin_x + x1;
        for x in ax0..ax1 {
            self.fb.draw_pixel(Coord::new(x, ay), color.into());
        }
    }

    /// Draw a vertical line from y0 to y1 (exclusive) at the given x.
    pub fn vline(&mut self, x: isize, y0: isize, y1: isize, color: Color) {
        let ax = self.origin_x + x;
        let ay0 = self.origin_y + y0;
        let ay1 = self.origin_y + y1;
        for y in ay0..ay1 {
            self.fb.draw_pixel(Coord::new(ax, y), color.into());
        }
    }

    /// Draw a 1px border rectangle (outline only).
    pub fn border_rect(&mut self, x: isize, y: isize, w: usize, h: usize, color: Color) {
        let x1 = x + w as isize;
        let y1 = y + h as isize;
        self.hline(x, x1, y, color);
        self.hline(x, x1, y1 - 1, color);
        self.vline(x, y, y1, color);
        self.vline(x1 - 1, y, y1, color);
    }

    /// Draw a filled rounded rectangle (approximate with filled rect + corner pixels).
    pub fn rounded_rect(&mut self, x: isize, y: isize, w: usize, h: usize, r: usize, color: Color) {
        if r == 0 || w < r * 2 || h < r * 2 {
            self.fill_rect(x, y, w, h, color);
            return;
        }
        // Main body (excluding corners)
        self.fill_rect(x + r as isize, y, w - 2 * r, h, color);
        self.fill_rect(x, y + r as isize, r, h - 2 * r, color);
        self.fill_rect(x + w as isize - r as isize, y + r as isize, r, h - 2 * r, color);
        // Fill corner circles
        let ri = r as isize;
        for dy in 0..ri {
            let dx = isqrt(ri * ri - dy * dy);
            // Top-left
            self.fill_rect(x + ri - dx, y + ri - dy, dx as usize, 1, color);
            // Top-right
            self.fill_rect(x + w as isize - ri, y + ri - dy, dx as usize, 1, color);
            // Bottom-left
            self.fill_rect(x + ri - dx, y + h as isize - ri + dy, dx as usize, 1, color);
            // Bottom-right
            self.fill_rect(x + w as isize - ri, y + h as isize - ri + dy, dx as usize, 1, color);
        }
    }

    // ───────────────────────────────────────────────────────────
    // Text
    // ───────────────────────────────────────────────────────────

    /// Draw a single character at (x, y) using the bitmap font.
    pub fn draw_char(&mut self, x: isize, y: isize, ch: char, color: Color) {
        let idx = ch as usize;
        if idx >= 256 { return; }
        let bitmap = &font::FONT_BASIC[idx];
        let ax = self.origin_x + x;
        let ay = self.origin_y + y;
        for row in 0..font::CHARACTER_HEIGHT {
            let bits = bitmap[row];
            for col in 0..8usize {
                if bits & (0x80 >> col) != 0 {
                    self.fb.draw_pixel(
                        Coord::new(ax + col as isize, ay + row as isize),
                        color.into(),
                    );
                }
            }
        }
    }

    /// Draw a text string at (x, y).
    pub fn text(&mut self, x: isize, y: isize, text: &str, color: Color) {
        let mut cx = x;
        for ch in text.chars() {
            self.draw_char(cx, y, ch, color);
            cx += font::CHARACTER_WIDTH as isize;
        }
    }

    /// Draw text clipped to a maximum number of characters.
    pub fn text_clipped(&mut self, x: isize, y: isize, text: &str, max_chars: usize, color: Color) {
        let mut cx = x;
        for (i, ch) in text.chars().enumerate() {
            if i >= max_chars { break; }
            self.draw_char(cx, y, ch, color);
            cx += font::CHARACTER_WIDTH as isize;
        }
    }

    /// Draw right-aligned text ending at x_right.
    pub fn text_right(&mut self, x_right: isize, y: isize, text: &str, color: Color) {
        let text_w = text.len() as isize * font::CHARACTER_WIDTH as isize;
        self.text(x_right - text_w, y, text, color);
    }

    /// Draw centered text within a given width.
    pub fn text_centered(&mut self, x: isize, y: isize, w: usize, text: &str, color: Color) {
        let text_w = text.len() * font::CHARACTER_WIDTH;
        let offset = (w.saturating_sub(text_w)) / 2;
        self.text(x + offset as isize, y, text, color);
    }
}

/// Integer square root via Newton's method.
fn isqrt(n: isize) -> isize {
    if n <= 0 { return 0; }
    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}
