//! Moteur de composition de MaiCompositor.
//!
//! Une seule tâche compose ; les applications ne font que déclarer des zones abîmées
//! ([`crate::damage`]). Une image se fait en quatre temps, sous le verrou de MGI :
//!
//! 1. **Instantané** : sous le verrou du gestionnaire (bref), la pile des fenêtres de l'avant
//!    vers l'arrière, le contour d'aperçu et la position du curseur ; puis `try_lock` de chaque
//!    fenêtre et du bureau. Le compositeur n'attend **jamais** le verrou d'une fenêtre : si une
//!    application dessine, l'image est remise un peu plus tard avec la même zone abîmée (pas
//!    d'inversion de priorité, pas d'interblocage avec l'ordre gestionnaire → fenêtre).
//! 2. **Opaque, de l'avant vers l'arrière** : pour chaque fenêtre, la partie encore à peindre de
//!    sa zone opaque (la fenêtre moins ses coins arrondis) est copiée en bloc, puis retirée de ce
//!    qui reste. Chaque pixel opaque n'est écrit qu'une fois, quel que soit le nombre de
//!    fenêtres empilées (rendu « front-to-back » avec élimination des surfaces cachées, comme
//!    le test de profondeur précoce des GPU ou l'*opaque region* de Wayland). Le bureau remplit
//!    ce qui reste.
//! 3. **Translucide, de l'arrière vers l'avant** : les coins arrondis (et les fenêtres
//!    déclarées translucides) sont mélangés par-dessus, dans l'ordre du peintre, seulement sur
//!    leurs pixels visibles. Puis le contour d'aperçu, limité à ses quatre bandes.
//!
//!    Le contenu d'une fenêtre en **composition directe** ([`window_inner::DirectContent`],
//!    fenêtres Wayland) est lu au moment de composer dans le tampon de l'application, converti
//!    en place dans le tampon arrière : aucune copie intermédiaire dans le tampon de la fenêtre.
//! 4. **Présentation** : seuls les rectangles abîmés passent du tampon arrière à la mémoire
//!    vidéo (lignes contiguës, écriture combinée). Le curseur est un **plan à part** : il n'est
//!    jamais dans le tampon arrière, il est écrit directement à l'affichage, mélangé au tampon
//!    arrière (sauvegarde du fond implicite : le tampon arrière *est* le fond). Bouger la souris
//!    coûte donc deux petits rectangles, sans recomposer aucune couche.
//!
//! **Sortie GPU** (MaiGPU, [`mgi::Scanout`]) : le tampon avant est alors la mémoire du scanout,
//! que l'appareil lit. Avant d'y écrire, on attend la fin de la présentation précédente
//! (`front_write`) ; chaque rectangle écrit est noté, et la boucle les envoie en une rafale
//! ([`Compositor::flush_output`] : transferts des seules zones écrites, puis un *flush* avec
//! fence). Le curseur passe sur le **plan matériel** quand son image tient dans le côté du
//! curseur de la sortie : le bouger n'écrit plus rien dans le tampon avant.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use framebuffer::AlphaPixel;
use mgi::{CursorPlane, Scanout, ScanoutRect};
use region2d::{Rect, Region};
use spin::{Mutex, MutexGuard};
use window_inner::WindowInner;

/// Peint la partie `r` (écran) de la fenêtre `g` placée en `(ox, oy)`, copiée ou mélangée
/// (`blend`) : le contenu direct est lu dans le tampon de l'application, le reste (décorations,
/// coins arrondis du bas) dans celui de la fenêtre.
#[allow(clippy::too_many_arguments)]
fn put_window(back: &mut [AlphaPixel], w: usize, g: &WindowInner, ox: i32, oy: i32, r: &Rect, blend: bool, row: &mut Vec<AlphaPixel>) {
    let fb = g.framebuffer();
    let (src, src_w) = (fb.buffer(), fb.width());
    let Some(d) = g.direct.as_ref() else {
        pixels::put_rect(back, w, src, src_w, ox, oy, r, blend);
        return;
    };
    let (ww, wh) = g.get_size();
    let win = Rect::from_xywh(ox, oy, ww as i32, wh as i32);
    let content = Rect::from_xywh(ox + d.x as i32, oy + d.y as i32, d.width as i32, d.height as i32).intersect(&win);
    let mut direct = Region::from_rect(r.intersect(&content));
    // Coins arrondis du bas (côté `rayon + 1`, comme la décoration) : dans le tampon de la
    // fenêtre, où le propriétaire a recopié le contenu avant que la décoration les arrondisse.
    let k = g.corner_radius as i32 + 1;
    if k > 1 && !direct.is_empty() {
        direct.subtract(Rect::new(win.x0, win.y1 - k, win.x0 + k, win.y1));
        direct.subtract(Rect::new(win.x1 - k, win.y1 - k, win.x1, win.y1));
    }
    let mut rest = Region::from_rect(*r);
    for q in direct.iter() {
        rest.subtract(*q);
    }
    for q in rest.iter() {
        pixels::put_rect(back, w, src, src_w, ox, oy, q, blend);
    }
    for q in direct.iter() {
        let n = q.width() as usize;
        let cx = (q.x0 - content.x0) as usize;
        for y in q.y0..q.y1 {
            let o = y as usize * w + q.x0 as usize;
            if o + n > back.len() {
                break;
            }
            let cy = (y - content.y0) as usize;
            if blend {
                row.clear();
                row.resize(n, AlphaPixel::default());
                d.plane.read_row(cx, cy, row);
                pixels::blend_row(row, &mut back[o..o + n]);
            } else {
                d.plane.read_row(cx, cy, &mut back[o..o + n]);
            }
        }
    }
}

use crate::cursor::CursorImage;
use crate::pixels;

/// Curseur à afficher : position du point chaud et image.
pub type CursorState = ((i32, i32), Arc<CursorImage>);

fn same_cursor(a: &Option<CursorState>, b: &Option<CursorState>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some((pa, ia)), Some((pb, ib))) => pa == pb && Arc::ptr_eq(ia, ib),
        _ => false,
    }
}

/// Couleur sous le bureau tant que l'explorateur ne l'a pas dessiné (Tokyo Night).
const DESKTOP_FALLBACK: AlphaPixel = AlphaPixel { blue: 0x26, green: 0x1B, red: 0x1A, alpha: 0 };

/// Ce qu'il faut savoir de la scène, copié sous le verrou du gestionnaire.
pub struct Scene {
    /// Fenêtres visibles, de l'avant vers l'arrière.
    pub windows: Vec<Arc<Mutex<WindowInner>>>,
    /// Contour d'aperçu de déplacement / redimensionnement.
    pub border: Option<Rect>,
}

/// Résultat d'une tentative de composition.
pub enum Outcome {
    /// Image composée et présentée (ou rien à faire).
    Done,
    /// Une fenêtre ou le bureau était verrouillé : la zone abîmée a été remise en attente.
    Busy,
}

/// Zones opaques d'une fenêtre (au plus trois bandes : la fenêtre moins ses quatre coins).
fn opaque_parts(r: &Rect, radius: i32, translucent: bool) -> ([Rect; 3], usize) {
    let mut out = [Rect::EMPTY; 3];
    if translucent || r.is_empty() {
        return (out, 0);
    }
    let rad = radius.max(0).min(r.width() / 2).min(r.height() / 2);
    if rad == 0 {
        out[0] = *r;
        return (out, 1);
    }
    let parts = [
        Rect::new(r.x0, r.y0 + rad, r.x1, r.y1 - rad),
        Rect::new(r.x0 + rad, r.y0, r.x1 - rad, r.y0 + rad),
        Rect::new(r.x0 + rad, r.y1 - rad, r.x1 - rad, r.y1),
    ];
    let mut n = 0;
    for p in parts {
        if !p.is_empty() {
            out[n] = p;
            n += 1;
        }
    }
    (out, n)
}

/// État propre à la tâche du compositeur.
pub struct Compositor {
    /// Curseur actuellement dessiné à l'affichage.
    cursor_drawn: Option<CursorState>,
    /// Sortie GPU branchée (MaiGPU).
    out: Option<Arc<dyn Scanout>>,
    /// Rectangles écrits dans le tampon avant depuis le dernier envoi à la sortie.
    written: Vec<Rect>,
    /// L'appareil lit peut-être encore le tampon avant (présentation pas encore attendue).
    out_busy: bool,
    /// Image chargée dans le curseur matériel (identité de l'`Arc`), s'il est affiché.
    hw_cursor: Option<Arc<CursorImage>>,
    /// Position du point chaud envoyée au curseur matériel.
    hw_pos: (i32, i32),
    /// Rafraîchissements consécutifs remis faute d'avoir pu verrouiller une fenêtre.
    pub busy_streak: u32,
    /// Plein écran exclusif au tour précédent (pour tout recomposer à sa sortie).
    was_exclusive: bool,
}

impl Default for Compositor {
    fn default() -> Self {
        Self::new()
    }
}

impl Compositor {
    pub fn new() -> Self {
        Self {
            cursor_drawn: None,
            out: None,
            written: Vec::new(),
            out_busy: false,
            hw_cursor: None,
            hw_pos: (i32::MIN, i32::MIN),
            busy_streak: 0,
            was_exclusive: false,
        }
    }

    /// Branche une sortie GPU : sa mémoire devient le tampon avant de MGI. Tout l'écran est à
    /// recomposer ensuite (l'appelant l'abîme).
    pub fn attach(&mut self, out: Arc<dyn Scanout>) -> Result<(), &'static str> {
        let m = mgi::MGI.get().ok_or("MGI non initialisé")?;
        let old = m.lock().attach_scanout(out.clone())?;
        // Le framebuffer du chargeur n'est plus affiché : sa projection est rendue.
        drop(old);
        // Le curseur logiciel n'est pas dans le nouveau tampon avant.
        self.cursor_drawn = None;
        self.hw_cursor = None;
        self.written.clear();
        self.out_busy = false;
        self.out = Some(out);
        Ok(())
    }

    /// À appeler avant toute écriture dans le tampon avant : attend que la sortie ait fini de
    /// lire la présentation précédente.
    fn front_write(&mut self) {
        if self.out_busy {
            if let Some(o) = self.out.as_ref() {
                o.wait_idle();
            }
            self.out_busy = false;
        }
    }

    /// Note un rectangle écrit dans le tampon avant.
    fn wrote(&mut self, r: Rect) {
        if self.out.is_some() && !r.is_empty() {
            self.written.push(r);
        }
    }

    /// Envoie à la sortie GPU ce qui a été écrit depuis le dernier envoi (asynchrone).
    pub fn flush_output(&mut self) {
        if self.written.is_empty() {
            return;
        }
        let Some(o) = self.out.as_ref() else {
            self.written.clear();
            return;
        };
        let rects: Vec<ScanoutRect> = self
            .written
            .iter()
            .filter(|r| !r.is_empty())
            .map(|r| ScanoutRect { x: r.x0.max(0) as u32, y: r.y0.max(0) as u32, width: (r.x1 - r.x0.max(0)).max(0) as u32, height: (r.y1 - r.y0.max(0)).max(0) as u32 })
            .collect();
        self.written.clear();
        o.present(&rects);
        self.out_busy = true;
    }

    /// Compose `damage` et présente. `cursor` : position voulue (`None` : curseur caché).
    pub fn frame(&mut self, mut damage: Region, scene: &Scene, cursor: Option<CursorState>) -> Outcome {
        let mgi_ref = match mgi::MGI.get() {
            Some(m) => m,
            None => return Outcome::Done,
        };
        // Attendre la sortie GPU avant de prendre le verrou de MGI (jamais endormi dessous).
        self.front_write();
        let mut mgi = mgi_ref.lock();
        let surf = mgi.surfaces();
        let (w, h) = (surf.width, surf.height);
        let screen = Rect::new(0, 0, w as i32, h as i32);
        damage.clip(&screen);

        if !damage.is_empty() {
            // 1. Instantané : toutes les fenêtres et le bureau, sans jamais attendre.
            let mut guards: Vec<MutexGuard<'_, WindowInner>> = Vec::with_capacity(scene.windows.len());
            for win in scene.windows.iter() {
                match win.try_lock() {
                    Some(g) => guards.push(g),
                    None => {
                        drop(guards);
                        crate::DAMAGE.lock().add_region(&damage);
                        self.busy_streak += 1;
                        return Outcome::Busy;
                    }
                }
            }
            let desktop = match crate::DESKTOP.get().map(|d| d.try_lock()) {
                Some(Some(d)) => Some(d),
                Some(None) => {
                    drop(guards);
                    crate::DAMAGE.lock().add_region(&damage);
                    self.busy_streak += 1;
                    return Outcome::Busy;
                }
                None => None,
            };
            self.busy_streak = 0;

            let back: &mut [AlphaPixel] = &mut *surf.back;

            // 2. Opaque, de l'avant vers l'arrière.
            let mut remaining = Region::with_max_rects(usize::MAX);
            for r in damage.iter() {
                remaining.add(*r);
            }
            // (indice de la fenêtre, partie translucide visible)
            let mut translucent: Vec<(usize, Region)> = Vec::new();
            let mut row = Vec::new();
            for (i, g) in guards.iter().enumerate() {
                if !g.visible || remaining.is_empty() {
                    continue;
                }
                let (ww, wh) = g.get_size();
                let pos = g.get_position();
                let rect = Rect::from_xywh(pos.x as i32, pos.y as i32, ww as i32, wh as i32);
                if !remaining.intersects_rect(&rect) {
                    continue;
                }
                let (parts, n) = opaque_parts(&rect, g.corner_radius as i32, g.translucent);
                for o in &parts[..n] {
                    let todo = remaining.intersect_rect(o);
                    for r in todo.iter() {
                        put_window(back, w, g, rect.x0, rect.y0, r, false, &mut row);
                    }
                    remaining.subtract(*o);
                }
                let rest = remaining.intersect_rect(&rect);
                if !rest.is_empty() {
                    translucent.push((i, rest));
                }
            }
            match desktop.as_ref() {
                Some(d) => {
                    let area = Rect::new(0, 0, d.width() as i32, d.height() as i32);
                    for r in remaining.iter() {
                        let inside = r.intersect(&area);
                        pixels::put_rect(back, w, d.buffer(), d.width(), 0, 0, &inside, false);
                        let (outside, n) = r.minus(&area);
                        for o in &outside[..n] {
                            pixels::fill_rect(back, w, o, DESKTOP_FALLBACK);
                        }
                    }
                }
                None => {
                    for r in remaining.iter() {
                        pixels::fill_rect(back, w, r, DESKTOP_FALLBACK);
                    }
                }
            }

            // 3. Translucide, de l'arrière vers l'avant, puis le contour d'aperçu.
            for (i, part) in translucent.iter().rev() {
                let g = &guards[*i];
                let pos = g.get_position();
                for r in part.iter() {
                    put_window(back, w, g, pos.x as i32, pos.y as i32, r, true, &mut row);
                }
            }
            drop(desktop);
            drop(guards);
            if let Some(b) = scene.border.as_ref() {
                for band in pixels::border_bands(b).iter() {
                    for r in damage.intersect_rect(band).iter() {
                        pixels::draw_border(back, w, b, r);
                    }
                }
            }

            // 4. Présentation des seules zones abîmées.
            self.front_write();
            for r in damage.iter() {
                pixels::present_rect(surf.front, back, w, r);
                self.wrote(*r);
            }
        }

        self.cursor_plane(&mut *surf.front, &*surf.back, w, h, &damage, cursor);
        Outcome::Done
    }

    /// Met à jour le seul plan du curseur (aucune recomposition). Vrai s'il a bougé.
    pub fn cursor_only(&mut self, cursor: Option<CursorState>) -> bool {
        if same_cursor(&cursor, &self.cursor_drawn) {
            return false;
        }
        if let Some(m) = mgi::MGI.get() {
            let mut mgi = m.lock();
            let surf = mgi.surfaces();
            let (w, h) = (surf.width, surf.height);
            self.cursor_plane(&mut *surf.front, &*surf.back, w, h, &Region::new(), cursor);
        }
        true
    }

    fn cursor_plane(
        &mut self,
        front: &mut [AlphaPixel],
        back: &[AlphaPixel],
        w: usize,
        h: usize,
        presented: &Region,
        cursor: Option<CursorState>,
    ) {
        if self.hw_cursor_plane(front, back, w, h, &cursor) {
            return;
        }
        let screen = Rect::new(0, 0, w as i32, h as i32);
        let rect_of = |((x, y), img): &CursorState| img.rect_at(*x, *y).intersect(&screen);
        let moved = !same_cursor(&cursor, &self.cursor_drawn);
        if moved {
            // Effacer l'ancien : l'affichage redevient égal au tampon arrière.
            if let Some(old) = self.cursor_drawn.clone() {
                let r = rect_of(&old);
                self.front_write();
                pixels::present_rect(front, back, w, &r);
                self.wrote(r);
            }
        }
        if let Some(c) = &cursor {
            if moved || presented.intersects_rect(&rect_of(c)) {
                let ((x, y), img) = c;
                self.front_write();
                img.draw(front, back, w, h, *x, *y);
                self.wrote(rect_of(c));
                crate::STAT_SW_CURSOR.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.cursor_drawn = cursor;
    }

    /// Curseur sur le plan matériel de la sortie GPU, si elle en a un et que l'image y tient.
    /// Rend faux s'il faut passer par le curseur logiciel (le plan matériel est alors caché).
    fn hw_cursor_plane(&mut self, front: &mut [AlphaPixel], back: &[AlphaPixel], w: usize, h: usize, cursor: &Option<CursorState>) -> bool {
        let Some(out) = self.out.clone() else { return false };
        let side = out.cursor_size();
        let fits = |img: &CursorImage| {
            side > 0
                && img.width <= side
                && img.height <= side
                && img.hot_x >= 0
                && img.hot_y >= 0
                && (img.hot_x as usize) < side
                && (img.hot_y as usize) < side
        };
        match cursor {
            Some(((x, y), img)) if fits(img.as_ref()) => {
                // Un curseur logiciel encore dessiné disparaît du tampon avant.
                if let Some(old) = self.cursor_drawn.take() {
                    let screen = Rect::new(0, 0, w as i32, h as i32);
                    let r = old.1.rect_at(old.0 .0, old.0 .1).intersect(&screen);
                    self.front_write();
                    pixels::present_rect(front, back, w, &r);
                    self.wrote(r);
                }
                let _ = h;
                let same_img = self.hw_cursor.as_ref().is_some_and(|c| Arc::ptr_eq(c, img));
                if !same_img {
                    let argb: Vec<u32> = img
                        .px
                        .iter()
                        .map(|p| {
                            // MaiOS : alpha 0 = opaque ; ARGB : 255 = opaque, non prémultiplié.
                            let a = 255 - p.alpha as u32;
                            a << 24 | (p.red as u32) << 16 | (p.green as u32) << 8 | p.blue as u32
                        })
                        .collect();
                    let plane = CursorPlane { argb: &argb, width: img.width, height: img.height, hot_x: img.hot_x as u32, hot_y: img.hot_y as u32 };
                    out.set_cursor(Some(&plane), *x, *y);
                    self.hw_cursor = Some(img.clone());
                    self.hw_pos = (*x, *y);
                } else if self.hw_pos != (*x, *y) {
                    out.move_cursor(*x, *y);
                    self.hw_pos = (*x, *y);
                }
                true
            }
            _ => {
                // Curseur caché, ou image trop grande pour le plan matériel.
                if self.hw_cursor.take().is_some() {
                    out.set_cursor(None, 0, 0);
                    self.hw_pos = (i32::MIN, i32::MIN);
                }
                false
            }
        }
    }

    /// Plein écran exclusif : le tampon de l'application va **directement** à l'affichage
    /// (sans passer par le tampon arrière ni par la scène), curseur caché.
    pub fn present_exclusive(&mut self) {
        let ptr = mgi::exclusive_buffer();
        if let (Some((buf, len)), Some(m)) = (ptr, mgi::MGI.get()) {
            self.front_write();
            let mut mgi = m.lock();
            let surf = mgi.surfaces();
            let n = len.min(surf.width * surf.height).min(surf.front.len());
            // SAFETY : tampon déclaré par l'application via SYS_GET_EVENT (sous-commande 1),
            // projeté dans la moitié haute partagée et long d'au moins `len` pixels.
            let src = unsafe { core::slice::from_raw_parts(buf as *const AlphaPixel, n) };
            self.front_write();
            surf.front[..n].copy_from_slice(src);
            self.cursor_drawn = None;
            let rows = n.div_ceil(surf.width.max(1));
            self.wrote(Rect::new(0, 0, surf.width as i32, rows as i32));
            // Curseur caché en plein écran exclusif.
            if self.hw_cursor.take().is_some() {
                if let Some(out) = self.out.as_ref() {
                    out.set_cursor(None, 0, 0);
                }
                self.hw_pos = (i32::MIN, i32::MIN);
            }
        }
        self.was_exclusive = true;
    }

    /// Vrai une seule fois à la sortie du plein écran exclusif.
    pub fn leaving_exclusive(&mut self) -> bool {
        core::mem::replace(&mut self.was_exclusive, false)
    }

    /// Une application plein écran sans fenêtre a dessiné directement dans le tampon arrière.
    pub fn present_raw(&mut self, cursor: Option<CursorState>) {
        if let Some(m) = mgi::MGI.get() {
            self.front_write();
            let mut mgi = m.lock();
            let surf = mgi.surfaces();
            let (w, h) = (surf.width, surf.height);
            let all = Rect::new(0, 0, w as i32, h as i32);
            self.front_write();
            pixels::present_rect(surf.front, surf.back, w, &all);
            self.wrote(all);
            self.cursor_drawn = None;
            self.cursor_plane(&mut *surf.front, &*surf.back, w, h, &Region::from_rect(all), cursor);
        }
    }
}

/// Curseur publié par le gestionnaire : position (lecture atomique) et image courante.
pub fn cursor_position() -> Option<CursorState> {
    if mgi::is_exclusive_fullscreen() || !crate::CURSOR_VISIBLE.load(Ordering::Acquire) {
        return None;
    }
    let v = crate::CURSOR_POS.load(Ordering::Acquire);
    Some((((v >> 32) as i32, v as u32 as i32), crate::cursor_image()))
}
