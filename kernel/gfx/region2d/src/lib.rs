//! # Régions pour MaiCompositor
//!
//! Une [`Region`] est un ensemble de rectangles **deux à deux disjoints** (demi-ouverts,
//! `[x0, x1) × [y0, y1)`). C'est la structure qu'emploient les compositeurs modernes pour
//! deux usages distincts :
//!
//! * **Suivi des zones abîmées** (*damage tracking*, comme `pixman_region32` dans Weston et
//!   wlroots) : chaque validation de surface, déplacement de fenêtre ou de contour ajoute un
//!   rectangle ; le compositeur ne recompose et ne présente que l'union. L'union est exacte
//!   tant que la région compte au plus `max_rects` rectangles ; au-delà, les deux rectangles
//!   dont l'enveloppe gaspille le moins de surface sont fusionnés (heuristique de coût de
//!   Chromium, `cc::DamageTracker` / `SimpleEnclosedRegion`), ce qui sur-estime la zone sans
//!   jamais rien oublier.
//! * **Élimination des surfaces cachées** : en composant du premier plan vers le fond, on
//!   soustrait la zone opaque de chaque fenêtre de ce qui reste à peindre. La soustraction et
//!   l'intersection sont toujours **exactes** (aucun budget) : sur-estimer ici ferait peindre
//!   une couche du fond par-dessus une fenêtre déjà dessinée.
//!
//! Les rectangles qui partagent un côté entier sont recollés après chaque opération, si bien
//! qu'une grille de tuiles adjacentes redevient un seul rectangle.
//!
//! Sans dépendance ni `unsafe` : testée sur l'hôte (`cargo test` dans une copie hors du
//! workspace, comme `llfree`).

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::vec::Vec;

/// Nombre de rectangles au-delà duquel [`Region::add`] commence à fusionner.
pub const DEFAULT_MAX_RECTS: usize = 32;

// ── Rectangle ────────────────────────────────────────────────────────────────

/// Rectangle demi-ouvert `[x0, x1) × [y0, y1)`. Vide si `x0 >= x1` ou `y0 >= y1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
pub struct Rect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl Rect {
    pub const EMPTY: Rect = Rect { x0: 0, y0: 0, x1: 0, y1: 0 };

    #[inline]
    pub const fn new(x0: i32, y0: i32, x1: i32, y1: i32) -> Self {
        Self { x0, y0, x1, y1 }
    }

    #[inline]
    pub fn from_xywh(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self::new(x, y, x.saturating_add(w), y.saturating_add(h))
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.x0 >= self.x1 || self.y0 >= self.y1
    }

    #[inline]
    pub fn width(&self) -> i32 {
        (self.x1 - self.x0).max(0)
    }

    #[inline]
    pub fn height(&self) -> i32 {
        (self.y1 - self.y0).max(0)
    }

    #[inline]
    pub fn area(&self) -> u64 {
        if self.is_empty() {
            0
        } else {
            self.width() as u64 * self.height() as u64
        }
    }

    /// Intersection (éventuellement vide).
    #[inline]
    pub fn intersect(&self, o: &Rect) -> Rect {
        Rect::new(self.x0.max(o.x0), self.y0.max(o.y0), self.x1.min(o.x1), self.y1.min(o.y1))
    }

    /// Vrai si l'intersection n'est pas vide.
    #[inline]
    pub fn intersects(&self, o: &Rect) -> bool {
        !self.is_empty()
            && !o.is_empty()
            && self.x0 < o.x1
            && o.x0 < self.x1
            && self.y0 < o.y1
            && o.y0 < self.y1
    }

    /// Vrai si `o` est entièrement dans `self` (un rectangle vide est contenu partout).
    #[inline]
    pub fn contains(&self, o: &Rect) -> bool {
        o.is_empty() || (self.x0 <= o.x0 && self.y0 <= o.y0 && o.x1 <= self.x1 && o.y1 <= self.y1)
    }

    #[inline]
    pub fn contains_point(&self, x: i32, y: i32) -> bool {
        x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1
    }

    /// Plus petit rectangle qui contient les deux (un rectangle vide est neutre).
    #[inline]
    pub fn hull(&self, o: &Rect) -> Rect {
        if self.is_empty() {
            *o
        } else if o.is_empty() {
            *self
        } else {
            Rect::new(self.x0.min(o.x0), self.y0.min(o.y0), self.x1.max(o.x1), self.y1.max(o.y1))
        }
    }

    #[inline]
    pub fn translate(&self, dx: i32, dy: i32) -> Rect {
        Rect::new(self.x0 + dx, self.y0 + dy, self.x1 + dx, self.y1 + dy)
    }

    /// Parties de `self` que `o` ne couvre pas : au plus quatre rectangles disjoints
    /// (bande du haut et du bas sur toute la largeur, puis gauche et droite de la bande
    /// médiane, ce qui garde des bandes horizontales longues, favorables aux copies de lignes).
    #[inline]
    pub fn minus(&self, o: &Rect) -> ([Rect; 4], usize) {
        let mut out = [Rect::EMPTY; 4];
        if self.is_empty() {
            return (out, 0);
        }
        if !self.intersects(o) {
            out[0] = *self;
            return (out, 1);
        }
        let i = self.intersect(o);
        let mut n = 0;
        if i.y0 > self.y0 {
            out[n] = Rect::new(self.x0, self.y0, self.x1, i.y0);
            n += 1;
        }
        if i.y1 < self.y1 {
            out[n] = Rect::new(self.x0, i.y1, self.x1, self.y1);
            n += 1;
        }
        if i.x0 > self.x0 {
            out[n] = Rect::new(self.x0, i.y0, i.x0, i.y1);
            n += 1;
        }
        if i.x1 < self.x1 {
            out[n] = Rect::new(i.x1, i.y0, self.x1, i.y1);
            n += 1;
        }
        (out, n)
    }

    /// Enveloppe de `a` et `b` s'ils partagent un côté entier (leur union est alors un rectangle).
    #[inline]
    fn glue(a: &Rect, b: &Rect) -> Option<Rect> {
        if a.y0 == b.y0 && a.y1 == b.y1 && (a.x1 == b.x0 || b.x1 == a.x0) {
            return Some(a.hull(b));
        }
        if a.x0 == b.x0 && a.x1 == b.x1 && (a.y1 == b.y0 || b.y1 == a.y0) {
            return Some(a.hull(b));
        }
        None
    }
}

// ── Région ───────────────────────────────────────────────────────────────────

/// Ensemble de rectangles non vides, deux à deux disjoints.
#[derive(Clone, Debug)]
pub struct Region {
    rects: Vec<Rect>,
    max_rects: usize,
}

impl Default for Region {
    fn default() -> Self {
        Self::new()
    }
}

impl Region {
    /// Région vide, budget [`DEFAULT_MAX_RECTS`].
    pub const fn new() -> Self {
        Self { rects: Vec::new(), max_rects: DEFAULT_MAX_RECTS }
    }

    /// Région vide dont [`add`](Self::add) fusionne au-delà de `max_rects` (`usize::MAX` :
    /// union toujours exacte).
    pub const fn with_max_rects(max_rects: usize) -> Self {
        Self { rects: Vec::new(), max_rects: if max_rects == 0 { 1 } else { max_rects } }
    }

    /// Région exacte (sans budget) réduite à `r`.
    pub fn from_rect(r: Rect) -> Self {
        let mut s = Self::with_max_rects(usize::MAX);
        if !r.is_empty() {
            s.rects.push(r);
        }
        s
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.rects.len()
    }

    #[inline]
    pub fn rects(&self) -> &[Rect] {
        &self.rects
    }

    #[inline]
    pub fn iter(&self) -> core::slice::Iter<'_, Rect> {
        self.rects.iter()
    }

    #[inline]
    pub fn clear(&mut self) {
        self.rects.clear();
    }

    /// Vide la région et rend son contenu (le budget est conservé).
    pub fn take(&mut self) -> Region {
        Region { rects: core::mem::take(&mut self.rects), max_rects: self.max_rects }
    }

    /// Enveloppe de toute la région.
    pub fn bounds(&self) -> Rect {
        self.rects.iter().fold(Rect::EMPTY, |acc, r| acc.hull(r))
    }

    /// Surface couverte (exacte : les rectangles sont disjoints).
    pub fn area(&self) -> u64 {
        self.rects.iter().map(Rect::area).sum()
    }

    #[inline]
    pub fn intersects_rect(&self, r: &Rect) -> bool {
        self.rects.iter().any(|e| e.intersects(r))
    }

    #[inline]
    pub fn contains_point(&self, x: i32, y: i32) -> bool {
        self.rects.iter().any(|e| e.contains_point(x, y))
    }

    /// Union avec `r`, exacte tant que le budget n'est pas dépassé (sinon sur-estimée).
    pub fn add(&mut self, r: Rect) {
        if r.is_empty() {
            return;
        }
        if self.rects.iter().any(|e| e.contains(&r)) {
            return;
        }
        // Ce que `r` recouvre entièrement disparaît : `r` le remplace d'un bloc.
        self.rects.retain(|e| !r.contains(e));

        let mut pieces: Vec<Rect> = Vec::with_capacity(4);
        pieces.push(r);
        let mut next: Vec<Rect> = Vec::with_capacity(8);
        for e in self.rects.iter() {
            if !e.intersects(&r) {
                continue;
            }
            next.clear();
            for p in pieces.iter() {
                let (arr, n) = p.minus(e);
                next.extend_from_slice(&arr[..n]);
            }
            core::mem::swap(&mut pieces, &mut next);
            if pieces.is_empty() {
                return;
            }
        }
        self.rects.extend_from_slice(&pieces);
        self.coalesce();
        self.enforce_budget();
    }

    /// Union avec une autre région.
    pub fn add_region(&mut self, o: &Region) {
        for r in o.rects.iter() {
            self.add(*r);
        }
    }

    /// Retire `r` (toujours exact).
    pub fn subtract(&mut self, r: Rect) {
        if r.is_empty() || !self.intersects_rect(&r) {
            return;
        }
        let mut out = Vec::with_capacity(self.rects.len() + 3);
        for e in self.rects.drain(..) {
            if e.intersects(&r) {
                let (arr, n) = e.minus(&r);
                out.extend_from_slice(&arr[..n]);
            } else {
                out.push(e);
            }
        }
        self.rects = out;
        self.coalesce();
    }

    /// Partie de la région qui tombe dans `r` (toujours exact, sans budget).
    pub fn intersect_rect(&self, r: &Rect) -> Region {
        let mut out = Region::with_max_rects(usize::MAX);
        for e in self.rects.iter() {
            let i = e.intersect(r);
            if !i.is_empty() {
                out.rects.push(i);
            }
        }
        out
    }

    /// Restreint la région à `r` sur place.
    pub fn clip(&mut self, r: &Rect) {
        self.rects.retain_mut(|e| {
            *e = e.intersect(r);
            !e.is_empty()
        });
    }

    pub fn translate(&mut self, dx: i32, dy: i32) {
        for e in self.rects.iter_mut() {
            *e = e.translate(dx, dy);
        }
    }

    /// Recolle les rectangles qui partagent un côté entier, jusqu'à stabilité.
    fn coalesce(&mut self) {
        loop {
            let mut changed = false;
            let mut i = 0;
            while i < self.rects.len() {
                let mut j = i + 1;
                while j < self.rects.len() {
                    if let Some(m) = Rect::glue(&self.rects[i], &self.rects[j]) {
                        self.rects[i] = m;
                        self.rects.swap_remove(j);
                        changed = true;
                    } else {
                        j += 1;
                    }
                }
                i += 1;
            }
            if !changed {
                break;
            }
        }
    }

    /// Ramène la région sous son budget en fusionnant les paires les moins coûteuses.
    ///
    /// L'enveloppe d'une paire absorbe tous les rectangles qu'elle touche (jusqu'à stabilité),
    /// ce qui garde l'ensemble disjoint et fait baisser le compte d'au moins un à chaque tour.
    fn enforce_budget(&mut self) {
        while self.rects.len() > self.max_rects && self.rects.len() >= 2 {
            let (mut bi, mut bj, mut best) = (0usize, 1usize, u64::MAX);
            for i in 0..self.rects.len() {
                for j in i + 1..self.rects.len() {
                    let (a, b) = (&self.rects[i], &self.rects[j]);
                    let waste = a.hull(b).area().saturating_sub(a.area() + b.area());
                    if waste < best {
                        best = waste;
                        bi = i;
                        bj = j;
                        if waste == 0 {
                            break;
                        }
                    }
                }
                if best == 0 {
                    break;
                }
            }
            let mut hull = self.rects[bi].hull(&self.rects[bj]);
            self.rects.swap_remove(bj.max(bi));
            self.rects.swap_remove(bj.min(bi));
            loop {
                let mut grew = false;
                let mut k = 0;
                while k < self.rects.len() {
                    if self.rects[k].intersects(&hull) {
                        hull = hull.hull(&self.rects[k]);
                        self.rects.swap_remove(k);
                        grew = true;
                    } else {
                        k += 1;
                    }
                }
                if !grew {
                    break;
                }
            }
            self.rects.push(hull);
            self.coalesce();
        }
    }
}

#[cfg(test)]
mod tests;
