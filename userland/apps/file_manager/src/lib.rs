//! Fichiers — gestionnaire de fichiers de MaiOS.
//!
//! - Barre d'outils : précédent, suivant, dossier parent, actualiser ; fil d'Ariane cliquable
//!   (Ctrl+L pour saisir un chemin) ; recherche (filtre instantané du dossier, Entrée pour une
//!   recherche récursive approchée notée comme fzf) ; vue liste ou grille ; aperçu.
//! - Panneau latéral : emplacements, dossiers récents, corbeille (déposer dessus = supprimer).
//! - Sélection multiple (Ctrl, Maj, Ctrl+A, lasso à la souris), glisser-déposer pour déplacer
//!   (Ctrl au dépôt : copier), menus contextuels, recherche au clavier par les premières lettres.
//! - Copier/couper/coller (presse-papiers partagé du système), copie récursive incrémentale
//!   avec progression et annulation, déplacement et renommage sans copie, corbeille restaurable,
//!   annulation (Ctrl+Z) des renommages, déplacements, suppressions et créations.
//! - Aperçu : texte, résumé ELF, WAD, dossier ; visionneuse plein panneau (texte ou hexadécimal)
//!   avec recherche.
//! - Ouverture selon le type réel (octets magiques) : applications MaiOS, binaires Linux par
//!   `run`, WAD de Doom, texte dans la visionneuse.

#![no_std]
extern crate alloc;
#[macro_use]
extern crate log;

mod fsops;
mod preview;
mod search;

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cmp::Ordering;

use color::Color;
use event_types::Event;
use fs_node::{DirRef, FileOrDir};
use fsops::{basename, join, parent_of, CopyJob, FileKind, Item, Undo, TRASH};
use keycodes_ascii::{KeyAction, KeyEvent, KeyboardModifiers, Keycode};
use mai_ui::canvas::{text_width, Canvas, Rect, CHAR_H, CHAR_W};
use mai_ui::clipboard::{self, Clip};
use mai_ui::icons::{self, Icon};
use mai_ui::input::{ctrl_letter, key_char, Pointer};
use mai_ui::kit::{
    self, human_bytes, pal, Align, ButtonKind, Column, Dialog, DialogResult, FieldEvent, Menu, MenuItem, Scroll,
    TextField, Toast, ToastKind,
};
use preview::Preview;
use search::Search;
use shapes::Coord;

// ================================================================
// MISE EN PAGE
// ================================================================

const WIN_W: usize = 1060;
const WIN_H: usize = 680;
const TOOLBAR_H: isize = 50;
const STATUS_H: isize = 28;
const SIDEBAR_W: isize = 212;
const PREVIEW_W: isize = 300;
const ROW_H: isize = 26;
const COLHEAD_H: isize = 30;
const SCROLL_W: isize = 10;
const TILE_W: isize = 112;
const TILE_H: isize = 100;
const SIDE_ROW: isize = 30;

const LIST_COLS: [Column; 3] = [
    Column::flex("Nom", 240, 1),
    Column::fixed("Taille", 116, Align::Right),
    Column::fixed("Type", 190, Align::Left),
];
const SEARCH_COLS: [Column; 3] = [
    Column::flex("Nom", 200, 3),
    Column::flex("Dossier", 160, 2),
    Column::fixed("Taille", 110, Align::Right),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    List,
    Grid,
}

// Menus.
const M_OPEN: u32 = 1;
const M_VIEW: u32 = 2;
const M_CUT: u32 = 3;
const M_COPY: u32 = 4;
const M_PASTE: u32 = 5;
const M_RENAME: u32 = 6;
const M_TRASH: u32 = 7;
const M_DELETE: u32 = 8;
const M_COPY_PATH: u32 = 9;
const M_PROPS: u32 = 10;
const M_NEW_DIR: u32 = 11;
const M_NEW_FILE: u32 = 12;
const M_REFRESH: u32 = 13;
const M_HIDDEN: u32 = 14;
const M_MODE: u32 = 15;
const M_RESTORE: u32 = 16;
const M_EMPTY_TRASH: u32 = 17;
const M_SELECT_ALL: u32 = 18;
const M_OPEN_LOCATION: u32 = 19;
const M_DUPLICATE: u32 = 20;

/// Objet d'une boîte de dialogue.
#[derive(Clone)]
enum Pending {
    None,
    NewDir,
    NewFile,
    Rename(String),
    DeleteForever(Vec<String>),
    EmptyTrash,
}

/// Cible d'un dépôt.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Drop {
    Folder(String),
    Trash,
}

/// Glisser en cours.
struct Drag {
    paths: Vec<String>,
    over: Option<Drop>,
}

/// Visionneuse plein panneau.
struct Viewer {
    title: String,
    subtitle: String,
    lines: Vec<String>,
    scroll: usize,
    hscroll: usize,
    find: TextField,
    find_focus: bool,
    matches: Vec<(usize, usize)>,
    current: usize,
    hex: bool,
}

#[derive(Default)]
struct Hits {
    back: Rect,
    fwd: Rect,
    up: Rect,
    refresh: Rect,
    crumbs: Vec<(Rect, String)>,
    path_area: Rect,
    search: Rect,
    mode_btn: Rect,
    preview_btn: Rect,
    side: Vec<(Rect, Side)>,
    main: Rect,
    list: Rect,
    track: Rect,
    columns: Vec<Rect>,
    job_cancel: Rect,
}

/// Entrée du panneau latéral.
#[derive(Clone, PartialEq, Eq)]
enum Side {
    Place(String),
    Trash,
}

struct Fm {
    area: Rect,
    pointer: Pointer,
    mods: KeyboardModifiers,
    hits: Hits,
    dirty: bool,

    cwd: String,
    dir: Option<DirRef>,
    fingerprint: u64,
    last_poll: u64,
    items: Vec<Item>,
    /// Indices (dans `items`) affichés : filtre, fichiers cachés, tri.
    view: Vec<usize>,
    sel: BTreeSet<String>,
    cursor: Option<usize>,
    anchor: Option<usize>,
    scroll: Scroll,
    sort: (usize, bool),
    mode: Mode,
    show_hidden: bool,
    show_preview: bool,
    back: Vec<String>,
    fwd: Vec<String>,
    recent: Vec<String>,
    places: Vec<(String, String, Icon)>,

    filter: TextField,
    filter_focus: bool,
    search: Option<Search>,
    path_edit: Option<TextField>,
    typeahead: String,
    typeahead_at: u64,

    job: Option<CopyJob>,
    undo: Vec<Undo>,
    menu: Option<Menu>,
    dialog: Option<Dialog>,
    pending: Pending,
    toast: Option<Toast>,
    viewer: Option<Viewer>,
    preview: Option<(String, u64, Preview)>,
    drag: Option<Drag>,
    band: Option<(isize, isize)>,
    hover_col: Option<usize>,
    hover_index: Option<usize>,
    trash_names: Vec<(String, String)>,
}

fn now_ns() -> u64 { task::scheduler::clock_ns() }

fn content_rect(win: &window::Window) -> Rect {
    let a = win.area();
    Rect::new(a.top_left.x, a.top_left.y, a.bottom_right.x - a.top_left.x, a.bottom_right.y - a.top_left.y)
}

/// Lance l'application dont la crate commence par `prefix`, avec `args`.
fn spawn_app_prefix(prefix: &str, args: Vec<String>) -> Result<(), String> {
    let ns = task::with_current_task(|t| t.namespace.clone()).map_err(|_| "aucun namespace")?;
    let file = ns.dir().get_file_starting_with(prefix).ok_or_else(|| format!("application « {} » introuvable", prefix.trim_end_matches('-')))?;
    let abs = file.lock().get_absolute_path();
    spawn_app_file(&abs, args, prefix.trim_end_matches('-'))
}

/// Lance la crate d'application `abs` (fichier objet d'un namespace).
fn spawn_app_file(abs: &str, args: Vec<String>, name: &str) -> Result<(), String> {
    let p = path::Path::new(abs);
    let ns = mod_mgmt::create_application_namespace(None).map_err(String::from)?;
    spawn::new_application_task_builder(p, Some(ns))
        .map_err(String::from)?
        .argument(args)
        .name(name.to_string())
        .spawn()
        .map(|_| ())
        .map_err(String::from)
}

impl Fm {
    fn new(area: Rect) -> Fm {
        let mut fm = Fm {
            area,
            pointer: Pointer::new(),
            mods: KeyboardModifiers::empty(),
            hits: Hits::default(),
            dirty: true,
            cwd: String::from("/"),
            dir: None,
            fingerprint: 0,
            last_poll: 0,
            items: Vec::new(),
            view: Vec::new(),
            sel: BTreeSet::new(),
            cursor: None,
            anchor: None,
            scroll: Scroll::default(),
            sort: (0, true),
            mode: Mode::List,
            show_hidden: false,
            show_preview: true,
            back: Vec::new(),
            fwd: Vec::new(),
            recent: Vec::new(),
            places: Vec::new(),
            filter: TextField::new(),
            filter_focus: false,
            search: None,
            path_edit: None,
            typeahead: String::new(),
            typeahead_at: 0,
            job: None,
            undo: Vec::new(),
            menu: None,
            dialog: None,
            pending: Pending::None,
            toast: None,
            viewer: None,
            preview: None,
            drag: None,
            band: None,
            hover_col: None,
            hover_index: None,
            trash_names: Vec::new(),
        };
        fm.refresh_places();
        let start = if fsops::lookup_dir("/disk").is_some() { "/disk" } else { "/" };
        fm.load(start);
        fm
    }

    fn toast(&mut self, text: String, kind: ToastKind) {
        self.toast = Some(Toast::new(text, kind, now_ns()));
        self.dirty = true;
    }

    fn refresh_places(&mut self) {
        const CANDIDATES: [(&str, &str, Icon); 8] = [
            ("MaiOS (racine)", "/", Icon::Disk),
            ("Disque", "/disk", Icon::Disk),
            ("Applications", "/disk/apps", Icon::App),
            ("Bureau", "/desktop", Icon::Desktop),
            ("Binaires Linux", "/extra_files", Icon::Binary),
            ("Temporaire", "/disk/tmp", Icon::Folder),
            ("Crates d'applications", "/namespaces/_applications", Icon::Library),
            ("Modules du noyau", "/namespaces/_kernel", Icon::Library),
        ];
        self.places = CANDIDATES
            .iter()
            .filter(|(_, p, _)| fsops::lookup_dir(p).is_some())
            .map(|(l, p, i)| (l.to_string(), p.to_string(), *i))
            .collect();
    }

    // ── Navigation ───────────────────────────────────────────────

    /// Charge le dossier `path` (sans toucher à l'historique).
    fn load(&mut self, path: &str) -> bool {
        let Some(d) = fsops::lookup_dir(path) else {
            self.toast(format!("Dossier introuvable : {}", path), ToastKind::Error);
            return false;
        };
        self.cwd = String::from(path);
        self.fingerprint = fsops::fingerprint(&d);
        self.items = fsops::list(&d, path);
        if path == TRASH {
            self.items.retain(|i| i.name != ".index");
            self.trash_names = fsops::trash_index();
        }
        self.dir = Some(d);
        self.sel.clear();
        self.cursor = None;
        self.anchor = None;
        self.scroll = Scroll::default();
        self.search = None;
        self.filter.set_text("");
        self.preview = None;
        self.rebuild_view();
        self.recent.retain(|p| p != path);
        self.recent.insert(0, String::from(path));
        self.recent.truncate(6);
        self.dirty = true;
        true
    }

    fn navigate(&mut self, path: &str) {
        let prev = self.cwd.clone();
        if path == prev {
            self.reload();
            return;
        }
        if self.load(path) {
            self.back.push(prev);
            self.fwd.clear();
        }
    }

    fn go_back(&mut self) {
        if let Some(p) = self.back.pop() {
            let cur = self.cwd.clone();
            if self.load(&p) {
                self.fwd.push(cur);
            }
        }
    }

    fn go_forward(&mut self) {
        if let Some(p) = self.fwd.pop() {
            let cur = self.cwd.clone();
            if self.load(&p) {
                self.back.push(cur);
            }
        }
    }

    fn go_up(&mut self) {
        if self.cwd != "/" {
            let child = basename(&self.cwd).to_string();
            let up = parent_of(&self.cwd);
            self.navigate(&up);
            self.select_name(&child);
        }
    }

    /// Relit le dossier en gardant la sélection et la position.
    fn reload(&mut self) {
        let Some(d) = fsops::lookup_dir(&self.cwd) else {
            let up = parent_of(&self.cwd);
            self.load(&up);
            return;
        };
        let cursor_name = self.cursor_item().map(|i| i.name.clone());
        self.fingerprint = fsops::fingerprint(&d);
        self.items = fsops::list(&d, &self.cwd);
        if self.cwd == TRASH {
            self.items.retain(|i| i.name != ".index");
            self.trash_names = fsops::trash_index();
        }
        self.dir = Some(d);
        let names: BTreeSet<String> = self.items.iter().map(|i| i.name.clone()).collect();
        self.sel.retain(|n| names.contains(n));
        self.rebuild_view();
        self.cursor = cursor_name.and_then(|n| self.view.iter().position(|&i| self.items[i].name == n));
        self.preview = None;
        self.dirty = true;
    }

    fn select_name(&mut self, name: &str) {
        let pos = self.view.iter().position(|&i| self.items[i].name == name).or_else(|| {
            self.view.iter().position(|&i| self.items[i].name.eq_ignore_ascii_case(name))
        });
        if let Some(pos) = pos {
            let name = self.items[self.view[pos]].name.clone();
            self.sel.clear();
            self.sel.insert(name);
            self.cursor = Some(pos);
            self.anchor = Some(pos);
            self.follow_cursor();
            self.dirty = true;
        }
    }

    // ── Vue ──────────────────────────────────────────────────────

    fn rebuild_view(&mut self) {
        let q: Vec<char> = self.filter.text().chars().map(fsops::fold).collect();
        let qs: String = q.iter().collect();
        let (col, asc) = self.sort;
        let mut v: Vec<usize> = (0..self.items.len())
            .filter(|&i| self.show_hidden || !self.items[i].hidden() || self.cwd == TRASH)
            .filter(|&i| qs.is_empty() || self.items[i].name.chars().map(fsops::fold).collect::<String>().contains(qs.as_str()))
            .collect();
        let items = &self.items;
        v.sort_by(|&a, &b| {
            let (x, y) = (&items[a], &items[b]);
            // Dossiers d'abord, quel que soit le sens.
            let dirs = y.is_dir.cmp(&x.is_dir);
            if dirs != Ordering::Equal {
                return dirs;
            }
            let o = match col {
                1 => if x.is_dir { x.children.cmp(&y.children) } else { x.size.cmp(&y.size) },
                2 => x.kind.label().cmp(y.kind.label()),
                _ => Ordering::Equal,
            };
            let o = o.then_with(|| fsops::natural_cmp(&x.name, &y.name));
            if asc { o } else { o.reverse() }
        });
        self.view = v;
        if let Some(c) = self.cursor {
            if c >= self.view.len() {
                self.cursor = self.view.len().checked_sub(1);
            }
        }
        self.dirty = true;
    }

    fn cursor_item(&self) -> Option<&Item> { self.cursor.and_then(|c| self.view.get(c)).map(|&i| &self.items[i]) }

    fn path_of(&self, name: &str) -> String { join(&self.cwd, name) }

    /// Chemins sélectionnés (ou l'élément sous le curseur).
    fn selection_paths(&self) -> Vec<String> {
        if let Some(s) = &self.search {
            return self.cursor.and_then(|c| s.hits.get(c)).map(|h| alloc::vec![h.path()]).unwrap_or_default();
        }
        if !self.sel.is_empty() {
            return self.view.iter().map(|&i| &self.items[i]).filter(|it| self.sel.contains(&it.name)).map(|it| self.path_of(&it.name)).collect();
        }
        self.cursor_item().map(|i| alloc::vec![self.path_of(&i.name)]).unwrap_or_default()
    }

    fn selection_size(&self) -> (usize, u64) {
        let mut n = 0;
        let mut b = 0;
        for &i in &self.view {
            let it = &self.items[i];
            if self.sel.contains(&it.name) {
                n += 1;
                b += it.size;
            }
        }
        (n, b)
    }

    fn per_row(&self) -> usize {
        match self.mode {
            Mode::List => 1,
            Mode::Grid => (self.hits.list.w / TILE_W).max(1) as usize,
        }
    }

    fn line_h(&self) -> isize {
        match self.mode {
            Mode::List => ROW_H,
            Mode::Grid => TILE_H,
        }
    }

    fn visible_lines(&self) -> usize { (self.hits.list.h.max(0) / if self.search.is_some() { ROW_H } else { self.line_h() }) as usize }

    fn total_lines(&self) -> usize {
        if let Some(s) = &self.search {
            return s.hits.len();
        }
        let pr = self.per_row();
        self.view.len().div_ceil(pr)
    }

    fn follow_cursor(&mut self) {
        if let Some(c) = self.cursor {
            let line = if self.search.is_some() { c } else { c / self.per_row() };
            let (vis, total) = (self.visible_lines(), self.total_lines());
            self.scroll.follow(line, vis.max(1), total);
        }
    }

    /// Rectangle de l'élément `i` de la vue.
    fn item_rect(&self, i: usize) -> Rect {
        let l = self.hits.list;
        if self.search.is_some() || self.mode == Mode::List {
            let row = i as isize - self.scroll.offset as isize;
            return Rect::new(l.x, l.y + row * ROW_H, l.w, ROW_H);
        }
        let pr = self.per_row();
        let line = (i / pr) as isize - self.scroll.offset as isize;
        let col = (i % pr) as isize;
        let gap = (l.w - pr as isize * TILE_W) / (pr as isize + 1);
        Rect::new(l.x + gap + col * (TILE_W + gap), l.y + 6 + line * TILE_H, TILE_W, TILE_H - 8)
    }

    /// Élément sous le pointeur.
    fn index_at(&self, px: isize, py: isize) -> Option<usize> {
        let l = self.hits.list;
        if !l.contains(px, py) || self.hits.track.contains(px, py) {
            return None;
        }
        let n = if let Some(s) = &self.search { s.hits.len() } else { self.view.len() };
        if self.search.is_some() || self.mode == Mode::List {
            let i = self.scroll.offset + ((py - l.y) / ROW_H) as usize;
            return (i < n).then_some(i);
        }
        let first = self.scroll.offset * self.per_row();
        (first..n).take((self.visible_lines() + 1) * self.per_row()).find(|&i| self.item_rect(i).contains(px, py))
    }

    // ── Opérations ───────────────────────────────────────────────

    fn open_cursor(&mut self) {
        if let Some(s) = &self.search {
            let Some(h) = self.cursor.and_then(|c| s.hits.get(c)).cloned() else { return };
            if h.is_dir {
                self.navigate(&h.path());
            } else {
                self.navigate(&h.dir);
                self.select_name(&h.name);
                let k = self.cursor_item().map(|i| i.kind).unwrap_or(FileKind::Binary);
                self.open_path(&h.path(), k);
            }
            return;
        }
        let Some(it) = self.cursor_item().cloned() else { return };
        let p = self.path_of(&it.name);
        self.open_path(&p, it.kind);
    }

    fn open_path(&mut self, path: &str, kind: FileKind) {
        let res = match kind {
            FileKind::Folder | FileKind::Volume => {
                self.navigate(path);
                return;
            }
            FileKind::MaiApp => {
                let name = basename(path).split('-').next().unwrap_or("app").to_string();
                spawn_app_file(path, Vec::new(), &name).map(|_| format!("Application « {} » lancée", name))
            }
            FileKind::ElfExec | FileKind::ElfDyn => {
                spawn_app_prefix("run-", alloc::vec![path.to_string()]).map(|_| format!("Lancement de {}", basename(path)))
            }
            FileKind::Wad => spawn_app_prefix("run-", alloc::vec!["doom".to_string(), "-iwad".to_string(), path.to_string()])
                .map(|_| "Lancement de Doom".to_string()),
            _ => {
                self.open_viewer(path, kind);
                return;
            }
        };
        match res {
            Ok(m) => self.toast(m, ToastKind::Success),
            Err(e) => self.toast(format!("Ouverture impossible : {}", e), ToastKind::Error),
        }
    }

    fn open_viewer(&mut self, path: &str, kind: FileKind) {
        let Some(FileOrDir::File(f)) = fsops::lookup(path) else { return };
        let len = f.lock().len();
        let hex = !kind.is_textual();
        let data = fsops::read_all(&f, if hex { 1 << 20 } else { 8 << 20 });
        let lines: Vec<String> = if hex {
            data.chunks(16)
                .enumerate()
                .map(|(i, c)| {
                    let mut s = format!("{:08x}  ", i * 16);
                    for (j, x) in c.iter().enumerate() {
                        s.push_str(&format!("{:02x} ", x));
                        if j == 7 {
                            s.push(' ');
                        }
                    }
                    for j in c.len()..16 {
                        s.push_str("   ");
                        if j == 7 {
                            s.push(' ');
                        }
                    }
                    s.push(' ');
                    for &x in c {
                        s.push(if (0x20..0x7f).contains(&x) { x as char } else { '.' });
                    }
                    s
                })
                .collect()
        } else {
            String::from_utf8_lossy(&data).lines().map(|l| l.replace('\t', "    ")).collect()
        };
        let cut = if data.len() < len { format!(" · {} premiers octets", human_bytes(data.len() as u64)) } else { String::new() };
        self.viewer = Some(Viewer {
            title: basename(path).to_string(),
            subtitle: format!("{} · {} · {} lignes{}", kind.label(), human_bytes(len as u64), lines.len(), cut),
            lines,
            scroll: 0,
            hscroll: 0,
            find: TextField::new(),
            find_focus: false,
            matches: Vec::new(),
            current: 0,
            hex,
        });
        self.dirty = true;
    }

    fn copy_selection(&mut self, cut: bool) {
        let paths = self.selection_paths();
        if paths.is_empty() {
            return;
        }
        let n = paths.len();
        clipboard::set_files(paths, cut);
        let verb = if cut { "coupé(s)" } else { "copié(s)" };
        self.toast(format!("{} élément(s) {}", n, verb), ToastKind::Info);
    }

    fn paste(&mut self) {
        let Clip::Files { paths, cut } = clipboard::get() else {
            self.toast("Le presse-papiers ne contient pas de fichiers".into(), ToastKind::Info);
            return;
        };
        let dst = self.cwd.clone();
        if cut {
            self.move_paths(&paths, &dst);
            clipboard::clear();
        } else {
            self.start_copy(&paths, &dst);
        }
    }

    fn start_copy(&mut self, paths: &[String], dst: &str) {
        if self.job.is_some() {
            self.toast("Une copie est déjà en cours".into(), ToastKind::Error);
            return;
        }
        match CopyJob::new(paths, dst) {
            Ok(j) => {
                self.job = Some(j);
                self.dirty = true;
            }
            Err(e) => self.toast(e, ToastKind::Error),
        }
    }

    fn move_paths(&mut self, paths: &[String], dst: &str) {
        let mut done = Vec::new();
        let mut errors = Vec::new();
        let mut by_copy = Vec::new();
        for p in paths {
            if parent_of(p) == dst {
                continue;
            }
            match fsops::move_to(p, dst) {
                Ok(newp) => done.push((p.clone(), newp)),
                Err(e) if e == fsops::NEEDS_COPY => by_copy.push(p.clone()),
                Err(e) => errors.push(e),
            }
        }
        if !by_copy.is_empty() {
            if self.job.is_some() {
                errors.push("une copie est déjà en cours".to_string());
            } else {
                match CopyJob::new(&by_copy, dst) {
                    Ok(j) => self.job = Some(j.into_move(&by_copy)),
                    Err(e) => errors.push(e),
                }
            }
        }
        if !done.is_empty() {
            let n = done.len();
            self.undo.push(Undo::Move(done));
            self.toast(format!("{} élément(s) déplacé(s)", n), ToastKind::Success);
        }
        if let Some(e) = errors.first() {
            self.toast(format!("Échec du déplacement : {}", e), ToastKind::Error);
        }
        self.reload();
    }

    fn trash_selection(&mut self) {
        let paths = self.selection_paths();
        if paths.is_empty() {
            return;
        }
        if self.cwd == TRASH || paths.iter().any(|p| p.starts_with(TRASH)) {
            self.ask_delete_forever(paths);
            return;
        }
        let mut names = Vec::new();
        let mut err = None;
        let mut no_trash = Vec::new();
        for p in &paths {
            match fsops::trash(p) {
                Ok(n) => names.push(n),
                Err(e) if e == fsops::NO_TRASH => no_trash.push(p.clone()),
                Err(e) => err = Some(e),
            }
        }
        if !no_trash.is_empty() {
            let l1 = format!("{} : supprimer définitivement {} élément(s) ?", fsops::NO_TRASH, no_trash.len());
            self.dialog = Some(Dialog::confirm(1, "Suppression définitive", &[l1.as_str(), "Cette action ne peut pas être annulée."], "Supprimer", true));
            self.pending = Pending::DeleteForever(no_trash);
        }
        if !names.is_empty() {
            let n = names.len();
            self.undo.push(Undo::Trash(names));
            self.toast(format!("{} élément(s) mis à la corbeille (Ctrl+Z pour annuler)", n), ToastKind::Success);
        }
        if let Some(e) = err {
            self.toast(e, ToastKind::Error);
        }
        self.reload();
    }

    fn ask_delete_forever(&mut self, paths: Vec<String>) {
        if paths.is_empty() {
            return;
        }
        let l1 = if paths.len() == 1 { format!("Supprimer définitivement « {} » ?", basename(&paths[0])) } else { format!("Supprimer définitivement {} éléments ?", paths.len()) };
        self.dialog = Some(Dialog::confirm(1, "Suppression définitive", &[l1.as_str(), "Cette action ne peut pas être annulée."], "Supprimer", true));
        self.pending = Pending::DeleteForever(paths);
        self.dirty = true;
    }

    fn restore_selection(&mut self) {
        let names: Vec<String> = self.selection_paths().iter().map(|p| basename(p).to_string()).collect();
        let mut ok = 0;
        for n in &names {
            match fsops::restore(n) {
                Ok(_) => ok += 1,
                Err(e) => self.toast(e, ToastKind::Error),
            }
        }
        if ok > 0 {
            self.toast(format!("{} élément(s) restauré(s)", ok), ToastKind::Success);
        }
        self.reload();
    }

    fn do_undo(&mut self) {
        let Some(u) = self.undo.pop() else {
            self.toast("Rien à annuler".into(), ToastKind::Info);
            return;
        };
        match u.revert() {
            Ok(()) => self.toast(format!("Annulé : {}", u.describe()), ToastKind::Success),
            Err(e) => self.toast(format!("Annulation impossible : {}", e), ToastKind::Error),
        }
        self.reload();
    }

    fn ask_rename(&mut self) {
        let Some(it) = self.cursor_item().cloned() else { return };
        self.dialog = Some(Dialog::prompt(2, "Renommer", &format!("Nouveau nom de « {} » :", it.name), &it.name, "Renommer"));
        self.pending = Pending::Rename(it.name);
        self.dirty = true;
    }

    fn ask_new(&mut self, dir: bool) {
        let name = if dir { "Nouveau dossier" } else { "nouveau.txt" };
        let d = self.dir.clone();
        let initial = d.map(|d| fsops::unique_name(&d, name, "2")).unwrap_or_else(|| name.to_string());
        let (t, m, b) = if dir { ("Nouveau dossier", "Nom du dossier :", "Créer") } else { ("Nouveau fichier", "Nom du fichier :", "Créer") };
        self.dialog = Some(Dialog::prompt(3, t, m, &initial, b));
        self.pending = if dir { Pending::NewDir } else { Pending::NewFile };
        self.dirty = true;
    }

    fn duplicate(&mut self) {
        let paths = self.selection_paths();
        let dst = self.cwd.clone();
        self.start_copy(&paths, &dst);
    }

    fn dialog_done(&mut self, res: DialogResult, value: String) {
        let pending = core::mem::replace(&mut self.pending, Pending::None);
        let ok = matches!(res, DialogResult::Button(1));
        if !ok {
            return;
        }
        let Some(dir) = self.dir.clone() else { return };
        match pending {
            Pending::NewDir | Pending::NewFile => {
                let r = if matches!(pending, Pending::NewDir) { fsops::create_dir(&dir, &value).map(|_| ()) } else { fsops::create_file(&dir, &value, b"").map(|_| ()) };
                match r {
                    Ok(()) => {
                        self.undo.push(Undo::Create(alloc::vec![self.path_of(&value)]));
                        self.reload();
                        self.select_name(&value);
                    }
                    Err(e) => {
                        self.pending = pending;
                        self.reopen_with_error(e, &value);
                    }
                }
            }
            Pending::Rename(old) => match fsops::rename(&dir, &old, &value) {
                Ok(()) => {
                    if old != value {
                        self.undo.push(Undo::Rename { dir: self.cwd.clone(), old, new: value.clone() });
                    }
                    self.reload();
                    self.select_name(&value);
                }
                Err(e) => {
                    self.pending = Pending::Rename(old);
                    self.reopen_with_error(e, &value);
                }
            },
            Pending::DeleteForever(paths) => {
                let mut n = 0;
                for p in &paths {
                    match fsops::remove(p) {
                        Ok(()) => n += 1,
                        Err(e) => self.toast(e, ToastKind::Error),
                    }
                }
                if self.cwd == TRASH {
                    let mut idx = fsops::trash_index();
                    idx.retain(|(name, _)| !paths.iter().any(|p| basename(p) == name));
                    let _ = fsops::write_trash_index(&idx);
                }
                self.toast(format!("{} élément(s) supprimé(s) définitivement", n), ToastKind::Success);
                self.reload();
            }
            Pending::EmptyTrash => match fsops::empty_trash() {
                Ok(n) => {
                    self.toast(format!("Corbeille vidée ({} élément(s))", n), ToastKind::Success);
                    self.reload();
                }
                Err(e) => self.toast(e, ToastKind::Error),
            },
            Pending::None => {}
        }
    }

    /// Rouvre la boîte de saisie avec un message d'erreur et le nom tenté.
    fn reopen_with_error(&mut self, e: String, attempted: &str) {
        let (title, msg) = match &self.pending {
            Pending::Rename(old) => ("Renommer".to_string(), format!("Nouveau nom de « {} » :", old)),
            Pending::NewDir => ("Nouveau dossier".to_string(), "Nom du dossier :".to_string()),
            Pending::NewFile => ("Nouveau fichier".to_string(), "Nom du fichier :".to_string()),
            _ => {
                self.toast(e, ToastKind::Error);
                return;
            }
        };
        let mut d = Dialog::prompt(2, &title, &msg, attempted, "Valider");
        d.error = Some(e);
        self.dialog = Some(d);
    }

    fn show_properties(&mut self) {
        let paths = self.selection_paths();
        let Some(path) = paths.first().cloned() else { return };
        let mut lines = Vec::new();
        if paths.len() > 1 {
            let (n, b) = self.selection_size();
            lines.push(format!("{} éléments sélectionnés, {} (fichiers)", n, human_bytes(b)));
        }
        lines.push(format!("Emplacement : {}", parent_of(&path)));
        let kind = self.items.iter().find(|i| i.name == basename(&path)).map(|i| i.kind).unwrap_or(FileKind::Binary);
        lines.push(format!("Type : {}", kind.label()));
        match preview::build(&path, kind) {
            Preview::Info { fields, .. } => {
                for (k, v) in fields {
                    lines.push(format!("{} : {}", k, v));
                }
            }
            Preview::Text { lines: l, truncated } => {
                if let Some(FileOrDir::File(f)) = fsops::lookup(&path) {
                    lines.push(format!("Taille : {}", human_bytes(f.lock().len() as u64)));
                }
                lines.push(format!("Lignes (aperçu) : {}{}", l.len(), if truncated { "+" } else { "" }));
            }
            Preview::None => {}
        }
        if self.cwd == TRASH {
            if let Some((_, orig)) = self.trash_names.iter().find(|(n, _)| n == basename(&path)) {
                lines.push(format!("Emplacement d'origine : {}", orig));
            }
        }
        let title = format!("Propriétés de « {} »", basename(&path));
        self.dialog = Some(Dialog::info(9, &title, lines));
        self.pending = Pending::None;
        self.dirty = true;
    }

    fn start_search(&mut self) {
        let q = self.filter.text();
        if q.trim().is_empty() {
            self.search = None;
        } else if let Some(d) = self.dir.clone() {
            self.search = Some(Search::new(d, &self.cwd, q.trim()));
            self.cursor = Some(0);
            self.scroll = Scroll::default();
        }
        self.dirty = true;
    }

    // ── Menus ────────────────────────────────────────────────────

    fn open_item_menu(&mut self, x: isize, y: isize) {
        let in_trash = self.cwd == TRASH;
        let single = self.selection_paths().len() == 1;
        let kind = self.cursor_item().map(|i| i.kind);
        let has_clip = matches!(clipboard::get(), Clip::Files { .. });
        let open_label = if kind.map_or(false, |k| k.is_runnable()) { "Exécuter" } else { "Ouvrir" };
        let mut items = alloc::vec![MenuItem::new(M_OPEN, open_label, "Entrée")];
        if kind.map_or(false, |k| !matches!(k, FileKind::Folder | FileKind::Volume)) {
            items.push(MenuItem::new(M_VIEW, if kind.map_or(false, |k| k.is_textual()) { "Afficher le texte" } else { "Afficher en hexadécimal" }, ""));
        }
        if self.search.is_some() {
            items.push(MenuItem::new(M_OPEN_LOCATION, "Ouvrir l'emplacement", ""));
        }
        items.push(MenuItem::separator());
        if in_trash {
            items.push(MenuItem::new(M_RESTORE, "Restaurer", ""));
            items.push(MenuItem::new(M_DELETE, "Supprimer définitivement", "Suppr").danger());
        } else {
            items.push(MenuItem::new(M_CUT, "Couper", "Ctrl+X"));
            items.push(MenuItem::new(M_COPY, "Copier", "Ctrl+C"));
            items.push(MenuItem::new(M_PASTE, "Coller ici", "Ctrl+V").enabled(has_clip));
            items.push(MenuItem::new(M_DUPLICATE, "Dupliquer", "Ctrl+D"));
            items.push(MenuItem::new(M_RENAME, "Renommer", "F2").enabled(single && self.search.is_none()));
            items.push(MenuItem::separator());
            items.push(MenuItem::new(M_TRASH, "Mettre à la corbeille", "Suppr"));
            items.push(MenuItem::new(M_DELETE, "Supprimer définitivement", "Maj+Suppr").danger());
        }
        items.push(MenuItem::separator());
        items.push(MenuItem::new(M_COPY_PATH, "Copier le chemin", "Ctrl+Maj+C"));
        items.push(MenuItem::new(M_PROPS, "Propriétés", "Alt+Entrée"));
        self.menu = Some(Menu::new(items, x, y, self.area));
        self.dirty = true;
    }

    fn open_background_menu(&mut self, x: isize, y: isize) {
        let has_clip = matches!(clipboard::get(), Clip::Files { .. });
        let mut items = Vec::new();
        if self.cwd == TRASH {
            items.push(MenuItem::new(M_EMPTY_TRASH, "Vider la corbeille", "").danger());
            items.push(MenuItem::separator());
        } else {
            items.push(MenuItem::new(M_NEW_DIR, "Nouveau dossier", "Ctrl+Maj+N"));
            items.push(MenuItem::new(M_NEW_FILE, "Nouveau fichier texte", "Ctrl+N"));
            items.push(MenuItem::new(M_PASTE, "Coller", "Ctrl+V").enabled(has_clip));
            items.push(MenuItem::separator());
        }
        items.push(MenuItem::new(M_SELECT_ALL, "Tout sélectionner", "Ctrl+A"));
        items.push(MenuItem::new(M_REFRESH, "Actualiser", "F5"));
        items.push(MenuItem::new(M_HIDDEN, if self.show_hidden { "Masquer les fichiers cachés" } else { "Afficher les fichiers cachés" }, "Ctrl+H"));
        items.push(MenuItem::new(M_MODE, if self.mode == Mode::List { "Vue en grille" } else { "Vue en liste" }, "Ctrl+2"));
        self.menu = Some(Menu::new(items, x, y, self.area));
        self.dirty = true;
    }

    fn menu_action(&mut self, id: u32) {
        match id {
            M_OPEN => self.open_cursor(),
            M_VIEW => {
                if let Some(p) = self.selection_paths().first().cloned() {
                    let k = self.items.iter().find(|i| i.name == basename(&p)).map(|i| i.kind).unwrap_or(FileKind::Binary);
                    self.open_viewer(&p, k);
                }
            }
            M_OPEN_LOCATION => {
                if let Some(h) = self.search.as_ref().and_then(|s| self.cursor.and_then(|c| s.hits.get(c))).cloned() {
                    self.navigate(&h.dir);
                    self.select_name(&h.name);
                }
            }
            M_CUT => self.copy_selection(true),
            M_COPY => self.copy_selection(false),
            M_PASTE => self.paste(),
            M_DUPLICATE => self.duplicate(),
            M_RENAME => self.ask_rename(),
            M_TRASH => self.trash_selection(),
            M_DELETE => {
                let p = self.selection_paths();
                self.ask_delete_forever(p);
            }
            M_RESTORE => self.restore_selection(),
            M_COPY_PATH => {
                let p = self.selection_paths().join("\n");
                clipboard::set_text(&p);
                self.toast("Chemin copié".into(), ToastKind::Info);
            }
            M_PROPS => self.show_properties(),
            M_NEW_DIR => self.ask_new(true),
            M_NEW_FILE => self.ask_new(false),
            M_REFRESH => self.reload(),
            M_HIDDEN => {
                self.show_hidden = !self.show_hidden;
                self.rebuild_view();
            }
            M_MODE => self.toggle_mode(),
            M_EMPTY_TRASH => {
                self.dialog = Some(Dialog::confirm(4, "Vider la corbeille", &["Supprimer définitivement tout le contenu de la corbeille ?"], "Vider", true));
                self.pending = Pending::EmptyTrash;
            }
            M_SELECT_ALL => self.select_all(),
            _ => {}
        }
        self.dirty = true;
    }

    fn toggle_mode(&mut self) {
        self.mode = if self.mode == Mode::List { Mode::Grid } else { Mode::List };
        self.scroll = Scroll::default();
        self.follow_cursor();
        self.dirty = true;
    }

    fn select_all(&mut self) {
        self.sel = self.view.iter().map(|&i| self.items[i].name.clone()).collect();
        self.dirty = true;
    }

    // ── Clavier ──────────────────────────────────────────────────

    fn on_key(&mut self, ev: &KeyEvent) {
        self.mods = ev.modifiers;
        if let Some(d) = self.dialog.as_mut() {
            let r = d.on_key(ev);
            if r != DialogResult::Open {
                let v = d.value();
                self.dialog = None;
                self.dialog_done(r, v);
            }
            self.dirty = true;
            return;
        }
        if self.menu.is_some() {
            if ev.keycode == Keycode::Escape {
                self.menu = None;
                self.dirty = true;
            }
            return;
        }
        if self.viewer.is_some() {
            self.viewer_key(ev);
            return;
        }
        if let Some(pe) = self.path_edit.as_mut() {
            match pe.on_key(ev) {
                FieldEvent::Submit => {
                    let target = fsops::normalize(&self.cwd, &pe.text());
                    self.path_edit = None;
                    match fsops::lookup(&target) {
                        Some(FileOrDir::Dir(_)) => self.navigate(&target),
                        Some(FileOrDir::File(_)) => {
                            let name = basename(&target).to_string();
                            self.navigate(&parent_of(&target));
                            self.select_name(&name);
                        }
                        None => self.toast(format!("Chemin introuvable : {}", target), ToastKind::Error),
                    }
                }
                FieldEvent::Cancel => self.path_edit = None,
                _ => {}
            }
            self.dirty = true;
            return;
        }
        let ctrl = ev.modifiers.is_control();
        let shift = ev.modifiers.is_shift();
        let alt = ev.modifiers.is_alt();
        match ctrl_letter(ev) {
            Some('f') => {
                self.filter_focus = true;
                self.filter.select_all();
                self.dirty = true;
                return;
            }
            Some('l') => {
                self.path_edit = Some(TextField::with_text(&self.cwd));
                if let Some(p) = self.path_edit.as_mut() {
                    p.select_all();
                }
                self.dirty = true;
                return;
            }
            _ => {}
        }
        if self.filter_focus {
            match self.filter.on_key(ev) {
                FieldEvent::Changed => {
                    self.search = None;
                    self.rebuild_view();
                    self.cursor = if self.view.is_empty() { None } else { Some(0) };
                    self.scroll = Scroll::default();
                }
                FieldEvent::Submit => self.start_search(),
                FieldEvent::Cancel => {
                    self.filter.set_text("");
                    self.filter_focus = false;
                    self.search = None;
                    self.rebuild_view();
                }
                FieldEvent::Ignored => {
                    self.nav_key(ev, shift);
                }
                FieldEvent::None => {}
            }
            self.dirty = true;
            return;
        }
        if ctrl {
            match ctrl_letter(ev) {
                Some('c') if shift => self.menu_action(M_COPY_PATH),
                Some('c') => self.copy_selection(false),
                Some('x') => self.copy_selection(true),
                Some('v') => self.paste(),
                Some('z') => self.do_undo(),
                Some('a') => self.select_all(),
                Some('d') => self.duplicate(),
                Some('h') => self.menu_action(M_HIDDEN),
                Some('n') => self.ask_new(shift),
                _ => match ev.keycode {
                    Keycode::Num1 => {
                        if self.mode != Mode::List {
                            self.toggle_mode();
                        }
                    }
                    Keycode::Num2 => {
                        if self.mode != Mode::Grid {
                            self.toggle_mode();
                        }
                    }
                    _ => {}
                },
            }
            return;
        }
        if alt {
            match ev.keycode {
                Keycode::Left => self.go_back(),
                Keycode::Right => self.go_forward(),
                Keycode::Up => self.go_up(),
                Keycode::Enter => self.show_properties(),
                _ => {}
            }
            return;
        }
        match ev.keycode {
            Keycode::F10 if shift => self.open_menu_keyboard(),
            Keycode::F2 => self.ask_rename(),
            Keycode::F3 => {
                self.show_preview = !self.show_preview;
                self.dirty = true;
            }
            Keycode::F5 => {
                self.refresh_places();
                self.reload();
            }
            Keycode::Delete => {
                if shift {
                    let p = self.selection_paths();
                    self.ask_delete_forever(p);
                } else {
                    self.trash_selection();
                }
            }
            Keycode::Backspace => self.go_up(),
            Keycode::Enter => self.open_cursor(),
            Keycode::Escape => {
                if self.search.is_some() || !self.filter.is_empty() {
                    self.search = None;
                    self.filter.set_text("");
                    self.rebuild_view();
                } else {
                    self.sel.clear();
                }
                self.dirty = true;
            }
            Keycode::Menu => self.open_menu_keyboard(),
            _ => {
                if !self.nav_key(ev, shift) {
                    self.type_ahead(ev);
                }
            }
        }
    }

    /// Menu contextuel ouvert au clavier (touche Menu, Maj+F10), sous l'élément courant.
    fn open_menu_keyboard(&mut self) {
        let r = self.cursor.map(|c| self.item_rect(c)).unwrap_or(self.hits.list);
        if self.cursor.is_some() {
            self.open_item_menu(r.x + 40, r.bottom());
        } else {
            self.open_background_menu(r.x + 40, r.y + 40);
        }
    }

    /// Recherche par les premières lettres (comme l'Explorateur et Finder).
    fn type_ahead(&mut self, ev: &KeyEvent) {
        let Some(ch) = key_char(ev) else { return };
        let now = now_ns();
        if now.saturating_sub(self.typeahead_at) > 1_000_000_000 {
            self.typeahead.clear();
        }
        self.typeahead_at = now;
        self.typeahead.push(fsops::fold(ch));
        let q = self.typeahead.clone();
        let n = self.view.len();
        let start = if q.chars().count() == 1 { self.cursor.map_or(0, |c| c + 1) } else { self.cursor.unwrap_or(0) };
        for k in 0..n {
            let i = (start + k) % n;
            let name: String = self.items[self.view[i]].name.chars().map(fsops::fold).collect();
            if name.starts_with(q.as_str()) {
                let nm = self.items[self.view[i]].name.clone();
                self.select_name(&nm);
                return;
            }
        }
    }

    /// Flèches et pages ; renvoie `false` si la touche n'est pas une touche de navigation.
    fn nav_key(&mut self, ev: &KeyEvent, shift: bool) -> bool {
        let n = if let Some(s) = &self.search { s.hits.len() } else { self.view.len() };
        if n == 0 {
            return matches!(ev.keycode, Keycode::Up | Keycode::Down | Keycode::PageUp | Keycode::PageDown | Keycode::Home | Keycode::End);
        }
        let pr = if self.search.is_some() { 1 } else { self.per_row() };
        let page = self.visible_lines().max(1) * pr;
        let cur = self.cursor;
        let next = match ev.keycode {
            Keycode::Up => cur.map_or(0, |c| c.saturating_sub(pr)),
            Keycode::Down => cur.map_or(0, |c| (c + pr).min(n - 1)),
            Keycode::Left if pr > 1 => cur.map_or(0, |c| c.saturating_sub(1)),
            Keycode::Right if pr > 1 => cur.map_or(0, |c| (c + 1).min(n - 1)),
            Keycode::PageUp => cur.map_or(0, |c| c.saturating_sub(page)),
            Keycode::PageDown => cur.map_or(0, |c| (c + page).min(n - 1)),
            Keycode::Home => 0,
            Keycode::End => n - 1,
            _ => return false,
        };
        self.move_cursor(next, shift, false);
        true
    }

    fn move_cursor(&mut self, i: usize, extend: bool, toggle: bool) {
        self.cursor = Some(i);
        if self.search.is_none() {
            let name = self.items[self.view[i]].name.clone();
            if extend {
                let a = self.anchor.unwrap_or(i);
                let (lo, hi) = (a.min(i), a.max(i));
                self.sel = (lo..=hi).map(|k| self.items[self.view[k]].name.clone()).collect();
            } else if toggle {
                if !self.sel.remove(&name) {
                    self.sel.insert(name);
                }
                self.anchor = Some(i);
            } else {
                self.sel.clear();
                self.sel.insert(name);
                self.anchor = Some(i);
            }
        }
        self.follow_cursor();
        self.dirty = true;
    }

    fn viewer_key(&mut self, ev: &KeyEvent) {
        let vis = (self.area.h - TOOLBAR_H - 60).max(CHAR_H) / (CHAR_H + 2);
        let Some(v) = self.viewer.as_mut() else { return };
        if ctrl_letter(ev) == Some('f') {
            v.find_focus = true;
            v.find.select_all();
            self.dirty = true;
            return;
        }
        if v.find_focus {
            match v.find.on_key(ev) {
                FieldEvent::Changed => {
                    let q = v.find.text().to_lowercase();
                    v.matches.clear();
                    if !q.is_empty() {
                        for (li, l) in v.lines.iter().enumerate() {
                            let low = l.to_lowercase();
                            let mut from = 0;
                            while let Some(p) = low[from..].find(&q) {
                                let col = low[..from + p].chars().count();
                                v.matches.push((li, col));
                                from += p + q.len().max(1);
                                if v.matches.len() > 100_000 {
                                    break;
                                }
                            }
                        }
                    }
                    v.current = 0;
                    if let Some(&(l, _)) = v.matches.first() {
                        v.scroll = l.saturating_sub(vis as usize / 2);
                    }
                }
                FieldEvent::Submit => {
                    if !v.matches.is_empty() {
                        v.current = if ev.modifiers.is_shift() { (v.current + v.matches.len() - 1) % v.matches.len() } else { (v.current + 1) % v.matches.len() };
                        v.scroll = v.matches[v.current].0.saturating_sub(vis as usize / 2);
                    }
                }
                FieldEvent::Cancel => v.find_focus = false,
                _ => {}
            }
            self.dirty = true;
            return;
        }
        let n = v.lines.len();
        let page = vis.max(1) as usize;
        match ev.keycode {
            Keycode::Escape | Keycode::Q => {
                self.viewer = None;
            }
            Keycode::Up => v.scroll = v.scroll.saturating_sub(1),
            Keycode::Down => v.scroll += 1,
            Keycode::PageUp => v.scroll = v.scroll.saturating_sub(page),
            Keycode::PageDown | Keycode::Space => v.scroll += page,
            Keycode::Home => v.scroll = 0,
            Keycode::End => v.scroll = n,
            Keycode::Left => v.hscroll = v.hscroll.saturating_sub(8),
            Keycode::Right => v.hscroll += 8,
            Keycode::F3 => {
                if !v.matches.is_empty() {
                    v.current = (v.current + 1) % v.matches.len();
                    v.scroll = v.matches[v.current].0.saturating_sub(page / 2);
                }
            }
            _ => {}
        }
        if let Some(v) = self.viewer.as_mut() {
            v.scroll = v.scroll.min(n.saturating_sub(page));
        }
        self.dirty = true;
    }

    // ── Souris ───────────────────────────────────────────────────

    fn drop_target_at(&self, px: isize, py: isize) -> Option<Drop> {
        for (r, s) in &self.hits.side {
            if r.contains(px, py) {
                return Some(match s {
                    Side::Place(p) => Drop::Folder(p.clone()),
                    Side::Trash => Drop::Trash,
                });
            }
        }
        if self.search.is_some() {
            return None;
        }
        let i = self.index_at(px, py)?;
        let it = &self.items[self.view[i]];
        if it.is_dir && !self.sel.contains(&it.name) {
            return Some(Drop::Folder(self.path_of(&it.name)));
        }
        None
    }

    fn on_mouse(&mut self, ev: &event_types::MousePositionEvent) {
        let a = self.pointer.update(ev, now_ns());
        let (px, py) = (self.pointer.x, self.pointer.y);
        if a.moved {
            self.dirty = true;
        }

        if let Some(d) = self.dialog.as_mut() {
            if a.moved {
                d.pointer_move(self.area, px, py);
            }
            if a.left_released {
                let r = d.click(self.area, px, py);
                if r != DialogResult::Open {
                    let v = d.value();
                    self.dialog = None;
                    self.dialog_done(r, v);
                }
            }
            return;
        }
        if let Some(m) = self.menu.as_mut() {
            m.hover = m.item_at(px, py);
            if a.left_released || a.right_released {
                let pick = m.hover.map(|i| m.items[i].id);
                let inside = m.rect().contains(px, py);
                if inside || a.left_released {
                    self.menu = None;
                    if let Some(id) = pick {
                        self.menu_action(id);
                    }
                }
            }
            return;
        }
        if self.viewer.is_some() {
            if a.wheel != 0 {
                if let Some(v) = self.viewer.as_mut() {
                    if a.wheel > 0 {
                        v.scroll = v.scroll.saturating_sub(3 * a.wheel as usize);
                    } else {
                        v.scroll = (v.scroll + 3 * (-a.wheel) as usize).min(v.lines.len().saturating_sub(1));
                    }
                }
            }
            if a.left_pressed && self.hits.back.contains(px, py) {
                self.viewer = None;
            }
            return;
        }

        // Glisser-déposer en cours.
        if self.drag.is_some() {
            if self.pointer.left {
                let t = self.drop_target_at(px, py);
                if let Some(d) = self.drag.as_mut() {
                    d.over = t;
                }
                return;
            }
            let Some(drag) = self.drag.take() else { return };
            match drag.over {
                Some(Drop::Folder(dst)) => {
                    if self.mods.is_control() {
                        self.start_copy(&drag.paths, &dst);
                    } else {
                        self.move_paths(&drag.paths, &dst);
                    }
                }
                Some(Drop::Trash) => self.trash_selection(),
                None => {}
            }
            return;
        }
        // Lasso.
        if let Some((x0, y0)) = self.band {
            if self.pointer.left {
                self.band_select(x0, y0, px, py);
                return;
            }
            self.band = None;
            return;
        }
        // Barre de défilement.
        let (vis, total) = (self.visible_lines(), self.total_lines());
        if self.scroll.grab.is_some() {
            if self.pointer.left {
                self.scroll.drag(self.hits.track, py, vis, total);
            } else {
                self.scroll.grab = None;
            }
            return;
        }
        if a.wheel != 0 && self.hits.list.contains(px, py) {
            self.scroll.wheel(a.wheel, vis, total);
        }
        self.hover_col = self.hits.columns.iter().position(|r| r.contains(px, py));
        self.hover_index = self.index_at(px, py);

        // Début d'un glisser depuis un élément sélectionné, ou d'un lasso sur le vide.
        if a.drag_started && self.search.is_none() {
            let (ox, oy) = self.pointer.press_origin();
            match self.index_at(ox, oy) {
                Some(i) => {
                    let name = self.items[self.view[i]].name.clone();
                    if !self.sel.contains(&name) {
                        self.move_cursor(i, false, false);
                    }
                    let paths = self.selection_paths();
                    if self.cwd != TRASH && !paths.is_empty() {
                        self.drag = Some(Drag { paths, over: None });
                    }
                }
                None if self.hits.list.contains(ox, oy) => {
                    if !self.mods.is_control() {
                        self.sel.clear();
                    }
                    self.band = Some((ox, oy));
                }
                None => {}
            }
            return;
        }

        if a.left_pressed {
            self.left_press(px, py, a.clicks);
        }
        if a.right_pressed {
            if let Some(i) = self.index_at(px, py) {
                if self.search.is_none() {
                    let name = self.items[self.view[i]].name.clone();
                    if !self.sel.contains(&name) {
                        self.move_cursor(i, false, false);
                    }
                } else {
                    self.cursor = Some(i);
                }
                self.open_item_menu(px, py);
            } else if self.hits.list.contains(px, py) {
                self.sel.clear();
                self.cursor = None;
                self.open_background_menu(px, py);
            }
        }
    }

    fn band_select(&mut self, x0: isize, y0: isize, x1: isize, y1: isize) {
        let r = Rect::new(x0.min(x1), y0.min(y1), (x1 - x0).abs(), (y1 - y0).abs());
        let n = self.view.len();
        let first = self.scroll.offset * self.per_row();
        let last = (first + (self.visible_lines() + 1) * self.per_row()).min(n);
        for i in first..last {
            let ir = self.item_rect(i);
            let name = self.items[self.view[i]].name.clone();
            if !ir.intersect(&r).is_empty() {
                self.sel.insert(name);
            } else if !self.mods.is_control() {
                self.sel.remove(&name);
            }
        }
        self.dirty = true;
    }

    fn left_press(&mut self, px: isize, py: isize, clicks: u8) {
        // Copie des zones (les actions ci-dessous modifient `self`).
        let h = &self.hits;
        let (back, fwd, up, refresh, mode_btn, preview_btn, job_cancel, search, path_area, track) =
            (h.back, h.fwd, h.up, h.refresh, h.mode_btn, h.preview_btn, h.job_cancel, h.search, h.path_area, h.track);
        let crumb = h.crumbs.iter().find(|(r, _)| r.contains(px, py)).map(|(_, p)| p.clone());
        let side = h.side.iter().find(|(r, _)| r.contains(px, py)).map(|(_, s)| s.clone());
        let column = h.columns.iter().position(|r| r.contains(px, py));
        if self.filter_focus && !search.contains(px, py) {
            self.filter_focus = false;
        }
        if self.path_edit.is_some() && !path_area.contains(px, py) {
            self.path_edit = None;
        }
        if back.contains(px, py) {
            return self.go_back();
        }
        if fwd.contains(px, py) {
            return self.go_forward();
        }
        if up.contains(px, py) {
            return self.go_up();
        }
        if refresh.contains(px, py) {
            self.refresh_places();
            return self.reload();
        }
        if mode_btn.contains(px, py) {
            return self.toggle_mode();
        }
        if preview_btn.contains(px, py) {
            self.show_preview = !self.show_preview;
            return;
        }
        if job_cancel.contains(px, py) {
            if let Some(j) = self.job.as_mut() {
                j.cancel();
            }
            return;
        }
        if search.contains(px, py) {
            self.filter_focus = true;
            self.filter.click(search, px);
            return;
        }
        if let Some(p) = crumb {
            return self.navigate(&p);
        }
        if path_area.contains(px, py) && self.path_edit.is_none() {
            let mut f = TextField::with_text(&self.cwd);
            f.select_all();
            self.path_edit = Some(f);
            return;
        }
        if let Some(s) = side {
            match s {
                Side::Place(p) => self.navigate(&p),
                Side::Trash => {
                    let _ = fsops::trash_dir();
                    self.navigate(TRASH);
                }
            }
            return;
        }
        if let Some(c) = column {
            if self.search.is_none() {
                self.sort = if self.sort.0 == c { (c, !self.sort.1) } else { (c, true) };
                self.rebuild_view();
            }
            return;
        }
        if track.contains(px, py) {
            let (vis, total) = (self.visible_lines(), self.total_lines());
            self.scroll.press(track, py, vis, total);
            return;
        }
        match self.index_at(px, py) {
            Some(i) => {
                if clicks >= 2 && Some(i) == self.cursor {
                    self.open_cursor();
                    return;
                }
                if self.search.is_some() {
                    self.cursor = Some(i);
                } else {
                    self.move_cursor(i, self.mods.is_shift(), self.mods.is_control());
                }
            }
            None if self.hits.list.contains(px, py) => {
                if !self.mods.is_control() {
                    self.sel.clear();
                    self.cursor = None;
                }
            }
            None => {}
        }
    }

    // ── Tâches de fond (copie, recherche, surveillance) ──────────

    fn tick(&mut self, now: u64) {
        if let Some(j) = self.job.as_mut() {
            j.run(6_000_000);
            self.dirty = true;
        }
        if self.job.as_ref().map_or(false, |j| j.finished()) {
            if let Some(j) = self.job.take() {
                if j.errors.is_empty() {
                    self.toast(format!("{} terminée ({})", j.label, human_bytes(j.done_bytes)), ToastKind::Success);
                } else {
                    self.toast(format!("{} : {} erreur(s), {}", j.label, j.errors.len(), j.errors[0]), ToastKind::Error);
                }
                if j.errors.is_empty() && !j.move_sources.is_empty() {
                    for src in &j.move_sources {
                        if let Err(e) = fsops::remove(src) {
                            self.toast(format!("Source conservée : {}", e), ToastKind::Error);
                        }
                    }
                } else if !j.created.is_empty() {
                    self.undo.push(Undo::Create(j.created));
                }
                self.reload();
            }
        }
        if let Some(s) = self.search.as_mut() {
            if !s.done && s.run(5_000_000) {
                self.dirty = true;
            }
        }
        // Surveillance du dossier (faute de notifications du VFS) : relecture si l'empreinte change.
        if now.saturating_sub(self.last_poll) > 1_500_000_000 && self.job.is_none() && self.dialog.is_none() {
            self.last_poll = now;
            if let Some(d) = &self.dir {
                if fsops::fingerprint(d) != self.fingerprint {
                    self.reload();
                }
            }
        }
    }

    // ── Dessin ───────────────────────────────────────────────────

    fn draw(&mut self, c: &mut Canvas) {
        let area = self.area;
        c.set_clip(area);
        c.fill(area, pal::BG);
        self.hits = Hits::default();
        let (toolbar, rest) = area.split_top(TOOLBAR_H);
        let (body, status) = rest.split_bottom(STATUS_H);
        self.draw_toolbar(c, toolbar);
        if self.viewer.is_some() {
            self.draw_viewer(c, body);
        } else {
            let (side, rest) = body.split_left(SIDEBAR_W);
            self.draw_sidebar(c, side);
            let show_prev = self.show_preview && rest.w > 700;
            let (main, prev) = if show_prev { rest.split_right(PREVIEW_W) } else { (rest, Rect::default()) };
            self.hits.main = main;
            self.draw_main(c, main);
            if show_prev {
                self.draw_preview(c, prev);
            }
        }
        self.draw_status(c, status);
        if let Some(d) = &self.drag {
            let label = if d.paths.len() == 1 { basename(&d.paths[0]).to_string() } else { format!("{} éléments", d.paths.len()) };
            let action = match &d.over {
                Some(Drop::Folder(p)) => format!("{} vers {}", if self.mods.is_control() { "Copier" } else { "Déplacer" }, basename(p)),
                Some(Drop::Trash) => "Mettre à la corbeille".to_string(),
                None => label.clone(),
            };
            let w = text_width(&action) + 44;
            let r = Rect::new(self.pointer.x + 14, self.pointer.y + 10, w, 28);
            c.shadow(r, 8, 6, 120);
            c.round_rect_alpha(r, 8, pal::SURFACE3, 235);
            icons::draw(c, Icon::File, r.x + 8, r.y + 6, 16);
            c.text(r.x + 30, r.y + 6, &action, pal::TEXT);
        }
        if let Some(m) = &self.menu {
            m.draw(c);
        }
        if let Some(d) = self.dialog.as_mut() {
            d.draw(c, area);
        }
        if let Some(t) = &self.toast {
            t.draw(c, area, now_ns());
        }
    }

    fn icon_button(&self, c: &mut Canvas, r: Rect, icon: Icon, enabled: bool) -> Rect {
        let hover = enabled && r.contains(self.pointer.x, self.pointer.y);
        if hover {
            c.round_rect(r, 8, pal::SURFACE3);
        }
        let s = 16;
        if enabled {
            icons::draw(c, icon, r.x + (r.w - s) / 2, r.y + (r.h - s) / 2, s);
        } else {
            // Icône grisée : dessinée puis voilée.
            icons::draw(c, icon, r.x + (r.w - s) / 2, r.y + (r.h - s) / 2, s);
            c.fill_alpha(r.inset(4), pal::SURFACE1, 170);
        }
        r
    }

    fn draw_toolbar(&mut self, c: &mut Canvas, r: Rect) {
        c.vgradient(r, pal::SURFACE1, pal::SURFACE0);
        c.hline(r.x, r.right(), r.bottom() - 1, pal::BORDER);
        let by = r.y + (r.h - 34) / 2;
        let mut x = r.x + 10;
        let viewer = self.viewer.is_some();
        self.hits.back = self.icon_button(c, Rect::new(x, by, 34, 34), Icon::Back, viewer || !self.back.is_empty());
        x += 38;
        self.hits.fwd = self.icon_button(c, Rect::new(x, by, 34, 34), Icon::Forward, !viewer && !self.fwd.is_empty());
        x += 38;
        self.hits.up = self.icon_button(c, Rect::new(x, by, 34, 34), Icon::Up, !viewer && self.cwd != "/");
        x += 38;
        self.hits.refresh = self.icon_button(c, Rect::new(x, by, 34, 34), Icon::Refresh, !viewer);
        x += 46;
        // Droite : basculements et recherche.
        let mut rx = r.right() - 10;
        rx -= 34;
        self.hits.preview_btn = self.icon_button(c, Rect::new(rx, by, 34, 34), Icon::Text, !viewer);
        if self.show_preview && !viewer {
            c.round_rect(Rect::new(rx + 9, by + 30, 16, 3), 1, pal::ACCENT);
        }
        rx -= 38;
        let mode_icon = if self.mode == Mode::List { Icon::Grid } else { Icon::List };
        self.hits.mode_btn = self.icon_button(c, Rect::new(rx, by, 34, 34), mode_icon, !viewer);
        let sw = 250.min((r.w / 4).max(150));
        rx -= sw + 10;
        let sr = Rect::new(rx, by + 2, sw, 30);
        if !viewer {
            let ph = "Rechercher (Entrée : sous-dossiers)";
            self.filter.draw(c, sr, self.filter_focus, ph, 20);
            icons::draw(c, Icon::Search, sr.x + 8, sr.y + 7, 16);
            self.hits.search = sr;
        }
        // Fil d'Ariane ou saisie du chemin.
        let pa = Rect::new(x, by + 2, (rx - 12 - x).max(80), 30);
        self.hits.path_area = pa;
        if viewer {
            if let Some(v) = &self.viewer {
                c.text_bold(pa.x + 4, pa.y + 7, &v.title, pal::TEXT);
            }
            return;
        }
        if let Some(pe) = self.path_edit.as_mut() {
            pe.draw(c, pa, true, "", 0);
            return;
        }
        c.round_rect(pa, 7, pal::SURFACE0);
        c.round_stroke(pa, 7, pal::BORDER, 255);
        // Segments, du dernier au premier, tant qu'ils tiennent.
        let mut segs: Vec<(String, String)> = Vec::new();
        segs.push(("MaiOS".to_string(), "/".to_string()));
        let mut acc = String::new();
        for s in self.cwd.split('/').filter(|s| !s.is_empty()) {
            acc.push('/');
            acc.push_str(s);
            segs.push((s.to_string(), acc.clone()));
        }
        let sep_w = CHAR_W * 3;
        let mut widths: Vec<isize> = segs.iter().map(|(l, _)| text_width(l) + 16).collect();
        let avail = pa.w - 16;
        let mut first = 0;
        while first + 1 < segs.len() && widths[first..].iter().sum::<isize>() + sep_w * (segs.len() - first) as isize > avail {
            first += 1;
        }
        let mut cx = pa.x + 6;
        let saved = c.push_clip(pa.inset(2));
        if first > 0 {
            c.text(cx, pa.y + 7, "..", pal::TEXT_DIM);
            cx += CHAR_W * 2 + 6;
        }
        let last = segs.len() - 1;
        for (i, (label, path)) in segs.iter().enumerate().skip(first) {
            let w = widths[i].min(pa.right() - cx - 4);
            let sr = Rect::new(cx, pa.y + 3, w, pa.h - 6);
            let hover = sr.contains(self.pointer.x, self.pointer.y);
            if hover {
                c.round_rect(sr, 5, pal::SURFACE3);
            }
            let col = if i == last { pal::TEXT } else { pal::TEXT_SOFT };
            if i == last {
                c.text_bold(cx + 8, pa.y + 7, label, col);
            } else {
                c.text(cx + 8, pa.y + 7, label, col);
            }
            self.hits.crumbs.push((sr, path.clone()));
            cx += w;
            if i != last {
                c.text(cx + 4, pa.y + 7, ">", pal::TEXT_FAINT);
                cx += sep_w - 8;
            }
        }
        widths.clear();
        c.set_clip(saved);
    }

    fn draw_sidebar(&mut self, c: &mut Canvas, r: Rect) {
        c.fill(r, pal::SURFACE0);
        c.vline(r.right() - 1, r.y, r.bottom(), pal::BORDER);
        let saved = c.push_clip(r);
        let mut y = r.y + 12;
        let over = self.drag.as_ref().and_then(|d| d.over.clone());
        let section = |c: &mut Canvas, y: &mut isize, t: &str| {
            c.text(r.x + 16, *y, t, pal::TEXT_FAINT);
            *y += CHAR_H + 6;
        };
        section(c, &mut y, "EMPLACEMENTS");
        let places = self.places.clone();
        for (label, path, icon) in places.iter() {
            let row = Rect::new(r.x + 8, y, r.w - 16, SIDE_ROW);
            let on = &self.cwd == path;
            let hover = row.contains(self.pointer.x, self.pointer.y);
            let target = over.as_ref() == Some(&Drop::Folder(path.clone()));
            if on || hover || target {
                c.round_rect(row, 7, if target { pal::ACCENT_SOFT } else if on { pal::SELECTION } else { pal::SURFACE2 });
            }
            icons::draw(c, *icon, row.x + 8, row.y + (SIDE_ROW - 16) / 2, 16);
            c.text_in(Rect::new(row.x + 30, row.y, row.w - 30, SIDE_ROW), 4, label, if on { pal::TEXT } else { pal::TEXT_SOFT });
            self.hits.side.push((row, Side::Place(path.clone())));
            y += SIDE_ROW + 2;
        }
        y += 10;
        let recent: Vec<String> = self.recent.iter().filter(|p| !self.places.iter().any(|(_, q, _)| q == *p)).take(4).cloned().collect();
        if !recent.is_empty() {
            section(c, &mut y, "RÉCENTS");
            for p in recent {
                let row = Rect::new(r.x + 8, y, r.w - 16, SIDE_ROW);
                let on = self.cwd == p;
                let hover = row.contains(self.pointer.x, self.pointer.y);
                let target = over.as_ref() == Some(&Drop::Folder(p.clone()));
                if on || hover || target {
                    c.round_rect(row, 7, if target { pal::ACCENT_SOFT } else if on { pal::SELECTION } else { pal::SURFACE2 });
                }
                icons::draw(c, Icon::Clock, row.x + 8, row.y + (SIDE_ROW - 16) / 2, 16);
                let label = if p == "/" { "/".to_string() } else { basename(&p).to_string() };
                c.text_in(Rect::new(row.x + 30, row.y, row.w - 30, SIDE_ROW), 4, &label, pal::TEXT_SOFT);
                self.hits.side.push((row, Side::Place(p.clone())));
                y += SIDE_ROW + 2;
            }
            y += 10;
        }
        // Corbeille en bas du panneau.
        let n_trash = fsops::lookup_dir(TRASH).map_or(0, |d| d.lock().list().iter().filter(|n| n.as_str() != ".index").count());
        let row = Rect::new(r.x + 8, (r.bottom() - SIDE_ROW - 12).max(y), r.w - 16, SIDE_ROW);
        let on = self.cwd == TRASH;
        let hover = row.contains(self.pointer.x, self.pointer.y);
        let target = over == Some(Drop::Trash);
        if on || hover || target {
            c.round_rect(row, 7, if target { pal::RED.darken(150) } else if on { pal::SELECTION } else { pal::SURFACE2 });
        }
        icons::draw(c, Icon::Trash, row.x + 8, row.y + (SIDE_ROW - 16) / 2, 16);
        c.text_in(Rect::new(row.x + 30, row.y, row.w - 30, SIDE_ROW), 4, "Corbeille", pal::TEXT_SOFT);
        if n_trash > 0 {
            let s = format!("{}", n_trash);
            kit::chip(c, row.right() - text_width(&s) - 20, row.y + 6, &s, pal::RED);
        }
        self.hits.side.push((row, Side::Trash));
        c.set_clip(saved);
    }

    fn draw_main(&mut self, c: &mut Canvas, r: Rect) {
        let (body, job_bar) = if self.job.is_some() { r.split_bottom(64) } else { (r, Rect::default()) };
        if self.search.is_some() {
            self.draw_search(c, body);
        } else if self.mode == Mode::List {
            self.draw_list(c, body);
        } else {
            self.draw_grid(c, body);
        }
        if self.job.is_some() {
            self.draw_job(c, job_bar);
        }
    }

    fn draw_list(&mut self, c: &mut Canvas, r: Rect) {
        let (head, list) = r.split_top(COLHEAD_H);
        let (list, track) = list.split_right(SCROLL_W);
        let cols = kit::table_header(c, Rect::new(head.x, head.y, head.w - SCROLL_W, head.h), &LIST_COLS, Some(self.sort), self.hover_col);
        self.hits.columns = cols.clone();
        self.hits.list = list;
        self.hits.track = track;
        let vis = (list.h / ROW_H) as usize;
        self.scroll.clamp(vis, self.view.len());
        let saved = c.push_clip(list);
        let over = self.drag.as_ref().and_then(|d| d.over.clone());
        for (n, vi) in (self.scroll.offset..self.view.len()).take(vis + 1).enumerate() {
            let it = &self.items[self.view[vi]];
            let y = list.y + n as isize * ROW_H;
            let row = Rect::new(list.x, y, list.w, ROW_H);
            let selected = self.sel.contains(&it.name);
            let target = matches!(&over, Some(Drop::Folder(p)) if *p == self.path_of(&it.name));
            let bg = if target {
                pal::ACCENT_SOFT
            } else if selected {
                pal::SELECTION
            } else if self.hover_index == Some(vi) {
                pal::SURFACE2
            } else if n % 2 == 1 {
                pal::SURFACE0.mix(pal::BG, 128)
            } else {
                pal::BG
            };
            c.fill(row, bg);
            if self.cursor == Some(vi) {
                c.fill(Rect::new(row.x, y, 3, ROW_H), pal::ACCENT);
            }
            icons::draw(c, it.kind.icon(), cols[0].x + 10, y + (ROW_H - 16) / 2, 16);
            let fg = if it.hidden() { pal::TEXT_DIM } else { pal::TEXT };
            c.text_fit(cols[0].x + 34, y + (ROW_H - CHAR_H) / 2, cols[0].w - 42, &it.name, fg);
            let size = if it.is_dir { format!("{} élément{}", it.children, if it.children > 1 { "s" } else { "" }) } else { human_bytes(it.size) };
            kit::cell(c, Rect::new(cols[1].x, y, cols[1].w, ROW_H), Align::Right, &size, pal::TEXT_DIM);
            let kind = if self.cwd == TRASH {
                self.trash_names.iter().find(|(n, _)| *n == it.name).map(|(_, o)| o.clone()).unwrap_or_else(|| it.kind.label().to_string())
            } else {
                it.kind.label().to_string()
            };
            kit::cell(c, Rect::new(cols[2].x, y, cols[2].w, ROW_H), Align::Left, &kind, pal::TEXT_DIM);
        }
        self.draw_empty_hint(c, list);
        self.draw_band(c);
        c.set_clip(saved);
        let hover = track.contains(self.pointer.x, self.pointer.y);
        self.scroll.draw(c, track, vis, self.view.len(), hover);
    }

    fn draw_grid(&mut self, c: &mut Canvas, r: Rect) {
        let (list, track) = r.split_right(SCROLL_W);
        self.hits.list = list;
        self.hits.track = track;
        let pr = self.per_row();
        let vis = (list.h / TILE_H) as usize;
        let total = self.view.len().div_ceil(pr);
        self.scroll.clamp(vis, total);
        let saved = c.push_clip(list);
        let over = self.drag.as_ref().and_then(|d| d.over.clone());
        let first = self.scroll.offset * pr;
        for vi in (first..self.view.len()).take((vis + 1) * pr) {
            let it = &self.items[self.view[vi]];
            let t = self.item_rect(vi);
            let selected = self.sel.contains(&it.name);
            let target = matches!(&over, Some(Drop::Folder(p)) if *p == self.path_of(&it.name));
            if target || selected || self.hover_index == Some(vi) {
                c.round_rect(t, 10, if target { pal::ACCENT_SOFT } else if selected { pal::SELECTION } else { pal::SURFACE2 });
            }
            if self.cursor == Some(vi) {
                c.round_stroke(t, 10, pal::ACCENT, 200);
            }
            icons::draw(c, it.kind.icon(), t.x + (t.w - 40) / 2, t.y + 8, 40);
            // Nom sur deux lignes au plus, centré.
            let max = ((t.w - 10) / CHAR_W) as usize;
            let chars: Vec<char> = it.name.chars().collect();
            let l1: String = chars.iter().take(max).collect();
            c.text_center(Rect::new(t.x, t.y + 54, t.w, CHAR_H), &l1, if it.hidden() { pal::TEXT_DIM } else { pal::TEXT });
            if chars.len() > max {
                let rest: String = chars.iter().skip(max).collect();
                c.text_center(Rect::new(t.x + 4, t.y + 54 + CHAR_H, t.w - 8, CHAR_H), &rest, pal::TEXT_SOFT);
            }
        }
        self.draw_empty_hint(c, list);
        self.draw_band(c);
        c.set_clip(saved);
        let hover = track.contains(self.pointer.x, self.pointer.y);
        self.scroll.draw(c, track, vis, total, hover);
    }

    fn draw_band(&self, c: &mut Canvas) {
        if let Some((x0, y0)) = self.band {
            let (x1, y1) = (self.pointer.x, self.pointer.y);
            let r = Rect::new(x0.min(x1), y0.min(y1), (x1 - x0).abs(), (y1 - y0).abs());
            c.fill_alpha(r, pal::ACCENT, 50);
            c.stroke(r, pal::ACCENT);
        }
    }

    fn draw_empty_hint(&self, c: &mut Canvas, r: Rect) {
        if !self.view.is_empty() {
            return;
        }
        let msg = if !self.filter.is_empty() {
            "Aucun élément ne correspond (Entrée : chercher dans les sous-dossiers)"
        } else if self.cwd == TRASH {
            "La corbeille est vide"
        } else {
            "Ce dossier est vide · clic droit pour créer"
        };
        icons::draw(c, if self.cwd == TRASH { Icon::Trash } else { Icon::FolderOpen }, r.x + (r.w - 48) / 2, r.y + r.h / 2 - 60, 48);
        c.text_center(Rect::new(r.x, r.y + r.h / 2, r.w, CHAR_H), msg, pal::TEXT_DIM);
    }

    fn draw_search(&mut self, c: &mut Canvas, r: Rect) {
        let (banner, rest) = r.split_top(34);
        c.fill(banner, pal::SURFACE1);
        let (head, list) = rest.split_top(COLHEAD_H);
        let (list, track) = list.split_right(SCROLL_W);
        let cols = kit::table_header(c, Rect::new(head.x, head.y, head.w - SCROLL_W, head.h), &SEARCH_COLS, None, None);
        self.hits.list = list;
        self.hits.track = track;
        let Some(s) = &self.search else { return };
        let state = if s.done { "terminée".to_string() } else { "en cours...".to_string() };
        let msg = format!("Recherche « {} » dans {} : {} résultat(s), {} éléments parcourus, {}", s.query, self.cwd, s.hits.len(), s.scanned, state);
        icons::draw(c, Icon::Search, banner.x + 12, banner.y + 9, 16);
        c.text_in(Rect::new(banner.x + 34, banner.y, banner.w - 40, banner.h), 4, &msg, pal::TEXT_SOFT);
        let vis = (list.h / ROW_H) as usize;
        let mut sc = self.scroll;
        sc.clamp(vis, s.hits.len());
        let saved = c.push_clip(list);
        for (n, hi) in (sc.offset..s.hits.len()).take(vis + 1).enumerate() {
            let h = &s.hits[hi];
            let y = list.y + n as isize * ROW_H;
            let row = Rect::new(list.x, y, list.w, ROW_H);
            let on = self.cursor == Some(hi);
            c.fill(row, if on { pal::SELECTION } else if self.hover_index == Some(hi) { pal::SURFACE2 } else if n % 2 == 1 { pal::SURFACE0.mix(pal::BG, 128) } else { pal::BG });
            let icon = if h.is_dir { Icon::Folder } else { fsops::classify(&h.name, &h.path(), &[], h.size as usize).icon() };
            icons::draw(c, icon, cols[0].x + 10, y + (ROW_H - 16) / 2, 16);
            // Nom avec les lettres retrouvées surlignées.
            let tx = cols[0].x + 34;
            let ty = y + (ROW_H - CHAR_H) / 2;
            let maxc = ((cols[0].w - 42) / CHAR_W).max(0) as usize;
            let shown: String = h.name.chars().take(maxc).collect();
            c.text(tx, ty, &shown, pal::TEXT);
            for &p in &h.m.positions {
                if p < maxc {
                    if let Some(ch) = h.name.chars().nth(p) {
                        let mut b = [0u8; 4];
                        c.fill_alpha(Rect::new(tx + p as isize * CHAR_W, ty, CHAR_W, CHAR_H), pal::ACCENT, 50);
                        c.text_bold(tx + p as isize * CHAR_W, ty, ch.encode_utf8(&mut b), pal::YELLOW);
                    }
                }
            }
            kit::cell(c, Rect::new(cols[1].x, y, cols[1].w, ROW_H), Align::Left, &h.dir, pal::TEXT_DIM);
            let size = if h.is_dir { "-".to_string() } else { human_bytes(h.size) };
            kit::cell(c, Rect::new(cols[2].x, y, cols[2].w, ROW_H), Align::Right, &size, pal::TEXT_DIM);
        }
        c.set_clip(saved);
        let total = s.hits.len();
        let hover = track.contains(self.pointer.x, self.pointer.y);
        sc.draw(c, track, vis, total, hover);
        self.scroll = sc;
    }

    fn draw_job(&mut self, c: &mut Canvas, r: Rect) {
        let Some(j) = &self.job else { return };
        c.fill(r, pal::SURFACE1);
        c.hline(r.x, r.right(), r.y, pal::BORDER);
        c.text_fit(r.x + 16, r.y + 10, r.w - 160, &j.label, pal::TEXT);
        let detail = format!("{} / {} · {} / {} éléments", human_bytes(j.done_bytes), human_bytes(j.total_bytes), j.done_items, j.total_items);
        c.text_fit(r.x + 16, r.y + 38, r.w - 160, &detail, pal::TEXT_DIM);
        kit::meter(c, Rect::new(r.x + 16, r.y + 30, r.w - 170, 4), j.progress_pm(), pal::ACCENT);
        let br = Rect::new(r.right() - 130, r.y + 16, 114, 32);
        let hover = br.contains(self.pointer.x, self.pointer.y);
        kit::button(c, br, "Annuler", ButtonKind::Normal, hover, true);
        self.hits.job_cancel = br;
    }

    fn draw_preview(&mut self, c: &mut Canvas, r: Rect) {
        c.fill(r, pal::SURFACE0);
        c.vline(r.x, r.y, r.bottom(), pal::BORDER);
        let target = if let Some(s) = &self.search {
            self.cursor.and_then(|i| s.hits.get(i)).map(|h| (h.path(), h.name.clone(), if h.is_dir { FileKind::Folder } else { fsops::classify(&h.name, &h.path(), &[], h.size as usize) }, h.size))
        } else {
            self.cursor_item().map(|it| (self.path_of(&it.name), it.name.clone(), it.kind, it.size))
        };
        let inner = r.inset(16);
        let Some((path, name, kind, size)) = target else {
            let (n, b) = self.selection_size();
            let msg = if n > 1 { format!("{} éléments ({})", n, human_bytes(b)) } else { "Aucune sélection".to_string() };
            icons::draw(c, Icon::FolderOpen, inner.x + (inner.w - 48) / 2, inner.y + 40, 48);
            c.text_center(Rect::new(inner.x, inner.y + 100, inner.w, CHAR_H), &msg, pal::TEXT_DIM);
            return;
        };
        // Recalcul seulement si l'élément ou sa taille change.
        let kind = if kind == FileKind::Binary || kind == FileKind::Empty || matches!(kind, FileKind::Text | FileKind::Code) {
            match fsops::lookup(&path) {
                Some(FileOrDir::File(f)) => {
                    let head = fsops::read_head(&f, 512);
                    fsops::classify(&name, &path, &head, size as usize)
                }
                _ => kind,
            }
        } else {
            kind
        };
        let stale = self.preview.as_ref().map_or(true, |(p, s, _)| *p != path || *s != size);
        if stale {
            self.preview = Some((path.clone(), size, preview::build(&path, kind)));
        }
        icons::draw(c, kind.icon(), inner.x + (inner.w - 56) / 2, inner.y + 4, 56);
        c.text_center(Rect::new(inner.x, inner.y + 68, inner.w, CHAR_H), &name, pal::TEXT);
        let sub = if matches!(kind, FileKind::Folder | FileKind::Volume) { kind.label().to_string() } else { format!("{} · {}", kind.label(), human_bytes(size)) };
        c.text_center(Rect::new(inner.x, inner.y + 88, inner.w, CHAR_H), &sub, pal::TEXT_DIM);
        let mut y = inner.y + 118;
        c.hline(inner.x, inner.right(), y - 8, pal::BORDER);
        let saved = c.push_clip(Rect::new(r.x + 1, y, r.w - 1, r.bottom() - y));
        match self.preview.as_ref().map(|(_, _, p)| p) {
            Some(Preview::Text { lines, truncated }) => {
                let lh = CHAR_H + 1;
                for (i, l) in lines.iter().enumerate() {
                    if y + lh > r.bottom() {
                        break;
                    }
                    let num = format!("{:>3}", i + 1);
                    c.text_alpha(inner.x - 6, y, &num, pal::TEXT_FAINT, 200);
                    c.text_fit(inner.x + 26, y, inner.w - 26, l, pal::TEXT_SOFT);
                    y += lh;
                }
                if *truncated && y + lh <= r.bottom() {
                    c.text(inner.x, y, "...", pal::TEXT_DIM);
                }
            }
            Some(Preview::Info { fields, hex }) => {
                for (k, v) in fields {
                    if y + 40 > r.bottom() {
                        break;
                    }
                    c.text_fit(inner.x, y, inner.w, k, pal::TEXT_DIM);
                    c.text_fit(inner.x, y + 17, inner.w, v, pal::TEXT);
                    y += 40;
                }
                if !hex.is_empty() {
                    y += 6;
                    for l in hex {
                        if y + CHAR_H > r.bottom() {
                            break;
                        }
                        c.text_fit(inner.x - 4, y, inner.w + 8, l, pal::TEXT_FAINT);
                        y += CHAR_H + 1;
                    }
                }
            }
            _ => {}
        }
        c.set_clip(saved);
    }

    fn draw_viewer(&mut self, c: &mut Canvas, r: Rect) {
        let Some(v) = self.viewer.as_mut() else { return };
        let (head, body) = r.split_top(44);
        c.fill(head, pal::SURFACE1);
        c.hline(head.x, head.right(), head.bottom() - 1, pal::BORDER);
        c.text_fit(head.x + 16, head.y + 14, head.w - 380, &v.subtitle, pal::TEXT_DIM);
        let fr = Rect::new(head.right() - 340, head.y + 7, 220, 30);
        v.find.draw(c, fr, v.find_focus, "Chercher (Ctrl+F)", 0);
        let info = if v.matches.is_empty() { "Échap : fermer".to_string() } else { format!("{} / {}", v.current + 1, v.matches.len()) };
        c.text_right(head.right() - 14, head.y + 14, &info, pal::TEXT_DIM);
        c.fill(body, pal::SURFACE0);
        let lh = CHAR_H + 2;
        let vis = (body.h / lh) as usize;
        let gutter = if v.hex { 0 } else { CHAR_W * (format!("{}", v.lines.len()).len() as isize + 2) };
        let saved = c.push_clip(body);
        let q = v.find.text();
        let qlen = q.chars().count();
        for (n, li) in (v.scroll..v.lines.len()).take(vis + 1).enumerate() {
            let y = body.y + 4 + n as isize * lh;
            if !v.hex {
                let num = format!("{}", li + 1);
                c.text_right(body.x + gutter - CHAR_W, y, &num, pal::TEXT_FAINT);
            }
            let x0 = body.x + gutter + 8;
            let line: String = v.lines[li].chars().skip(v.hscroll).collect();
            // Surlignage des occurrences.
            for (mi, &(ml, mc)) in v.matches.iter().enumerate() {
                if ml == li && mc >= v.hscroll {
                    let hx = x0 + (mc - v.hscroll) as isize * CHAR_W;
                    let col = if mi == v.current { pal::ORANGE } else { pal::YELLOW };
                    c.fill_alpha(Rect::new(hx, y, qlen as isize * CHAR_W, CHAR_H), col, if mi == v.current { 150 } else { 70 });
                }
            }
            c.text(x0, y, &line, if v.hex { pal::TEXT_SOFT } else { pal::TEXT });
        }
        if !v.hex {
            c.vline(body.x + gutter, body.y, body.bottom(), pal::BORDER);
        }
        c.set_clip(saved);
        // Barre de défilement simple.
        let total = v.lines.len();
        let sc = Scroll { offset: v.scroll, grab: None };
        sc.draw(c, Rect::new(body.right() - SCROLL_W, body.y, SCROLL_W, body.h), vis, total, false);
    }

    fn draw_status(&mut self, c: &mut Canvas, r: Rect) {
        c.fill(r, pal::SURFACE0);
        c.hline(r.x, r.right(), r.y, pal::BORDER);
        let hidden = self.items.len().saturating_sub(self.view.len());
        let mut left = format!("{} élément{}", self.view.len(), if self.view.len() > 1 { "s" } else { "" });
        if hidden > 0 && self.filter.is_empty() {
            left.push_str(&format!(" ({} caché{})", hidden, if hidden > 1 { "s" } else { "" }));
        }
        let (n, b) = self.selection_size();
        if n > 0 {
            left.push_str(&format!("  ·  {} sélectionné{}", n, if n > 1 { "s" } else { "" }));
            if b > 0 {
                left.push_str(&format!(" ({})", human_bytes(b)));
            }
        }
        if let Clip::Files { paths, cut } = clipboard::get() {
            left.push_str(&format!("  ·  Presse-papiers : {} élément(s) {}", paths.len(), if cut { "coupé(s)" } else { "copié(s)" }));
        }
        if !self.undo.is_empty() {
            left.push_str("  ·  Ctrl+Z : annuler");
        }
        c.text_in(Rect::new(r.x, r.y, r.w - 260, r.h), 14, &left, pal::TEXT_DIM);
        let fa = frame_allocator::stats();
        let right = format!("Mémoire libre : {}", human_bytes(fa.free_frames as u64 * 4096));
        c.text_in_right(r, 14, &right, pal::TEXT_DIM);
    }
}

pub fn main(args: Vec<String>) -> isize {
    let mut win = match window::Window::with_title(
        "Fichiers".to_string(),
        Coord::new(70, 50),
        WIN_W,
        WIN_H,
        Color::new(0x001A1B26),
    ) {
        Ok(w) => w,
        Err(e) => {
            error!("[file_manager] fenêtre : {}", e);
            return -1;
        }
    };
    win.set_app_id("maios.files");
    let mut fm = Fm::new(content_rect(&win));
    // Dossier passé en argument (`file_manager /disk/apps`) : on l'ouvre. Fichier
    // (`file_manager /desktop/notes.txt`, bureau) : son dossier, le fichier choisi et ouvert
    // (visionneuse, ou programme lancé).
    if let Some(p) = args.first() {
        let target = fsops::normalize(&fm.cwd, p);
        if fsops::lookup_dir(&target).is_some() {
            fm.navigate(&target);
        } else if let Some(FileOrDir::File(_)) = fsops::lookup(&target) {
            fm.navigate(&fsops::parent_of(&target));
            fm.select_name(fsops::basename(&target));
            let kind = fm.cursor_item().map(|i| i.kind).unwrap_or(FileKind::Binary);
            fm.open_path(&target, kind);
        }
    }

    loop {
        loop {
            match win.handle_event() {
                Ok(Some(Event::ExitEvent)) => return 0,
                Ok(Some(Event::KeyboardEvent(ke))) => {
                    fm.mods = ke.key_event.modifiers;
                    if ke.key_event.action == KeyAction::Pressed {
                        fm.on_key(&ke.key_event);
                    }
                }
                Ok(Some(Event::MousePositionEvent(me))) => fm.on_mouse(&me),
                Ok(Some(Event::WindowResizeEvent(_))) => {
                    fm.area = content_rect(&win);
                    fm.dirty = true;
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        let now = now_ns();
        fm.tick(now);
        let animating = fm.toast.as_ref().map_or(false, |t| t.alive(now));
        if fm.toast.as_ref().map_or(false, |t| !t.alive(now)) {
            fm.toast = None;
            fm.dirty = true;
        }
        if fm.dirty || animating {
            {
                let mut fb = win.framebuffer_mut();
                let mut c = Canvas::from_framebuffer(&mut fb);
                fm.draw(&mut c);
            }
            if let Err(e) = win.render(None) {
                error!("[file_manager] rendu : {}", e);
            }
            fm.dirty = false;
        }
        let busy = fm.job.is_some() || fm.search.as_ref().map_or(false, |s| !s.done);
        let _ = sleep::sleep(sleep::Duration::from_millis(if busy { 4 } else { 16 }));
    }
}
