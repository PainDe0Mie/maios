//! Window manager — owns the stacking list of windows and composites them to the screen.
//!
//! Rendering pipeline (back to front):
//!   `bottom_fb` (wallpaper) → hidden list (skipped) → show_list → active → `top_fb` (cursor / float border)
//!
//! The manager exposes a stable `refresh_windows(Option<Rectangle>)` API: pass `None` to
//! repaint the full screen or `Some(rect)` to repaint only the dirty region.

#![no_std]
extern crate spin;
#[macro_use]
extern crate log;
extern crate alloc;
extern crate mpmc;
extern crate event_types;
extern crate compositor;
extern crate color;
extern crate shapes;
extern crate framebuffer;
extern crate framebuffer_compositor;
extern crate framebuffer_drawer;
extern crate keycodes_ascii;
extern crate mod_mgmt;
extern crate mouse_data;
extern crate scheduler;
extern crate spawn;
extern crate window_inner;
extern crate mgi;
extern crate cpu;
extern crate preemption;

use alloc::collections::VecDeque;
use alloc::string::ToString;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use mpmc::Queue;
use event_types::{Event, MousePositionEvent};
use framebuffer::{Framebuffer, AlphaPixel};
use color::Color;
use shapes::{Coord, Rectangle};
use keycodes_ascii::{KeyAction, KeyEvent, Keycode};
use mouse_data::MouseEvent;
use spin::{Mutex, Once};
use window_inner::{WindowInner, WindowMovingStatus, ResizeEdge};
use mgi::{MGI, DrawCommand};

/// The global window manager instance.
pub static WINDOW_MANAGER: Once<Mutex<WindowManager>> = Once::new();

// ── Mouse cursor ──────────────────────────────────────────────────────────────

const MOUSE_W: usize = 11;
const MOUSE_H: usize = 18;

static MOUSE_IMAGE: [[Color; MOUSE_H]; MOUSE_W] = {
    const T: Color = color::TRANSPARENT;
    const C: Color = color::BLACK;
    const B: Color = color::WHITE;
    [
        [B, B, B, B, B, B, B, B, B, B, B, B, B, B, B, B, T, T],
        [T, B, C, C, C, C, C, C, C, C, C, C, C, C, B, T, T, T],
        [T, T, B, C, C, C, C, C, C, C, C, C, C, B, T, T, T, T],
        [T, T, T, B, C, C, C, C, C, C, C, C, B, T, T, T, T, T],
        [T, T, T, T, B, C, C, C, C, C, C, C, C, B, B, T, T, T],
        [T, T, T, T, T, B, C, C, C, C, C, C, C, C, C, B, B, T],
        [T, T, T, T, T, T, B, C, C, C, C, B, B, C, C, C, C, B],
        [T, T, T, T, T, T, T, B, C, C, B, T, T, B, B, C, B, T],
        [T, T, T, T, T, T, T, T, B, C, B, T, T, T, T, B, B, T],
        [T, T, T, T, T, T, T, T, T, B, B, T, T, T, T, T, T, T],
        [T, T, T, T, T, T, T, T, T, T, B, T, T, T, T, T, T, T],
    ]
};

// ── Floating border (shown while dragging) ────────────────────────────────────

const FLOAT_BORDER_PX:    usize = 2;
const FLOAT_BORDER_COLOR: Color = Color::new(0x005DADE2);

// ── WindowManager ─────────────────────────────────────────────────────────────

/// Manages the stacking order and rendering of all windows.
pub struct WindowManager {
    /// Visible windows below the active one, back to front.
    show_list:           VecDeque<Weak<Mutex<WindowInner>>>,
    /// Windows that are hidden (minimised).
    hide_list:           VecDeque<Weak<Mutex<WindowInner>>>,
    /// The window currently holding focus.
    active:              Weak<Mutex<WindowInner>>,
    /// Current mouse position in screen coordinates.
    mouse:               Coord,
    mouse_btn_left:      bool,
    mouse_btn_right:     bool,
    /// Saved floating border rectangle (cleared when drag ends).
    repositioned_border: Option<Rectangle>,
    /// Desktop background framebuffer.
    bottom_fb:           Framebuffer<AlphaPixel>,
    /// Overlay framebuffer: floating border + cursor.
    top_fb:              Framebuffer<AlphaPixel>,
}

impl WindowManager {
    // ── List helpers ──────────────────────────────────────────────────────────

    /// Removes stale (dropped) weak references from both lists.
    fn purge_expired(&mut self) {
        self.show_list.retain(|w| w.upgrade().is_some());
        self.hide_list.retain(|w| w.upgrade().is_some());
    }

    /// Pops the first live entry from `list`, discarding dead ones.
    fn pop_first_live(list: &mut VecDeque<Weak<Mutex<WindowInner>>>) -> Option<Weak<Mutex<WindowInner>>> {
        while let Some(w) = list.pop_front() {
            if w.upgrade().is_some() { return Some(w); }
        }
        None
    }

    fn index_in_show(&self, target: &Arc<Mutex<WindowInner>>) -> Option<usize> {
        self.show_list.iter().position(|w|
            w.upgrade().map_or(false, |a| Arc::ptr_eq(&a, target))
        )
    }

    fn index_in_hide(&self, target: &Arc<Mutex<WindowInner>>) -> Option<usize> {
        self.hide_list.iter().position(|w|
            w.upgrade().map_or(false, |a| Arc::ptr_eq(&a, target))
        )
    }

    // ── Focus management ──────────────────────────────────────────────────────

    /// Makes `inner_ref` the active (focused) window.
    ///
    /// The previously active window is demoted to the front of `show_list`.
    /// Returns `true` if the window was already active.
    pub fn set_active(
        &mut self,
        inner_ref: &Arc<Mutex<WindowInner>>,
        refresh: bool,
    ) -> Result<bool, &'static str> {
        self.purge_expired();

        // Already active — nothing to do.
        if let Some(current) = self.active.upgrade() {
            if Arc::ptr_eq(&current, inner_ref) {
                if refresh {
                    let area = inner_ref.lock().get_envelope();
                    self.refresh_bottom_windows(Some(area), true)?;
                }
                return Ok(true);
            }

            // Demote the current active to show_list (guard against duplicates).
            if self.index_in_show(&current).is_none() {
                self.show_list.push_front(Arc::downgrade(&current));
            }
        }

        // Remove target from wherever it is (show or hide).
        if let Some(i) = self.index_in_show(inner_ref) { self.show_list.remove(i); }
        if let Some(i) = self.index_in_hide(inner_ref) { self.hide_list.remove(i); }

        self.active = Arc::downgrade(inner_ref);

        if refresh {
            let area = inner_ref.lock().get_envelope();
            self.refresh_bottom_windows(Some(area), true)?;
        }
        Ok(false)
    }

    // ── Window lifecycle ──────────────────────────────────────────────────────

    /// Removes a window from the manager and refreshes the uncovered area.
    pub fn delete_window(&mut self, inner_ref: &Arc<Mutex<WindowInner>>) -> Result<(), &'static str> {
        self.purge_expired();

        let envelope = inner_ref.lock().get_envelope();
        let area = Some(envelope);

        if let Some(current) = self.active.upgrade() {
            if Arc::ptr_eq(&current, inner_ref) {
                self.refresh_bottom_windows(area, false)?;
                if let Some(next) = Self::pop_first_live(&mut self.show_list) {
                    self.active = next;
                } else {
                    self.active = Weak::new();
                }
                return Ok(());
            }
        }

        if let Some(i) = self.index_in_show(inner_ref) {
            self.show_list.remove(i);
            self.refresh_windows(area)?;
            return Ok(());
        }

        if let Some(i) = self.index_in_hide(inner_ref) {
            self.hide_list.remove(i);
            return Ok(()); // nothing visible changed
        }

        Err("delete_window: window not found in any list")
    }

    /// Hides (minimises) a window.
    pub fn hide_window(&mut self, inner_ref: &Arc<Mutex<WindowInner>>) -> Result<(), &'static str> {
        self.purge_expired();

        let is_active = self.active.upgrade()
            .map_or(false, |a| Arc::ptr_eq(&a, inner_ref));

        if is_active {
            if self.index_in_hide(inner_ref).is_none() {
                self.hide_list.push_back(Arc::downgrade(inner_ref));
            }
            if let Some(next) = Self::pop_first_live(&mut self.show_list) {
                self.active = next;
            } else {
                self.active = Weak::new();
            }
            let area = inner_ref.lock().get_envelope();
            self.refresh_bottom_windows(Some(area), true)?;
        } else if let Some(i) = self.index_in_show(inner_ref) {
            self.show_list.remove(i);
            if self.index_in_hide(inner_ref).is_none() {
                self.hide_list.push_back(Arc::downgrade(inner_ref));
            }
            let area = inner_ref.lock().get_envelope();
            self.refresh_windows(Some(area))?;
        }

        Ok(())
    }

    /// Unhides (restores) a window, making it visible but not necessarily focused.
    pub fn show_window(&mut self, inner_ref: &Arc<Mutex<WindowInner>>) -> Result<(), &'static str> {
        self.purge_expired();
        if let Some(i) = self.index_in_hide(inner_ref) {
            self.hide_list.remove(i);
            if self.index_in_show(inner_ref).is_none() {
                self.show_list.push_back(Arc::downgrade(inner_ref));
            }
            let area = inner_ref.lock().get_envelope();
            self.refresh_bottom_windows(Some(area), true)?;
        }
        Ok(())
    }

    // ── Rendering ─────────────────────────────────────────────────────────────

    /// Repaints the screen, compositing all visible layers.
    ///
    /// Pass `None` to repaint everything, or `Some(rect)` to repaint only that region.
    /// Set `include_active` to `false` to skip the active window (used when it is being deleted).
    pub fn refresh_bottom_windows(
        &mut self,
        region: Option<Rectangle>,
        include_active: bool,
    ) -> Result<(), &'static str> {
        // Collect live references (avoid holding multiple locks simultaneously).
        let mut windows: Vec<Arc<Mutex<WindowInner>>> = self.show_list
            .iter()
            .filter_map(|w| w.upgrade())
            .collect();

        if include_active {
            if let Some(a) = self.active.upgrade() {
                windows.push(a);
            }
        }

        let guards: Vec<_> = windows.iter().map(|w| w.lock()).collect();

        let mut cmds = Vec::new();
        let bg_buf = self.bottom_fb.buffer();
        let bg_w   = self.bottom_fb.width();

        match region {
            Some(r) => {
                cmds.push(DrawCommand::BlitRegion { src: bg_buf, src_width: bg_w,
                    dest_top_left: Coord::new(0, 0), region: r });
                for g in &guards {
                    cmds.push(DrawCommand::BlitRegion {
                        src: g.framebuffer().buffer(),
                        src_width: g.framebuffer().width(),
                        dest_top_left: g.get_position(),
                        region: r,
                    });
                }
                cmds.push(DrawCommand::BlitRegion {
                    src: self.top_fb.buffer(), src_width: self.top_fb.width(),
                    dest_top_left: Coord::new(0, 0), region: r,
                });
            }
            None => {
                cmds.push(DrawCommand::Blit { src: bg_buf, src_width: bg_w,
                    dest_top_left: Coord::new(0, 0) });
                for g in &guards {
                    cmds.push(DrawCommand::Blit {
                        src: g.framebuffer().buffer(),
                        src_width: g.framebuffer().width(),
                        dest_top_left: g.get_position(),
                    });
                }
                cmds.push(DrawCommand::Blit {
                    src: self.top_fb.buffer(), src_width: self.top_fb.width(),
                    dest_top_left: Coord::new(0, 0),
                });
            }
        }

        MGI.get().ok_or("MGI not initialised")?.lock().submit(&cmds);
        Ok(())
    }

    /// Convenience: repaint the entire visible stack including the active window.
    pub fn refresh_windows(&mut self, region: Option<Rectangle>) -> Result<(), &'static str> {
        self.refresh_bottom_windows(region, true)
    }

    /// Repaint only the active window.
    pub fn refresh_active_window(&mut self, region: Option<Rectangle>) -> Result<(), &'static str> {
        if let Some(a) = self.active.upgrade() {
            let g = a.lock();
            let cmd = match region {
                Some(r) => DrawCommand::BlitRegion {
                    src: g.framebuffer().buffer(),
                    src_width: g.framebuffer().width(),
                    dest_top_left: g.get_position(),
                    region: r,
                },
                None => DrawCommand::Blit {
                    src: g.framebuffer().buffer(),
                    src_width: g.framebuffer().width(),
                    dest_top_left: g.get_position(),
                },
            };
            MGI.get().ok_or("MGI not initialised")?.lock().submit(&[cmd]);
        }
        Ok(())
    }

    /// Repaint only the top overlay (cursor + floating border).
    pub fn refresh_top(&mut self, region: Option<Rectangle>) -> Result<(), &'static str> {
        let cmd = match region {
            Some(r) => DrawCommand::BlitRegion {
                src: self.top_fb.buffer(), src_width: self.top_fb.width(),
                dest_top_left: Coord::new(0, 0), region: r,
            },
            None => DrawCommand::Blit {
                src: self.top_fb.buffer(), src_width: self.top_fb.width(),
                dest_top_left: Coord::new(0, 0),
            },
        };
        MGI.get().ok_or("MGI not initialised")?.lock().submit(&[cmd]);
        Ok(())
    }

    /// Flush the composed image to the display.
    pub fn present(&mut self) {
        if let Some(mgi) = MGI.get() { mgi.lock().present(); }
    }

    // ── Mouse ─────────────────────────────────────────────────────────────────

    /// Moves the cursor by `delta` and repaints the dirty rect.
    fn move_mouse(&mut self, delta: Coord) -> Result<(), &'static str> {
        let (sw, sh) = self.get_screen_size();
        let new = Coord::new(
            (self.mouse.x + delta.x).clamp(0, sw as isize - 3),
            (self.mouse.y + delta.y).clamp(0, sh as isize - 3),
        );
        self.move_mouse_to(new)
    }

    /// Moves the cursor to an absolute position and repaints the dirty rect.
    fn move_mouse_to(&mut self, new: Coord) -> Result<(), &'static str> {
        let old = self.mouse;
        let (fb_w, fb_h) = self.top_fb.get_size();
        let tp: AlphaPixel = color::TRANSPARENT.into();

        // Erase old cursor from top_fb
        {
            let buf = self.top_fb.buffer_mut();
            for row in 0..MOUSE_H {
                let y = old.y + row as isize;
                if y < 0 || y >= fb_h as isize { continue; }
                let xs = old.x.clamp(0, fb_w as isize) as usize;
                let xe = (old.x + MOUSE_W as isize).clamp(0, fb_w as isize) as usize;
                if xs < xe {
                    buf[y as usize * fb_w + xs..y as usize * fb_w + xe].fill(tp);
                }
            }
        }

        self.mouse = new;

        // Draw new cursor into top_fb
        {
            let buf = self.top_fb.buffer_mut();
            for col in 0..MOUSE_W {
                for row in 0..MOUSE_H {
                    let x = new.x + col as isize;
                    let y = new.y + row as isize;
                    if x < 0 || y < 0 || x >= fb_w as isize || y >= fb_h as isize { continue; }
                    let c = MOUSE_IMAGE[col][row];
                    if c.transparency() != 255 {
                        buf[y as usize * fb_w + x as usize] = c.into();
                    }
                }
            }
        }

        // Minimal dirty rect covering both old and new cursor positions
        let dirty = Rectangle {
            top_left:     Coord::new(old.x.min(new.x), old.y.min(new.y)),
            bottom_right: Coord::new(
                old.x.max(new.x) + MOUSE_W as isize,
                old.y.max(new.y) + MOUSE_H as isize,
            ),
        };

        // Composite: background + visible windows + cursor overlay
        let windows: Vec<_> = self.show_list.iter().filter_map(|w| w.upgrade()).collect();
        let active_opt       = self.active.upgrade();
        let guards: Vec<_>   = windows.iter().map(|w| w.lock()).collect();
        let active_guard      = active_opt.as_ref().map(|a| a.lock());

        let mut cmds = Vec::new();
        cmds.push(DrawCommand::BlitRegion {
            src: self.bottom_fb.buffer(), src_width: self.bottom_fb.width(),
            dest_top_left: Coord::new(0, 0), region: dirty,
        });
        for g in &guards {
            cmds.push(DrawCommand::BlitRegion {
                src: g.framebuffer().buffer(), src_width: g.framebuffer().width(),
                dest_top_left: g.get_position(), region: dirty,
            });
        }
        if let Some(ref g) = active_guard {
            cmds.push(DrawCommand::BlitRegion {
                src: g.framebuffer().buffer(), src_width: g.framebuffer().width(),
                dest_top_left: g.get_position(), region: dirty,
            });
        }
        cmds.push(DrawCommand::BlitRegion {
            src: self.top_fb.buffer(), src_width: self.top_fb.width(),
            dest_top_left: Coord::new(0, 0), region: dirty,
        });

        let mut mgi = MGI.get().ok_or("MGI not initialised")?.lock();
        mgi.submit(&cmds);
        mgi.present();
        Ok(())
    }

    /// Refreshes only the cursor overlay region.
    pub fn refresh_mouse(&mut self) -> Result<(), &'static str> {
        let region = Some(Rectangle {
            top_left:     self.mouse,
            bottom_right: self.mouse + (MOUSE_W as isize, MOUSE_H as isize),
        });
        self.refresh_top(region)
    }

    pub fn mouse_position(&self) -> Coord   { self.mouse }
    pub fn mouse_left(&self)     -> bool    { self.mouse_btn_left }
    pub fn mouse_right(&self)    -> bool    { self.mouse_btn_right }

    // ── Floating border (drag preview) ────────────────────────────────────────

    /// Draws or erases the floating border and updates `repositioned_border`.
    fn refresh_floating_border(&mut self, show: bool, new_border: Rectangle) -> Result<(), &'static str> {
        // Erase the old border (always)
        if let Some(old) = self.repositioned_border.take() {
            self.draw_floating_border(&old, color::TRANSPARENT);
            self.refresh_bottom_windows(Some(old), true)?;
        }
        if show {
            self.draw_floating_border(&new_border, FLOAT_BORDER_COLOR);
            self.refresh_top(Some(new_border))?;
            self.repositioned_border = Some(new_border);
        }
        Ok(())
    }

    fn draw_floating_border(&mut self, border: &Rectangle, color: Color) {
        let pixel = color.into();
        for i in 0..(FLOAT_BORDER_PX as isize) {
            let w = (border.bottom_right.x - border.top_left.x) - 2 * i;
            let h = (border.bottom_right.y - border.top_left.y) - 2 * i;
            if w <= 0 || h <= 0 { break; }
            framebuffer_drawer::draw_rectangle(
                &mut self.top_fb,
                border.top_left + (i, i),
                w as usize,
                h as usize,
                pixel,
            );
        }
    }

    // ── Move / resize ─────────────────────────────────────────────────────────

    /// Applies the pending move or resize to the active window, then clears the
    /// floating border and refreshes the screen.
    pub fn move_active_window(&mut self) -> Result<(), &'static str> {
        let active = self.active.upgrade().ok_or("no active window to move")?;

        // Clear the floating border first.
        self.refresh_floating_border(false, Rectangle {
            top_left: Coord::new(0, 0), bottom_right: Coord::new(0, 0),
        })?;

        let (old_rect, new_rect) = {
            let mut win = active.lock();
            let (w, h) = win.get_size();
            let pos     = win.get_position();
            let old_rect = Rectangle {
                top_left: pos, bottom_right: pos + (w as isize, h as isize),
            };

            let mouse = self.mouse;

            let new_rect = match win.moving {
                WindowMovingStatus::Moving(base) => {
                    let new_pos = pos + (mouse.x - base.x, mouse.y - base.y);
                    win.set_position(new_pos);
                    Rectangle {
                        top_left: new_pos, bottom_right: new_pos + (w as isize, h as isize),
                    }
                }
                WindowMovingStatus::Resizing(base, edge) => {
                    let dx = mouse.x - base.x;
                    let dy = mouse.y - base.y;
                    let mut r = old_rect;
                    match edge {
                        ResizeEdge::Right       => r.bottom_right.x += dx,
                        ResizeEdge::Bottom      => r.bottom_right.y += dy,
                        ResizeEdge::Left        => r.top_left.x     += dx,
                        ResizeEdge::Top         => r.top_left.y     += dy,
                        ResizeEdge::BottomRight => { r.bottom_right.x += dx; r.bottom_right.y += dy; }
                        ResizeEdge::BottomLeft  => { r.top_left.x     += dx; r.bottom_right.y += dy; }
                        ResizeEdge::TopRight    => { r.bottom_right.x += dx; r.top_left.y     += dy; }
                        ResizeEdge::TopLeft     => { r.top_left.x     += dx; r.top_left.y     += dy; }
                    }
                    win.resize(r)?;
                    r
                }
                WindowMovingStatus::Stationary => return Err("window is not moving"),
            };

            (old_rect, new_rect)
        };

        self.refresh_bottom_windows(Some(old_rect), false)?;
        self.refresh_active_window(Some(new_rect))?;
        self.refresh_mouse()?;
        Ok(())
    }

    /// Shows the floating border at its new position based on the active window's
    /// current drag state.
    pub fn move_floating_border(&mut self) -> Result<(), &'static str> {
        let mouse = self.mouse;

        let border_opt = self.active.upgrade().and_then(|a| {
            let win = a.lock();
            let pos = win.get_position();
            let (w, h) = win.get_size();
            match win.moving {
                WindowMovingStatus::Moving(base) => {
                    let tl = pos + (mouse.x - base.x, mouse.y - base.y);
                    Some(Rectangle { top_left: tl, bottom_right: tl + (w as isize, h as isize) })
                }
                WindowMovingStatus::Resizing(base, edge) => {
                    let dx = mouse.x - base.x;
                    let dy = mouse.y - base.y;
                    let mut r = Rectangle {
                        top_left: pos,
                        bottom_right: pos + (w as isize, h as isize),
                    };
                    match edge {
                        ResizeEdge::Right       => r.bottom_right.x += dx,
                        ResizeEdge::Bottom      => r.bottom_right.y += dy,
                        ResizeEdge::Left        => r.top_left.x     += dx,
                        ResizeEdge::Top         => r.top_left.y     += dy,
                        ResizeEdge::BottomRight => { r.bottom_right.x += dx; r.bottom_right.y += dy; }
                        ResizeEdge::BottomLeft  => { r.top_left.x     += dx; r.bottom_right.y += dy; }
                        ResizeEdge::TopRight    => { r.bottom_right.x += dx; r.top_left.y     += dy; }
                        ResizeEdge::TopLeft     => { r.top_left.x     += dx; r.top_left.y     += dy; }
                    }
                    if r.width()  < 50 { r.bottom_right.x = r.top_left.x + 50; }
                    if r.height() < 50 { r.bottom_right.y = r.top_left.y + 50; }
                    Some(r)
                }
                WindowMovingStatus::Stationary => None,
            }
        });

        match border_opt {
            Some(b) => self.refresh_floating_border(true, b),
            None    => self.refresh_floating_border(false, Rectangle {
                top_left: Coord::new(0, 0), bottom_right: Coord::new(0, 0),
            }),
        }
    }

    // ── Event routing ─────────────────────────────────────────────────────────

    /// Routes a keyboard event to the active window.
    fn route_keyboard(&self, key_event: KeyEvent) -> Result<(), &'static str> {
        let win = self.active.upgrade()
            .ok_or("no active window to receive keyboard event")?;
        let result = win.lock().send_event(Event::new_keyboard_event(key_event))
            .map_err(|_| "keyboard event queue full");
        result
    }

    /// Routes a mouse event to the topmost window that the cursor is over.
    ///
    /// Checked from front (topmost) to back: active → show_list in reverse → give up.
    fn route_mouse(&self, mouse_event: MouseEvent) -> Result<(), &'static str> {
        let cursor = self.mouse;

        let build_event = |win_pos: Coord| -> MousePositionEvent {
            MousePositionEvent {
                coordinate:        cursor - win_pos,
                gcoordinate:       cursor,
                scrolling_up:      mouse_event.movement.scroll_movement > 0,
                scrolling_down:    mouse_event.movement.scroll_movement < 0,
                left_button_hold:  mouse_event.buttons.left(),
                right_button_hold: mouse_event.buttons.right(),
                fourth_button_hold: mouse_event.buttons.fourth(),
                fifth_button_hold: mouse_event.buttons.fifth(),
            }
        };

        // 1. Check active window first (also accepts events while dragging).
        if let Some(a) = self.active.upgrade() {
            let win = a.lock();
            let pos = win.get_position();
            if win.contains(cursor - pos) || matches!(win.moving, WindowMovingStatus::Moving(_)) {
                return win.send_event(Event::MousePositionEvent(build_event(pos)))
                    .map_err(|_| "mouse event queue full");
            }
        }

        // 2. Check show_list from front (topmost below active) to back.
        for weak in &self.show_list {
            if let Some(w) = weak.upgrade() {
                let win = w.lock();
                if !win.visible { continue; }
                let pos = win.get_position();
                if win.contains(cursor - pos) {
                    return win.send_event(Event::MousePositionEvent(build_event(pos)))
                        .map_err(|_| "mouse event queue full");
                }
            }
        }

        // Cursor is over the desktop — no window targeted.
        Ok(())
    }

    // ── Public queries ────────────────────────────────────────────────────────

    /// Returns `true` if `window` is the currently active window.
    pub fn is_active(&self, window: &Arc<Mutex<WindowInner>>) -> bool {
        self.active.upgrade().map_or(false, |a| Arc::ptr_eq(&a, window))
    }

    /// Returns `(width, height)` of the screen in pixels.
    pub fn get_screen_size(&self) -> (usize, usize) {
        MGI.get().expect("MGI not initialised").lock().resolution()
    }

    pub fn get_bottom_framebuffer_mut(&mut self) -> &mut Framebuffer<AlphaPixel> {
        &mut self.bottom_fb
    }

    pub fn get_bottom_framebuffer(&self) -> &Framebuffer<AlphaPixel> {
        &self.bottom_fb
    }

    pub fn get_active_window(&self) -> Option<Arc<Mutex<WindowInner>>> {
        self.active.upgrade()
    }

    pub fn is_window_hidden(&self, task_id: u64) -> bool {
        let id = task_id as usize;
        self.hide_list.iter().any(|w|
            w.upgrade().map_or(false, |w| w.lock().task_id == Some(id))
        )
    }

    pub fn get_window_by_task_id(&self, task_id: usize) -> Option<Arc<Mutex<WindowInner>>> {
        let check = |w: &Weak<Mutex<WindowInner>>| -> Option<Arc<Mutex<WindowInner>>> {
            let arc = w.upgrade()?;
            if arc.lock().task_id == Some(task_id) { Some(arc) } else { None }
        };
        if let Some(a) = self.active.upgrade() {
            if a.lock().task_id == Some(task_id) { return Some(a); }
        }
        self.show_list.iter().find_map(check)
            .or_else(|| self.hide_list.iter().find_map(check))
    }

    pub fn refresh_all(&mut self) -> Result<(), &'static str> {
        self.refresh_bottom_windows(None, true)
    }
}

// ── Initialisation ────────────────────────────────────────────────────────────

/// Initialises the window manager and returns `(keyboard_producer, mouse_producer)`.
pub fn init() -> Result<(Queue<Event>, Queue<Event>), &'static str> {
    mgi::init()?;
    let (width, height) = MGI.get().expect("MGI not initialised").lock().resolution();

    let mut bottom_fb = Framebuffer::new(width, height, None)?;
    let mut top_fb    = Framebuffer::new(width, height, None)?;
    bottom_fb.fill(color::LIGHT_GRAY.into());
    top_fb.fill(color::TRANSPARENT.into());

    let mouse = Coord { x: width as isize / 2, y: height as isize / 2 };

    WINDOW_MANAGER.call_once(|| Mutex::new(WindowManager {
        show_list: VecDeque::new(),
        hide_list: VecDeque::new(),
        active:    Weak::new(),
        mouse,
        mouse_btn_left:  false,
        mouse_btn_right: false,
        repositioned_border: None,
        bottom_fb,
        top_fb,
    }));

    {
        let mut wm = WINDOW_MANAGER.get().unwrap().lock();
        wm.refresh_bottom_windows(None, false)?;
        wm.move_mouse_to(mouse)?;
        wm.present();
    }

    // Create I/O queues and start the WM event loop.
    let key_consumer: Queue<Event>   = Queue::with_capacity(100);
    let key_producer                  = key_consumer.clone();
    let mouse_consumer: Queue<Event> = Queue::with_capacity(100);
    let mouse_producer                = mouse_consumer.clone();

    spawn::new_task_builder(window_manager_loop, (key_consumer, mouse_consumer))
        .name("window_manager_loop".to_string())
        .spawn()?;

    Ok((key_producer, mouse_producer))
}

// ── WM event loop ─────────────────────────────────────────────────────────────

fn window_manager_loop(
    (key_consumer, mouse_consumer): (Queue<Event>, Queue<Event>),
) -> Result<(), &'static str> {
    // The WM loop should not be preempted in the middle of a render.
    drop(preemption::hold_preemption());

    loop {
        let mut need_present = false;

        // Drain up to 16 keyboard events per iteration.
        for _ in 0..16 {
            match key_consumer.pop() {
                Some(Event::KeyboardEvent(ref e)) => {
                    handle_keyboard(e.key_event)?;
                    need_present = true;
                }
                _ => break,
            }
        }

        // Coalesce mouse movement: accumulate deltas, keep only the last event for button state.
        let mut dx = 0isize;
        let mut dy = 0isize;
        let mut last_mouse: Option<MouseEvent> = None;
        for _ in 0..32 {
            match mouse_consumer.pop() {
                Some(Event::MouseMovementEvent(ref m)) => {
                    dx += m.movement.x_movement as isize;
                    dy += m.movement.y_movement as isize;
                    last_mouse = Some(m.clone());
                }
                _ => break,
            }
        }

        if let Some(m) = last_mouse {
            if let Some(wm) = WINDOW_MANAGER.get() {
                let mut wm = wm.lock();
                wm.mouse_btn_left  = m.buttons.left();
                wm.mouse_btn_right = m.buttons.right();
                if dx != 0 || dy != 0 {
                    wm.move_mouse(Coord::new(dx, -dy))?;
                    need_present = true;
                }
            }
            handle_mouse(m)?;
        }

        if need_present {
            if let Some(wm) = WINDOW_MANAGER.get() {
                wm.lock().present();
            }
        }

        scheduler::schedule();
    }
}

fn handle_keyboard(key: KeyEvent) -> Result<(), &'static str> {
    let wm_ref = WINDOW_MANAGER.get().ok_or("window manager not initialised")?;

    // ── Global keyboard shortcuts ─────────────────────────────────────────────
    if key.modifiers.is_super_key() && key.action == KeyAction::Pressed {
        let (sw, sh) = wm_ref.lock().get_screen_size();
        let (w, h)   = (sw as isize, sh as isize);
        let snap: Option<Rectangle> = match key.keycode {
            Keycode::Left  => Some(Rectangle { top_left: Coord::new(0, 0),     bottom_right: Coord::new(w/2, h) }),
            Keycode::Right => Some(Rectangle { top_left: Coord::new(w/2, 0),   bottom_right: Coord::new(w, h)   }),
            Keycode::Up    => Some(Rectangle { top_left: Coord::new(0, 0),     bottom_right: Coord::new(w, h/2) }),
            Keycode::Down  => Some(Rectangle { top_left: Coord::new(0, h/2),   bottom_right: Coord::new(w, h)   }),
            _ => None,
        };
        if let Some(rect) = snap {
            let mut wm = wm_ref.lock();
            if let Some(active) = wm.get_active_window() {
                active.lock().resize(rect)?;
                wm.refresh_bottom_windows(None, true)?;
            }
            return Ok(());
        }
    }

    // Ctrl+Alt+T — open a new terminal.
    if key.modifiers.is_control() && key.modifiers.is_alt()
        && key.keycode == Keycode::T && key.action == KeyAction::Pressed
    {
        let ns   = mod_mgmt::create_application_namespace(None)?;
        let file = ns.dir().get_file_starting_with("shell-")
            .ok_or("shell application not found")?;
        let path = file.lock().get_absolute_path();
        spawn::new_application_task_builder(path.as_ref(), Some(ns))?
            .name("shell".to_string())
            .spawn()?;
        return Ok(());
    }

    // Pass everything else to the active window.
    if let Err(e) = wm_ref.lock().route_keyboard(key) {
        warn!("window_manager: could not route keyboard event: {}", e);
    }
    Ok(())
}

fn handle_mouse(event: MouseEvent) -> Result<(), &'static str> {
    let wm = WINDOW_MANAGER.get().ok_or("window manager not initialised")?.lock();
    // Errors here are expected when the cursor is over the desktop.
    let _ = wm.route_mouse(event);
    Ok(())
}