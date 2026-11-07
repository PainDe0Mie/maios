//! Explorer — gestionnaire de bureau pour mai_os
//!
//! Les icônes du bureau sont lues depuis le dossier `root:/desktop/`.
//! Chaque fichier `.desk` dans ce dossier décrit un raccourci :
//!
//!   name=Terminal
//!   prefix=shell-
//!   color=1ABC9C
//!
//! Pour ajouter une app au bureau : créer un fichier .desk dans /desktop.
//! Pour retirer une app : supprimer le fichier .desk.
//!
//! Les raccourcis par défaut (Terminal, TaskManager, Fichiers) sont créés
//! au premier boot par `desktop_init::init()`.

#![no_std]
extern crate alloc;
extern crate color;
extern crate mai_ui;
extern crate shapes;
extern crate spawn;
extern crate task;
extern crate window_manager;
extern crate scheduler;
extern crate sleep;
extern crate path;
extern crate mod_mgmt;
extern crate window_inner;
extern crate root;
extern crate fs_node;
extern crate memfs;
extern crate vfs_node;
extern crate io;
extern crate framebuffer;
extern crate font;
#[cfg(target_arch = "x86_64")]
extern crate rtc;

mod paint;

#[macro_use] extern crate log;

use alloc::vec::Vec;
use alloc::string::{String, ToString};
use alloc::format;
use color::Color;
use shapes::{Coord, Rectangle};
use window_inner::WindowInner;
use fs_node::{FileOrDir, DirRef};
use framebuffer::AlphaPixel;
use mai_ui::theme;
use paint::{rgb, Canvas, Glyph, Rgb, SegStyle, BLACK, WHITE};

// ================================================================
// CONFIG
// ================================================================
const TASKBAR_H:       usize = 48;
const ICON_W:          usize = 100;
const ICON_H:          usize = 88;
const ICON_TILE:       isize = 48;
const ICON_GAP:        usize = 10;
const DESKTOP_PAD_X:   usize = 20;
const DESKTOP_PAD_Y:   usize = 22;
const TASKBAR_BTN_W:   usize = 152;
const TASKBAR_BTN_H:   usize = 36;
const TASKBAR_BTN_GAP: usize = 6;
const TASKBAR_BTN_X0:  usize = 62;

// Palette locale (Tokyo Night) en complément de mai_ui::theme
const C_TASKBAR:   Rgb = rgb(0x12131F);
const C_TEXT:      Rgb = rgb(0xE4E9FF);
const C_TEXT_SOFT: Rgb = rgb(0xA9B1D6);
const C_TEXT_DIM:  Rgb = rgb(0x737AA2);

// ================================================================
// FORMAT DES RACCOURCIS .desk
// ================================================================
//
// Fichier texte UTF-8, une clé=valeur par ligne :
//   name=Terminal          ← nom affiché sous l'icône
//   prefix=shell-          ← préfixe du crate dans le namespace
//   color=1ABC9C           ← couleur hex RGB sans #
//
// Les lignes commençant par # sont ignorées.

#[derive(Clone)]
struct ShortcutDef {
    name:   String,
    prefix: String,
    color:  Color,
}

/// Parse un fichier .desk (contenu texte) en ShortcutDef.
fn parse_desk(content: &str) -> Option<ShortcutDef> {
    let mut name   = None;
    let mut prefix = None;
    let mut color  = None;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        if let Some(v) = line.strip_prefix("name=")   { name   = Some(v.to_string()); }
        if let Some(v) = line.strip_prefix("prefix=") { prefix = Some(v.to_string()); }
        if let Some(v) = line.strip_prefix("color=")  {
            let rgb = u32::from_str_radix(v.trim(), 16).unwrap_or(0xC0CAF5);
            color = Some(Color::new(rgb));
        }
    }

    Some(ShortcutDef {
        name:   name?,
        prefix: prefix?,
        color:  color.unwrap_or(Color::new(0x00C0CAF5)),
    })
}

// ================================================================
// LECTURE DU DOSSIER BUREAU
// ================================================================

/// Lit `root:/desktop/` et retourne tous les raccourcis valides.
fn load_shortcuts() -> Vec<ShortcutDef> {
    let desktop_dir = {
        let root = root::get_root();
        // Drop lock immédiatement après get
        let dir = root.lock().get("desktop");
        match dir {
            Some(FileOrDir::Dir(d)) => d,
            _ => {
                warn!("[explorer] root:/desktop/ introuvable — bureau vide");
                return Vec::new();
            }
        }
    };

    let names = desktop_dir.lock().list();
    let mut shortcuts = Vec::new();

    for name in &names {
        if !name.ends_with(".desk") { continue; }

        let file = match desktop_dir.lock().get(name) {
            Some(FileOrDir::File(f)) => f,
            _ => continue,
        };

        // Lire le contenu du fichier
        let len = file.lock().len();
        let mut buf = alloc::vec![0u8; len];
        use io::ByteReader;
        let _ = file.lock().read_at(&mut buf, 0);

        if let Ok(text) = core::str::from_utf8(&buf) {
            if let Some(def) = parse_desk(text) {
                shortcuts.push(def);
            }
        }
    }

    shortcuts
}

// ================================================================
// ICÔNE
// ================================================================
struct Icon {
    x: isize, y: isize,
    app_path: String,
    name:     String,
    color:    Color,
    glyph:    Glyph,
    hovered:  bool,
}

/// Choisit le pictogramme d'une app d'après le préfixe de son crate.
fn glyph_for(def: &ShortcutDef) -> Glyph {
    match def.prefix.as_str() {
        "shell-"        => Glyph::Terminal,
        "task_manager-" => Glyph::Monitor,
        "file_manager-" => Glyph::Folder,
        _ => Glyph::Letter(
            def.name.chars().next().map(|c| c.to_ascii_uppercase()).unwrap_or('?'),
        ),
    }
}

impl Icon {
    fn hit(&self, px: isize, py: isize) -> bool {
        px >= self.x && px < self.x + ICON_W as isize
        && py >= self.y && py < self.y + ICON_H as isize
    }
}

/// Construit la liste des icônes depuis les raccourcis lus dans /desktop.
fn build_icons(shortcuts: &[ShortcutDef], sh: usize) -> Vec<Icon> {
    // Les icônes s'empilent en colonnes à gauche, de haut en bas.
    let per_col = ((sh.saturating_sub(TASKBAR_H + DESKTOP_PAD_Y)) / (ICON_H + ICON_GAP)).max(1);

    let ns_dir = match task::with_current_task(|t| t.namespace.dir().clone()) {
        Ok(d)  => d,
        Err(_) => { error!("[explorer] no current task"); return Vec::new(); }
    };

    let mut icons = Vec::new();
    let mut idx = 0usize;

    for def in shortcuts {
        // Cherche le crate dans le namespace (ex: "shell-abc123")
        if let Some(file) = ns_dir.get_file_starting_with(&def.prefix) {
            let full = file.lock().get_absolute_path();
            let col  = idx / per_col;
            let row  = idx % per_col;
            icons.push(Icon {
                x:        (DESKTOP_PAD_X + col * (ICON_W + ICON_GAP)) as isize,
                y:        (DESKTOP_PAD_Y + row * (ICON_H + ICON_GAP)) as isize,
                app_path: full,
                name:     def.name.clone(),
                color:    def.color,
                glyph:    glyph_for(def),
                hovered:  false,
            });
            idx += 1;
        } else {
            warn!("[explorer] App '{}' (prefix='{}') introuvable dans le namespace",
                  def.name, def.prefix);
        }
    }

    icons
}

// ================================================================
// APP EN COURS (taskbar)
// ================================================================
#[derive(PartialEq, Clone, Copy)]
enum AppState { Normal, Minimized }

struct RunningApp {
    name:         String,
    color:        Color,
    task:         task::JoinableTaskRef,
    task_id:      usize,
    state:        AppState,
    btn_hov:      bool,
    window_seen:  bool,
    window_ref:   Option<alloc::sync::Arc<spin::Mutex<WindowInner>>>,
}

impl RunningApp {
    fn is_minimized(&self) -> bool { self.state == AppState::Minimized }

    fn update_window_seen(&mut self) {
        if self.window_seen { return; }
        if let Some(wm_ref) = window_manager::WINDOW_MANAGER.get() {
            let wm = wm_ref.lock();
            if let Some(win) = wm.get_window_by_task_id(self.task_id) {
                self.window_seen = true;
                self.window_ref = Some(win);
                return;
            }
            if let Some(active) = wm.get_active_window() {
                let win_tid = active.lock().task_id;
                if let Some(wtid) = win_tid {
                    if wtid > self.task_id && wtid < self.task_id + 10 {
                        self.window_seen = true;
                        self.window_ref = Some(active);
                    }
                }
            }
        }
    }

    fn should_remove(&self) -> bool {
        if self.task.has_exited() { return true; }
        if self.window_seen {
            if let Some(ref win) = self.window_ref {
                if let Some(wm_ref) = window_manager::WINDOW_MANAGER.get() {
                    let tid = win.lock().task_id.unwrap_or(0);
                    return wm_ref.lock().get_window_by_task_id(tid).is_none();
                }
            }
        }
        false
    }

    fn btn_rect(&self, sh: usize, i: usize) -> Rectangle {
        let x = (TASKBAR_BTN_X0 + i * (TASKBAR_BTN_W + TASKBAR_BTN_GAP)) as isize;
        let y = (sh - TASKBAR_H + (TASKBAR_H - TASKBAR_BTN_H) / 2) as isize;
        Rectangle {
            top_left:     Coord::new(x, y),
            bottom_right: Coord::new(x + TASKBAR_BTN_W as isize, y + TASKBAR_BTN_H as isize),
        }
    }
}

// ================================================================
// HORLOGE
// ================================================================

#[derive(Clone, Copy, PartialEq, Eq)]
struct Clock {
    hour:  u8,
    min:   u8,
    day:   u8,
    month: u8,
    year:  u16,
}

#[cfg(target_arch = "x86_64")]
fn read_clock() -> Option<Clock> {
    let t = rtc::read_rtc();
    if t.months == 0 || t.months > 12 || t.days == 0 || t.days > 31 {
        return None;
    }
    Some(Clock {
        hour:  t.hours % 24,
        min:   t.minutes % 60,
        day:   t.days,
        month: t.months,
        year:  2000 + t.years as u16,
    })
}

#[cfg(not(target_arch = "x86_64"))]
fn read_clock() -> Option<Clock> { None }

const JOURS: [&str; 7] = ["Dimanche", "Lundi", "Mardi", "Mercredi", "Jeudi", "Vendredi", "Samedi"];
const MOIS: [&str; 12] = [
    "janvier", "février", "mars", "avril", "mai", "juin",
    "juillet", "août", "septembre", "octobre", "novembre", "décembre",
];

/// Jour de la semaine (0 = dimanche), algorithme de Sakamoto.
fn weekday(year: u16, month: u8, day: u8) -> usize {
    const T: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let mut y = year as i32;
    if month < 3 { y -= 1; }
    let m = (month.clamp(1, 12) - 1) as usize;
    (y + y / 4 - y / 100 + y / 400 + T[m] + day as i32).rem_euclid(7) as usize
}

impl Clock {
    fn long_date(&self) -> String {
        format!("{} {} {} {}",
                JOURS[weekday(self.year, self.month, self.day)],
                self.day,
                MOIS[(self.month.clamp(1, 12) - 1) as usize],
                self.year)
    }

    fn short_date(&self) -> String {
        format!("{:02}/{:02}/{}", self.day, self.month, self.year)
    }

    fn greeting(&self) -> &'static str {
        match self.hour {
            5..=11  => "Bonjour",
            12..=17 => "Bon après-midi",
            _       => "Bonsoir",
        }
    }
}

// ================================================================
// DESSIN
// ================================================================

fn redraw(wall: &[AlphaPixel], icons: &[Icon], apps: &[RunningApp], clock: Option<Clock>) {
    let wm_ref = match window_manager::WINDOW_MANAGER.get() { Some(r) => r, None => return };
    let mut wm = wm_ref.lock();
    {
        let fb = wm.get_bottom_framebuffer_mut();
        let (w, h) = (fb.width(), fb.height());
        let buf = fb.buffer_mut();
        if buf.len() != wall.len() || buf.len() < w * h {
            warn!("[explorer] taille du fond d'écran incohérente");
            return;
        }
        // Le fond d'écran est calculé une seule fois puis recopié.
        buf.copy_from_slice(wall);

        let mut c = Canvas::new(buf, w, h);
        draw_clock_widget(&mut c, clock);
        for icon in icons {
            draw_icon(&mut c, icon);
        }
        draw_taskbar(&mut c, apps, clock);
    }
    let _ = wm.refresh_all();
    wm.present();
}

/// Grande horloge en haut à droite du bureau.
fn draw_clock_widget(c: &mut Canvas, clock: Option<Clock>) {
    let clk = match clock { Some(k) => k, None => return };
    let accent: Rgb = theme::C_ACCENT.into();
    let purple: Rgb = theme::C_PURPLE.into();

    let st = SegStyle { w: 30, h: 56, t: 7 };
    let digits_w = 4 * st.w + 2 * (st.w / 3) + 2 * (st.w / 3 + 2) + st.t;
    let right = c.width() - 44;
    let top = 38;
    let x = right - digits_w;

    // Halo coloré diffus derrière les chiffres, ombre, puis chiffres.
    c.clock_digits(x - 1, top + 1, clk.hour, clk.min, st, accent, 40);
    c.clock_digits(x + 2, top + 4, clk.hour, clk.min, st, BLACK, 120);
    c.clock_digits(x, top, clk.hour, clk.min, st, C_TEXT, 255);

    // Petit trait d'accent en dégradé
    let bar_y = top + st.h + 14;
    for i in 0..40isize {
        let col = accent.mix(purple, (i * 256 / 40) as u32);
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

/// Icône du bureau : tuile en dégradé + pictogramme + libellé.
fn draw_icon(c: &mut Canvas, icon: &Icon) {
    let (x, y) = (icon.x, icon.y);
    let (w, h) = (ICON_W as isize, ICON_H as isize);

    if icon.hovered {
        c.rrect(x, y, w, h, 12, WHITE, 30);
        c.rrect_outline(x, y, w, h, 12, WHITE, 55);
    }

    let base: Rgb = icon.color.into();
    let tx = x + (w - ICON_TILE) / 2;
    let ty = y + 8;
    c.shadow(tx, ty, ICON_TILE, ICON_TILE, 13, 150);
    c.rrect_grad(tx, ty, ICON_TILE, ICON_TILE, 13, base.lighten(70), base.darken(70), 255);
    c.rrect_outline(tx, ty, ICON_TILE, ICON_TILE, 13, WHITE, 45);
    c.glyph(icon.glyph, tx, ty, ICON_TILE, base);

    let max_chars = (w / paint::CHAR_W) as usize;
    let label: String = icon.name.chars().take(max_chars).collect();
    let lw = paint::text_width(&label, 1, 0);
    c.text_shadow(x + (w - lw) / 2, ty + ICON_TILE + 10, &label, C_TEXT);
}

/// Logo « M » du bouton MaiOS (avec ombre).
fn draw_mai_logo(c: &mut Canvas, x: isize, y: isize) {
    for &(d, col, a) in &[(1isize, BLACK, 70u32), (0, WHITE, 255)] {
        let (ox, oy) = (x + d, y + d);
        c.fill(ox + 9, oy + 10, 3, 16, col, a);
        c.fill(ox + 24, oy + 10, 3, 16, col, a);
        for i in 0..7 {
            c.fill(ox + 11 + i, oy + 11 + i, 2, 2, col, a);
            c.fill(ox + 23 - i, oy + 11 + i, 2, 2, col, a);
        }
    }
}

/// Barre des tâches translucide : bouton MaiOS, apps ouvertes, horloge.
fn draw_taskbar(c: &mut Canvas, apps: &[RunningApp], clock: Option<Clock>) {
    let (w, h) = (c.width(), c.height());
    let th = TASKBAR_H as isize;
    let ty = h - th;
    let accent: Rgb = theme::C_ACCENT.into();
    let purple: Rgb = theme::C_PURPLE.into();

    // Ombre douce au-dessus de la barre, puis verre fumé.
    for i in 0..16isize {
        c.fill(0, ty - 16 + i, w, 1, BLACK, (i as u32 + 1) * 4);
    }
    c.fill(0, ty, w, th, C_TASKBAR, 205);
    c.fill(0, ty, w, 1, WHITE, 32);
    c.fill(0, ty + 1, w, 1, WHITE, 8);

    // Bouton MaiOS
    let (bx, by, bs) = (12isize, ty + 6, 36isize);
    c.shadow(bx, by, bs, bs, 10, 90);
    c.rrect_grad(bx, by, bs, bs, 10, accent.lighten(40), purple, 255);
    c.rrect_outline(bx, by, bs, bs, 10, WHITE, 60);
    draw_mai_logo(c, bx, by);

    // Séparateur
    c.fill(TASKBAR_BTN_X0 as isize - 7, ty + 12, 1, th - 24, WHITE, 32);

    // Applications ouvertes
    for (i, app) in apps.iter().enumerate() {
        let r = app.btn_rect(h as usize, i);
        let (x, y) = (r.top_left.x, r.top_left.y);
        let (bw, bh) = (TASKBAR_BTN_W as isize, TASKBAR_BTN_H as isize);
        let minimized = app.is_minimized();
        let (fill_a, edge_a) = if app.btn_hov { (44, 64) }
                               else if minimized { (10, 20) }
                               else { (26, 42) };
        c.rrect(x, y, bw, bh, 9, WHITE, fill_a);
        c.rrect_outline(x, y, bw, bh, 9, WHITE, edge_a);

        let col: Rgb = app.color.into();
        c.rrect_grad(x + 11, y + (bh - 14) / 2, 14, 14, 7,
                     col.lighten(70), col.darken(40), if minimized { 140 } else { 255 });

        let max_chars = ((bw - 44) / paint::CHAR_W) as usize;
        let label: String = app.name.chars().take(max_chars).collect();
        c.text(x + 34, y + (bh - paint::CHAR_H) / 2, &label,
               if minimized { C_TEXT_DIM } else { C_TEXT });

        if minimized {
            c.rrect(x + bw / 2 - 3, y + bh - 4, 6, 3, 1, C_TEXT_DIM, 255);
        } else {
            c.rrect(x + bw / 2 - 12, y + bh - 4, 24, 3, 1, accent, 255);
        }
    }

    // Horloge compacte
    let right = w - 16;
    match clock {
        Some(clk) => {
            let time = format!("{:02}:{:02}", clk.hour, clk.min);
            let date = clk.short_date();
            c.text(right - paint::text_width(&time, 1, 0), ty + 7, &time, C_TEXT);
            c.text(right - paint::text_width(&date, 1, 0), ty + 25, &date, C_TEXT_SOFT);
        }
        None => {
            let s = "MaiOS";
            c.text(right - paint::text_width(s, 1, 0), ty + 16, s, C_TEXT_SOFT);
        }
    }
}

// ================================================================
// LANCER UNE APP
// ================================================================
fn spawn_app(icon: &Icon) -> Option<RunningApp> {
    let p = path::Path::new(&icon.app_path);
    match mod_mgmt::create_application_namespace(None) {
        Ok(ns) => match spawn::new_application_task_builder(&p, Some(ns)) {
            Ok(b) => match b.name(format!("app_{}", icon.name)).spawn() {
                Ok(j) => {
                    let id = j.0.id;
                    Some(RunningApp {
                        name:        icon.name.clone(),
                        color:       icon.color,
                        task:        j,
                        task_id:     id,
                        state:       AppState::Normal,
                        btn_hov:     false,
                        window_seen: false,
                        window_ref:  None,
                    })
                }
                Err(e) => { warn!("[explorer] Spawn: {}", e); None }
            },
            Err(e) => { warn!("[explorer] Builder: {}", e); None }
        },
        Err(e) => { warn!("[explorer] Namespace: {}", e); None }
    }
}

// ================================================================
// INIT DU BUREAU (crée /desktop avec les raccourcis par défaut)
// ================================================================

/// Contenu par défaut des fichiers .desk
const DEFAULT_SHORTCUTS: &[(&str, &str, &str, &str)] = &[
    // (nom_fichier, name, prefix, color_hex)
    ("terminal.desk",    "Terminal",   "shell-",        "1ABC9C"),
    ("taskmanager.desk", "TaskManager","task_manager-", "9B59B6"),
    ("files.desk",       "Fichiers",   "file_manager-", "E0AF68"),
];

fn init_desktop() {
    let root = root::get_root();

    // Vérifie si /desktop existe déjà — drop le lock AVANT de créer
    let desktop_exists = {
        let locked = root.lock();
        matches!(locked.get("desktop"), Some(FileOrDir::Dir(_)))
    };
    // lock complètement relâché ici avant tout appel récursif sur root

    if desktop_exists {
        info!("[explorer] /desktop existe déjà");
        return;
    }

    let desktop: DirRef = match vfs_node::VFSDirectory::create("desktop".to_string(), root) {
        Ok(d)  => d,
        Err(e) => { error!("[explorer] Impossible de créer /desktop: {}", e); return; }
    };

    // Crée les fichiers .desk par défaut
    for (filename, name, prefix, color) in DEFAULT_SHORTCUTS {
        let content = format!("name={}\nprefix={}\ncolor={}\n", name, prefix, color);
        let bytes = content.as_bytes();
        match memfs::MemFile::create(filename.to_string(), &desktop) {
            Ok(file) => {
                use io::ByteWriter;
                let mut locked = file.lock();
                let _ = locked.write_at(bytes, 0);
                info!("[explorer] Raccourci créé: /desktop/{}", filename);
            }
            Err(e) => warn!("[explorer] Impossible de créer {}: {}", filename, e),
        }
    }
}

// ================================================================
// POINT D'ENTRÉE
// ================================================================
pub fn main(_args: Vec<String>) -> isize {
    info!("[explorer] Starting...");

    let (sw, sh) = match window_manager::WINDOW_MANAGER.get() {
        Some(r) => r.lock().get_screen_size(),
        None => { error!("[explorer] No WM"); return -1; }
    };

    // -- Init bureau (crée /desktop si besoin) --
    init_desktop();

    // -- Charge les raccourcis depuis /desktop --
    let shortcuts = load_shortcuts();
    info!("[explorer] {} raccourci(s) trouvé(s) dans /desktop", shortcuts.len());

    // -- Construit les icônes --
    let mut icons = build_icons(&shortcuts, sh);
    info!("[explorer] {} icône(s) affichée(s)", icons.len());

    // -- Fond d'écran procédural (calculé une seule fois) --
    let (fb_w, fb_h) = {
        let wm = window_manager::WINDOW_MANAGER.get().unwrap().lock();
        let fb = wm.get_bottom_framebuffer();
        (fb.width(), fb.height())
    };
    let wall = paint::wallpaper(fb_w, fb_h);
    info!("[explorer] Fond d'écran généré ({}x{}, écran {}x{})", fb_w, fb_h, sw, sh);

    let mut apps:     Vec<RunningApp> = Vec::new();
    let mut dirty     = true;
    let mut prev_left = false;
    let mut clock     = read_clock();
    let mut tick: u32 = 0;

    loop {
        // Horloge : relue ~1 fois par seconde, redessin à chaque nouvelle minute.
        tick = tick.wrapping_add(1);
        if tick % 60 == 0 {
            let now = read_clock();
            if now != clock { clock = now; dirty = true; }
        }

        // Mise à jour window_seen
        for app in apps.iter_mut() {
            let was_seen = app.window_seen;
            app.update_window_seen();
            if !was_seen && app.window_seen { dirty = true; }
        }

        // Nettoyage apps fermées
        let before = apps.len();
        apps.retain(|a| !a.should_remove());
        if apps.len() != before { dirty = true; }

        // État souris
        let (mx, my, left) = {
            let wm = window_manager::WINDOW_MANAGER.get().unwrap().lock();
            let pos = wm.mouse_position();
            (pos.x, pos.y, wm.mouse_left())
        };

        // Hover icônes
        for icon in icons.iter_mut() {
            let h = icon.hit(mx, my);
            if icon.hovered != h { icon.hovered = h; dirty = true; }
        }
        // Hover taskbar
        for (i, app) in apps.iter_mut().enumerate() {
            let r = app.btn_rect(sh, i);
            let h = mx >= r.top_left.x && mx < r.bottom_right.x
                 && my >= r.top_left.y && my < r.bottom_right.y;
            if app.btn_hov != h { app.btn_hov = h; dirty = true; }
        }

        if dirty { redraw(&wall, &icons, &apps, clock); dirty = false; }

        // Clic (front → relâché)
        let clicked = prev_left && !left;
        prev_left = left;

        if clicked {
            let mut handled = false;

            // Clic taskbar
            for i in 0..apps.len() {
                let r = apps[i].btn_rect(sh, i);
                if mx >= r.top_left.x && mx < r.bottom_right.x
                    && my >= r.top_left.y && my < r.bottom_right.y
                {
                    if apps[i].is_minimized() {
                        apps[i].state = AppState::Normal;
                        if let Some(ref win) = apps[i].window_ref {
                            if let Some(wm_ref) = window_manager::WINDOW_MANAGER.get() {
                                let _ = wm_ref.lock().set_active(win, true);
                            }
                        }
                    } else {
                        apps[i].state = AppState::Minimized;
                        if let Some(ref win) = apps[i].window_ref {
                            if let Some(wm_ref) = window_manager::WINDOW_MANAGER.get() {
                                let _ = wm_ref.lock().hide_window(win);
                            }
                        }
                    }
                    dirty   = true;
                    handled = true;
                    break;
                }
            }

            // Clic icône bureau
            if !handled {
                for idx in 0..icons.len() {
                    if icons[idx].hit(mx, my) {
                        // Ne pas relancer une app déjà en cours
                        let already_running = apps.iter().any(|a| a.name == icons[idx].name && !a.task.has_exited());
                        if !already_running {
                            if let Some(app) = spawn_app(&icons[idx]) {
                                apps.push(app);
                                dirty = true;
                            }
                        }
                        break;
                    }
                }
            }
        }

        // Use sleep instead of bare schedule() — EEVDF needs tasks to
        // explicitly block so they are dequeued from the CFS tree.
        // A bare schedule() yield keeps the task in the tree and may
        // cause starvation with EEVDF's vruntime-based selection.
        let _ = sleep::sleep(sleep::Duration::from_millis(16));
    }

    #[allow(unreachable_code)]
    0
}
