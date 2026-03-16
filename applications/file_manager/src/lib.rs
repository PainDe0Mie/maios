//! Gestionnaire de fichiers pour mai_os
//!
//! Navigation style Windows :
//!   "Root:/" au lieu de "/"
//!   "Root:/apps/shell" etc.
//!
//! Contrôles :
//!   UP/DOWN  — navigation dans la liste
//!   ENTER    — ouvrir dossier / lancer fichier
//!   BACKSPACE — dossier parent
//!   Q        — quitter

#![no_std]
extern crate alloc;
extern crate color;
extern crate font;
extern crate framebuffer;
extern crate framebuffer_drawer;
extern crate fs_node;
extern crate root;
extern crate path;
extern crate shapes;
extern crate task;
extern crate window;
extern crate window_manager;
extern crate scheduler;
extern crate event_types;
extern crate keycodes_ascii;
extern crate spawn;
extern crate mod_mgmt;

#[macro_use] extern crate log;

use alloc::vec::Vec;
use alloc::string::{String, ToString};
use alloc::format;
use alloc::sync::Arc;
use color::Color;
use shapes::Coord;
use framebuffer::{Framebuffer, AlphaPixel};
use fs_node::{DirRef, FileOrDir};
use event_types::Event;
use keycodes_ascii::{KeyAction, Keycode};

// ================================================================
// COULEURS
// ================================================================
const C_BG:         Color = Color::new(0x001A1B26);
const C_PANEL:      Color = Color::new(0x0024283A);
const C_BORDER:     Color = Color::new(0x00414868);
const C_ACCENT:     Color = Color::new(0x007AA2F7);
const C_FG:         Color = Color::new(0x00C0CAF5);
const C_FG_DIM:     Color = Color::new(0x00565F89);
const C_SELECTED:   Color = Color::new(0x002D3F6B);
const C_HOVER:      Color = Color::new(0x00252535);
const C_DIR:        Color = Color::new(0x00E0AF68);
const C_FILE:       Color = Color::new(0x00C0CAF5);
const C_EXEC:       Color = Color::new(0x009ECE6A);
const C_HEADER_BG:  Color = Color::new(0x00292E42);
const C_PATHBAR:    Color = Color::new(0x001E2030);

// ================================================================
// LAYOUT
// ================================================================
const WIN_W:      usize = 700;
const WIN_H:      usize = 480;
const PAD:        isize = 10;
const TITLEBAR_H: usize = 28;  // géré par window crate
const PATHBAR_H:  usize = 26;
const HEADER_H:   usize = 20;
const ROW_H:      usize = 20;
const COL_ICON:   isize = PAD;
const COL_NAME:   isize = PAD + 18;
const COL_TYPE:   isize = 440;
const COL_SIZE:   isize = 560;
const LIST_START: usize = PATHBAR_H + HEADER_H + 4;
const FOOTER_H:   usize = 22;

// ================================================================
// HELPERS DESSIN
// ================================================================
type Fb = Framebuffer<AlphaPixel>;

fn fill(fb: &mut Fb, x: isize, y: isize, w: usize, h: usize, c: Color) {
    framebuffer_drawer::fill_rectangle(fb, Coord::new(x, y), w, h, c.into());
}

fn hline(fb: &mut Fb, x0: isize, x1: isize, y: isize, c: Color) {
    for x in x0..x1 { fb.draw_pixel(Coord::new(x, y), c.into()); }
}

fn draw_char(fb: &mut Fb, x: isize, y: isize, ch: char, c: Color) {
    let idx = ch as usize;
    if idx >= 256 { return; }
    let bitmap = &font::FONT_BASIC[idx];
    for row in 0..font::CHARACTER_HEIGHT {
        let bits = bitmap[row];
        for col in 0..8usize {
            if bits & (0x80 >> col) != 0 {
                fb.draw_pixel(Coord::new(x + col as isize, y + row as isize), c.into());
            }
        }
    }
}

fn draw_text(fb: &mut Fb, x: isize, y: isize, text: &str, c: Color) {
    let mut cx = x;
    for ch in text.chars() {
        draw_char(fb, cx, y, ch, c);
        cx += font::CHARACTER_WIDTH as isize;
    }
}

fn draw_text_clipped(fb: &mut Fb, x: isize, y: isize, text: &str, max_px: isize, c: Color) {
    let max_chars = (max_px / font::CHARACTER_WIDTH as isize) as usize;
    let s: String = text.chars().take(max_chars).collect();
    draw_text(fb, x, y, &s, c);
}

// ================================================================
// CONVERSION CHEMIN : "/" → "Root:/"
// ================================================================
fn to_display_path(abs: &str) -> String {
    if abs.is_empty() || abs == "/" {
        return "Root:/".to_string();
    }
    format!("Root:{}", abs)
}

// ================================================================
// ENTRÉE DE LISTE
// ================================================================
#[derive(Clone)]
struct Entry {
    name:   String,
    is_dir: bool,
    /// true si le nom commence par un préfixe d'appli connu
    is_exec: bool,
}

impl Entry {
    fn type_str(&self) -> &'static str {
        if self.is_dir  { "Dossier" }
        else if self.is_exec { "Application" }
        else             { "Fichier" }
    }

    fn color(&self) -> Color {
        if self.is_dir  { C_DIR }
        else if self.is_exec { C_EXEC }
        else             { C_FILE }
    }

    fn icon(&self) -> &'static str {
        if self.is_dir { "[D]" } else if self.is_exec { "[X]" } else { "[F]" }
    }
}

fn is_executable(name: &str) -> bool {
    // Dans Theseus/mai_os, les apps ont un préfixe "a#" ou finissent par "-<hash>"
    // Heuristique : contient un '-' et pas de '.'  ou finit en suffixe connu
    (name.contains('-') && !name.contains('.'))
    || name.ends_with(".elf")
}

// ================================================================
// LECTURE D'UN RÉPERTOIRE
// ================================================================
fn list_dir(dir: &DirRef) -> Vec<Entry> {
    let locked = dir.lock();
    let mut names = locked.list();
    names.sort();

    names.into_iter().map(|name| {
        let is_dir = locked.get(&name)
            .map(|fod| fod.is_dir())
            .unwrap_or(false);
        let is_exec = !is_dir && is_executable(&name);
        Entry { name, is_dir, is_exec }
    }).collect()
}

// ================================================================
// RENDU
// ================================================================
fn redraw(
    fb: &mut Fb,
    ox: isize,   // offset X (border gauche)
    oy: isize,   // offset Y (titlebar + border haut)
    cw: usize,   // largeur contenu
    ch: usize,   // hauteur contenu
    entries: &[Entry],
    current_dir: &DirRef,
    selected: usize,
    scroll: usize,
) {
    // Toutes les coordonnées sont relatives au contenu (après titlebar/border)
    let w = cw as isize;
    let h = ch;

    // Fond contenu uniquement
    fill(fb, ox, oy, cw, h, C_BG);

    // ── Barre de chemin ─────────────────────────────────────────
    fill(fb, ox, oy, cw, PATHBAR_H, C_PATHBAR);
    hline(fb, ox, ox + w, oy + PATHBAR_H as isize, C_BORDER);

    let abs = current_dir.lock().get_absolute_path();
    let display = to_display_path(&abs);
    draw_text(fb, ox + PAD, oy + 5, &display, C_ACCENT);

    draw_text(fb, ox + w - 80, oy + 5, "<- Back", C_FG_DIM);

    // ── Header colonnes ─────────────────────────────────────────
    let hy = oy + PATHBAR_H as isize;
    fill(fb, ox, hy, cw, HEADER_H, C_HEADER_BG);
    hline(fb, ox, ox + w, hy + HEADER_H as isize - 1, C_ACCENT);

    draw_text(fb, ox + COL_NAME, hy + 2, "Nom", C_FG_DIM);
    draw_text(fb, ox + COL_TYPE, hy + 2, "Type", C_FG_DIM);
    draw_text(fb, ox + COL_SIZE, hy + 2, "Info", C_FG_DIM);

    // ── Liste ───────────────────────────────────────────────────
    let list_y  = oy + LIST_START as isize;
    let visible = (h.saturating_sub(LIST_START + FOOTER_H)) / ROW_H;

    for (i, entry) in entries.iter().skip(scroll).take(visible).enumerate() {
        let ry = list_y + (i * ROW_H) as isize;
        let abs_idx = scroll + i;

        // Fond ligne
        let bg = if abs_idx == selected { C_SELECTED }
                 else if i % 2 == 0     { C_BG }
                 else                    { C_HOVER };
        fill(fb, ox, ry, cw, ROW_H, bg);

        // Icône
        draw_text(fb, ox + COL_ICON, ry + 2, entry.icon(), entry.color());

        // Nom (tronqué)
        let max_name_px = COL_TYPE - COL_NAME - 8;
        draw_text_clipped(fb, ox + COL_NAME, ry + 2, &entry.name, max_name_px, entry.color());

        // Type
        draw_text(fb, ox + COL_TYPE, ry + 2, entry.type_str(), C_FG_DIM);

        // Info
        let info = if entry.is_dir { "-".to_string() } else { "".to_string() };
        draw_text(fb, ox + COL_SIZE, ry + 2, &info, C_FG_DIM);

        // Séparateur
        hline(fb, ox, ox + w, ry + ROW_H as isize - 1, Color::new(0x00252535));
    }

    // ── Scrollbar ────────────────────────────────────────────────
    if entries.len() > visible && visible > 0 {
        let sb_x  = ox + w - 7;
        fill(fb, sb_x, list_y, 5, visible * ROW_H, Color::new(0x00111115));
        let thumb_h = ((visible * ROW_H * visible) / entries.len().max(1)).max(12) as isize;
        let thumb_y = if entries.len() > visible {
            (scroll * visible * ROW_H / (entries.len() - visible + 1)) as isize
        } else { 0 };
        fill(fb, sb_x, list_y + thumb_y, 5, thumb_h as usize, C_BORDER);
    }

    // ── Footer ───────────────────────────────────────────────────
    let fy = oy + h as isize - FOOTER_H as isize;
    fill(fb, ox, fy, cw, FOOTER_H, C_PANEL);
    hline(fb, ox, ox + w, fy, C_BORDER);

    let count_str = format!("{} element(s)", entries.len());
    draw_text(fb, ox + PAD, fy + 3, &count_str, C_FG_DIM);
    draw_text(fb, ox + w / 2 - 120, fy + 3,
        "ENTER:ouvrir  BACK:parent  Q:quitter", C_FG_DIM);
}

// ================================================================
// LANCER UNE APPLICATION
// ================================================================
fn try_launch(name: &str) {
    // Cherche le fichier dans le namespace de la tâche courante
    let ns = match task::with_current_task(|t| t.namespace.clone()) {
        Ok(ns) => ns,
        Err(_) => { error!("[files] no current task namespace"); return; }
    };

    if let Some(file) = ns.dir().get_file_starting_with(name) {
        let abs = file.lock().get_absolute_path();
        let p   = path::Path::new(&abs);
        match mod_mgmt::create_application_namespace(None) {
            Ok(new_ns) => {
                match spawn::new_application_task_builder(p, Some(new_ns)) {
                    Ok(b) => { let _ = b.name(name.to_string()).spawn(); }
                    Err(e) => error!("[files] spawn builder: {}", e),
                }
            }
            Err(e) => error!("[files] namespace: {}", e),
        }
    } else {
        info!("[files] '{}' not launchable (not found in namespace)", name);
    }
}

// ================================================================
// POINT D'ENTRÉE
// ================================================================
pub fn main(_args: Vec<String>) -> isize {
    info!("[file_manager] Starting...");

    let mut win = match window::Window::with_title(
        "Gestionnaire de fichiers".to_string(),
        Coord::new(80, 50),
        WIN_W, WIN_H,
        C_BG,
    ) {
        Ok(w)  => w,
        Err(e) => { error!("[file_manager] window: {}", e); return -1; }
    };

    // Récupère l'offset du contenu (après titlebar + bordure)
    // win.area() retourne un Rectangle relatif au coin haut-gauche de la fenêtre
    let content_area = win.area();
    let off_x = content_area.top_left.x;
    let off_y = content_area.top_left.y;
    let content_w = (content_area.bottom_right.x - content_area.top_left.x) as usize;
    let content_h = (content_area.bottom_right.y - content_area.top_left.y) as usize;

    // Démarre à la racine
    let mut current_dir: DirRef = root::get_root().clone();
    let mut entries  = list_dir(&current_dir);
    let mut selected = 0usize;
    let mut scroll   = 0usize;
    let mut dirty    = true;

    loop {
        // ── Événements ──────────────────────────────────────────
        loop {
            match win.handle_event() {
                Ok(Some(Event::KeyboardEvent(ke))) => {
                    if ke.key_event.action != KeyAction::Pressed { continue; }
                    let visible = (WIN_H - LIST_START - FOOTER_H) / ROW_H;

                    match ke.key_event.keycode {
                        // Quitter
                        Keycode::Q => return 0,

                        // Navigation
                        Keycode::Up => {
                            if selected > 0 { selected -= 1; }
                            if selected < scroll { scroll = selected; }
                            dirty = true;
                        }
                        Keycode::Down => {
                            if selected + 1 < entries.len() { selected += 1; }
                            if selected >= scroll + visible {
                                scroll = selected.saturating_sub(visible - 1);
                            }
                            dirty = true;
                        }

                        // Ouvrir dossier ou lancer fichier
                        Keycode::Enter => {
                            if let Some(entry) = entries.get(selected) {
                                if entry.is_dir {
                                    // Navigue dans le sous-dossier
                                    let locked = current_dir.lock();
                                    if let Some(FileOrDir::Dir(sub)) = locked.get(&entry.name) {
                                        drop(locked);
                                        current_dir = sub;
                                        entries  = list_dir(&current_dir);
                                        selected = 0;
                                        scroll   = 0;
                                        dirty    = true;
                                    }
                                } else if entry.is_exec {
                                    try_launch(&entry.name);
                                }
                            }
                        }

                        // Dossier parent
                        Keycode::Backspace => {
                            let parent = current_dir.lock().get_parent_dir();
                            if let Some(p) = parent {
                                current_dir = p;
                                entries  = list_dir(&current_dir);
                                selected = 0;
                                scroll   = 0;
                                dirty    = true;
                            }
                        }

                        // Page Up / Page Down
                        Keycode::PageUp => {
                            selected = selected.saturating_sub(visible);
                            scroll   = scroll.saturating_sub(visible);
                            dirty    = true;
                        }
                        Keycode::PageDown => {
                            selected = (selected + visible).min(entries.len().saturating_sub(1));
                            if selected >= scroll + visible {
                                scroll = selected.saturating_sub(visible - 1);
                            }
                            dirty = true;
                        }

                        _ => {}
                    }
                }
                Ok(Some(Event::MousePositionEvent(me))) => {
                    // Clic gauche sur une ligne
                    if me.left_button_hold {
                        // Les coords souris sont relatives au contenu de la fenêtre
                        let click_y = me.coordinate.y;
                        let list_top = LIST_START as isize;
                        if click_y >= list_top {
                            let idx = ((click_y - list_top) / ROW_H as isize) as usize + scroll;
                            if idx < entries.len() {
                                selected = idx;
                                dirty    = true;
                            }
                        }
                        // Clic sur "Back"
                        let w = content_w as isize;
                        if me.coordinate.x >= w - 80 && me.coordinate.y < PATHBAR_H as isize {
                            let parent = current_dir.lock().get_parent_dir();
                            if let Some(p) = parent {
                                current_dir = p;
                                entries  = list_dir(&current_dir);
                                selected = 0;
                                scroll   = 0;
                                dirty    = true;
                            }
                        }
                    }
                }
                Ok(None) => break,
                Ok(Some(_)) => {}
                Err(_) => break,
            }
        }

        // ── Redraw ──────────────────────────────────────────────
        if dirty {
            {
                let mut fb = win.framebuffer_mut();
                redraw(&mut fb, off_x, off_y, content_w, content_h,
                       &entries, &current_dir, selected, scroll);
            }
            if let Err(e) = win.render(None) {
                error!("[file_manager] render: {}", e);
            }
            dirty = false;
        }

        scheduler::schedule();
    }

    #[allow(unreachable_code)]
    0
}