//! The `WindowInner` struct is the internal representation of a `Window` used by the window manager.
//!
//! In comparison, the `Window` struct is application-facing, meaning it is used by (owned by)
//! and exposed directly to applications or tasks that wish to display content.
//!
//! The window manager typically holds `Weak` references to a `WindowInner` struct,
//! which allows it to control the window itself and handle non-application-related
//! components of the window, such as the title bar, border, etc.

#![no_std]

extern crate mpmc;
extern crate event_types;
extern crate framebuffer;
extern crate shapes;
extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use mpmc::Queue;
use event_types::Event;
use framebuffer::{Framebuffer, AlphaPixel};
use shapes::{Coord, Rectangle};

/// Height of the title bar in pixels.
pub const DEFAULT_TITLE_BAR_HEIGHT: usize = 32;
/// Width of the left, right, and bottom borders in pixels.
pub const DEFAULT_BORDER_SIZE: usize = 1;

/// Minimum window dimensions to ensure decorations are always visible.
pub const MIN_WINDOW_WIDTH:  usize = 120;
pub const MIN_WINDOW_HEIGHT: usize = DEFAULT_TITLE_BAR_HEIGHT + DEFAULT_BORDER_SIZE + 40;

/// The edge being dragged during a resize operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResizeEdge {
    Top, Bottom, Left, Right,
    TopLeft, TopRight, BottomLeft, BottomRight,
}

/// Whether a window is currently being manipulated by the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowMovingStatus {
    /// The window is not in motion.
    Stationary,
    /// The window is being dragged; the `Coord` is the mouse position when dragging started.
    Moving(Coord),
    /// The window is being resized; the `Coord` is the mouse anchor, `ResizeEdge` is which edge.
    Resizing(Coord, ResizeEdge),
}

/// The system-facing internal representation of a window.
///
/// The window manager interacts with this directly; application code must use
/// the `Window` wrapper instead.
pub struct WindowInner {
    /// Top-left corner of the window, relative to the screen.
    coordinate: Coord,
    /// Left/right/bottom border thickness in pixels.
    pub border_size: usize,
    /// Title bar height in pixels.
    pub title_bar_height: usize,
    /// The task that owns this window, used to route events.
    pub task_id: Option<usize>,
    /// Whether this window is currently visible (not hidden).
    pub visible: bool,
    /// Producer end of this window's event queue.
    /// The window manager pushes events here; `Window` pops them.
    event_producer: Queue<Event>,
    /// Virtual framebuffer for this window's pixels.
    framebuffer: Framebuffer<AlphaPixel>,
    /// Current drag/resize state.
    pub moving: WindowMovingStatus,
    /// Rayon des coins arrondis : le compositeur mélange ces quatre coins et copie le reste en
    /// bloc (zone opaque, comme `wl_surface.set_opaque_region`). Trop grand reste correct
    /// (plus de mélange), trop petit ne l'est pas.
    pub corner_radius: usize,
    /// Fenêtre entièrement translucide : tous ses pixels sont mélangés à ce qui est dessous.
    pub translucent: bool,
    /// Appelée après chaque événement déposé dans la file de la fenêtre (sûre en interruption :
    /// une sonnette). Un propriétaire qui dort au lieu de scruter sa file (le serveur Wayland)
    /// s'y fait réveiller.
    pub on_event: Option<fn()>,
    /// Contenu lu directement dans le tampon de l'application (composition directe) : le
    /// compositeur ne lit pas `framebuffer` sur cette zone, sauf aux coins arrondis du bas, que
    /// le propriétaire y recopie (la décoration les arrondit dans `framebuffer`).
    pub direct: Option<DirectContent>,
    /// Décorée par le système (barre de titre, bordure). `false` : l'application dessine les
    /// siennes (décorations côté client, `xdg-decoration` en mode `client_side`) ; titre et
    /// bordure ont alors une épaisseur nulle.
    pub decorated: bool,
    /// Titre affiché (barre de titre, barre des tâches).
    pub title: String,
    /// Identifiant de l'application (`xdg_toplevel.set_app_id`, nom du fichier `.desktop`) :
    /// la barre des tâches en tire l'icône.
    pub app_id: String,
    /// Numéro de création, croissant : ordre stable des boutons de la barre des tâches.
    pub serial: u64,
}

/// Prochain numéro de création de fenêtre.
static NEXT_SERIAL: AtomicU64 = AtomicU64::new(1);

/// Source de pixels lue par le compositeur au moment de composer (tampon d'un client Wayland,
/// sans copie). Doit rester valide et lisible tant qu'elle est attachée à une fenêtre.
pub trait ContentPlane: Send + Sync {
    /// Écrit dans `out` la ligne `y` du contenu, colonnes `[x, x + out.len())`, dans la
    /// convention de MaiOS (alpha `0` = opaque, couleurs non prémultipliées).
    fn read_row(&self, x: usize, y: usize, out: &mut [AlphaPixel]);
}

/// Contenu direct d'une fenêtre : sa source et sa place dans la fenêtre.
#[derive(Clone)]
pub struct DirectContent {
    pub plane: Arc<dyn ContentPlane>,
    /// Coin du contenu dans la fenêtre (pixels de la fenêtre).
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

/// Rayon par défaut, prudent, tant que la décoration n'a pas fixé le sien.
pub const DEFAULT_CORNER_RADIUS: usize = 16;

impl WindowInner {
    /// Creates a new `WindowInner` backed by the given `framebuffer`.
    pub fn new(
        coordinate: Coord,
        framebuffer: Framebuffer<AlphaPixel>,
        event_producer: Queue<Event>,
        task_id: Option<usize>,
    ) -> WindowInner {
        WindowInner {
            coordinate,
            border_size: DEFAULT_BORDER_SIZE,
            title_bar_height: DEFAULT_TITLE_BAR_HEIGHT,
            task_id,
            visible: true,
            event_producer,
            framebuffer,
            moving: WindowMovingStatus::Stationary,
            corner_radius: DEFAULT_CORNER_RADIUS,
            translucent: false,
            on_event: None,
            direct: None,
            decorated: true,
            title: String::new(),
            app_id: String::new(),
            serial: NEXT_SERIAL.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// Returns `true` if `coordinate` (relative to this window's top-left) is within bounds.
    #[inline]
    pub fn contains(&self, coordinate: Coord) -> bool {
        self.framebuffer.contains(coordinate)
    }

    /// Returns `(width, height)` of this window in pixels.
    #[inline]
    pub fn get_size(&self) -> (usize, usize) {
        self.framebuffer.get_size()
    }

    /// Returns the top-left position of this window, relative to the screen.
    #[inline]
    pub fn get_position(&self) -> Coord {
        self.coordinate
    }

    /// Sets the top-left position of this window, relative to the screen.
    #[inline]
    pub fn set_position(&mut self, coordinate: Coord) {
        self.coordinate = coordinate;
    }

    /// Returns the full bounding rectangle of this window (including decorations).
    #[inline]
    pub fn get_envelope(&self) -> Rectangle {
        let (w, h) = self.get_size();
        let pos = self.get_position();
        Rectangle {
            top_left:     pos,
            bottom_right: pos + (w as isize, h as isize),
        }
    }

    /// Immutable reference to this window's framebuffer.
    #[inline]
    pub fn framebuffer(&self) -> &Framebuffer<AlphaPixel> {
        &self.framebuffer
    }

    /// Mutable reference to this window's framebuffer.
    #[inline]
    pub fn framebuffer_mut(&mut self) -> &mut Framebuffer<AlphaPixel> {
        &mut self.framebuffer
    }

    /// Returns the pixel at `coordinate`, or `None` if out of bounds.
    #[inline]
    pub fn get_pixel(&self, coordinate: Coord) -> Option<AlphaPixel> {
        self.framebuffer.get_pixel(coordinate)
    }

    /// Border thickness in pixels (left, right, bottom).
    #[inline]
    pub fn get_border_size(&self) -> usize { self.border_size }

    /// Title bar height in pixels.
    #[inline]
    pub fn get_title_bar_height(&self) -> usize { self.title_bar_height }

    /// The content area: the region inside the window excluding decorations.
    ///
    /// Coordinates are relative to this window's top-left corner.
    pub fn content_area(&self) -> Rectangle {
        let (w, h) = self.get_size();
        Rectangle {
            top_left:     Coord::new(self.border_size as isize, self.title_bar_height as isize),
            bottom_right: Coord::new(
                (w - self.border_size)  as isize,
                (h - self.border_size) as isize,
            ),
        }
    }

    /// Returns `true` if `coordinate` (screen-relative) falls inside the title bar.
    pub fn is_in_title_bar(&self, coord_relative_to_window: Coord) -> bool {
        let (w, _) = self.get_size();
        coord_relative_to_window.y >= 0
            && (coord_relative_to_window.y as usize) < self.title_bar_height
            && coord_relative_to_window.x >= 0
            && (coord_relative_to_window.x as usize) < w
    }

    /// Resizes and repositions this window to fit `new_position`.
    ///
    /// Clamps to minimum dimensions, then sends a resize event to the owning `Window`.
    pub fn resize(&mut self, mut new_position: Rectangle) -> Result<(), &'static str> {
        // Enforce minimum size
        if new_position.width()  < MIN_WINDOW_WIDTH  {
            new_position.bottom_right.x = new_position.top_left.x + MIN_WINDOW_WIDTH as isize;
        }
        if new_position.height() < MIN_WINDOW_HEIGHT {
            new_position.bottom_right.y = new_position.top_left.y + MIN_WINDOW_HEIGHT as isize;
        }

        self.coordinate  = new_position.top_left;
        self.framebuffer = Framebuffer::new(new_position.width(), new_position.height(), None)?;

        // Fill with opaque black to avoid graphical artifacts until the app redraws.
        self.framebuffer.fill(AlphaPixel { alpha: 0, ..Default::default() });

        // Notify the owning Window so it can redraw its content.
        self.send_event(Event::new_window_resize_event(self.content_area()))
            .map_err(|_| "Failed to enqueue resize event; window event queue was full.")?;

        Ok(())
    }

    /// Sends `event` to this window.
    ///
    /// Returns `Err(event)` if the queue was full.
    #[inline]
    pub fn send_event(&self, event: Event) -> Result<(), Event> {
        self.event_producer.push(event)?;
        if let Some(ring) = self.on_event {
            ring();
        }
        Ok(())
    }
}
