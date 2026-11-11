//! MaiCompositor : gestionnaire de fenêtres et compositeur de MaiOS.
//!
//! # Architecture
//!
//! ```text
//!  applications ──damage(rect)──► DAMAGE (région) ─┐
//!  clavier / souris (IRQ) ──notify_input()────────┼──► sonnette ──► tâche « mai_compositor »
//!  sys_vsync_wait ──attente──► VSYNC_WQ ◄─────────┘        │
//!                                                          ├─ entrées : routage, raccourcis
//!                                                          ├─ horloge d'images (60 Hz, à la demande)
//!                                                          ├─ compose.rs : opaque avant→arrière,
//!                                                          │  translucide arrière→avant, présentation
//!                                                          └─ plan du curseur (asynchrone)
//! ```
//!
//! * **Une seule tâche compose.** Les méthodes `refresh_*` du gestionnaire, `Window::render`
//!   et `sys_present` ne font qu'ajouter un rectangle à la zone abîmée puis sonner : aucune
//!   application ne prend plus le verrou de MGI ni celui des autres fenêtres pour composer.
//! * **Pilotée par les événements.** La tâche dort tant que rien ne change (aucun réveil
//!   périodique, hormis un battement de sûreté par seconde). Elle se réveille sur une entrée,
//!   une zone abîmée, une attente de VSync ou l'échéance de la prochaine image.
//! * **Horloge d'images** (*frame clock*, comme `ClutterFrameClock` de Mutter) : au plus une
//!   image par période de 16,67 ms. Après un repos, la première image part tout de suite
//!   (latence minimale), les suivantes sont calées sur la période. Le compteur VSync n'avance
//!   qu'à chaque battement : `sys_vsync_wait` se bloque sur une file d'attente au lieu de
//!   scruter.
//! * **Curseur en plan séparé**, mis à jour sans attendre l'horloge (comme un curseur matériel).
//!
//! Plan et mesures : `.claude/plans/` (MaiCompositor).

#![no_std]

extern crate alloc;

mod compose;
pub mod cursor;
pub mod doorbell;
mod pixels;

use alloc::collections::VecDeque;
use alloc::string::ToString;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};

use event_types::{Event, MousePositionEvent};
use framebuffer::{AlphaPixel, Framebuffer};
use keycodes_ascii::{KeyAction, KeyEvent, KeyboardModifiers, Keycode};
use log::{error, info, warn};
use mgi::MGI;
use mouse_data::MouseEvent;
use mpmc::Queue;
use region2d::{Rect, Region};
use shapes::{Coord, Rectangle};
use spin::{Mutex, Once};
use sync_irq::IrqSafeMutex;
use time::{Duration, Instant, Monotonic};
use wait_queue::WaitQueue;
use window_inner::{ResizeEdge, WindowInner, WindowMovingStatus, MIN_WINDOW_HEIGHT, MIN_WINDOW_WIDTH};

use compose::{Compositor, Outcome, Scene};
pub use cursor::{CursorImage, CursorShape};
pub use doorbell::Doorbell;

/// The global window manager instance.
pub static WINDOW_MANAGER: Once<Mutex<WindowManager>> = Once::new();

/// Sonnette du bureau (l'explorateur) : appelée quand un événement lui est remis ou que la
/// liste des fenêtres change (focus, réduction, fermeture, titre). Ne doit rien verrouiller
/// du gestionnaire : elle est appelée sous son verrou.
static DESKTOP_RING: Once<fn()> = Once::new();

/// Installe la sonnette du bureau (une seule fois).
pub fn set_desktop_ring(f: fn()) {
    DESKTOP_RING.call_once(|| f);
}

/// Prévient le bureau qu'il a quelque chose à relire (sans verrou).
pub fn notify_desktop() {
    if let Some(f) = DESKTOP_RING.get() {
        f();
    }
}

/// Bureau (fond d'écran, icônes, horloge) dessiné par l'explorateur, sous toutes les fenêtres.
static DESKTOP: Once<Mutex<Framebuffer<AlphaPixel>>> = Once::new();
/// Zone abîmée depuis la dernière image (coordonnées écran).
static DAMAGE: IrqSafeMutex<Region> = IrqSafeMutex::new(Region::new());
/// Réveil de la tâche du compositeur.
static DOORBELL: Doorbell = Doorbell::new();
/// Position du curseur publiée pour le plan du curseur : `(x << 32) | y` (32 bits signés chacun).
static CURSOR_POS: AtomicU64 = AtomicU64::new(0);
static CURSOR_VISIBLE: AtomicBool = AtomicBool::new(true);
/// Image du curseur posée par une application (`None` : flèche par défaut).
static CURSOR_IMG: IrqSafeMutex<Option<Arc<CursorImage>>> = IrqSafeMutex::new(None);
/// Touches enfoncées (codes evdev), tenues par la tâche du compositeur pour tout le clavier.
#[allow(clippy::declare_interior_mutable_const)]
const KEY_ZERO: AtomicU64 = AtomicU64::new(0);
static KEYS: [AtomicU64; KEY_WORDS] = [KEY_ZERO; KEY_WORDS];
/// Modificateurs du dernier événement de clavier.
static KBD_MODS: AtomicU16 = AtomicU16::new(0);
const KEY_WORDS: usize = (keycodes_ascii::evdev::KEY_MAX as usize + 64) / 64;
/// Une application plein écran sans fenêtre a dessiné dans le tampon arrière de MGI.
static RAW_COMMIT: AtomicBool = AtomicBool::new(false);
/// Tâches qui attendent la prochaine VSync (l'horloge d'images tourne tant qu'il y en a).
static VSYNC_WAITERS: AtomicUsize = AtomicUsize::new(0);
static VSYNC_WQ: WaitQueue = WaitQueue::new();
/// Résolution, fixée à l'initialisation (lue sans prendre le verrou de MGI).
static SCREEN: Once<(usize, usize)> = Once::new();
/// Génération des demandes d'image : chaque dommage et chaque [`frame_gen`] l'augmentent,
/// **sous le verrou de [`DAMAGE`]**. L'image qui prend la zone abîmée note la génération du
/// moment : tout ce qui a été demandé jusque-là est dans cette image.
static GEN: AtomicU64 = AtomicU64::new(0);
/// Une image est demandée sans rien abîmer ([`frame_gen`]).
static WANT_FRAME: AtomicBool = AtomicBool::new(false);
/// Dernière image présentée (lue par MaiLand pour `presentation-time` et les rappels `frame`).
static PRESENTED: IrqSafeMutex<Presentation> = IrqSafeMutex::new(Presentation { seq: 0, time_ns: 0, gen: 0 });
/// Échéance de la prochaine image de l'horloge, en nanosecondes monotones.
static NEXT_FRAME_NS: AtomicU64 = AtomicU64::new(0);
/// Prévenu après chaque image présentée (hors de tout verrou).
static PRESENT_HOOK: Once<fn()> = Once::new();
/// Sortie GPU à brancher par la tâche du compositeur ([`attach_scanout`]).
static PENDING_OUTPUT: spin::Mutex<Option<Arc<dyn mgi::Scanout>>> = spin::Mutex::new(None);
/// Une sortie GPU est branchée.
static HAS_OUTPUT: AtomicBool = AtomicBool::new(false);
/// La sortie GPU branchée, pour les outils ([`gpu_report`]).
static OUTPUT: spin::Once<Arc<dyn mgi::Scanout>> = spin::Once::new();

/// Période de l'horloge d'images (60 Hz).
pub const FRAME_PERIOD: Duration = Duration::from_nanos(16_666_667);
/// Délai avant de retenter une image quand une fenêtre était verrouillée.
const BUSY_RETRY: Duration = Duration::from_micros(500);
/// Battement de sûreté quand rien ne se passe.
const HEARTBEAT: Duration = Duration::from_secs(1);
/// Événements d'entrée traités par tour avant de laisser la composition passer.
const INPUT_BATCH: usize = 64;

// ── Statistiques ─────────────────────────────────────────────────────────────

static STAT_FRAMES: AtomicU64 = AtomicU64::new(0);
static STAT_BUSY: AtomicU64 = AtomicU64::new(0);
static STAT_CURSOR: AtomicU64 = AtomicU64::new(0);
/// Tracés du curseur logiciel dans le tampon avant (aucun quand le plan matériel sert).
pub(crate) static STAT_SW_CURSOR: AtomicU64 = AtomicU64::new(0);
static STAT_PIXELS: AtomicU64 = AtomicU64::new(0);
static STAT_COMPOSE_NS: AtomicU64 = AtomicU64::new(0);

/// Compteurs cumulés du compositeur (pour les bancs et le gestionnaire des tâches).
#[derive(Clone, Copy, Debug, Default)]
pub struct CompositorStats {
    /// Images composées (battements de l'horloge).
    pub frames: u64,
    /// Images remises parce qu'une fenêtre était verrouillée.
    pub busy_retries: u64,
    /// Mises à jour du seul plan du curseur.
    pub cursor_updates: u64,
    /// Tracés du curseur logiciel (le curseur matériel d'une sortie GPU n'en fait aucun).
    pub sw_cursor_draws: u64,
    /// Pixels recomposés et présentés.
    pub pixels: u64,
    /// Temps passé à composer, en nanosecondes.
    pub compose_ns: u64,
}

pub fn stats() -> CompositorStats {
    CompositorStats {
        frames: STAT_FRAMES.load(Ordering::Relaxed),
        busy_retries: STAT_BUSY.load(Ordering::Relaxed),
        cursor_updates: STAT_CURSOR.load(Ordering::Relaxed),
        sw_cursor_draws: STAT_SW_CURSOR.load(Ordering::Relaxed),
        pixels: STAT_PIXELS.load(Ordering::Relaxed),
        compose_ns: STAT_COMPOSE_NS.load(Ordering::Relaxed),
    }
}

// ── API de validation (pour les fenêtres, les applications et les appels système) ────────────

#[inline]
fn rect_of(r: &Rectangle) -> Rect {
    Rect::new(r.top_left.x as i32, r.top_left.y as i32, r.bottom_right.x as i32, r.bottom_right.y as i32)
}

/// Résolution de l'écran, sans verrou.
pub fn screen_size() -> (usize, usize) {
    SCREEN.get().copied().unwrap_or((0, 0))
}

/// Déclare `r` (coordonnées écran) abîmé et réveille le compositeur.
pub fn damage(r: Rectangle) {
    damage_rect(rect_of(&r));
}

/// Comme [`damage`], avec un [`Rect`].
pub fn damage_rect(r: Rect) {
    if r.is_empty() {
        return;
    }
    {
        let mut d = DAMAGE.lock();
        d.add(r);
        GEN.fetch_add(1, Ordering::AcqRel);
    }
    DOORBELL.ring();
}

/// Une image présentée par l'horloge du compositeur.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Presentation {
    /// Numéro de l'image (compteur VSync de MGI).
    pub seq: u64,
    /// Instant où l'image a atteint l'affichage, en nanosecondes de l'horloge monotone (celle
    /// de `CLOCK_MONOTONIC` des processus).
    pub time_ns: u64,
    /// Plus grande génération ([`frame_gen`]) comprise dans cette image.
    pub gen: u64,
}

/// Nanosecondes de l'horloge monotone.
pub fn monotonic_ns(t: Instant) -> u64 {
    t.duration_since(Instant::ZERO).as_nanos() as u64
}

/// Demande une image (même sans rien abîmer) et rend sa génération : quand
/// [`last_presentation`]`().gen` l'atteint, tout ce qui a été abîmé avant cet appel est à
/// l'écran.
pub fn frame_gen() -> u64 {
    let g = {
        let _d = DAMAGE.lock();
        WANT_FRAME.store(true, Ordering::Release);
        GEN.fetch_add(1, Ordering::AcqRel) + 1
    };
    DOORBELL.ring();
    g
}

/// Dernière image présentée.
pub fn last_presentation() -> Presentation {
    *PRESENTED.lock()
}

/// Instant le plus tôt (nanosecondes monotones) où une image demandée maintenant peut
/// atteindre l'affichage : la prochaine échéance de l'horloge, ou tout de suite au repos.
pub fn next_frame_ns(now: Instant) -> u64 {
    NEXT_FRAME_NS.load(Ordering::Acquire).max(monotonic_ns(now))
}

/// Pose la fonction appelée après chaque image présentée (une seule, posée une fois : le
/// serveur Wayland). Elle doit être brève et sûre partout (sonner une tâche).
pub fn set_present_hook(f: fn()) {
    PRESENT_HOOK.call_once(|| f);
}

/// Branche une sortie GPU (MaiGPU) : la tâche du compositeur en fait le tampon avant de MGI à
/// son prochain tour, recompose tout l'écran et présente ensuite par elle (zones abîmées,
/// curseur matériel). La sortie doit avoir la résolution de l'écran.
pub fn attach_scanout(out: Arc<dyn mgi::Scanout>) {
    *PENDING_OUTPUT.lock() = Some(out);
    DOORBELL.ring();
}

/// Vrai une fois une sortie GPU branchée.
pub fn has_gpu_output() -> bool {
    HAS_OUTPUT.load(Ordering::Acquire)
}

/// Identité et compteurs du GPU qui affiche l'écran (`None` : framebuffer du chargeur, sans
/// GPU). Ne prend ni le verrou de MGI ni celui du compositeur : appelable par une application.
pub fn gpu_report() -> Option<mgi::GpuReport> {
    OUTPUT.get().and_then(|o| o.report())
}

/// Déclare tout l'écran abîmé.
pub fn damage_all() {
    let (w, h) = screen_size();
    damage_rect(Rect::new(0, 0, w as i32, h as i32));
}

/// Demande une image (sans rien abîmer : utile pour le curseur et la VSync).
pub fn request_frame() {
    DOORBELL.ring();
}

/// Une application plein écran sans fenêtre a fini de dessiner dans le tampon arrière.
pub fn commit_raw_backbuffer() {
    RAW_COMMIT.store(true, Ordering::Release);
    DOORBELL.ring();
}

/// Le tampon du plein écran exclusif est prêt (il est recopié à chaque image de toute façon).
pub fn commit_exclusive() {
    DOORBELL.ring();
}

/// Numéro de la dernière image affichée.
pub fn vsync_counter() -> u64 {
    mgi::vsync_counter()
}

/// Bloque jusqu'à la prochaine image de l'horloge du compositeur ; renvoie son numéro.
pub fn vsync_wait() -> u64 {
    let start = mgi::vsync_counter();
    if WINDOW_MANAGER.get().is_none() {
        let _ = sleep::sleep(FRAME_PERIOD);
        return mgi::vsync_counter();
    }
    VSYNC_WAITERS.fetch_add(1, Ordering::AcqRel);
    DOORBELL.ring();
    let n = VSYNC_WQ.wait_until(|| {
        let n = mgi::vsync_counter();
        if n != start { Some(n) } else { None }
    });
    VSYNC_WAITERS.fetch_sub(1, Ordering::AcqRel);
    n
}

/// Accès en lecture au bureau.
pub fn with_desktop<R>(f: impl FnOnce(&Framebuffer<AlphaPixel>) -> R) -> Option<R> {
    DESKTOP.get().map(|d| f(&d.lock()))
}

/// Accès en écriture au bureau. L'appelant déclare ensuite la zone modifiée ([`damage`]).
pub fn with_desktop_mut<R>(f: impl FnOnce(&mut Framebuffer<AlphaPixel>) -> R) -> Option<R> {
    DESKTOP.get().map(|d| f(&mut d.lock()))
}

/// Image du curseur courante.
pub fn cursor_image() -> Arc<CursorImage> {
    let img = CURSOR_IMG.lock().clone();
    img.unwrap_or_else(|| cursor::shape_image(CursorShape::Default))
}

/// Pose l'image du curseur (`None` : flèche par défaut). Le plan du curseur est redessiné à la
/// prochaine occasion, sans recomposer l'écran.
pub fn set_cursor_image(img: Option<Arc<CursorImage>>) {
    *CURSOR_IMG.lock() = img;
    DOORBELL.ring();
}

/// Pose une forme intégrée du curseur.
pub fn set_cursor_shape(shape: CursorShape) {
    set_cursor_image(if shape == CursorShape::Default { None } else { Some(cursor::shape_image(shape)) });
}

/// Touches enfoncées du clavier (codes evdev).
#[derive(Clone, Copy)]
pub struct PressedKeys([u64; KEY_WORDS]);

impl PressedKeys {
    pub fn contains(&self, code: u16) -> bool {
        let c = code as usize;
        c / 64 < KEY_WORDS && self.0[c / 64] & (1 << (c % 64)) != 0
    }

    /// Codes enfoncés, croissants.
    pub fn iter(&self) -> impl Iterator<Item = u16> + '_ {
        (0..KEY_WORDS * 64).filter(|&c| self.0[c / 64] & (1 << (c % 64)) != 0).map(|c| c as u16)
    }
}

/// État du clavier vu par le compositeur (toutes fenêtres confondues) : touches enfoncées et
/// modificateurs. Sert à qui reçoit le focus (`wl_keyboard.enter` porte les touches enfoncées).
pub fn keyboard_state() -> (PressedKeys, KeyboardModifiers) {
    let mut k = [0u64; KEY_WORDS];
    for (d, s) in k.iter_mut().zip(KEYS.iter()) {
        *d = s.load(Ordering::Acquire);
    }
    (PressedKeys(k), KeyboardModifiers::from_bits_truncate(KBD_MODS.load(Ordering::Acquire)))
}

fn track_key(key: &KeyEvent) {
    let c = key.code as usize;
    if c != 0 && c / 64 < KEY_WORDS {
        let bit = 1u64 << (c % 64);
        match key.action {
            KeyAction::Pressed => KEYS[c / 64].fetch_or(bit, Ordering::AcqRel),
            KeyAction::Released => KEYS[c / 64].fetch_and(!bit, Ordering::AcqRel),
        };
    }
    KBD_MODS.store(key.modifiers.bits(), Ordering::Release);
}

/// Position du pointeur à l'écran (lecture atomique, sans verrou).
pub fn pointer_position() -> (isize, isize) {
    let v = CURSOR_POS.load(Ordering::Acquire);
    ((v >> 32) as i32 as isize, v as u32 as i32 as isize)
}

fn publish_cursor(c: Coord) {
    let v = ((c.x as i32 as u32 as u64) << 32) | (c.y as i32 as u32 as u64);
    CURSOR_POS.store(v, Ordering::Release);
    DOORBELL.ring();
}

fn ring_input() {
    DOORBELL.ring();
}

// ── WindowManager ─────────────────────────────────────────────────────────────

/// Pile des fenêtres, focus, souris et contour d'aperçu.
pub struct WindowManager {
    /// Fenêtres visibles sous la fenêtre active, de l'arrière vers l'avant.
    show_list: VecDeque<Weak<Mutex<WindowInner>>>,
    /// Fenêtres réduites.
    hide_list: VecDeque<Weak<Mutex<WindowInner>>>,
    /// Fenêtre qui a le focus (au sommet de la pile).
    active: Weak<Mutex<WindowInner>>,
    /// Fenêtres au-dessus de toutes les autres, sans focus ni décorations (menus et bulles des
    /// clients Wayland), de l'arrière vers l'avant.
    overlays: Vec<Weak<Mutex<WindowInner>>>,
    /// Fenêtre qui a reçu le dernier événement de pointeur (elle reçoit `PointerLeave` quand
    /// le pointeur passe ailleurs).
    hover: Weak<Mutex<WindowInner>>,
    /// Saisie implicite : la fenêtre où un bouton a été enfoncé reçoit le pointeur jusqu'au
    /// relâchement du dernier bouton, même hors d'elle (sélection glissée, barres de
    /// défilement).
    implicit_grab: Weak<Mutex<WindowInner>>,
    /// Saisie explicite (menu ouvert) : tous les événements de pointeur vont à cette fenêtre.
    pointer_grab: Weak<Mutex<WindowInner>>,
    /// Position du curseur, coordonnées écran.
    mouse: Coord,
    mouse_btn_left: bool,
    mouse_btn_right: bool,
    /// Contour d'aperçu pendant un déplacement ou un redimensionnement.
    border: Option<Rect>,
    screen: (usize, usize),
    /// Bande réservée en bas de l'écran (barre des tâches) : les fenêtres agrandies ou ancrées
    /// s'arrêtent au-dessus.
    reserved_bottom: usize,
    /// File du bureau (l'explorateur) : pointeur hors de toute fenêtre, clavier sans fenêtre
    /// active, touche Super seule.
    desktop: Option<Queue<Event>>,
    /// Le pointeur était sur le bureau au dernier événement (le bureau reçoit `PointerLeave`
    /// quand il passe sur une fenêtre).
    desktop_hover: bool,
    /// Saisie explicite du clavier (menu du système ouvert) : toutes les touches vont à cette
    /// fenêtre.
    keyboard_grab: Weak<Mutex<WindowInner>>,
    /// Super enfoncée sans autre touche depuis : son relâchement ouvre le menu du système.
    super_alone: bool,
}

/// Une fenêtre gérée, telle que la voit la barre des tâches.
pub struct WindowEntry {
    pub window: Arc<Mutex<WindowInner>>,
    /// Elle a le focus.
    pub active: bool,
    /// Elle est réduite.
    pub hidden: bool,
}

impl WindowManager {
    // ── Listes ────────────────────────────────────────────────────────────────

    fn purge_expired(&mut self) {
        self.show_list.retain(|w| w.upgrade().is_some());
        self.hide_list.retain(|w| w.upgrade().is_some());
        self.overlays.retain(|w| w.upgrade().is_some());
    }

    /// Prévient une fenêtre qu'elle gagne ou perd le focus du clavier.
    fn notify_focus(w: &Arc<Mutex<WindowInner>>, focused: bool) {
        let _ = w.lock().send_event(Event::FocusEvent(focused));
    }

    /// Retire la fenêtre vivante la plus en avant de `list` (rangée de l'arrière vers l'avant) :
    /// celle qui était juste sous la fenêtre active reprend le focus.
    fn pop_top_live(list: &mut VecDeque<Weak<Mutex<WindowInner>>>) -> Option<Weak<Mutex<WindowInner>>> {
        while let Some(w) = list.pop_back() {
            if w.upgrade().is_some() {
                return Some(w);
            }
        }
        None
    }

    fn index_in(list: &VecDeque<Weak<Mutex<WindowInner>>>, target: &Arc<Mutex<WindowInner>>) -> Option<usize> {
        list.iter().position(|w| w.upgrade().map_or(false, |a| Arc::ptr_eq(&a, target)))
    }

    fn is_active_ref(&self, w: &Arc<Mutex<WindowInner>>) -> bool {
        self.active.upgrade().map_or(false, |a| Arc::ptr_eq(&a, w))
    }

    /// Pile visible, de l'avant vers l'arrière (pour le compositeur).
    fn scene(&self) -> Scene {
        let mut windows = Vec::with_capacity(self.show_list.len() + self.overlays.len() + 1);
        for o in self.overlays.iter().rev() {
            if let Some(o) = o.upgrade() {
                windows.push(o);
            }
        }
        if let Some(a) = self.active.upgrade() {
            if !windows.iter().any(|x| Arc::ptr_eq(x, &a)) {
                windows.push(a);
            }
        }
        for w in self.show_list.iter().rev() {
            if let Some(w) = w.upgrade() {
                if !windows.iter().any(|x| Arc::ptr_eq(x, &w)) {
                    windows.push(w);
                }
            }
        }
        Scene { windows, border: self.border }
    }

    // ── Focus ─────────────────────────────────────────────────────────────────

    /// Met `inner_ref` au sommet de la pile et lui donne le focus.
    ///
    /// L'ancienne fenêtre active redescend en tête de `show_list`. Renvoie `true` si la
    /// fenêtre était déjà active.
    pub fn set_active(&mut self, inner_ref: &Arc<Mutex<WindowInner>>, refresh: bool) -> Result<bool, &'static str> {
        self.purge_expired();
        if let Some(current) = self.active.upgrade() {
            if Arc::ptr_eq(&current, inner_ref) {
                if refresh {
                    damage(inner_ref.lock().get_envelope());
                }
                return Ok(true);
            }
            if Self::index_in(&self.show_list, &current).is_none() {
                self.show_list.push_back(Arc::downgrade(&current));
            }
            Self::notify_focus(&current, false);
        }
        if let Some(i) = Self::index_in(&self.show_list, inner_ref) {
            self.show_list.remove(i);
        }
        if let Some(i) = Self::index_in(&self.hide_list, inner_ref) {
            self.hide_list.remove(i);
        }
        self.active = Arc::downgrade(inner_ref);
        Self::notify_focus(inner_ref, true);
        notify_desktop();
        if refresh {
            damage(inner_ref.lock().get_envelope());
        }
        Ok(false)
    }

    // ── Cycle de vie ──────────────────────────────────────────────────────────

    /// Retire une fenêtre et abîme la zone qu'elle découvre.
    pub fn delete_window(&mut self, inner_ref: &Arc<Mutex<WindowInner>>) -> Result<(), &'static str> {
        self.purge_expired();
        let envelope = inner_ref.lock().get_envelope();
        if let Some(i) = self.overlays.iter().position(|w| w.upgrade().is_some_and(|a| Arc::ptr_eq(&a, inner_ref))) {
            self.overlays.remove(i);
            damage(envelope);
            return Ok(());
        }
        notify_desktop();
        let mut found = false;
        if self.is_active_ref(inner_ref) {
            self.active = Self::pop_top_live(&mut self.show_list).unwrap_or_default();
            if let Some(a) = self.active.upgrade() {
                Self::notify_focus(&a, true);
            }
            found = true;
        }
        if let Some(i) = Self::index_in(&self.show_list, inner_ref) {
            self.show_list.remove(i);
            found = true;
        }
        if let Some(i) = Self::index_in(&self.hide_list, inner_ref) {
            self.hide_list.remove(i);
            return Ok(()); // rien de visible ne change
        }
        if !found {
            return Err("delete_window: window not found in any list");
        }
        damage(envelope);
        Ok(())
    }

    /// Réduit une fenêtre.
    pub fn hide_window(&mut self, inner_ref: &Arc<Mutex<WindowInner>>) -> Result<(), &'static str> {
        self.purge_expired();
        notify_desktop();
        let visible = if self.is_active_ref(inner_ref) {
            self.active = Self::pop_top_live(&mut self.show_list).unwrap_or_default();
            Self::notify_focus(inner_ref, false);
            if let Some(a) = self.active.upgrade() {
                Self::notify_focus(&a, true);
            }
            true
        } else if let Some(i) = Self::index_in(&self.show_list, inner_ref) {
            self.show_list.remove(i);
            true
        } else {
            false
        };
        if visible {
            if Self::index_in(&self.hide_list, inner_ref).is_none() {
                self.hide_list.push_back(Arc::downgrade(inner_ref));
            }
            damage(inner_ref.lock().get_envelope());
        }
        Ok(())
    }

    /// Restaure une fenêtre réduite (visible, sans forcément le focus).
    pub fn show_window(&mut self, inner_ref: &Arc<Mutex<WindowInner>>) -> Result<(), &'static str> {
        self.purge_expired();
        if let Some(i) = Self::index_in(&self.hide_list, inner_ref) {
            self.hide_list.remove(i);
            if Self::index_in(&self.show_list, inner_ref).is_none() {
                self.show_list.push_back(Arc::downgrade(inner_ref));
            }
            damage(inner_ref.lock().get_envelope());
            notify_desktop();
        }
        Ok(())
    }

    /// Ajoute une fenêtre au-dessus de toutes les autres (menu, bulle) : elle ne prend pas le
    /// focus ; elle disparaît avec `delete_window` (ou quand elle est détruite).
    pub fn add_overlay(&mut self, inner_ref: &Arc<Mutex<WindowInner>>) {
        self.purge_expired();
        if !self.overlays.iter().any(|w| w.upgrade().is_some_and(|a| Arc::ptr_eq(&a, inner_ref))) {
            self.overlays.push(Arc::downgrade(inner_ref));
        }
        damage(inner_ref.lock().get_envelope());
    }

    /// Saisie explicite du pointeur (`None` : relâchée) : tant qu'elle dure, tous les
    /// événements de pointeur vont à `w`, coordonnées relatives à `w` (hors d'elle comprises).
    pub fn set_pointer_grab(&mut self, w: Option<&Arc<Mutex<WindowInner>>>) {
        self.pointer_grab = w.map(Arc::downgrade).unwrap_or_default();
    }

    // ── Rafraîchissements : de simples déclarations de zone abîmée ────────────

    /// Abîme `region` (`None` : tout l'écran). `_include_active` n'a plus d'effet : la scène
    /// est lue au moment de composer, les listes sont déjà à jour.
    pub fn refresh_bottom_windows(&mut self, region: Option<Rectangle>, _include_active: bool) -> Result<(), &'static str> {
        match region {
            Some(r) => damage(r),
            None => damage_all(),
        }
        Ok(())
    }

    pub fn refresh_windows(&mut self, region: Option<Rectangle>) -> Result<(), &'static str> {
        self.refresh_bottom_windows(region, true)
    }

    pub fn refresh_active_window(&mut self, region: Option<Rectangle>) -> Result<(), &'static str> {
        match (region, self.active.upgrade()) {
            (Some(r), _) => damage(r),
            (None, Some(a)) => damage(a.lock().get_envelope()),
            (None, None) => {}
        }
        Ok(())
    }

    pub fn refresh_top(&mut self, region: Option<Rectangle>) -> Result<(), &'static str> {
        self.refresh_bottom_windows(region, true)
    }

    pub fn refresh_all(&mut self) -> Result<(), &'static str> {
        damage_all();
        Ok(())
    }

    /// Demande une image (la présentation est faite par la tâche du compositeur).
    pub fn present(&mut self) {
        request_frame();
    }

    // ── Souris ────────────────────────────────────────────────────────────────

    fn move_mouse(&mut self, delta: Coord) {
        let (sw, sh) = self.screen;
        let new = Coord::new(
            (self.mouse.x + delta.x).clamp(0, sw as isize - 3),
            (self.mouse.y + delta.y).clamp(0, sh as isize - 3),
        );
        if new != self.mouse {
            self.mouse = new;
            publish_cursor(new);
            // Le contour d'aperçu suit le curseur à la cadence du compositeur, sans attendre
            // que l'application traite ses événements.
            if self.drag_state().is_some() {
                let _ = self.move_floating_border();
            }
        }
    }

    pub fn mouse_position(&self) -> Coord {
        self.mouse
    }
    pub fn mouse_left(&self) -> bool {
        self.mouse_btn_left
    }
    pub fn mouse_right(&self) -> bool {
        self.mouse_btn_right
    }

    /// Affiche ou cache le curseur (plan du curseur).
    pub fn set_cursor_visible(&mut self, visible: bool) {
        CURSOR_VISIBLE.store(visible, Ordering::Release);
        request_frame();
    }

    // ── Contour d'aperçu ──────────────────────────────────────────────────────

    fn set_border(&mut self, new: Option<Rect>) {
        if self.border == new {
            return;
        }
        let mut d = DAMAGE.lock();
        for b in self.border.iter().chain(new.iter()) {
            for band in pixels::border_bands(b).iter() {
                d.add(*band);
            }
        }
        drop(d);
        self.border = new;
        DOORBELL.ring();
    }

    fn drag_state(&self) -> Option<WindowMovingStatus> {
        let a = self.active.upgrade()?;
        let m = a.try_lock()?.moving;
        match m {
            WindowMovingStatus::Stationary => None,
            other => Some(other),
        }
    }

    /// Rectangle visé par le déplacement / redimensionnement en cours de la fenêtre active.
    fn target_rect(&self, win: &WindowInner) -> Option<Rectangle> {
        let mouse = self.mouse;
        let pos = win.get_position();
        let (w, h) = win.get_size();
        match win.moving {
            WindowMovingStatus::Moving(base) => {
                let tl = pos + (mouse.x - base.x, mouse.y - base.y);
                Some(Rectangle { top_left: tl, bottom_right: tl + (w as isize, h as isize) })
            }
            WindowMovingStatus::Resizing(base, edge) => {
                let dx = mouse.x - base.x;
                let dy = mouse.y - base.y;
                let mut r = Rectangle { top_left: pos, bottom_right: pos + (w as isize, h as isize) };
                match edge {
                    ResizeEdge::Right => r.bottom_right.x += dx,
                    ResizeEdge::Bottom => r.bottom_right.y += dy,
                    ResizeEdge::Left => r.top_left.x += dx,
                    ResizeEdge::Top => r.top_left.y += dy,
                    ResizeEdge::BottomRight => { r.bottom_right.x += dx; r.bottom_right.y += dy; }
                    ResizeEdge::BottomLeft => { r.top_left.x += dx; r.bottom_right.y += dy; }
                    ResizeEdge::TopRight => { r.bottom_right.x += dx; r.top_left.y += dy; }
                    ResizeEdge::TopLeft => { r.top_left.x += dx; r.top_left.y += dy; }
                }
                // Mêmes minima que `WindowInner::resize`, pour que l'aperçu soit fidèle.
                if r.width() < MIN_WINDOW_WIDTH {
                    r.bottom_right.x = r.top_left.x + MIN_WINDOW_WIDTH as isize;
                }
                if r.height() < MIN_WINDOW_HEIGHT {
                    r.bottom_right.y = r.top_left.y + MIN_WINDOW_HEIGHT as isize;
                }
                Some(r)
            }
            WindowMovingStatus::Stationary => None,
        }
    }

    /// Applique le déplacement ou le redimensionnement en cours à la fenêtre active.
    pub fn move_active_window(&mut self) -> Result<(), &'static str> {
        let active = self.active.upgrade().ok_or("no active window to move")?;
        let (old_rect, new_rect) = {
            let mut win = active.lock();
            let old_rect = win.get_envelope();
            let target = self.target_rect(&win).ok_or("window is not moving")?;
            match win.moving {
                WindowMovingStatus::Moving(_) => win.set_position(target.top_left),
                WindowMovingStatus::Resizing(..) => win.resize(target)?,
                WindowMovingStatus::Stationary => return Err("window is not moving"),
            }
            (old_rect, win.get_envelope())
        };
        self.set_border(None);
        damage(old_rect);
        damage(new_rect);
        Ok(())
    }

    /// Met le contour d'aperçu à la position visée par la fenêtre active.
    pub fn move_floating_border(&mut self) -> Result<(), &'static str> {
        let target = self.active.upgrade().and_then(|a| {
            let win = a.try_lock()?;
            self.target_rect(&win)
        });
        self.set_border(target.as_ref().map(rect_of));
        Ok(())
    }

    // ── Routage des événements ────────────────────────────────────────────────

    fn route_keyboard(&mut self, key_event: KeyEvent) -> Result<(), &'static str> {
        // Super seule (enfoncée puis relâchée sans autre touche) : au bureau (menu du système).
        let is_super = matches!(key_event.keycode, Keycode::SuperKeyLeft | Keycode::SuperKeyRight);
        if is_super {
            match key_event.action {
                KeyAction::Pressed => self.super_alone = true,
                KeyAction::Released => {
                    if core::mem::replace(&mut self.super_alone, false) {
                        self.to_desktop(Event::new_keyboard_event(key_event));
                    }
                }
            }
            return Ok(());
        }
        if key_event.action == KeyAction::Pressed {
            self.super_alone = false;
        }
        let target = self.keyboard_grab.upgrade().or_else(|| self.active.upgrade());
        match target {
            Some(win) => {
                let r = win.lock().send_event(Event::new_keyboard_event(key_event)).map_err(|_| "keyboard event queue full");
                r
            }
            None => {
                self.to_desktop(Event::new_keyboard_event(key_event));
                Ok(())
            }
        }
    }

    /// Fenêtre la plus haute sous le curseur (menus d'abord ; la fenêtre active pendant un
    /// déplacement ou un redimensionnement).
    fn under_cursor(&self) -> Option<Arc<Mutex<WindowInner>>> {
        let cursor = self.mouse;
        let hit = |a: &Arc<Mutex<WindowInner>>| {
            let w = a.lock();
            w.visible && w.contains(cursor - w.get_position())
        };
        for o in self.overlays.iter().rev() {
            if let Some(a) = o.upgrade() {
                if hit(&a) {
                    return Some(a);
                }
            }
        }
        if let Some(a) = self.active.upgrade() {
            let moving = a.lock().moving != WindowMovingStatus::Stationary;
            if moving || hit(&a) {
                return Some(a);
            }
        }
        self.show_list.iter().rev().filter_map(|w| w.upgrade()).find(hit)
    }

    /// Change la fenêtre survolée ; l'ancienne reçoit `PointerLeave`.
    fn set_hover(&mut self, t: Option<&Arc<Mutex<WindowInner>>>) {
        let old = self.hover.upgrade();
        let same = match (&old, t) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            if let Some(o) = old {
                let _ = o.lock().send_event(Event::PointerLeave);
            }
            self.hover = t.map(Arc::downgrade).unwrap_or_default();
        }
    }

    fn send_mouse(t: &Arc<Mutex<WindowInner>>, ev: &MousePositionEvent) -> Result<(), &'static str> {
        let w = t.lock();
        let mut e = ev.clone();
        e.coordinate = ev.gcoordinate - w.get_position();
        w.send_event(Event::MousePositionEvent(e)).map_err(|_| "mouse event queue full")
    }

    /// Envoie un événement de pointeur (`gcoordinate` renseignée) : à la fenêtre qui tient la
    /// saisie explicite, sinon à celle de la saisie implicite, sinon à la plus haute sous le
    /// curseur.
    fn route_mouse(&mut self, ev: &MousePositionEvent) -> Result<(), &'static str> {
        let held = ev.left_button_hold || ev.right_button_hold || ev.middle_button_hold || ev.fourth_button_hold || ev.fifth_button_hold;
        let target = self.pointer_grab.upgrade().or_else(|| self.implicit_grab.upgrade()).or_else(|| self.under_cursor());
        self.set_hover(target.as_ref());
        // Le bureau reçoit le pointeur hors des fenêtres (coordonnées de l'écran), et
        // `PointerLeave` quand il passe sur une fenêtre.
        let on_desktop = target.is_none();
        if on_desktop {
            self.desktop_hover = true;
            let mut e = ev.clone();
            e.coordinate = ev.gcoordinate;
            self.to_desktop(Event::MousePositionEvent(e));
        } else if core::mem::replace(&mut self.desktop_hover, false) {
            self.to_desktop(Event::PointerLeave);
        }
        let r = match &target {
            Some(t) => Self::send_mouse(t, ev),
            None => Ok(()),
        };
        if held {
            if self.implicit_grab.upgrade().is_none() {
                self.implicit_grab = target.as_ref().map(Arc::downgrade).unwrap_or_default();
            }
        } else if self.implicit_grab.upgrade().is_some() {
            // Fin de la saisie implicite : le pointeur revient à ce qui est sous lui.
            self.implicit_grab = Weak::new();
            let now = self.pointer_grab.upgrade().or_else(|| self.under_cursor());
            let moved = match (&now, &target) {
                (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
                (None, None) => false,
                _ => true,
            };
            if moved {
                self.set_hover(now.as_ref());
                if let Some(n) = &now {
                    let mut e = ev.clone();
                    e.scroll = 0;
                    e.scrolling_up = false;
                    e.scrolling_down = false;
                    let _ = Self::send_mouse(n, &e);
                }
            }
        }
        r
    }

    // ── Requêtes ──────────────────────────────────────────────────────────────

    pub fn is_active(&self, window: &Arc<Mutex<WindowInner>>) -> bool {
        self.is_active_ref(window)
    }

    /// `(largeur, hauteur)` de l'écran (sans verrou de MGI).
    pub fn get_screen_size(&self) -> (usize, usize) {
        self.screen
    }

    pub fn get_active_window(&self) -> Option<Arc<Mutex<WindowInner>>> {
        self.active.upgrade()
    }

    /// Réserve `px` pixels en bas de l'écran (barre des tâches).
    pub fn set_reserved_bottom(&mut self, px: usize) {
        self.reserved_bottom = px.min(self.screen.1 / 2);
    }

    /// Zone de travail : l'écran moins la bande réservée.
    pub fn work_area(&self) -> Rectangle {
        let (w, h) = self.screen;
        Rectangle { top_left: Coord::new(0, 0), bottom_right: Coord::new(w as isize, (h - self.reserved_bottom) as isize) }
    }

    /// Fenêtres gérées (visibles et réduites, ni menus ni surimpressions), dans l'ordre de leur
    /// création.
    pub fn windows(&self) -> Vec<WindowEntry> {
        let active = self.active.upgrade();
        let mut out: Vec<WindowEntry> = Vec::new();
        let mut push = |w: Arc<Mutex<WindowInner>>, hidden: bool| {
            if out.iter().any(|e| Arc::ptr_eq(&e.window, &w)) {
                return;
            }
            let is_active = active.as_ref().is_some_and(|a| Arc::ptr_eq(a, &w));
            out.push(WindowEntry { window: w, active: is_active, hidden });
        };
        if let Some(a) = &active {
            push(a.clone(), false);
        }
        for w in self.show_list.iter().filter_map(|w| w.upgrade()) {
            push(w, false);
        }
        for w in self.hide_list.iter().filter_map(|w| w.upgrade()) {
            push(w, true);
        }
        out.sort_by_key(|e| e.window.lock().serial);
        out
    }

    /// Donne au bureau la file de ses événements (pointeur hors des fenêtres, clavier sans
    /// fenêtre active, Super seule).
    pub fn set_desktop_events(&mut self, q: Queue<Event>) {
        self.desktop = Some(q);
    }

    /// Saisie explicite du clavier (`None` : relâchée).
    pub fn set_keyboard_grab(&mut self, w: Option<&Arc<Mutex<WindowInner>>>) {
        self.keyboard_grab = w.map(Arc::downgrade).unwrap_or_default();
    }

    fn to_desktop(&self, ev: Event) {
        if let Some(q) = &self.desktop {
            let _ = q.push(ev);
            notify_desktop();
        }
    }

    pub fn is_window_hidden(&self, task_id: u64) -> bool {
        let id = task_id as usize;
        self.hide_list.iter().any(|w| w.upgrade().map_or(false, |w| w.lock().task_id == Some(id)))
    }

    pub fn get_window_by_task_id(&self, task_id: usize) -> Option<Arc<Mutex<WindowInner>>> {
        let check = |w: &Weak<Mutex<WindowInner>>| -> Option<Arc<Mutex<WindowInner>>> {
            let arc = w.upgrade()?;
            let owned = arc.lock().task_id == Some(task_id);
            if owned { Some(arc) } else { None }
        };
        if let Some(a) = self.active.upgrade() {
            if a.lock().task_id == Some(task_id) {
                return Some(a);
            }
        }
        self.show_list.iter().find_map(check).or_else(|| self.hide_list.iter().find_map(check))
    }
}

// ── Initialisation ────────────────────────────────────────────────────────────

/// Initialise le gestionnaire et le compositeur ; renvoie `(clavier, souris)` pour les pilotes.
pub fn init() -> Result<(Queue<Event>, Queue<Event>), &'static str> {
    mgi::init()?;
    let (width, height) = MGI.get().ok_or("MGI not initialised")?.lock().resolution();
    SCREEN.call_once(|| (width, height));

    let mut desktop = Framebuffer::new(width, height, None)?;
    // Fond Tokyo Night en attendant que l'explorateur dessine le bureau.
    desktop.fill(AlphaPixel { blue: 0x26, green: 0x1B, red: 0x1A, alpha: 0 });
    DESKTOP.call_once(|| Mutex::new(desktop));

    let mouse = Coord { x: width as isize / 2, y: height as isize / 2 };
    WINDOW_MANAGER.call_once(|| {
        Mutex::new(WindowManager {
            show_list: VecDeque::new(),
            hide_list: VecDeque::new(),
            active: Weak::new(),
            overlays: Vec::new(),
            hover: Weak::new(),
            implicit_grab: Weak::new(),
            pointer_grab: Weak::new(),
            mouse,
            mouse_btn_left: false,
            mouse_btn_right: false,
            border: None,
            screen: (width, height),
            reserved_bottom: 0,
            desktop: None,
            desktop_hover: false,
            keyboard_grab: Weak::new(),
            super_alone: false,
        })
    });
    publish_cursor(mouse);
    damage_all();

    let key_consumer: Queue<Event> = Queue::with_capacity(256);
    let key_producer = key_consumer.clone();
    let mouse_consumer: Queue<Event> = Queue::with_capacity(256);
    let mouse_producer = mouse_consumer.clone();

    spawn::new_task_builder(compositor_loop, (key_consumer, mouse_consumer))
        .name("mai_compositor".to_string())
        .spawn()?;
    info!("MaiCompositor : {}x{}, horloge d'images 60 Hz, composition pilotée par les événements", width, height);
    Ok((key_producer, mouse_producer))
}

// ── Tâche du compositeur ──────────────────────────────────────────────────────

/// Prochaine échéance de l'horloge : calée sur la période, sans rattraper les images manquées.
#[inline]
fn advance(next: Instant, now: Instant) -> Instant {
    let n = next + FRAME_PERIOD;
    if n <= now { now + FRAME_PERIOD } else { n }
}

#[inline]
fn earliest(a: Instant, b: Instant) -> Instant {
    if b < a { b } else { a }
}

fn compositor_loop((keys, mice): (Queue<Event>, Queue<Event>)) -> Result<(), &'static str> {
    DOORBELL.bind_current();
    event_types::set_input_notifier(ring_input);

    let mut c = Compositor::new();
    let mut next_vblank = time::now::<Monotonic>();

    loop {
        let pending_out = PENDING_OUTPUT.lock().take();
        if let Some(out) = pending_out {
            let name = alloc::string::String::from(out.name());
            match c.attach(out.clone()) {
                Ok(()) => {
                    OUTPUT.call_once(|| out);
                    HAS_OUTPUT.store(true, Ordering::Release);
                    damage_all();
                    warn!("MaiCompositor : sortie {} branchee (presentation par zones abimees)", name);
                }
                Err(e) => error!("MaiCompositor : sortie {} refusee : {}", name, e),
            }
        }
        let more_input = pump_input(&keys, &mice);

        let now = time::now::<Monotonic>();
        let exclusive = mgi::is_exclusive_fullscreen();
        if !exclusive && c.leaving_exclusive() {
            damage_all();
        }
        let pending = RAW_COMMIT.load(Ordering::Acquire) || WANT_FRAME.load(Ordering::Acquire) || !DAMAGE.lock().is_empty();
        let waiters = VSYNC_WAITERS.load(Ordering::Acquire) > 0;
        let mut deadline = now + HEARTBEAT;
        let mut composed = false;

        if pending || waiters || exclusive {
            if now >= next_vblank {
                let cursor = compose::cursor_position();
                // La zone abîmée et la génération qu'elle couvre, prises ensemble.
                let (dmg, gen) = {
                    let mut d = DAMAGE.lock();
                    WANT_FRAME.store(false, Ordering::Release);
                    (d.take(), GEN.load(Ordering::Acquire))
                };
                let outcome = if exclusive {
                    RAW_COMMIT.store(false, Ordering::Release);
                    c.present_exclusive();
                    Outcome::Done
                } else {
                    if RAW_COMMIT.swap(false, Ordering::AcqRel) {
                        c.present_raw(cursor.clone());
                    }
                    let area = dmg.area();
                    let scene = match WINDOW_MANAGER.get() {
                        Some(wm) => wm.lock().scene(),
                        None => Scene { windows: Vec::new(), border: None },
                    };
                    let t0 = time::now::<Monotonic>();
                    let o = c.frame(dmg, &scene, cursor);
                    drop(scene);
                    if let Outcome::Done = o {
                        let dt = time::now::<Monotonic>().duration_since(t0);
                        STAT_COMPOSE_NS.fetch_add(dt.as_nanos() as u64, Ordering::Relaxed);
                        STAT_PIXELS.fetch_add(area, Ordering::Relaxed);
                    }
                    o
                };
                match outcome {
                    Outcome::Done => {
                        let seq = mgi::signal_vblank();
                        let time_ns = monotonic_ns(time::now::<Monotonic>());
                        *PRESENTED.lock() = Presentation { seq, time_ns, gen };
                        VSYNC_WQ.notify_all();
                        if let Some(hook) = PRESENT_HOOK.get() {
                            hook();
                        }
                        STAT_FRAMES.fetch_add(1, Ordering::Relaxed);
                        next_vblank = advance(next_vblank, now);
                        NEXT_FRAME_NS.store(monotonic_ns(next_vblank), Ordering::Release);
                        composed = true;
                    }
                    Outcome::Busy => {
                        STAT_BUSY.fetch_add(1, Ordering::Relaxed);
                        deadline = earliest(deadline, now + BUSY_RETRY);
                    }
                }
            } else {
                deadline = earliest(deadline, next_vblank);
            }
        }

        // Plan du curseur : sans attendre l'horloge d'images.
        if !composed && !exclusive && c.cursor_only(compose::cursor_position()) {
            STAT_CURSOR.fetch_add(1, Ordering::Relaxed);
        }

        // Sortie GPU : ce qui vient d'être écrit dans le tampon avant part en une rafale.
        c.flush_output();

        if more_input || composed {
            continue;
        }
        DOORBELL.wait(deadline);
    }
}

/// Traite les entrées en attente. Ne fait jamais échouer la boucle : une erreur est journalisée
/// (l'ancienne boucle se terminait sur la première erreur, et l'écran restait figé).
/// Renvoie `true` s'il reste probablement des événements.
fn pump_input(keys: &Queue<Event>, mice: &Queue<Event>) -> bool {
    let mut more = false;
    let mut n = 0;
    while let Some(ev) = keys.pop() {
        if let Event::KeyboardEvent(ref e) = ev {
            if let Err(err) = handle_keyboard(e.key_event) {
                warn!("compositeur : raccourci clavier : {}", err);
            }
        }
        n += 1;
        if n >= INPUT_BATCH {
            more = true;
            break;
        }
    }

    keyboard::process_deferred_led_update();

    // Les déplacements purs sont cumulés ; un changement de bouton ou un cran de molette est
    // livré aussitôt, à la position atteinte, avec l'état exact des boutons (aucun clic bref
    // perdu, aucun bouton « collé »). Un pointeur absolu (tablette) remplace la position
    // visée ; un déplacement relatif qui le suit s'y ajoute.
    let mut motion = Motion::Rel(0, 0);
    let mut last: Option<MouseEvent> = None;
    let mut n = 0;
    while let Some(ev) = mice.pop() {
        if let Event::MouseMovementEvent(ref m) = ev {
            motion = match (m.absolute, motion) {
                (Some(a), _) => {
                    let (sw, sh) = screen_size();
                    let (x, y) = a.to_screen(sw, sh);
                    Motion::Abs(x, y)
                }
                (None, Motion::Rel(dx, dy)) => Motion::Rel(dx + m.movement.x_movement as isize, dy + m.movement.y_movement as isize),
                // Relatif après absolu : `y_movement` positif vers le haut, l'écran vers le bas.
                (None, Motion::Abs(x, y)) => Motion::Abs(x + m.movement.x_movement as isize, y - m.movement.y_movement as isize),
            };
            let changed = last.as_ref().map_or(true, |l| {
                l.buttons.left() != m.buttons.left()
                    || l.buttons.right() != m.buttons.right()
                    || l.buttons.middle() != m.buttons.middle()
                    || l.buttons.fourth() != m.buttons.fourth()
                    || l.buttons.fifth() != m.buttons.fifth()
            });
            if changed || m.movement.scroll_movement != 0 {
                deliver_mouse(motion, m);
                motion = Motion::Rel(0, 0);
            }
            last = Some(m.clone());
        }
        n += 1;
        if n >= INPUT_BATCH {
            more = true;
            break;
        }
    }
    if let Some(m) = last {
        if motion != Motion::Rel(0, 0) {
            deliver_mouse(motion, &m);
        }
    }
    more
}

/// Mouvement du pointeur cumulé entre deux livraisons.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Motion {
    /// Déplacement relatif (souris : `dy` positif vers le haut).
    Rel(isize, isize),
    /// Point visé de l'écran (tablette).
    Abs(isize, isize),
}

fn deliver_mouse(motion: Motion, m: &MouseEvent) {
    if let Some(wm) = WINDOW_MANAGER.get() {
        let mut wm = wm.lock();
        wm.mouse_btn_left = m.buttons.left();
        wm.mouse_btn_right = m.buttons.right();
        match motion {
            Motion::Rel(0, 0) => {}
            Motion::Rel(dx, dy) => wm.move_mouse(Coord::new(dx, -dy)),
            Motion::Abs(x, y) => {
                let cur = wm.mouse;
                wm.move_mouse(Coord::new(x - cur.x, y - cur.y));
            }
        }
        let s = m.movement.scroll_movement;
        let ev = MousePositionEvent {
            coordinate: Coord::new(0, 0),
            gcoordinate: wm.mouse,
            scrolling_up: s > 0,
            scrolling_down: s < 0,
            left_button_hold: m.buttons.left(),
            right_button_hold: m.buttons.right(),
            fourth_button_hold: m.buttons.fourth(),
            fifth_button_hold: m.buttons.fifth(),
            middle_button_hold: m.buttons.middle(),
            scroll: s,
        };
        // File d'une fenêtre pleine : l'événement est perdu pour elle, rien de plus.
        let _ = wm.route_mouse(&ev);
    }
}

fn handle_keyboard(key: KeyEvent) -> Result<(), &'static str> {
    track_key(&key);
    // File globale pour SYS_GET_EVENT (Doom).
    let ascii_val = key.keycode.to_ascii(key.modifiers).map(|c| c as u32).unwrap_or(0);
    let mut mods = 0u32;
    if key.modifiers.is_shift() {
        mods |= 1;
    }
    if key.modifiers.is_control() {
        mods |= 2;
    }
    if key.modifiers.is_alt() {
        mods |= 4;
    }
    mgi::push_key_event(key.keycode as u32, key.action == KeyAction::Pressed, ascii_val, mods);

    let wm_ref = WINDOW_MANAGER.get().ok_or("window manager not initialised")?;

    // Super + flèches : ancrer la fenêtre active sur une moitié de l'écran.
    if key.modifiers.is_super_key() && key.action == KeyAction::Pressed {
        let work = wm_ref.lock().work_area();
        let (w, h) = (work.width() as isize, work.height() as isize);
        let snap = match key.keycode {
            Keycode::Left => Some(Rectangle { top_left: Coord::new(0, 0), bottom_right: Coord::new(w / 2, h) }),
            Keycode::Right => Some(Rectangle { top_left: Coord::new(w / 2, 0), bottom_right: Coord::new(w, h) }),
            Keycode::Up => Some(Rectangle { top_left: Coord::new(0, 0), bottom_right: Coord::new(w, h / 2) }),
            Keycode::Down => Some(Rectangle { top_left: Coord::new(0, h / 2), bottom_right: Coord::new(w, h) }),
            _ => None,
        };
        if let Some(rect) = snap {
            let active = wm_ref.lock().get_active_window();
            if let Some(active) = active {
                let old = active.lock().get_envelope();
                active.lock().resize(rect)?;
                damage(old);
                damage(rect);
            }
            return Ok(());
        }
    }

    // Ctrl+Maj+Espace : bascule QWERTY / AZERTY.
    if key.modifiers.is_control() && key.modifiers.is_shift() && key.keycode == Keycode::Space && key.action == KeyAction::Pressed {
        let new_layout = keycodes_ascii::toggle_layout();
        info!("Keyboard layout switched to: {}", new_layout.name());
        return Ok(());
    }

    // Ctrl+Alt+T : nouveau terminal.
    if key.modifiers.is_control() && key.modifiers.is_alt() && key.keycode == Keycode::T && key.action == KeyAction::Pressed {
        let ns = mod_mgmt::create_application_namespace(None)?;
        let file = ns.dir().get_file_starting_with("shell-").ok_or("shell application not found")?;
        let path = file.lock().get_absolute_path();
        spawn::new_application_task_builder(path.as_ref(), Some(ns))?.name("shell".to_string()).spawn()?;
        return Ok(());
    }

    if let Err(e) = wm_ref.lock().route_keyboard(key) {
        warn!("window_manager: could not route keyboard event: {}", e);
    }
    Ok(())
}
