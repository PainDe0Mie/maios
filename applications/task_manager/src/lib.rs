//! Task Manager pour mai_os
//!
//! Affiche en temps réel :
//!   - RAM : libre / utilisée / totale (frames × 4 KiB)
//!   - CPU : tâches actives / running / total
//!   - Liste paginée de toutes les tâches avec état, CPU, flags
//!
//! Contrôles :
//!   ↑ / ↓        — navigation
//!   K            — kill la tâche sélectionnée
//!   R / F5       — forcer un refresh immédiat
//!   Q / Échap    — quitter

#![no_std]
extern crate alloc;
extern crate color;
extern crate font;
extern crate framebuffer;
extern crate framebuffer_drawer;
extern crate frame_allocator;
extern crate shapes;
extern crate task;
extern crate task_struct;
extern crate window;
extern crate window_manager;
extern crate scheduler;
extern crate event_types;
extern crate keycodes_ascii;
extern crate cpu;

#[macro_use] extern crate log;

use alloc::vec::Vec;
use alloc::string::{String, ToString};
use alloc::format;
use color::Color;
use shapes::Coord;
use framebuffer::{Framebuffer, AlphaPixel};
use task_struct::RunState;
use event_types::Event;
use keycodes_ascii::{KeyAction, Keycode};

// ────────────────────────────────────────────────────────────────
// Palette Tokyo Night
// ────────────────────────────────────────────────────────────────
const C_BG:       Color = Color::new(0x001A1B26);
const C_SURFACE:  Color = Color::new(0x0024283A);
const C_SURFACE2: Color = Color::new(0x00292E42);
const C_BORDER:   Color = Color::new(0x00414868);
const C_ACCENT:   Color = Color::new(0x007AA2F7);
const C_GREEN:    Color = Color::new(0x009ECE6A);
const C_YELLOW:   Color = Color::new(0x00E0AF68);
const C_RED:      Color = Color::new(0x00F7768E);
const C_PURPLE:   Color = Color::new(0x00BB9AF7);
const C_CYAN:     Color = Color::new(0x007DCFFF);
const C_FG:       Color = Color::new(0x00C0CAF5);
const C_FG_DIM:   Color = Color::new(0x00565F89);
const C_ROW_ALT:  Color = Color::new(0x001E2030);
const C_SEL:      Color = Color::new(0x002D3F6B);
const C_SEL_BAR:  Color = Color::new(0x007AA2F7);

// ────────────────────────────────────────────────────────────────
// Layout
// ────────────────────────────────────────────────────────────────
const WIN_W: usize = 800;
const WIN_H: usize = 540;

const TITLE_H:   usize = 28;
const STATS_H:   usize = 98;
const HEADER_H:  usize = 20;
const FOOTER_H:  usize = 20;
const ROW_H:     usize = 18;
const PAD:       isize = 12;

/// Y de début de la liste (relatif au contenu de la fenêtre)
fn list_top() -> usize { TITLE_H + STATS_H + HEADER_H }

/// Nombre de lignes visibles dans content_h pixels
fn visible_rows(ch: usize) -> usize {
    ch.saturating_sub(list_top() + FOOTER_H) / ROW_H
}

// Colonnes (relatifs au bord gauche du contenu)
const COL_ID:    isize = PAD;
const COL_NAME:  isize = PAD + 48;
const COL_STATE: isize = PAD + 280;
const COL_CPU:   isize = PAD + 390;
const COL_FLAGS: isize = PAD + 450;
const COL_IDLE:  isize = PAD + 560;

// ────────────────────────────────────────────────────────────────
// Dessin de base
// ────────────────────────────────────────────────────────────────
type Fb = Framebuffer<AlphaPixel>;

#[inline]
fn fill(fb: &mut Fb, x: isize, y: isize, w: usize, h: usize, c: Color) {
    if w == 0 || h == 0 { return; }
    framebuffer_drawer::fill_rectangle(fb, Coord::new(x, y), w, h, c.into());
}

#[inline]
fn hline(fb: &mut Fb, x0: isize, x1: isize, y: isize, c: Color) {
    if x1 <= x0 { return; }
    framebuffer_drawer::fill_rectangle(fb, Coord::new(x0, y), (x1 - x0) as usize, 1, c.into());
}

fn draw_char(fb: &mut Fb, x: isize, y: isize, ch: char, c: Color) {
    let idx = ch as usize;
    if idx >= 256 { return; }
    let bm = &font::FONT_BASIC[idx];
    for row in 0..font::CHARACTER_HEIGHT {
        let bits = bm[row];
        for col in 0..8usize {
            if bits & (0x80 >> col) != 0 {
                fb.draw_pixel(Coord::new(x + col as isize, y + row as isize), c.into());
            }
        }
    }
}

fn draw_text(fb: &mut Fb, x: isize, y: isize, s: &str, c: Color) {
    let mut cx = x;
    for ch in s.chars() {
        draw_char(fb, cx, y, ch, c);
        cx += font::CHARACTER_WIDTH as isize;
    }
}

fn draw_text_clipped(fb: &mut Fb, x: isize, y: isize, s: &str, max_px: isize, c: Color) {
    let fw = font::CHARACTER_WIDTH as isize;
    if max_px < fw { return; }
    let max_ch = (max_px / fw) as usize;
    let t: String = s.chars().take(max_ch).collect();
    draw_text(fb, x, y, &t, c);
}

/// Barre de progression (0..=100) avec fond, remplissage et bordure.
fn draw_bar(fb: &mut Fb, x: isize, y: isize, w: usize, h: usize, pct: usize, fg: Color) {
    fill(fb, x, y, w, h, Color::new(0x00111118));
    let filled = (w * pct.min(100)) / 100;
    if filled > 0 { fill(fb, x, y, filled, h, fg); }
    // Bordure fine
    hline(fb, x, x + w as isize, y, C_BORDER);
    hline(fb, x, x + w as isize, y + h as isize - 1, C_BORDER);
    framebuffer_drawer::fill_rectangle(fb, Coord::new(x, y), 1, h, C_BORDER.into());
    framebuffer_drawer::fill_rectangle(fb, Coord::new(x + w as isize - 1, y), 1, h, C_BORDER.into());
}

// ────────────────────────────────────────────────────────────────
// Données système
// ────────────────────────────────────────────────────────────────

/// Nombre de frames physiques libres.
fn free_frames() -> usize {
    let mut total = 0usize;
    let _ = frame_allocator::inspect_then_allocate_free_frames(&mut |chunk| {
        total += chunk.size_in_frames();
        frame_allocator::FramesIteratorRequest::Next
    });
    total
}

// ────────────────────────────────────────────────────────────────
// Infos tâche
// ────────────────────────────────────────────────────────────────

struct TaskInfo {
    id:      usize,
    name:    String,
    state:   RunState,
    on_cpu:  Option<u32>,
    is_idle: bool,
    pinned:  bool,
}

fn collect_tasks() -> Vec<TaskInfo> {
    let mut v: Vec<TaskInfo> = task::all_tasks()
        .into_iter()
        .map(|(id, tr)| TaskInfo {
            id,
            name:    tr.name.clone(),
            state:   tr.runstate.load(),
            on_cpu:  Option::<cpu::CpuId>::from(tr.running_on_cpu.load()).map(|c| c.value()),
            is_idle: tr.is_an_idle_task,
            pinned:  Option::<cpu::CpuId>::from(tr.pinned_core.load()).is_some(),
        })
        .collect();
    // Tri : running d'abord, puis runnable, puis le reste, par id croissant
    v.sort_by(|a, b| {
        let rank = |t: &TaskInfo| match t.state {
            RunState::Runnable if t.on_cpu.is_some() => 0,
            RunState::Runnable                        => 1,
            RunState::Blocked                         => 2,
            _                                         => 3,
        };
        rank(a).cmp(&rank(b)).then(a.id.cmp(&b.id))
    });
    v
}

fn state_label(s: &RunState) -> (&'static str, Color) {
    match s {
        RunState::Runnable => ("RUNNING ", C_GREEN),
        RunState::Blocked  => ("BLOCKED ", C_YELLOW),
        RunState::Exited(_)=> ("EXITED  ", C_RED),
        _                  => ("UNKNOWN ", C_FG_DIM),
    }
}

// ────────────────────────────────────────────────────────────────
// État de l'application
// ────────────────────────────────────────────────────────────────

struct TmState {
    tasks:          Vec<TaskInfo>,
    free_frames:    usize,
    /// Estimation du total physique : fixé au premier refresh.
    total_frames:   usize,
    scroll:         usize,
    selected:       usize,
    dirty:          bool,
    refresh_ticks:  usize,
    /// Dimensions du contenu de la fenêtre
    off_x: isize, off_y: isize,
    cw: usize,    ch: usize,
}

impl TmState {
    fn new(win: &window::Window) -> Self {
        let area = win.area();
        let off_x = area.top_left.x;
        let off_y = area.top_left.y;
        let cw = (area.bottom_right.x - area.top_left.x) as usize;
        let ch = (area.bottom_right.y - area.top_left.y) as usize;

        let ff = free_frames();
        // Heuristique initiale : total ≈ 2× le libre au démarrage
        // (la majeure partie de la RAM est allouée dès le boot kernel)
        // Une vraie valeur nécessiterait un compteur global au boot.
        let total = ff.saturating_mul(2).max(ff + 16384); // min 16384 frames = 64 MiB

        TmState {
            tasks: collect_tasks(),
            free_frames: ff,
            total_frames: total,
            scroll: 0, selected: 0,
            dirty: true, refresh_ticks: 0,
            off_x, off_y, cw, ch,
        }
    }

    fn vis(&self) -> usize { visible_rows(self.ch) }

    fn clamp_scroll(&mut self) {
        let vis = self.vis();
        let len = self.tasks.len();
        if self.selected < self.scroll { self.scroll = self.selected; }
        if vis > 0 && self.selected >= self.scroll + vis {
            self.scroll = self.selected - vis + 1;
        }
        let max_s = len.saturating_sub(vis);
        if self.scroll > max_s { self.scroll = max_s; }
    }

    fn refresh(&mut self) {
        self.tasks       = collect_tasks();
        self.free_frames = free_frames();
        self.clamp_scroll();
        self.dirty = true;
    }
}

// ────────────────────────────────────────────────────────────────
// Rendu
// ────────────────────────────────────────────────────────────────

fn redraw(fb: &mut Fb, s: &TmState) {
    let ox = s.off_x;
    let oy = s.off_y;
    let cw = s.cw;
    let ch = s.ch;
    let w  = cw as isize;
    let fw = font::CHARACTER_WIDTH as isize;
    let fh = font::CHARACTER_HEIGHT as isize;

    // ── Fond ────────────────────────────────────────────────────
    fill(fb, ox, oy, cw, ch, C_BG);

    // ── Titre ───────────────────────────────────────────────────
    fill(fb, ox, oy, cw, TITLE_H, C_SURFACE);
    hline(fb, ox, ox + w, oy + TITLE_H as isize, C_ACCENT);
    let title_y = oy + (TITLE_H as isize - fh) / 2;
    draw_text(fb, ox + PAD, title_y, "Mai Task Manager", C_ACCENT);
    let task_count_str = format!("{} tasks", s.tasks.len());
    draw_text(fb, ox + w - (task_count_str.len() as isize * fw) - PAD, title_y,
              &task_count_str, C_FG_DIM);

    // ── Section stats ────────────────────────────────────────────
    let sy = oy + TITLE_H as isize;
    fill(fb, ox, sy, cw, STATS_H, C_SURFACE);
    hline(fb, ox, ox + w, sy + STATS_H as isize - 1, C_BORDER);

    let text_y = sy + 8;
    let bar_y  = text_y + fh + 4;

    // — RAM —
    let bar_w: usize = (cw / 2).saturating_sub(24);
    let free_mb  = (s.free_frames * 4096) / (1024 * 1024);
    let total_mb = (s.total_frames * 4096) / (1024 * 1024);
    let used_mb  = total_mb.saturating_sub(free_mb);
    let ram_pct  = if total_mb > 0 { (used_mb * 100) / total_mb } else { 0 };

    // Couleur de la barre RAM selon pression mémoire
    let ram_bar_color = if ram_pct > 85 { C_RED }
                        else if ram_pct > 65 { C_YELLOW }
                        else { C_ACCENT };

    draw_text(fb, ox + PAD, text_y, "RAM", C_ACCENT);
    draw_bar(fb, ox + PAD, bar_y, bar_w, 12, ram_pct, ram_bar_color);
    let ram_str = format!("{} / {} MiB  ({}% used)", used_mb, total_mb, ram_pct);
    draw_text(fb, ox + PAD, bar_y + 16, &ram_str, C_FG);

    // — CPU (ratio tâches runnable non-idle / total) —
    let cpu_x = ox + PAD + bar_w as isize + 20;
    let running = s.tasks.iter().filter(|t| t.on_cpu.is_some()).count();
    let runnable = s.tasks.iter()
        .filter(|t| matches!(t.state, RunState::Runnable) && !t.is_idle).count();
    let total = s.tasks.len().max(1);
    let cpu_pct = (runnable * 100) / total;
    let cpu_bar_color = if cpu_pct > 85 { C_RED }
                        else if cpu_pct > 60 { C_YELLOW }
                        else { C_GREEN };

    draw_text(fb, cpu_x, text_y, "CPU", C_ACCENT);
    draw_bar(fb, cpu_x, bar_y, bar_w, 12, cpu_pct, cpu_bar_color);
    let cpu_str = format!("{} running  {} runnable  {} total", running, runnable, total);
    draw_text(fb, cpu_x, bar_y + 16, &cpu_str, C_FG);

    // — Ligne inférieure stats —
    let info_y = bar_y + 34;
    let blocked = s.tasks.iter().filter(|t| matches!(t.state, RunState::Blocked)).count();
    let exited  = s.tasks.iter().filter(|t| matches!(t.state, RunState::Exited(_))).count();
    let info_str = format!("Blocked: {}   Exited: {}   Idle: {}",
        blocked, exited,
        s.tasks.iter().filter(|t| t.is_idle).count());
    draw_text(fb, ox + PAD, info_y, &info_str, C_FG_DIM);

    // ── Header colonnes ──────────────────────────────────────────
    let hdr_y = oy + (TITLE_H + STATS_H) as isize;
    fill(fb, ox, hdr_y, cw, HEADER_H, C_SURFACE2);
    hline(fb, ox, ox + w, hdr_y + HEADER_H as isize - 1, C_ACCENT);
    let ht = hdr_y + (HEADER_H as isize - fh) / 2;
    draw_text(fb, ox + COL_ID,    ht, "ID", C_FG_DIM);
    draw_text(fb, ox + COL_NAME,  ht, "Name", C_FG_DIM);
    draw_text(fb, ox + COL_STATE, ht, "State", C_FG_DIM);
    draw_text(fb, ox + COL_CPU,   ht, "CPU", C_FG_DIM);
    draw_text(fb, ox + COL_FLAGS, ht, "Flags", C_FG_DIM);

    // ── Liste des tâches ─────────────────────────────────────────
    let list_y0 = oy + (TITLE_H + STATS_H + HEADER_H) as isize;
    let vis      = s.vis();
    let footer_y = oy + ch as isize - FOOTER_H as isize;

    for (row, task) in s.tasks.iter().skip(s.scroll).take(vis).enumerate() {
        let abs_idx = s.scroll + row;
        let ry      = list_y0 + (row * ROW_H) as isize;
        let text_y  = ry + (ROW_H as isize - fh) / 2;

        // Fond : sélection > alterné
        let bg = if abs_idx == s.selected { C_SEL }
                 else if row % 2 == 0     { C_BG }
                 else                      { C_ROW_ALT };
        fill(fb, ox, ry, cw, ROW_H, bg);

        // Barre de sélection gauche
        if abs_idx == s.selected {
            fill(fb, ox, ry, 3, ROW_H, C_SEL_BAR);
        }

        let (state_s, state_c) = state_label(&task.state);
        let name_c = if task.is_idle { C_FG_DIM } else { C_FG };

        // ID
        draw_text(fb, ox + COL_ID, text_y, &format!("{:<4}", task.id), C_PURPLE);

        // Nom (tronqué)
        let max_name = COL_STATE - COL_NAME - 4;
        draw_text_clipped(fb, ox + COL_NAME, text_y, &task.name, max_name, name_c);

        // État
        draw_text(fb, ox + COL_STATE, text_y, state_s, state_c);

        // CPU
        let cpu_s = task.on_cpu.map(|c| format!("#{}", c)).unwrap_or_else(|| "-".into());
        draw_text(fb, ox + COL_CPU, text_y, &cpu_s, C_CYAN);

        // Flags
        let mut flags = String::new();
        if task.is_idle  { flags.push_str("IDLE "); }
        if task.pinned   { flags.push_str("PIN "); }
        if flags.is_empty() { flags.push('-'); }
        draw_text(fb, ox + COL_FLAGS, text_y, &flags, C_FG_DIM);

        // Séparateur
        hline(fb, ox + 3, ox + w, ry + ROW_H as isize - 1, Color::new(0x00202030));
    }

    // Zone vide sous les lignes
    let last_row_y = list_y0 + (vis * ROW_H) as isize;
    if last_row_y < footer_y {
        fill(fb, ox, last_row_y, cw, (footer_y - last_row_y) as usize, C_BG);
    }

    // ── Scrollbar ────────────────────────────────────────────────
    let total_tasks = s.tasks.len();
    let sb_area_h   = vis * ROW_H;
    let sb_x        = ox + w - 8;
    fill(fb, sb_x, list_y0, 6, sb_area_h, Color::new(0x00111118));
    if total_tasks > vis && vis > 0 {
        let thumb_h   = ((vis * sb_area_h) / total_tasks).max(16);
        let max_off   = total_tasks - vis;
        let thumb_y   = if max_off > 0 { (s.scroll * (sb_area_h - thumb_h)) / max_off } else { 0 };
        fill(fb, sb_x + 1, list_y0 + thumb_y as isize, 4, thumb_h, C_BORDER);
    }

    // ── Footer ───────────────────────────────────────────────────
    fill(fb, ox, footer_y, cw, FOOTER_H, C_SURFACE2);
    hline(fb, ox, ox + w, footer_y, C_BORDER);
    let ft_y = footer_y + (FOOTER_H as isize - fh) / 2;
    draw_text(fb, ox + PAD, ft_y,
              "UP/DOWN:nav  K:kill  R:refresh  Q:quit",
              C_FG_DIM);

    // Compteur sélection
    if total_tasks > 0 {
        let sel_str = format!("{}/{}", s.selected + 1, total_tasks);
        draw_text(fb, ox + w - (sel_str.len() as isize * fw) - PAD, ft_y, &sel_str, C_FG_DIM);
    }
}

// ────────────────────────────────────────────────────────────────
// Point d'entrée
// ────────────────────────────────────────────────────────────────

pub fn main(_args: Vec<String>) -> isize {
    info!("[task_manager] démarrage");

    let mut win = match window::Window::with_title(
        "Task Manager".to_string(),
        Coord::new(50, 40),
        WIN_W, WIN_H,
        C_BG,
    ) {
        Ok(w)  => w,
        Err(e) => { error!("[task_manager] window: {}", e); return -1; }
    };

    let mut state = TmState::new(&win);

    loop {
        state.refresh_ticks += 1;
        // Auto-refresh toutes les ~120 itérations (~2s)
        if state.refresh_ticks >= 120 {
            state.refresh_ticks = 0;
            state.refresh();
        }

        // ── Événements ──────────────────────────────────────────
        loop {
            match win.handle_event() {
                Ok(Some(Event::ExitEvent)) => return 0,

                Ok(Some(Event::KeyboardEvent(ke))) => {
                    if ke.key_event.action != KeyAction::Pressed { continue; }
                    let len = state.tasks.len();
                    let vis = state.vis();

                    match ke.key_event.keycode {
                        Keycode::Q | Keycode::Escape => return 0,

                        Keycode::R | Keycode::F5 => {
                            state.refresh_ticks = 120;
                        }

                        Keycode::Up => {
                            if state.selected > 0 { state.selected -= 1; }
                            state.clamp_scroll();
                            state.dirty = true;
                        }
                        Keycode::Down => {
                            if state.selected + 1 < len { state.selected += 1; }
                            state.clamp_scroll();
                            state.dirty = true;
                        }
                        Keycode::Home => {
                            state.selected = 0;
                            state.clamp_scroll();
                            state.dirty = true;
                        }
                        Keycode::End => {
                            if len > 0 { state.selected = len - 1; }
                            state.clamp_scroll();
                            state.dirty = true;
                        }
                        Keycode::PageUp => {
                            state.selected = state.selected.saturating_sub(vis);
                            state.clamp_scroll();
                            state.dirty = true;
                        }
                        Keycode::PageDown => {
                            state.selected = (state.selected + vis).min(len.saturating_sub(1));
                            state.clamp_scroll();
                            state.dirty = true;
                        }

                        Keycode::K => {
                            if let Some(t) = state.tasks.get(state.selected) {
                                if t.is_idle {
                                    info!("[task_manager] cannot kill idle task");
                                } else if let Some(tr) = task::get_task(t.id) {
                                    let _ = tr.kill(task_struct::KillReason::Requested);
                                    info!("[task_manager] killed task {}", t.id);
                                    state.refresh_ticks = 120; // refresh immédiat
                                }
                            }
                        }

                        _ => {}
                    }
                }

                Ok(Some(Event::WindowResizeEvent(_))) => {
                    let a = win.area();
                    state.off_x = a.top_left.x;
                    state.off_y = a.top_left.y;
                    state.cw    = (a.bottom_right.x - a.top_left.x) as usize;
                    state.ch    = (a.bottom_right.y - a.top_left.y) as usize;
                    state.clamp_scroll();
                    state.dirty = true;
                }

                Ok(None)    => break,
                Ok(Some(_)) => {}
                Err(_)      => break,
            }
        }

        if state.dirty {
            {
                let mut fb = win.framebuffer_mut();
                redraw(&mut fb, &state);
            }
            if let Err(e) = win.render(None) {
                error!("[task_manager] render: {}", e);
            }
            state.dirty = false;
        }

        scheduler::schedule();
    }

    #[allow(unreachable_code)]
    0
}