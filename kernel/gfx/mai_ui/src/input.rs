//! Entrées des applications : pointeur (clics, double-clics, glisser, molette) et clavier.
//!
//! Le gestionnaire de fenêtres ne transmet que des positions absolues et l'état des boutons ;
//! [`Pointer`] en tire des événements (front montant/descendant, double-clic mesuré sur
//! l'horloge, seuil de glisser) comme le font les boîtes à outils de bureau.

use event_types::MousePositionEvent;
use keycodes_ascii::{KeyEvent, Keycode};

/// Délai maximal entre deux clics d'un double-clic (valeur par défaut de Windows et de GTK).
pub const DOUBLE_CLICK_NS: u64 = 500_000_000;
/// Distance maximale (pixels) entre deux clics d'un double-clic.
pub const DOUBLE_CLICK_DIST: isize = 4;
/// Déplacement à partir duquel un appui devient un glisser.
pub const DRAG_THRESHOLD: isize = 4;

/// Ce qu'un événement de souris a produit.
#[derive(Clone, Copy, Debug, Default)]
pub struct PointerActions {
    pub moved: bool,
    pub left_pressed: bool,
    pub left_released: bool,
    pub right_pressed: bool,
    pub right_released: bool,
    /// Nombre de clics de la série en cours au moment de l'appui (1, 2 = double-clic, 3…).
    pub clicks: u8,
    /// Molette : > 0 vers le haut.
    pub wheel: i32,
    /// Le glisser vient de dépasser le seuil.
    pub drag_started: bool,
}

/// État du pointeur, en coordonnées du tampon de la fenêtre.
#[derive(Clone, Copy, Debug, Default)]
pub struct Pointer {
    pub x: isize,
    pub y: isize,
    pub left: bool,
    pub right: bool,
    /// Le pointeur est dans la fenêtre (dernier événement reçu récent).
    pub inside: bool,
    press_x: isize,
    press_y: isize,
    last_click_ns: u64,
    last_click_x: isize,
    last_click_y: isize,
    click_count: u8,
    /// Glisser en cours (bouton gauche tenu au-delà du seuil).
    pub dragging: bool,
}

impl Pointer {
    pub const fn new() -> Pointer {
        Pointer {
            x: -1, y: -1, left: false, right: false, inside: false,
            press_x: 0, press_y: 0, last_click_ns: 0, last_click_x: 0, last_click_y: 0,
            click_count: 0, dragging: false,
        }
    }

    /// Point de départ du glisser en cours.
    pub fn press_origin(&self) -> (isize, isize) { (self.press_x, self.press_y) }

    /// Intègre un événement du gestionnaire de fenêtres (`coordinate` relative à la fenêtre).
    pub fn update(&mut self, ev: &MousePositionEvent, now_ns: u64) -> PointerActions {
        let mut a = PointerActions::default();
        let (x, y) = (ev.coordinate.x, ev.coordinate.y);
        a.moved = x != self.x || y != self.y;
        self.x = x;
        self.y = y;
        self.inside = true;
        if ev.scrolling_up {
            a.wheel += 1;
        }
        if ev.scrolling_down {
            a.wheel -= 1;
        }
        if ev.left_button_hold && !self.left {
            a.left_pressed = true;
            let near = (x - self.last_click_x).abs() <= DOUBLE_CLICK_DIST
                && (y - self.last_click_y).abs() <= DOUBLE_CLICK_DIST;
            if near && now_ns.saturating_sub(self.last_click_ns) <= DOUBLE_CLICK_NS {
                self.click_count = self.click_count.saturating_add(1);
            } else {
                self.click_count = 1;
            }
            self.last_click_ns = now_ns;
            self.last_click_x = x;
            self.last_click_y = y;
            self.press_x = x;
            self.press_y = y;
            self.dragging = false;
        }
        if !ev.left_button_hold && self.left {
            a.left_released = true;
        }
        if ev.left_button_hold && !self.dragging
            && ((x - self.press_x).abs() > DRAG_THRESHOLD || (y - self.press_y).abs() > DRAG_THRESHOLD)
        {
            self.dragging = true;
            a.drag_started = true;
        }
        if !ev.left_button_hold {
            self.dragging = false;
        }
        if ev.right_button_hold && !self.right {
            a.right_pressed = true;
        }
        if !ev.right_button_hold && self.right {
            a.right_released = true;
        }
        self.left = ev.left_button_hold;
        self.right = ev.right_button_hold;
        a.clicks = self.click_count;
        a
    }
}

/// Caractère imprimable produit par une touche (disposition du clavier courante), sans les
/// raccourcis Ctrl/Alt.
pub fn key_char(ev: &KeyEvent) -> Option<char> {
    if ev.modifiers.is_control() || (ev.modifiers.is_alt() && !ev.modifiers.is_alt_gr()) {
        return None;
    }
    match ev.keycode {
        Keycode::Enter | Keycode::Tab | Keycode::Backspace | Keycode::Delete | Keycode::Escape => None,
        _ => ev.keycode.to_ascii(ev.modifiers).filter(|c| !c.is_control()),
    }
}

/// Raccourci Ctrl+lettre : renvoie la lettre (minuscule) si Ctrl est tenu.
pub fn ctrl_letter(ev: &KeyEvent) -> Option<char> {
    if !ev.modifiers.is_control() {
        return None;
    }
    let c = ev.keycode.to_ascii(keycodes_ascii::KeyboardModifiers::empty())?;
    c.is_ascii_alphabetic().then(|| c.to_ascii_lowercase())
}
