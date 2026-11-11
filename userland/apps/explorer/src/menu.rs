//! Menu MaiOS (bouton « M » de la barre des tâches, ou touche Super seule).
//!
//! Une surimpression translucide au-dessus de tout, ouverte avec la saisie du pointeur et du
//! clavier : un clic hors du panneau le ferme, toutes les touches lui reviennent. Contenu :
//! champ de recherche (il suffit de taper), grille des applications (molette, flèches,
//! Entrée), session et boutons Redémarrer / Éteindre (un second clic confirme).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use event_types::{Event, MousePositionEvent};
use framebuffer::Framebuffer;
use keycodes_ascii::{KeyAction, Keycode};
use mpmc::Queue;
use shapes::Coord;
use spin::Mutex;
use time::{Duration, Instant, Monotonic};
use window_inner::WindowInner;

use crate::catalog::App;
use crate::icons::Icons;
use crate::paint::{self, rgb, Canvas, Rgb, WHITE};
use crate::{C_ACCENT, C_PURPLE, C_TEXT, C_TEXT_DIM, C_TEXT_SOFT};

/// Marge transparente autour du panneau (ombre).
const MARGIN: usize = 18;
const PANEL_W: usize = 620;
const PANEL_H: usize = 640;
const RADIUS: isize = 16;
const CELL_W: isize = 140;
const CELL_H: isize = 108;
const GRID_TOP: isize = 108;
const FOOTER_H: isize = 84;
const ICON: usize = 48;
/// Délai pour confirmer Redémarrer / Éteindre.
const ARM_MS: u64 = 5000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hit {
    App(usize),
    Restart,
    Poweroff,
    Panel,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Power {
    Restart,
    Poweroff,
}

pub enum Action {
    /// Lancer l'appli (indice dans le catalogue).
    Launch(usize),
    Close,
    Reboot,
    Poweroff,
}

pub struct Menu {
    pub inner: Arc<Mutex<WindowInner>>,
    pub events: Queue<Event>,
    pub open: bool,
    /// Taille du panneau.
    pw: isize,
    ph: isize,
    query: String,
    /// Applis montrées (indices du catalogue), dans l'ordre.
    shown: Vec<usize>,
    sel: usize,
    /// Première rangée visible.
    scroll: usize,
    hover: Option<Hit>,
    press: Option<Hit>,
    left: bool,
    /// Un bouton quelconque était enfoncé.
    any: bool,
    armed: Option<(Power, Instant)>,
    message: Option<String>,
    rects: Vec<(isize, isize, isize, isize, Hit)>,
    dirty: bool,
}

impl Menu {
    pub fn new(screen: (usize, usize), bar_h: usize, ring: fn()) -> Result<Menu, &'static str> {
        let (sw, sh) = screen;
        let pw = PANEL_W.min(sw.saturating_sub(16));
        let ph = PANEL_H.min(sh.saturating_sub(bar_h + 16));
        let (w, h) = (pw + 2 * MARGIN, ph + 2 * MARGIN);
        let fb = Framebuffer::new(w, h, None)?;
        let events = Queue::with_capacity(256);
        // Panneau à 10 px du bord gauche et de la barre des tâches.
        let x = 10isize - MARGIN as isize;
        let y = (sh - bar_h - 10 - ph - MARGIN) as isize;
        let mut inner = WindowInner::new(Coord::new(x.max(0), y.max(0)), fb, events.clone(), None);
        inner.border_size = 0;
        inner.title_bar_height = 0;
        inner.corner_radius = 0;
        inner.decorated = false;
        inner.translucent = true;
        inner.on_event = Some(ring);
        inner.title = String::from("Menu MaiOS");
        Ok(Menu {
            inner: Arc::new(Mutex::new(inner)),
            events,
            open: false,
            pw: pw as isize,
            ph: ph as isize,
            query: String::new(),
            shown: Vec::new(),
            sel: 0,
            scroll: 0,
            hover: None,
            press: None,
            left: false,
            any: false,
            armed: None,
            message: None,
            rects: Vec::new(),
            dirty: true,
        })
    }

    /// Ouvre le menu (au-dessus de tout, saisies du pointeur et du clavier).
    pub fn show(&mut self, apps: &[App]) {
        if self.open {
            return;
        }
        while self.events.pop().is_some() {}
        self.open = true;
        self.query.clear();
        self.sel = 0;
        self.scroll = 0;
        self.hover = None;
        self.press = None;
        self.left = false;
        self.any = false;
        self.armed = None;
        self.message = None;
        self.filter(apps);
        self.dirty = true;
        if let Some(wm) = window_manager::WINDOW_MANAGER.get() {
            let mut wm = wm.lock();
            wm.add_overlay(&self.inner);
            wm.set_pointer_grab(Some(&self.inner));
            wm.set_keyboard_grab(Some(&self.inner));
        }
    }

    pub fn hide(&mut self) {
        if !self.open {
            return;
        }
        self.open = false;
        if let Some(wm) = window_manager::WINDOW_MANAGER.get() {
            let mut wm = wm.lock();
            wm.set_pointer_grab(None);
            wm.set_keyboard_grab(None);
            let _ = wm.delete_window(&self.inner);
        }
    }

    fn filter(&mut self, apps: &[App]) {
        let q = self.query.trim().to_lowercase();
        let mut v: Vec<(u8, usize)> = apps.iter().enumerate().filter_map(|(i, a)| a.rank(&q).map(|r| (r, i))).collect();
        v.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| apps[a.1].name.to_lowercase().cmp(&apps[b.1].name.to_lowercase())));
        self.shown = v.into_iter().map(|(_, i)| i).collect();
        self.sel = 0;
        self.scroll = 0;
    }

    /// Géométrie de la grille : (x0, colonnes, rangées visibles).
    fn grid(&self) -> (isize, usize, usize) {
        let x = MARGIN as isize;
        let cols = ((self.pw - 32) / CELL_W).max(1) as usize;
        let x0 = x + (self.pw - cols as isize * CELL_W) / 2;
        let rows = ((self.ph - GRID_TOP - FOOTER_H - 8) / CELL_H).max(1) as usize;
        (x0, cols, rows)
    }

    fn total_rows(&self) -> usize {
        let (_, cols, _) = self.grid();
        self.shown.len().div_ceil(cols)
    }

    fn clamp_scroll(&mut self) {
        let (_, _, rows) = self.grid();
        self.scroll = self.scroll.min(self.total_rows().saturating_sub(rows));
    }

    /// Fait défiler pour montrer la sélection.
    fn reveal_sel(&mut self) {
        let (_, cols, rows) = self.grid();
        let r = self.sel / cols;
        if r < self.scroll {
            self.scroll = r;
        } else if r >= self.scroll + rows {
            self.scroll = r + 1 - rows;
        }
    }

    fn hit(&self, p: Coord) -> Option<Hit> {
        self.rects.iter().rev().find(|&&(x, y, w, h, _)| p.x >= x && p.x < x + w && p.y >= y && p.y < y + h).map(|r| r.4)
    }

    /// Le point (coordonnées de la surimpression) est-il dans le panneau ?
    fn in_panel(&self, p: Coord) -> bool {
        let m = MARGIN as isize;
        p.x >= m && p.y >= m && p.x < m + self.pw && p.y < m + self.ph
    }

    pub fn on_event(&mut self, ev: Event, apps: &[App]) -> Option<Action> {
        if !self.open {
            return None;
        }
        match ev {
            Event::MousePositionEvent(m) => self.on_mouse(&m),
            Event::KeyboardEvent(k) => self.on_key(&k.key_event, apps),
            Event::PointerLeave => {
                self.hover = None;
                self.dirty = true;
                None
            }
            _ => None,
        }
    }

    fn on_mouse(&mut self, m: &MousePositionEvent) -> Option<Action> {
        let p = m.coordinate;
        let pressed = m.left_button_hold && !self.left;
        let released = !m.left_button_hold && self.left;
        self.left = m.left_button_hold;
        // Clic (n'importe quel bouton) hors du panneau : fermer.
        let any = m.left_button_hold || m.right_button_hold || m.middle_button_hold;
        let any_pressed = any && !self.any;
        self.any = any;
        if any_pressed && !self.in_panel(p) {
            return Some(Action::Close);
        }
        if m.scroll != 0 && self.in_panel(p) {
            let s = self.scroll as isize - m.scroll as isize;
            self.scroll = s.max(0) as usize;
            self.clamp_scroll();
            self.dirty = true;
        }
        let hit = self.hit(p);
        if hit != self.hover {
            self.hover = hit;
            self.dirty = true;
        }
        if pressed {
            self.press = hit;
            self.dirty = true;
        }
        if released {
            let p = self.press.take();
            self.dirty = true;
            if p.is_some() && p == hit {
                return match hit? {
                    Hit::App(i) => Some(Action::Launch(*self.shown.get(i)?)),
                    Hit::Restart => self.arm(Power::Restart),
                    Hit::Poweroff => self.arm(Power::Poweroff),
                    Hit::Panel => None,
                };
            }
        }
        None
    }

    /// Premier clic : demande confirmation ; second clic dans le délai : exécute.
    fn arm(&mut self, p: Power) -> Option<Action> {
        let now = time::now::<Monotonic>();
        if let Some((q, at)) = self.armed {
            if q == p && now < at + Duration::from_millis(ARM_MS) {
                self.armed = None;
                return Some(match p {
                    Power::Restart => Action::Reboot,
                    Power::Poweroff => Action::Poweroff,
                });
            }
        }
        self.armed = Some((p, now));
        self.message = None;
        None
    }

    /// Échéance de la confirmation en cours (pour la faire disparaître).
    pub fn deadline(&self) -> Option<Instant> {
        self.armed.map(|(_, at)| at + Duration::from_millis(ARM_MS))
    }

    /// Retire la confirmation expirée.
    pub fn tick(&mut self, now: Instant) {
        if self.deadline().is_some_and(|d| now >= d) {
            self.armed = None;
            self.dirty = true;
        }
    }

    /// Message d'erreur montré au pied du menu.
    pub fn set_message(&mut self, m: String) {
        self.message = Some(m);
        self.armed = None;
        self.dirty = true;
    }

    fn on_key(&mut self, k: &keycodes_ascii::KeyEvent, apps: &[App]) -> Option<Action> {
        if k.action != KeyAction::Pressed {
            return None;
        }
        let (_, cols, rows) = self.grid();
        let n = self.shown.len();
        self.dirty = true;
        match k.keycode {
            Keycode::Escape => {
                if self.query.is_empty() {
                    return Some(Action::Close);
                }
                self.query.clear();
                self.filter(apps);
            }
            Keycode::Enter => return self.shown.get(self.sel).map(|&i| Action::Launch(i)),
            Keycode::Backspace => {
                if self.query.pop().is_some() {
                    self.filter(apps);
                }
            }
            Keycode::Left if self.sel > 0 => self.sel -= 1,
            Keycode::Right if self.sel + 1 < n => self.sel += 1,
            Keycode::Up if self.sel >= cols => self.sel -= cols,
            Keycode::Down if n > 0 => self.sel = (self.sel + cols).min(n - 1),
            Keycode::PageUp => self.sel = self.sel.saturating_sub(cols * rows),
            Keycode::PageDown if n > 0 => self.sel = (self.sel + cols * rows).min(n - 1),
            Keycode::Home => self.sel = 0,
            Keycode::End => self.sel = n.saturating_sub(1),
            _ => {
                let c = mai_ui::input::key_char(k)?;
                if self.query.chars().count() < 40 {
                    self.query.push(c);
                    self.filter(apps);
                }
            }
        }
        self.reveal_sel();
        None
    }

    /// Redessine si besoin.
    pub fn update(&mut self, apps: &[App], icons: &mut Icons) {
        if !self.open || !self.dirty {
            return;
        }
        self.dirty = false;
        let env = {
            let arc = self.inner.clone();
            let mut inner = arc.lock();
            let (w, h) = inner.get_size();
            let fb = inner.framebuffer_mut();
            let mut c = Canvas::new(fb.buffer_mut(), w, h);
            self.rects.clear();
            self.draw(&mut c, apps, icons);
            inner.get_envelope()
        };
        window_manager::damage(env);
    }

    fn draw(&mut self, c: &mut Canvas, apps: &[App], icons: &mut Icons) {
        let m = MARGIN as isize;
        let (pw, ph) = (self.pw, self.ph);
        c.clear();
        c.shadow(m, m, pw, ph, RADIUS, 150);
        c.rrect(m, m, pw, ph, RADIUS, rgb(0x15162A), 244);
        // Halo coloré discret en haut du panneau.
        c.rrect_grad(m, m, pw, 120, RADIUS, C_ACCENT.darken(150), rgb(0x15162A), 90);
        c.rrect_outline(m, m, pw, ph, RADIUS, WHITE, 30);
        self.rects.push((m, m, pw, ph, Hit::Panel));

        // Champ de recherche.
        let (fx, fy, fw, fh) = (m + 24, m + 22, pw - 48, 42);
        c.rrect(fx, fy, fw, fh, fh / 2, rgb(0x0C0D18), 200);
        c.rrect_outline(fx, fy, fw, fh, fh / 2, C_ACCENT, 170);
        paint::search_icon(c, fx + 14, fy + 10, 22, C_TEXT_SOFT, 255);
        let ty = fy + (fh - paint::CHAR_H) / 2;
        let tx = fx + 46;
        if self.query.is_empty() {
            c.text(tx, ty, "Rechercher une application", C_TEXT_DIM);
            c.fill(tx - 2, ty, 2, paint::CHAR_H, C_ACCENT, 230);
        } else {
            let q = paint::ellipsize(&self.query, fw - 70);
            c.text(tx, ty, &q, C_TEXT);
            c.fill(tx + paint::text_width(&q, 1, 0) + 1, ty, 2, paint::CHAR_H, C_ACCENT, 230);
        }

        // Titre de la grille.
        let title = if self.query.is_empty() { "Applications" } else { "Résultats" };
        c.text(m + 28, m + 80, title, C_TEXT);
        let count = alloc::format!("{}", self.shown.len());
        c.text(m + pw - 28 - paint::text_width(&count, 1, 0), m + 80, &count, C_TEXT_DIM);

        // Grille.
        let (x0, cols, rows) = self.grid();
        self.clamp_scroll();
        let top = m + GRID_TOP;
        if self.shown.is_empty() {
            let s = "Aucune application ne correspond";
            c.text(m + (pw - paint::text_width(s, 1, 0)) / 2, top + 40, s, C_TEXT_SOFT);
        }
        let first = self.scroll * cols;
        for (k, &ai) in self.shown.iter().enumerate().skip(first).take(rows * cols) {
            let slot = (k - first) as isize;
            let (cx, cy) = (x0 + (slot % cols as isize) * CELL_W, top + (slot / cols as isize) * CELL_H);
            let app = &apps[ai];
            let hov = self.hover == Some(Hit::App(k));
            let sel = k == self.sel;
            if sel {
                c.rrect(cx + 4, cy, CELL_W - 8, CELL_H - 4, 12, WHITE, if hov { 34 } else { 24 });
                c.rrect_outline(cx + 4, cy, CELL_W - 8, CELL_H - 4, 12, C_ACCENT, 140);
            } else if hov {
                c.rrect(cx + 4, cy, CELL_W - 8, CELL_H - 4, 12, WHITE, 18);
            }
            let img = icons.app(app, ICON);
            let ix = cx + (CELL_W - ICON as isize) / 2;
            c.image(ix, cy + 10, &img, 255);
            let lines = paint::wrap2(&app.name, CELL_W - 14);
            for (li, line) in lines.iter().enumerate() {
                let lw = paint::text_width(line, 1, 0);
                c.text(cx + (CELL_W - lw) / 2, cy + 64 + li as isize * 17, line, C_TEXT);
            }
            self.rects.push((cx + 4, cy, CELL_W - 8, CELL_H - 4, Hit::App(k)));
        }
        // Barre de défilement.
        let total = self.total_rows();
        if total > rows {
            let track_h = rows as isize * CELL_H;
            let th = (track_h * rows as isize / total as isize).max(24);
            let ty = top + (track_h - th) * self.scroll as isize / (total - rows) as isize;
            c.rrect(m + pw - 9, top, 4, track_h, 2, WHITE, 16);
            c.rrect(m + pw - 9, ty, 4, th, 2, WHITE, 90);
        }

        // Pied : session et alimentation.
        let fy = m + ph - FOOTER_H;
        c.fill(m + 1, fy, pw - 2, 1, WHITE, 22);
        c.rrect(m + 1, fy + 1, pw - 2, FOOTER_H - 1, RADIUS, rgb(0x0C0D18), 90);
        let (ax, ay, asz) = (m + 26, fy + 20, 44);
        c.rrect_grad(ax, ay, asz, asz, asz / 2, C_ACCENT.lighten(20), C_PURPLE, 255);
        paint::logo(c, ax, ay, asz, WHITE);
        c.text(ax + asz + 14, ay + 4, "MaiOS", C_TEXT);
        c.text(ax + asz + 14, ay + 23, "Session root", C_TEXT_DIM);

        let bs = 44;
        let px = m + pw - 26 - bs;
        let rx = px - bs - 10;
        let by = fy + 20;
        for (x, hit, power) in [(rx, Hit::Restart, Power::Restart), (px, Hit::Poweroff, Power::Poweroff)] {
            let armed = self.armed.is_some_and(|(p, _)| p == power);
            let hov = self.hover == Some(hit);
            let col: Rgb = if power == Power::Poweroff { rgb(0xE5484D) } else { C_ACCENT };
            if armed {
                c.rrect(x, by, bs, bs, bs / 2, col, 230);
            } else if hov {
                c.rrect(x, by, bs, bs, bs / 2, WHITE, 30);
            }
            let ink = if armed { WHITE } else if hov { C_TEXT } else { C_TEXT_SOFT };
            match power {
                Power::Restart => paint::restart_icon(c, x + 8, by + 8, bs - 16, ink, 255),
                Power::Poweroff => paint::power_icon(c, x + 8, by + 8, bs - 16, ink, 255),
            }
            self.rects.push((x, by, bs, bs, hit));
        }
        let note: Option<(String, Rgb)> = match (&self.message, self.armed) {
            (Some(msg), _) => Some((msg.clone(), rgb(0xF7768E))),
            (None, Some((Power::Poweroff, _))) => Some(("Cliquez encore pour éteindre".into(), rgb(0xFF9E64))),
            (None, Some((Power::Restart, _))) => Some(("Cliquez encore pour redémarrer".into(), rgb(0x7DCFFF))),
            _ => match self.hover {
                Some(Hit::Restart) => Some(("Redémarrer".into(), C_TEXT_SOFT)),
                Some(Hit::Poweroff) => Some(("Éteindre".into(), C_TEXT_SOFT)),
                _ => None,
            },
        };
        if let Some((s, col)) = note {
            let room = rx - 12 - (ax + asz + 120);
            let s = paint::ellipsize(&s, room);
            c.text(rx - 12 - paint::text_width(&s, 1, 0), by + (bs - paint::CHAR_H) / 2, &s, col);
        }
    }
}
