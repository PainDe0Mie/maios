//! MGI — Mai Graphics Infrastructure
//!
//! # Pipeline
//! ```text
//!  DrawCommand::Blit/FillRect/…
//!       │
//!       ▼
//!  submit()  ─►  backbuffer (RAM)
//!               + TileMap.mark(rect)   ← O(1) par tuile
//!       │
//!       ▼
//!  present()  ─►  scan TileMap dirty bits
//!                 fusionne les spans contigus
//!                 memcpy(back → front) par span horizontal
//! ```
//!
//! # Optimisations clés
//! 1. **TileMap bitset** (64×64px par tuile) — suivi O(1), scan O(tuiles_sales)
//! 2. **Fast-path opaque** — memcpy pur quand alpha == 0 sur toute une ligne
//! 3. **Span merging dans present()** — tuiles contiguës → un seul `copy_nonoverlapping`
//! 4. **Clipping unifié** — une seule passe d'intersection avant toute écriture

#![no_std]

extern crate alloc;
extern crate color;
extern crate shapes;
extern crate framebuffer;
extern crate framebuffer_drawer;
#[macro_use]
extern crate log;
extern crate spin;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use color::Color;
use shapes::{Coord, Rectangle};
use framebuffer::{Framebuffer, AlphaPixel};
use spin::{Mutex, Once};
use core::sync::atomic::{AtomicU64, AtomicBool, AtomicUsize, Ordering};

// ────────────────────────────────────────────────────────────────────────────
// Instance globale
// ────────────────────────────────────────────────────────────────────────────

/// Instance globale de MGI — initialisée une seule fois par `init()`.
pub static MGI: Once<Mutex<Mgi>> = Once::new();

/// Initialise le sous-système graphique.
/// Doit être appelée avant tout accès à `MGI`.
pub fn init() -> Result<(), &'static str> {
    let fb       = framebuffer::init::<AlphaPixel>()?;
    let provider = Box::new(SoftwareGraphicsProvider::new(fb)?);
    MGI.call_once(|| Mutex::new(Mgi::new(provider)));
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────────
// VSync counter
// ────────────────────────────────────────────────────────────────────────────

/// Compteur VSync : une unité par image de l'horloge du compositeur (60 Hz quand quelqu'un
/// attend ou que l'écran change, arrêté sinon).
static VSYNC_FRAME: AtomicU64 = AtomicU64::new(0);

/// Returns the current VSync frame number.
/// Applications use this to wait for the next frame.
pub fn vsync_counter() -> u64 {
    VSYNC_FRAME.load(Ordering::Acquire)
}

/// Fait avancer le compteur VSync. Appelée par le compositeur à chaque battement de son
/// horloge d'images (une fois par image affichée), et par lui seul.
pub fn signal_vblank() -> u64 {
    VSYNC_FRAME.fetch_add(1, Ordering::AcqRel) + 1
}

// ────────────────────────────────────────────────────────────────────────────
// Backbuffer direct access (for SYS_MAP_FRAMEBUFFER)
// ────────────────────────────────────────────────────────────────────────────

/// Returns `(backbuffer_ptr, width, height)` for direct pixel access.
///
/// The pointer points to BGRA8888 pixels (4 bytes each) in a contiguous
/// row-major buffer. stride = width * 4.
pub fn get_backbuffer_info() -> Option<(usize, usize, usize)> {
    let mgi_ref = MGI.get()?;
    let guard = mgi_ref.lock();
    let (w, h) = guard.provider.resolution();
    // Get a raw pointer to the backbuffer data
    let ptr = guard.backbuffer_ptr();
    Some((ptr, w, h))
}

// ────────────────────────────────────────────────────────────────────────────
// Exclusive fullscreen mode
// ────────────────────────────────────────────────────────────────────────────

static EXCLUSIVE_FS: AtomicBool = AtomicBool::new(false);
static EXCLUSIVE_BUF_PTR: AtomicUsize = AtomicUsize::new(0);
static EXCLUSIVE_BUF_LEN: AtomicUsize = AtomicUsize::new(0);

/// Enables exclusive fullscreen mode with the given user buffer.
pub fn set_exclusive_fullscreen(buf_ptr: usize, buf_len: usize) {
    EXCLUSIVE_BUF_PTR.store(buf_ptr, Ordering::Release);
    EXCLUSIVE_BUF_LEN.store(buf_len, Ordering::Release);
    EXCLUSIVE_FS.store(true, Ordering::Release);
    info!("MGI: exclusive fullscreen enabled (buf={:#x}, len={})", buf_ptr, buf_len);
}

/// Disables exclusive fullscreen mode.
pub fn clear_exclusive_fullscreen() {
    EXCLUSIVE_FS.store(false, Ordering::Release);
    EXCLUSIVE_BUF_PTR.store(0, Ordering::Relaxed);
    EXCLUSIVE_BUF_LEN.store(0, Ordering::Relaxed);
    info!("MGI: exclusive fullscreen disabled");
}

/// Returns `true` if exclusive fullscreen mode is active.
pub fn is_exclusive_fullscreen() -> bool {
    EXCLUSIVE_FS.load(Ordering::Acquire)
}

/// Tampon du plein écran exclusif `(adresse, nombre de pixels)`, s'il est actif.
///
/// Le compositeur le recopie directement vers l'affichage à chaque image.
pub fn exclusive_buffer() -> Option<(usize, usize)> {
    if !is_exclusive_fullscreen() {
        return None;
    }
    let ptr = EXCLUSIVE_BUF_PTR.load(Ordering::Acquire);
    let len = EXCLUSIVE_BUF_LEN.load(Ordering::Acquire);
    if ptr == 0 || len == 0 { None } else { Some((ptr, len)) }
}

// ────────────────────────────────────────────────────────────────────────────
// Input event queue (for SYS_GET_EVENT)
// ────────────────────────────────────────────────────────────────────────────

/// A raw input event that can be read by C programs via SYS_GET_EVENT.
///
/// Layout: 16 bytes, matching the C struct `maios_event_t`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RawInputEvent {
    /// Event type: 1 = key press, 2 = key release, 0 = none
    pub event_type: u32,
    /// Keycode (scancode-based, matching Doom's key numbering)
    pub keycode: u32,
    /// ASCII character (0 if not printable)
    pub ascii: u32,
    /// Modifier flags (shift, ctrl, alt)
    pub modifiers: u32,
}

/// Global input event ring buffer.
const EVENT_QUEUE_SIZE: usize = 64;
static EVENT_QUEUE: Mutex<EventRing> = Mutex::new(EventRing::new());

struct EventRing {
    buf: [RawInputEvent; EVENT_QUEUE_SIZE],
    head: usize,
    tail: usize,
}

impl EventRing {
    const fn new() -> Self {
        Self {
            buf: [RawInputEvent { event_type: 0, keycode: 0, ascii: 0, modifiers: 0 }; EVENT_QUEUE_SIZE],
            head: 0,
            tail: 0,
        }
    }

    fn push(&mut self, ev: RawInputEvent) {
        let next = (self.head + 1) % EVENT_QUEUE_SIZE;
        if next == self.tail {
            // Queue full — drop oldest
            self.tail = (self.tail + 1) % EVENT_QUEUE_SIZE;
        }
        self.buf[self.head] = ev;
        self.head = next;
    }

    fn pop(&mut self) -> Option<RawInputEvent> {
        if self.head == self.tail {
            None
        } else {
            let ev = self.buf[self.tail];
            self.tail = (self.tail + 1) % EVENT_QUEUE_SIZE;
            Some(ev)
        }
    }
}

/// Push a keyboard event into the global input queue.
/// Called by the window manager / keyboard handler.
pub fn push_key_event(keycode: u32, pressed: bool, ascii: u32, modifiers: u32) {
    EVENT_QUEUE.lock().push(RawInputEvent {
        event_type: if pressed { 1 } else { 2 },
        keycode,
        ascii,
        modifiers,
    });
}

/// Pop the next input event, or return None if the queue is empty.
pub fn pop_input_event() -> Option<RawInputEvent> {
    EVENT_QUEUE.lock().pop()
}

// ────────────────────────────────────────────────────────────────────────────
// DrawCommand — interface publique inchangée
// ────────────────────────────────────────────────────────────────────────────

/// Commandes de dessin primitives soumises au backend graphique.
///
/// Les commandes `Blit` et `BlitRegion` empruntent les données source
/// avec une durée de vie `'a` — le caller doit garantir que les pixels
/// restent valides jusqu'à la fin de `submit()`.
#[derive(Debug, Clone)]
pub enum DrawCommand<'a> {
    /// Remplit un rectangle d'une couleur unie.
    FillRect { rect: Rectangle, color: Color },
    /// Dessine le contour d'un rectangle.
    DrawRect { rect: Rectangle, color: Color },
    /// Dessine un segment.
    DrawLine { start: Coord, end: Coord, color: Color },
    /// Dessine un cercle (contour).
    DrawCircle { center: Coord, radius: usize, color: Color },
    /// Copie un framebuffer entier vers l'écran.
    Blit {
        src:          &'a [AlphaPixel],
        src_width:    usize,
        dest_top_left: Coord,
    },
    /// Copie une zone rectangulaire d'un framebuffer vers l'écran.
    BlitRegion {
        src:          &'a [AlphaPixel],
        src_width:    usize,
        dest_top_left: Coord,
        /// Zone en coordonnées écran absolues.
        region:        Rectangle,
    },
}

// ────────────────────────────────────────────────────────────────────────────
// GraphicsProvider trait
// ────────────────────────────────────────────────────────────────────────────

/// Accès direct aux deux tampons, pour le compositeur (seul à écrire dans le tampon arrière
/// pendant une image) : `back` en RAM, `front` projeté sur la mémoire vidéo.
pub struct Surfaces<'a> {
    pub back:   &'a mut [AlphaPixel],
    pub front:  &'a mut [AlphaPixel],
    pub width:  usize,
    pub height: usize,
}

/// Trait implémenté par tous les backends graphiques (soft, VirtIO, GPU…).
pub trait GraphicsProvider {
    /// Les deux tampons à la fois (emprunts disjoints).
    fn surfaces(&mut self) -> Surfaces<'_>;
    /// Exécute les commandes sur le backbuffer et met à jour le dirty tracking.
    fn submit<'a>(&mut self, commands: &[DrawCommand<'a>]);
    /// Copie les zones sales du backbuffer vers la VRAM/frontbuffer.
    fn present(&mut self);
    /// Résolution courante de l'écran en pixels.
    fn resolution(&self) -> (usize, usize);
    /// Invalide la totalité de l'écran (force un repaint complet).
    fn invalidate_all(&mut self);
    /// Returns raw pointer to backbuffer pixel data (immutable).
    fn backbuffer_raw_ptr(&self) -> usize;
    /// Returns raw pointer to backbuffer pixel data (mutable).
    fn backbuffer_raw_ptr_mut(&mut self) -> usize;
    /// Returns raw pointer to frontbuffer pixel data (immutable).
    fn frontbuffer_raw_ptr(&self) -> usize;
    /// Remplace le tampon avant (même taille) et rend l'ancien.
    fn replace_frontbuffer(&mut self, fb: Framebuffer<AlphaPixel>) -> Result<Framebuffer<AlphaPixel>, Framebuffer<AlphaPixel>>;
}

// ────────────────────────────────────────────────────────────────────────────
// Scanout — sortie d'affichage branchée à chaud (GPU)
// ────────────────────────────────────────────────────────────────────────────

/// Rectangle de l'écran à présenter, en pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanoutRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Image d'un curseur matériel : ARGB 8888 **non prémultiplié** (alpha 255 = opaque, l'inverse
/// de MaiOS), `width × height` pixels ligne par ligne, point chaud dans l'image.
pub struct CursorPlane<'a> {
    pub argb: &'a [u32],
    pub width: usize,
    pub height: usize,
    pub hot_x: u32,
    pub hot_y: u32,
}

/// État d'un GPU pour les outils (gestionnaire des tâches) : identité et compteurs **cumulés**
/// depuis le démarrage ; l'outil en fait des débits par différence entre deux lectures.
#[derive(Clone, Debug, Default)]
pub struct GpuReport {
    /// Carte (« virtio-gpu »), avec l'écran annoncé par l'EDID s'il y en a un.
    pub adapter: String,
    /// Pilote et ce qu'il sait faire (« virtio_gpu, 2D »).
    pub driver: String,
    /// Emplacement sur le bus (« PCI 00:02.0 »).
    pub location: String,
    /// Fonctions négociées (« EDID, EVENT_IDX »).
    pub features: String,
    /// Mode du scanout.
    pub width: u32,
    pub height: u32,
    /// Mémoire des ressources de l'appareil (tampons d'image, curseur), en octets.
    pub memory_bytes: u64,
    /// Temps où l'appareil avait au moins une commande d'affichage en cours (ns) : base de
    /// l'utilisation, comme le temps actif d'un moteur dans le gestionnaire des tâches de
    /// Windows (WDDM) ou `drm-engine-*` de `fdinfo` sous Linux.
    pub busy_ns: u64,
    /// Présentations, octets transférés vers l'appareil.
    pub presents: u64,
    pub transfer_bytes: u64,
    /// Somme et nombre des latences « soumission → fence » des présentations, et maximum.
    pub present_lat_total_ns: u64,
    pub present_lat_count: u64,
    pub present_lat_max_ns: u64,
    /// Commandes, erreurs, notifications de l'appareil, interruptions reçues.
    pub commands: u64,
    pub errors: u64,
    pub notifies: u64,
    pub interrupts: u64,
    /// Curseur matériel présent, déplacements du curseur.
    pub hw_cursor: bool,
    pub cursor_moves: u64,
}

/// Une sortie d'affichage pilotée par un GPU (virtio-gpu…) : MGI prend sa mémoire comme
/// tampon avant ([`Mgi::attach_scanout`]), et le compositeur lui dit quelles zones il a
/// changées.
///
/// Contrat : le compositeur appelle [`Scanout::wait_idle`] avant d'écrire dans le tampon avant
/// (l'appareil peut encore lire la présentation précédente), puis [`Scanout::present`] avec
/// les rectangles écrits. `present` ne bloque pas.
pub trait Scanout: Send + Sync {
    /// Nom pour les journaux (« virtio-gpu »).
    fn name(&self) -> &str;
    /// Taille du scanout en pixels.
    fn size(&self) -> (usize, usize);
    /// La mémoire du scanout comme tampon (B8G8R8X8, une seule fois). Elle doit vivre tant que
    /// la sortie est branchée : MGI la garde pour toujours.
    fn take_framebuffer(&self) -> Option<Framebuffer<AlphaPixel>>;
    /// Envoie les rectangles à l'affichage (asynchrone).
    fn present(&self, rects: &[ScanoutRect]);
    /// Attend que l'appareil ait fini de lire le tampon avant.
    fn wait_idle(&self);
    /// Vrai si aucune présentation n'est en cours.
    fn is_idle(&self) -> bool;
    /// Côté du curseur matériel en pixels (0 : pas de curseur matériel).
    fn cursor_size(&self) -> usize;
    /// Pose l'image du curseur matériel (`None` : caché) au point `(x, y)` de l'écran (le
    /// point chaud y est placé).
    fn set_cursor(&self, image: Option<&CursorPlane<'_>>, x: i32, y: i32);
    /// Déplace le curseur matériel (point chaud en `(x, y)`).
    fn move_cursor(&self, x: i32, y: i32);
    /// Identité et compteurs du GPU (sans verrou bloquant : lu par les applications).
    fn report(&self) -> Option<GpuReport> {
        None
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Mgi — façade publique
// ────────────────────────────────────────────────────────────────────────────

/// Point d'entrée principal de MGI.
pub struct Mgi {
    provider: Box<dyn GraphicsProvider + Send>,
    scanout: Option<Arc<dyn Scanout>>,
}

impl Mgi {
    pub fn new(provider: Box<dyn GraphicsProvider + Send>) -> Self {
        Self { provider, scanout: None }
    }

    /// Branche une sortie GPU : sa mémoire devient le tampon avant (même résolution exigée).
    /// L'ancien tampon avant (le framebuffer du chargeur) est rendu. L'appelant recompose
    /// ensuite tout l'écran.
    pub fn attach_scanout(&mut self, out: Arc<dyn Scanout>) -> Result<Framebuffer<AlphaPixel>, &'static str> {
        if out.size() != self.provider.resolution() {
            return Err("MGI : le scanout n'a pas la résolution de l'écran");
        }
        let fb = out.take_framebuffer().ok_or("MGI : tampon du scanout déjà pris")?;
        if fb.get_size() != self.provider.resolution() {
            return Err("MGI : tampon du scanout de mauvaise taille");
        }
        let old = self.provider.replace_frontbuffer(fb).map_err(|_| "MGI : tampon avant refusé")?;
        info!("MGI : sortie {} branchée", out.name());
        self.scanout = Some(out);
        Ok(old)
    }

    /// Sortie GPU branchée, s'il y en a une.
    pub fn scanout(&self) -> Option<&Arc<dyn Scanout>> {
        self.scanout.as_ref()
    }

    /// Soumet une liste de commandes de dessin.
    #[inline]
    pub fn submit<'a>(&mut self, commands: &[DrawCommand<'a>]) {
        self.provider.submit(commands);
    }

    /// Présente le backbuffer (copie vers la VRAM).
    #[inline]
    pub fn present(&mut self) {
        self.provider.present();
    }

    /// Résolution courante `(largeur, hauteur)`.
    #[inline]
    pub fn resolution(&self) -> (usize, usize) {
        self.provider.resolution()
    }

    /// Force le repaint de tout l'écran lors du prochain `present()`.
    #[inline]
    pub fn invalidate_all(&mut self) {
        self.provider.invalidate_all();
    }

    /// Les deux tampons, pour le compositeur.
    #[inline]
    pub fn surfaces(&mut self) -> Surfaces<'_> {
        self.provider.surfaces()
    }

    /// Returns a raw pointer to the backbuffer pixel data (for SYS_MAP_FRAMEBUFFER).
    pub fn backbuffer_ptr(&self) -> usize {
        self.provider.backbuffer_raw_ptr()
    }

    /// Returns a mutable raw pointer to the backbuffer pixel data.
    pub fn backbuffer_ptr_mut(&mut self) -> usize {
        self.provider.backbuffer_raw_ptr_mut()
    }

    /// Returns a raw pointer to the frontbuffer pixel data (for VirtIO-GPU flush).
    pub fn frontbuffer_ptr(&self) -> usize {
        self.provider.frontbuffer_raw_ptr()
    }
}

// ────────────────────────────────────────────────────────────────────────────
// TileMap — suivi des zones sales en O(1) / O(tuiles)
// ────────────────────────────────────────────────────────────────────────────
//
// L'écran est découpé en tuiles de TILE_SIZE×TILE_SIZE pixels.
// Chaque tuile est représentée par un bit dans un tableau de mots de 64 bits.
//
// Paramètres par défaut :
//   TILE_SIZE = 64px
//   TILE_WORDS = 2  → max 128 tuiles larges → max 8192px de large
//   MAX_TILE_ROWS = 40 → max 40 tuiles hautes → max 2560px de haut
//
// Pour 1920×1080 : 30×17 = 510 tuiles actives.
// Pour 3840×2160 : 60×34 = 2040 tuiles actives (tient dans 32 u64).

/// Taille d'une tuile en pixels (doit être une puissance de 2).
const TILE_SHIFT: usize = 6;
const TILE_SIZE:  usize = 1 << TILE_SHIFT; // 64
const TILE_MASK:  usize = TILE_SIZE - 1;

/// Nombre de mots u64 par rangée de tuiles.
/// 2 × 64 = 128 tuiles colonnes → 8192px max de large.
const TILE_WORDS: usize = 2;

/// Nombre maximum de rangées de tuiles.
/// 40 × 64 = 2560px max de haut.
const MAX_TILE_ROWS: usize = 40;

/// Suivi des zones "sales" (modifiées depuis le dernier `present()`).
struct TileMap {
    /// `dirty[ty][word]` : bit tx%64 du mot tx/64 = tuile (tx, ty) sale.
    dirty:     [[u64; TILE_WORDS]; MAX_TILE_ROWS],
    /// Nombre de colonnes de tuiles pour la résolution courante.
    num_tcols: usize,
    /// Nombre de rangées de tuiles pour la résolution courante.
    num_trows: usize,
    /// Largeur de l'écran en pixels.
    screen_w:  usize,
    /// Hauteur de l'écran en pixels.
    screen_h:  usize,
}

impl TileMap {
    fn new(screen_w: usize, screen_h: usize) -> Self {
        let num_tcols = (screen_w + TILE_SIZE - 1) >> TILE_SHIFT;
        let num_trows = (screen_h + TILE_SIZE - 1) >> TILE_SHIFT;
        assert!(
            num_tcols <= TILE_WORDS * 64,
            "MGI TileMap: écran trop large ({} > {} tuiles)",
            num_tcols, TILE_WORDS * 64
        );
        assert!(
            num_trows <= MAX_TILE_ROWS,
            "MGI TileMap: écran trop haut ({} > {} tuiles)",
            num_trows, MAX_TILE_ROWS
        );
        Self {
            dirty: [[0u64; TILE_WORDS]; MAX_TILE_ROWS],
            num_tcols,
            num_trows,
            screen_w,
            screen_h,
        }
    }

    /// Marque toutes les tuiles qui intersectent `rect` comme sales.
    ///
    /// Complexité : O(tuiles intersectées) — en pratique O(1) pour une fenêtre.
    #[inline]
    fn mark(&mut self, rect: Rectangle) {
        let x0 = rect.top_left.x.max(0) as usize;
        let y0 = rect.top_left.y.max(0) as usize;
        let x1 = (rect.bottom_right.x as usize).min(self.screen_w);
        let y1 = (rect.bottom_right.y as usize).min(self.screen_h);
        if x0 >= x1 || y0 >= y1 { return; }

        let tx0 = x0 >> TILE_SHIFT;
        let ty0 = y0 >> TILE_SHIFT;
        // Les tuiles couvrent [tx0*64, tx1*64), on arrondit vers le haut
        let tx1 = ((x1 + TILE_MASK) >> TILE_SHIFT).min(self.num_tcols);
        let ty1 = ((y1 + TILE_MASK) >> TILE_SHIFT).min(self.num_trows);

        for ty in ty0..ty1 {
            for tx in tx0..tx1 {
                // Mot : tx / 64, bit : tx % 64
                self.dirty[ty][tx >> 6] |= 1u64 << (tx & 63);
            }
        }
    }

    /// Invalide la totalité de l'écran.
    #[inline]
    fn mark_all(&mut self) {
        for ty in 0..self.num_trows {
            let full_words = self.num_tcols / 64;
            let rem_bits   = self.num_tcols % 64;
            for w in 0..full_words  { self.dirty[ty][w] = !0u64; }
            if rem_bits > 0         { self.dirty[ty][full_words] = (1u64 << rem_bits) - 1; }
        }
    }

    /// Remet tous les bits à zéro après un `present()`.
    #[inline]
    fn clear(&mut self) {
        for row in &mut self.dirty {
            for w in row.iter_mut() { *w = 0; }
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Utilitaires de blit
// ────────────────────────────────────────────────────────────────────────────

/// Vérifie si une ligne de pixels est entièrement opaque (alpha == 0 dans le
/// modèle Mai OS où 0 = opaque et 255 = transparent).
///
/// Utilise `all()` avec short-circuit : coût minimal si le premier pixel
/// n'est pas opaque.
#[inline]
fn row_is_opaque(row: &[AlphaPixel]) -> bool {
    row.iter().all(|p| p.alpha == 0)
}

/// Copie une ligne source dans le backbuffer.
///
/// Fast-path : si tous les pixels sont opaques → `copy_from_slice` (memcpy).
/// Slow-path : sinon → `composite_buffer` (alpha blend).
///
/// Le fast-path est ~3–5× plus rapide car il évite les multiplications
/// de l'alpha blend. Il est déclenché pour toutes les fenêtres non transparentes
/// (cas de loin le plus courant).
#[inline]
fn blit_row_into(
    dst_buf: &mut [AlphaPixel],
    dst_offset: usize,
    src: &[AlphaPixel],
    fb: &mut Framebuffer<AlphaPixel>,
) {
    if row_is_opaque(src) {
        dst_buf[dst_offset..dst_offset + src.len()].copy_from_slice(src);
    } else {
        fb.composite_buffer(src, dst_offset);
    }
}

/// Calcule l'intersection clippée entre un blit source et l'écran.
/// Retourne `(x0, y0, x1, y1)` ou `None` si entièrement hors-écran.
#[inline]
fn clip_source(
    dest:   Coord,
    src_w:  usize,
    src_h:  usize,
    fb_w:   usize,
    fb_h:   usize,
) -> Option<(usize, usize, usize, usize)> {
    if dest.x >= fb_w as isize || dest.y >= fb_h as isize { return None; }
    let x0 = dest.x.max(0) as usize;
    let y0 = dest.y.max(0) as usize;
    let x1 = (dest.x + src_w as isize).min(fb_w as isize);
    let y1 = (dest.y + src_h as isize).min(fb_h as isize);
    if x1 <= 0 || y1 <= 0 { return None; }
    Some((x0, y0, x1 as usize, y1 as usize))
}

// ────────────────────────────────────────────────────────────────────────────
// SoftwareGraphicsProvider
// ────────────────────────────────────────────────────────────────────────────

/// Backend graphique logiciel (CPU).
///
/// Toute la composition se fait dans `backbuffer` (RAM).
/// `present()` copie uniquement les tuiles sales vers `frontbuffer` (VRAM).
pub struct SoftwareGraphicsProvider {
    /// Buffer de composition en RAM.
    backbuffer:  Framebuffer<AlphaPixel>,
    /// Framebuffer mappé sur la VRAM (ou le framebuffer UEFI/VESA).
    frontbuffer: Framebuffer<AlphaPixel>,
    /// Suivi des tuiles modifiées depuis le dernier `present()`.
    tilemap:     TileMap,
}

impl SoftwareGraphicsProvider {
    pub fn new(frontbuffer: Framebuffer<AlphaPixel>) -> Result<Self, &'static str> {
        let (w, h) = frontbuffer.get_size();
        let backbuffer = Framebuffer::new(w, h, None)?;
        let tilemap    = TileMap::new(w, h);
        info!("MGI Software: {}×{}, {} tuiles ({}×{})",
            w, h,
            tilemap.num_tcols * tilemap.num_trows,
            tilemap.num_tcols, tilemap.num_trows);
        Ok(Self { backbuffer, frontbuffer, tilemap })
    }

    // ── Primitives internes ───────────────────────────────────────────────

    fn cmd_fill_rect(&mut self, rect: Rectangle, color: Color) {
        framebuffer_drawer::fill_rectangle(
            &mut self.backbuffer,
            rect.top_left, rect.width(), rect.height(),
            color.into(),
        );
        self.tilemap.mark(rect);
    }

    fn cmd_draw_rect(&mut self, rect: Rectangle, color: Color) {
        framebuffer_drawer::draw_rectangle(
            &mut self.backbuffer,
            rect.top_left, rect.width(), rect.height(),
            color.into(),
        );
        self.tilemap.mark(rect);
    }

    fn cmd_draw_line(&mut self, start: Coord, end: Coord, color: Color) {
        framebuffer_drawer::draw_line(&mut self.backbuffer, start, end, color.into());
        self.tilemap.mark(Rectangle {
            top_left:     Coord::new(start.x.min(end.x),     start.y.min(end.y)),
            bottom_right: Coord::new(start.x.max(end.x) + 1, start.y.max(end.y) + 1),
        });
    }

    fn cmd_draw_circle(&mut self, center: Coord, radius: usize, color: Color) {
        framebuffer_drawer::draw_circle(&mut self.backbuffer, center, radius, color.into());
        let r = radius as isize;
        self.tilemap.mark(Rectangle {
            top_left:     center - (r,     r),
            bottom_right: center + (r + 1, r + 1),
        });
    }

    /// Blit complet : copie le framebuffer source en entier vers le backbuffer.
    fn cmd_blit(&mut self, src: &[AlphaPixel], src_width: usize, dest: Coord) {
        if src_width == 0 || src.is_empty() { return; }
        let src_height   = src.len() / src_width;
        let (fb_w, fb_h) = self.backbuffer.get_size();

        let (x0, y0, x1, y1) = match clip_source(dest, src_width, src_height, fb_w, fb_h) {
            Some(c) => c,
            None    => return,
        };
        let copy_w  = x1 - x0;
        let src_x   = (x0 as isize - dest.x) as usize;

        for y in y0..y1 {
            let src_y   = (y as isize - dest.y) as usize;
            let src_off = src_y * src_width + src_x;
            let dst_off = y * fb_w + x0;

            if src_off + copy_w > src.len() { break; }
            let src_row = &src[src_off..src_off + copy_w];

            // Fast-path opaque ou alpha blend
            if row_is_opaque(src_row) {
                self.backbuffer.buffer_mut()[dst_off..dst_off + copy_w]
                    .copy_from_slice(src_row);
            } else {
                self.backbuffer.composite_buffer(src_row, dst_off);
            }
        }

        self.tilemap.mark(Rectangle {
            top_left:     dest,
            bottom_right: dest + (src_width as isize, src_height as isize),
        });
    }

    /// Blit partiel : copie uniquement la zone `region` (coords écran absolues).
    fn cmd_blit_region(
        &mut self,
        src:       &[AlphaPixel],
        src_width: usize,
        dest:      Coord,
        region:    Rectangle,
    ) {
        if src_width == 0 || src.is_empty() { return; }
        let src_height   = src.len() / src_width;
        let (fb_w, fb_h) = self.backbuffer.get_size();

        // Triple intersection : region ∩ [dest, dest+src) ∩ [0, screen)
        let rx0 = region.top_left.x
            .max(dest.x)
            .max(0) as usize;
        let ry0 = region.top_left.y
            .max(dest.y)
            .max(0) as usize;
        let rx1 = region.bottom_right.x
            .min(dest.x + src_width as isize)
            .min(fb_w as isize);
        let ry1 = region.bottom_right.y
            .min(dest.y + src_height as isize)
            .min(fb_h as isize);

        if rx1 <= 0 || ry1 <= 0 || rx0 >= fb_w || ry0 >= fb_h { return; }
        let (rx1, ry1) = (rx1 as usize, ry1 as usize);
        if rx0 >= rx1 || ry0 >= ry1 { return; }

        let copy_w = rx1 - rx0;
        let src_x  = rx0 as isize - dest.x;
        if src_x < 0 { return; }
        let src_x = src_x as usize;

        for y in ry0..ry1 {
            let src_y   = (y as isize - dest.y) as usize;
            let src_off = src_y * src_width + src_x;
            let dst_off = y * fb_w + rx0;

            if src_off + copy_w > src.len() { break; }
            let src_row = &src[src_off..src_off + copy_w];

            if row_is_opaque(src_row) {
                self.backbuffer.buffer_mut()[dst_off..dst_off + copy_w]
                    .copy_from_slice(src_row);
            } else {
                self.backbuffer.composite_buffer(src_row, dst_off);
            }
        }

        self.tilemap.mark(region);
    }
}

impl GraphicsProvider for SoftwareGraphicsProvider {
    fn surfaces(&mut self) -> Surfaces<'_> {
        let (width, height) = self.frontbuffer.get_size();
        Surfaces {
            back: self.backbuffer.buffer_mut(),
            front: self.frontbuffer.buffer_mut(),
            width,
            height,
        }
    }

    fn submit<'a>(&mut self, commands: &[DrawCommand<'a>]) {
        for cmd in commands {
            match cmd {
                DrawCommand::FillRect   { rect, color }           => self.cmd_fill_rect(*rect, *color),
                DrawCommand::DrawRect   { rect, color }           => self.cmd_draw_rect(*rect, *color),
                DrawCommand::DrawLine   { start, end, color }     => self.cmd_draw_line(*start, *end, *color),
                DrawCommand::DrawCircle { center, radius, color } => self.cmd_draw_circle(*center, *radius, *color),
                DrawCommand::Blit { src, src_width, dest_top_left } =>
                    self.cmd_blit(src, *src_width, *dest_top_left),
                DrawCommand::BlitRegion { src, src_width, dest_top_left, region } =>
                    self.cmd_blit_region(src, *src_width, *dest_top_left, *region),
            }
        }
    }

    /// Copie les zones sales du backbuffer vers le frontbuffer (VRAM).
    ///
    /// # Algorithme
    ///
    /// Pour chaque rangée de tuiles ty :
    ///   Scan du bitset → trouve les spans de tuiles contiguës sales.
    ///   Pour chaque span [tx_start, tx_end) :
    ///     Pour chaque ligne de pixels y dans [ty×64, (ty+1)×64) :
    ///       `copy_nonoverlapping(back[y*W+x_start], front[y*W+x_start], copy_w)`
    ///
    /// Le span merging réduit le nombre d'appels à `copy_nonoverlapping`
    /// de O(tuiles) à O(spans) — en pratique ~2–4× moins d'appels.
    fn present(&mut self) {
        let (fb_w, fb_h) = self.frontbuffer.get_size();
        let total        = fb_w * fb_h;

        // Accès bruts pour éviter le double emprunt immutable/mutable.
        let back_ptr  = self.backbuffer.buffer().as_ptr();
        let front_ptr = self.frontbuffer.buffer_mut().as_mut_ptr();

        let num_trows = self.tilemap.num_trows;
        let num_tcols = self.tilemap.num_tcols;

        for ty in 0..num_trows {
            let y_start = ty * TILE_SIZE;
            let y_end   = (y_start + TILE_SIZE).min(fb_h);

            // Itère sur les mots u64 de la rangée ty
            for w in 0..TILE_WORDS {
                let mut bits = self.tilemap.dirty[ty][w];
                if bits == 0 { continue; }

                // Extraire les spans de bits consécutifs à 1 (tuiles sales contiguës)
                while bits != 0 {
                    // Premier bit à 1 dans ce mot → début du span
                    let lsb      = bits.trailing_zeros() as usize;
                    // Compter les bits consécutifs à 1 à partir de lsb
                    let span_len = (bits >> lsb).trailing_ones() as usize;

                    let tx_start = (w * 64 + lsb).min(num_tcols);
                    let tx_end   = (tx_start + span_len).min(num_tcols);

                    let x_start = tx_start * TILE_SIZE;
                    let x_end   = (tx_end * TILE_SIZE).min(fb_w);
                    let copy_w  = x_end.saturating_sub(x_start);

                    if copy_w > 0 {
                        for y in y_start..y_end {
                            let offset = y * fb_w + x_start;
                            // Vérification de bornes avant l'unsafe
                            if offset + copy_w > total { break; }
                            // SAFETY :
                            //   - `offset` et `offset+copy_w` sont dans les bornes (vérif ci-dessus)
                            //   - `backbuffer` et `frontbuffer` sont deux allocations distinctes
                            //     → pas d'aliasing possible
                            unsafe {
                                core::ptr::copy_nonoverlapping(
                                    back_ptr.add(offset),
                                    front_ptr.add(offset),
                                    copy_w,
                                );
                            }
                        }
                    }

                    // Effacer le span traité du masque
                    // Exemple : bits = 0b01110100, lsb=2, span_len=1 → mask=0b00000100
                    //           bits = 0b01110000 après
                    let span_mask = if span_len >= 64 {
                        !0u64
                    } else {
                        ((1u64 << span_len) - 1) << lsb
                    };
                    bits &= !span_mask;
                }
            }
        }

        // Remettre le tilemap à zéro pour la prochaine frame
        self.tilemap.clear();
        // Le compteur VSync n'avance plus ici : c'est l'horloge d'images du compositeur.
    }

    fn resolution(&self) -> (usize, usize) {
        self.frontbuffer.get_size()
    }

    fn invalidate_all(&mut self) {
        self.tilemap.mark_all();
    }

    fn backbuffer_raw_ptr(&self) -> usize {
        self.backbuffer.buffer().as_ptr() as usize
    }

    fn backbuffer_raw_ptr_mut(&mut self) -> usize {
        self.backbuffer.buffer_mut().as_mut_ptr() as usize
    }

    fn frontbuffer_raw_ptr(&self) -> usize {
        self.frontbuffer.buffer().as_ptr() as usize
    }

    fn replace_frontbuffer(&mut self, fb: Framebuffer<AlphaPixel>) -> Result<Framebuffer<AlphaPixel>, Framebuffer<AlphaPixel>> {
        if fb.get_size() != self.frontbuffer.get_size() {
            return Err(fb);
        }
        Ok(core::mem::replace(&mut self.frontbuffer, fb))
    }
}