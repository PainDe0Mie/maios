//! Task Manager pour mai_os
//!
//! Affiche :
//!  - Stats RAM (libre / total estimé)
//!  - Liste des tâches avec état et CPU
//!  - Utilisation CPU approximée (ratio runnable/total)
//!
//! Pas de GPU API disponible dans le kernel → section affichée comme N/A

#![no_std]
extern crate alloc;
extern crate color;
extern crate font;
extern crate framebuffer;
extern crate frame_allocator;
extern crate shapes;
extern crate task;
extern crate task_struct;
extern crate window;
extern crate window_manager;
extern crate scheduler;
extern crate event_types;
extern crate memory_swap;
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

// ================================================================
// COULEURS (palette Tokyonight)
// ================================================================
const C_BG:         Color = Color::new(0x001A1B26);
const C_PANEL:      Color = Color::new(0x0024283A);
const C_BORDER:     Color = Color::new(0x00414868);
const C_ACCENT:     Color = Color::new(0x007AA2F7);
const C_GREEN:      Color = Color::new(0x009ECE6A);
const C_YELLOW:     Color = Color::new(0x00E0AF68);
const C_RED:        Color = Color::new(0x00F7768E);
const C_PURPLE:     Color = Color::new(0x00BB9AF7);
const C_CYAN:       Color = Color::new(0x007DCFFF);
const C_FG:         Color = Color::new(0x00C0CAF5);
const C_FG_DIM:     Color = Color::new(0x00565F89);
const C_HEADER_BG:  Color = Color::new(0x00292E42);

// ================================================================
// LAYOUT
// ================================================================
const WIN_W:         usize = 760;
const WIN_H:         usize = 520;
const PADDING:       isize = 12;
const ROW_H:         usize = 18;
const COL_ID:        isize = PADDING;
const COL_NAME:      isize = PADDING + 52;
const COL_STATE:     isize = PADDING + 260;
const COL_CPU:       isize = PADDING + 380;
const COL_FLAGS:     isize = PADDING + 460;
const HEADER_H:      usize = font::CHARACTER_HEIGHT + 8;
const SECTION_H:     usize = 110;  // hauteur section stats
const LIST_START_Y:  usize = SECTION_H + HEADER_H + 8;

// ================================================================
// HELPERS DE DESSIN
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

/// Tronque le texte à `max_chars` et dessine
fn draw_text_clipped(fb: &mut Fb, x: isize, y: isize, text: &str, max_chars: usize, c: Color) {
    let truncated: String = text.chars().take(max_chars).collect();
    draw_text(fb, x, y, &truncated, c);
}

/// Dessine une barre de progression (0..=100)
fn draw_bar(fb: &mut Fb, x: isize, y: isize, w: usize, h: usize, pct: usize, fg: Color) {
    fill(fb, x, y, w, h, Color::new(0x00111115));
    let filled = (w * pct.min(100)) / 100;
    if filled > 0 {
        fill(fb, x, y, filled, h, fg);
    }
    // bordure
    for px in x..x+w as isize {
        fb.draw_pixel(Coord::new(px, y), C_BORDER.into());
        fb.draw_pixel(Coord::new(px, y + h as isize - 1), C_BORDER.into());
    }
    for py in y..y+h as isize {
        fb.draw_pixel(Coord::new(x, py), C_BORDER.into());
        fb.draw_pixel(Coord::new(x + w as isize - 1, py), C_BORDER.into());
    }
}

// ================================================================
// STATS RAM
// ================================================================

/// Compte les frames libres en itérant FREE_GENERAL_FRAMES_LIST
fn count_free_frames() -> usize {
    let mut total_free = 0usize;
    let _ = frame_allocator::inspect_then_allocate_free_frames(&mut |free_frames| {
        total_free += free_frames.size_in_frames();
        frame_allocator::FramesIteratorRequest::Next
    });
    total_free
}

// ================================================================
// DONNÉES D'UNE TÂCHE
// ================================================================
struct TaskInfo {
    id:       usize,
    name:     String,
    state:    RunState,
    on_cpu:   Option<u32>,
    is_idle:  bool,
    pinned:   bool,
}

fn collect_tasks() -> Vec<TaskInfo> {
    task::all_tasks()
        .into_iter()
        .map(|(id, task_ref)| {
            let state    = task_ref.runstate.load();
            let on_cpu: Option<u32> = Option::<cpu::CpuId>::from(task_ref.running_on_cpu.load())
                .map(|c| c.value());
            let is_idle  = task_ref.is_an_idle_task;
            let pinned: bool = Option::<cpu::CpuId>::from(task_ref.pinned_core.load()).is_some();
            let name     = task_ref.name.clone();
            TaskInfo { id, name, state, on_cpu, is_idle, pinned }
        })
        .collect()
}

fn state_str(state: &RunState) -> (&'static str, Color) {
    match state {
        RunState::Runnable                  => ("RUNNABLE", C_GREEN),
        RunState::Blocked                   => ("BLOCKED ",  C_YELLOW),
        RunState::Exited(_)                 => ("EXITED  ",  C_RED),
        _                                   => ("UNKNOWN ",  C_FG_DIM),
    }
}

// ================================================================
// RENDU
// ================================================================

fn redraw(fb: &mut Fb, tasks: &[TaskInfo], free_frames: usize,
          total_frames_estimate: usize, scroll: usize, win_h: usize)
{
    let w = fb.width() as isize;

    // ── Fond général ────────────────────────────────────────────
    fill(fb, 0, 0, fb.width(), fb.height(), C_BG);

    // ── Titre ───────────────────────────────────────────────────
    fill(fb, 0, 0, fb.width(), 28, C_PANEL);
    hline(fb, 0, w, 28, C_ACCENT);
    draw_text(fb, PADDING, 6, "Mai Task Manager", C_ACCENT);
    // version right-aligned
    draw_text(fb, w - 80, 6, "v0.1.0", C_FG_DIM);

    // ── Section Stats ───────────────────────────────────────────
    let sy: isize = 32;
    fill(fb, PADDING, sy, WIN_W - 2*PADDING as usize, SECTION_H - 4, C_PANEL);
    hline(fb, PADDING, w - PADDING, sy, C_BORDER);
    hline(fb, PADDING, w - PADDING, sy + SECTION_H as isize - 4, C_BORDER);

    // — RAM —
    let ram_title_y = sy + 6;
    draw_text(fb, PADDING + 6, ram_title_y, "RAM", C_ACCENT);

    let free_mb  = (free_frames * 4) / 1024; // frames * 4KB / 1024 = MB
    let total_mb = (total_frames_estimate * 4) / 1024;
    let used_mb  = total_mb.saturating_sub(free_mb);
    let ram_pct  = if total_mb > 0 { (used_mb * 100) / total_mb } else { 0 };

    let bar_y = ram_title_y + font::CHARACTER_HEIGHT as isize + 4;
    draw_bar(fb, PADDING + 6, bar_y, 340, 14, ram_pct, C_ACCENT);

    let ram_str = format!("{} MB / {} MB  ({}% used)", used_mb, total_mb, ram_pct);
    draw_text(fb, PADDING + 6, bar_y + 18, &ram_str, C_FG);

    // — CPU (approximation : ratio tasks Running+Runnable / total) —
    let total_tasks = tasks.len();
    let active_tasks = tasks.iter()
        .filter(|t| matches!(t.state, RunState::Runnable) && !t.is_idle)
        .count();
    let running_tasks = tasks.iter()
        .filter(|t| t.on_cpu.is_some())
        .count();

    let cpu_x: isize = PADDING + 380;
    draw_text(fb, cpu_x, ram_title_y, "CPU", C_ACCENT);

    // on ne peut pas mesurer le vrai % CPU sans compteur TSC entre deux frames
    // → on affiche le nombre de tâches actives/running
    let cpu_str = format!("{} running / {} tasks", running_tasks, total_tasks);
    draw_text(fb, cpu_x, bar_y, &cpu_str, C_FG);

    // Barre visuelle : proportion non-idle non-blocked
    let cpu_pct = if total_tasks > 0 { (active_tasks * 100) / total_tasks } else { 0 };
    draw_bar(fb, cpu_x, bar_y + 20, 340, 14, cpu_pct, C_GREEN);

    // — GPU —
    let gpu_x: isize = PADDING + 6;
    let gpu_y = bar_y + 44;
    draw_text(fb, gpu_x, gpu_y, "GPU", C_ACCENT);
    draw_text(fb, gpu_x, gpu_y + font::CHARACTER_HEIGHT as isize + 2,
              "N/A (no GPU API in kernel)", C_FG_DIM);

    // — Swap —
    let swap_x: isize = cpu_x;
    draw_text(fb, swap_x, gpu_y, "SWAP", C_ACCENT);
    if let Some((used, total)) = memory_swap::usage() {
        let swap_str = format!("{} / {} pages used", used, total);
        draw_text(fb, swap_x, gpu_y + font::CHARACTER_HEIGHT as isize + 2, &swap_str, C_CYAN);
    } else {
        draw_text(fb, swap_x, gpu_y + font::CHARACTER_HEIGHT as isize + 2,
                  "not initialized", C_FG_DIM);
    }

    // ── Header liste tâches ──────────────────────────────────────
    let header_y = (SECTION_H + 34) as isize;
    fill(fb, 0, header_y, fb.width(), HEADER_H, C_HEADER_BG);
    hline(fb, 0, w, header_y + HEADER_H as isize - 1, C_ACCENT);

    draw_text(fb, COL_ID,    header_y + 4, "ID   ", C_FG_DIM);
    draw_text(fb, COL_NAME,  header_y + 4, "NAME", C_FG_DIM);
    draw_text(fb, COL_STATE, header_y + 4, "STATE   ", C_FG_DIM);
    draw_text(fb, COL_CPU,   header_y + 4, "CPU", C_FG_DIM);
    draw_text(fb, COL_FLAGS, header_y + 4, "FLAGS", C_FG_DIM);

    // ── Liste des tâches ─────────────────────────────────────────
    let list_y = header_y + HEADER_H as isize;
    let visible_rows = (win_h.saturating_sub(LIST_START_Y)) / ROW_H;

    let display_tasks: Vec<&TaskInfo> = tasks.iter()
        .skip(scroll)
        .take(visible_rows)
        .collect();

    for (i, task) in display_tasks.iter().enumerate() {
        let row_y = list_y + (i * ROW_H) as isize;

        // Fond alterné
        let bg = if i % 2 == 0 { C_BG } else { Color::new(0x001E2030) };
        fill(fb, 0, row_y, fb.width(), ROW_H, bg);

        // Couleur selon état
        let (state_s, state_c) = state_str(&task.state);

        // Idle tasks en grisé
        let name_color = if task.is_idle { C_FG_DIM } else { C_FG };

        // ID
        let id_str = format!("{:<4}", task.id);
        draw_text(fb, COL_ID, row_y + 1, &id_str, C_PURPLE);

        // Nom (tronqué à ~25 chars)
        let max_name = (COL_STATE - COL_NAME) as usize / font::CHARACTER_WIDTH;
        draw_text_clipped(fb, COL_NAME, row_y + 1, &task.name, max_name, name_color);

        // État
        draw_text(fb, COL_STATE, row_y + 1, state_s, state_c);

        // CPU
        let cpu_str = match task.on_cpu {
            Some(c) => format!("#{}", c),
            None    => "-  ".to_string(),
        };
        draw_text(fb, COL_CPU, row_y + 1, &cpu_str, C_CYAN);

        // Flags
        let mut flags = String::new();
        if task.is_idle  { flags.push_str("IDLE "); }
        if task.pinned   { flags.push_str("PIN"); }
        if flags.is_empty() { flags.push('-'); }
        draw_text(fb, COL_FLAGS, row_y + 1, &flags, C_FG_DIM);

        // Séparateur bas de ligne
        hline(fb, 0, w, row_y + ROW_H as isize - 1, Color::new(0x00252535));
    }

    // ── Scrollbar ────────────────────────────────────────────────
    if tasks.len() > visible_rows {
        let sb_x = w - 8;
        let sb_h = (win_h - LIST_START_Y) as isize;
        fill(fb, sb_x, list_y, 6, sb_h as usize, Color::new(0x00111115));

        let thumb_h = ((visible_rows * sb_h as usize) / tasks.len()).max(20) as isize;
        let thumb_y = (scroll * sb_h as usize / tasks.len()) as isize;
        fill(fb, sb_x, list_y + thumb_y, 6, thumb_h as usize, C_BORDER);
    }

    // ── Footer : raccourcis ──────────────────────────────────────
    let footer_y = win_h as isize - ROW_H as isize - 2;
    fill(fb, 0, footer_y, fb.width(), ROW_H + 2, C_PANEL);
    hline(fb, 0, w, footer_y, C_BORDER);
    draw_text(fb, PADDING, footer_y + 2,
              "[UP/DOWN] scroll   [K] kill task   [R] refresh   [Q] quit",
              C_FG_DIM);
}

// ================================================================
// POINT D'ENTRÉE
// ================================================================

pub fn main(_args: Vec<alloc::string::String>) -> isize {
    info!("[task_manager] Starting...");

    // Crée la fenêtre
    let mut win = match window::Window::with_title(
        "Task Manager".to_string(),
        Coord::new(60, 60),
        WIN_W, WIN_H,
        C_BG,
    ) {
        Ok(w)  => w,
        Err(e) => { error!("[task_manager] Window: {}", e); return -1; }
    };

    // Estime la RAM totale au démarrage (free + déjà alloué)
    // On utilise le free actuel comme base min ; pour une vraie valeur
    // il faudrait un registre de total au boot.
    // Heuristique : on multiplie par 1.5 la première mesure
    let initial_free = count_free_frames();
    let total_estimate = initial_free + initial_free / 2;

    let mut scroll:     usize = 0;
    let mut selected:   usize = 0;
    let mut refresh_tick: usize = 0;

    loop {
        refresh_tick += 1;
        let do_refresh = refresh_tick >= 60; // refresh toutes les ~60 itérations

        // ── Événements ──────────────────────────────────────────
        loop {
            match win.handle_event() {
                Ok(Some(Event::KeyboardEvent(ke))) => {
                    use keycodes_ascii::{KeyAction, Keycode};
                    if ke.key_event.action != KeyAction::Pressed { continue; }
                    match ke.key_event.keycode {
                        Keycode::Q => return 0,
                        Keycode::R => { refresh_tick = 60; }
                        Keycode::Up => {
                            if selected > 0 { selected -= 1; }
                            if selected < scroll { scroll = selected; }
                        }
                        Keycode::Down => {
                            selected += 1;
                            let tasks = collect_tasks();
                            if selected >= tasks.len() {
                                selected = tasks.len().saturating_sub(1);
                            }
                            let visible = (WIN_H - LIST_START_Y) / ROW_H;
                            if selected >= scroll + visible {
                                scroll = selected.saturating_sub(visible - 1);
                            }
                        }
                        Keycode::K => {
                            let tasks = collect_tasks();
                            if let Some(t) = tasks.get(selected) {
                                if let Some(task_ref) = task::get_task(t.id) {
                                    let _ = task_ref.kill(task_struct::KillReason::Requested);
                                    info!("[task_manager] Killed task {}", t.id);
                                }
                            }
                            refresh_tick = 60;
                        }
                        _ => {}
                    }
                }
                Ok(None) => break, // plus d'events en attente
                Ok(Some(_)) => {}  // autres events ignorés
                Err(_) => break,
            }
        }

        if do_refresh {
            refresh_tick = 0;
            let tasks      = collect_tasks();
            let free_frames = count_free_frames();

            // Borne scroll
            let visible = (WIN_H - LIST_START_Y) / ROW_H;
            if scroll + visible > tasks.len() {
                scroll = tasks.len().saturating_sub(visible);
            }

            // Dessine dans le framebuffer de la fenêtre
            {
                let mut fb = win.framebuffer_mut();
                redraw(&mut fb, &tasks, free_frames, total_estimate, scroll, WIN_H);
            }

            // Demande au WM de rafraîchir la zone de la fenêtre
            if let Err(e) = win.render(None) {
                error!("[task_manager] render: {}", e);
            }
        }

        scheduler::schedule();
    }

    #[allow(unreachable_code)]
    0
}