//! This crate acts as a manager of a list of windows. It defines a `WindowManager` structure and an instance of it. 
//!
//! A window manager holds a set of `WindowInner` objects, including an active window, a list of shown windows and a list of hidden windows. The hidden windows are totally overlapped by others.
//!
//! A window manager owns a bottom framebuffer and a top framebuffer. The bottom is the background of the desktop and the top framebuffer contains a floating window border and a mouse arrow. 
//! A window manager also contains a final framebuffer which is mapped to the screen. In refreshing an area, the manager will render all the framebuffers to the final one in order: bottom -> hide list -> showlist -> active -> top.
//!
//! The window manager provides methods to update within some bounding boxes rather than the whole screen for better performance.

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
use compositor::CompositableRegion;

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

/// The instance of the default window manager
pub static WINDOW_MANAGER: Once<Mutex<WindowManager>> = Once::new();

/// The width and height size of mouse in number of pixels.
const MOUSE_POINTER_SIZE_Y: usize = 18;
const MOUSE_POINTER_SIZE_X: usize = 11;
/// The mouse pointer image defined as a 2-D pixel array.
static MOUSE_POINTER_IMAGE: [[Color; MOUSE_POINTER_SIZE_Y]; MOUSE_POINTER_SIZE_X] = {
    const T: Color = color::TRANSPARENT;
    const C: Color = color::BLACK; // Cursor
    const B: Color = color::WHITE; // Border
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

// the border indicating new window position and size
const WINDOW_BORDER_SIZE: usize = 3;
// border's inner color
const WINDOW_BORDER_COLOR_INNER: Color = Color::new(0x005DADE2);

/// Window manager structure which maintains a list of windows and a mouse.
pub struct WindowManager {
    hide_list: VecDeque<Weak<Mutex<WindowInner>>>,
    show_list: VecDeque<Weak<Mutex<WindowInner>>>,
    active: Weak<Mutex<WindowInner>>,
    mouse: Coord,
    mouse_btn_left: bool,
    mouse_btn_right: bool,
    repositioned_border: Option<Rectangle>,
    bottom_fb: Framebuffer<AlphaPixel>,
    top_fb: Framebuffer<AlphaPixel>,
}

impl WindowManager {

    pub fn is_window_hidden(&self, task_id: u64) -> bool {
        let task_id = task_id as usize;
        if self.hide_list.iter().any(|w| w.upgrade().map_or(false, |w| w.lock().task_id == Some(task_id))) {
            return true;
        }
        if self.show_list.iter().any(|w| w.upgrade().map_or(false, |w| w.lock().task_id == Some(task_id))) {
            return false;
        }
        true
    }

    fn purge_expired_lists(&mut self) {
        self.show_list.retain(|w| w.upgrade().is_some());
        self.hide_list.retain(|w| w.upgrade().is_some());
    }

    fn pop_front_valid(list: &mut VecDeque<Weak<Mutex<WindowInner>>>) -> Option<Weak<Mutex<WindowInner>>> {
        while let Some(w) = list.pop_front() {
            if w.upgrade().is_some() {
                return Some(w);
            }
        }
        None
    }

    pub fn get_window_by_task_id(&self, task_id: usize) -> Option<Arc<Mutex<WindowInner>>> {
        if let Some(w) = self.active.upgrade() {
            if w.lock().task_id == Some(task_id) { return Some(w); }
        }
        for weak in &self.show_list {
            if let Some(w) = weak.upgrade() {
                if w.lock().task_id == Some(task_id) { return Some(w); }
            }
        }
        for weak in &self.hide_list {
            if let Some(w) = weak.upgrade() {
                if w.lock().task_id == Some(task_id) { return Some(w); }
            }
        }
        None
    }

    pub fn set_active(
        &mut self,
        inner_ref: &Arc<Mutex<WindowInner>>,
        refresh: bool,
    ) -> Result<bool, &'static str> {
        // 1. purge zombies
        self.show_list.retain(|w| w.upgrade().is_some());
        self.hide_list.retain(|w| w.upgrade().is_some());

        // 2. si déjà actif → juste refresh
        if let Some(current_active) = self.active.upgrade() {
            if Arc::ptr_eq(&current_active, inner_ref) {
                if refresh {
                    let area = inner_ref.lock().get_envelope();
                    self.refresh_bottom_windows(Some(area), true)?;
                }
                return Ok(true);
            }

            // déplacer ancien active vers show_list (UNE SEULE FOIS)
            if self.is_window_in_show_list(&current_active).is_none() {
                self.show_list.push_front(Arc::downgrade(&current_active));
            }
        }

        // 3. retirer target de partout
        if let Some(i) = self.is_window_in_show_list(inner_ref) {
            self.show_list.remove(i);
        }
        if let Some(i) = self.is_window_in_hide_list(inner_ref) {
            self.hide_list.remove(i);
        }

        // 4. set active
        self.active = Arc::downgrade(inner_ref);

        // 5. refresh
        if refresh {
            let area = inner_ref.lock().get_envelope();
            self.refresh_bottom_windows(Some(area), true)?;
        }

        Ok(false)
    }

    /// Returns the index of a window if it is in the show list
    fn is_window_in_show_list(&mut self, window: &Arc<Mutex<WindowInner>>) -> Option<usize> {
        for (i, item) in self.show_list.iter().enumerate() {
            if let Some(item_ptr) = item.upgrade() {
                if Arc::ptr_eq(&item_ptr, window) {
                    return Some(i);
                }
            }
        }
        None
    }

    /// Returns the index of a window if it is in the hide list
    fn is_window_in_hide_list(&mut self, window: &Arc<Mutex<WindowInner>>) -> Option<usize> {
        for (i, item) in self.hide_list.iter().enumerate() {
            if let Some(item_ptr) = item.upgrade() {
                if Arc::ptr_eq(&item_ptr, window) {
                    return Some(i);
                }
            }
        }
        None
    }

    /// delete a window and refresh its region
    pub fn delete_window(&mut self, inner_ref: &Arc<Mutex<WindowInner>>) -> Result<(), &'static str> {
        let (top_left, bottom_right) = {
            let inner = inner_ref.lock();
            let top_left = inner.get_position();
            let (width, height) = inner.get_size();
            let bottom_right = top_left + (width as isize, height as isize);
            (top_left, bottom_right)
        };
        let area = Some(Rectangle { top_left, bottom_right });

        // Purge expired entries first
        self.purge_expired_lists();

        // If deleted window is the active one, refresh and pick next visible (show_list) only.
        if let Some(current_active) = self.active.upgrade() {
            if Arc::ptr_eq(&current_active, inner_ref) {
                // redraw underlying windows excluding the active one
                self.refresh_bottom_windows(area, false)?;

                // promote next valid from show_list only (do NOT promote from hide_list)
                if let Some(next_weak) = WindowManager::pop_front_valid(&mut self.show_list) {
                    self.active = next_weak;
                } else {
                    self.active = Weak::new();
                }
                return Ok(());
            }
        }

        // If it's in show_list, remove it and refresh region
        if let Some(index) = self.is_window_in_show_list(inner_ref) {
            self.show_list.remove(index);
            self.refresh_windows(area)?;
            return Ok(());
        }

        // If it's in hide_list, remove it (no visual refresh needed)
        if let Some(index) = self.is_window_in_hide_list(inner_ref) {
            self.hide_list.remove(index);
            return Ok(());
        }

        Err("cannot find this window")
    }

    /// Hide a window
    pub fn hide_window(&mut self, inner_ref: &Arc<Mutex<WindowInner>>) -> Result<(), &'static str> {
        // Purge expired weak refs up-front
        self.purge_expired_lists();

        let is_active = if let Some(current_active) = self.active.upgrade() {
            Arc::ptr_eq(&current_active, inner_ref)
        } else {
            false
        };

        if is_active {
            // move current active to hidden
            // avoid duplicates in hide_list
            if self.is_window_in_hide_list(inner_ref).is_none() {
                self.hide_list.push_back(Arc::downgrade(inner_ref));
            }

            // promote next visible from show_list only, skipping expired
            if let Some(next) = WindowManager::pop_front_valid(&mut self.show_list) {
                self.active = next;
            } else {
                self.active = Weak::new();
            }

            let area = inner_ref.lock().get_envelope();
            self.refresh_bottom_windows(Some(area), true)?;
        } else if let Some(index) = self.is_window_in_show_list(inner_ref) {
            // Remove from show list, move to hide list
            self.show_list.remove(index);
            if self.is_window_in_hide_list(inner_ref).is_none() {
                self.hide_list.push_back(Arc::downgrade(inner_ref));
            }

            let area = inner_ref.lock().get_envelope();
            self.refresh_windows(Some(area))?;
        }

        Ok(())
    }

    /// Refresh the region in `_bounding_box`. Only render the bottom final framebuffer and windows. Ignore the active window if `active` is false.
    pub fn refresh_bottom_windows(
        &mut self,
        bounding_box: Option<Rectangle>,
        active: bool,
    ) -> Result<(), &'static str> {
        let mut window_ref_list = Vec::new();
        for window in &self.show_list {
            if let Some(window_ref) = window.upgrade() {
                window_ref_list.push(window_ref);
            }
        }
        if active {
            if let Some(window_ref) = self.active.upgrade() {
                window_ref_list.push(window_ref);
            }
        }

        let locked_window_list: Vec<_> = window_ref_list.iter().map(|x| x.lock()).collect();
        let mut mgi = MGI.get().expect("MGI not initialized").lock();
        let mut commands = Vec::new();

        if let Some(region) = bounding_box {
            // BlitRegion — on ne copie QUE la zone dirty, pas tout l'écran
            commands.push(DrawCommand::BlitRegion {
                src: self.bottom_fb.buffer(),
                src_width: self.bottom_fb.width(),
                dest_top_left: Coord::new(0, 0),
                region,
            });
            for window in &locked_window_list {
                commands.push(DrawCommand::BlitRegion {
                    src: window.framebuffer().buffer(),
                    src_width: window.framebuffer().width(),
                    dest_top_left: window.get_position(),
                    region,
                });
            }
        } else {
            // Blit complet — seulement pour l'init ou resize plein écran
            commands.push(DrawCommand::Blit {
                src: self.bottom_fb.buffer(),
                src_width: self.bottom_fb.width(),
                dest_top_left: Coord::new(0, 0),
            });
            for window in &locked_window_list {
                commands.push(DrawCommand::Blit {
                    src: window.framebuffer().buffer(),
                    src_width: window.framebuffer().width(),
                    dest_top_left: window.get_position(),
                });
            }
        }

        if let Some(region) = bounding_box {
            commands.push(DrawCommand::BlitRegion {
                src: self.top_fb.buffer(),
                src_width: self.top_fb.width(),
                dest_top_left: Coord::new(0, 0),
                region,
            });
        } else {
            commands.push(DrawCommand::Blit {
                src: self.top_fb.buffer(),
                src_width: self.top_fb.width(),
                dest_top_left: Coord::new(0, 0),
            });
        }

        mgi.submit(&commands);
        Ok(())
    }

    pub fn refresh_top(&mut self, bounding_box: Option<Rectangle>) -> Result<(), &'static str> {
        let mut mgi = MGI.get().expect("MGI not initialized").lock();
        if let Some(region) = bounding_box {
            mgi.submit(&[DrawCommand::BlitRegion {
                src: self.top_fb.buffer(),
                src_width: self.top_fb.width(),
                dest_top_left: Coord::new(0, 0),
                region,
            }]);
        } else {
            mgi.submit(&[DrawCommand::Blit {
                src: self.top_fb.buffer(),
                src_width: self.top_fb.width(),
                dest_top_left: Coord::new(0, 0),
            }]);
        }
        Ok(())
    }

    /// Refresh the part in `_bounding_box` of every window. `_bounding_box` is a region relative to the top-left of the screen. Refresh the whole screen if the bounding box is None.
    pub fn refresh_windows<B: CompositableRegion + Clone>(
        &mut self, 
        _bounding_box: impl IntoIterator<Item = B> + Clone,
    ) -> Result<(), &'static str> {
        // reference of windows
        let mut window_ref_list = Vec::new();
        for window in &self.show_list {
            if let Some(window_ref) = window.upgrade() {
                window_ref_list.push(window_ref);
            }
        }

        if let Some(window_ref) = self.active.upgrade() {
            window_ref_list.push(window_ref)
        }

        // lock windows
        let locked_window_list = &window_ref_list.iter().map(|x| x.lock()).collect::<Vec<_>>();
        // create updated framebuffer info objects
        let mut mgi = MGI.get().expect("MGI not initialized").lock();
        let mut commands = Vec::new();
        for window in locked_window_list {
            commands.push(DrawCommand::Blit { src: window.framebuffer().buffer(), src_width: window.framebuffer().width(), dest_top_left: window.get_position() });
        }
        mgi.submit(&commands);
        Ok(())
    }


    /// Refresh the part in `_bounding_box` of the active window. `_bounding_box` is a region relative to the top-left of the screen. Refresh the whole screen if the bounding box is None.
    pub fn refresh_active_window(&mut self, _bounding_box: Option<Rectangle>) -> Result<(), &'static str> {
        if let Some(window_ref) = self.active.upgrade() {
            let window = window_ref.lock();
            let mut mgi = MGI.get().expect("MGI not initialized").lock();
            mgi.submit(&[DrawCommand::Blit {
                src: window.framebuffer().buffer(),
                src_width: window.framebuffer().width(),
                dest_top_left: window.get_position(),
            }]);
            Ok(())
        } else {
            Ok(())
        } 
    }
    
    pub fn refresh_bottom_region(&mut self, region: Rectangle) -> Result<(), &'static str> {
        let mut mgi = MGI.get().expect("MGI not initialized").lock();
        mgi.submit(&[DrawCommand::BlitRegion {
            src: self.bottom_fb.buffer(),
            src_width: self.bottom_fb.width(),
            dest_top_left: Coord::new(0, 0),
            region,
        }]);
        mgi.submit(&[DrawCommand::BlitRegion {
            src: self.top_fb.buffer(),
            src_width: self.top_fb.width(),
            dest_top_left: Coord::new(0, 0),
            region,
        }]);
        Ok(())
    }

    /// Passes the given keyboard event to the currently active window.
    fn pass_keyboard_event_to_window(&self, key_event: KeyEvent) -> Result<(), &'static str> {
        let active_window = self.active.upgrade().ok_or("no window was set as active to receive a keyboard event")?;
        active_window.lock().send_event(Event::new_keyboard_event(key_event))
            .map_err(|_e| "Failed to enqueue the keyboard event; window event queue was full.")?;
        Ok(())
    }

    /// Passes the given mouse event to the window that the mouse is currently over. 
    /// 
    /// If the mouse is not over any window, an error is returned; 
    /// however, this error is quite common and expected when the mouse is not positioned within a window,
    /// and is not a true failure. 
    fn pass_mouse_event_to_window(&self, mouse_event: MouseEvent) -> Result<(), &'static str> {
        let coordinate = { &self.mouse };
        let mut event: MousePositionEvent = MousePositionEvent {
            coordinate: Coord::new(0, 0),
            gcoordinate: *coordinate,
            scrolling_up: mouse_event.movement.scroll_movement > 0, //TODO: might be more beneficial to save scroll_movement here
            scrolling_down: mouse_event.movement.scroll_movement < 0, //FIXME: also might be the wrong way around
            left_button_hold: mouse_event.buttons.left(),
            right_button_hold: mouse_event.buttons.right(),
            fourth_button_hold: mouse_event.buttons.fourth(),
            fifth_button_hold: mouse_event.buttons.fifth(),
        };

        // TODO: FIXME:  improve this logic to just send the mouse event to the top-most window in the entire WM list,
        //               not just necessarily the active one. (For example, scroll wheel events can be sent to non-active windows).

        // first check the active one
        if let Some(current_active) = self.active.upgrade() {
            let current_active_win = current_active.lock();
            let current_coordinate = current_active_win.get_position();
            if current_active_win.contains(*coordinate - current_coordinate) || matches!(current_active_win.moving, WindowMovingStatus::Moving(_))
            {
                event.coordinate = *coordinate - current_coordinate;
                // debug!("pass to active: {}, {}", event.x, event.y);
                current_active_win.send_event(Event::MousePositionEvent(event))
                    .map_err(|_e| "Failed to enqueue the mouse event; window event queue was full.")?;
                return Ok(());
            }
        }

        // TODO FIXME: (kevinaboos): the logic below here is actually incorrect -- it could send mouse events to an invisible window below others.

        // then check show_list
        for i in 0..self.show_list.len() {
            if let Some(now_inner_mutex) = self.show_list[i].upgrade() {
                let now_inner = now_inner_mutex.lock();
                let current_coordinate = now_inner.get_position();
                if now_inner.contains(*coordinate - current_coordinate) {
                    event.coordinate = *coordinate - current_coordinate;
                    now_inner.send_event(Event::MousePositionEvent(event))
                        .map_err(|_e| "Failed to enqueue the mouse event; window event queue was full.")?;
                    return Ok(());
                }
            }
        }

        Err("the mouse position does not fall within the bounds of any window")
    }

    /// Refresh the floating border, which is used to show the outline of a window while it is being moved. 
    /// `show` indicates whether to show the border or not.
    /// `new_border` defines the rectangular outline of the border.
    fn refresh_floating_border(
        &mut self,
        show: bool,
        new_border: Rectangle,
    ) -> Result<(), &'static str> {
        // first clear old border if exists
        if let Some(border) = self.repositioned_border {
            self.draw_floating_border(&border, color::TRANSPARENT);
            self.refresh_bottom_windows(Some(border), true)?;
        }

        // then draw current border
        if show {
            self.draw_floating_border(&new_border, WINDOW_BORDER_COLOR_INNER);
            self.refresh_top(Some(new_border))?;
            self.repositioned_border = Some(new_border);
        } else {
            self.repositioned_border = None;
        }

        Ok(())
    }

    /// draw the floating border with `pixel`. Return the list of coordinates of pixels that were updated.
    /// `border` indicates the position of the border as a rectangle.
    /// `color` is the color of the floating border.
    fn draw_floating_border(&mut self, border: &Rectangle, color: Color) -> Vec<Coord> {
        let mut coordinates = Vec::new();
        let pixel = color.into();
        for i in 0..(WINDOW_BORDER_SIZE) as isize {
            let width = (border.bottom_right.x - border.top_left.x) - 2 * i;
            let height = (border.bottom_right.y - border.top_left.y) - 2 * i;
            let coordinate = border.top_left + (i, i);
            if width <= 0 || height <= 0 {
                break;
            }
            framebuffer_drawer::draw_rectangle(
                &mut self.top_fb, 
                coordinate, 
                width as usize, 
                height as usize, 
                pixel
            );

            for m in 0..width {
                coordinates.push(coordinate + (m, 0));
                coordinates.push(coordinate + (m, height));
            }            
            
            for m in 1..height - 1 {
                coordinates.push(coordinate + (0, m));
                coordinates.push(coordinate + (width, m));
            }            
        }

        coordinates
    }

    /// take active window's base position and current mouse, move the window with delta
    pub fn move_active_window(&mut self) -> Result<(), &'static str> {
        if let Some(current_active) = self.active.upgrade() {
            let border = Rectangle { 
                top_left: Coord::new(0, 0), 
                bottom_right: Coord::new(0, 0) 
            };
            self.refresh_floating_border(false, border)?;

            let (old_top_left, old_bottom_right, new_top_left, new_bottom_right) = {
                let mut current_active_win = current_active.lock();
                let (current_x, current_y) = {
                    let m = &self.mouse;
                    (m.x, m.y)
                };
                match current_active_win.moving {
                    WindowMovingStatus::Moving(base) => {
                        let old_top_left = current_active_win.get_position();
                        let new_top_left = old_top_left + ((current_x - base.x), (current_y - base.y));
                        let (width, height) = current_active_win.get_size();
                        let old_bottom_right = old_top_left + (width as isize, height as isize);
                        let new_bottom_right = new_top_left + (width as isize, height as isize);
                        current_active_win.set_position(new_top_left);
                        (old_top_left, old_bottom_right, new_top_left, new_bottom_right)        
                    },
                    WindowMovingStatus::Resizing(base, edge) => {
                        let old_top_left = current_active_win.get_position();
                        let (w, h) = current_active_win.get_size();
                        let old_bottom_right = old_top_left + (w as isize, h as isize);
                        
                        let dx = current_x - base.x;
                        let dy = current_y - base.y;
                        
                        let mut new_rect = Rectangle { top_left: old_top_left, bottom_right: old_bottom_right };
                        
                        match edge {
                            ResizeEdge::Right => new_rect.bottom_right.x += dx,
                            ResizeEdge::Bottom => new_rect.bottom_right.y += dy,
                            ResizeEdge::Left => new_rect.top_left.x += dx,
                            ResizeEdge::Top => new_rect.top_left.y += dy,
                            ResizeEdge::BottomRight => { new_rect.bottom_right.x += dx; new_rect.bottom_right.y += dy; },
                            ResizeEdge::BottomLeft => { new_rect.top_left.x += dx; new_rect.bottom_right.y += dy; },
                            ResizeEdge::TopRight => { new_rect.bottom_right.x += dx; new_rect.top_left.y += dy; },
                            ResizeEdge::TopLeft => { new_rect.top_left.x += dx; new_rect.top_left.y += dy; },
                        }

                        // Minimal size constraint
                        if new_rect.width() < 50 { new_rect.bottom_right.x = new_rect.top_left.x + 50; }
                        if new_rect.height() < 50 { new_rect.bottom_right.y = new_rect.top_left.y + 50; }

                        // Apply resize
                        current_active_win.resize(new_rect)?;

                        (old_top_left, old_bottom_right, new_rect.top_left, new_rect.bottom_right)
                    },
                    WindowMovingStatus::Stationary => {
                        return Err("The window is not moving");
                    }
                }
            };
            self.refresh_bottom_windows(Some(Rectangle{top_left: old_top_left, bottom_right: old_bottom_right}), false)?;

            self.refresh_active_window(Some(Rectangle{top_left: new_top_left, bottom_right: new_bottom_right}))?;
            self.refresh_mouse()?;
        } else {
            return Err("cannot find active window to move");
        }
        Ok(())
    }

    /// Refresh the mouse display
    pub fn refresh_mouse(&mut self) -> Result<(), &'static str> {
        let _bounding_box = Some(Rectangle {
            top_left: self.mouse,
            bottom_right: self.mouse + (MOUSE_POINTER_SIZE_X as isize, MOUSE_POINTER_SIZE_Y as isize)
        });
        self.refresh_top(_bounding_box)
    }

    /// Move mouse. `relative` indicates the new position relative to current position.
    fn move_mouse(&mut self, relative: Coord) -> Result<(), &'static str> {
        let old = self.mouse;
        let mut new = old + relative;
        
        let (screen_width, screen_height) = self.get_screen_size();
        if new.x < 0 {
            new.x = 0;
        }
        if new.y < 0 {
            new.y = 0;
        }

        // keep mouse pointer border in the screen when it is at the right or bottom side.
        const MOUSE_POINTER_BORDER: isize = 3;
        new.x = core::cmp::min(new.x, screen_width as isize - MOUSE_POINTER_BORDER);
        new.y = core::cmp::min(new.y, screen_height as isize - MOUSE_POINTER_BORDER);
            
        self.move_mouse_to(new)
    }
    // Move mouse to absolute position `new`
    fn move_mouse_to(&mut self, new: Coord) -> Result<(), &'static str> {
        let old = self.mouse;

        // 1. Effacer ancien curseur dans top_fb
        let (fb_w, fb_h) = self.top_fb.get_size();
        let transparent_pixel: AlphaPixel = color::TRANSPARENT.into();
        {
            let buf = self.top_fb.buffer_mut();
            for row in 0..MOUSE_POINTER_SIZE_Y {
                let y = old.y + row as isize;
                if y < 0 || y >= fb_h as isize { continue; }
                let x_start = core::cmp::max(0, old.x) as usize;
                let x_end = core::cmp::min(fb_w as isize, old.x + MOUSE_POINTER_SIZE_X as isize) as usize;
                if x_start < x_end {
                    buf[y as usize * fb_w + x_start .. y as usize * fb_w + x_end].fill(transparent_pixel);
                }
            }
        }

        // 2. Dessiner nouveau curseur dans top_fb
        self.mouse = new;
        {
            let buf = self.top_fb.buffer_mut();
            for col in 0..MOUSE_POINTER_SIZE_X {
                for row in 0..MOUSE_POINTER_SIZE_Y {
                    let x = new.x + col as isize;
                    let y = new.y + row as isize;
                    if x < 0 || y < 0 || x >= fb_w as isize || y >= fb_h as isize { continue; }
                    let color = MOUSE_POINTER_IMAGE[col][row];
                    if color.transparency() != 255 {
                        buf[y as usize * fb_w + x as usize] = color.into();
                    }
                }
            }
        }

        // 3. Dirty rect couvrant old + new
        let dirty = Rectangle {
            top_left: Coord::new(
                core::cmp::min(old.x, new.x),
                core::cmp::min(old.y, new.y),
            ),
            bottom_right: Coord::new(
                core::cmp::max(old.x, new.x) + MOUSE_POINTER_SIZE_X as isize,
                core::cmp::max(old.y, new.y) + MOUSE_POINTER_SIZE_Y as isize,
            ),
        };

        
        // On récupère les pointeurs (Arc) et on les fait vivre jusqu'à la fin de la fonction
        let window_ref_list: Vec<_> = self.show_list.iter()
            .filter_map(|w| w.upgrade())
            .collect();
            
        // NOUVEAU : On stocke l'Arc de la fenêtre active en dehors du `if`
        let active_ref_opt = self.active.upgrade();

        // Ces variables DOIVENT être déclarées avant `commands`
        let mut locked_windows = Vec::new();
        for window_ref in &window_ref_list {
            locked_windows.push(window_ref.lock());
        }

        let mut active_window_guard = None;
        // NOUVEAU : On emprunte l'Arc avec `&` pour ne pas le consommer
        if let Some(active_ref) = &active_ref_opt {
            active_window_guard = Some(active_ref.lock());
        }
        
        // --- Création des commandes de dessin ---
        let mut commands = Vec::new();

        // Background
        commands.push(DrawCommand::BlitRegion {
            src: self.bottom_fb.buffer(),
            src_width: self.bottom_fb.width(),
            dest_top_left: Coord::new(0, 0),
            region: dirty,
        });

        // Fenêtres dans la zone dirty
        for window in &locked_windows {
            commands.push(DrawCommand::BlitRegion {
                src: window.framebuffer().buffer(),
                src_width: window.framebuffer().width(),
                dest_top_left: window.get_position(),
                region: dirty,
            });
        }

        if let Some(window) = &active_window_guard {
            commands.push(DrawCommand::BlitRegion {
                src: window.framebuffer().buffer(),
                src_width: window.framebuffer().width(),
                dest_top_left: window.get_position(),
                region: dirty,
            });
        }

        // Curseur (top layer)
        commands.push(DrawCommand::BlitRegion {
            src: self.top_fb.buffer(),
            src_width: self.top_fb.width(),
            dest_top_left: Coord::new(0, 0),
            region: dirty,
        });

        let mut mgi = MGI.get().ok_or("MGI not initialized")?.lock();
        mgi.submit(&commands);
        mgi.present();
        Ok(())
    }

    pub fn mouse_position(&self) -> Coord { self.mouse }
    pub fn mouse_left(&self) -> bool { self.mouse_btn_left }
    pub fn mouse_right(&self) -> bool { self.mouse_btn_right }

    pub fn move_floating_border(&mut self) -> Result<(), &'static str> {
        let (new_x, new_y) = {
            let m = &self.mouse;
            (m.x, m.y)
        };
        
        if let Some(current_active) = self.active.upgrade() {
            let (is_draw, border_start, border_end) = {
                let current_active_win = current_active.lock();
                match current_active_win.moving {
                    WindowMovingStatus::Moving(base) => {
                        // move this window
                        // for better performance, while moving window, only border is shown for indication
                        let coordinate = current_active_win.get_position();
                        // let (current_x, current_y) = (coordinate.x, coordinate.y);
                        let (width, height) = current_active_win.get_size();
                        let border_start = coordinate + (new_x - base.x, new_y - base.y);
                        let border_end = border_start + (width as isize, height as isize);
                        (true, border_start, border_end)
                    }
                    WindowMovingStatus::Resizing(base, edge) => {
                        let coordinate = current_active_win.get_position();
                        let (width, height) = current_active_win.get_size();
                        let dx = new_x - base.x;
                        let dy = new_y - base.y;

                        let mut start = coordinate;
                        let mut end = coordinate + (width as isize, height as isize);

                        match edge {
                            ResizeEdge::Right => end.x += dx,
                            ResizeEdge::Bottom => end.y += dy,
                            ResizeEdge::Left => start.x += dx,
                            ResizeEdge::Top => start.y += dy,
                            ResizeEdge::BottomRight => { end.x += dx; end.y += dy; },
                            ResizeEdge::BottomLeft => { start.x += dx; end.y += dy; },
                            ResizeEdge::TopRight => { end.x += dx; start.y += dy; },
                            ResizeEdge::TopLeft => { start.x += dx; start.y += dy; },
                        }
                        // Minimal visual constraint for the border
                        if end.x - start.x < 50 { end.x = start.x + 50; }
                        if end.y - start.y < 50 { end.y = start.y + 50; }

                        (true, start, end)
                    }
                    WindowMovingStatus::Stationary => (false, Coord::new(0, 0), Coord::new(0, 0)),
                }
            };
            let border = Rectangle {
                top_left: border_start,
                bottom_right: border_end,
            };
            self.refresh_floating_border(is_draw, border)?;
        } else {
            let border = Rectangle {
                top_left: Coord::new(0, 0),
                bottom_right: Coord::new(0, 0),
            };
            self.refresh_floating_border(false, border)?;
        }

        Ok(())
    }

    /// Presents the back buffer to the screen by copying it to the final framebuffer.
    pub fn present(&mut self) {
        MGI.get().expect("MGI not initialized").lock().present();
    }

    /// Returns true if the given `window` is the currently active window.
    pub fn is_active(&self, window: &Arc<Mutex<WindowInner>>) -> bool {
        self.active.upgrade()
            .map(|active| Arc::ptr_eq(&active, window))
            .unwrap_or(false)
    }

    /// Returns the `(width, height)` in pixels of the screen itself (the final framebuffer).
    pub fn get_screen_size(&self) -> (usize, usize) {
        MGI.get().expect("MGI not initialized").lock().resolution()
    }

    /// Get a mutable reference to the bottom framebuffer (desktop layer).
    /// This allows applications to draw the wallpaper/desktop background directly.
    /// WARNING: Direct manipulation of this framebuffer bypasses the window manager's 
    /// refresh tracking. Call `refresh_bottom_area()` after modifying to see changes.
    pub fn get_bottom_framebuffer_mut(&mut self) -> &mut Framebuffer<AlphaPixel> {
        &mut self.bottom_fb
    }

    /// Get an immutable reference to the bottom framebuffer.
    pub fn get_bottom_framebuffer(&self) -> &Framebuffer<AlphaPixel> {
        &self.bottom_fb
    }

    /// Refresh the entire bottom framebuffer area on the final screen.
    /// Call this after directly modifying `get_bottom_framebuffer_mut()`.
    pub fn refresh_bottom_area(&mut self) -> Result<(), &'static str> {
        self.refresh_bottom_windows(Option::<Rectangle>::None, false)
    }

    pub fn refresh_all(&mut self) -> Result<(), &'static str> {
        self.refresh_bottom_windows(Option::<Rectangle>::None, true)
    }

    pub fn get_active_window(&self) -> Option<Arc<Mutex<WindowInner>>> {
        self.active.upgrade()
    }

}

/// Initialize the window manager. It returns (keyboard_producer, mouse_producer) for the I/O devices.
pub fn init() -> Result<(Queue<Event>, Queue<Event>), &'static str> {
    mgi::init()?;
    let (width, height) = MGI.get().expect("MGI not initialized").lock().resolution();

    let mut bottom_fb = Framebuffer::new(width, height, None)?;
    let mut top_fb = Framebuffer::new(width, height, None)?;
    bottom_fb.fill(color::LIGHT_GRAY.into());
    top_fb.fill(color::TRANSPARENT.into());

    let mouse = Coord {
        x: width as isize / 2,
        y: height as isize / 2,
    };

    let window_manager = WindowManager {
        hide_list: VecDeque::new(),
        show_list: VecDeque::new(),
        active: Weak::new(),
        mouse,
        mouse_btn_left: false,
        mouse_btn_right: false,
        repositioned_border: None,
        bottom_fb,
        top_fb,
    };
    WINDOW_MANAGER.call_once(|| Mutex::new(window_manager));

    {
        let mut wm = WINDOW_MANAGER.get().unwrap().lock();
        wm.refresh_bottom_windows(Option::<Rectangle>::None, false)?;
        wm.move_mouse_to(mouse)?;
        wm.present();
    }

    // queues...
    let key_consumer: Queue<Event> = Queue::with_capacity(100);
    let key_producer = key_consumer.clone();
    let mouse_consumer: Queue<Event> = Queue::with_capacity(100);
    let mouse_producer = mouse_consumer.clone();

    spawn::new_task_builder(window_manager_loop, (key_consumer, mouse_consumer))
        .name("window_manager_loop".to_string())
        .spawn()?;

    Ok((key_producer, mouse_producer))
}

// fn rdtsc() -> u64 {
//     unsafe {
//         let lo: u32;
//         let hi: u32;
//         core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi);
//         ((hi as u64) << 32) | lo as u64
//     }
// }

/// handles all keyboard and mouse movement in this window manager
fn window_manager_loop(
    (key_consumer, mouse_consumer): (Queue<Event>, Queue<Event>),
) -> Result<(), &'static str> {
    drop(preemption::hold_preemption());
    let mut idle_counter: usize = 0;
    loop {
        let mut need_present = false;

        for _ in 0..16 {
            match key_consumer.pop() {
                Some(Event::KeyboardEvent(ref e)) => {
                    keyboard_handle_application(e.key_event)?;
                    need_present = true;
                }
                _ => break,
            }
        }

        let mut dx = 0isize;
        let mut dy = 0isize;
        let mut last_mouse: Option<MouseEvent> = None;
        for _ in 0..32 {
            match mouse_consumer.pop() {
                Some(Event::MouseMovementEvent(ref m)) => {
                    dx = dx.saturating_add(m.movement.x_movement as isize);
                    dy = dy.saturating_add(m.movement.y_movement as isize);
                    last_mouse = Some(m.clone());
                }
                _ => break,
            }
        }

        if let Some(mouse_event) = last_mouse {
            if let Some(wm) = WINDOW_MANAGER.get() {
                let mut wm = wm.lock();
                wm.mouse_btn_left  = mouse_event.buttons.left();
                wm.mouse_btn_right = mouse_event.buttons.right();
            }
            if dx != 0 || dy != 0 {
                WINDOW_MANAGER
                    .get()
                    .ok_or("WM not init")?
                    .lock()
                    .move_mouse(Coord::new(dx, -dy))?;
                need_present = true;
            }
            cursor_handle_application(mouse_event)?;
        }

        // Forcer un refresh toutes les ~500 itérations même sans input
        // pour que les apps qui redessinent seules soient affichées
        idle_counter += 1;
        if idle_counter >= 500 {
            idle_counter = 0;
            need_present = true;
        }

        if need_present {
            if let Some(wm) = WINDOW_MANAGER.get() {
                wm.lock().present();
            }
        }

        scheduler::schedule();
    }
}

/// handle keyboard event, push it to the active window if one exists
fn keyboard_handle_application(key_input: KeyEvent) -> Result<(), &'static str> {
    let win_mgr = WINDOW_MANAGER.get().ok_or("The window manager was not yet initialized")?;
    
    // First, we handle keyboard shortcuts understood by the window manager.
    
    // "Super + Arrow" will resize and move windows to the specified half of the screen (left, right, top, or bottom)
    if key_input.modifiers.is_super_key() && key_input.action == KeyAction::Pressed {
        let screen_dimensions = win_mgr.lock().get_screen_size();
        let (width, height) = (screen_dimensions.0 as isize, screen_dimensions.1 as isize);
        let new_position: Option<Rectangle> = match key_input.keycode {
            Keycode::Left => Some(Rectangle {
                top_left:     Coord { x: 0, y: 0 },
                bottom_right: Coord { x: width / 2, y: height },
            }),
            Keycode::Right => Some(Rectangle {
                top_left:     Coord { x: width / 2, y: 0 },
                bottom_right: Coord { x: width, y: height },
            }),
            Keycode::Up => Some(Rectangle {
                top_left:     Coord { x: 0, y: 0 },
                bottom_right: Coord { x: width, y: height / 2 },
            }),
            Keycode::Down => Some(Rectangle {
                top_left:     Coord { x: 0, y: height / 2 },
                bottom_right: Coord { x: width, y: height },
            }),
            _ => None,
        };
        
        if let Some(position) = new_position {
            let mut wm = win_mgr.lock();
            if let Some(active_window) = wm.active.upgrade() {
                debug!("window_manager: resizing active window to {:?}", new_position);
                active_window.lock().resize(position)?;

                wm.refresh_bottom_windows(Option::<Rectangle>::None, true)?;
            }
        }

        return Ok(());
    }

    // Spawn a new terminal via Ctrl+Alt+T
    if key_input.modifiers.is_control()
        && key_input.modifiers.is_alt()
        && key_input.keycode == Keycode::T
        && key_input.action == KeyAction::Pressed
    {
        // Because this task (the window manager loop) runs in a kernel-only namespace,
        // we have to create a new application namespace in order to be able to actually spawn a shell.

        let new_app_namespace = mod_mgmt::create_application_namespace(None)?;
        let shell_objfile = new_app_namespace.dir().get_file_starting_with("shell-")
            .ok_or("Couldn't find shell application file to run upon Ctrl+Alt+T")?;
        let path = shell_objfile.lock().get_absolute_path();
        spawn::new_application_task_builder(path.as_ref(), Some(new_app_namespace))?
            .name("shell".to_string())
            .spawn()?;

        debug!("window_manager: spawned new shell app in new app namespace.");
        return Ok(());
    }

    // Any keyboard event unhandled above should be passed to the active window.
    if let Err(_e) = win_mgr.lock().pass_keyboard_event_to_window(key_input) {
        warn!("window_manager: failed to pass keyboard event to active window. Error: {:?}", _e);
    }
    Ok(())
}

/// handle mouse event, push it to related window or anyone asked for it
fn cursor_handle_application(mouse_event: MouseEvent) -> Result<(), &'static str> {
    let wm = WINDOW_MANAGER.get().ok_or("The static window manager was not yet initialized")?.lock();
    if wm.pass_mouse_event_to_window(mouse_event).is_err() {
    }
    Ok(())
}
