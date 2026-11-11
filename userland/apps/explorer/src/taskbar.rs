//! Barre des tâches : une surimpression translucide de MaiCompositor en bas de l'écran (la
//! bande est réservée : les fenêtres agrandies s'arrêtent au-dessus).
//!
//! De gauche à droite : bouton MaiOS (menu), un bouton par fenêtre (icône, titre, état ; croix
//! de fermeture au survol) et par lancement en cours, horloge, bande « afficher le bureau ».
//! Clic gauche : activer ou réduire ; clic du milieu ou croix : fermer.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use event_types::{Event, MousePositionEvent};
use framebuffer::Framebuffer;
use mai_icons::Image;
use mpmc::Queue;
use shapes::Coord;
use spin::Mutex;
use window_inner::WindowInner;

use crate::paint::{self, rgb, Canvas, Rgb, WHITE};
use crate::{Clock, C_ACCENT, C_PURPLE, C_TEXT, C_TEXT_DIM, C_TEXT_SOFT};

pub const HEIGHT: usize = 48;

const BTN_Y: isize = 5;
const BTN_H: isize = 38;
const START_X: isize = 8;
const START_W: isize = 38;
const ITEMS_X: isize = 62;
const ITEM_MAX_W: isize = 216;
const ITEM_MIN_W: isize = 46;
const ITEM_GAP: isize = 4;
const CLOCK_W: isize = 104;
const SHOW_DESKTOP_W: isize = 10;
const ICON: usize = 24;

/// Identité d'un bouton.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    /// Fenêtre (numéro de création `WindowInner::serial`).
    Window(u64),
    /// Lancement en cours.
    Pending(u64),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum State {
    Active,
    Visible,
    Hidden,
    /// Phase de l'animation (0..3).
    Launching(u8),
}

#[derive(Clone)]
pub struct Item {
    pub key: Key,
    pub title: String,
    pub icon: Arc<Image>,
    pub state: State,
}

impl PartialEq for Item {
    fn eq(&self, o: &Self) -> bool {
        self.key == o.key && self.title == o.title && self.state == o.state && Arc::ptr_eq(&self.icon, &o.icon)
    }
}

/// Ce que montre la barre.
#[derive(Clone, PartialEq)]
pub struct Model {
    pub items: Vec<Item>,
    pub menu_open: bool,
    pub clock: Option<Clock>,
    pub desktop_shown: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hit {
    Start,
    Item(usize),
    Close(usize),
    Clock,
    ShowDesktop,
}

/// Ce que l'utilisateur a demandé.
pub enum Action {
    ToggleMenu,
    /// Clic sur un bouton : activer, ou réduire la fenêtre active.
    Click(Key),
    Close(Key),
    ShowDesktop,
}

pub struct Taskbar {
    pub inner: Arc<Mutex<WindowInner>>,
    pub events: Queue<Event>,
    width: usize,
    model: Option<Model>,
    rects: Vec<(isize, isize, isize, isize, Hit)>,
    hover: Option<Hit>,
    press: Option<Hit>,
    press_mid: Option<Hit>,
    left: bool,
    middle: bool,
    dirty: bool,
}

impl Taskbar {
    pub fn new(screen: (usize, usize), ring: fn()) -> Result<Taskbar, &'static str> {
        let (sw, sh) = screen;
        let fb = Framebuffer::new(sw, HEIGHT, None)?;
        let events = Queue::with_capacity(256);
        let mut inner = WindowInner::new(Coord::new(0, (sh - HEIGHT) as isize), fb, events.clone(), None);
        inner.border_size = 0;
        inner.title_bar_height = 0;
        inner.corner_radius = 0;
        inner.decorated = false;
        inner.translucent = true;
        inner.on_event = Some(ring);
        inner.title = String::from("Barre des tâches");
        let inner = Arc::new(Mutex::new(inner));
        let wm = window_manager::WINDOW_MANAGER.get().ok_or("gestionnaire de fenêtres absent")?;
        {
            let mut wm = wm.lock();
            wm.set_reserved_bottom(HEIGHT);
            wm.add_overlay(&inner);
        }
        Ok(Taskbar {
            inner,
            events,
            width: sw,
            model: None,
            rects: Vec::new(),
            hover: None,
            press: None,
            press_mid: None,
            left: false,
            middle: false,
            dirty: true,
        })
    }

    fn hit(&self, p: Coord) -> Option<Hit> {
        // Les croix passent avant leur bouton (rangées après lui).
        self.rects
            .iter()
            .rev()
            .find(|&&(x, y, w, h, _)| p.x >= x && p.x < x + w && p.y >= y && p.y < y + h)
            .map(|r| r.4)
    }

    fn key_of(&self, i: usize) -> Option<Key> {
        self.model.as_ref()?.items.get(i).map(|it| it.key)
    }

    /// Événement de la file de la barre ; rend l'action demandée.
    pub fn on_event(&mut self, ev: Event) -> Option<Action> {
        match ev {
            Event::MousePositionEvent(m) => self.on_mouse(&m),
            Event::PointerLeave => {
                if self.hover.take().is_some() || self.press.take().is_some() {
                    self.dirty = true;
                }
                self.left = false;
                self.middle = false;
                None
            }
            _ => None,
        }
    }

    fn on_mouse(&mut self, m: &MousePositionEvent) -> Option<Action> {
        let hit = self.hit(m.coordinate);
        if hit != self.hover {
            self.hover = hit;
            self.dirty = true;
        }
        let mut action = None;
        if m.left_button_hold && !self.left {
            self.press = hit;
            self.dirty = true;
        } else if !m.left_button_hold && self.left {
            let p = self.press.take();
            self.dirty = true;
            if p.is_some() && p == hit {
                action = match hit {
                    Some(Hit::Start) => Some(Action::ToggleMenu),
                    Some(Hit::Item(i)) => self.key_of(i).map(Action::Click),
                    Some(Hit::Close(i)) => self.key_of(i).map(Action::Close),
                    Some(Hit::ShowDesktop) => Some(Action::ShowDesktop),
                    _ => None,
                };
            }
        }
        if m.middle_button_hold && !self.middle {
            self.press_mid = hit;
        } else if !m.middle_button_hold && self.middle {
            if let Some(Hit::Item(i) | Hit::Close(i)) = self.press_mid.take() {
                if matches!(hit, Some(Hit::Item(j) | Hit::Close(j)) if j == i) {
                    action = self.key_of(i).map(Action::Close);
                }
            }
        }
        self.left = m.left_button_hold;
        self.middle = m.middle_button_hold;
        action
    }

    /// Redessine si le modèle ou le survol a changé.
    pub fn update(&mut self, model: &Model) {
        if !self.dirty && self.model.as_ref() == Some(model) {
            return;
        }
        self.model = Some(model.clone());
        self.dirty = false;
        let env = {
            let arc = self.inner.clone();
            let mut inner = arc.lock();
            let (w, h) = inner.get_size();
            let fb = inner.framebuffer_mut();
            let mut c = Canvas::new(fb.buffer_mut(), w, h);
            self.rects.clear();
            self.draw(&mut c, model);
            inner.get_envelope()
        };
        window_manager::damage(env);
    }

    fn draw(&mut self, c: &mut Canvas, m: &Model) {
        let w = self.width as isize;
        let h = HEIGHT as isize;
        let accent = C_ACCENT;
        c.clear();
        // Verre fumé : fond sombre presque opaque, reflet en haut, liseré clair.
        c.fill(0, 0, w, h, rgb(0x0E0F1C), 222);
        for i in 0..h / 2 {
            c.fill(0, i, w, 1, WHITE, (h as u32 / 2 - i as u32) / 3);
        }
        c.fill(0, 0, w, 1, WHITE, 38);

        // Bouton MaiOS.
        let hov = self.hover == Some(Hit::Start);
        let down = self.press == Some(Hit::Start);
        let (top, bot) = if down {
            (accent.darken(20), C_PURPLE.darken(40))
        } else if hov || m.menu_open {
            (accent.lighten(55), C_PURPLE.lighten(25))
        } else {
            (accent.lighten(20), C_PURPLE)
        };
        if m.menu_open {
            c.rrect(START_X - 3, BTN_Y - 3, START_W + 6, BTN_H + 6, 14, accent, 70);
        }
        c.shadow(START_X, BTN_Y, START_W, BTN_H, 11, 80);
        c.rrect_grad(START_X, BTN_Y, START_W, BTN_H, 11, top, bot, 255);
        c.rrect_outline(START_X, BTN_Y, START_W, BTN_H, 11, WHITE, if hov { 110 } else { 60 });
        paint::logo(c, START_X + 1, BTN_Y, BTN_H, WHITE);
        self.rects.push((START_X, BTN_Y, START_W, BTN_H, Hit::Start));

        // Séparateur.
        c.fill(ITEMS_X - 8, 12, 1, h - 24, WHITE, 34);

        // Boutons des fenêtres.
        let right = w - SHOW_DESKTOP_W - CLOCK_W - 12;
        let n = m.items.len() as isize;
        let avail = (right - ITEMS_X).max(0);
        let bw = if n == 0 { ITEM_MAX_W } else { ((avail + ITEM_GAP) / n - ITEM_GAP).clamp(ITEM_MIN_W, ITEM_MAX_W) };
        for (i, it) in m.items.iter().enumerate() {
            let x = ITEMS_X + i as isize * (bw + ITEM_GAP);
            if x + bw > right {
                break; // plus de place : les derniers boutons sont omis
            }
            self.draw_item(c, i, it, x, bw);
        }

        // Horloge.
        let cx = w - SHOW_DESKTOP_W - CLOCK_W - 4;
        if self.hover == Some(Hit::Clock) {
            c.rrect(cx, BTN_Y, CLOCK_W, BTN_H, 9, WHITE, 18);
        }
        let tr = cx + CLOCK_W - 10;
        match m.clock {
            Some(k) => {
                let time = alloc::format!("{:02}:{:02}", k.hour, k.min);
                let date = k.short_date();
                c.text(tr - paint::text_width(&time, 1, 0), 7, &time, C_TEXT);
                c.text(tr - paint::text_width(&date, 1, 0), 25, &date, C_TEXT_SOFT);
            }
            None => c.text(tr - paint::text_width("MaiOS", 1, 0), 16, "MaiOS", C_TEXT_SOFT),
        }
        self.rects.push((cx, BTN_Y, CLOCK_W, BTN_H, Hit::Clock));

        // Afficher le bureau.
        let sx = w - SHOW_DESKTOP_W;
        c.fill(sx - 1, 10, 1, h - 20, WHITE, 40);
        let a = match (self.hover == Some(Hit::ShowDesktop), m.desktop_shown) {
            (true, _) => 46,
            (false, true) => 26,
            _ => 0,
        };
        if a > 0 {
            c.fill(sx, 0, SHOW_DESKTOP_W, h, WHITE, a);
        }
        self.rects.push((sx, 0, SHOW_DESKTOP_W, h, Hit::ShowDesktop));
    }

    fn draw_item(&mut self, c: &mut Canvas, i: usize, it: &Item, x: isize, bw: isize) {
        let hov = matches!(self.hover, Some(Hit::Item(j) | Hit::Close(j)) if j == i);
        let down = matches!(self.press, Some(Hit::Item(j)) if j == i);
        let (fill, edge) = match it.state {
            State::Active => (34, 46),
            State::Visible => (14, 22),
            State::Hidden | State::Launching(_) => (0, 0),
        };
        let fill = if down { fill + 8 } else if hov { fill + 18 } else { fill };
        let edge = if hov { edge + 16 } else { edge };
        if fill > 0 {
            c.rrect(x, BTN_Y, bw, BTN_H, 9, WHITE, fill);
        }
        if edge > 0 {
            c.rrect_outline(x, BTN_Y, bw, BTN_H, 9, WHITE, edge);
        }

        // Icône, centrée si le bouton est trop étroit pour le titre.
        let with_title = bw >= 92;
        let ix = if with_title { x + 10 } else { x + (bw - ICON as isize) / 2 };
        let iy = BTN_Y + (BTN_H - ICON as isize) / 2 - 1;
        let opacity = match it.state {
            State::Hidden => 170,
            State::Launching(_) => 150,
            _ => 255,
        };
        c.image(ix, iy, &it.icon, opacity);

        // Indicateur sous le bouton.
        let base = BTN_Y + BTN_H - 3;
        match it.state {
            State::Active => c.rrect(x + bw / 2 - 12, base, 24, 3, 1, C_ACCENT, 255),
            State::Visible => c.rrect(x + bw / 2 - 5, base, 10, 3, 1, C_TEXT_SOFT, 200),
            State::Hidden => c.rrect(x + bw / 2 - 2, base, 4, 3, 1, C_TEXT_DIM, 230),
            State::Launching(p) => {
                for k in 0..3 {
                    let lit = k == p as isize;
                    c.rrect(x + bw / 2 - 9 + k * 7, base, 4, 3, 1, if lit { C_ACCENT } else { C_TEXT_DIM }, if lit { 255 } else { 150 });
                }
            }
        }

        // Titre et croix de fermeture.
        let closable = matches!(it.key, Key::Window(_)) && hov && bw >= 110;
        if with_title {
            let tx = x + 42;
            let tw = bw - 42 - if closable { 30 } else { 10 };
            let label = paint::ellipsize(&it.title, tw);
            let col: Rgb = match it.state {
                State::Active => C_TEXT,
                State::Visible => C_TEXT_SOFT,
                _ => C_TEXT_DIM,
            };
            c.text(tx, BTN_Y + (BTN_H - paint::CHAR_H) / 2, &label, col);
        }
        self.rects.push((x, BTN_Y, bw, BTN_H, Hit::Item(i)));
        if closable {
            let (cx, cy, cs) = (x + bw - 26, BTN_Y + (BTN_H - 20) / 2, 20);
            let on = self.hover == Some(Hit::Close(i));
            if on {
                c.rrect(cx, cy, cs, cs, 6, rgb(0xE5484D), 230);
            }
            paint::close_icon(c, cx, cy, cs, if on { WHITE } else { C_TEXT_SOFT }, 255);
            self.rects.push((cx, cy, cs, cs, Hit::Close(i)));
        }
    }
}
