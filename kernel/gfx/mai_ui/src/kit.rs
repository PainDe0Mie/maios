//! Composants interactifs de MaiUI, dessinés sur un [`Canvas`] : palette, boutons, onglets,
//! champ de saisie éditable, en-têtes de tableau triables, barres de défilement, menus
//! contextuels, boîtes de dialogue, notifications et historiques de mesures.
//!
//! Modèle « mode immédiat » (Dear ImGui, Casey Muratori 2005) : l'application garde son état,
//! redessine tout à chaque changement, et chaque composant renvoie les zones à tester pour le
//! pointeur. Pas d'arbre de widgets à synchroniser, pas d'allocation dans le dessin.

use alloc::string::String;
use alloc::vec::Vec;
use keycodes_ascii::{KeyEvent, Keycode};

use crate::canvas::{rgb, text_width, Canvas, Rect, Rgb, CHAR_H, CHAR_W};
use crate::input::key_char;

// ================================================================
// PALETTE (Tokyo Night, niveaux de surface façon Material 3)
// ================================================================

pub mod pal {
    use crate::canvas::{rgb, Rgb};
    /// Fond de la fenêtre.
    pub const BG: Rgb = rgb(0x1A1B26);
    /// Surfaces de plus en plus élevées (panneaux, cartes, survol).
    pub const SURFACE0: Rgb = rgb(0x16161E);
    pub const SURFACE1: Rgb = rgb(0x1F2335);
    pub const SURFACE2: Rgb = rgb(0x24283B);
    pub const SURFACE3: Rgb = rgb(0x2F3549);
    pub const SURFACE4: Rgb = rgb(0x3B4261);
    pub const BORDER: Rgb = rgb(0x2A2E42);
    pub const BORDER_STRONG: Rgb = rgb(0x414868);
    pub const TEXT: Rgb = rgb(0xC0CAF5);
    pub const TEXT_SOFT: Rgb = rgb(0xA9B1D6);
    pub const TEXT_DIM: Rgb = rgb(0x737AA2);
    pub const TEXT_FAINT: Rgb = rgb(0x565F89);
    pub const ACCENT: Rgb = rgb(0x7AA2F7);
    pub const ACCENT_SOFT: Rgb = rgb(0x3D59A1);
    pub const SELECTION: Rgb = rgb(0x2E3C64);
    pub const GREEN: Rgb = rgb(0x9ECE6A);
    pub const TEAL: Rgb = rgb(0x73DACA);
    pub const CYAN: Rgb = rgb(0x7DCFFF);
    pub const YELLOW: Rgb = rgb(0xE0AF68);
    pub const ORANGE: Rgb = rgb(0xFF9E64);
    pub const RED: Rgb = rgb(0xF7768E);
    pub const PURPLE: Rgb = rgb(0xBB9AF7);
    pub const MAGENTA: Rgb = rgb(0xFF007C);
}

/// Couleur d'une charge (0–100 %) : vert, jaune, orange, rouge.
pub fn load_color(pct: u32) -> Rgb {
    match pct {
        0..=49 => pal::GREEN,
        50..=74 => pal::YELLOW,
        75..=89 => pal::ORANGE,
        _ => pal::RED,
    }
}

/// Teinte stable dérivée d'un entier (couleurs de séries, de catégories).
pub fn series_color(i: usize) -> Rgb {
    const S: [Rgb; 8] = [pal::ACCENT, pal::GREEN, pal::PURPLE, pal::ORANGE, pal::CYAN, pal::YELLOW, pal::TEAL, pal::RED];
    S[i % S.len()]
}

// ================================================================
// BOUTONS, PUCES, BARRES
// ================================================================

/// Style de bouton.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    /// Action principale (fond d'accent).
    Primary,
    /// Action ordinaire.
    Normal,
    /// Action destructrice (rouge).
    Danger,
    /// Bouton plat (barre d'outils), visible au survol.
    Flat,
}

/// Bouton à coins arrondis ; renvoie sa zone.
pub fn button(c: &mut Canvas, r: Rect, label: &str, kind: ButtonKind, hover: bool, enabled: bool) -> Rect {
    let (bg, fg) = match kind {
        ButtonKind::Primary => (pal::ACCENT, rgb(0x1A1B26)),
        ButtonKind::Danger => (pal::RED, rgb(0x1A1B26)),
        ButtonKind::Normal => (pal::SURFACE3, pal::TEXT),
        ButtonKind::Flat => (pal::SURFACE3, pal::TEXT),
    };
    let bg = if hover && enabled { bg.lighten(28) } else { bg };
    if kind != ButtonKind::Flat || (hover && enabled) {
        c.round_rect(r, 6, if enabled { bg } else { pal::SURFACE2 });
    }
    let fg = if enabled { fg } else { pal::TEXT_FAINT };
    c.text_center(r, label, fg);
    r
}

/// Largeur conseillée d'un bouton pour `label`.
pub fn button_width(label: &str) -> isize { text_width(label) + 24 }

/// Puce (badge) arrondie ; renvoie sa largeur.
pub fn chip(c: &mut Canvas, x: isize, y: isize, label: &str, fg: Rgb) -> isize {
    let w = text_width(label) + 12;
    c.round_rect_alpha(Rect::new(x, y, w, CHAR_H + 2), 8, fg, 46);
    c.text(x + 6, y + 1, label, fg);
    w
}

/// Barre de progression arrondie (0–1000 ‰).
pub fn meter(c: &mut Canvas, r: Rect, permille: u32, color: Rgb) {
    let rad = r.h / 2;
    c.round_rect(r, rad, pal::SURFACE3);
    let w = (r.w as i64 * permille.min(1000) as i64 / 1000) as isize;
    if w > 0 {
        c.round_rect(Rect::new(r.x, r.y, w.max(r.h), r.h), rad, color);
    }
}

/// Barre segmentée : plusieurs parts consécutives (`(‰, couleur)`), par exemple la mémoire
/// utilisée, en cache et libre.
pub fn stacked_meter(c: &mut Canvas, r: Rect, parts: &[(u32, Rgb)]) {
    c.round_rect(r, r.h / 2, pal::SURFACE3);
    let saved = c.push_clip(r);
    let mut x = r.x;
    for &(pm, col) in parts {
        let w = (r.w as i64 * pm.min(1000) as i64 / 1000) as isize;
        if w > 0 {
            c.fill(Rect::new(x, r.y, w, r.h), col);
        }
        x += w;
    }
    c.set_clip(saved);
}

// ================================================================
// ONGLETS
// ================================================================

/// Barre d'onglets soulignés ; renvoie la zone de chaque onglet.
pub fn tabs(c: &mut Canvas, r: Rect, labels: &[&str], active: usize, hover: Option<usize>) -> Vec<Rect> {
    c.fill(r, pal::SURFACE0);
    c.hline(r.x, r.right(), r.bottom() - 1, pal::BORDER);
    let mut out = Vec::with_capacity(labels.len());
    let mut x = r.x + 8;
    for (i, l) in labels.iter().enumerate() {
        let w = text_width(l) + 28;
        let tr = Rect::new(x, r.y, w, r.h);
        let on = i == active;
        if hover == Some(i) && !on {
            c.round_rect_alpha(tr.inset_xy(2, 5), 6, pal::SURFACE3, 200);
        }
        let fg = if on { pal::TEXT } else if hover == Some(i) { pal::TEXT_SOFT } else { pal::TEXT_DIM };
        if on {
            c.text_bold(tr.x + 14, tr.y + (tr.h - CHAR_H) / 2, l, fg);
            c.round_rect(Rect::new(tr.x + 10, tr.bottom() - 3, tr.w - 20, 3), 1, pal::ACCENT);
        } else {
            c.text(tr.x + 14, tr.y + (tr.h - CHAR_H) / 2, l, fg);
        }
        out.push(tr);
        x += w + 2;
    }
    out
}

// ================================================================
// CHAMP DE SAISIE
// ================================================================

/// Résultat d'une touche dans un [`TextField`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FieldEvent {
    None,
    /// Le texte a changé.
    Changed,
    /// Entrée.
    Submit,
    /// Échap.
    Cancel,
    /// Touche non consommée (flèches haut/bas…) : à traiter par l'application.
    Ignored,
}

/// Champ de saisie d'une ligne : curseur, sélection (Maj + flèches, Ctrl+A), suppression par
/// mot (Ctrl+Retour arrière), défilement horizontal.
#[derive(Clone, Default, Debug)]
pub struct TextField {
    chars: Vec<char>,
    /// Position du curseur (en caractères).
    pub cursor: usize,
    /// Autre extrémité de la sélection.
    anchor: Option<usize>,
    /// Premier caractère visible.
    scroll: usize,
}

impl TextField {
    pub fn new() -> TextField { TextField::default() }

    pub fn with_text(s: &str) -> TextField {
        let mut f = TextField::default();
        f.set_text(s);
        f
    }

    pub fn text(&self) -> String { self.chars.iter().collect() }
    pub fn is_empty(&self) -> bool { self.chars.is_empty() }

    pub fn set_text(&mut self, s: &str) {
        self.chars = s.chars().collect();
        self.cursor = self.chars.len();
        self.anchor = None;
        self.scroll = 0;
    }

    /// Sélectionne tout.
    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.chars.len();
    }

    /// Sélectionne la partie avant le dernier point (renommage : le nom sans l'extension).
    pub fn select_stem(&mut self) {
        let dot = self.chars.iter().rposition(|&c| c == '.').filter(|&i| i > 0);
        self.anchor = Some(0);
        self.cursor = dot.unwrap_or(self.chars.len());
    }

    fn selection(&self) -> Option<(usize, usize)> {
        let a = self.anchor?;
        (a != self.cursor).then(|| (a.min(self.cursor), a.max(self.cursor)))
    }

    fn delete_selection(&mut self) -> bool {
        if let Some((s, e)) = self.selection() {
            self.chars.drain(s..e);
            self.cursor = s;
            self.anchor = None;
            true
        } else {
            self.anchor = None;
            false
        }
    }

    fn word_left(&self, mut i: usize) -> usize {
        while i > 0 && !self.chars[i - 1].is_alphanumeric() {
            i -= 1;
        }
        while i > 0 && self.chars[i - 1].is_alphanumeric() {
            i -= 1;
        }
        i
    }

    fn word_right(&self, mut i: usize) -> usize {
        let n = self.chars.len();
        while i < n && !self.chars[i].is_alphanumeric() {
            i += 1;
        }
        while i < n && self.chars[i].is_alphanumeric() {
            i += 1;
        }
        i
    }

    fn move_to(&mut self, pos: usize, select: bool) {
        if select {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
        } else {
            self.anchor = None;
        }
        self.cursor = pos.min(self.chars.len());
    }

    /// Insère du texte à la place de la sélection.
    pub fn insert(&mut self, s: &str) {
        self.delete_selection();
        for ch in s.chars() {
            self.chars.insert(self.cursor, ch);
            self.cursor += 1;
        }
    }

    /// Traite une touche (appui seulement).
    pub fn on_key(&mut self, ev: &KeyEvent) -> FieldEvent {
        let shift = ev.modifiers.is_shift();
        let ctrl = ev.modifiers.is_control();
        match ev.keycode {
            Keycode::Enter => return FieldEvent::Submit,
            Keycode::Escape => return FieldEvent::Cancel,
            Keycode::Left => {
                let to = if ctrl { self.word_left(self.cursor) } else { self.cursor.saturating_sub(1) };
                if !shift && self.selection().is_some() {
                    let (s, _) = self.selection().unwrap_or((to, to));
                    self.move_to(s, false);
                } else {
                    self.move_to(to, shift);
                }
                return FieldEvent::None;
            }
            Keycode::Right => {
                let to = if ctrl { self.word_right(self.cursor) } else { self.cursor + 1 };
                if !shift && self.selection().is_some() {
                    let (_, e) = self.selection().unwrap_or((to, to));
                    self.move_to(e, false);
                } else {
                    self.move_to(to, shift);
                }
                return FieldEvent::None;
            }
            Keycode::Home => {
                self.move_to(0, shift);
                return FieldEvent::None;
            }
            Keycode::End => {
                let n = self.chars.len();
                self.move_to(n, shift);
                return FieldEvent::None;
            }
            Keycode::Backspace => {
                if self.delete_selection() {
                    return FieldEvent::Changed;
                }
                if self.cursor == 0 {
                    return FieldEvent::None;
                }
                let from = if ctrl { self.word_left(self.cursor) } else { self.cursor - 1 };
                self.chars.drain(from..self.cursor);
                self.cursor = from;
                return FieldEvent::Changed;
            }
            Keycode::Delete => {
                if self.delete_selection() {
                    return FieldEvent::Changed;
                }
                if self.cursor >= self.chars.len() {
                    return FieldEvent::None;
                }
                let to = if ctrl { self.word_right(self.cursor) } else { self.cursor + 1 };
                self.chars.drain(self.cursor..to);
                return FieldEvent::Changed;
            }
            Keycode::A if ctrl => {
                self.select_all();
                return FieldEvent::None;
            }
            _ => {}
        }
        match key_char(ev) {
            Some(ch) => {
                let mut b = [0u8; 4];
                self.insert(ch.encode_utf8(&mut b));
                FieldEvent::Changed
            }
            None => FieldEvent::Ignored,
        }
    }

    /// Place le curseur au clic (abscisse dans le champ dessiné en `r`).
    pub fn click(&mut self, r: Rect, px: isize) {
        let rel = (px - r.x - 8).max(0) / CHAR_W;
        let pos = self.scroll + rel as usize;
        self.move_to(pos, false);
    }

    /// Dessine le champ ; `icon` réserve une place à gauche (loupe…).
    pub fn draw(&mut self, c: &mut Canvas, r: Rect, focused: bool, placeholder: &str, left_pad: isize) {
        let bg = if focused { pal::SURFACE0 } else { pal::SURFACE1 };
        c.round_rect(r, 7, bg);
        c.round_stroke(r, 7, if focused { pal::ACCENT } else { pal::BORDER_STRONG }, if focused { 255 } else { 160 });
        let tx = r.x + 8 + left_pad;
        let ty = r.y + (r.h - CHAR_H) / 2;
        let cols = ((r.w - 16 - left_pad) / CHAR_W).max(1) as usize;
        // Défilement : le curseur reste visible.
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        }
        if self.cursor > self.scroll + cols {
            self.scroll = self.cursor - cols;
        }
        if self.chars.is_empty() && !placeholder.is_empty() {
            c.text_fit(tx, ty, r.w - 16 - left_pad, placeholder, pal::TEXT_FAINT);
        }
        let saved = c.push_clip(Rect::new(tx, r.y, r.w - 16 - left_pad, r.h));
        if let Some((s, e)) = self.selection() {
            let s = s.max(self.scroll);
            if e > s {
                let x0 = tx + (s - self.scroll) as isize * CHAR_W;
                let w = (e - s) as isize * CHAR_W;
                c.fill(Rect::new(x0, ty, w, CHAR_H), pal::ACCENT_SOFT);
            }
        }
        let visible: String = self.chars.iter().skip(self.scroll).take(cols + 1).collect();
        c.text(tx, ty, &visible, pal::TEXT);
        if focused {
            let cx = tx + (self.cursor - self.scroll) as isize * CHAR_W;
            c.fill(Rect::new(cx, ty - 1, 2, CHAR_H + 2), pal::ACCENT);
        }
        c.set_clip(saved);
    }
}

// ================================================================
// TABLEAUX
// ================================================================

/// Alignement d'une colonne.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

/// Colonne de tableau : titre, largeur minimale, part de l'espace restant, alignement.
#[derive(Clone, Copy)]
pub struct Column {
    pub title: &'static str,
    pub min: isize,
    pub flex: isize,
    pub align: Align,
}

impl Column {
    pub const fn fixed(title: &'static str, w: isize, align: Align) -> Column { Column { title, min: w, flex: 0, align } }
    pub const fn flex(title: &'static str, min: isize, flex: isize) -> Column { Column { title, min, flex, align: Align::Left } }
}

/// Répartit `width` entre les colonnes : minimum garanti, reste au prorata de `flex`.
pub fn layout_columns(x: isize, width: isize, cols: &[Column]) -> Vec<Rect> {
    let min: isize = cols.iter().map(|c| c.min).sum();
    let flex: isize = cols.iter().map(|c| c.flex).sum();
    let extra = (width - min).max(0);
    let mut out = Vec::with_capacity(cols.len());
    let mut cx = x;
    for (i, col) in cols.iter().enumerate() {
        let mut w = col.min + if flex > 0 { extra * col.flex / flex } else { 0 };
        if i + 1 == cols.len() {
            w = (x + width - cx).max(col.min.min(width));
        }
        out.push(Rect::new(cx, 0, w, 0));
        cx += w;
    }
    out
}

/// En-tête de tableau, avec la colonne de tri et son sens ; renvoie les zones des colonnes.
pub fn table_header(c: &mut Canvas, r: Rect, cols: &[Column], sort: Option<(usize, bool)>, hover: Option<usize>) -> Vec<Rect> {
    c.fill(r, pal::SURFACE1);
    c.hline(r.x, r.right(), r.bottom() - 1, pal::BORDER);
    let mut out = layout_columns(r.x, r.w, cols);
    for (i, (col, cr)) in cols.iter().zip(out.iter_mut()).enumerate() {
        cr.y = r.y;
        cr.h = r.h;
        if hover == Some(i) {
            c.fill(Rect::new(cr.x, cr.y, cr.w, cr.h - 1), pal::SURFACE2);
        }
        let sorted = sort.map_or(false, |(s, _)| s == i);
        let fg = if sorted { pal::TEXT } else { pal::TEXT_DIM };
        let arrow = match sort {
            Some((s, true)) if s == i => " ▲",
            Some((s, false)) if s == i => " ▼",
            _ => "",
        };
        let mut label = String::from(col.title);
        label.push_str(arrow);
        match col.align {
            Align::Left => c.text_in(*cr, 10, &label, fg),
            Align::Right => c.text_in_right(*cr, 10, &label, fg),
        };
        if i > 0 {
            c.vline(cr.x, cr.y + 6, cr.bottom() - 6, pal::BORDER);
        }
    }
    out
}

/// Texte d'une cellule selon l'alignement de sa colonne.
pub fn cell(c: &mut Canvas, r: Rect, align: Align, s: &str, fg: Rgb) {
    match align {
        Align::Left => c.text_in(r, 10, s, fg),
        Align::Right => c.text_in_right(r, 10, s, fg),
    };
}

// ================================================================
// DÉFILEMENT
// ================================================================

/// Défilement vertical d'une liste de lignes de hauteur fixe.
#[derive(Clone, Copy, Default, Debug)]
pub struct Scroll {
    /// Première ligne visible.
    pub offset: usize,
    /// Glisser du curseur de la barre : décalage du pointeur dans le curseur.
    pub grab: Option<isize>,
}

impl Scroll {
    /// Garde `sel` visible dans une fenêtre de `visible` lignes sur `total`.
    pub fn follow(&mut self, sel: usize, visible: usize, total: usize) {
        if visible == 0 {
            return;
        }
        if sel < self.offset {
            self.offset = sel;
        } else if sel >= self.offset + visible {
            self.offset = sel + 1 - visible;
        }
        self.clamp(visible, total);
    }

    pub fn clamp(&mut self, visible: usize, total: usize) {
        self.offset = self.offset.min(total.saturating_sub(visible));
    }

    /// Molette : 3 lignes par cran.
    pub fn wheel(&mut self, delta: i32, visible: usize, total: usize) {
        if delta > 0 {
            self.offset = self.offset.saturating_sub(3 * delta as usize);
        } else {
            self.offset += 3 * (-delta) as usize;
        }
        self.clamp(visible, total);
    }

    /// Curseur de la barre dessinée dans `track`.
    pub fn thumb(&self, track: Rect, visible: usize, total: usize) -> Option<Rect> {
        if total <= visible || visible == 0 {
            return None;
        }
        let th = ((track.h as i64 * visible as i64 / total as i64) as isize).max(24).min(track.h);
        let range = (track.h - th).max(1);
        let max_off = (total - visible) as i64;
        let ty = track.y + (range as i64 * self.offset as i64 / max_off) as isize;
        Some(Rect::new(track.x, ty, track.w, th))
    }

    /// Barre de défilement fine (s'épaissit au survol).
    pub fn draw(&self, c: &mut Canvas, track: Rect, visible: usize, total: usize, hover: bool) {
        if let Some(t) = self.thumb(track, visible, total) {
            let w = if hover || self.grab.is_some() { track.w } else { track.w / 2 + 1 };
            let tr = Rect::new(track.right() - w - 1, t.y + 2, w, t.h - 4);
            c.round_rect_alpha(tr, w / 2, pal::TEXT_DIM, if hover || self.grab.is_some() { 200 } else { 110 });
        }
    }

    /// Commence à glisser si `py` est sur le curseur, ou saute à la page cliquée.
    pub fn press(&mut self, track: Rect, py: isize, visible: usize, total: usize) {
        if let Some(t) = self.thumb(track, visible, total) {
            if py >= t.y && py < t.bottom() {
                self.grab = Some(py - t.y);
            } else if py < t.y {
                self.offset = self.offset.saturating_sub(visible);
            } else {
                self.offset += visible;
                self.clamp(visible, total);
            }
        }
    }

    /// Suit le pointeur pendant le glisser.
    pub fn drag(&mut self, track: Rect, py: isize, visible: usize, total: usize) {
        if let (Some(g), Some(t)) = (self.grab, self.thumb(track, visible, total)) {
            let range = (track.h - t.h).max(1) as i64;
            let pos = (py - g - track.y).clamp(0, range as isize) as i64;
            self.offset = (pos * (total - visible) as i64 / range) as usize;
            self.clamp(visible, total);
        }
    }
}

// ================================================================
// MENUS CONTEXTUELS
// ================================================================

/// Entrée de menu.
#[derive(Clone)]
pub struct MenuItem {
    pub label: String,
    pub shortcut: &'static str,
    pub enabled: bool,
    pub danger: bool,
    /// Identifiant renvoyé à l'application.
    pub id: u32,
}

impl MenuItem {
    pub fn new(id: u32, label: &str, shortcut: &'static str) -> MenuItem {
        MenuItem { label: String::from(label), shortcut, enabled: true, danger: false, id }
    }
    pub fn enabled(mut self, e: bool) -> MenuItem { self.enabled = e; self }
    pub fn danger(mut self) -> MenuItem { self.danger = true; self }
    /// Séparateur.
    pub fn separator() -> MenuItem { MenuItem { label: String::new(), shortcut: "", enabled: false, danger: false, id: u32::MAX } }
    fn is_separator(&self) -> bool { self.id == u32::MAX }
}

const MENU_ROW: isize = 26;
const MENU_SEP: isize = 9;

/// Menu contextuel ouvert.
#[derive(Clone)]
pub struct Menu {
    pub items: Vec<MenuItem>,
    pub x: isize,
    pub y: isize,
    pub hover: Option<usize>,
}

impl Menu {
    /// Menu en `(x, y)`, déplacé pour rester dans `bounds`.
    pub fn new(items: Vec<MenuItem>, x: isize, y: isize, bounds: Rect) -> Menu {
        let mut m = Menu { items, x, y, hover: None };
        let r = m.rect();
        if r.right() > bounds.right() {
            m.x = (bounds.right() - r.w - 2).max(bounds.x);
        }
        if r.bottom() > bounds.bottom() {
            m.y = (y - r.h).max(bounds.y);
        }
        m
    }

    pub fn rect(&self) -> Rect {
        let w = self
            .items
            .iter()
            .map(|i| text_width(&i.label) + if i.shortcut.is_empty() { 0 } else { text_width(i.shortcut) + 28 })
            .max()
            .unwrap_or(80)
            + 44;
        let h: isize = self.items.iter().map(|i| if i.is_separator() { MENU_SEP } else { MENU_ROW }).sum::<isize>() + 8;
        Rect::new(self.x, self.y, w.max(160), h)
    }

    /// Entrée sous le pointeur.
    pub fn item_at(&self, px: isize, py: isize) -> Option<usize> {
        let r = self.rect();
        if !r.contains(px, py) {
            return None;
        }
        let mut y = r.y + 4;
        for (i, it) in self.items.iter().enumerate() {
            let h = if it.is_separator() { MENU_SEP } else { MENU_ROW };
            if py >= y && py < y + h {
                return (!it.is_separator() && it.enabled).then_some(i);
            }
            y += h;
        }
        None
    }

    pub fn draw(&self, c: &mut Canvas) {
        let r = self.rect();
        c.shadow(r, 8, 8, 120);
        c.round_rect(r, 8, pal::SURFACE2);
        c.round_stroke(r, 8, pal::BORDER_STRONG, 255);
        let mut y = r.y + 4;
        for (i, it) in self.items.iter().enumerate() {
            if it.is_separator() {
                c.hline(r.x + 10, r.right() - 10, y + MENU_SEP / 2, pal::BORDER_STRONG);
                y += MENU_SEP;
                continue;
            }
            let row = Rect::new(r.x + 4, y, r.w - 8, MENU_ROW);
            if self.hover == Some(i) && it.enabled {
                c.round_rect(row, 5, if it.danger { pal::RED.darken(150) } else { pal::ACCENT_SOFT });
            }
            let fg = if !it.enabled {
                pal::TEXT_FAINT
            } else if it.danger {
                pal::RED
            } else {
                pal::TEXT
            };
            c.text_in(row, 14, &it.label, fg);
            if !it.shortcut.is_empty() {
                c.text_in_right(row, 12, it.shortcut, pal::TEXT_DIM);
            }
            y += MENU_ROW;
        }
    }
}

// ================================================================
// BOÎTES DE DIALOGUE
// ================================================================

/// Boîte de dialogue modale : titre, message, champ facultatif, boutons.
pub struct Dialog {
    pub title: String,
    pub message: Vec<String>,
    /// Champ de saisie (nom d'un nouveau dossier, renommage…).
    pub field: Option<TextField>,
    /// Libellés des boutons ; le dernier est l'action principale.
    pub buttons: Vec<&'static str>,
    /// Le bouton principal est destructeur.
    pub danger: bool,
    pub hover: Option<usize>,
    /// Identifiant de l'action, interprété par l'application.
    pub tag: u32,
    /// Message d'erreur affiché sous le champ.
    pub error: Option<String>,
}

/// Réponse d'une boîte de dialogue.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DialogResult {
    /// Rien de décidé.
    Open,
    /// Bouton choisi (index dans `buttons`).
    Button(usize),
    /// Fermée sans choix (Échap).
    Dismissed,
}

impl Dialog {
    pub fn confirm(tag: u32, title: &str, message: &[&str], yes: &'static str, danger: bool) -> Dialog {
        Dialog {
            title: String::from(title),
            message: message.iter().map(|s| String::from(*s)).collect(),
            field: None,
            buttons: alloc::vec!["Annuler", yes],
            danger,
            hover: None,
            tag,
            error: None,
        }
    }

    pub fn prompt(tag: u32, title: &str, message: &str, initial: &str, ok: &'static str) -> Dialog {
        let mut f = TextField::with_text(initial);
        f.select_stem();
        Dialog {
            title: String::from(title),
            message: if message.is_empty() { Vec::new() } else { alloc::vec![String::from(message)] },
            field: Some(f),
            buttons: alloc::vec!["Annuler", ok],
            danger: false,
            hover: None,
            tag,
            error: None,
        }
    }

    pub fn info(tag: u32, title: &str, lines: Vec<String>) -> Dialog {
        Dialog { title: String::from(title), message: lines, field: None, buttons: alloc::vec!["Fermer"], danger: false, hover: None, tag, error: None }
    }

    fn rect(&self, bounds: Rect) -> Rect {
        let longest = self.message.iter().map(|m| text_width(m)).max().unwrap_or(0).max(text_width(&self.title));
        let w = (longest + 48).clamp(360, (bounds.w - 40).max(200));
        let h = 64 + self.message.len() as isize * (CHAR_H + 4)
            + if self.field.is_some() { 44 } else { 0 }
            + if self.error.is_some() { CHAR_H + 6 } else { 0 }
            + 52;
        bounds.center(w, h)
    }

    fn button_rects(&self, r: Rect) -> Vec<Rect> {
        let mut out = Vec::new();
        let mut x = r.right() - 16;
        for b in self.buttons.iter().rev() {
            let w = button_width(b).max(96);
            x -= w;
            out.push(Rect::new(x, r.bottom() - 46, w, 30));
            x -= 10;
        }
        out.reverse();
        out
    }

    /// Dessine la boîte (et un voile sur `bounds`).
    pub fn draw(&mut self, c: &mut Canvas, bounds: Rect) {
        c.fill_alpha(bounds, rgb(0x000000), 120);
        let r = self.rect(bounds);
        c.shadow(r, 12, 14, 160);
        c.round_rect(r, 12, pal::SURFACE2);
        c.round_stroke(r, 12, pal::BORDER_STRONG, 255);
        c.text_bold(r.x + 24, r.y + 22, &self.title, pal::TEXT);
        let mut y = r.y + 52;
        for m in &self.message {
            c.text_fit(r.x + 24, y, r.w - 48, m, pal::TEXT_SOFT);
            y += CHAR_H + 4;
        }
        if let Some(f) = self.field.as_mut() {
            f.draw(c, Rect::new(r.x + 24, y + 4, r.w - 48, 30), true, "", 0);
            y += 44;
        }
        if let Some(e) = &self.error {
            c.text_fit(r.x + 24, y, r.w - 48, e, pal::RED);
        }
        let n = self.buttons.len();
        for (i, (b, br)) in self.buttons.iter().zip(self.button_rects(r)).enumerate() {
            let kind = if i + 1 == n {
                if self.danger { ButtonKind::Danger } else { ButtonKind::Primary }
            } else {
                ButtonKind::Normal
            };
            button(c, br, b, kind, self.hover == Some(i), true);
        }
    }

    /// Survol du pointeur.
    pub fn pointer_move(&mut self, bounds: Rect, px: isize, py: isize) {
        let r = self.rect(bounds);
        self.hover = self.button_rects(r).iter().position(|b| b.contains(px, py));
    }

    /// Clic : bouton choisi, ou placement du curseur dans le champ.
    pub fn click(&mut self, bounds: Rect, px: isize, py: isize) -> DialogResult {
        let r = self.rect(bounds);
        if let Some(i) = self.button_rects(r).iter().position(|b| b.contains(px, py)) {
            return DialogResult::Button(i);
        }
        if let Some(f) = self.field.as_mut() {
            let fy = r.y + 52 + self.message.len() as isize * (CHAR_H + 4) + 4;
            let fr = Rect::new(r.x + 24, fy, r.w - 48, 30);
            if fr.contains(px, py) {
                f.click(fr, px);
            }
        }
        DialogResult::Open
    }

    /// Clavier : Entrée = bouton principal, Échap = fermer.
    pub fn on_key(&mut self, ev: &KeyEvent) -> DialogResult {
        if let Some(f) = self.field.as_mut() {
            match f.on_key(ev) {
                FieldEvent::Submit => return DialogResult::Button(self.buttons.len() - 1),
                FieldEvent::Cancel => return DialogResult::Dismissed,
                FieldEvent::Changed => self.error = None,
                _ => {}
            }
            return DialogResult::Open;
        }
        match ev.keycode {
            Keycode::Enter => DialogResult::Button(self.buttons.len() - 1),
            Keycode::Escape => DialogResult::Dismissed,
            _ => DialogResult::Open,
        }
    }

    /// Texte du champ.
    pub fn value(&self) -> String { self.field.as_ref().map(|f| f.text()).unwrap_or_default() }
}

// ================================================================
// NOTIFICATIONS
// ================================================================

/// Gravité d'une notification.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Error,
}

/// Notification éphémère en bas de la fenêtre.
pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    /// Instant d'expiration (ns, horloge de l'ordonnanceur).
    pub until_ns: u64,
}

impl Toast {
    pub fn new(text: String, kind: ToastKind, now_ns: u64) -> Toast {
        let dur = if kind == ToastKind::Error { 5_000_000_000 } else { 3_000_000_000 };
        Toast { text, kind, until_ns: now_ns + dur }
    }

    pub fn draw(&self, c: &mut Canvas, bounds: Rect, now_ns: u64) {
        if now_ns >= self.until_ns {
            return;
        }
        // Fondu pendant les 400 dernières millisecondes.
        let left = self.until_ns - now_ns;
        let a = if left < 400_000_000 { (left * 255 / 400_000_000) as u32 } else { 255 };
        let w = (text_width(&self.text) + 52).min(bounds.w - 40);
        let r = Rect::new(bounds.x + (bounds.w - w) / 2, bounds.bottom() - 56, w, 34);
        let col = match self.kind {
            ToastKind::Info => pal::ACCENT,
            ToastKind::Success => pal::GREEN,
            ToastKind::Error => pal::RED,
        };
        c.shadow(r, 10, 6, a / 2);
        c.round_rect_alpha(r, 10, pal::SURFACE3, a);
        c.round_rect_alpha(Rect::new(r.x + 12, r.y + 12, 10, 10), 5, col, a);
        c.text_alpha(r.x + 32, r.y + (r.h - CHAR_H) / 2, &self.text, pal::TEXT, a);
    }

    pub fn alive(&self, now_ns: u64) -> bool { now_ns < self.until_ns }
}

// ================================================================
// HISTORIQUES DE MESURES
// ================================================================

/// Historique circulaire de `N` mesures (graphes de performance).
#[derive(Clone)]
pub struct History<const N: usize> {
    buf: [u32; N],
    len: usize,
    head: usize,
}

impl<const N: usize> Default for History<N> {
    fn default() -> Self { History { buf: [0; N], len: 0, head: 0 } }
}

impl<const N: usize> History<N> {
    pub fn push(&mut self, v: u32) {
        self.buf[self.head] = v;
        self.head = (self.head + 1) % N;
        self.len = (self.len + 1).min(N);
    }

    /// Dernière valeur.
    pub fn last(&self) -> u32 {
        if self.len == 0 { 0 } else { self.buf[(self.head + N - 1) % N] }
    }

    /// Valeurs du plus ancien au plus récent, complétées à gauche par des zéros (largeur fixe).
    pub fn padded(&self) -> [u32; N] {
        let mut out = [0u32; N];
        for i in 0..self.len {
            out[N - self.len + i] = self.buf[(self.head + N - self.len + i) % N];
        }
        out
    }

    pub fn max(&self) -> u32 { self.buf.iter().take(self.len.max(1)).copied().max().unwrap_or(0) }
}

/// Grille de fond d'un graphe (lignes horizontales et verticales discrètes).
pub fn chart_grid(c: &mut Canvas, r: Rect, rows: isize, cols: isize) {
    c.fill(r, pal::SURFACE0);
    for i in 1..rows {
        let y = r.y + r.h * i / rows;
        c.fill_alpha(Rect::new(r.x, y, r.w, 1), pal::BORDER_STRONG, 90);
    }
    for i in 1..cols {
        let x = r.x + r.w * i / cols;
        c.fill_alpha(Rect::new(x, r.y, 1, r.h), pal::BORDER_STRONG, 60);
    }
    c.stroke(r, pal::BORDER);
}

// ================================================================
// FORMATAGE
// ================================================================

/// Taille lisible en unités binaires (« 1,5 Mio »).
pub fn human_bytes(n: u64) -> String {
    const U: [&str; 6] = ["o", "Kio", "Mio", "Gio", "Tio", "Pio"];
    if n < 1024 {
        return alloc::format!("{} o", n);
    }
    let mut v = n as u128 * 10;
    let mut u = 0;
    while v >= 10240 && u < U.len() - 1 {
        v /= 1024;
        u += 1;
    }
    // `v` en dixièmes de l'unité `u`.
    let (int, frac) = (v / 10, v % 10);
    if int >= 100 || frac == 0 {
        alloc::format!("{} {}", int, U[u])
    } else {
        alloc::format!("{},{} {}", int, frac, U[u])
    }
}

/// Durée lisible depuis des nanosecondes (« 2 h 05 min », « 3,2 s »).
pub fn human_duration(ns: u64) -> String {
    let s = ns / 1_000_000_000;
    if s >= 86_400 {
        alloc::format!("{} j {} h", s / 86_400, (s % 86_400) / 3600)
    } else if s >= 3600 {
        alloc::format!("{} h {:02} min", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        alloc::format!("{} min {:02} s", s / 60, s % 60)
    } else if ns >= 1_000_000_000 {
        alloc::format!("{},{} s", s, (ns % 1_000_000_000) / 100_000_000)
    } else if ns >= 1_000_000 {
        alloc::format!("{} ms", ns / 1_000_000)
    } else if ns >= 1_000 {
        alloc::format!("{} µs", ns / 1_000)
    } else {
        alloc::format!("{} ns", ns)
    }
}

/// Pourcentage en dixièmes (‰ → « 12,5 % »).
pub fn permille_str(pm: u32) -> String {
    if pm >= 1000 {
        return String::from("100 %");
    }
    alloc::format!("{},{} %", pm / 10, pm % 10)
}

/// Nombre avec séparateur de milliers (espace fine remplacée par une espace).
pub fn thousands(n: u64) -> String {
    let s = alloc::format!("{}", n);
    let mut out = String::new();
    let len = s.len();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}
