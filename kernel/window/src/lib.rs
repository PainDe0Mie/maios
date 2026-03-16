//! A `Window` object is owned by an application and wraps a `WindowInner` managed by the
//! window manager.  It draws its own decorations (title bar, borders, buttons) and filters
//! internal events (moving, resizing, button clicks) before forwarding the rest to the app.
//!
//! See `applications/new_window` for a usage example.

#![no_std]
#![feature(type_alias_impl_trait)]

extern crate alloc;
extern crate mpmc;
extern crate event_types;
extern crate spin;
#[macro_use]
extern crate log;
extern crate framebuffer;
extern crate framebuffer_drawer;
extern crate mouse;
extern crate window_inner;
extern crate window_manager;
extern crate shapes;
extern crate color;
extern crate dereffer;
extern crate task;
extern crate font;

use alloc::string::String;
use alloc::sync::Arc;
use dereffer::{DerefsTo, DerefsToMut};
use mpmc::Queue;
use event_types::{Event, MousePositionEvent};
use framebuffer::{Framebuffer, AlphaPixel};
use color::Color;
use shapes::{Coord, Rectangle};
use spin::{Mutex, MutexGuard};
use window_inner::{
    WindowInner, WindowMovingStatus, ResizeEdge,
    DEFAULT_BORDER_SIZE, DEFAULT_TITLE_BAR_HEIGHT,
    MIN_WINDOW_WIDTH, MIN_WINDOW_HEIGHT,
};
use window_manager::WINDOW_MANAGER;

// ── Tokyo Night colour palette ────────────────────────────────────────────────

const TN_BG:        Color = Color::new(0x001A1B26); // title bar bg (active)
const TN_BG_DARK:   Color = Color::new(0x00121212); // title bar bg (inactive)
const TN_BORDER:    Color = Color::new(0x007AA2F7); // blue accent (active)
const TN_BORDER_DIM:Color = Color::new(0x00292E42); // inactive border
const TN_TEXT:      Color = Color::new(0x00C0CAF5); // primary text
const TN_TEXT_DIM:  Color = Color::new(0x00565F89); // inactive text
const TN_HIGHLIGHT: Color = Color::new(0x002F334D); // subtle top glow

// Button colours
const BTN_CLOSE:    Color = Color::new(0x00F7768E); // red
const BTN_MAXIMIZE: Color = Color::new(0x009ECE6A); // green
const BTN_MINIMIZE: Color = Color::new(0x00E0AF68); // yellow
const BTN_HOVER_DIM:Color = Color::new(0x00414868); // hover bg (non-close)
const BTN_INACTIVE: Color = Color::new(0x0024283B); // inactive button bg

// ── Layout constants ──────────────────────────────────────────────────────────

/// Corner rounding radius.
const WINDOW_RADIUS: usize = 8;
/// Button diameter in pixels.
const BTN_SIZE: usize = 12;
/// Gap between buttons.
const BTN_GAP:  usize = 8;
/// Right margin before the first button.
const BTN_MARGIN_RIGHT: usize = 12;
/// Edge resize hit zone width.
const RESIZE_MARGIN: usize = 6;

// ── Button identity ───────────────────────────────────────────────────────────

/// The three window-control buttons, ordered left-to-right (macOS convention).
#[derive(Copy, Clone, PartialEq, Eq)]
enum Button {
    /// Closes the window.
    Close    = 0,
    /// Minimises (hides) the window.
    Minimize = 1,
    /// Maximises or restores the window.
    Maximize = 2,
}

impl Button {
    /// Returns the horizontal center of this button relative to the window's right edge.
    fn x_center(self, window_width: usize) -> isize {
        // Buttons are right-aligned; index 0 = rightmost.
        let idx = self as usize;
        let right_edge = window_width - BTN_MARGIN_RIGHT;
        (right_edge - idx * (BTN_SIZE + BTN_GAP) - BTN_SIZE / 2) as isize
    }

    /// Base (idle) fill color of this button.
    fn base_color(self) -> Color {
        match self {
            Button::Close    => BTN_CLOSE,
            Button::Maximize => BTN_MAXIMIZE,
            Button::Minimize => BTN_MINIMIZE,
        }
    }
}

/// Button render state.
#[derive(Copy, Clone, PartialEq, Eq)]
enum BtnState {
    Idle,
    Hover,
    Pressed,
}

// ── Deferred action flags ─────────────────────────────────────────────────────

/// Actions to perform after the event-processing loop, outside the inner lock.
///
/// Using a struct of booleans is deliberately simple — these flags are
/// computed and then acted on in a single sequential block, so there is no
/// risk of incorrect combinations.
#[derive(Default)]
struct PendingActions {
    refresh_buttons:       bool,
    refresh_float_border:  bool,
    finalise_move:         bool,
    set_active:            bool,
    hide_window:           bool,
}

// ── Window ────────────────────────────────────────────────────────────────────

/// The application-facing window handle.
pub struct Window {
    /// System-facing inner state (shared with the window manager via `Weak`).
    inner: Arc<Mutex<WindowInner>>,
    /// Event queue — the window manager pushes, the application pops via `handle_event`.
    event_consumer: Queue<Event>,
    /// Last mouse event, used to detect press-then-release ("click") patterns.
    last_mouse: MousePositionEvent,
    /// Cached active state to avoid redundant border redraws.
    last_is_active: bool,
    /// Window title displayed in the title bar.
    title: String,
    /// Saved geometry for restoring after maximise.
    restore_bounds: Option<Rectangle>,
}

impl Window {
    /// Creates a new window with the default title `"Mai Window"`.
    pub fn new(
        coordinate: Coord,
        width: usize,
        height: usize,
        background: Color,
    ) -> Result<Window, &'static str> {
        Self::with_title("Mai Window".into(), coordinate, width, height, background)
    }

    /// Creates a new window with a custom title.
    pub fn with_title(
        title: String,
        coordinate: Coord,
        width: usize,
        height: usize,
        background: Color,
    ) -> Result<Window, &'static str> {
        debug!("Window::with_title: {:?} at {:?} {}x{}", title, coordinate, width, height);

        if width < MIN_WINDOW_WIDTH || height < MIN_WINDOW_HEIGHT {
            return Err("window is too small for decorations");
        }

        let wm_ref = WINDOW_MANAGER.get().ok_or("window manager not initialised")?;

        let mut framebuffer = Framebuffer::new(width, height, None)?;
        framebuffer.fill(background.into());

        let event_consumer = Queue::with_capacity(100);
        let event_producer = event_consumer.clone();

        let task_id = task::get_my_current_task_id();
        let window_inner = WindowInner::new(coordinate, framebuffer, event_producer, Some(task_id));

        let mut window = Window {
            inner: Arc::new(Mutex::new(window_inner)),
            event_consumer,
            last_mouse: MousePositionEvent::default(),
            last_is_active: true,
            title,
            restore_bounds: None,
        };

        window.redraw_decorations(true);

        let mut wm = wm_ref.lock();
        wm.set_active(&window.inner, false)?;

        let area = window.inner.lock().get_envelope();
        wm.refresh_bottom_windows(Some(area), true)?;

        Ok(window)
    }

    // ── Event handling ────────────────────────────────────────────────────────

    /// Processes pending events and returns the first one the application should handle.
    ///
    /// Window-management events (moving, resizing, button clicks) are handled
    /// transparently and are **not** forwarded to the caller.
    ///
    /// Returns `Ok(None)` when the queue is empty.
    pub fn handle_event(&mut self) -> Result<Option<Event>, &'static str> {
        let wm_ref = WINDOW_MANAGER.get().ok_or("window manager not initialised")?;

        // Sync the active indicator without holding the WM lock during event processing.
        let is_active = wm_ref.lock().is_active(&self.inner);
        if is_active != self.last_is_active {
            self.last_is_active = is_active;
            self.redraw_decorations(is_active);
        }

        let mut pending   = PendingActions::default();
        let mut app_event: Option<Event> = None;

        while let Some(event) = self.event_consumer.pop() {
            match &event {
                Event::MousePositionEvent(mouse) => {
                    let mouse = mouse.clone();
                    self.process_mouse_event(&mouse, &mut pending, &mut app_event);
                    self.last_mouse = mouse;
                }
                other => {
                    app_event = Some(other.clone());
                }
            }

            // Return the first non-internal event immediately; remaining events
            // stay in the queue for the next call.
            if app_event.is_some() {
                break;
            }
        }

        // ── Deferred actions (all outside the inner lock) ─────────────────────

        let (screen_w, screen_h) = wm_ref.lock().get_screen_size();
        let _ = (screen_w, screen_h); // suppress unused warning if not needed below

        if pending.hide_window {
            wm_ref.lock().hide_window(&self.inner)?;
        }

        {
            let mut wm = wm_ref.lock();

            if pending.set_active {
                wm.set_active(&self.inner, true)?;
            }
            if pending.refresh_buttons {
                let area = self.button_area();
                wm.refresh_active_window(Some(area))?;
                wm.refresh_mouse()?;
            }
            if pending.refresh_float_border {
                wm.move_floating_border()?;
            }
            if pending.finalise_move {
                wm.move_active_window()?;
                self.inner.lock().moving = WindowMovingStatus::Stationary;
                self.redraw_decorations(self.last_is_active);
            }
        }

        Ok(app_event)
    }

    /// Processes a single mouse event, updating `pending` and `app_event`.
    fn process_mouse_event(
        &mut self,
        mouse: &MousePositionEvent,
        pending: &mut PendingActions,
        app_event: &mut Option<Event>,
    ) {
        let mut inner = self.inner.lock();
        let (width, height) = inner.get_size();

        match inner.moving.clone() {
            // ── Window is being dragged or resized ────────────────────────────
            WindowMovingStatus::Moving(_) | WindowMovingStatus::Resizing(_, _) => {
                if !mouse.left_button_hold {
                    pending.finalise_move = true;
                }
                pending.refresh_float_border = true;
            }

            // ── Window is stationary ──────────────────────────────────────────
            WindowMovingStatus::Stationary => {
                let mx = mouse.coordinate.x;
                let my = mouse.coordinate.y;
                let in_title = (my as usize) < inner.title_bar_height && (mx as usize) < width;

                if in_title {
                    // ── Title bar hit ─────────────────────────────────────────
                    let hit = self.button_hit(mx, width);

                    if let Some(btn) = hit {
                        let state = if mouse.left_button_hold {
                            BtnState::Pressed
                        } else {
                            BtnState::Hover
                        };
                        self.draw_button(btn, state, &mut inner);
                        pending.refresh_buttons = true;

                        // Click = was pressed last frame, released this frame
                        if self.last_mouse.left_button_hold && !mouse.left_button_hold {
                            match btn {
                                Button::Close => {
                                    *app_event = Some(Event::ExitEvent);
                                    return;
                                }
                                Button::Maximize => {
                                    drop(inner); // release before calling resize
                                    self.toggle_maximize();
                                    return;
                                }
                                Button::Minimize => {
                                    pending.hide_window = true;
                                }
                            }
                        }
                    } else {
                        // Reset all buttons to idle
                        for btn in [Button::Close, Button::Minimize, Button::Maximize] {
                            self.draw_button(btn, BtnState::Idle, &mut inner);
                        }
                        pending.refresh_buttons = true;

                        // Start dragging on press
                        if !self.last_mouse.left_button_hold && mouse.left_button_hold {
                            inner.moving = WindowMovingStatus::Moving(mouse.gcoordinate);
                            pending.refresh_float_border = true;
                        }
                    }

                    // Title bar events are never forwarded to the application.
                } else {
                    // ── Content / border area ─────────────────────────────────
                    let m = RESIZE_MARGIN as isize;
                    let w = width as isize;
                    let h = height as isize;

                    let on_left   = mx < m;
                    let on_right  = mx > w - m;
                    let on_top    = my < m;
                    let on_bottom = my > h - m;
                    let on_border = on_left || on_right || on_top || on_bottom;

                    if on_border && mouse.left_button_hold {
                        let edge = resize_edge(on_top, on_bottom, on_left, on_right);
                        inner.moving = WindowMovingStatus::Resizing(mouse.gcoordinate, edge);
                        pending.refresh_float_border = true;
                    } else {
                        // Forward to the application.
                        *app_event = Some(Event::MousePositionEvent(mouse.clone()));
                    }
                }

                // Clicking anywhere in the window raises it.
                if !self.last_mouse.left_button_hold && mouse.left_button_hold {
                    pending.set_active = true;
                }
            }
        }
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Renders (composites) the given area of this window to the screen.
    ///
    /// Pass `None` to refresh the whole window.
    pub fn render(&mut self, area: Option<Rectangle>) -> Result<(), &'static str> {
        let wm_ref = WINDOW_MANAGER.get().ok_or("window manager not initialised")?;
        let origin = self.inner.lock().get_position();
        let absolute = area.map(|bb| bb + origin);
        wm_ref.lock().refresh_windows(absolute)
    }

    /// Returns the window's content area (excluding decorations), relative to the window.
    pub fn area(&self) -> Rectangle {
        self.inner.lock().content_area()
    }

    /// Immutable reference to this window's framebuffer.
    pub fn framebuffer(&self) -> FramebufferRef {
        FramebufferRef::new(self.inner.lock(), |g| g.framebuffer())
    }

    /// Mutable reference to this window's framebuffer.
    pub fn framebuffer_mut(&mut self) -> FramebufferRefMut {
        FramebufferRefMut::new(self.inner.lock(), |g| g.framebuffer(), |g| g.framebuffer_mut())
    }

    /// Returns `true` if this window currently has focus.
    pub fn is_active(&self) -> bool {
        WINDOW_MANAGER.get()
            .map(|wm| wm.lock().is_active(&self.inner))
            .unwrap_or(false)
    }

    // ── Decoration drawing ────────────────────────────────────────────────────

    /// Redraws title bar, borders, corner rounding, and all three buttons.
    pub fn redraw_decorations(&mut self, active: bool) {
        let mut inner = self.inner.lock();
        self.draw_title_bar(&mut inner, active);
        self.draw_borders(&mut inner, active);
        self.draw_rounded_corners(&mut inner);
        drop(inner);

        let mut inner = self.inner.lock();
        for btn in [Button::Close, Button::Minimize, Button::Maximize] {
            self.draw_button(btn, BtnState::Idle, &mut inner);
        }
    }

    /// Draws the title bar background, accent line, and title text.
    fn draw_title_bar(&self, inner: &mut WindowInner, active: bool) {
        let (width, _) = inner.get_size();
        let tbh = inner.title_bar_height;

        let bg = if active { TN_BG } else { TN_BG_DARK };
        framebuffer_drawer::fill_rectangle(inner.framebuffer_mut(),
            Coord::new(0, 0), width, tbh, bg.into());

        // Subtle top glow strip (active only)
        if active {
            framebuffer_drawer::fill_rectangle(inner.framebuffer_mut(),
                Coord::new(0, 0), width, 1, TN_HIGHLIGHT.into());
        }

        // Bottom accent line
        let accent = if active { TN_BORDER } else { TN_BORDER_DIM };
        framebuffer_drawer::fill_rectangle(inner.framebuffer_mut(),
            Coord::new(0, (tbh - 1) as isize), width, 1, accent.into());

        // Title text — centred in the gap left of the three buttons
        let btn_zone_width = 3 * BTN_SIZE + 2 * BTN_GAP + BTN_MARGIN_RIGHT + 4;
        let avail_start = 12usize;
        let avail_end   = width.saturating_sub(btn_zone_width);
        let text        = &self.title;
        let text_px_w   = text.chars().count() * font::CHARACTER_WIDTH;
        let text_x = if text_px_w < avail_end.saturating_sub(avail_start) {
            (avail_start + avail_end.saturating_sub(text_px_w)) / 2
        } else {
            avail_start
        };
        let text_y = tbh.saturating_sub(font::CHARACTER_HEIGHT) / 2;
        let text_color = if active { TN_TEXT } else { TN_TEXT_DIM };

        let mut cx = text_x as isize;
        for ch in text.chars() {
            let idx = ch as usize;
            if idx >= 256 { cx += font::CHARACTER_WIDTH as isize; continue; }
            let bitmap = &font::FONT_BASIC[idx];
            for row in 0..font::CHARACTER_HEIGHT {
                let bits = bitmap[row];
                for col in 0..8usize {
                    if bits & (0x80 >> col) != 0 {
                        inner.framebuffer_mut().draw_pixel(
                            Coord::new(cx + col as isize, text_y as isize + row as isize),
                            text_color.into(),
                        );
                    }
                }
            }
            cx += font::CHARACTER_WIDTH as isize;
            if cx >= avail_end as isize { break; }
        }
    }

    /// Draws the three side borders (left, right, bottom).
    fn draw_borders(&self, inner: &mut WindowInner, active: bool) {
        let (width, height) = inner.get_size();
        let bs  = inner.border_size;
        let tbh = inner.title_bar_height;
        let bc  = if active { TN_BORDER } else { TN_BORDER_DIM };

        // Left
        framebuffer_drawer::fill_rectangle(inner.framebuffer_mut(),
            Coord::new(0, tbh as isize), bs, height - tbh, bc.into());
        // Right
        framebuffer_drawer::fill_rectangle(inner.framebuffer_mut(),
            Coord::new((width - bs) as isize, tbh as isize), bs, height - tbh, bc.into());
        // Bottom
        framebuffer_drawer::fill_rectangle(inner.framebuffer_mut(),
            Coord::new(0, (height - bs) as isize), width, bs, bc.into());
    }

    /// Masks top corners with transparency so the window appears rounded.
    fn draw_rounded_corners(&self, inner: &mut WindowInner) {
        let (width, _) = inner.get_size();
        let r2 = WINDOW_RADIUS * WINDOW_RADIUS;
        let tp = color::TRANSPARENT.into();
        for i in 0..WINDOW_RADIUS {
            for j in 0..WINDOW_RADIUS {
                let dx = WINDOW_RADIUS - i;
                let dy = WINDOW_RADIUS - j;
                if dx * dx + dy * dy > r2 {
                    inner.framebuffer_mut()
                        .overwrite_pixel(Coord::new(i as isize, j as isize), tp);
                    inner.framebuffer_mut()
                        .overwrite_pixel(Coord::new((width - i - 1) as isize, j as isize), tp);
                }
            }
        }
    }

    /// Draws a single button as a filled circle with an icon in the centre.
    fn draw_button(&self, btn: Button, state: BtnState, inner: &mut WindowInner) {
        let (width, _) = inner.get_size();
        let cx = btn.x_center(width);
        let cy = (inner.title_bar_height / 2) as isize;
        let r  = (BTN_SIZE / 2) as isize;

        let fill = match state {
            BtnState::Idle    => btn.base_color(),
            BtnState::Hover   => if btn == Button::Close { BTN_CLOSE } else { BTN_HOVER_DIM },
            BtnState::Pressed => BTN_INACTIVE,
        };

        // Filled circle
        framebuffer_drawer::fill_circle(
            inner.framebuffer_mut(),
            Coord::new(cx, cy),
            BTN_SIZE / 2,
            fill.into(),
        );

        // Icon drawn only on hover or press
        if state != BtnState::Idle {
            let icon_color = color::WHITE.into();
            let s = 2isize;
            let center = Coord::new(cx, cy);
            match btn {
                Button::Close => {
                    framebuffer_drawer::draw_line(inner.framebuffer_mut(),
                        center + (-s, -s), center + (s, s), icon_color);
                    framebuffer_drawer::draw_line(inner.framebuffer_mut(),
                        center + (s, -s), center + (-s, s), icon_color);
                }
                Button::Maximize => {
                    framebuffer_drawer::draw_line(inner.framebuffer_mut(),
                        center + (-s, -s), center + (s, -s), icon_color);
                    framebuffer_drawer::draw_line(inner.framebuffer_mut(),
                        center + (-s,  s), center + (s,  s), icon_color);
                    framebuffer_drawer::draw_line(inner.framebuffer_mut(),
                        center + (-s, -s), center + (-s, s), icon_color);
                    framebuffer_drawer::draw_line(inner.framebuffer_mut(),
                        center + ( s, -s), center + (s,  s), icon_color);
                }
                Button::Minimize => {
                    framebuffer_drawer::draw_line(inner.framebuffer_mut(),
                        center + (-s, 0), center + (s, 0), icon_color);
                }
            }
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Returns which button, if any, contains `x` (window-relative coordinate).
    fn button_hit(&self, x: isize, window_width: usize) -> Option<Button> {
        let r = (BTN_SIZE / 2 + 2) as isize; // slightly larger hit zone
        for btn in [Button::Close, Button::Minimize, Button::Maximize] {
            let cx = btn.x_center(window_width);
            if (x - cx).abs() <= r {
                return Some(btn);
            }
        }
        None
    }

    /// Rectangle covering the entire title bar (used for selective refresh).
    fn button_area(&self) -> Rectangle {
        let inner = self.inner.lock();
        let w = inner.get_size().0;
        Rectangle {
            top_left:     Coord::new(0, 0),
            bottom_right: Coord::new(w as isize, inner.title_bar_height as isize),
        }
    }

    /// Toggles between maximised and restored state.
    fn toggle_maximize(&mut self) {
        let wm_ref = match WINDOW_MANAGER.get() {
            Some(wm) => wm,
            None => return,
        };
        let (screen_w, screen_h) = wm_ref.lock().get_screen_size();

        if let Some(restore) = self.restore_bounds.take() {
            // Restore
            if let Err(e) = self.inner.lock().resize(restore) {
                error!("toggle_maximize restore failed: {}", e);
            }
        } else {
            // Maximise — save current geometry first
            let (pos, size) = {
                let inner = self.inner.lock();
                (inner.get_position(), inner.get_size())
            };
            self.restore_bounds = Some(Rectangle {
                top_left:     pos,
                bottom_right: pos + (size.0 as isize, size.1 as isize),
            });
            let full = Rectangle {
                top_left:     Coord::new(0, 0),
                bottom_right: Coord::new(screen_w as isize, screen_h as isize),
            };
            if let Err(e) = self.inner.lock().resize(full) {
                error!("toggle_maximize expand failed: {}", e);
            }
        }

        self.redraw_decorations(self.last_is_active);
        if let Err(e) = wm_ref.lock().refresh_bottom_windows(None, true) {
            error!("toggle_maximize refresh failed: {}", e);
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        if let Some(wm) = WINDOW_MANAGER.get() {
            if let Err(e) = wm.lock().delete_window(&self.inner) {
                error!("Window::drop — delete_window failed: {:?}", e);
            }
        } else {
            error!("Window::drop — window manager not initialised");
        }
    }
}

// ── Helper: resize edge from border hit ──────────────────────────────────────

fn resize_edge(top: bool, bottom: bool, left: bool, right: bool) -> ResizeEdge {
    match (top, bottom, left, right) {
        (true,  _,     true,  _    ) => ResizeEdge::TopLeft,
        (true,  _,     _,     true ) => ResizeEdge::TopRight,
        (_,     true,  true,  _    ) => ResizeEdge::BottomLeft,
        (_,     true,  _,     true ) => ResizeEdge::BottomRight,
        (true,  _,     _,     _    ) => ResizeEdge::Top,
        (_,     true,  _,     _    ) => ResizeEdge::Bottom,
        (_,     _,     true,  _    ) => ResizeEdge::Left,
        _                            => ResizeEdge::Right,
    }
}

// ── Framebuffer reference wrappers ───────────────────────────────────────────

/// Immutable borrow of a window's framebuffer (releases the lock on drop).
pub type FramebufferRef<'g> =
    DerefsTo<MutexGuard<'g, WindowInner>, Framebuffer<AlphaPixel>>;

/// Mutable borrow of a window's framebuffer (releases the lock on drop).
pub type FramebufferRefMut<'g> =
    DerefsToMut<MutexGuard<'g, WindowInner>, Framebuffer<AlphaPixel>>;