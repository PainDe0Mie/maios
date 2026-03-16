//! A `Window` object should be owned by an application. It can display a `Displayable` object in its framebuffer. See `applications/new_window` as a demo to use this library.
//!
//! This library will create a window with default title bar and border. It handles the commonly used interactions like moving
//! the window or close the window. Also, it is responsible to show title bar differently when window is active. 
//!
//! A window can render itself to the screen via a window manager. The window manager will compute the bounding box of the updated part and composites it with other existing windows according to their order.
//!
//! The library
//! frees applications from handling the complicated interaction with window manager, however, advanced users could learn from
//! this library about how to use window manager APIs directly.
//!

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
use window_inner::{WindowInner, WindowMovingStatus, ResizeEdge, DEFAULT_BORDER_SIZE, DEFAULT_TITLE_BAR_HEIGHT};
use window_manager::{WINDOW_MANAGER};

const WINDOW_TITLE_HIGHLIGHT: Color = Color::new(0x002F334D); // Légère surbrillance

const WINDOW_RADIUS: usize = 8;
// const WINDOW_BORDER_COLOR_INACTIVE: Color  = Color::new(0x00292E42); // gris foncé
// const WINDOW_BORDER_COLOR_ACTIVE_TOP: Color    = Color::new(0x001A1B26); // quasi-noir
// const WINDOW_BORDER_COLOR_ACTIVE_BOTTOM: Color = Color::new(0x001A1B26);
// const WINDOW_BORDER_ACCENT: Color          = Color::new(0x007AA2F7); // bleu tokyonight
// const WINDOW_BUTTON_COLOR_CLOSE: Color     = Color::new(0x00F7768E); // rouge tokyonight
// const WINDOW_BUTTON_COLOR_NORMAL: Color    = Color::new(0x00000000);
// const WINDOW_BUTTON_COLOR_HOVER: Color     = Color::new(0x00414868);

// Button dimensions
const WINDOW_BUTTON_WIDTH: usize = 14;
const WINDOW_BUTTON_HEIGHT: usize = 14;
const WINDOW_BUTTON_SPACING: usize = 8;
const WINDOW_BUTTON_MARGIN_RIGHT: usize = 12;

// Resize margin
const RESIZE_MARGIN: usize = 5;

// The buttons shown in title bar
#[derive(Copy, Clone)]
enum TopButton {
    Close,
    MinimizeMaximize,
    Hide,
}

impl From<usize> for TopButton {
    fn from(item: usize) -> Self {
        match item {
            0 => TopButton::Close,
            1 => TopButton::MinimizeMaximize,
            2 => TopButton::Hide,
            _ => TopButton::Close,
        }
    }
}


/// This struct is the application-facing representation of a window.
/// 
pub struct Window {
    /// The system-facing inner representation of this window.
    /// The window manager interacts with this object directly;
    /// thus, applications should not be able to access this directly. 
    /// 
    /// This is wrapped in an `Arc` such that the window manager can hold `Weak` references to it.
    inner: Arc<Mutex<WindowInner>>,
    /// The event queue
    event_consumer: Queue<Event>,
    /// last mouse position event, used to judge click and press-moving event
    /// TODO FIXME (kevinaboos): why is mouse-specific stuff here? 
    last_mouse_position_event: MousePositionEvent,
    /// record last result of whether this window is active, to reduce redraw overhead
    last_is_active: bool,
    /// The window title
    #[allow(dead_code)]
    title: String,
    /// The bounds to restore to when un-maximizing
    restore_bounds: Option<Rectangle>,
}

impl Window {
    /// Creates a new window to be displayed on screen. 
    /// 
    /// The given `framebuffer` will be filled with the `initial_background` color.
    /// 
    /// The newly-created `Window` will be set as the "active" window that has current focus. 
    /// 
    /// # Arguments: 
    /// * `coordinate`: the position of the window relative to the top-left corner of the screen.
    /// * `width`, `height`: the dimensions of the window in pixels.
    /// * `initial_background`: the default color of the window.
    pub fn new(
        coordinate: Coord,
        width: usize,
        height: usize,
        initial_background: Color,
    ) -> Result<Window, &'static str> {
        Self::with_title(String::from("Mai Window"), coordinate, width, height, initial_background)
    }

    /// Creates a new window with a specific title.
    pub fn with_title(
        title: String,
        coordinate: Coord,
        width: usize,
        height: usize,
        initial_background: Color,
    ) -> Result<Window, &'static str> {
        debug!("Window::with_title: creating window at {:?} size {}x{}", coordinate, width, height);
        let wm_ref = window_manager::WINDOW_MANAGER.get().ok_or("The window manager is not initialized")?;

        // Create a new virtual framebuffer to hold this window's contents only,
        // and fill it with the initial background color.
        let mut framebuffer = Framebuffer::new(width, height, None)?;
        framebuffer.fill(initial_background.into());
        let (width, height) = framebuffer.get_size();

        // TODO: FIXME: (kevinaboos) this condition seems wrong... at least the first conditional does.
        if width <= 2 * DEFAULT_TITLE_BAR_HEIGHT || height <= DEFAULT_TITLE_BAR_HEIGHT + DEFAULT_BORDER_SIZE {
            return Err("window dimensions must be large enough for the title bar and borders to be drawn");
        }

        // Create an event queue to allow the window manager to pass events to this `Window` via its `WindowInner` instance,
        // and to allow applications to receive events from this `Window` object itself.
        let event_consumer = Queue::with_capacity(100);
        let event_producer = event_consumer.clone();

        let current_task_id = task::get_my_current_task_id(); 
        
        let window_inner = WindowInner::new(coordinate, framebuffer, event_producer, Some(current_task_id));
        
        let mut window = Window {
            inner: Arc::new(Mutex::new(window_inner)),
            event_consumer,
            last_mouse_position_event: MousePositionEvent::default(),
            last_is_active: true, // new window is now set as the active window by default 
            title,
            restore_bounds: None,
        };

        // Draw the actual window frame, the title bar and borders.
        window.draw_border(true);
        {
            let mut inner = window.inner.lock();
            window.show_button(TopButton::Close, 1, &mut inner);
            window.show_button(TopButton::MinimizeMaximize, 1, &mut inner);
            window.show_button(TopButton::Hide, 1, &mut inner);
        }

        let mut wm = wm_ref.lock();
        wm.set_active(&window.inner, false)?;

        let area = window.inner.lock().get_envelope();
        wm.refresh_bottom_windows(Some(area), true)?;
        
        Ok(window)
    }


    /// Tries to receive an `Event` that has been sent to this `Window`.
    /// If no events exist on the queue, it returns `Ok(None)`. 
    /// 
    /// "Internal" events will be automatically handled rather than returned. 
    /// If an error occurs while obtaining the event (or when handling internal events),
    ///
    /// Otherwise, the event at the front of this window's event queue will be popped off and returned. 
    pub fn handle_event(&mut self) -> Result<Option<Event>, &'static str> {
        let mut call_later_do_refresh_floating_border = false;
        let mut call_later_do_move_active_window = false;
        let mut need_to_set_active = false;
        let mut need_refresh_three_button = false;
        let mut call_later_do_hide_window = false;

        let wm_ref = window_manager::WINDOW_MANAGER.get().ok_or("The window manager is not initialized")?;

        let (screen_width, screen_height) = wm_ref.lock().get_screen_size(); // Get screen size once
        
        let is_active = {
            let wm = wm_ref.lock();
            wm.is_active(&self.inner)
        };
        if is_active != self.last_is_active {
            self.draw_border(is_active);
            self.last_is_active = is_active;
            let mut inner = self.inner.lock();
            self.show_button(TopButton::Close, 1, &mut inner);
            self.show_button(TopButton::MinimizeMaximize, 1, &mut inner);
            self.show_button(TopButton::Hide, 1, &mut inner);
        }

        // If we cannot handle this event as an "internal" event (e.g., clicking on the window title bar or border),
        // we simply return that event from this function such that the application can handle it. 
        let mut unhandled_event: Option<Event> = None;

        
        while let Some(event) = self.event_consumer.pop() {
            // TODO FIXME: for a performant design, the goal is to AVOID holding the lock on `inner` as much as possible. 
            //             That means that most of the drawing logic should be moved into the `window_inner` crate itself.            
            let mut inner = self.inner.lock();
            let (width, height) = inner.get_size();

            match event {
                Event::MousePositionEvent(ref mouse_event) => {
                    match inner.moving.clone() {
                        WindowMovingStatus::Moving(_) => {
                            // only wait for left button up to exit this mode
                            if !mouse_event.left_button_hold {
                                self.last_mouse_position_event = mouse_event.clone();
                                call_later_do_move_active_window = true;
                            }
                            call_later_do_refresh_floating_border = true;
                        },
                        WindowMovingStatus::Resizing(_, _) => {
                            if !mouse_event.left_button_hold {
                                self.last_mouse_position_event = mouse_event.clone();
                                call_later_do_move_active_window = true;
                            }
                            call_later_do_refresh_floating_border = true;
                        },
                        WindowMovingStatus::Stationary => {
                            if (mouse_event.coordinate.y as usize) < inner.title_bar_height
                                && (mouse_event.coordinate.x as usize) < width
                            {
                                // the region of title bar
                                let mut is_three_button = false;
                                // Check buttons (Right aligned)
                                for i in 0..3 {
                                    // 0: Close, 1: Maximize, 2: Hide
                                    // Position from right: width - (i+1)*BUTTON_WIDTH
                                    // let btn_x_start = width - (i + 1) * WINDOW_BUTTON_WIDTH;
                                    // let btn_x_end = width - i * WINDOW_BUTTON_WIDTH;
                                    
                                    let btn_x_end = width - WINDOW_BUTTON_MARGIN_RIGHT - i * (WINDOW_BUTTON_WIDTH + WINDOW_BUTTON_SPACING);
                                    let btn_x_start = btn_x_end - WINDOW_BUTTON_WIDTH;

                                    if (mouse_event.coordinate.x as usize) >= btn_x_start && (mouse_event.coordinate.x as usize) < btn_x_end
                                    {
                                        is_three_button = true;
                                        if mouse_event.left_button_hold {
                                            self.show_button(TopButton::from(i), 2, &mut inner);
                                            need_refresh_three_button = true;
                                        } else {
                                            self.show_button(TopButton::from(i), 0, &mut inner);
                                            need_refresh_three_button = true;
                                            if self.last_mouse_position_event.left_button_hold {
                                                // Click event handling
                                                if i == 0 {
                                                    // Close
                                                    return Ok(Some(Event::ExitEvent));
                                                } else if i == 1 {
                                                    // Maximize / Restore
                                                    if let Some(restore) = self.restore_bounds {
                                                        // Restore
                                                        inner.resize(restore)?;
                                                        self.restore_bounds = None;
                                                    } else {
                                                        // Maximize
                                                        // Save current bounds and maximize
                                                        let pos = inner.get_position();
                                                        let (w, h) = inner.get_size();
                                                        self.restore_bounds = Some(Rectangle {
                                                            top_left: pos, 
                                                            bottom_right: pos + (w as isize, h as isize) 
                                                        });
                                                        let new_rect = Rectangle {
                                                            top_left: Coord::new(0, 0),
                                                            bottom_right: Coord::new(screen_width as isize, screen_height as isize),
                                                        };
                                                        inner.resize(new_rect)?;
                                                    }
                                                } else if i == 2 {
                                                    // Hide - defer this action
                                                    call_later_do_hide_window = true;
                                                }
                                            }
                                        }
                                    } else {
                                        self.show_button(TopButton::from(i), 1, &mut inner);
                                        need_refresh_three_button = true;
                                    }
                                }
                                // check if user clicked and held the title bar, which means user wanted to move the window
                                if !is_three_button
                                    && !self.last_mouse_position_event.left_button_hold
                                    && mouse_event.left_button_hold
                                {
                                    inner.moving = WindowMovingStatus::Moving(mouse_event.gcoordinate);
                                    call_later_do_refresh_floating_border = true;
                                }
                            } else {
                                // Check for resize (borders)
                                let mx = mouse_event.coordinate.x;
                                let my = mouse_event.coordinate.y;
                                let w = width as isize;
                                let h = height as isize;
                                let m = RESIZE_MARGIN as isize;

                                let left = mx < m;
                                let right = mx > w - m;
                                let top = my < m;
                                let bottom = my > h - m;

                                if (left || right || top || bottom) && mouse_event.left_button_hold {
                                    let edge = if top && left { ResizeEdge::TopLeft }
                                    else if top && right { ResizeEdge::TopRight }
                                    else if bottom && left { ResizeEdge::BottomLeft }
                                    else if bottom && right { ResizeEdge::BottomRight }
                                    else if left { ResizeEdge::Left }
                                    else if right { ResizeEdge::Right }
                                    else if top { ResizeEdge::Top }
                                    else { ResizeEdge::Bottom };

                                    inner.moving = WindowMovingStatus::Resizing(mouse_event.gcoordinate, edge);
                                    call_later_do_refresh_floating_border = true;
                                } else {
                                    // The mouse event occurred within the actual window content
                                    unhandled_event = Some(Event::MousePositionEvent(mouse_event.clone()));
                                }
                            }
                            
                            if unhandled_event.is_some() {
                                // Already handled above
                            } else {
                                unhandled_event = Some(Event::MousePositionEvent(mouse_event.clone()));
                            }
                            if (mouse_event.coordinate.y as usize) < height
                                && (mouse_event.coordinate.x as usize) < width
                                && !self.last_mouse_position_event.left_button_hold
                                && mouse_event.left_button_hold
                            {
                                need_to_set_active = true;
                            }
                            self.last_mouse_position_event = mouse_event.clone();
                        }
                    }
                }
                unhandled => {
                    unhandled_event = Some(unhandled);
                }
            }

            // Immediately return any unhandled events to the caller
            // before we loop back to handle additional events.
            if unhandled_event.is_some() {
                break;
            }
        }

        if call_later_do_hide_window {
            wm_ref.lock().hide_window(&self.inner)?;
        }

        let mut wm = wm_ref.lock();
        if need_to_set_active {
            wm.set_active(&self.inner, true)?;
        }

        if need_refresh_three_button {
            let area = self.get_button_area();
            wm.refresh_active_window(Some(area))?;
            wm.refresh_mouse()?;
        }

        if call_later_do_refresh_floating_border {
            wm.move_floating_border()?;
        }

        if call_later_do_move_active_window {
            wm.move_active_window()?;
            self.inner.lock().moving = WindowMovingStatus::Stationary;
            
            // FIX: Redraw border and buttons after resize/move to ensure they don't disappear
            self.draw_border(true);
            let mut inner = self.inner.lock();
            self.show_button(TopButton::Close, 1, &mut inner);
            self.show_button(TopButton::MinimizeMaximize, 1, &mut inner);
            self.show_button(TopButton::Hide, 1, &mut inner);
        }

        Ok(unhandled_event)
    }

    /// Renders the area of this `Window` specified by the given `bounding_box`,
    /// which is relative to the top-left coordinate of this `Window`.
    /// 
    /// Refreshes the whole window if `bounding_box` is `None`.
    /// 
    /// This method should be invoked after updating the window's contents in order to see its new content.
    pub fn render(&mut self, bounding_box: Option<Rectangle>) -> Result<(), &'static str> {
        let wm_ref = WINDOW_MANAGER.get().ok_or("The static window manager was not yet initialized")?;

        // Convert the given relative `bounding_box` to an absolute one (relative to the screen, not the window).
        let coordinate = {
            let window = self.inner.lock();
            window.get_position()
        };
        let absolute_bounding_box = bounding_box.map(|bb| bb + coordinate);

        wm_ref.lock().refresh_windows(absolute_bounding_box)
    }

    /// Returns a `Rectangle` describing the position and dimensions of this Window's content region,
    /// i.e., the area within the window excluding the title bar and border
    /// that is available for rendering application content. 
    /// 
    /// The returned `Rectangle` is expressed relative to this Window's position.
    pub fn area(&self) -> Rectangle {
        self.inner.lock().content_area()
    }

    /// Returns an immutable reference to this window's virtual `Framebuffer`. 
    pub fn framebuffer(&self) -> FramebufferRef {
        FramebufferRef::new(
            self.inner.lock(),
            |guard| guard.framebuffer(),
        )
    }

    /// Returns a mutable reference to this window's virtual `Framebuffer`. 
    pub fn framebuffer_mut(&mut self) -> FramebufferRefMut {
        FramebufferRefMut::new(
            self.inner.lock(),
            |guard| guard.framebuffer(),
            |guard| guard.framebuffer_mut(),
        )
    }

    /// Returns `true` if this window is the currently active window. 
    /// 
    /// Obtains the lock on the window manager instance. 
    pub fn is_active(&self) -> bool {
        WINDOW_MANAGER.get()
            .map(|wm| wm.lock().is_active(&self.inner))
            .unwrap_or(false)
    }

    /// Draw the border of this window, with argument of whether this window is active now
    fn draw_border(&mut self, active: bool) {
        let mut inner = self.inner.lock();
        let border_size = inner.border_size;
        let title_bar_height = inner.title_bar_height;
        let (width, height) = inner.get_size();

        // ── Fond de la titlebar ─────────────────────────────────────

        let title_bg = if active {
            Color::new(0x001A1B26)
        } else {
            Color::new(0x00121212)
        };

        framebuffer_drawer::fill_rectangle(
            inner.framebuffer_mut(),
            Coord::new(0, 0),
            width,
            title_bar_height,
            title_bg.into(),
        );

        if active {
            framebuffer_drawer::fill_rectangle(
                inner.framebuffer_mut(),
                Coord::new(0, 0),
                width,
                1,
                WINDOW_TITLE_HIGHLIGHT.into(),
            );
        }

        // ── Ligne d'accent en bas de la titlebar ────────────────────
        let accent = if active { Color::new(0x007AA2F7) } else { Color::new(0x00292E42) };
        framebuffer_drawer::fill_rectangle(
            inner.framebuffer_mut(),
            Coord::new(0, (title_bar_height - 1) as isize),
            width,
            1,
            accent.into(),
        );

        // ── Bordures gauche / bas / droite ──────────────────────────
        let border_color = if active { Color::new(0x007AA2F7) } else { Color::new(0x00292E42) };

        // gauche
        framebuffer_drawer::fill_rectangle(
            inner.framebuffer_mut(),
            Coord::new(0, title_bar_height as isize),
            border_size,
            height - title_bar_height,
            border_color.into(),
        );
        // bas
        framebuffer_drawer::fill_rectangle(
            inner.framebuffer_mut(),
            Coord::new(0, (height - border_size) as isize),
            width,
            border_size,
            border_color.into(),
        );
        // droite
        framebuffer_drawer::fill_rectangle(
            inner.framebuffer_mut(),
            Coord::new((width - border_size) as isize, title_bar_height as isize),
            border_size,
            height - title_bar_height,
            border_color.into(),
        );

        // ── Titre centré dans la titlebar ───────────────────────────
        let title_text = &self.title;
        let title_len = title_text.chars().count();
        let text_w = title_len * font::CHARACTER_WIDTH;
        // Zone disponible : entre les boutons (3 boutons à droite = 3*24=72px)
        let avail_start = 8usize;
        let avail_end = width.saturating_sub(3 * WINDOW_BUTTON_WIDTH + 8);
        let text_x = if text_w < avail_end.saturating_sub(avail_start) {
            (avail_start + avail_end.saturating_sub(text_w)) / 2
        } else {
            avail_start
        };
        let text_y = (title_bar_height.saturating_sub(font::CHARACTER_HEIGHT)) / 2;
        let text_color = if active { color::WHITE } else { Color::new(0x00565F89) };

        // Dessine chaque caractère du titre
        let mut cx = text_x as isize;
        for ch in title_text.chars() {
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

        // ── Coins arrondis ──────────────────────────────────────────
        let r2 = WINDOW_RADIUS * WINDOW_RADIUS;
        let trans_pixel = color::TRANSPARENT.into();
        for i in 0..WINDOW_RADIUS {
            for j in 0..WINDOW_RADIUS {
                let dx1 = WINDOW_RADIUS - i;
                let dy1 = WINDOW_RADIUS - j;
                if dx1 * dx1 + dy1 * dy1 > r2 {
                    inner.framebuffer_mut().overwrite_pixel(Coord::new(i as isize, j as isize), trans_pixel);
                    inner.framebuffer_mut().overwrite_pixel(Coord::new((width - i - 1) as isize, j as isize), trans_pixel);
                }
            }
        }
    }

    /// show three button with status. state = 0,1,2 for three different color
    fn show_button(&self, button: TopButton, state: usize, inner: &mut WindowInner) {
        let (width, _) = inner.get_size();
        let index = match button {
            TopButton::Close => 0,
            TopButton::MinimizeMaximize => 1,
            TopButton::Hide => 2,
        };
        
        // Position avec marges et espacement
        let x = width - WINDOW_BUTTON_MARGIN_RIGHT - (index + 1) * WINDOW_BUTTON_WIDTH - index * WINDOW_BUTTON_SPACING;
        // Centrage vertical
        let y = (inner.title_bar_height.saturating_sub(WINDOW_BUTTON_HEIGHT)) / 2;

        let bg_color = match (button, state) {
            (TopButton::Close, 0) | (TopButton::Close, 2) => Color::new(0x00F7768E), // rouge tokyo
            (_, 0) => Color::new(0x00414868), // hover plus doux
            (_, 2) => Color::new(0x00565F89), // pressé
            _ => Color::new(0x0024283B), // Fond inactif très discret
        };

        // Draw background
        framebuffer_drawer::fill_rectangle(
            inner.framebuffer_mut(),
            Coord::new(x as isize, y as isize),
            WINDOW_BUTTON_WIDTH,
            WINDOW_BUTTON_HEIGHT,
            bg_color.into(),
        );

        // Draw Icon (White)
        let icon_color = color::WHITE.into();
        let center_x = x as isize + (WINDOW_BUTTON_WIDTH as isize / 2);
        let center_y = y as isize + (WINDOW_BUTTON_HEIGHT as isize / 2);
        let center = Coord::new(center_x, center_y);
        
        match button {
            TopButton::Close => {
                // Croix plus fine et équilibrée
                let s = 3;
                framebuffer_drawer::draw_line(inner.framebuffer_mut(), center + (-s, -s), center + (s, s), icon_color);
                framebuffer_drawer::draw_line(inner.framebuffer_mut(), center + (s, -s), center + (-s, s), icon_color);
            },
            TopButton::MinimizeMaximize => {
                // Carré épuré
                let s = 3;
                framebuffer_drawer::draw_line(inner.framebuffer_mut(), center + (-s, -s), center + (s, -s), icon_color); // haut
                framebuffer_drawer::draw_line(inner.framebuffer_mut(), center + (-s, s), center + (s, s), icon_color);   // bas
                framebuffer_drawer::draw_line(inner.framebuffer_mut(), center + (-s, -s), center + (-s, s), icon_color); // gauche
                framebuffer_drawer::draw_line(inner.framebuffer_mut(), center + (s, -s), center + (s, s), icon_color);   // droite
            },
            TopButton::Hide => {
                // Ligne simple en bas
                let s = 3;
                framebuffer_drawer::draw_line(inner.framebuffer_mut(), center + (-s, s), center + (s, s), icon_color);
            },
        }
    }

    /// Gets the rectangle occupied by the three buttons
    fn get_button_area(&self) -> Rectangle {
        let inner = self.inner.lock();
        let width = inner.get_size().0;
        Rectangle {
            top_left: Coord::new(0, 0),
            bottom_right: Coord::new(width as isize, inner.title_bar_height as isize)
        }
    }
}

impl Drop for Window{
    fn drop(&mut self){
        if let Some(wm) = WINDOW_MANAGER.get() {
            if let Err(err) = wm.lock().delete_window(&self.inner) {
                error!("Failed to delete_window upon drop: {:?}", err);
            }
        } else {
            error!("BUG: Could not delete_window upon drop because the window manager was not initialized");
        }
    }
}

/// A wrapper around a locked inner window that immutably derefs to a `Framebuffer`.
///
/// The lock is auto-released when this object is dropped.
pub type FramebufferRef<'g> = DerefsTo<MutexGuard<'g, WindowInner>, Framebuffer<AlphaPixel>>;

/// A wrapper around a locked inner window that mutably derefs to a `Framebuffer`.
///
/// The lock is auto-released when this object is dropped.
pub type FramebufferRefMut<'g> = DerefsToMut<MutexGuard<'g, WindowInner>, Framebuffer<AlphaPixel>>;
