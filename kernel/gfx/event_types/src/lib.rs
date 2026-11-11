#![no_std]

extern crate alloc;

use alloc::string::String;
use keycodes_ascii::KeyEvent;
use mouse_data::MouseEvent;
use gamepad_data::GamepadState;
use shapes::{Coord, Rectangle};
use core::sync::atomic::{AtomicUsize, Ordering};

// ── Sonnette d'entrée ────────────────────────────────────────────────────────
//
// Les pilotes (clavier PS/2, souris PS/2, HID USB) déposent leurs événements dans des files
// sans verrou puis appellent [`notify_input`]. Le compositeur y inscrit, une seule fois, la
// fonction qui le réveille : il n'a plus besoin de scruter les files à intervalle fixe.
// `notify_input` peut être appelée depuis un gestionnaire d'interruption : elle ne prend aucun
// verrou (une lecture atomique et un appel).

static INPUT_NOTIFIER: AtomicUsize = AtomicUsize::new(0);

/// Inscrit la fonction appelée après chaque événement d'entrée (une seule à la fois).
pub fn set_input_notifier(f: fn()) {
    INPUT_NOTIFIER.store(f as usize, Ordering::Release);
}

/// Signale qu'un événement d'entrée vient d'être déposé (sûr en interruption).
#[inline]
pub fn notify_input() {
    let f = INPUT_NOTIFIER.load(Ordering::Acquire);
    if f != 0 {
        // SAFETY : la valeur provient toujours d'un `fn()` inscrit par `set_input_notifier`.
        let f: fn() = unsafe { core::mem::transmute::<usize, fn()>(f) };
        f();
    }
}

/// An event describing mouse position rather than movement differential from last event.
/// It contains two position, `coodinate` for the relative position in each window, and `gcoordinate` for global absolute position of the screen.
#[derive(Debug, Clone)]
pub struct MousePositionEvent {
    /// the relative position in window
    pub coordinate: Coord,
    /// the global position in window
    pub gcoordinate: Coord,
    /// whether the mouse is scrolling up
    pub scrolling_up: bool,
    /// whether the mouse is scrolling down
    pub scrolling_down: bool,
    /// whether the left button holds
    pub left_button_hold: bool,
    /// whether the right button holds
    pub right_button_hold: bool,
    /// whether the fourth button holds
    pub fourth_button_hold: bool,
    /// whether the fifth button holds
    pub fifth_button_hold: bool,
    /// Bouton du milieu enfoncé.
    pub middle_button_hold: bool,
    /// Crans de molette de cet événement (positif : vers le haut), 0 sans défilement.
    pub scroll: i8,
}

impl Default for MousePositionEvent {
    fn default() -> Self {
        MousePositionEvent {
            coordinate: Coord::new(0, 0),
            gcoordinate: Coord::new(0, 0),
            scrolling_up: false,
            scrolling_down: false,
            left_button_hold: false,
            right_button_hold: false,
            fourth_button_hold: false,
            fifth_button_hold: false,
            middle_button_hold: false,
            scroll: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    /// An input event from a keyboard
    KeyboardEvent(KeyboardInputEvent),
    /// An input event from a mouse
    MouseMovementEvent(MouseEvent),
    /// An event indicating that another entity wants to print the given `String`.
    OutputEvent(String),
    /// Tells an application that the window manager has resized or moved its window
    /// so that it knows to refresh its display and perform any necessary tasks, such as text reflow.
    ///
    /// The new position and size of the window is given by the `Rectangle` within,
    /// and represents the content area within the window that is accessible to the application,
    /// which excludes the window title bar, borders, etc.
    WindowResizeEvent(Rectangle),
    /// The event tells application about mouse's position currently (including relative to a window and relative to a screen)
    MousePositionEvent(MousePositionEvent),
    /// An input event from a USB gamepad/joystick.
    GamepadEvent(GamepadState),
    ExitEvent,
    /// La fenêtre gagne (`true`) ou perd (`false`) le focus du clavier.
    FocusEvent(bool),
    /// Le pointeur a quitté la zone de la fenêtre où il était (une autre fenêtre ou le bureau
    /// le reçoit désormais) : plus d'événement de pointeur jusqu'au prochain survol.
    PointerLeave,
}

impl Event {
    /// Create a new keyboard event
    pub fn new_keyboard_event(kev: KeyEvent) -> Event {
        Event::KeyboardEvent(KeyboardInputEvent::new(kev))
    }

    /// Create a new output event
    pub fn new_output_event<S>(s: S) -> Event
    where
        S: Into<String>,
    {
        Event::OutputEvent(s.into())
    }

    /// Create a new window resize event
    pub fn new_window_resize_event(new_position: Rectangle) -> Event {
        Event::WindowResizeEvent(new_position)
    }
}

/// A keyboard event, indicating that one or more keys were pressed or released.
#[derive(Debug, Clone)]
pub struct KeyboardInputEvent {
    /// The key input event from i/o device
    pub key_event: KeyEvent,
}

impl KeyboardInputEvent {
    /// Create a new key board input event. `key` is the key input from the keyboard
    pub fn new(key_event: KeyEvent) -> KeyboardInputEvent {
        KeyboardInputEvent { key_event }
    }
}