//! Arbre AVL intrusif et augmenté pour EEVDF.
//!
//! * Clé : `(deadline, id)`. Le nœud le plus à gauche est l'échéance la plus proche, gardé en
//!   cache pour un choix en O(1) dans le cas courant (Linux 6.8, « sched/eevdf: Sort the rbtree by
//!   virtual deadline », Abel Wu 2023).
//! * Augmentation : chaque nœud connaît le plus petit `vruntime` de son sous-arbre. La recherche
//!   « échéance la plus proche parmi les tâches éligibles » élague alors tout sous-arbre sans tâche
//!   éligible : O(log n) au pire.
//! * AVL plutôt que rouge-noir : hauteur ≤ 1,44 log₂ n contre 2 log₂ n, donc moins de nœuds
//!   visités par recherche, et c'est la recherche qui domine ici (à chaque choix, à chaque test de
//!   préemption au réveil).
//!
//! Tous les pointeurs sont ceux des entités elles-mêmes : aucune allocation.

use core::ptr;
use crate::entity::{vt_lt, vt_min, Entity};

pub struct Tree {
    root: *mut Entity,
    leftmost: *mut Entity,
    len: usize,
}

// SAFETY : l'arbre n'est manipulé que sous le verrou de sa file.
unsafe impl Send for Tree {}

#[inline(always)]
unsafe fn h(n: *mut Entity) -> i32 {
    if n.is_null() { 0 } else { (*n).height }
}

#[inline(always)]
unsafe fn less(a: *const Entity, b: *const Entity) -> bool {
    let (da, db) = ((*a).deadline, (*b).deadline);
    if da != db { vt_lt(da, db) } else { (*a).id < (*b).id }
}

/// Recalcule hauteur et augmentation d'un nœud à partir de ses fils.
#[inline(always)]
unsafe fn update(n: *mut Entity) {
    let (l, r) = ((*n).left, (*n).right);
    (*n).height = 1 + h(l).max(h(r));
    let mut m = (*n).vruntime;
    if !l.is_null() { m = vt_min(m, (*l).min_vruntime); }
    if !r.is_null() { m = vt_min(m, (*r).min_vruntime); }
    (*n).min_vruntime = m;
}

impl Tree {
    pub const fn new() -> Tree {
        Tree { root: ptr::null_mut(), leftmost: ptr::null_mut(), len: 0 }
    }

    #[inline]
    pub fn len(&self) -> usize { self.len }

    #[inline]
    pub fn is_empty(&self) -> bool { self.root.is_null() }

    #[inline]
    pub fn first(&self) -> *mut Entity { self.leftmost }

    #[inline]
    pub fn root(&self) -> *mut Entity { self.root }

    /// Plus petit `vruntime` de tout l'arbre, s'il n'est pas vide.
    #[inline]
    pub fn min_vruntime(&self) -> Option<u64> {
        if self.root.is_null() { None } else { unsafe { Some((*self.root).min_vruntime) } }
    }

    unsafe fn replace_child(&mut self, parent: *mut Entity, old: *mut Entity, new: *mut Entity) {
        if parent.is_null() {
            self.root = new;
        } else if (*parent).left == old {
            (*parent).left = new;
        } else {
            (*parent).right = new;
        }
    }

    unsafe fn rotate_right(&mut self, n: *mut Entity) -> *mut Entity {
        let l = (*n).left;
        (*n).left = (*l).right;
        if !(*l).right.is_null() { (*(*l).right).parent = n; }
        (*l).parent = (*n).parent;
        self.replace_child((*n).parent, n, l);
        (*l).right = n;
        (*n).parent = l;
        update(n);
        update(l);
        l
    }

    unsafe fn rotate_left(&mut self, n: *mut Entity) -> *mut Entity {
        let r = (*n).right;
        (*n).right = (*r).left;
        if !(*r).left.is_null() { (*(*r).left).parent = n; }
        (*r).parent = (*n).parent;
        self.replace_child((*n).parent, n, r);
        (*r).left = n;
        (*n).parent = r;
        update(n);
        update(r);
        r
    }

    /// Remonte de `n` jusqu'à la racine : hauteurs, augmentation, rééquilibrage.
    /// On remonte toujours jusqu'en haut, car l'augmentation peut changer au-delà du point où
    /// la hauteur se stabilise.
    unsafe fn retrace(&mut self, mut n: *mut Entity) {
        while !n.is_null() {
            update(n);
            let bal = h((*n).left) - h((*n).right);
            if bal > 1 {
                let l = (*n).left;
                if h((*l).left) < h((*l).right) {
                    self.rotate_left(l);
                }
                n = self.rotate_right(n);
            } else if bal < -1 {
                let r = (*n).right;
                if h((*r).right) < h((*r).left) {
                    self.rotate_right(r);
                }
                n = self.rotate_left(n);
            }
            n = (*n).parent;
        }
    }

    /// Insère `e` (qui ne doit pas déjà être dans un arbre).
    ///
    /// # Safety
    /// `e` est valide, hors de tout arbre, et le reste tant qu'il y est.
    pub unsafe fn insert(&mut self, e: *mut Entity) {
        debug_assert!(!(*e).in_tree);
        (*e).left = ptr::null_mut();
        (*e).right = ptr::null_mut();
        (*e).height = 1;
        (*e).min_vruntime = (*e).vruntime;
        (*e).in_tree = true;
        self.len += 1;

        let mut parent = ptr::null_mut();
        let mut cur = self.root;
        let mut go_left = false;
        let mut always_left = true;
        while !cur.is_null() {
            parent = cur;
            go_left = less(e, cur);
            if go_left {
                cur = (*cur).left;
            } else {
                cur = (*cur).right;
                always_left = false;
            }
        }
        (*e).parent = parent;
        if parent.is_null() {
            self.root = e;
        } else if go_left {
            (*parent).left = e;
        } else {
            (*parent).right = e;
        }
        if always_left {
            self.leftmost = e;
        }
        self.retrace(parent);
    }

    /// Retire `z` de l'arbre.
    ///
    /// # Safety
    /// `z` est dans cet arbre.
    pub unsafe fn remove(&mut self, z: *mut Entity) {
        debug_assert!((*z).in_tree);
        if self.leftmost == z {
            // Le successeur du plus à gauche : le plus à gauche de son fils droit, sinon son parent.
            self.leftmost = if !(*z).right.is_null() { leftmost_of((*z).right) } else { (*z).parent };
        }
        let from;
        if (*z).left.is_null() || (*z).right.is_null() {
            let child = if (*z).left.is_null() { (*z).right } else { (*z).left };
            let parent = (*z).parent;
            self.replace_child(parent, z, child);
            if !child.is_null() { (*child).parent = parent; }
            from = parent;
        } else {
            let s = leftmost_of((*z).right);
            if (*s).parent == z {
                // `s` est le fils droit direct de `z` et n'a pas de fils gauche.
                (*s).left = (*z).left;
                (*(*z).left).parent = s;
                (*s).parent = (*z).parent;
                self.replace_child((*z).parent, z, s);
                (*s).height = (*z).height;
                from = s;
            } else {
                let sp = (*s).parent;
                let sr = (*s).right;
                (*sp).left = sr;
                if !sr.is_null() { (*sr).parent = sp; }
                (*s).left = (*z).left;
                (*(*z).left).parent = s;
                (*s).right = (*z).right;
                (*(*z).right).parent = s;
                (*s).parent = (*z).parent;
                self.replace_child((*z).parent, z, s);
                (*s).height = (*z).height;
                from = sp;
            }
        }
        (*z).left = ptr::null_mut();
        (*z).right = ptr::null_mut();
        (*z).parent = ptr::null_mut();
        (*z).in_tree = false;
        self.len -= 1;
        self.retrace(from);
    }

    /// Échéance la plus proche parmi les entités pour lesquelles `eligible(vruntime)` est vrai.
    ///
    /// Si le sous-arbre gauche contient une entité éligible (son `min_vruntime` l'est), la réponse
    /// y est forcément (échéances plus petites) ; sinon c'est le nœud lui-même s'il est éligible ;
    /// sinon on continue à droite. `eligible` doit être monotone : si `v` est éligible, tout
    /// `v' < v` l'est aussi.
    pub fn pick_eligible(&self, eligible: impl Fn(u64) -> bool) -> *mut Entity {
        unsafe {
            // Chemin rapide : la plus proche échéance est souvent éligible.
            let first = self.leftmost;
            if !first.is_null() && eligible((*first).vruntime) {
                return first;
            }
            let mut n = self.root;
            while !n.is_null() {
                let l = (*n).left;
                if !l.is_null() && eligible((*l).min_vruntime) {
                    n = l;
                    continue;
                }
                if eligible((*n).vruntime) {
                    return n;
                }
                n = (*n).right;
            }
            ptr::null_mut()
        }
    }

    /// Dernière entité (échéance la plus lointaine) : candidate naturelle à la migration, car la
    /// moins pressée et la plus froide en cache.
    pub fn last(&self) -> *mut Entity {
        let mut n = self.root;
        if n.is_null() {
            return n;
        }
        unsafe {
            while !(*n).right.is_null() {
                n = (*n).right;
            }
        }
        n
    }

    /// Parcours en ordre (échéances croissantes). Usage : diagnostic, vol de tâche filtré.
    pub fn for_each(&self, mut f: impl FnMut(*mut Entity) -> bool) {
        unsafe {
            let mut n = self.leftmost;
            while !n.is_null() {
                if !f(n) {
                    return;
                }
                n = successor(n);
            }
        }
    }

    /// Vérifie toutes les invariants (tests et assertions de débogage). Renvoie le nombre de nœuds.
    pub fn validate(&self) -> Result<usize, &'static str> {
        unsafe fn check(n: *mut Entity, parent: *mut Entity) -> Result<(i32, usize), &'static str> {
            if n.is_null() {
                return Ok((0, 0));
            }
            if (*n).parent != parent { return Err("parent incohérent"); }
            if !(*n).in_tree { return Err("in_tree faux"); }
            let (hl, cl) = check((*n).left, n)?;
            let (hr, cr) = check((*n).right, n)?;
            if (hl - hr).abs() > 1 { return Err("déséquilibre AVL"); }
            if (*n).height != 1 + hl.max(hr) { return Err("hauteur fausse"); }
            let l = (*n).left;
            let r = (*n).right;
            if !l.is_null() && !less(l, n) { return Err("ordre gauche"); }
            if !r.is_null() && !less(n, r) { return Err("ordre droit"); }
            let mut m = (*n).vruntime;
            if !l.is_null() { m = vt_min(m, (*l).min_vruntime); }
            if !r.is_null() { m = vt_min(m, (*r).min_vruntime); }
            if m != (*n).min_vruntime { return Err("augmentation fausse"); }
            Ok((1 + hl.max(hr), 1 + cl + cr))
        }
        unsafe {
            let (_, count) = check(self.root, ptr::null_mut())?;
            if count != self.len { return Err("longueur fausse"); }
            let lm = if self.root.is_null() { ptr::null_mut() } else { leftmost_of(self.root) };
            if lm != self.leftmost { return Err("cache du plus à gauche faux"); }
            Ok(count)
        }
    }
}

unsafe fn leftmost_of(mut n: *mut Entity) -> *mut Entity {
    while !(*n).left.is_null() {
        n = (*n).left;
    }
    n
}

unsafe fn successor(n: *mut Entity) -> *mut Entity {
    if !(*n).right.is_null() {
        return leftmost_of((*n).right);
    }
    let mut c = n;
    let mut p = (*n).parent;
    while !p.is_null() && (*p).right == c {
        c = p;
        p = (*p).parent;
    }
    p
}
