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
extern crate path;
extern crate mod_mgmt;
extern crate window_inner;
extern crate root;
extern crate fs_node;
extern crate memfs;
extern crate vfs_node;
extern crate io;

#[macro_use] extern crate log;

use alloc::vec::Vec;
use alloc::string::{String, ToString};
use alloc::format;
use color::Color;
use shapes::{Coord, Rectangle};
use window_inner::WindowInner;
use fs_node::{FileOrDir, DirRef};
use mai_ui::draw::DrawContext;
use mai_ui::theme;

// ================================================================
// CONFIG
// ================================================================
const TASKBAR_H:       usize = 48;
const ICON_W:          usize = 76;
const ICON_H:          usize = 76;
const ICON_INNER:      usize = 44;
const ICON_GAP:        usize = 20;
const ICONS_PER_ROW:   usize = 8;
const DESKTOP_PAD_X:   usize = 28;
const DESKTOP_PAD_Y:   usize = 28;
const TASKBAR_BTN_W:   usize = 152;
const TASKBAR_BTN_H:   usize = 34;
const TASKBAR_BTN_GAP: usize = 4;
const TASKBAR_BTN_X0:  usize = 58;

// Colors kept as local consts (not in mai_ui theme)
const C_TASKBAR:      Color = Color::new(0x0013141F);
const C_BTN_ACTIVE:   Color = Color::new(0x003D59A1);
const C_ICON_HOVER:   Color = Color::new(0x002A2F4C);

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
    hovered:  bool,
}

impl Icon {
    fn hit(&self, px: isize, py: isize) -> bool {
        px >= self.x && px < self.x + ICON_W as isize
        && py >= self.y && py < self.y + ICON_H as isize
    }
}

/// Construit la liste des icônes depuis les raccourcis lus dans /desktop.
fn build_icons(shortcuts: &[ShortcutDef]) -> Vec<Icon> {
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
            let col  = idx % ICONS_PER_ROW;
            let row  = idx / ICONS_PER_ROW;
            icons.push(Icon {
                x:        (DESKTOP_PAD_X + col * (ICON_W + ICON_GAP)) as isize,
                y:        (DESKTOP_PAD_Y + row * (ICON_H + ICON_GAP + 22)) as isize,
                app_path: full,
                name:     def.name.clone(),
                color:    def.color,
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
// DESSIN
// ================================================================

fn redraw(sw: usize, sh: usize, icons: &[Icon], apps: &[RunningApp]) {
    let wm_ref = match window_manager::WINDOW_MANAGER.get() { Some(r) => r, None => return };
    let mut wm = wm_ref.lock();
    let fb = wm.get_bottom_framebuffer_mut();
    let mut ctx = DrawContext::new(fb);

    // Fond dégradé subtil
    ctx.fill_rect(0, 0, sw, sh, theme::C_BG);
    for y in 0..180isize {
        let e = ((180-y)/10) as u32;
        ctx.hline(0, sw as isize, y, Color::new(0x001A1B26u32.saturating_add(e)));
    }

    // -- Icônes --
    for icon in icons {
        let bg = if icon.hovered { C_ICON_HOVER } else { theme::C_PANEL };
        // Ombre légère
        ctx.fill_rect(icon.x+3, icon.y+3, ICON_W, ICON_H, Color::new(0x000D0E17));
        ctx.rounded_rect(icon.x, icon.y, ICON_W, ICON_H, 8, bg);
        ctx.border_rect(icon.x, icon.y, ICON_W, ICON_H, theme::C_BORDER);
        // Carré coloré centré
        let ix = icon.x + ((ICON_W - ICON_INNER) / 2) as isize;
        let iy = icon.y + ((ICON_H - ICON_INNER) / 2) as isize - 4;
        ctx.rounded_rect(ix, iy, ICON_INNER, ICON_INNER, 6, icon.color);
        // Highlight si hover
        if icon.hovered {
            ctx.hline(icon.x+4, icon.x+ICON_W as isize-4, icon.y+1, Color::new(0x40FFFFFF));
        }
        // Label sous l'icône — truncate to fit icon width
        let max_chars = ICON_W / theme::CHAR_W;
        let label = if icon.name.len() > max_chars { &icon.name[..max_chars] } else { &icon.name };
        ctx.text_centered(icon.x, icon.y + ICON_H as isize - 14,
                          ICON_W, label, theme::C_FG);
    }

    // -- Taskbar --
    let ty = (sh - TASKBAR_H) as isize;
    ctx.fill_rect(0, ty, sw, TASKBAR_H, C_TASKBAR);
    ctx.hline(0, sw as isize, ty,   theme::C_ACCENT);
    ctx.hline(0, sw as isize, ty+1, Color::new(0x004A72C4));

    // Bouton Mai (logo M)
    let btn_y = ty + ((TASKBAR_H as isize - 34) / 2);
    let dk = Color::new(0x001A1B26);
    ctx.rounded_rect(10, btn_y, 36, 34, 6, theme::C_ACCENT);
    ctx.vline(14, btn_y+8, btn_y+26, dk);
    ctx.vline(30, btn_y+8, btn_y+26, dk);
    for d in 0..4isize {
        ctx.pixel(15+d, btn_y+9+d,  dk);
        ctx.pixel(29-d, btn_y+9+d,  dk);
    }

    // Boutons apps dans la taskbar
    for (i, app) in apps.iter().enumerate() {
        let r   = app.btn_rect(sh, i);
        let bx  = r.top_left.x;
        let by  = r.top_left.y;
        let bg  = if app.btn_hov             { theme::C_BTN_HOVER }
                  else if app.is_minimized() { theme::C_BTN       }
                  else                       { C_BTN_ACTIVE        };

        ctx.rounded_rect(bx, by, TASKBAR_BTN_W, TASKBAR_BTN_H, 5, bg);
        ctx.border_rect(bx, by, TASKBAR_BTN_W, TASKBAR_BTN_H, theme::C_ACCENT);
        ctx.rounded_rect(bx+7, by+(TASKBAR_BTN_H as isize-12)/2, 12, 12, 3, app.color);

        let lx = bx + 24;
        let ly = by + (TASKBAR_BTN_H as isize - theme::CHAR_H as isize) / 2;
        let max_chars = (TASKBAR_BTN_W - 28) / theme::CHAR_W;
        let label = if app.name.len() > max_chars { &app.name[..max_chars] } else { &app.name };
        ctx.text(lx, ly, label, theme::C_FG);

        if !app.is_minimized() {
            ctx.hline(bx+6, bx+TASKBAR_BTN_W as isize-6, by+TASKBAR_BTN_H as isize-3, theme::C_ACCENT);
            ctx.hline(bx+6, bx+TASKBAR_BTN_W as isize-6, by+TASKBAR_BTN_H as isize-2, theme::C_ACCENT);
        } else {
            let mid_y = by + TASKBAR_BTN_H as isize / 2;
            let mid_x = bx + TASKBAR_BTN_W as isize / 2;
            ctx.hline(mid_x-10, mid_x+10, mid_y,   theme::C_FG);
            ctx.hline(mid_x-10, mid_x+10, mid_y+1, theme::C_FG);
        }
    }

    // Heure fictive en haut à droite (placeholder)
    ctx.text(sw as isize - 70, ty + 8, "mai_os", theme::C_FG_DIM);

    drop(ctx);
    drop(wm);

    if let Some(wm) = window_manager::WINDOW_MANAGER.get() {
        let mut wm = wm.lock();
        let _ = wm.refresh_all();
        wm.present();
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
    let mut icons = build_icons(&shortcuts);
    info!("[explorer] {} icône(s) affichée(s)", icons.len());

    let mut apps:     Vec<RunningApp> = Vec::new();
    let mut dirty     = true;
    let mut prev_left = false;

    loop {
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

        if dirty { redraw(sw, sh, &icons, &apps); dirty = false; }

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
                        if let Some(app) = spawn_app(&icons[idx]) {
                            apps.push(app);
                            dirty = true;
                        }
                        break;
                    }
                }
            }
        }

        scheduler::schedule();
    }

    #[allow(unreachable_code)]
    0
}
