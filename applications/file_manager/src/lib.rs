//! Gestionnaire de fichiers — Mai OS
//!
//! Architecture : sidebar (favoris + arbre) + panneau principal (liste détaillée)
//!
//! Souris :
//!   Clic simple        — sélectionne
//!   Double-clic        — ouvre dossier / lance fichier
//!   Clic sidebar       — navigation rapide
//!   Molette (PageUp/Down pour l'instant)
//!
//! Clavier :
//!   ↑ / ↓             — navigation
//!   Entrée            — ouvre / lance
//!   Retour arrière    — dossier parent
//!   PageUp / PageDown — défilement rapide
//!   Home / End        — début / fin de liste
//!   Suppr / X         — supprimer le fichier/dossier sélectionné
//!   F5 / R            — rafraîchir le répertoire
//!   Q                 — quitter

#![no_std]
extern crate alloc;
extern crate color;
extern crate framebuffer;
extern crate mai_ui;
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
use color::Color;
use shapes::Coord;
use framebuffer::{Framebuffer, AlphaPixel};
use mai_ui::draw::DrawContext;
use mai_ui::theme;
use fs_node::{DirRef, FileOrDir};
use event_types::Event;
use keycodes_ascii::{KeyAction, Keycode};

// ────────────────────────────────────────────────────────────────
// APP-SPECIFIC COLORS (not in theme)
// ────────────────────────────────────────────────────────────────
const C_SEL_ACT:  Color = Color::new(0x003D5480);
const C_STRIPE:   Color = Color::new(0x001E2030);
const C_FILE:     Color = Color::new(0x00A9B1D6);
const C_DISK:     Color = Color::new(0x0089DDFF);  // bleu clair pour volumes disque

// ────────────────────────────────────────────────────────────────
// DIMENSIONS FENÊTRE
// ────────────────────────────────────────────────────────────────
const WIN_W: usize = 850;
const WIN_H: usize = 560;

// ────────────────────────────────────────────────────────────────
// LAYOUT INTERNE (coords relatives au contenu de la fenêtre)
// ────────────────────────────────────────────────────────────────
const PATHBAR_H:   usize = 28;
const TOOLBAR_H:   usize = 24;
const SIDEBAR_W:   usize = 170;
const HEADER_H:    usize = 22;
const ROW_H:       usize = 20;
const FOOTER_H:    usize = 22;
const SCROLLBAR_W: usize = 8;

/// Largeur du panneau principal (hors sidebar et scrollbar)
fn main_w(cw: usize) -> usize {
    cw.saturating_sub(SIDEBAR_W).saturating_sub(SCROLLBAR_W)
}

/// Y du premier pixel de la liste (relatif au contenu)
const LIST_TOP: usize = PATHBAR_H + TOOLBAR_H + HEADER_H;

/// Nombre de lignes visibles dans content_h pixels
fn visible_rows(ch: usize) -> usize {
    ch.saturating_sub(LIST_TOP + FOOTER_H) / ROW_H
}

// ────────────────────────────────────────────────────────────────
// COLONNES dans le panneau principal
// ────────────────────────────────────────────────────────────────
const C_ICON: isize = 4;
const C_NAME: isize = 20;

// ────────────────────────────────────────────────────────────────
// CHEMIN AFFICHÉ
// ────────────────────────────────────────────────────────────────
fn display_path(dir: &DirRef) -> String {
    let abs = dir.lock().get_absolute_path();
    if abs.is_empty() || abs == "/" {
        "Root:/".to_string()
    } else {
        format!("Root:{}", abs)
    }
}

// ────────────────────────────────────────────────────────────────
// ENTRÉE DE RÉPERTOIRE (avec taille)
// ────────────────────────────────────────────────────────────────
#[derive(Clone)]
struct Entry {
    name:    String,
    is_dir:  bool,
    is_exec: bool,
    is_disk: bool,
    size:    usize,
}

impl Entry {
    fn kind_str(&self) -> &'static str {
        if self.is_disk      { "Volume"      }
        else if self.is_dir  { "Dossier"     }
        else if self.is_exec { "Application" }
        else                 { "Fichier"     }
    }

    fn color(&self) -> Color {
        if self.is_disk      { C_DISK          }
        else if self.is_dir  { theme::C_YELLOW }
        else if self.is_exec { theme::C_GREEN  }
        else                 { C_FILE          }
    }

    fn icon(&self) -> char {
        if self.is_disk      { '#' }
        else if self.is_dir  { 'D' }
        else if self.is_exec { 'X' }
        else                 { 'F' }
    }

    fn size_str(&self) -> String {
        if self.is_dir || self.is_disk { "--".to_string() }
        else if self.size < 1024 { format!("{} B", self.size) }
        else if self.size < 1024 * 1024 { format!("{} KB", self.size / 1024) }
        else { format!("{} MB", self.size / (1024 * 1024)) }
    }
}

fn is_executable(name: &str) -> bool {
    (name.contains('-') && !name.contains('.')) || name.ends_with(".elf")
}

fn is_disk_volume(name: &str) -> bool {
    name == "disk" || name.starts_with("disk")
}

// ────────────────────────────────────────────────────────────────
// LECTURE D'UN RÉPERTOIRE (avec taille des fichiers)
// ────────────────────────────────────────────────────────────────
fn list_dir(dir: &DirRef) -> Vec<Entry> {
    let locked = dir.lock();
    let mut names = locked.list();
    // Dossiers d'abord, puis tri alphabétique
    names.sort_by(|a, b| {
        let a_dir = locked.get(a).map_or(false, |f| f.is_dir());
        let b_dir = locked.get(b).map_or(false, |f| f.is_dir());
        match (a_dir, b_dir) {
            (true, false) => core::cmp::Ordering::Less,
            (false, true) => core::cmp::Ordering::Greater,
            _             => a.cmp(b),
        }
    });
    names.into_iter().map(|name| {
        let fod = locked.get(&name);
        let is_dir  = fod.as_ref().map_or(false, |f| f.is_dir());
        let is_disk = is_dir && is_disk_volume(&name);
        let is_exec = !is_dir && is_executable(&name);
        let size = match &fod {
            Some(FileOrDir::File(f)) => f.lock().len(),
            _ => 0,
        };
        Entry { name, is_dir, is_exec, is_disk, size }
    }).collect()
}

// ────────────────────────────────────────────────────────────────
// FAVORIS (sidebar) — avec accès disque
// ────────────────────────────────────────────────────────────────
struct Bookmark {
    label: &'static str,
    path:  &'static str,
}

const BOOKMARKS: &[Bookmark] = &[
    Bookmark { label: "Root",     path: "/"          },
    Bookmark { label: "Disque",   path: "/disk"      },
    Bookmark { label: "Apps",     path: "/disk/apps"  },
    Bookmark { label: "Home",     path: "/disk/home"  },
    Bookmark { label: "System",   path: "/disk/system" },
    Bookmark { label: "Modules",  path: "/apps"      },
    Bookmark { label: "Libs",     path: "/libs"      },
];

// ────────────────────────────────────────────────────────────────
// BARRE D'OUTILS
// ────────────────────────────────────────────────────────────────
struct ToolButton {
    label: &'static str,
    key:   &'static str,
    width: usize,
}

const TOOL_BUTTONS: &[ToolButton] = &[
    ToolButton { label: "Supprimer",  key: "Suppr", width: 90 },
    ToolButton { label: "Rafraichir", key: "F5",    width: 90 },
    ToolButton { label: "Parent",     key: "Bksp",  width: 75 },
];

// ────────────────────────────────────────────────────────────────
// LANCEMENT D'APPLICATION
// ────────────────────────────────────────────────────────────────
fn try_launch(name: &str) {
    let ns = match task::with_current_task(|t| t.namespace.clone()) {
        Ok(ns) => ns,
        Err(_) => { error!("[files] no namespace"); return; }
    };
    if let Some(file) = ns.dir().get_file_starting_with(name) {
        let abs   = file.lock().get_absolute_path();
        let p     = path::Path::new(&abs);
        match mod_mgmt::create_application_namespace(None) {
            Ok(new_ns) => {
                match spawn::new_application_task_builder(p, Some(new_ns)) {
                    Ok(b) => { let _ = b.name(name.to_string()).spawn(); }
                    Err(e) => error!("[files] spawn: {}", e),
                }
            }
            Err(e) => error!("[files] ns: {}", e),
        }
    } else {
        info!("[files] '{}' not found in namespace", name);
    }
}

// ────────────────────────────────────────────────────────────────
// NAVIGATION VIA UN CHEMIN ABSOLU
// ────────────────────────────────────────────────────────────────
fn navigate_to(path_str: &str) -> Option<DirRef> {
    let root_dir = root::get_root().clone();
    if path_str == "/" {
        return Some(root_dir);
    }
    let mut cur = root_dir;
    for segment in path_str.split('/').filter(|s| !s.is_empty()) {
        let next = cur.lock().get(segment)
            .and_then(|fod| if let FileOrDir::Dir(d) = fod { Some(d) } else { None })?;
        cur = next;
    }
    Some(cur)
}

// ────────────────────────────────────────────────────────────────
// DOUBLE-CLIC : détecte deux clics rapides sur la même ligne
// ────────────────────────────────────────────────────────────────
struct ClickTracker {
    last_idx:   usize,
    last_ticks: usize,
    tick: usize,
}

impl ClickTracker {
    const fn new() -> Self {
        Self { last_idx: usize::MAX, last_ticks: 0, tick: 0 }
    }

    fn advance(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }

    fn click(&mut self, idx: usize) -> bool {
        let double = idx == self.last_idx
            && self.tick.wrapping_sub(self.last_ticks) < 30;
        self.last_idx   = idx;
        self.last_ticks = self.tick;
        double
    }
}

// ────────────────────────────────────────────────────────────────
// RENDU COMPLET
// ────────────────────────────────────────────────────────────────
#[allow(clippy::too_many_arguments)]
fn redraw(
    fb:       &mut Framebuffer<AlphaPixel>,
    ox: isize, oy: isize,
    cw: usize, ch: usize,
    entries:  &[Entry],
    cur_dir:  &DirRef,
    selected: usize,
    scroll:   usize,
    status:   &str,
) {
    let mut ctx = DrawContext::new(fb);
    let mw  = main_w(cw);
    let vis = visible_rows(ch);
    let fw  = theme::CHAR_W as isize;
    let fh  = theme::CHAR_H as isize;

    // ── Fond général ────────────────────────────────────────────
    ctx.fill_rect(ox, oy, cw, ch, theme::C_BG);

    // ══════════════════════════════════════════════════════════
    // SIDEBAR
    // ══════════════════════════════════════════════════════════
    let sb_x = ox;
    let sb_y = oy;
    ctx.fill_rect(sb_x, sb_y, SIDEBAR_W, ch, theme::C_PANEL);
    ctx.vline(sb_x + SIDEBAR_W as isize - 1, sb_y, sb_y + ch as isize, theme::C_BORDER);

    // Titre sidebar
    ctx.fill_rect(sb_x, sb_y, SIDEBAR_W, PATHBAR_H, theme::C_HEADER);
    ctx.text(sb_x + 8, sb_y + (PATHBAR_H as isize - fh) / 2, "NAVIGATION", theme::C_ACCENT);
    ctx.hline(sb_x, sb_x + SIDEBAR_W as isize, sb_y + PATHBAR_H as isize, theme::C_BORDER);

    // Favoris
    for (i, bm) in BOOKMARKS.iter().enumerate() {
        let ry = sb_y + PATHBAR_H as isize + (i * ROW_H) as isize;
        let text_y = ry + (ROW_H as isize - fh) / 2;
        // Icône selon le type
        let icon = if bm.path.contains("disk") && bm.path != "/disk/apps" { '#' } else { '>' };
        let icon_color = if bm.path.starts_with("/disk") { C_DISK } else { theme::C_ACCENT };
        ctx.draw_char(sb_x + 6, text_y, icon, icon_color);
        let max_chars = ((SIDEBAR_W as isize - 24).max(0) as usize) / theme::CHAR_W;
        ctx.text_clipped(sb_x + 18, text_y, bm.label, max_chars, theme::C_FG);
    }

    // Séparateur entre favoris et arborescence courante
    let sep_y = sb_y + PATHBAR_H as isize + (BOOKMARKS.len() * ROW_H) as isize + 4;
    ctx.hline(sb_x + 8, sb_x + SIDEBAR_W as isize - 8, sep_y, theme::C_BORDER);

    // Chemin décomposé (mini arbre)
    let abs = cur_dir.lock().get_absolute_path();
    let segments: Vec<&str> = abs.split('/').filter(|s| !s.is_empty()).collect();
    let mut tree_y = sep_y + 6;
    let max_chars_tree = ((SIDEBAR_W as isize - 16).max(0) as usize) / theme::CHAR_W;
    ctx.text_clipped(sb_x + 8, tree_y, "Root:/", max_chars_tree, theme::C_PURPLE);
    tree_y += ROW_H as isize;
    for (depth, seg) in segments.iter().enumerate() {
        let indent = sb_x + 8 + (depth as isize + 1) * 6;
        let avail = ((SIDEBAR_W as isize - (indent - sb_x) - 4).max(0) as usize) / theme::CHAR_W;
        let seg_icon = if *seg == "disk" || seg.starts_with("disk") { '#' } else { '+' };
        let seg_color = if *seg == "disk" || seg.starts_with("disk") { C_DISK } else { theme::C_FG_DIM };
        ctx.draw_char(indent, tree_y, seg_icon, seg_color);
        ctx.text_clipped(indent + fw, tree_y, seg, avail, theme::C_FG);
        tree_y += ROW_H as isize;
        if tree_y > sb_y + ch as isize - FOOTER_H as isize { break; }
    }

    // ══════════════════════════════════════════════════════════
    // PANNEAU PRINCIPAL
    // ══════════════════════════════════════════════════════════
    let mx = ox + SIDEBAR_W as isize;

    // ── Barre de chemin ─────────────────────────────────────────
    ctx.fill_rect(mx, oy, mw + SCROLLBAR_W, PATHBAR_H, theme::C_HEADER);
    ctx.hline(mx, mx + (mw + SCROLLBAR_W) as isize, oy + PATHBAR_H as isize, theme::C_ACCENT);

    let path_str = display_path(cur_dir);
    let text_y_path = oy + (PATHBAR_H as isize - fh) / 2;
    ctx.draw_char(mx + 6, text_y_path, '~', theme::C_ACCENT);
    let max_chars_path = ((mw as isize - 24).max(0) as usize) / theme::CHAR_W;
    ctx.text_clipped(mx + 18, text_y_path, &path_str, max_chars_path, theme::C_FG);

    // ── Barre d'outils ──────────────────────────────────────────
    let tb_y = oy + PATHBAR_H as isize;
    ctx.fill_rect(mx, tb_y, mw + SCROLLBAR_W, TOOLBAR_H, theme::C_PANEL);
    ctx.hline(mx, mx + (mw + SCROLLBAR_W) as isize, tb_y + TOOLBAR_H as isize - 1, theme::C_BORDER);

    let tb_text_y = tb_y + (TOOLBAR_H as isize - fh) / 2;
    let mut btn_x = mx + 4;
    for btn in TOOL_BUTTONS {
        ctx.fill_rect(btn_x, tb_y + 3, btn.width, TOOLBAR_H - 6, theme::C_HEADER);
        ctx.border_rect(btn_x, tb_y + 3, btn.width, TOOLBAR_H - 6, theme::C_BORDER);
        let label = format!("{} [{}]", btn.label, btn.key);
        let max_btn_chars = (btn.width.saturating_sub(8)) / theme::CHAR_W;
        ctx.text_clipped(btn_x + 4, tb_text_y, &label, max_btn_chars, theme::C_FG_DIM);
        btn_x += btn.width as isize + 4;
    }

    // ── Header colonnes ─────────────────────────────────────────
    let hdr_y  = oy + (PATHBAR_H + TOOLBAR_H) as isize;
    let col_size_x  = mx + mw as isize - 180;
    let col_type_x  = mx + mw as isize - 100;
    ctx.fill_rect(mx, hdr_y, mw + SCROLLBAR_W, HEADER_H, theme::C_HEADER);
    ctx.hline(mx, mx + mw as isize, hdr_y + HEADER_H as isize - 1, theme::C_ACCENT);

    let hdr_text_y = hdr_y + (HEADER_H as isize - fh) / 2;
    ctx.text(mx + C_NAME + fw, hdr_text_y, "Nom", theme::C_FG_DIM);
    ctx.text(col_size_x,       hdr_text_y, "Taille", theme::C_FG_DIM);
    ctx.text(col_type_x,       hdr_text_y, "Type", theme::C_FG_DIM);

    // ── Lignes ──────────────────────────────────────────────────
    let list_y0 = oy + LIST_TOP as isize;

    for (row, entry) in entries.iter().skip(scroll).take(vis).enumerate() {
        let abs_idx = scroll + row;
        let ry      = list_y0 + (row * ROW_H) as isize;
        let text_y  = ry + (ROW_H as isize - fh) / 2;

        // Fond de ligne
        let bg = if abs_idx == selected {
            C_SEL_ACT
        } else if row % 2 == 0 {
            theme::C_BG
        } else {
            C_STRIPE
        };
        ctx.fill_rect(mx, ry, mw, ROW_H, bg);

        // Indicateur de sélection
        if abs_idx == selected {
            ctx.fill_rect(mx, ry, 3, ROW_H, theme::C_ACCENT);
        }

        // Icône
        let icon_color = if abs_idx == selected { theme::C_FG } else { entry.color() };
        ctx.draw_char(mx + C_ICON, text_y, entry.icon(), icon_color);

        // Nom
        let max_name_px = col_size_x - (mx + C_NAME + fw) - 8;
        let max_name_chars = (max_name_px.max(0) as usize) / theme::CHAR_W;
        ctx.text_clipped(mx + C_NAME + fw, text_y, &entry.name, max_name_chars, entry.color());

        // Taille
        let size_s = entry.size_str();
        ctx.text_clipped(col_size_x, text_y, &size_s, 10, theme::C_FG_DIM);

        // Type
        ctx.text(col_type_x, text_y, entry.kind_str(), theme::C_FG_DIM);

        // Séparateur horizontal
        ctx.hline(mx + 3, mx + mw as isize, ry + ROW_H as isize - 1,
            Color::new(0x00202030));
    }

    // Zone vide sous les lignes
    let last_row_y = list_y0 + (vis * ROW_H) as isize;
    let footer_y   = oy + ch as isize - FOOTER_H as isize;
    if last_row_y < footer_y {
        ctx.fill_rect(mx, last_row_y, mw, (footer_y - last_row_y) as usize, theme::C_BG);
    }

    // ── Scrollbar ────────────────────────────────────────────────
    let sb_area_x = mx + mw as isize;
    let sb_area_h = vis * ROW_H;
    ctx.fill_rect(sb_area_x, list_y0, SCROLLBAR_W, sb_area_h, Color::new(0x00111118));
    ctx.vline(sb_area_x, list_y0, list_y0 + sb_area_h as isize, theme::C_BORDER);

    if entries.len() > vis && vis > 0 {
        let total     = entries.len();
        let thumb_h   = ((vis * sb_area_h) / total).max(16);
        let max_off   = total - vis;
        let thumb_y   = if max_off > 0 {
            (scroll * (sb_area_h - thumb_h)) / max_off
        } else { 0 };
        ctx.fill_rect(sb_area_x + 1, list_y0 + thumb_y as isize,
            SCROLLBAR_W - 2, thumb_h, theme::C_BORDER);
    }

    // ── Footer ───────────────────────────────────────────────────
    ctx.fill_rect(mx, footer_y, mw + SCROLLBAR_W, FOOTER_H, theme::C_HEADER);
    ctx.hline(mx, mx + (mw + SCROLLBAR_W) as isize, footer_y, theme::C_BORDER);

    let ft_text_y = footer_y + (FOOTER_H as isize - fh) / 2;
    let count_str = if entries.is_empty() {
        "Dossier vide".to_string()
    } else {
        format!("{} elements  |  sel: {}", entries.len(), selected + 1)
    };
    ctx.text(mx + 8, ft_text_y, &count_str, theme::C_FG_DIM);

    // Status / raccourcis
    let hint = if status.is_empty() {
        "Entree:ouvrir  Suppr:supprimer  F5:rafraichir  Q:quitter"
    } else {
        status
    };
    let hint_x = mx + mw as isize / 2 - (hint.len() as isize * fw) / 2;
    if hint_x > mx + 100 {
        let max_hint_chars = ((mw as isize - 110).max(0) as usize) / theme::CHAR_W;
        ctx.text_clipped(hint_x, ft_text_y, hint, max_hint_chars, theme::C_FG_DIM);
    }
}

// ────────────────────────────────────────────────────────────────
// ÉTAT DE L'APPLICATION
// ────────────────────────────────────────────────────────────────
struct FileManager {
    current_dir: DirRef,
    entries:     Vec<Entry>,
    selected:    usize,
    scroll:      usize,
    dirty:       bool,
    status:      String,
    clicker:     ClickTracker,

    off_x: isize,
    off_y: isize,
    cw:    usize,
    ch:    usize,
}

impl FileManager {
    fn new(win: &window::Window) -> Self {
        let area = win.area();
        let off_x = area.top_left.x;
        let off_y = area.top_left.y;
        let cw = (area.bottom_right.x - area.top_left.x) as usize;
        let ch = (area.bottom_right.y - area.top_left.y) as usize;
        let root_dir = root::get_root().clone();
        let entries  = list_dir(&root_dir);
        Self {
            current_dir: root_dir,
            entries,
            selected: 0,
            scroll: 0,
            dirty: true,
            status: String::new(),
            clicker: ClickTracker::new(),
            off_x, off_y, cw, ch,
        }
    }

    fn vis(&self) -> usize { visible_rows(self.ch) }

    fn clamp_scroll(&mut self) {
        let vis = self.vis();
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if vis > 0 && self.selected >= self.scroll + vis {
            self.scroll = self.selected - vis + 1;
        }
        let max_scroll = self.entries.len().saturating_sub(vis);
        if self.scroll > max_scroll { self.scroll = max_scroll; }
    }

    fn navigate_into(&mut self, dir: DirRef) {
        self.current_dir = dir;
        self.entries = list_dir(&self.current_dir);
        self.selected = 0;
        self.scroll   = 0;
        self.dirty    = true;
    }

    fn navigate_parent(&mut self) {
        let parent = self.current_dir.lock().get_parent_dir();
        if let Some(p) = parent {
            self.navigate_into(p);
        }
    }

    fn refresh(&mut self) {
        self.entries = list_dir(&self.current_dir);
        if self.selected >= self.entries.len() {
            self.selected = self.entries.len().saturating_sub(1);
        }
        self.clamp_scroll();
        self.status = "Rafraichi".to_string();
        self.dirty  = true;
    }

    fn open_selected(&mut self) {
        if let Some(entry) = self.entries.get(self.selected).cloned() {
            if entry.is_dir || entry.is_disk {
                let sub = self.current_dir.lock()
                    .get(&entry.name)
                    .and_then(|fod| if let FileOrDir::Dir(d) = fod { Some(d) } else { None });
                if let Some(d) = sub {
                    self.navigate_into(d);
                }
            } else if entry.is_exec {
                self.status = format!("Lancement: {}", entry.name);
                self.dirty  = true;
                try_launch(&entry.name);
            } else {
                self.status = format!("{} — {} octets", entry.name, entry.size);
                self.dirty  = true;
            }
        }
    }

    fn delete_selected(&mut self) {
        if let Some(entry) = self.entries.get(self.selected).cloned() {
            // Empêcher la suppression de volumes disque et répertoires système
            if entry.is_disk {
                self.status = "Impossible de supprimer un volume".to_string();
                self.dirty = true;
                return;
            }

            let mut locked = self.current_dir.lock();
            if let Some(fod) = locked.get(&entry.name) {
                locked.remove(&fod);
                drop(locked);
                self.status = format!("Supprime: {}", entry.name);
                self.refresh();
            } else {
                self.status = format!("Non trouve: {}", entry.name);
                self.dirty = true;
            }
        }
    }

    // ── Gestion clavier ──────────────────────────────────────────
    fn on_key(&mut self, kc: Keycode) -> bool {
        let vis = self.vis();
        let len = self.entries.len();
        match kc {
            Keycode::Q => return true,

            Keycode::Up => {
                if self.selected > 0 { self.selected -= 1; }
                self.clamp_scroll();
                self.dirty = true;
            }
            Keycode::Down => {
                if self.selected + 1 < len { self.selected += 1; }
                self.clamp_scroll();
                self.dirty = true;
            }
            Keycode::Home => {
                self.selected = 0;
                self.clamp_scroll();
                self.dirty = true;
            }
            Keycode::End => {
                if len > 0 { self.selected = len - 1; }
                self.clamp_scroll();
                self.dirty = true;
            }
            Keycode::PageUp => {
                self.selected = self.selected.saturating_sub(vis);
                self.clamp_scroll();
                self.dirty = true;
            }
            Keycode::PageDown => {
                self.selected = (self.selected + vis).min(len.saturating_sub(1));
                self.clamp_scroll();
                self.dirty = true;
            }
            Keycode::Enter => {
                self.open_selected();
            }
            Keycode::Backspace => {
                self.navigate_parent();
            }
            // Supprimer
            Keycode::Delete | Keycode::X => {
                self.delete_selected();
            }
            // Rafraîchir
            Keycode::F5 | Keycode::R => {
                self.refresh();
            }
            _ => {}
        }
        false
    }

    // ── Gestion souris ───────────────────────────────────────────
    fn on_mouse_click(&mut self, coord: Coord, was_left: bool, is_left: bool) -> bool {
        if !was_left && !is_left { return false; }
        if !is_left { return false; }

        let x = coord.x;
        let y = coord.y;

        // ── Clic sidebar (x < SIDEBAR_W) ─────────────────────────
        if x < SIDEBAR_W as isize {
            let bm_y_start = PATHBAR_H as isize;
            if y >= bm_y_start {
                let idx = ((y - bm_y_start) / ROW_H as isize) as usize;
                if let Some(bm) = BOOKMARKS.get(idx) {
                    if let Some(dir) = navigate_to(bm.path) {
                        self.navigate_into(dir);
                    } else {
                        self.status = format!("{} non disponible", bm.label);
                        self.dirty = true;
                    }
                }
            }
            return false;
        }

        // ── Clic barre d'outils ──────────────────────────────────
        let tb_y_start = PATHBAR_H as isize;
        let tb_y_end   = tb_y_start + TOOLBAR_H as isize;
        if y >= tb_y_start && y < tb_y_end {
            let rx = x - SIDEBAR_W as isize;
            // Bouton Supprimer [0..90], Rafraîchir [94..184], Parent [188..263]
            if rx >= 4 && rx < 94       { self.delete_selected(); }
            else if rx >= 98 && rx < 188 { self.refresh(); }
            else if rx >= 192 && rx < 267 { self.navigate_parent(); }
            return false;
        }

        // ── Clic dans la liste ───────────────────────────────────
        if y >= LIST_TOP as isize {
            let row_rel = y - LIST_TOP as isize;
            let vis     = self.vis();
            let list_px_h = vis * ROW_H;
            if row_rel >= 0 && (row_rel as usize) < list_px_h {
                let row = (row_rel / ROW_H as isize) as usize;
                let idx = self.scroll + row;
                if idx < self.entries.len() {
                    let double = self.clicker.click(idx);
                    self.selected = idx;
                    self.dirty    = true;
                    if double {
                        self.open_selected();
                    }
                }
            }
        }

        false
    }
}

// ────────────────────────────────────────────────────────────────
// POINT D'ENTRÉE
// ────────────────────────────────────────────────────────────────
pub fn main(_args: alloc::vec::Vec<String>) -> isize {
    info!("[file_manager] starting");

    let mut win = match window::Window::with_title(
        "Gestionnaire de fichiers".to_string(),
        Coord::new(60, 40),
        WIN_W, WIN_H,
        theme::C_BG,
    ) {
        Ok(w)  => w,
        Err(e) => { error!("[file_manager] window: {}", e); return -1; }
    };

    let mut fm = FileManager::new(&win);
    let mut last_left = false;

    loop {
        fm.clicker.advance();

        // ── Drainer tous les événements disponibles ──────────────
        loop {
            match win.handle_event() {
                Ok(Some(Event::ExitEvent)) => return 0,

                Ok(Some(Event::KeyboardEvent(ke))) => {
                    if ke.key_event.action != KeyAction::Pressed { continue; }
                    if fm.on_key(ke.key_event.keycode) { return 0; }
                }

                Ok(Some(Event::MousePositionEvent(me))) => {
                    let quit = fm.on_mouse_click(
                        me.coordinate,
                        last_left,
                        me.left_button_hold,
                    );
                    last_left = me.left_button_hold;
                    if quit { return 0; }
                }

                Ok(Some(Event::WindowResizeEvent(_area))) => {
                    let a = win.area();
                    fm.off_x = a.top_left.x;
                    fm.off_y = a.top_left.y;
                    fm.cw    = (a.bottom_right.x - a.top_left.x) as usize;
                    fm.ch    = (a.bottom_right.y - a.top_left.y) as usize;
                    fm.clamp_scroll();
                    fm.dirty = true;
                }

                Ok(None) => break,
                Ok(Some(_)) => {}
                Err(_) => break,
            }
        }

        // ── Redraw si nécessaire ────────────────────────────────
        if fm.dirty {
            {
                let mut fb = win.framebuffer_mut();
                redraw(
                    &mut fb,
                    fm.off_x, fm.off_y,
                    fm.cw,    fm.ch,
                    &fm.entries,
                    &fm.current_dir,
                    fm.selected,
                    fm.scroll,
                    &fm.status,
                );
            }
            if let Err(e) = win.render(None) {
                error!("[file_manager] render: {}", e);
            }
            fm.dirty  = false;
            fm.status.clear();
        }

        scheduler::schedule();
    }

    #[allow(unreachable_code)]
    0
}
