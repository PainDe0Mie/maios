//! # MaiUI — MaiOS Widget Toolkit
//!
//! Provides reusable UI components for MaiOS desktop applications.
//! Built on top of the framebuffer drawing primitives with the Tokyo Night theme.
//!
//! ## Architecture
//!
//! - **Immediate-mode rendering**: Widgets draw directly to a framebuffer.
//! - **No heap allocations** in drawing hot paths (only in layout building).
//! - **Theme-first**: Tokyo Night palette baked in, with easy override.
//! - **Composable**: Combine widgets via `VStack`, `HStack`, `Panel`.
//!
//! ## Usage
//!
//! ```no_run
//! use mai_ui::prelude::*;
//!
//! // In your app's render function:
//! let mut ctx = DrawContext::new(&mut framebuffer);
//! Label::new("Hello MaiOS").color(theme::C_ACCENT).draw(&mut ctx, 10, 10);
//! Button::new("Click me").draw(&mut ctx, 10, 40);
//! ProgressBar::new(75).draw(&mut ctx, 10, 80, 300);
//! ```

#![no_std]

extern crate alloc;

pub mod theme;
pub mod draw;
pub mod widgets;

/// Re-exports for convenient `use mai_ui::prelude::*;`
pub mod prelude {
    pub use crate::theme;
    pub use crate::draw::DrawContext;
    pub use crate::widgets::*;
}
