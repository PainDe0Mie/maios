//! `wl_region` : suite d'ajouts et de retraits de rectangles (coordonnées de la surface).
//!
//! Le serveur n'en a besoin (W1) que pour la région opaque : « couvre-t-elle tout ce
//! rectangle ? ». On garde donc les opérations telles quelles et on calcule à la demande la
//! partie *non couverte* : `¬(R ∪ a) = ¬R − a` et `¬(R − s) = ¬R ∪ s`. Une approximation de
//! `region2d` (trop de rectangles fusionnés) ne fait que grossir la partie non couverte : la
//! réponse reste prudente (une surface jugée translucide à tort est seulement mélangée).

use alloc::vec::Vec;
use region2d::{Rect, Region};
use wl_proto::protocols::wl_region;
use wl_proto::Args;

use crate::client::Fd;

/// Opérations gardées au plus (au-delà, la région n'est plus jamais jugée couvrante).
const MAX_OPS: usize = 1024;

#[derive(Clone, Default)]
pub(crate) struct RegionOps {
    ops: Vec<(bool, Rect)>,
    overflow: bool,
}

/// Rectangle `x, y, w, h` du protocole (vide si `w` ou `h` n'est pas positif), sans débordement.
pub(crate) fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
    if w <= 0 || h <= 0 {
        return Rect::EMPTY;
    }
    Rect::new(x, y, x.saturating_add(w), y.saturating_add(h))
}

impl RegionOps {
    pub fn request(&mut self, op: u16, args: &Args<'_, Fd>) {
        let add = match op {
            wl_region::req::ADD => true,
            wl_region::req::SUBTRACT => false,
            _ => return,
        };
        let r = rect(args.int(0), args.int(1), args.int(2), args.int(3));
        if r.is_empty() {
            return;
        }
        if self.ops.len() >= MAX_OPS {
            self.overflow = true;
            return;
        }
        self.ops.push((add, r));
    }

    /// Le point `(x, y)` est-il dans la région ? Exact : la dernière opération qui le contient
    /// décide (ajout → dedans, retrait → dehors).
    pub fn contains_point(&self, x: i32, y: i32) -> bool {
        self.ops.iter().rev().find(|(_, r)| r.contains_point(x, y)).is_some_and(|&(add, _)| add)
    }

    /// Vrai si la région couvre tout `r` (réponse prudente : faux en cas de doute).
    pub fn covers(&self, r: Rect) -> bool {
        if self.overflow || r.is_empty() {
            return !self.overflow;
        }
        let mut uncovered = Region::with_max_rects(64);
        uncovered.add(r);
        for &(add, q) in &self.ops {
            if add {
                uncovered.subtract(q);
            } else {
                uncovered.add(q.intersect(&r));
            }
        }
        uncovered.is_empty()
    }
}
