//! Gestionnaire des tâches de MaiOS.
//!
//! Quatre vues :
//! - **Processus** : arbre des processus Linux (et de leurs fils), applications MaiOS et tâches
//!   du noyau, avec CPU (part de la machine), mémoire, état, cœur ; tri par colonne, recherche,
//!   menu contextuel (terminer, tuer, suspendre, priorité, propriétés).
//! - **Performance** : graphes sur deux minutes du processeur (total et par cœur), de la
//!   mémoire, du tas du noyau et du GPU (utilisation de l'appareil, débit, latence de
//!   présentation, composition), avec charge moyenne et pression (PSI).
//! - **Ordonnanceur** : compteurs de MaiSched par cœur (commutations, réveils, migrations,
//!   préemptions, latence de réveil) et indice d'équité de Jain entre les cœurs.
//! - **Détails** : toutes les tâches avec leurs paramètres EEVDF (poids, tranche, rafale).
//!
//! Clavier : Ctrl+Tab (vue suivante), Ctrl+F ou `/` (recherche), flèches, Pg préc./suiv.,
//! Origine/Fin, ← → (replier/déplier), Entrée (propriétés), Suppr (fin de tâche),
//! Maj+Suppr (tuer), Espace (pause), + et − (période), Échap (fermer la recherche).

#![no_std]
extern crate alloc;
#[macro_use]
extern crate log;

mod sample;

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use color::Color;
use event_types::Event;
use keycodes_ascii::{KeyAction, KeyEvent, Keycode};
use mai_ui::canvas::{text_width, Canvas, Rect, Rgb, CHAR_H};
use mai_ui::icons::{self, Icon};
use mai_ui::input::{ctrl_letter, key_char, Pointer};
use mai_ui::kit::{
    self, chart_grid, human_bytes, human_duration, pal, permille_str, thousands, Align, ButtonKind, Column, Dialog,
    DialogResult, FieldEvent, Menu, MenuItem, Scroll, TextField, Toast, ToastKind,
};
use sample::{Entry, Kind, Sampler, State, HIST};
use shapes::Coord;

// ================================================================
// MISE EN PAGE
// ================================================================

const WIN_W: usize = 1200;
const WIN_H: usize = 720;
const HEADER_H: isize = 52;
const TABS_H: isize = 38;
const STATUS_H: isize = 28;
const ROW_H: isize = 26;
const COLHEAD_H: isize = 30;
const SIDEBAR_W: isize = 236;
const SCROLL_W: isize = 10;
const INDENT: isize = 18;

const TAB_LABELS: [&str; 4] = ["Processus", "Performance", "Ordonnanceur", "Détails"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Processes = 0,
    Performance = 1,
    Scheduler = 2,
    Details = 3,
}

impl Tab {
    fn from(i: usize) -> Tab {
        match i {
            1 => Tab::Performance,
            2 => Tab::Scheduler,
            3 => Tab::Details,
            _ => Tab::Processes,
        }
    }
}

// Colonnes de la vue Processus.
const P_NAME: usize = 0;
const P_PID: usize = 1;
const P_STATE: usize = 2;
const P_CPU: usize = 3;
const P_MEM: usize = 4;
const P_THREADS: usize = 5;
const P_CORE: usize = 6;
const PROC_COLS: [Column; 7] = [
    Column::flex("Nom", 220, 1),
    Column::fixed("PID", 72, Align::Right),
    Column::fixed("État", 138, Align::Left),
    Column::fixed("Processeur", 134, Align::Right),
    Column::fixed("Mémoire", 108, Align::Right),
    Column::fixed("Fils", 62, Align::Right),
    Column::fixed("Cœur", 66, Align::Right),
];

// Colonnes de la vue Détails.
const DETAIL_COLS: [Column; 12] = [
    Column::fixed("ID", 64, Align::Right),
    Column::flex("Nom", 160, 1),
    Column::fixed("Type", 92, Align::Left),
    Column::fixed("État", 104, Align::Left),
    Column::fixed("Classe", 104, Align::Left),
    Column::fixed("Nice", 64, Align::Right),
    Column::fixed("Poids", 72, Align::Right),
    Column::fixed("Tranche", 98, Align::Right),
    Column::fixed("Rafale", 92, Align::Right),
    Column::fixed("Temps CPU", 116, Align::Right),
    Column::fixed("Coeur", 76, Align::Right),
    Column::fixed("Épinglé", 90, Align::Right),
];

// Colonnes de la vue Ordonnanceur.
const SCHED_COLS: [Column; 10] = [
    Column::fixed("Cœur", 64, Align::Left),
    Column::flex("Utilisation", 150, 1),
    Column::fixed("Pression", 98, Align::Right),
    Column::fixed("File", 62, Align::Right),
    Column::fixed("Commut./s", 106, Align::Right),
    Column::fixed("Réveils/s", 104, Align::Right),
    Column::fixed("distants", 92, Align::Right),
    Column::fixed("Migr./s", 86, Align::Right),
    Column::fixed("Préempt./s", 112, Align::Right),
    Column::fixed("Latence moy. / max", 176, Align::Right),
];

// Identifiants des actions de menu.
const M_END: u32 = 1;
const M_KILL: u32 = 2;
const M_STOP: u32 = 3;
const M_CONT: u32 = 4;
const M_PRIO_HIGH: u32 = 5;
const M_PRIO_NORMAL: u32 = 6;
const M_PRIO_LOW: u32 = 7;
const M_PROPS: u32 = 8;

// Identifiants des boîtes de dialogue.
const D_KILL_KERNEL: u32 = 1;
const D_PROPS: u32 = 2;

/// Groupes de la vue Processus.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Group {
    Linux,
    Apps,
    Kernel,
}

impl Group {
    fn label(self) -> &'static str {
        match self {
            Group::Linux => "Processus Linux",
            Group::Apps => "Applications MaiOS",
            Group::Kernel => "Tâches du noyau",
        }
    }
}

/// Ligne affichée.
#[derive(Clone, Copy)]
enum VRow {
    Group { g: Group, count: usize, cpu_pm: u32, mem: u64 },
    Item { idx: usize, depth: u8, expandable: bool, expanded: bool },
}

/// Identité stable d'une ligne (survit aux rafraîchissements).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Key {
    Group(Group),
    Entry(u8, usize),
}

fn kind_code(k: Kind) -> u8 {
    match k {
        Kind::Process => 0,
        Kind::Thread => 1,
        Kind::App => 2,
        Kind::Kernel => 3,
        Kind::Idle => 4,
    }
}

fn kind_label(k: Kind) -> &'static str {
    match k {
        Kind::Process => "Processus",
        Kind::Thread => "Fil",
        Kind::App => "Application",
        Kind::Kernel => "Noyau",
        Kind::Idle => "Inactive",
    }
}

fn kind_icon(k: Kind) -> Icon {
    match k {
        Kind::Process => Icon::Binary,
        Kind::Thread => Icon::Thread,
        Kind::App => Icon::App,
        Kind::Kernel => Icon::Gear,
        Kind::Idle => Icon::Clock,
    }
}

fn state_color(s: State) -> Rgb {
    match s {
        State::Running => pal::GREEN,
        State::Ready => pal::CYAN,
        State::Sleeping => pal::TEXT_DIM,
        State::Stopped => pal::YELLOW,
        State::Starting => pal::PURPLE,
        State::Exited => pal::RED,
    }
}

/// Zones à tester pour le pointeur, calculées au dessin.
#[derive(Default)]
struct Hits {
    tabs: Vec<Rect>,
    search: Rect,
    columns: Vec<Rect>,
    list: Rect,
    track: Rect,
    end_button: Rect,
    sidebar: Vec<Rect>,
    pause_button: Rect,
}

struct App {
    sampler: Sampler,
    tab: Tab,
    /// Zone de contenu de la fenêtre (hors décorations).
    area: Rect,
    pointer: Pointer,
    hits: Hits,
    dirty: bool,
    quit: bool,
    paused: bool,
    next_sample: u64,

    // Vue Processus / Détails.
    rows: Vec<VRow>,
    selected: Option<Key>,
    scroll: Scroll,
    sort: (usize, bool),
    detail_sort: (usize, bool),
    collapsed: BTreeSet<Key>,
    /// Arbres repliés : clés des processus dont les enfants sont masqués ; les processus
    /// avec des fils sont repliés par défaut, les groupes dépliés.
    expanded: BTreeSet<Key>,
    search: TextField,
    search_focus: bool,
    hover_row: Option<usize>,
    hover_col: Option<usize>,
    hover_tab: Option<usize>,

    // Vue Performance.
    perf_sel: usize,

    menu: Option<Menu>,
    dialog: Option<Dialog>,
    /// Cible de la boîte de dialogue en cours.
    pending: Option<(Kind, usize, Option<usize>)>,
    toast: Option<Toast>,
}

fn now_ns() -> u64 { task::scheduler::clock_ns() }

impl App {
    fn new(area: Rect) -> App {
        let sampler = Sampler::new();
        let mut a = App {
            sampler,
            tab: Tab::Processes,
            area,
            pointer: Pointer::new(),
            hits: Hits::default(),
            dirty: true,
            quit: false,
            paused: false,
            next_sample: now_ns() + 300_000_000,
            rows: Vec::new(),
            selected: None,
            scroll: Scroll::default(),
            sort: (P_CPU, false),
            detail_sort: (0, true),
            collapsed: BTreeSet::new(),
            expanded: BTreeSet::new(),
            search: TextField::new(),
            search_focus: false,
            hover_row: None,
            hover_col: None,
            hover_tab: None,
            perf_sel: 0,
            menu: None,
            dialog: None,
            pending: None,
            toast: None,
        };
        a.rebuild();
        a
    }

    fn toast(&mut self, text: String, kind: ToastKind) {
        self.toast = Some(Toast::new(text, kind, now_ns()));
        self.dirty = true;
    }

    // ── Modèle de la liste ───────────────────────────────────────

    fn entry_key(e: &Entry) -> Key { Key::Entry(kind_code(e.kind), e.id) }

    fn cmp_entries(&self, a: &Entry, b: &Entry, col: usize, asc: bool) -> core::cmp::Ordering {
        use core::cmp::Ordering;
        let o = match col {
            P_NAME => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            P_PID => a.id.cmp(&b.id),
            P_STATE => (a.state as u8).cmp(&(b.state as u8)),
            P_CPU => a.cpu_pm.cmp(&b.cpu_pm).then(a.cpu_ns.cmp(&b.cpu_ns)),
            P_MEM => a.mem.cmp(&b.mem),
            P_THREADS => a.threads.cmp(&b.threads),
            P_CORE => a.core.cmp(&b.core),
            _ => Ordering::Equal,
        };
        let o = if asc { o } else { o.reverse() };
        o.then(a.id.cmp(&b.id))
    }

    fn matches_filter(e: &Entry, q: &str) -> bool {
        if q.is_empty() {
            return true;
        }
        e.name.to_lowercase().contains(q) || e.path.to_lowercase().contains(q) || format!("{}", e.id) == q
    }

    /// Reconstruit les lignes affichées (groupes, arbre, tri, filtre).
    fn rebuild(&mut self) {
        let q = self.search.text().to_lowercase();
        let entries = &self.sampler.entries;
        let (col, asc) = self.sort;
        let mut rows = Vec::new();
        for g in [Group::Linux, Group::Apps, Group::Kernel] {
            let members: Vec<usize> = entries
                .iter()
                .enumerate()
                .filter(|(_, e)| match g {
                    Group::Linux => e.kind == Kind::Process || e.kind == Kind::Thread,
                    Group::Apps => e.kind == Kind::App,
                    Group::Kernel => e.kind == Kind::Kernel || e.kind == Kind::Idle,
                })
                .map(|(i, _)| i)
                .collect();
            let tops: Vec<usize> = members.iter().copied().filter(|&i| entries[i].kind != Kind::Thread).collect();
            if tops.is_empty() {
                continue;
            }
            let cpu: u32 = tops.iter().filter(|&&i| entries[i].kind != Kind::Idle).map(|&i| entries[i].cpu_pm).sum();
            let mem: u64 = tops.iter().map(|&i| entries[i].mem).sum();
            let count = if q.is_empty() { tops.len() } else { members.iter().filter(|&&i| Self::matches_filter(&entries[i], &q)).count() };
            if count == 0 {
                continue;
            }
            rows.push(VRow::Group { g, count, cpu_pm: cpu, mem });
            if self.collapsed.contains(&Key::Group(g)) {
                continue;
            }
            if !q.is_empty() || g != Group::Linux {
                // Liste plate (filtrée et triée) ; l'inactivité reste en fin de liste.
                let mut flat: Vec<usize> = members.iter().copied().filter(|&i| Self::matches_filter(&entries[i], &q)).collect();
                flat.sort_by(|&a, &b| {
                    let (ea, eb) = (&entries[a], &entries[b]);
                    (ea.kind == Kind::Idle).cmp(&(eb.kind == Kind::Idle)).then(self.cmp_entries(ea, eb, col, asc))
                });
                for i in flat {
                    rows.push(VRow::Item { idx: i, depth: 0, expandable: false, expanded: false });
                }
                continue;
            }
            // Arbre des processus Linux : racines = processus dont le parent n'est pas listé.
            let pids: BTreeSet<usize> = tops.iter().map(|&i| entries[i].id).collect();
            let mut roots: Vec<usize> = tops.iter().copied().filter(|&i| entries[i].parent.map_or(true, |p| !pids.contains(&p))).collect();
            roots.sort_by(|&a, &b| self.cmp_entries(&entries[a], &entries[b], col, asc));
            for r in roots {
                self.push_tree(&mut rows, r, 0, &tops, &members);
            }
        }
        self.rows = rows;
        // La sélection disparue revient à la première ligne.
        if let Some(sel) = self.selected {
            if !self.rows.iter().any(|r| self.row_key(r) == sel) {
                self.selected = None;
            }
        }
        let vis = self.visible_rows();
        self.scroll.clamp(vis, self.rows.len());
        self.dirty = true;
    }

    fn push_tree(&self, rows: &mut Vec<VRow>, idx: usize, depth: u8, procs: &[usize], members: &[usize]) {
        let entries = &self.sampler.entries;
        let e = &entries[idx];
        let (col, asc) = self.sort;
        let mut kids: Vec<usize> = procs.iter().copied().filter(|&i| entries[i].parent == Some(e.id) && entries[i].kind == Kind::Process).collect();
        kids.sort_by(|&a, &b| self.cmp_entries(&entries[a], &entries[b], col, asc));
        let threads: Vec<usize> = if e.threads > 1 {
            members.iter().copied().filter(|&i| entries[i].kind == Kind::Thread && entries[i].parent == Some(e.id)).collect()
        } else {
            Vec::new()
        };
        let expandable = !kids.is_empty() || !threads.is_empty();
        let key = Self::entry_key(e);
        let expanded = expandable && self.expanded.contains(&key);
        rows.push(VRow::Item { idx, depth, expandable, expanded });
        if !expanded || depth > 30 {
            return;
        }
        let mut threads = threads;
        threads.sort_by(|&a, &b| self.cmp_entries(&entries[a], &entries[b], col, asc));
        for t in threads {
            rows.push(VRow::Item { idx: t, depth: depth + 1, expandable: false, expanded: false });
        }
        for k in kids {
            self.push_tree(rows, k, depth + 1, procs, members);
        }
    }

    fn row_key(&self, r: &VRow) -> Key {
        match *r {
            VRow::Group { g, .. } => Key::Group(g),
            VRow::Item { idx, .. } => Self::entry_key(&self.sampler.entries[idx]),
        }
    }

    fn selected_index(&self) -> Option<usize> {
        let sel = self.selected?;
        self.rows.iter().position(|r| self.row_key(r) == sel)
    }

    fn selected_entry(&self) -> Option<&Entry> {
        let i = self.selected_index()?;
        match self.rows[i] {
            VRow::Item { idx, .. } => self.sampler.entries.get(idx),
            VRow::Group { .. } => None,
        }
    }

    /// Lignes de la vue Détails (toutes les entités, triées).
    fn detail_order(&self) -> Vec<usize> {
        let q = self.search.text().to_lowercase();
        let entries = &self.sampler.entries;
        let mut v: Vec<usize> = (0..entries.len()).filter(|&i| Self::matches_filter(&entries[i], &q)).collect();
        let (col, asc) = self.detail_sort;
        v.sort_by(|&a, &b| {
            let (x, y) = (&entries[a], &entries[b]);
            let o = match col {
                0 => x.id.cmp(&y.id),
                1 => x.name.to_lowercase().cmp(&y.name.to_lowercase()),
                2 => kind_code(x.kind).cmp(&kind_code(y.kind)),
                3 => (x.state as u8).cmp(&(y.state as u8)),
                4 => x.class.cmp(y.class),
                5 => x.nice.cmp(&y.nice),
                6 => x.weight.cmp(&y.weight),
                7 => x.slice_ns.cmp(&y.slice_ns),
                8 => x.burst_ns.cmp(&y.burst_ns),
                9 => x.cpu_ns.cmp(&y.cpu_ns),
                10 => x.core.cmp(&y.core),
                _ => x.pinned.cmp(&y.pinned),
            };
            let o = if asc { o } else { o.reverse() };
            o.then(x.id.cmp(&y.id)).then(kind_code(x.kind).cmp(&kind_code(y.kind)))
        });
        v
    }

    fn list_len(&self) -> usize {
        match self.tab {
            Tab::Details => self.detail_order().len(),
            _ => self.rows.len(),
        }
    }

    fn visible_rows(&self) -> usize { (self.hits.list.h.max(0) / ROW_H) as usize }

    // ── Actions ──────────────────────────────────────────────────

    fn target(&self) -> Option<(Kind, usize, Option<usize>)> {
        let e = self.selected_entry()?;
        Some((e.kind, e.id, e.task_id))
    }

    /// Envoie un signal au processus (Linux) ; `false` si ce n'est pas un processus.
    #[cfg(target_arch = "x86_64")]
    fn signal(&mut self, kind: Kind, id: usize, sig: i32, what: &str) -> bool {
        let pid = match kind {
            Kind::Process => id,
            Kind::Thread => match self.sampler.entries.iter().find(|e| e.kind == Kind::Thread && e.id == id).and_then(|e| e.parent) {
                Some(p) => p,
                None => return false,
            },
            _ => return false,
        };
        match maiproc::signal_process(pid, sig) {
            Ok(()) => self.toast(format!("{} envoyé au processus {}", what, pid), ToastKind::Success),
            Err(e) => self.toast(format!("Échec : {}", e), ToastKind::Error),
        }
        true
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn signal(&mut self, _kind: Kind, _id: usize, _sig: i32, _what: &str) -> bool { false }

    fn kill_task(&mut self, task_id: usize) {
        match task::get_task(task_id) {
            Some(t) if t.is_an_idle_task => self.toast("La tâche inactive d'un cœur ne peut pas être arrêtée".into(), ToastKind::Error),
            Some(t) => match t.kill(task_struct::KillReason::Requested) {
                Ok(()) => self.toast(format!("Tâche {} « {} » arrêtée", task_id, t.name), ToastKind::Success),
                Err(_) => self.toast(format!("La tâche {} est déjà terminée", task_id), ToastKind::Error),
            },
            None => self.toast(format!("Tâche {} introuvable", task_id), ToastKind::Error),
        }
        self.next_sample = 0;
    }

    /// Fin de tâche (`SIGTERM` pour un processus) ou arrêt forcé (`SIGKILL`).
    fn end_selected(&mut self, force: bool) {
        let Some((kind, id, task_id)) = self.target() else { return };
        match kind {
            Kind::Process | Kind::Thread => {
                let (sig, what) = if force { (9, "SIGKILL") } else { (15, "SIGTERM") };
                self.signal(kind, id, sig, what);
                self.next_sample = now_ns() + 200_000_000;
            }
            Kind::App => {
                if let Some(t) = task_id {
                    self.kill_task(t);
                }
            }
            Kind::Kernel => {
                self.pending = Some((kind, id, task_id));
                let name = self.selected_entry().map(|e| e.name.clone()).unwrap_or_default();
                let l1 = format!("Arrêter la tâche du noyau {} « {} » ?", id, name);
                self.dialog = Some(Dialog::confirm(
                    D_KILL_KERNEL,
                    "Arrêter une tâche du noyau",
                    &[l1.as_str(), "Le système peut devenir instable si elle rend un service."],
                    "Arrêter",
                    true,
                ));
                self.dirty = true;
            }
            Kind::Idle => self.toast("La tâche inactive d'un cœur ne peut pas être arrêtée".into(), ToastKind::Error),
        }
    }

    /// Tâches de l'ordonnanceur concernées par la sélection (tous les fils d'un processus).
    fn selected_tasks(&self) -> Vec<usize> {
        let Some(e) = self.selected_entry() else { return Vec::new() };
        match e.kind {
            Kind::Process => self
                .sampler
                .entries
                .iter()
                .filter(|t| t.kind == Kind::Thread && t.parent == Some(e.id))
                .filter_map(|t| t.task_id)
                .chain(e.task_id)
                .collect::<BTreeSet<usize>>()
                .into_iter()
                .collect(),
            _ => e.task_id.into_iter().collect(),
        }
    }

    fn set_nice(&mut self, nice: i8) {
        let tasks = self.selected_tasks();
        let mut n = 0;
        for id in &tasks {
            if let Some(t) = task::get_task(*id) {
                if !t.is_an_idle_task && task::scheduler::set_priority(&t, (nice + 20) as u8) {
                    n += 1;
                }
            }
        }
        self.toast(format!("Priorité (nice {}) appliquée à {} tâche(s)", nice, n), ToastKind::Success);
        self.next_sample = 0;
    }

    fn open_properties(&mut self) {
        let Some(e) = self.selected_entry().cloned() else { return };
        let mut lines = Vec::new();
        lines.push(format!("Type : {}   Identifiant : {}", kind_label(e.kind), e.id));
        if !e.path.is_empty() {
            lines.push(format!("Exécutable : {}", e.path));
        }
        if let Some(p) = e.parent {
            lines.push(format!("Parent : {}   Groupe : {}   Session : {}", p, e.pgid, e.sid));
        }
        lines.push(format!("État : {}   Cœur : {}", e.state.label(), e.core.map_or("-".to_string(), |c| c.to_string())));
        lines.push(format!("Temps processeur : {}   Part actuelle : {}", human_duration(e.cpu_ns), permille_str(e.cpu_pm)));
        if e.kind == Kind::Process {
            lines.push(format!("Mémoire résidente : {}   Fils : {}", human_bytes(e.mem), e.threads));
            lines.push(format!("Âge : {}", human_duration(e.age_ns)));
            #[cfg(target_arch = "x86_64")]
            if let Some(p) = maiproc::find_process(e.id) {
                if let Some(mm) = p.mm() {
                    let mut vmas = 0usize;
                    let mut virt = 0u64;
                    uvm::for_each_vma(&mm, |v| {
                        vmas += 1;
                        virt += (v.end - v.start) as u64;
                    });
                    lines.push(format!("Espace virtuel : {} en {} zones (VMA)", human_bytes(virt), vmas));
                }
                lines.push(format!("Descripteurs ouverts : {}", p.files.lock().iter().count()));
            }
        } else {
            lines.push(format!("Pile du noyau : {}", human_bytes(e.mem)));
        }
        lines.push(format!("Classe : {}   Nice : {}   Poids : {}", e.class, e.nice, e.weight));
        lines.push(format!(
            "Tranche EEVDF : {}   Rafale moyenne : {}",
            human_duration(e.slice_ns),
            human_duration(e.burst_ns)
        ));
        if let Some(c) = e.pinned {
            lines.push(format!("Épinglée sur le cœur {}", c));
        }
        let title = format!("Propriétés de « {} »", e.name);
        self.dialog = Some(Dialog::info(D_PROPS, &title, lines));
        self.dirty = true;
    }

    fn open_menu(&mut self, x: isize, y: isize) {
        let Some(e) = self.selected_entry() else { return };
        let is_proc = matches!(e.kind, Kind::Process | Kind::Thread);
        let stopped = e.state == State::Stopped;
        let killable = e.kind != Kind::Idle;
        let mut items = alloc::vec![
            MenuItem::new(M_END, "Fin de tâche", "Suppr").enabled(killable),
            MenuItem::new(M_KILL, "Tuer (SIGKILL)", "Maj+Suppr").enabled(is_proc).danger(),
        ];
        if is_proc {
            items.push(if stopped {
                MenuItem::new(M_CONT, "Reprendre (SIGCONT)", "")
            } else {
                MenuItem::new(M_STOP, "Suspendre (SIGSTOP)", "")
            });
        }
        items.push(MenuItem::separator());
        items.push(MenuItem::new(M_PRIO_HIGH, "Priorité haute (nice -10)", "").enabled(killable));
        items.push(MenuItem::new(M_PRIO_NORMAL, "Priorité normale (nice 0)", "").enabled(killable));
        items.push(MenuItem::new(M_PRIO_LOW, "Priorité basse (nice +10)", "").enabled(killable));
        items.push(MenuItem::separator());
        items.push(MenuItem::new(M_PROPS, "Propriétés", "Entrée"));
        self.menu = Some(Menu::new(items, x, y, self.area));
        self.dirty = true;
    }

    fn menu_action(&mut self, id: u32) {
        match id {
            M_END => self.end_selected(false),
            M_KILL => self.end_selected(true),
            M_STOP | M_CONT => {
                if let Some((k, i, _)) = self.target() {
                    let (sig, what) = if id == M_STOP { (19, "SIGSTOP") } else { (18, "SIGCONT") };
                    self.signal(k, i, sig, what);
                    self.next_sample = now_ns() + 150_000_000;
                }
            }
            M_PRIO_HIGH => self.set_nice(-10),
            M_PRIO_NORMAL => self.set_nice(0),
            M_PRIO_LOW => self.set_nice(10),
            M_PROPS => self.open_properties(),
            _ => {}
        }
    }

    fn dialog_done(&mut self, tag: u32, res: DialogResult) {
        if tag == D_KILL_KERNEL && res == DialogResult::Button(1) {
            if let Some((_, _, Some(t))) = self.pending.take() {
                self.kill_task(t);
            }
        }
        self.pending = None;
    }

    // ── Événements ───────────────────────────────────────────────

    fn select_row(&mut self, i: usize) {
        if let Some(r) = self.rows.get(i).copied() {
            self.selected = Some(self.row_key(&r));
            let vis = self.visible_rows();
            self.scroll.follow(i, vis, self.rows.len());
            self.dirty = true;
        }
    }

    fn toggle_row(&mut self, i: usize) {
        let Some(r) = self.rows.get(i).copied() else { return };
        match r {
            VRow::Group { g, .. } => {
                let k = Key::Group(g);
                if !self.collapsed.remove(&k) {
                    self.collapsed.insert(k);
                }
            }
            VRow::Item { expandable: true, .. } => {
                let k = self.row_key(&r);
                if !self.expanded.remove(&k) {
                    self.expanded.insert(k);
                }
            }
            _ => return,
        }
        self.rebuild();
    }

    fn on_key(&mut self, ev: &KeyEvent) {
        if let Some(d) = self.dialog.as_mut() {
            let tag = d.tag;
            let r = d.on_key(ev);
            if r != DialogResult::Open {
                self.dialog = None;
                self.dialog_done(tag, r);
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
        let ctrl = ev.modifiers.is_control();
        if ctrl && ev.keycode == Keycode::Tab {
            let n = TAB_LABELS.len();
            let i = if ev.modifiers.is_shift() { (self.tab as usize + n - 1) % n } else { (self.tab as usize + 1) % n };
            self.switch_tab(Tab::from(i));
            return;
        }
        if ctrl_letter(ev) == Some('f') {
            self.search_focus = true;
            self.search.select_all();
            self.dirty = true;
            return;
        }
        if self.search_focus {
            match self.search.on_key(ev) {
                FieldEvent::Changed => {
                    self.rebuild();
                    self.scroll.offset = 0;
                }
                FieldEvent::Cancel => {
                    self.search.set_text("");
                    self.search_focus = false;
                    self.rebuild();
                }
                FieldEvent::Submit => self.search_focus = false,
                // Flèches, pages, Suppr… : la liste reste pilotable pendant la recherche.
                FieldEvent::Ignored => self.nav_key(ev),
                FieldEvent::None => {}
            }
            self.dirty = true;
            return;
        }
        match ev.keycode {
            Keycode::Space => {
                self.paused = !self.paused;
                let msg = if self.paused { "Mise à jour en pause" } else { "Mise à jour reprise" };
                self.toast(msg.into(), ToastKind::Info);
            }
            Keycode::PadPlus | Keycode::Equals => self.set_period(true),
            Keycode::PadMinus | Keycode::Minus => self.set_period(false),
            Keycode::F5 => self.next_sample = 0,
            _ => {
                if key_char(ev) == Some('/') {
                    self.search_focus = true;
                    self.dirty = true;
                } else {
                    self.nav_key(ev);
                }
            }
        }
    }

    fn set_period(&mut self, longer: bool) {
        const P: [u32; 4] = [1, 2, 5, 10];
        let i = P.iter().position(|&p| p == self.sampler.period_s).unwrap_or(0);
        let j = if longer { (i + 1).min(P.len() - 1) } else { i.saturating_sub(1) };
        self.sampler.period_s = P[j];
        self.toast(format!("Mise à jour toutes les {} s", P[j]), ToastKind::Info);
    }

    fn nav_key(&mut self, ev: &KeyEvent) {
        if self.tab == Tab::Performance {
            let n = PERF_FIXED + self.sampler.cores.len();
            match ev.keycode {
                Keycode::Up => self.perf_sel = self.perf_sel.saturating_sub(1),
                Keycode::Down => self.perf_sel = (self.perf_sel + 1).min(n - 1),
                _ => return,
            }
            self.dirty = true;
            return;
        }
        if self.tab != Tab::Processes {
            let len = self.list_len();
            let vis = self.visible_rows().max(1);
            match ev.keycode {
                Keycode::Up => self.scroll.offset = self.scroll.offset.saturating_sub(1),
                Keycode::Down => self.scroll.offset += 1,
                Keycode::PageUp => self.scroll.offset = self.scroll.offset.saturating_sub(vis),
                Keycode::PageDown => self.scroll.offset += vis,
                Keycode::Home => self.scroll.offset = 0,
                Keycode::End => self.scroll.offset = len,
                _ => return,
            }
            self.scroll.clamp(vis, len);
            self.dirty = true;
            return;
        }
        let len = self.rows.len();
        if len == 0 {
            return;
        }
        let cur = self.selected_index();
        let vis = self.visible_rows().max(1);
        let shift = ev.modifiers.is_shift();
        match ev.keycode {
            Keycode::Up => self.select_row(cur.map_or(0, |i| i.saturating_sub(1))),
            Keycode::Down => self.select_row(cur.map_or(0, |i| (i + 1).min(len - 1))),
            Keycode::PageUp => self.select_row(cur.map_or(0, |i| i.saturating_sub(vis))),
            Keycode::PageDown => self.select_row(cur.map_or(0, |i| (i + vis).min(len - 1))),
            Keycode::Home => self.select_row(0),
            Keycode::End => self.select_row(len - 1),
            Keycode::Left | Keycode::Right => {
                if let Some(i) = cur {
                    let open = match self.rows[i] {
                        VRow::Group { g, .. } => !self.collapsed.contains(&Key::Group(g)),
                        VRow::Item { expanded, .. } => expanded,
                    };
                    if open == (ev.keycode == Keycode::Left) {
                        self.toggle_row(i);
                    }
                }
            }
            Keycode::Enter => {
                if let Some(i) = cur {
                    if matches!(self.rows[i], VRow::Group { .. }) {
                        self.toggle_row(i);
                    } else {
                        self.open_properties();
                    }
                }
            }
            Keycode::Delete => self.end_selected(shift),
            Keycode::F10 if shift => {
                let (x, y) = (self.area.x + self.area.w / 2, self.area.y + self.area.h / 2);
                self.open_menu(x, y);
            }
            Keycode::Menu => {
                let (x, y) = (self.area.x + self.area.w / 2, self.area.y + self.area.h / 2);
                self.open_menu(x, y);
            }
            _ => {}
        }
    }

    fn switch_tab(&mut self, t: Tab) {
        if self.tab != t {
            self.tab = t;
            self.scroll = Scroll::default();
            self.hover_row = None;
            self.hover_col = None;
            self.dirty = true;
        }
    }

    fn on_mouse(&mut self, ev: &event_types::MousePositionEvent) {
        let now = now_ns();
        let a = self.pointer.update(ev, now);
        let (px, py) = (self.pointer.x, self.pointer.y);

        // Boîte de dialogue modale.
        if let Some(d) = self.dialog.as_mut() {
            if a.moved {
                d.pointer_move(self.area, px, py);
                self.dirty = true;
            }
            if a.left_released {
                let tag = d.tag;
                let r = d.click(self.area, px, py);
                if r != DialogResult::Open {
                    self.dialog = None;
                    self.dialog_done(tag, r);
                }
                self.dirty = true;
            }
            return;
        }
        // Menu contextuel.
        if let Some(m) = self.menu.as_mut() {
            let h = m.item_at(px, py);
            if h != m.hover {
                m.hover = h;
                self.dirty = true;
            }
            if a.left_released || a.right_released {
                let inside = m.rect().contains(px, py);
                let pick = h.map(|i| m.items[i].id);
                if inside || a.left_released {
                    self.menu = None;
                    self.dirty = true;
                    if let Some(id) = pick {
                        self.menu_action(id);
                    }
                }
            }
            return;
        }

        // Survols.
        let ht = self.hits.tabs.iter().position(|r| r.contains(px, py));
        if ht != self.hover_tab {
            self.hover_tab = ht;
            self.dirty = true;
        }
        let hc = self.hits.columns.iter().position(|r| r.contains(px, py));
        if hc != self.hover_col {
            self.hover_col = hc;
            self.dirty = true;
        }
        let hr = if self.hits.list.contains(px, py) && !self.hits.track.contains(px, py) {
            Some(self.scroll.offset + ((py - self.hits.list.y) / ROW_H) as usize)
        } else {
            None
        };
        if hr != self.hover_row {
            self.hover_row = hr;
            self.dirty = true;
        }

        // Glisser de la barre de défilement.
        let len = self.list_len();
        let vis = self.visible_rows();
        if self.scroll.grab.is_some() {
            if self.pointer.left {
                self.scroll.drag(self.hits.track, py, vis, len);
            } else {
                self.scroll.grab = None;
            }
            self.dirty = true;
            return;
        }

        if a.wheel != 0 && self.hits.list.contains(px, py) {
            self.scroll.wheel(a.wheel, vis, len);
            self.dirty = true;
        }

        if a.left_pressed {
            if let Some(i) = ht {
                self.switch_tab(Tab::from(i));
                return;
            }
            if self.hits.search.contains(px, py) {
                self.search_focus = true;
                self.search.click(self.hits.search, px);
                self.dirty = true;
                return;
            }
            if self.search_focus {
                self.search_focus = false;
                self.dirty = true;
            }
            if self.hits.pause_button.contains(px, py) {
                self.paused = !self.paused;
                self.dirty = true;
                return;
            }
            if self.hits.end_button.contains(px, py) {
                self.end_selected(false);
                return;
            }
            if self.hits.track.contains(px, py) {
                self.scroll.press(self.hits.track, py, vis, len);
                self.dirty = true;
                return;
            }
            if let Some(c) = hc {
                match self.tab {
                    Tab::Processes => {
                        self.sort = if self.sort.0 == c { (c, !self.sort.1) } else { (c, c == P_NAME) };
                        self.rebuild();
                    }
                    Tab::Details => {
                        self.detail_sort = if self.detail_sort.0 == c { (c, !self.detail_sort.1) } else { (c, c <= 2) };
                    }
                    _ => {}
                }
                self.dirty = true;
                return;
            }
            if let Some(i) = self.hits.sidebar.iter().position(|r| r.contains(px, py)) {
                self.perf_sel = i;
                self.dirty = true;
                return;
            }
            if self.tab == Tab::Processes {
                if let Some(i) = hr.filter(|&i| i < self.rows.len()) {
                    // Chevron : replier / déplier.
                    let depth = match self.rows[i] {
                        VRow::Item { depth, .. } => depth as isize,
                        VRow::Group { .. } => -1,
                    };
                    let chevron_x = self.hits.list.x + 8 + (depth.max(0)) * INDENT;
                    self.select_row(i);
                    if matches!(self.rows[i], VRow::Group { .. }) || (px >= chevron_x - 4 && px < chevron_x + 16) || a.clicks >= 2 {
                        if a.clicks >= 2 && matches!(self.rows[i], VRow::Item { expandable: false, .. }) {
                            self.open_properties();
                        } else {
                            self.toggle_row(i);
                        }
                    }
                }
            }
        }
        if a.right_pressed && self.tab == Tab::Processes {
            if let Some(i) = hr.filter(|&i| i < self.rows.len()) {
                self.select_row(i);
                if matches!(self.rows[i], VRow::Item { .. }) {
                    self.open_menu(px, py);
                }
            }
        }
    }

    // ── Dessin ───────────────────────────────────────────────────

    fn draw(&mut self, c: &mut Canvas) {
        let now = now_ns();
        let area = self.area;
        c.set_clip(area);
        c.fill(area, pal::BG);
        self.hits = Hits::default();

        let (header, rest) = area.split_top(HEADER_H);
        let (tabs, rest) = rest.split_top(TABS_H);
        let (body, status) = rest.split_bottom(STATUS_H);
        self.draw_header(c, header);
        self.hits.tabs = kit::tabs(c, tabs, &TAB_LABELS, self.tab as usize, self.hover_tab);
        match self.tab {
            Tab::Processes => self.draw_processes(c, body),
            Tab::Performance => self.draw_performance(c, body),
            Tab::Scheduler => self.draw_scheduler(c, body),
            Tab::Details => self.draw_details(c, body),
        }
        self.draw_status(c, status);

        if let Some(m) = &self.menu {
            m.draw(c);
        }
        if let Some(d) = self.dialog.as_mut() {
            d.draw(c, area);
        }
        if let Some(t) = &self.toast {
            t.draw(c, area, now);
        }
    }

    fn draw_header(&mut self, c: &mut Canvas, r: Rect) {
        c.vgradient(r, pal::SURFACE1, pal::SURFACE0);
        icons::draw(c, Icon::Process, r.x + 16, r.y + (r.h - 24) / 2, 24);
        c.text_bold(r.x + 52, r.y + (r.h - CHAR_H) / 2, "Gestionnaire des tâches", pal::TEXT);
        // Indicateurs.
        let mut x = r.right() - 16;
        let s = &self.sampler;
        let mut chips = alloc::vec![
            (format!("Pression {}", permille_str(s.psi_pm(0))), if s.psi_pm(0) > 200 { pal::ORANGE } else { pal::TEAL }),
            (format!("Mémoire {}", permille_str(s.mem.used_pm())), kit::load_color(s.mem.used_pm() / 10)),
            (format!("Processeur {}", permille_str(s.busy_pm)), kit::load_color(s.busy_pm / 10)),
        ];
        if s.gpu.report.is_some() {
            chips.insert(0, (format!("GPU {}", permille_str(s.gpu.busy_pm)), kit::load_color(s.gpu.busy_pm / 10)));
        }
        for (label, col) in chips.iter() {
            let w = text_width(label) + 12;
            x -= w;
            kit::chip(c, x, r.y + (r.h - CHAR_H - 2) / 2, label, *col);
            x -= 8;
        }
        // Recherche.
        let sw = 280.min((x - r.x - 300).max(140));
        let sr = Rect::new(x - sw - 8, r.y + (r.h - 30) / 2, sw, 30);
        let ph = if self.tab == Tab::Details { "Rechercher (nom, ID)" } else { "Rechercher un processus (Ctrl+F)" };
        self.search.draw(c, sr, self.search_focus, ph, 20);
        icons::draw(c, Icon::Search, sr.x + 8, sr.y + (sr.h - 16) / 2, 16);
        self.hits.search = sr;
        c.hline(r.x, r.right(), r.bottom() - 1, pal::BORDER);
    }

    fn draw_status(&mut self, c: &mut Canvas, r: Rect) {
        c.fill(r, pal::SURFACE0);
        c.hline(r.x, r.right(), r.y, pal::BORDER);
        let s = &self.sampler;
        let text = format!(
            "{} tâches  ·  {} processus  ·  {} fils  ·  {} cœurs  ·  Disponibilité {}",
            s.nr_tasks,
            s.nr_processes,
            s.nr_threads,
            s.cores.len(),
            human_duration(s.uptime_ns)
        );
        c.text_in(r, 14, &text, pal::TEXT_DIM);
        let p = if self.paused { "En pause (Espace)".to_string() } else { format!("Mise à jour : {} s", s.period_s) };
        let pw = text_width(&p) + 20;
        let pr = Rect::new(r.right() - pw - 10, r.y + 3, pw, r.h - 6);
        let hover = pr.contains(self.pointer.x, self.pointer.y);
        if hover || self.paused {
            c.round_rect(pr, 6, if self.paused { pal::YELLOW.darken(150) } else { pal::SURFACE3 });
        }
        c.text_center(pr, &p, if self.paused { pal::YELLOW } else { pal::TEXT_DIM });
        self.hits.pause_button = pr;
    }

    fn draw_processes(&mut self, c: &mut Canvas, body: Rect) {
        let (main, footer) = body.split_bottom(48);
        let (head, list) = main.split_top(COLHEAD_H);
        let (list, track) = list.split_right(SCROLL_W);
        let head = Rect::new(head.x, head.y, head.w - SCROLL_W, head.h);
        let cols = kit::table_header(c, head, &PROC_COLS, Some(self.sort), self.hover_col);
        self.hits.columns = cols.clone();
        self.hits.list = list;
        self.hits.track = track;
        let vis = (list.h / ROW_H) as usize;
        self.scroll.clamp(vis, self.rows.len());
        let saved = c.push_clip(list);
        let sel = self.selected_index();
        let total_cpu = self.sampler.busy_pm.max(1);
        for (n, ri) in (self.scroll.offset..self.rows.len()).take(vis + 1).enumerate() {
            let y = list.y + n as isize * ROW_H;
            let row = Rect::new(list.x, y, list.w, ROW_H);
            let is_sel = sel == Some(ri);
            let hover = self.hover_row == Some(ri);
            match self.rows[ri] {
                VRow::Group { g, count, cpu_pm, mem } => {
                    c.fill(row, if is_sel { pal::SELECTION } else { pal::SURFACE1 });
                    let open = !self.collapsed.contains(&Key::Group(g));
                    c.text(row.x + 10, y + (ROW_H - CHAR_H) / 2, if open { "▼" } else { "►" }, pal::TEXT_DIM);
                    let label = format!("{} ({})", g.label(), count);
                    c.text_bold(row.x + 30, y + (ROW_H - CHAR_H) / 2, &label, pal::ACCENT);
                    let cc = Rect::new(cols[P_CPU].x, y, cols[P_CPU].w, ROW_H);
                    kit::cell(c, cc, Align::Right, &permille_str(cpu_pm), pal::TEXT_SOFT);
                    if mem > 0 {
                        let mc = Rect::new(cols[P_MEM].x, y, cols[P_MEM].w, ROW_H);
                        kit::cell(c, mc, Align::Right, &human_bytes(mem), pal::TEXT_SOFT);
                    }
                }
                VRow::Item { idx, depth, expandable, expanded } => {
                    let e = &self.sampler.entries[idx];
                    let bg = if is_sel {
                        pal::SELECTION
                    } else if hover {
                        pal::SURFACE2
                    } else if n % 2 == 1 {
                        pal::SURFACE0.mix(pal::BG, 128)
                    } else {
                        pal::BG
                    };
                    c.fill(row, bg);
                    // Chaleur de la colonne CPU (comme Windows) : plus la part est grande, plus le fond est chaud.
                    let cc = Rect::new(cols[P_CPU].x, y, cols[P_CPU].w, ROW_H);
                    if e.cpu_pm > 5 && e.kind != Kind::Idle {
                        let heat = (e.cpu_pm * 1000 / total_cpu.max(100)).min(1000);
                        c.fill_alpha(cc, kit::load_color(e.cpu_pm / 10), 30 + heat * 90 / 1000);
                    }
                    if e.mem > 64 * 1024 * 1024 && e.kind == Kind::Process {
                        let mc = Rect::new(cols[P_MEM].x, y, cols[P_MEM].w, ROW_H);
                        c.fill_alpha(mc, pal::PURPLE, 40);
                    }
                    if is_sel {
                        c.fill(Rect::new(row.x, y, 3, ROW_H), pal::ACCENT);
                    }
                    let x0 = row.x + 8 + depth as isize * INDENT;
                    if expandable {
                        c.text(x0, y + (ROW_H - CHAR_H) / 2, if expanded { "▼" } else { "►" }, pal::TEXT_DIM);
                    }
                    icons::draw(c, kind_icon(e.kind), x0 + 16, y + (ROW_H - 16) / 2, 16);
                    let name_col = cols[P_NAME];
                    let tx = x0 + 38;
                    let name_c = if e.kind == Kind::Idle { pal::TEXT_DIM } else { pal::TEXT };
                    let w = c.text_fit(tx, y + (ROW_H - CHAR_H) / 2, name_col.right() - tx - 8, &e.name, name_c);
                    if e.kind == Kind::Process && !e.path.is_empty() && tx + w + 80 < name_col.right() {
                        let px = tx + w + 10;
                        c.text_fit(px, y + (ROW_H - CHAR_H) / 2, name_col.right() - px - 8, &e.path, pal::TEXT_FAINT);
                    }
                    let cell = |i: usize| Rect::new(cols[i].x, y, cols[i].w, ROW_H);
                    kit::cell(c, cell(P_PID), Align::Right, &format!("{}", e.id), pal::TEXT_DIM);
                    c.disc(cols[P_STATE].x + 14, y + ROW_H / 2, 3, state_color(e.state), 255);
                    c.text_in(Rect::new(cols[P_STATE].x + 14, y, cols[P_STATE].w - 14, ROW_H), 10, e.state.label(), pal::TEXT_SOFT);
                    kit::cell(c, cc, Align::Right, &permille_str(e.cpu_pm), if e.cpu_pm > 0 { pal::TEXT } else { pal::TEXT_DIM });
                    let mem = if e.mem == 0 { "-".to_string() } else { human_bytes(e.mem) };
                    kit::cell(c, cell(P_MEM), Align::Right, &mem, pal::TEXT_SOFT);
                    let th = if e.kind == Kind::Process { format!("{}", e.threads) } else { "-".to_string() };
                    kit::cell(c, cell(P_THREADS), Align::Right, &th, pal::TEXT_DIM);
                    let core = e.core.map_or("-".to_string(), |x| format!("{}", x));
                    kit::cell(c, cell(P_CORE), Align::Right, &core, pal::CYAN);
                }
            }
        }
        if self.rows.is_empty() {
            c.text_center(list, "Aucun résultat", pal::TEXT_DIM);
        }
        c.set_clip(saved);
        let hover_track = track.contains(self.pointer.x, self.pointer.y);
        self.scroll.draw(c, track, vis, self.rows.len(), hover_track);

        // Pied : sélection et bouton d'action.
        c.fill(footer, pal::SURFACE0);
        c.hline(footer.x, footer.right(), footer.y, pal::BORDER);
        let info = match self.selected_entry() {
            Some(e) => format!("{} {} « {} » · {} · temps processeur {}", kind_label(e.kind), e.id, e.name, e.class, human_duration(e.cpu_ns)),
            None => "Sélectionnez une ligne · clic droit pour les actions".to_string(),
        };
        c.text_in(Rect::new(footer.x, footer.y, footer.w - 200, footer.h), 16, &info, pal::TEXT_DIM);
        let can = self.selected_entry().map_or(false, |e| e.kind != Kind::Idle);
        let br = Rect::new(footer.right() - 156, footer.y + 9, 140, 30);
        let hover = br.contains(self.pointer.x, self.pointer.y);
        kit::button(c, br, "Fin de tâche", ButtonKind::Danger, hover, can);
        self.hits.end_button = if can { br } else { Rect::default() };
    }

    fn draw_details(&mut self, c: &mut Canvas, body: Rect) {
        let (head, list) = body.split_top(COLHEAD_H);
        let (list, track) = list.split_right(SCROLL_W);
        let head = Rect::new(head.x, head.y, head.w - SCROLL_W, head.h);
        let cols = kit::table_header(c, head, &DETAIL_COLS, Some(self.detail_sort), self.hover_col);
        self.hits.columns = cols.clone();
        self.hits.list = list;
        self.hits.track = track;
        let order = self.detail_order();
        let vis = (list.h / ROW_H) as usize;
        self.scroll.clamp(vis, order.len());
        let saved = c.push_clip(list);
        for (n, &i) in order.iter().skip(self.scroll.offset).take(vis + 1).enumerate() {
            let e = &self.sampler.entries[i];
            let y = list.y + n as isize * ROW_H;
            let row = Rect::new(list.x, y, list.w, ROW_H);
            let hover = self.hover_row == Some(self.scroll.offset + n);
            c.fill(row, if hover { pal::SURFACE2 } else if n % 2 == 1 { pal::SURFACE0.mix(pal::BG, 128) } else { pal::BG });
            let cell = |k: usize| Rect::new(cols[k].x, y, cols[k].w, ROW_H);
            let vals: [(String, Rgb); 12] = [
                (format!("{}", e.id), pal::TEXT_DIM),
                (e.name.clone(), if e.kind == Kind::Idle { pal::TEXT_DIM } else { pal::TEXT }),
                (kind_label(e.kind).to_string(), pal::TEXT_SOFT),
                (e.state.label().to_string(), state_color(e.state)),
                (e.class.to_string(), pal::PURPLE),
                (format!("{}", e.nice), pal::TEXT_SOFT),
                (format!("{}", e.weight), pal::TEXT_SOFT),
                (if e.slice_ns == 0 { "-".into() } else { human_duration(e.slice_ns) }, pal::TEXT_SOFT),
                (if e.burst_ns == 0 { "-".into() } else { human_duration(e.burst_ns) }, pal::TEXT_SOFT),
                (human_duration(e.cpu_ns), pal::TEXT),
                (e.core.map_or("-".into(), |x| format!("{}", x)), pal::CYAN),
                (e.pinned.map_or("-".into(), |x| format!("{}", x)), pal::TEXT_DIM),
            ];
            for (k, (v, col)) in vals.iter().enumerate() {
                kit::cell(c, cell(k), DETAIL_COLS[k].align, v, *col);
            }
        }
        c.set_clip(saved);
        let hover_track = track.contains(self.pointer.x, self.pointer.y);
        self.scroll.draw(c, track, vis, order.len(), hover_track);
    }

    fn draw_scheduler(&mut self, c: &mut Canvas, body: Rect) {
        let s = &self.sampler;
        // Cartes de synthèse.
        let (cards, rest) = body.inset(16).split_top(84);
        let n = s.cores.len().max(1) as u64;
        let lat_avg = s.cores.iter().map(|k| k.wake_lat_avg_ns).sum::<u64>() / n;
        let lat_max = s.cores.iter().map(|k| k.wake_lat_max_ns).max().unwrap_or(0);
        // Indice d'équité de Jain (1984) sur l'utilisation des cœurs : 1 = charge parfaitement répartie.
        let sum: u64 = s.cores.iter().map(|k| k.busy_pm as u64).sum();
        let sq: u64 = s.cores.iter().map(|k| (k.busy_pm as u64).pow(2)).sum();
        let jain = if sq == 0 { 1000 } else { sum * sum * 1000 / (n * sq) };
        let card_vals = [
            ("Commutations / s", thousands(s.switches_ps), pal::ACCENT),
            ("Réveils / s", thousands(s.wakeups_ps), pal::GREEN),
            ("Migrations / s", thousands(s.migrations_ps), pal::PURPLE),
            ("Latence de réveil", format!("{} (max {})", human_duration(lat_avg), human_duration(lat_max)), pal::ORANGE),
            (
                "Équité entre cœurs (Jain)",
                if sum < 50 * n { "- (charge trop faible)".to_string() } else { format!("{},{:03}", jain / 1000, jain % 1000) },
                if jain > 800 || sum < 50 * n { pal::GREEN } else { pal::YELLOW },
            ),
        ];
        let cw = (cards.w - 12 * (card_vals.len() as isize - 1)) / card_vals.len() as isize;
        for (i, (label, val, col)) in card_vals.iter().enumerate() {
            let r = Rect::new(cards.x + i as isize * (cw + 12), cards.y, cw, cards.h);
            c.round_rect(r, 10, pal::SURFACE1);
            c.round_rect(Rect::new(r.x, r.y + 14, 3, r.h - 28), 1, *col);
            c.text_fit(r.x + 16, r.y + 16, r.w - 24, label, pal::TEXT_DIM);
            c.text_bold(r.x + 16, r.y + 46, val, pal::TEXT);
        }
        // Tableau par cœur.
        let (_, table) = rest.split_top(16);
        let (head, list) = table.split_top(COLHEAD_H);
        let cols = kit::table_header(c, head, &SCHED_COLS, None, None);
        let saved = c.push_clip(list);
        for (n, k) in s.cores.iter().enumerate() {
            let y = list.y + n as isize * (ROW_H + 6);
            let row = Rect::new(list.x, y, list.w, ROW_H + 6);
            c.fill(row, if n % 2 == 1 { pal::SURFACE0.mix(pal::BG, 128) } else { pal::BG });
            let cell = |i: usize| Rect::new(cols[i].x, y, cols[i].w, ROW_H + 6);
            icons::draw(c, Icon::Cpu, cols[0].x + 8, y + (ROW_H + 6 - 16) / 2, 16);
            c.text_in(Rect::new(cols[0].x + 24, y, cols[0].w - 24, ROW_H + 6), 6, &format!("{}", k.id), pal::TEXT);
            let ur = cell(1).inset_xy(10, 9);
            kit::meter(c, Rect::new(ur.x, ur.y, ur.w - 70, ur.h), k.busy_pm, kit::load_color(k.busy_pm / 10));
            c.text_right(ur.right(), y + (ROW_H + 6 - CHAR_H) / 2, &permille_str(k.busy_pm), pal::TEXT);
            let v = [
                permille_str(k.pressure_pm),
                format!("{}", k.nr_running),
                thousands(k.switches_ps),
                thousands(k.wakeups_ps),
                thousands(k.remote_wakeups_ps),
                thousands(k.migrations_ps),
                format!("{} / {}", k.tick_preempt_ps, k.wake_preempt_ps),
                format!("{} / {}", human_duration(k.wake_lat_avg_ns), human_duration(k.wake_lat_max_ns)),
            ];
            for (j, t) in v.iter().enumerate() {
                kit::cell(c, cell(j + 2), Align::Right, t, pal::TEXT_SOFT);
            }
        }
        c.set_clip(saved);
        let foot = format!(
            "Préempt./s : au tick / au réveil.  Pression : part du temps où une tâche prête attendait le cœur (PSI).  Charge moyenne : {} {} {}",
            fmt_load(s.load_x100(0)),
            fmt_load(s.load_x100(1)),
            fmt_load(s.load_x100(2))
        );
        c.text_fit(body.x + 16, body.bottom() - CHAR_H - 8, body.w - 32, &foot, pal::TEXT_FAINT);
    }

    fn draw_performance(&mut self, c: &mut Canvas, body: Rect) {
        let (side, main) = body.split_left(SIDEBAR_W);
        c.fill(side, pal::SURFACE0);
        c.vline(side.right() - 1, side.y, side.bottom(), pal::BORDER);
        let s = &self.sampler;
        // Entrées : processeur, mémoire, tas, puis chaque cœur.
        let mut items: Vec<(String, String, Icon, Rgb, [u32; HIST], u32)> = Vec::new();
        items.push(("Processeur".into(), permille_str(s.busy_pm), Icon::Cpu, pal::ACCENT, s.hist_cpu.padded(), 1000));
        items.push((
            "Mémoire".into(),
            format!("{} ({})", human_bytes(s.mem.used()), permille_str(s.mem.used_pm())),
            Icon::Memory,
            pal::PURPLE,
            s.hist_mem.padded(),
            1000,
        ));
        let heap_now = s.mem.heap_segments as u64 * 2 * 1024 * 1024 + s.mem.heap_large_bytes;
        items.push(("Tas du noyau".into(), human_bytes(heap_now), Icon::Library, pal::TEAL, s.hist_heap.padded(), s.hist_heap.max().max(1)));
        let g = &s.gpu;
        let gpu_val = match &g.report {
            Some(_) => format!("{} · {} img/s", permille_str(g.busy_pm), g.frames_ps),
            None => format!("Sans accélération · {} img/s", g.frames_ps),
        };
        items.push(("GPU".into(), gpu_val, Icon::Gpu, pal::GREEN, s.hist_gpu.padded(), 1000));
        for k in &s.cores {
            items.push((format!("Cœur {}", k.id), permille_str(k.busy_pm), Icon::Cpu, kit::series_color(k.id), k.hist.padded(), 1000));
        }
        self.perf_sel = self.perf_sel.min(items.len() - 1);
        let ih = 60;
        let saved = c.push_clip(side);
        for (i, (name, val, icon, col, hist, max)) in items.iter().enumerate() {
            let r = Rect::new(side.x + 8, side.y + 8 + i as isize * (ih + 4), side.w - 16, ih);
            if r.y > side.bottom() {
                break;
            }
            let on = i == self.perf_sel;
            let hover = r.contains(self.pointer.x, self.pointer.y);
            if on || hover {
                c.round_rect(r, 8, if on { pal::SELECTION } else { pal::SURFACE2 });
            }
            let spark = Rect::new(r.x + 8, r.y + 8, 64, r.h - 16);
            c.fill(spark, pal::SURFACE0);
            c.area_chart(spark, &hist[HIST - 40..], *max, *col, *col, 140);
            c.stroke(spark, pal::BORDER);
            icons::draw(c, *icon, r.x + 82, r.y + 10, 16);
            c.text_fit(r.x + 104, r.y + 10, r.w - 110, name, pal::TEXT);
            c.text_fit(r.x + 82, r.y + 34, r.w - 88, val, pal::TEXT_DIM);
            self.hits.sidebar.push(r);
        }
        c.set_clip(saved);

        let m = main.inset(20);
        let (title, rest) = m.split_top(44);
        let (name, sub, _, col, hist, max) = &items[self.perf_sel];
        c.text_bold(title.x, title.y + 4, name, pal::TEXT);
        let subtitle = match self.perf_sel {
            0 => format!("{} cœurs · MaiSched (EEVDF, tranches adaptatives)", s.cores.len()),
            1 => format!("Allocateur de frames : {}", s.mem.allocator),
            2 => "kalloc (segments de 2 Mio, listes libres partagées)".to_string(),
            3 => match &s.gpu.report {
                Some(r) => format!("{} · {}", r.adapter, r.driver),
                None => "Framebuffer du chargeur (Limine), composition par le processeur".to_string(),
            },
            _ => format!("Utilisation {}", sub),
        };
        c.text_fit(title.x, title.y + 24, title.w, &subtitle, pal::TEXT_DIM);
        let right_label = match self.perf_sel {
            1 => permille_str(s.mem.used_pm()),
            3 => permille_str(s.gpu.main_pm()),
            _ => sub.clone(),
        };
        c.text_right(title.right(), title.y + 4, &right_label, *col);

        // Graphe principal.
        let chart_h = (rest.h * 45 / 100).max(120);
        let (chart, rest) = rest.split_top(chart_h);
        chart_grid(c, chart, 4, 12);
        c.area_chart(chart.inset(1), hist, *max, *col, *col, 150);
        let axis = match self.perf_sel {
            2 => human_bytes(*max as u64 * 1024),
            _ => "100 %".to_string(),
        };
        c.text_alpha(chart.x + 6, chart.y + 4, &axis, pal::TEXT_DIM, 200);
        if self.perf_sel == 3 {
            let what = if s.gpu.report.is_some() { "Appareil (temps actif)" } else { "Composition (temps processeur)" };
            c.text_right(chart.right() - 6, chart.y + 4, what, pal::TEXT_DIM);
        }
        c.text_right(chart.right() - 6, chart.bottom() + 4, "maintenant", pal::TEXT_FAINT);
        let span = format!("{} s", HIST as u32 * s.period_s);
        c.text(chart.x, chart.bottom() + 4, &span, pal::TEXT_FAINT);

        let (_, rest) = rest.split_top(26);
        match self.perf_sel {
            0 => self.draw_cpu_details(c, rest),
            1 => self.draw_mem_details(c, rest),
            2 => self.draw_heap_details(c, rest),
            3 => self.draw_gpu_details(c, rest),
            i => self.draw_core_details(c, rest, i - PERF_FIXED),
        }
    }

    fn stat_grid(c: &mut Canvas, r: Rect, stats: &[(&str, String)], cols: usize) {
        let cw = r.w / cols as isize;
        for (i, (k, v)) in stats.iter().enumerate() {
            let x = r.x + (i % cols) as isize * cw;
            let y = r.y + (i / cols) as isize * 44;
            if y + 40 > r.bottom() {
                break;
            }
            c.text_fit(x, y, cw - 16, k, pal::TEXT_DIM);
            c.text_fit(x, y + 18, cw - 16, v, pal::TEXT);
        }
    }

    fn draw_cpu_details(&self, c: &mut Canvas, r: Rect) {
        let s = &self.sampler;
        // Mini-graphes par cœur (jusqu'à 16).
        let n = s.cores.len().min(16);
        let (minis, rest) = if n > 1 { r.split_top(70) } else { (Rect::default(), r) };
        if n > 1 {
            let w = (minis.w - 8 * (n as isize - 1)) / n as isize;
            for (i, k) in s.cores.iter().take(n).enumerate() {
                let cr = Rect::new(minis.x + i as isize * (w + 8), minis.y, w, 54);
                chart_grid(c, cr, 2, 4);
                c.area_chart(cr.inset(1), &k.hist.padded(), 1000, kit::series_color(k.id), kit::series_color(k.id), 130);
                c.text_alpha(cr.x + 4, cr.y + 2, &format!("{}", k.id), pal::TEXT_DIM, 200);
            }
        }
        let stats = [
            ("Utilisation", permille_str(s.busy_pm)),
            ("Pression (10 s / 60 s / 300 s)", format!("{} / {} / {}", permille_str(s.psi_pm(0)), permille_str(s.psi_pm(1)), permille_str(s.psi_pm(2)))),
            ("Charge moyenne (1 / 5 / 15 min)", format!("{}  {}  {}", fmt_load(s.load_x100(0)), fmt_load(s.load_x100(1)), fmt_load(s.load_x100(2)))),
            ("Tâches", thousands(s.nr_tasks as u64)),
            ("Processus / fils", format!("{} / {}", s.nr_processes, s.nr_threads)),
            ("Commutations / s", thousands(s.switches_ps)),
            ("Réveils / s", thousands(s.wakeups_ps)),
            ("Migrations / s", thousands(s.migrations_ps)),
            ("Disponibilité", human_duration(s.uptime_ns)),
        ];
        Self::stat_grid(c, rest, &stats, 3);
    }

    fn draw_mem_details(&self, c: &mut Canvas, r: Rect) {
        let m = &self.sampler.mem;
        let (bar, rest) = r.split_top(44);
        let t = m.total.max(1);
        let used = m.used().saturating_sub(m.user_rss);
        let parts = [
            ((m.user_rss * 1000 / t) as u32, pal::ORANGE),
            ((used * 1000 / t) as u32, pal::PURPLE),
            ((m.zero_pool * 1000 / t) as u32, pal::TEAL),
        ];
        kit::stacked_meter(c, Rect::new(bar.x, bar.y, bar.w, 14), &parts);
        let legend = [("Processus", pal::ORANGE), ("Noyau et caches", pal::PURPLE), ("Pré-zéroée", pal::TEAL), ("Libre", pal::SURFACE3)];
        let mut x = bar.x;
        for (l, col) in legend {
            c.round_rect(Rect::new(x, bar.y + 22, 10, 10), 3, col);
            x += 16 + c.text(x + 16, bar.y + 19, l, pal::TEXT_DIM) + 20;
        }
        let stats = [
            ("Totale", human_bytes(m.total)),
            ("Utilisée", format!("{} ({})", human_bytes(m.used()), permille_str(m.used_pm()))),
            ("Libre", human_bytes(m.free)),
            ("Processus (résidente)", human_bytes(m.user_rss)),
            ("Pool pré-zéroé", human_bytes(m.zero_pool)),
            ("Pages de 2 Mio libres", format!("{} ({})", m.huge_free / (2 * 1024 * 1024), human_bytes(m.huge_free))),
            ("Allocations / libérations par s", format!("{} / {}", thousands(m.allocs_ps), thousands(m.frees_ps))),
            ("Pool zéro : servies / manquées", format!("{} / {}", thousands(m.zeroed_hits), thousands(m.zeroed_misses))),
            ("Échecs d'allocation", thousands(m.failures)),
            ("Réservée libre (MMIO…)", human_bytes(m.reserved_free)),
        ];
        Self::stat_grid(c, rest, &stats, 3);
    }

    fn draw_heap_details(&self, c: &mut Canvas, r: Rect) {
        let m = &self.sampler.mem;
        let stats = [
            ("Segments de 2 Mio mappés", format!("{} ({})", m.heap_segments, human_bytes(m.heap_segments as u64 * 2 * 1024 * 1024))),
            ("Segments en réserve", format!("{}", m.heap_pool)),
            ("Gros objets (mappage direct)", format!("{} ({})", m.heap_large, human_bytes(m.heap_large_bytes))),
            ("Allocations / s", thousands(m.heap_allocs_ps)),
            ("Libérations distantes / s", thousands(m.heap_remote_ps)),
            ("Modèle", "mimalloc : 3 listes par page".to_string()),
        ];
        Self::stat_grid(c, r, &stats, 3);
    }

    fn draw_gpu_details(&self, c: &mut Canvas, r: Rect) {
        let s = &self.sampler;
        let g = &s.gpu;
        // Deux mini-graphes : l'appareil (s'il y en a un) et la composition.
        let (minis, rest) = r.split_top(70);
        let w = (minis.w - 8) / 2;
        let dev = if g.report.is_some() { s.hist_gpu.padded() } else { [0; HIST] };
        let series: [(&str, [u32; HIST], Rgb); 2] = [
            (if g.report.is_some() { "Appareil" } else { "Pas de GPU" }, dev, pal::GREEN),
            ("Composition (CPU)", s.hist_compose.padded(), pal::CYAN),
        ];
        for (i, (label, hist, col)) in series.iter().enumerate() {
            let cr = Rect::new(minis.x + i as isize * (w + 8), minis.y, w, 54);
            chart_grid(c, cr, 2, 4);
            c.area_chart(cr.inset(1), hist, 1000, *col, *col, 130);
            c.text_alpha(cr.x + 4, cr.y + 2, label, pal::TEXT_DIM, 200);
        }
        let (sw, sh) = g.screen;
        let mut stats: Vec<(&str, String)> = Vec::new();
        match &g.report {
            Some(rp) => {
                stats.push(("Utilisation", permille_str(g.busy_pm)));
                stats.push(("Présentations / s", thousands(g.presents_ps)));
                stats.push(("Débit vers l'appareil", format!("{}/s", human_bytes(g.transfer_bps))));
                stats.push((
                    "Latence de présentation moy. / max",
                    format!("{} / {}", human_duration(g.present_lat_ns), human_duration(rp.present_lat_max_ns)),
                ));
                stats.push(("Commandes / s", thousands(g.commands_ps)));
                stats.push(("Interruptions / s", thousands(g.interrupts_ps)));
                stats.push(("Mode", format!("{} x {}", rp.width, rp.height)));
                stats.push(("Mémoire de l'appareil", human_bytes(rp.memory_bytes)));
                let cursor = if rp.hw_cursor {
                    format!("matériel, {} dépl./s", thousands(g.cursor_moves_ps))
                } else {
                    format!("logiciel, {} tracés/s", thousands(g.sw_cursor_ps))
                };
                stats.push(("Curseur", cursor));
                stats.push(("Images composées / s", thousands(g.frames_ps)));
                stats.push(("Composition moy.", human_duration(g.compose_avg_ns)));
                stats.push(("Pixels composés / s", thousands(g.pixels_ps)));
                stats.push(("Emplacement", rp.location.clone()));
                stats.push(("Fonctions", if rp.features.is_empty() { "-".to_string() } else { rp.features.clone() }));
                stats.push(("Erreurs", thousands(rp.errors)));
            }
            None => {
                stats.push(("Accélération", "aucune (framebuffer du chargeur)".to_string()));
                stats.push(("Mode", format!("{} x {}", sw, sh)));
                stats.push(("Composition (part d'un cœur)", permille_str(g.compose_pm)));
                stats.push(("Images composées / s", thousands(g.frames_ps)));
                stats.push(("Composition moy.", human_duration(g.compose_avg_ns)));
                stats.push(("Pixels composés / s", thousands(g.pixels_ps)));
                stats.push(("Curseur", format!("logiciel, {} tracés/s", thousands(g.sw_cursor_ps))));
                stats.push(("Pour l'activer", "QEMU : -vga none -device virtio-vga".to_string()));
            }
        }
        Self::stat_grid(c, rest, &stats, 3);
    }

    fn draw_core_details(&self, c: &mut Canvas, r: Rect, i: usize) {
        let Some(k) = self.sampler.cores.get(i) else { return };
        let stats = [
            ("Utilisation", permille_str(k.busy_pm)),
            ("Pression", permille_str(k.pressure_pm)),
            ("Tâches en file", format!("{}", k.nr_running)),
            ("Commutations / s", thousands(k.switches_ps)),
            ("Réveils / s (dont distants)", format!("{} ({})", thousands(k.wakeups_ps), thousands(k.remote_wakeups_ps))),
            ("Migrations / s", thousands(k.migrations_ps)),
            ("Préemptions au tick / au réveil", format!("{} / {}", k.tick_preempt_ps, k.wake_preempt_ps)),
            ("Latence de réveil moy. / max", format!("{} / {}", human_duration(k.wake_lat_avg_ns), human_duration(k.wake_lat_max_ns))),
            ("Allocations du tas / s", thousands(k.heap_allocs_ps)),
        ];
        Self::stat_grid(c, r, &stats, 3);
    }
}

/// Entrées fixes de la vue Performance avant les cœurs (processeur, mémoire, tas, GPU).
const PERF_FIXED: usize = 4;

fn fmt_load(x100: u64) -> String { format!("{},{:02}", x100 / 100, x100 % 100) }

fn content_rect(win: &window::Window) -> Rect {
    let a = win.area();
    Rect::new(a.top_left.x, a.top_left.y, a.bottom_right.x - a.top_left.x, a.bottom_right.y - a.top_left.y)
}

pub fn main(_args: Vec<String>) -> isize {
    let mut win = match window::Window::with_title(
        "Gestionnaire des tâches".to_string(),
        Coord::new(48, 36),
        WIN_W,
        WIN_H,
        Color::new(0x001A1B26),
    ) {
        Ok(w) => w,
        Err(e) => {
            error!("[task_manager] fenêtre : {}", e);
            return -1;
        }
    };
    win.set_app_id("maios.taskmanager");
    let mut app = App::new(content_rect(&win));

    loop {
        loop {
            match win.handle_event() {
                Ok(Some(Event::ExitEvent)) => return 0,
                Ok(Some(Event::KeyboardEvent(ke))) => {
                    if ke.key_event.action == KeyAction::Pressed {
                        app.on_key(&ke.key_event);
                    }
                }
                Ok(Some(Event::MousePositionEvent(me))) => app.on_mouse(&me),
                Ok(Some(Event::WindowResizeEvent(_))) => {
                    app.area = content_rect(&win);
                    app.dirty = true;
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
            if app.quit {
                return 0;
            }
        }

        let now = now_ns();
        if !app.paused && now >= app.next_sample {
            app.sampler.sample();
            app.rebuild();
            app.next_sample = now + app.sampler.period_s as u64 * 1_000_000_000;
        }
        let animating = app.toast.as_ref().map_or(false, |t| t.alive(now));
        if app.toast.as_ref().map_or(false, |t| !t.alive(now)) {
            app.toast = None;
            app.dirty = true;
        }
        if app.dirty || animating {
            {
                let mut fb = win.framebuffer_mut();
                let mut c = Canvas::from_framebuffer(&mut fb);
                app.draw(&mut c);
            }
            if let Err(e) = win.render(None) {
                error!("[task_manager] rendu : {}", e);
            }
            app.dirty = false;
        }
        let _ = sleep::sleep(sleep::Duration::from_millis(16));
    }
}
