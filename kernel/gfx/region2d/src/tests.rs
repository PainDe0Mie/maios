//! Tests hôte : `cargo test` dans une copie de `kernel/gfx/region` hors du workspace.
//!
//! Chaque opération est rejouée sur une grille de booléens (référence) et la région est
//! vérifiée après chaque pas : rectangles non vides, deux à deux disjoints, couverture
//! identique à la référence (exacte) ou la contenant (budget dépassé).

use super::*;

const N: i32 = 48;

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
    fn range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + (self.next() % (hi - lo) as u32) as i32
    }
    fn rect(&mut self) -> Rect {
        let x0 = self.range(-4, N + 4);
        let y0 = self.range(-4, N + 4);
        let w = self.range(0, 24);
        let h = self.range(0, 24);
        Rect::from_xywh(x0, y0, w, h)
    }
}

type Grid = Vec<bool>;

fn idx(x: i32, y: i32) -> Option<usize> {
    if x < 0 || y < 0 || x >= N || y >= N {
        None
    } else {
        Some((y * N + x) as usize)
    }
}

fn paint(g: &mut Grid, r: &Rect, v: bool) {
    for y in r.y0..r.y1 {
        for x in r.x0..r.x1 {
            if let Some(i) = idx(x, y) {
                g[i] = v;
            }
        }
    }
}

/// Vérifie les invariants. `exact` : la couverture (dans la grille) doit égaler la référence.
fn check(reg: &Region, g: &Grid, exact: bool) {
    let rs = reg.rects();
    for (i, a) in rs.iter().enumerate() {
        assert!(!a.is_empty(), "rectangle vide {a:?}");
        for b in rs[i + 1..].iter() {
            assert!(!a.intersects(b), "chevauchement {a:?} / {b:?}");
        }
    }
    for y in 0..N {
        for x in 0..N {
            let inside = reg.contains_point(x, y);
            let want = g[idx(x, y).unwrap()];
            if exact {
                assert_eq!(inside, want, "pixel ({x},{y})");
            } else if want {
                assert!(inside, "pixel abîmé oublié ({x},{y})");
            }
        }
    }
    let sum: u64 = rs.iter().map(Rect::area).sum();
    assert_eq!(sum, reg.area());
}

/// Les opérations sont faites dans la grille [0, N)² pour que la référence soit complète.
fn clipped(r: Rect) -> Rect {
    r.intersect(&Rect::new(0, 0, N, N))
}

#[test]
fn minus_covers_exactly() {
    let mut rng = Lcg(7);
    for _ in 0..5000 {
        let a = clipped(rng.rect());
        let b = rng.rect();
        let (arr, n) = a.minus(&b);
        let mut g = vec![false; (N * N) as usize];
        paint(&mut g, &a, true);
        paint(&mut g, &clipped(b), false);
        let mut reg = Region::with_max_rects(usize::MAX);
        for r in &arr[..n] {
            assert!(!r.is_empty());
            reg.rects.push(*r);
        }
        check(&reg, &g, true);
    }
}

#[test]
fn exact_add_subtract_random() {
    for seed in 1..40 {
        let mut rng = Lcg(seed);
        let mut reg = Region::with_max_rects(usize::MAX);
        let mut g = vec![false; (N * N) as usize];
        for step in 0..200 {
            let r = clipped(rng.rect());
            if step % 3 == 2 {
                reg.subtract(r);
                paint(&mut g, &r, false);
            } else {
                reg.add(r);
                paint(&mut g, &r, true);
            }
            check(&reg, &g, true);
        }
    }
}

#[test]
fn budget_never_forgets_damage() {
    for seed in 100..140 {
        let mut rng = Lcg(seed);
        let mut reg = Region::with_max_rects(6);
        let mut g = vec![false; (N * N) as usize];
        for _ in 0..150 {
            let r = clipped(rng.rect());
            reg.add(r);
            paint(&mut g, &r, true);
            assert!(reg.len() <= 6, "budget dépassé : {}", reg.len());
            check(&reg, &g, false);
        }
    }
}

#[test]
fn intersect_and_clip_are_exact() {
    let mut rng = Lcg(999);
    for _ in 0..300 {
        let mut reg = Region::with_max_rects(usize::MAX);
        let mut g = vec![false; (N * N) as usize];
        for _ in 0..12 {
            let r = clipped(rng.rect());
            reg.add(r);
            paint(&mut g, &r, true);
        }
        let c = clipped(rng.rect());
        let inter = reg.intersect_rect(&c);
        let mut gi = vec![false; (N * N) as usize];
        for y in c.y0..c.y1 {
            for x in c.x0..c.x1 {
                let i = idx(x, y).unwrap();
                gi[i] = g[i];
            }
        }
        check(&inter, &gi, true);
        let mut cl = reg.clone();
        cl.clip(&c);
        check(&cl, &gi, true);
    }
}

#[test]
fn tiles_glue_back_into_one_rect() {
    let mut reg = Region::new();
    for ty in 0..4 {
        for tx in 0..5 {
            reg.add(Rect::from_xywh(tx * 64, ty * 64, 64, 64));
        }
    }
    assert_eq!(reg.rects(), &[Rect::new(0, 0, 320, 256)]);
}

#[test]
fn occlusion_front_to_back() {
    // Deux fenêtres opaques qui se chevauchent sur un écran 100×100 : ce qui reste à peindre
    // pour le fond est l'écran moins les deux fenêtres.
    let screen = Rect::new(0, 0, 100, 100);
    let mut remaining = Region::from_rect(screen);
    let top = Rect::new(10, 10, 60, 60);
    let below = Rect::new(40, 40, 90, 90);
    let top_part = remaining.intersect_rect(&top);
    assert_eq!(top_part.area(), 50 * 50);
    remaining.subtract(top);
    let below_part = remaining.intersect_rect(&below);
    assert_eq!(below_part.area(), 50 * 50 - 20 * 20);
    remaining.subtract(below);
    assert_eq!(remaining.area(), 100 * 100 - 2 * 50 * 50 + 20 * 20);
}

#[test]
fn take_keeps_budget_and_empties() {
    let mut reg = Region::with_max_rects(3);
    reg.add(Rect::new(0, 0, 4, 4));
    let t = reg.take();
    assert!(reg.is_empty());
    assert_eq!(t.len(), 1);
    assert_eq!(t.max_rects, 3);
}
