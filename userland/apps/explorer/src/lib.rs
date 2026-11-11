//! Explorer — bureau, barre des tâches et menu de MaiOS.
//!
//! - Bureau : fond d'écran procédural, grande horloge, tout le contenu de `root:/desktop`
//!   (`desk`) : raccourcis `.desk`/`.desktop` (vraies icônes du thème quand l'appli en a une :
//!   `mai_icons`), dossiers et fichiers (icônes de `mai_ui`, vignettes des images), ouverts
//!   d'un clic (Fichiers, programme, visionneuse). Événements du pointeur hors des fenêtres et
//!   touche Super seule reçus par la file du bureau de MaiCompositor.
//! - Barre des tâches (`taskbar`) et menu MaiOS (`menu`) : surimpressions translucides.
//! - Catalogue (`catalog`) : raccourcis natifs et applications Linux (`.desktop`).
//!
//! Aucune scrutation : la tâche dort sur une sonnette (`window_manager::Doorbell`) que sonnent
//! la file du bureau, les deux surimpressions et tout changement de la liste des fenêtres
//! (`window_manager::set_desktop_ring`) ; seule l'horloge la réveille chaque seconde.

#![no_std]
extern crate alloc;
#[macro_use]
extern crate log;

mod catalog;
mod desk;
mod icons;
mod menu;
mod paint;
mod taskbar;

use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use event_types::Event;
use framebuffer::AlphaPixel;
use keycodes_ascii::{KeyAction, Keycode};
use mpmc::Queue;
use spin::Mutex;
use time::{Duration, Instant};
use window_inner::WindowInner;
use window_manager::Doorbell;

use catalog::App;
use desk::Target;
use icons::Icons;
use mai_ui::files::FileKind;
use mai_ui::icons::Icon;
use paint::{rgb, Canvas, Rgb, SegStyle, BLACK, WHITE};

// ================================================================
// CONFIGURATION
// ================================================================

const ICON_W: isize = 118;
const ICON_H: isize = 104;
const ICON_SIZE: usize = 48;
const ICON_GAP: isize = 10;
const DESKTOP_PAD_X: isize = 20;
const DESKTOP_PAD_Y: isize = 22;
/// Un lancement sans fenêtre au bout de ce délai est oublié.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(120);

// Palette (Tokyo Night).
pub(crate) const C_ACCENT: Rgb = rgb(0x7AA2F7);
pub(crate) const C_PURPLE: Rgb = rgb(0xBB9AF7);
pub(crate) const C_TEXT: Rgb = rgb(0xE4E9FF);
pub(crate) const C_TEXT_SOFT: Rgb = rgb(0xA9B1D6);
pub(crate) const C_TEXT_DIM: Rgb = rgb(0x737AA2);

/// Sonnette de la tâche de l'explorateur.
static BELL: Doorbell = Doorbell::new();

fn ring() {
    BELL.ring();
}

// ================================================================
// HORLOGE
// ================================================================

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Clock {
    pub hour: u8,
    pub min: u8,
    pub day: u8,
    pub month: u8,
    pub year: u16,
}

#[cfg(target_arch = "x86_64")]
fn read_clock() -> Option<Clock> {
    let t = rtc::read_rtc();
    if t.months == 0 || t.months > 12 || t.days == 0 || t.days > 31 {
        return None;
    }
    Some(Clock { hour: t.hours % 24, min: t.minutes % 60, day: t.days, month: t.months, year: 2000 + t.years as u16 })
}

#[cfg(not(target_arch = "x86_64"))]
fn read_clock() -> Option<Clock> {
    None
}

const JOURS: [&str; 7] = ["Dimanche", "Lundi", "Mardi", "Mercredi", "Jeudi", "Vendredi", "Samedi"];
const MOIS: [&str; 12] = [
    "janvier", "février", "mars", "avril", "mai", "juin", "juillet", "août", "septembre", "octobre", "novembre", "décembre",
];

/// Jour de la semaine (0 = dimanche), algorithme de Sakamoto.
fn weekday(year: u16, month: u8, day: u8) -> usize {
    const T: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let mut y = year as i32;
    if month < 3 {
        y -= 1;
    }
    let m = (month.clamp(1, 12) - 1) as usize;
    (y + y / 4 - y / 100 + y / 400 + T[m] + day as i32).rem_euclid(7) as usize
}

impl Clock {
    fn long_date(&self) -> String {
        format!(
            "{} {} {} {}",
            JOURS[weekday(self.year, self.month, self.day)],
            self.day,
            MOIS[(self.month.clamp(1, 12) - 1) as usize],
            self.year
        )
    }

    pub(crate) fn short_date(&self) -> String {
        format!("{:02}/{:02}/{}", self.day, self.month, self.year)
    }

    fn greeting(&self) -> &'static str {
        match self.hour {
            5..=11 => "Bonjour",
            12..=17 => "Bon après-midi",
            _ => "Bonsoir",
        }
    }
}

// ================================================================
// BUREAU
// ================================================================

/// Place d'une entrée du bureau (même indice que `Desktop::entries`).
struct DeskIcon {
    x: isize,
    y: isize,
}

impl DeskIcon {
    fn hit(&self, px: isize, py: isize) -> bool {
        px >= self.x && px < self.x + ICON_W && py >= self.y && py < self.y + ICON_H
    }

    fn rect(&self) -> shapes::Rectangle {
        shapes::Rectangle {
            top_left: shapes::Coord::new(self.x - 8, self.y - 8),
            bottom_right: shapes::Coord::new(self.x + ICON_W + 8, self.y + ICON_H + 8),
        }
    }
}

/// `n` entrées rangées en colonnes à gauche, de haut en bas, au-dessus de la barre des tâches.
fn layout_icons(n: usize, sh: usize) -> Vec<DeskIcon> {
    let usable = sh as isize - taskbar::HEIGHT as isize - DESKTOP_PAD_Y;
    let per_col = (usable / (ICON_H + ICON_GAP)).max(1);
    (0..n as isize)
        .map(|k| {
            let (col, row) = (k / per_col, k % per_col);
            DeskIcon { x: DESKTOP_PAD_X + col * (ICON_W + ICON_GAP), y: DESKTOP_PAD_Y + row * (ICON_H + ICON_GAP) }
        })
        .collect()
}

/// Apparence d'une entrée du bureau.
enum Look {
    /// Icône d'application (thème ou tuile).
    Image(Arc<mai_icons::Image>),
    /// Vignette d'une image.
    Thumb(Arc<icons::Thumb>),
    /// Icône de fichier de `mai_ui` (comme dans Fichiers).
    Icon(Icon),
}

struct Desktop {
    wall: Vec<AlphaPixel>,
    /// Contenu de `/desktop` et place de chaque entrée.
    entries: Vec<desk::Entry>,
    icons: Vec<DeskIcon>,
    /// Empreinte du dossier à la dernière lecture.
    fingerprint: u64,
    screen_h: usize,
    hover: Option<usize>,
    press: Option<usize>,
    left: bool,
    clock: Option<Clock>,
}

impl Desktop {
    /// Relit `/desktop` si son contenu a changé ; rend `true` s'il faut tout redessiner.
    fn rescan(&mut self, apps: &mut Vec<App>, cache: &mut Icons, force: bool) -> bool {
        let fp = desk::fingerprint();
        if !force && fp == self.fingerprint {
            return false;
        }
        self.fingerprint = fp;
        self.entries = desk::scan(apps);
        self.icons = layout_icons(self.entries.len(), self.screen_h);
        self.hover = None;
        self.press = None;
        let paths: Vec<&str> = self.entries.iter().map(|e| e.path.as_str()).collect();
        cache.keep_thumbs(&paths);
        true
    }

    /// Redessine le bureau et abîme `damage` (`None` : tout).
    fn redraw(&self, apps: &[App], cache: &mut Icons, damage: Option<shapes::Rectangle>) {
        let ok = window_manager::with_desktop_mut(|fb| {
            let (w, h) = (fb.width(), fb.height());
            let buf = fb.buffer_mut();
            if buf.len() != self.wall.len() || buf.len() < w * h {
                warn!("[explorer] taille du fond d'écran incohérente");
                return false;
            }
            buf.copy_from_slice(&self.wall);
            let mut c = Canvas::new(buf, w, h);
            draw_clock_widget(&mut c, self.clock);
            for (k, (icon, e)) in self.icons.iter().zip(&self.entries).enumerate() {
                let look = match e.target {
                    Target::App(i) => Look::Image(cache.app(&apps[i], ICON_SIZE)),
                    Target::Folder => Look::Icon(Icon::Folder),
                    Target::File(FileKind::Image) => {
                        cache.thumb(&e.path, e.len, ICON_SIZE).map(Look::Thumb).unwrap_or(Look::Icon(Icon::Image))
                    }
                    Target::File(kind) => Look::Icon(kind.icon()),
                };
                draw_icon(&mut c, icon, &e.label, &look, self.hover == Some(k), self.press == Some(k));
            }
            true
        });
        if ok == Some(true) {
            match damage {
                Some(r) => window_manager::damage(r),
                None => window_manager::damage_all(),
            }
        }
    }
}

/// Grande horloge en haut à droite du bureau.
fn draw_clock_widget(c: &mut Canvas, clock: Option<Clock>) {
    let Some(clk) = clock else { return };
    let st = SegStyle { w: 30, h: 56, t: 7 };
    let digits_w = 4 * st.w + 2 * (st.w / 3) + 2 * (st.w / 3 + 2) + st.t;
    let right = c.width() - 44;
    let top = 38;
    let x = right - digits_w;

    c.clock_digits(x - 1, top + 1, clk.hour, clk.min, st, C_ACCENT, 40);
    c.clock_digits(x + 2, top + 4, clk.hour, clk.min, st, BLACK, 120);
    c.clock_digits(x, top, clk.hour, clk.min, st, C_TEXT, 255);

    let bar_y = top + st.h + 14;
    for i in 0..40isize {
        let col = C_ACCENT.mix(C_PURPLE, (i * 256 / 40) as u32);
        c.fill(right - 40 + i, bar_y, 1, 3, col, 255);
    }
    let date = clk.long_date();
    let dw = paint::text_width(&date, 1, 1);
    c.text_ex(right - dw + 1, bar_y + 13, &date, 1, 1, BLACK, 160);
    c.text_ex(right - dw, bar_y + 12, &date, 1, 1, C_TEXT, 255);
    let greet = format!("{}, bienvenue sur MaiOS", clk.greeting());
    let gw = paint::text_width(&greet, 1, 0);
    c.text_shadow(right - gw, bar_y + 32, &greet, C_TEXT_SOFT);
}

/// Entrée du bureau : icône (ombre douce), libellé sur deux lignes au plus.
fn draw_icon(c: &mut Canvas, icon: &DeskIcon, name: &str, look: &Look, hovered: bool, pressed: bool) {
    let (x, y) = (icon.x, icon.y);
    if hovered || pressed {
        c.rrect(x, y, ICON_W, ICON_H, 12, WHITE, if pressed { 44 } else { 28 });
        c.rrect_outline(x, y, ICON_W, ICON_H, 12, WHITE, 55);
    }
    let s = ICON_SIZE as isize;
    let (ix, iy) = (x + (ICON_W - s) / 2, y + 8);
    match look {
        Look::Image(img) => {
            c.shadow(ix + 4, iy + 4, s - 8, s - 8, 12, 110);
            c.image(ix, iy, img, 255);
        }
        Look::Thumb(t) => {
            // Photo posée : ombre, liseré blanc autour de la partie visible.
            let (w, h) = (t.w as isize, t.h as isize);
            let (tx, ty) = (ix + (s - w) / 2, iy + (s - h) / 2);
            c.shadow(tx + 2, ty + 3, w, h, 3, 120);
            c.fill(tx - 2, ty - 2, w + 4, h + 4, WHITE, 235);
            c.image(ix, iy, &t.img, 255);
        }
        Look::Icon(i) => {
            c.shadow(ix + 8, iy + 8, s - 16, s - 12, 8, 90);
            let (buf, w, h) = c.raw();
            let mut u = mai_ui::canvas::Canvas::new(buf, w, h);
            mai_ui::icons::draw(&mut u, *i, ix, iy, s);
        }
    }
    let lines = paint::wrap2(name, ICON_W - 6);
    for (k, line) in lines.iter().enumerate() {
        let lw = paint::text_width(line, 1, 0);
        c.text_shadow(x + (ICON_W - lw) / 2, iy + s + 8 + k as isize * (paint::CHAR_H + 1), line, C_TEXT);
    }
}

// ================================================================
// FENÊTRES ET LANCEMENTS
// ================================================================

/// Fenêtre vue par la barre des tâches.
struct Win {
    serial: u64,
    window: Arc<Mutex<WindowInner>>,
    active: bool,
    hidden: bool,
    title: String,
    app_id: String,
    task_id: Option<usize>,
}

fn snapshot() -> Vec<Win> {
    let Some(wm) = window_manager::WINDOW_MANAGER.get() else { return Vec::new() };
    let entries = wm.lock().windows();
    entries
        .into_iter()
        .map(|e| {
            let (serial, title, app_id, task_id) = {
                let i = e.window.lock();
                (i.serial, i.title.clone(), i.app_id.clone(), i.task_id)
            };
            Win { serial, window: e.window, active: e.active, hidden: e.hidden, title, app_id, task_id }
        })
        .collect()
}

/// Appli du catalogue d'une fenêtre.
fn app_of<'a>(apps: &'a [App], w: &Win) -> Option<(usize, &'a App)> {
    apps.iter().enumerate().find(|(_, a)| a.matches(&w.app_id))
}

/// Lancement en attente de sa première fenêtre.
struct Pending {
    id: u64,
    app: usize,
    task: task::JoinableTaskRef,
    /// Numéro de la dernière fenêtre existante au lancement.
    floor: u64,
    started: Instant,
}

/// Lance la crate d'application `crate_path` avec `args`, dans un espace de noms neuf.
fn spawn_crate(crate_path: &str, args: Vec<String>, name: String) -> Result<task::JoinableTaskRef, String> {
    let p = path::Path::new(crate_path);
    let ns = mod_mgmt::create_application_namespace(None).map_err(|e| format!("espace de noms : {e}"))?;
    let b = spawn::new_application_task_builder(&p, Some(ns)).map_err(|e| format!("constructeur : {e}"))?;
    b.argument(args).name(name).spawn().map_err(|e| format!("lancement : {e}"))
}

fn spawn_app(app: &App) -> Result<task::JoinableTaskRef, String> {
    spawn_crate(&app.path, app.args.clone(), format!("app_{}", app.id))
}

/// Lance l'appli dont la crate commence par `prefix`.
fn spawn_prefix(prefix: &str, args: Vec<String>, name: &str) -> Result<task::JoinableTaskRef, String> {
    let path = catalog::crate_path(prefix).ok_or_else(|| format!("application « {} » introuvable", prefix.trim_end_matches('-')))?;
    spawn_crate(&path, args, String::from(name))
}

// ================================================================
// ÉTAT
// ================================================================

struct Explorer {
    apps: Vec<App>,
    cache: Icons,
    desktop: Desktop,
    taskbar: taskbar::Taskbar,
    menu: menu::Menu,
    pending: Vec<Pending>,
    next_pending: u64,
    /// Tâches lancées, gardées jusqu'à leur fin.
    children: Vec<task::JoinableTaskRef>,
    /// Fenêtres réduites par « afficher le bureau » (restaurées au clic suivant).
    stashed: Vec<Weak<Mutex<WindowInner>>>,
    /// Icônes des fenêtres déjà résolues : numéro → (app_id, icône).
    win_icons: alloc::collections::BTreeMap<u64, (String, Arc<mai_icons::Image>)>,
}

impl Explorer {
    fn launch(&mut self, ai: usize) {
        if self.pending.iter().any(|p| p.app == ai) {
            return; // déjà en cours de lancement
        }
        let app = &self.apps[ai];
        if app.single {
            // Programme Linux à instance unique : ramener sa fenêtre.
            if let Some(w) = snapshot().into_iter().filter(|w| app.matches(&w.app_id)).last() {
                if let Some(wm) = window_manager::WINDOW_MANAGER.get() {
                    let _ = wm.lock().set_active(&w.window, true);
                }
                return;
            }
        }
        let floor = snapshot().iter().map(|w| w.serial).max().unwrap_or(0);
        match spawn_app(app) {
            Ok(task) => {
                info!("[explorer] lancement de {} (tâche {})", app.name, task.0.id);
                self.children.push(task.clone());
                self.next_pending += 1;
                self.pending.push(Pending { id: self.next_pending, app: ai, task, floor, started: Instant::now() });
            }
            Err(e) => warn!("[explorer] {} : {e}", app.name),
        }
    }

    /// Ouvre l'entrée `k` du bureau : appli, dossier (Fichiers), programme Linux (dans
    /// l'environnement d'une session Wayland si `apprun.sh` est là), WAD (Doom), sinon
    /// Fichiers sur le fichier (visionneuse de texte, d'images, hexadécimale).
    fn open_entry(&mut self, k: usize) {
        let Some(e) = self.desktop.entries.get(k) else { return };
        let path = e.path.clone();
        let s = |v: &str| String::from(v);
        let r = match e.target {
            Target::App(i) => return self.launch(i),
            Target::File(FileKind::ElfExec | FileKind::ElfDyn) => {
                let args = if catalog::exists(catalog::APPRUN) {
                    alloc::vec![s("/bin/bash"), s(catalog::APPRUN), s("31536000"), path.clone()]
                } else {
                    alloc::vec![path.clone()]
                };
                spawn_prefix("run-", args, "run")
            }
            Target::File(FileKind::Wad) => spawn_prefix("run-", alloc::vec![s("doom"), s("-iwad"), path.clone()], "doom"),
            Target::Folder | Target::File(_) => spawn_prefix("file_manager-", alloc::vec![path.clone()], "file_manager"),
        };
        match r {
            Ok(t) => {
                info!("[explorer] ouverture de {path}");
                self.children.push(t);
            }
            Err(e) => warn!("[explorer] {path} : {e}"),
        }
    }

    /// Rattache les nouvelles fenêtres aux lancements en cours, oublie les lancements finis.
    fn resolve_pending(&mut self, wins: &[Win]) {
        let apps = &self.apps;
        self.pending.retain(|p| {
            let app = &apps[p.app];
            let found = wins.iter().find(|w| w.serial > p.floor && (app.matches(&w.app_id) || w.task_id == Some(p.task.0.id)));
            if let Some(w) = found {
                if w.app_id.is_empty() {
                    w.window.lock().app_id = app.id.clone();
                }
                return false;
            }
            !p.task.has_exited() && p.started.elapsed() < LAUNCH_TIMEOUT
        });
        self.children.retain(|t| !t.has_exited());
    }

    fn taskbar_model(&mut self, wins: &[Win], clock: Option<Clock>) -> taskbar::Model {
        let mut items = Vec::with_capacity(wins.len() + self.pending.len());
        self.win_icons.retain(|s, _| wins.iter().any(|w| w.serial == *s));
        for w in wins {
            let app = app_of(&self.apps, w);
            let icon = match self.win_icons.get(&w.serial) {
                Some((id, img)) if *id == w.app_id => img.clone(),
                _ => {
                    let img = self.cache.window(app.map(|a| a.1), &w.app_id, &w.title, 24);
                    self.win_icons.insert(w.serial, (w.app_id.clone(), img.clone()));
                    img
                }
            };
            let title = if !w.title.is_empty() {
                w.title.clone()
            } else if let Some((_, a)) = app {
                a.name.clone()
            } else if !w.app_id.is_empty() {
                w.app_id.clone()
            } else {
                String::from("Fenêtre")
            };
            let state = if w.hidden {
                taskbar::State::Hidden
            } else if w.active {
                taskbar::State::Active
            } else {
                taskbar::State::Visible
            };
            items.push(taskbar::Item { key: taskbar::Key::Window(w.serial), title, icon, state });
        }
        for p in &self.pending {
            let app = &self.apps[p.app];
            let phase = ((p.started.elapsed().as_millis() / 300) % 3) as u8;
            items.push(taskbar::Item {
                key: taskbar::Key::Pending(p.id),
                title: format!("{}...", app.name),
                icon: self.cache.app(app, 24),
                state: taskbar::State::Launching(phase),
            });
        }
        taskbar::Model { items, menu_open: self.menu.open, clock, desktop_shown: !self.stashed.is_empty() }
    }

    fn on_taskbar(&mut self, a: taskbar::Action) {
        let Some(wm) = window_manager::WINDOW_MANAGER.get() else { return };
        match a {
            taskbar::Action::ToggleMenu => self.toggle_menu(),
            taskbar::Action::Click(taskbar::Key::Window(s)) => {
                if let Some(w) = snapshot().into_iter().find(|w| w.serial == s) {
                    let mut wm = wm.lock();
                    if w.active && !w.hidden {
                        let _ = wm.hide_window(&w.window);
                    } else {
                        let _ = wm.set_active(&w.window, true);
                    }
                    self.stashed.clear();
                }
            }
            taskbar::Action::Close(taskbar::Key::Window(s)) => {
                if let Some(w) = snapshot().into_iter().find(|w| w.serial == s) {
                    let _ = w.window.lock().send_event(Event::ExitEvent);
                }
            }
            taskbar::Action::Click(taskbar::Key::Pending(_)) | taskbar::Action::Close(taskbar::Key::Pending(_)) => {}
            taskbar::Action::ShowDesktop => self.show_desktop(),
        }
    }

    /// Réduit toutes les fenêtres visibles ; au clic suivant, les restaure.
    fn show_desktop(&mut self) {
        let Some(wm) = window_manager::WINDOW_MANAGER.get() else { return };
        let wins = snapshot();
        let mut wm = wm.lock();
        if self.stashed.is_empty() {
            // La fenêtre active en dernier : elle reprend le focus à la restauration.
            let mut order: Vec<&Win> = wins.iter().filter(|w| !w.hidden).collect();
            order.sort_by_key(|w| w.active);
            for w in order {
                let _ = wm.hide_window(&w.window);
                self.stashed.push(Arc::downgrade(&w.window));
            }
        } else {
            let list: Vec<_> = self.stashed.drain(..).filter_map(|w| w.upgrade()).collect();
            for w in &list {
                let _ = wm.show_window(w);
            }
            if let Some(last) = list.last() {
                let _ = wm.set_active(last, true);
            }
        }
    }

    fn toggle_menu(&mut self) {
        if self.menu.open {
            self.menu.hide();
        } else {
            self.menu.show(&self.apps);
        }
    }

    fn on_menu(&mut self, a: menu::Action) {
        match a {
            menu::Action::Launch(i) => {
                self.menu.hide();
                self.launch(i);
            }
            menu::Action::Close => self.menu.hide(),
            menu::Action::Reboot => {
                info!("[explorer] redémarrage demandé");
                #[cfg(target_arch = "x86_64")]
                acpi::power::reboot();
                #[cfg(not(target_arch = "x86_64"))]
                self.menu.set_message(String::from("Redémarrage non pris en charge"));
            }
            menu::Action::Poweroff => {
                info!("[explorer] extinction demandée");
                #[cfg(target_arch = "x86_64")]
                let r = acpi::power::poweroff();
                #[cfg(not(target_arch = "x86_64"))]
                let r: Result<(), &str> = Err("non pris en charge");
                if let Err(e) = r {
                    warn!("[explorer] extinction impossible : {e}");
                    self.menu.set_message(format!("Extinction impossible : {e}"));
                }
            }
        }
    }

    /// Événement de la file du bureau ; rend la zone du bureau à redessiner.
    fn on_desktop(&mut self, ev: Event) -> Option<Option<shapes::Rectangle>> {
        let d = &mut self.desktop;
        match ev {
            Event::MousePositionEvent(m) => {
                let (x, y) = (m.coordinate.x, m.coordinate.y);
                let hit = d.icons.iter().position(|i| i.hit(x, y));
                let mut changed: Vec<usize> = Vec::new();
                if hit != d.hover {
                    changed.extend(d.hover.iter().chain(hit.iter()).copied());
                    d.hover = hit;
                }
                if m.left_button_hold && !d.left {
                    d.press = hit;
                    changed.extend(hit);
                } else if !m.left_button_hold && d.left {
                    let p = d.press.take();
                    changed.extend(p);
                    if let (Some(k), true) = (p, p == hit) {
                        d.left = false;
                        let r = d.icons[k].rect();
                        self.open_entry(k);
                        return Some(Some(r));
                    }
                }
                d.left = m.left_button_hold;
                if changed.is_empty() {
                    return None;
                }
                Some(changed.iter().map(|&k| d.icons[k].rect()).reduce(union))
            }
            Event::PointerLeave => {
                d.left = false;
                let k = d.hover.take().or(d.press.take())?;
                Some(Some(d.icons[k].rect()))
            }
            Event::KeyboardEvent(k) => {
                let e = k.key_event;
                if matches!(e.keycode, Keycode::SuperKeyLeft | Keycode::SuperKeyRight) && e.action == KeyAction::Released {
                    self.toggle_menu();
                }
                None
            }
            _ => None,
        }
    }
}

fn union(a: shapes::Rectangle, b: shapes::Rectangle) -> shapes::Rectangle {
    shapes::Rectangle {
        top_left: shapes::Coord::new(a.top_left.x.min(b.top_left.x), a.top_left.y.min(b.top_left.y)),
        bottom_right: shapes::Coord::new(a.bottom_right.x.max(b.bottom_right.x), a.bottom_right.y.max(b.bottom_right.y)),
    }
}

// ================================================================
// POINT D'ENTRÉE
// ================================================================

pub fn main(_args: Vec<String>) -> isize {
    info!("[explorer] démarrage");
    let Some(wm) = window_manager::WINDOW_MANAGER.get() else {
        error!("[explorer] pas de gestionnaire de fenêtres");
        return -1;
    };
    let (sw, sh) = wm.lock().get_screen_size();
    BELL.bind_current();

    catalog::init_desktop();
    let mut apps = catalog::load();
    let mut cache = Icons::new();

    let taskbar = match taskbar::Taskbar::new((sw, sh), ring) {
        Ok(t) => t,
        Err(e) => {
            error!("[explorer] barre des tâches : {e}");
            return -1;
        }
    };
    let menu = match menu::Menu::new((sw, sh), taskbar::HEIGHT, ring) {
        Ok(m) => m,
        Err(e) => {
            error!("[explorer] menu : {e}");
            return -1;
        }
    };

    let (fb_w, fb_h) = window_manager::with_desktop(|fb| (fb.width(), fb.height())).unwrap_or((sw, sh));
    let wall = paint::wallpaper(fb_w, fb_h);
    let mut desktop = Desktop {
        wall,
        entries: Vec::new(),
        icons: Vec::new(),
        fingerprint: 0,
        screen_h: sh,
        hover: None,
        press: None,
        left: false,
        clock: read_clock(),
    };
    desktop.rescan(&mut apps, &mut cache, true);

    let desk_events: Queue<Event> = Queue::with_capacity(256);
    window_manager::set_desktop_ring(ring);
    wm.lock().set_desktop_events(desk_events.clone());

    let mut ex = Explorer {
        apps,
        cache,
        desktop,
        taskbar,
        menu,
        pending: Vec::new(),
        next_pending: 0,
        children: Vec::new(),
        stashed: Vec::new(),
        win_icons: alloc::collections::BTreeMap::new(),
    };
    ex.desktop.redraw(&ex.apps, &mut ex.cache, None);
    info!("[explorer] bureau prêt : {} élément(s), écran {sw}x{sh}", ex.desktop.entries.len());

    let mut next_clock = Instant::now() + Duration::from_secs(1);
    loop {
        // 1. Événements.
        let mut desk_damage: Option<Option<shapes::Rectangle>> = None;
        while let Some(ev) = desk_events.pop() {
            if let Some(r) = ex.on_desktop(ev) {
                desk_damage = Some(match (desk_damage, r) {
                    (None, r) => r,
                    (Some(Some(a)), Some(b)) => Some(union(a, b)),
                    _ => None,
                });
            }
        }
        while let Some(ev) = ex.taskbar.events.pop() {
            if let Some(a) = ex.taskbar.on_event(ev) {
                ex.on_taskbar(a);
            }
        }
        while let Some(ev) = ex.menu.events.pop() {
            if let Some(a) = ex.menu.on_event(ev, &ex.apps) {
                ex.on_menu(a);
            }
        }

        // 2. Horloge et contenu de /desktop (chaque seconde).
        let now = Instant::now();
        if now >= next_clock {
            next_clock = now + Duration::from_secs(1);
            let c = read_clock();
            if c != ex.desktop.clock {
                ex.desktop.clock = c;
                desk_damage = Some(None);
            }
            if ex.desktop.rescan(&mut ex.apps, &mut ex.cache, false) {
                desk_damage = Some(None);
            }
        }
        ex.menu.tick(now);

        // 3. Fenêtres, lancements, dessin.
        let wins = snapshot();
        ex.resolve_pending(&wins);
        if !ex.stashed.is_empty() && wins.iter().any(|w| !w.hidden) {
            ex.stashed.clear(); // une fenêtre est revenue autrement : plus rien à restaurer
        }
        let model = ex.taskbar_model(&wins, ex.desktop.clock);
        ex.taskbar.update(&model);
        ex.menu.update(&ex.apps, &mut ex.cache);
        if let Some(r) = desk_damage {
            ex.desktop.redraw(&ex.apps, &mut ex.cache, r);
        }
        drop(wins);

        // 4. Attente : sonnette, horloge, animation d'un lancement, confirmation du menu.
        let mut deadline = next_clock;
        if !ex.pending.is_empty() {
            deadline = deadline.min(now + Duration::from_millis(150));
        }
        if let Some(d) = ex.menu.deadline() {
            deadline = deadline.min(d);
        }
        BELL.wait(deadline);
    }
}
