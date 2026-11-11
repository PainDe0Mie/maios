//! A `Window` object is owned by an application and wraps a `WindowInner` managed by the
//! window manager.  It draws its own decorations (title bar, borders, buttons) and filters
//! internal events (moving, resizing, button clicks) before forwarding the rest to the app.
//!
//! Decorations are rendered by the `deco` module: anti-aliased rounded corners (written
//! through the alpha channel so the compositor smooths them over the wallpaper), a
//! gradient title bar, and macOS-style "traffic light" buttons on the left.
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

mod deco;

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
    MIN_WINDOW_WIDTH, MIN_WINDOW_HEIGHT, DEFAULT_BORDER_SIZE, DEFAULT_TITLE_BAR_HEIGHT,
};
use window_manager::WINDOW_MANAGER;

// ── Layout constants ──────────────────────────────────────────────────────────

/// Corner rounding radius (disabled while maximised).
const WINDOW_RADIUS: isize = 10;
/// Width of the left / right / bottom resize hit zones.
const RESIZE_MARGIN: isize = 6;
/// Height of the top resize hit zone (kept thin so it doesn't steal title-bar drags).
const TOP_RESIZE_MARGIN: isize = 3;

// ── Button identity ───────────────────────────────────────────────────────────

/// The three window-control buttons, ordered left-to-right (macOS convention).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Button {
    /// Closes the window.
    Close    = 0,
    /// Minimises (hides) the window.
    Minimize = 1,
    /// Maximises or restores the window.
    Maximize = 2,
}

impl Button {
    fn from_index(i: usize) -> Option<Button> {
        match i {
            0 => Some(Button::Close),
            1 => Some(Button::Minimize),
            2 => Some(Button::Maximize),
            _ => None,
        }
    }
}

// ── Deferred action flags ─────────────────────────────────────────────────────

/// Actions to perform after the event-processing loop, outside the inner lock.
#[derive(Default)]
struct PendingActions {
    refresh_buttons:      bool,
    refresh_float_border: bool,
    finalise_move:        bool,
    set_active:           bool,
    hide_window:          bool,
    /// The framebuffer was reallocated (resize / snap) and needs new decorations.
    redraw_frame:         bool,
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
    /// Cached active state to avoid redundant decoration redraws.
    last_is_active: bool,
    /// Window title displayed in the title bar.
    title: String,
    /// Saved geometry for restoring after maximise (`Some` while maximised).
    restore_bounds: Option<Rectangle>,
    /// Button currently under the cursor.
    hovered: Option<Button>,
    /// Whether the left button is held on `hovered`.
    pressed: bool,
    /// Un bouton a été enfoncé dans le contenu et n'est pas encore relâché : tous les
    /// événements de pointeur vont à l'application, même hors du contenu (saisie implicite).
    content_grab: bool,
    /// Le dernier événement de pointeur transmis était dans le contenu (l'application reçoit
    /// `PointerLeave` quand le pointeur en sort).
    in_content: bool,
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
        Self::with_options(title, coordinate, width, height, background, true)
    }

    /// Crée une fenêtre décorée par le système (`decorated`) ou dont l'application dessine
    /// elle-même barre de titre et bordure (aucune décoration, contenu = fenêtre entière).
    pub fn with_options(
        title: String,
        coordinate: Coord,
        width: usize,
        height: usize,
        background: Color,
        decorated: bool,
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
        let mut window_inner = WindowInner::new(coordinate, framebuffer, event_producer, Some(task_id));
        window_inner.title = title.clone();
        if !decorated {
            window_inner.decorated = false;
            window_inner.title_bar_height = 0;
            window_inner.border_size = 0;
            window_inner.corner_radius = 0;
        }

        let mut window = Window {
            inner: Arc::new(Mutex::new(window_inner)),
            event_consumer,
            last_mouse: MousePositionEvent::default(),
            last_is_active: true,
            title,
            restore_bounds: None,
            hovered: None,
            pressed: false,
            content_grab: false,
            in_content: false,
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
    /// transparently and are **not** forwarded to the caller. Resize events are
    /// forwarded *after* the decorations have been redrawn.
    ///
    /// Returns `Ok(None)` when the queue is empty.
    pub fn handle_event(&mut self) -> Result<Option<Event>, &'static str> {
        let wm_ref = WINDOW_MANAGER.get().ok_or("window manager not initialised")?;

        // Sync the active indicator without holding the WM lock during event processing.
        let is_active = wm_ref.lock().is_active(&self.inner);
        if is_active != self.last_is_active {
            self.last_is_active = is_active;
            if !is_active {
                self.hovered = None;
                self.pressed = false;
            }
            self.redraw_decorations(is_active);
            let area = self.envelope();
            wm_ref.lock().refresh_windows(Some(area))?;
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
                Event::WindowResizeEvent(..) => {
                    pending.redraw_frame = true;
                    app_event = Some(event.clone());
                }
                Event::PointerLeave => {
                    // Le pointeur est passé à une autre fenêtre ou au bureau.
                    if self.hovered.is_some() || self.pressed {
                        self.hovered = None;
                        self.pressed = false;
                        let mut inner = self.inner.lock();
                        self.paint_buttons(&mut inner);
                        pending.refresh_buttons = true;
                    }
                    self.content_grab = false;
                    if core::mem::replace(&mut self.in_content, false) {
                        app_event = Some(Event::PointerLeave);
                    }
                    self.last_mouse = MousePositionEvent::default();
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

        if pending.hide_window {
            wm_ref.lock().hide_window(&self.inner)?;
        }

        let mut wm = wm_ref.lock();

        if pending.set_active {
            wm.set_active(&self.inner, true)?;
        }
        if pending.refresh_float_border {
            wm.move_floating_border()?;
        }
        if pending.finalise_move {
            wm.move_active_window()?;
            self.inner.lock().moving = WindowMovingStatus::Stationary;
            pending.redraw_frame = true;
        }
        if pending.redraw_frame {
            self.redraw_decorations(self.last_is_active);
            let area = self.envelope();
            wm.refresh_windows(Some(area))?;
        } else if pending.refresh_buttons && !pending.hide_window {
            // Full stack in this small region: correct even if this window is not on top.
            let area = self.button_area();
            wm.refresh_windows(Some(area))?;
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

        // ── Window is being dragged or resized ────────────────────────────────
        if inner.moving != WindowMovingStatus::Stationary {
            if !mouse.left_button_hold {
                pending.finalise_move = true;
            }
            pending.refresh_float_border = true;
            return;
        }

        // ── Saisie implicite du contenu : tout va à l'application jusqu'au relâchement ──
        let held = |m: &MousePositionEvent| {
            m.left_button_hold || m.right_button_hold || m.middle_button_hold || m.fourth_button_hold || m.fifth_button_hold
        };
        // Sans décoration du système, toute la fenêtre est du contenu : l'application déplace
        // et redimensionne elle-même (`begin_move`, `begin_resize`).
        if !inner.decorated {
            let any_pressed_now = !held(&self.last_mouse) && held(mouse);
            if !self.last_mouse.left_button_hold && mouse.left_button_hold {
                pending.set_active = true;
            }
            if any_pressed_now {
                self.content_grab = true;
            }
            if self.content_grab && !held(mouse) {
                self.content_grab = false;
            }
            self.in_content = true;
            *app_event = Some(Event::MousePositionEvent(mouse.clone()));
            return;
        }
        if self.content_grab {
            if !held(mouse) {
                self.content_grab = false;
            }
            self.in_content = true;
            *app_event = Some(Event::MousePositionEvent(mouse.clone()));
            return;
        }

        let (width, height) = inner.get_size();
        let (w, h) = (width as isize, height as isize);
        let tbh = inner.title_bar_height as isize;
        let (mx, my) = (mouse.coordinate.x, mouse.coordinate.y);
        let pressed_now  = !self.last_mouse.left_button_hold && mouse.left_button_hold;
        let released_now = self.last_mouse.left_button_hold && !mouse.left_button_hold;
        let any_pressed_now = !held(&self.last_mouse) && held(mouse);


        // Clicking anywhere in the window raises it.
        if pressed_now {
            pending.set_active = true;
        }

        // ── Buttons: hover / pressed feedback (only redrawn when it changes) ──
        let hit = if my >= 0 && my < tbh {
            deco::button_at(mx, my, tbh).and_then(Button::from_index)
        } else {
            None
        };
        let now_pressed = hit.is_some() && mouse.left_button_hold;
        if hit != self.hovered || now_pressed != self.pressed {
            self.hovered = hit;
            self.pressed = now_pressed;
            self.paint_buttons(&mut inner);
            pending.refresh_buttons = true;
        }

        if let Some(btn) = hit {
            // Hors du contenu (barre de titre, bord saisi) : l'application l'apprend une fois.
            if core::mem::replace(&mut self.in_content, false) {
                *app_event = Some(Event::PointerLeave);
            }
            if released_now {
                match btn {
                    Button::Close => {
                        *app_event = Some(Event::ExitEvent);
                    }
                    Button::Maximize => {
                        drop(inner); // release before resizing
                        self.toggle_maximize();
                    }
                    Button::Minimize => {
                        self.hovered = None;
                        self.pressed = false;
                        self.paint_buttons(&mut inner);
                        pending.hide_window = true;
                    }
                }
            }
            return;
        }

        // ── Resize: a press that starts on an edge ────────────────────────────
        let on_left   = mx < RESIZE_MARGIN;
        let on_right  = mx >= w - RESIZE_MARGIN;
        let on_top    = my < TOP_RESIZE_MARGIN;
        let on_bottom = my >= h - RESIZE_MARGIN;
        if pressed_now && (on_left || on_right || on_top || on_bottom) {
            let edge = resize_edge(on_top, on_bottom, on_left, on_right);
            inner.moving = WindowMovingStatus::Resizing(mouse.gcoordinate, edge);
            pending.refresh_float_border = true;
            drop(inner);
            // Hors du contenu (barre de titre, bord saisi) : l'application l'apprend une fois.
            if core::mem::replace(&mut self.in_content, false) {
                *app_event = Some(Event::PointerLeave);
            }
            return;
        }

        // ── Title bar: drag to move; never forwarded to the application ───────
        if my < tbh {
            if pressed_now {
                inner.moving = WindowMovingStatus::Moving(mouse.gcoordinate);
                pending.refresh_float_border = true;
            }
            drop(inner);
            // Hors du contenu (barre de titre, bord saisi) : l'application l'apprend une fois.
            if core::mem::replace(&mut self.in_content, false) {
                *app_event = Some(Event::PointerLeave);
            }
            return;
        }

        // ── Content area ──────────────────────────────────────────────────────
        if any_pressed_now {
            self.content_grab = true;
        }
        self.in_content = true;
        *app_event = Some(Event::MousePositionEvent(mouse.clone()));
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Renders (composites) the given area of this window to the screen.
    ///
    /// Pass `None` to refresh the whole window.
    pub fn render(&mut self, area: Option<Rectangle>) -> Result<(), &'static str> {
        let wm_ref = WINDOW_MANAGER.get().ok_or("window manager not initialised")?;
        let origin = {
            let mut inner = self.inner.lock();
            // The app may have painted over the bottom corners: round them again.
            if inner.decorated {
                let style = self.style(&inner, self.last_is_active);
                inner.corner_radius = style.radius.max(0) as usize;
                let (w, h) = inner.get_size();
                let mut surface = deco::Surface::new(inner.framebuffer_mut().buffer_mut(), w, h);
                deco::paint_bottom_corners(&mut surface, &style);
            }
            inner.get_position()
        };
        let absolute = area.map(|bb| bb + origin);
        wm_ref.lock().refresh_windows(absolute)
    }

    /// Changes the title and repaints the title bar.
    pub fn set_title(&mut self, title: String) -> Result<(), &'static str> {
        self.title = title;
        let (w, tbh) = {
            let mut inner = self.inner.lock();
            inner.title = self.title.clone();
            window_manager::notify_desktop(); // la barre des tâches montre le titre
            if !inner.decorated {
                return Ok(());
            }
            self.paint_title_bar(&mut inner, self.last_is_active);
            (inner.get_size().0, inner.title_bar_height)
        };
        self.render(Some(Rectangle {
            top_left:     Coord::new(0, 0),
            bottom_right: Coord::new(w as isize, tbh as isize),
        }))
    }

    /// Returns the window's title.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Identifiant de l'application (la barre des tâches en tire l'icône).
    pub fn set_app_id(&mut self, app_id: &str) {
        self.inner.lock().app_id = String::from(app_id);
        window_manager::notify_desktop(); // la barre des tâches en tire l'icône
    }

    /// La fenêtre porte les décorations du système.
    pub fn is_decorated(&self) -> bool {
        self.inner.lock().decorated
    }

    /// Ajoute ou retire les décorations du système en gardant la taille du contenu et sa place
    /// à l'écran (la fenêtre grandit ou rétrécit autour). Le propriétaire reçoit un
    /// `WindowResizeEvent` (nouvelle zone de contenu).
    pub fn set_decorated(&mut self, on: bool) -> Result<(), &'static str> {
        let wm_ref = WINDOW_MANAGER.get().ok_or("window manager not initialised")?;
        let old = self.envelope();
        {
            let mut inner = self.inner.lock();
            if inner.decorated == on {
                return Ok(());
            }
            let c = inner.content_area();
            let (cw, ch) = (c.width(), c.height());
            let origin = inner.get_position() + (c.top_left.x, c.top_left.y);
            let (b, t) = if on { (DEFAULT_BORDER_SIZE, DEFAULT_TITLE_BAR_HEIGHT) } else { (0, 0) };
            inner.decorated = on;
            inner.border_size = b;
            inner.title_bar_height = t;
            inner.corner_radius = if on { WINDOW_RADIUS as usize } else { 0 };
            let tl = origin - (b as isize, t as isize);
            let size = ((cw + 2 * b) as isize, (ch + t + b) as isize);
            inner.resize(Rectangle { top_left: tl, bottom_right: tl + size })?;
        }
        self.hovered = None;
        self.pressed = false;
        self.redraw_decorations(self.last_is_active);
        let new = self.envelope();
        let mut wm = wm_ref.lock();
        wm.refresh_windows(Some(old))?;
        wm.refresh_windows(Some(new))
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

    /// Sets the task ID that owns this window.
    pub fn set_task_id(&mut self, task_id: Option<usize>) {
        self.inner.lock().task_id = task_id;
    }

    /// Access the underlying `WindowInner` reference.
    pub fn inner(&self) -> &Arc<Mutex<WindowInner>> {
        &self.inner
    }

    /// Redimensionne la fenêtre pour que sa zone de contenu mesure `width` × `height` (même coin
    /// supérieur gauche), redessine la décoration et abîme l'ancienne et la nouvelle enveloppe.
    /// La file reçoit un `WindowResizeEvent`, comme après un redimensionnement à la souris.
    pub fn resize_content(&mut self, width: usize, height: usize) -> Result<(), &'static str> {
        let wm_ref = WINDOW_MANAGER.get().ok_or("window manager not initialised")?;
        let old = self.envelope();
        {
            let mut inner = self.inner.lock();
            let (b, t) = (inner.border_size, inner.title_bar_height);
            let pos = inner.get_position();
            let size = ((width + 2 * b) as isize, (height + t + b) as isize);
            inner.resize(Rectangle { top_left: pos, bottom_right: pos + size })?;
        }
        self.restore_bounds = None;
        self.redraw_decorations(self.last_is_active);
        let new = self.envelope();
        let mut wm = wm_ref.lock();
        wm.refresh_windows(Some(old))?;
        wm.refresh_windows(Some(new))
    }

    /// Returns `true` if this window is currently maximised.
    pub fn is_maximized(&self) -> bool {
        self.restore_bounds.is_some()
    }

    /// Agrandit la fenêtre à tout l'écran (`true`) ou lui rend sa taille d'avant (`false`).
    pub fn set_maximized(&mut self, on: bool) {
        if on != self.is_maximized() {
            self.toggle_maximize();
        }
    }

    /// Déplacement interactif demandé par l'application (`xdg_toplevel.move`) : la fenêtre
    /// suit le pointeur jusqu'au relâchement du bouton gauche, comme une prise par la barre de
    /// titre. Sans effet si le bouton n'est pas enfoncé.
    pub fn begin_move(&mut self) {
        self.begin_drag(None);
    }

    /// Redimensionnement interactif depuis le bord `edge` (`xdg_toplevel.resize`).
    pub fn begin_resize(&mut self, edge: ResizeEdge) {
        self.begin_drag(Some(edge));
    }

    fn begin_drag(&mut self, edge: Option<ResizeEdge>) {
        let Some(wm_ref) = WINDOW_MANAGER.get() else { return };
        let (pos, left) = {
            let wm = wm_ref.lock();
            (wm.mouse_position(), wm.mouse_left())
        };
        if !left || self.is_maximized() {
            return;
        }
        self.content_grab = false;
        self.in_content = false;
        self.inner.lock().moving = match edge {
            None => WindowMovingStatus::Moving(pos),
            Some(e) => WindowMovingStatus::Resizing(pos, e),
        };
        let _ = wm_ref.lock().move_floating_border();
    }

    // ── Decoration drawing ────────────────────────────────────────────────────

    /// Redraws the title bar, borders, rounded corners, title and buttons.
    pub fn redraw_decorations(&mut self, active: bool) {
        let mut inner = self.inner.lock();
        if !inner.decorated {
            inner.corner_radius = 0;
            return;
        }
        let style = self.style(&inner, active);
        inner.corner_radius = style.radius.max(0) as usize;
        let (w, h) = inner.get_size();
        let mut s = deco::Surface::new(inner.framebuffer_mut().buffer_mut(), w, h);
        deco::paint_title_bar(&mut s, &style, 0, w as isize);
        deco::paint_side_borders(&mut s, &style);
        deco::paint_title(&mut s, &style, &self.title);
        deco::paint_buttons(&mut s, &style, self.hovered.map(|b| b as usize), self.pressed);
    }

    /// Repaints the whole title bar (background, title and buttons).
    fn paint_title_bar(&self, inner: &mut WindowInner, active: bool) {
        let style = self.style(inner, active);
        let (w, h) = inner.get_size();
        let mut s = deco::Surface::new(inner.framebuffer_mut().buffer_mut(), w, h);
        deco::paint_title_bar(&mut s, &style, 0, w as isize);
        deco::paint_title(&mut s, &style, &self.title);
        deco::paint_buttons(&mut s, &style, self.hovered.map(|b| b as usize), self.pressed);
    }

    /// Repaints only the button group with the current hover / pressed state.
    fn paint_buttons(&self, inner: &mut WindowInner) {
        if !inner.decorated {
            return;
        }
        let style = self.style(inner, self.last_is_active);
        let (w, h) = inner.get_size();
        let mut s = deco::Surface::new(inner.framebuffer_mut().buffer_mut(), w, h);
        deco::paint_buttons(&mut s, &style, self.hovered.map(|b| b as usize), self.pressed);
    }

    fn style(&self, inner: &WindowInner, active: bool) -> deco::Style {
        deco::Style {
            title_bar_height: inner.title_bar_height as isize,
            radius: if self.restore_bounds.is_some() { 0 } else { WINDOW_RADIUS },
            active,
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Full window rectangle in screen coordinates.
    fn envelope(&self) -> Rectangle {
        self.inner.lock().get_envelope()
    }

    /// Button group rectangle in screen coordinates (used for selective refresh).
    fn button_area(&self) -> Rectangle {
        let inner = self.inner.lock();
        let pos = inner.get_position();
        Rectangle {
            top_left:     pos + (deco::BUTTON_ZONE.0, 0),
            bottom_right: pos + (deco::BUTTON_ZONE.1, inner.title_bar_height as isize),
        }
    }

    /// Toggles between maximised and restored state.
    fn toggle_maximize(&mut self) {
        let wm_ref = match WINDOW_MANAGER.get() {
            Some(wm) => wm,
            None => return,
        };
        // Agrandie : la zone de travail (l'écran moins la barre des tâches).
        let work = wm_ref.lock().work_area();

        self.hovered = None;
        self.pressed = false;

        if let Some(restore) = self.restore_bounds.take() {
            if let Err(e) = self.inner.lock().resize(restore) {
                error!("toggle_maximize restore failed: {}", e);
            }
        } else {
            let envelope = self.envelope();
            self.restore_bounds = Some(envelope);
            if let Err(e) = self.inner.lock().resize(work) {
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
