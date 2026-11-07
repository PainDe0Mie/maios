//! Classe temps réel (`SCHED_FIFO` / `SCHED_RR`, POSIX 1003.1b).
//!
//! 100 listes doublement chaînées intrusives (une par priorité) et un masque de 128 bits des
//! priorités non vides, comme le `rt_prio_array` de Linux et la « ready summary » du répartiteur
//! de Windows NT : mise en file, retrait et choix en O(1) (un `leading_zeros`), sans allocation.

use core::ptr;
use crate::entity::{Class, Entity};

pub const RT_PRIOS: usize = 100;

pub struct RtRq {
    heads: [*mut Entity; RT_PRIOS],
    tails: [*mut Entity; RT_PRIOS],
    bitmap: u128,
    nr: u32,
    curr: *mut Entity,
}

// SAFETY : manipulée uniquement sous le verrou de la file du cœur.
unsafe impl Send for RtRq {}

#[inline]
fn prio_of(e: *const Entity) -> usize {
    unsafe {
        match (*e).class {
            Class::Rt { prio, .. } => (prio as usize).min(RT_PRIOS - 1),
            _ => 0,
        }
    }
}

impl RtRq {
    pub const fn new() -> RtRq {
        RtRq {
            heads: [ptr::null_mut(); RT_PRIOS],
            tails: [ptr::null_mut(); RT_PRIOS],
            bitmap: 0,
            nr: 0,
            curr: ptr::null_mut(),
        }
    }

    /// Tâches prêtes, courante comprise.
    #[inline]
    pub fn nr_runnable(&self) -> u32 { self.nr }

    #[inline]
    pub fn curr(&self) -> *mut Entity { self.curr }

    /// Priorité la plus haute en attente (hors courante).
    #[inline]
    pub fn top_prio(&self) -> Option<usize> {
        if self.bitmap == 0 { None } else { Some(127 - self.bitmap.leading_zeros() as usize) }
    }

    unsafe fn link(&mut self, e: *mut Entity, head: bool) {
        let p = prio_of(e);
        (*e).rt_prev = ptr::null_mut();
        (*e).rt_next = ptr::null_mut();
        if self.heads[p].is_null() {
            self.heads[p] = e;
            self.tails[p] = e;
        } else if head {
            (*e).rt_next = self.heads[p];
            (*self.heads[p]).rt_prev = e;
            self.heads[p] = e;
        } else {
            (*e).rt_prev = self.tails[p];
            (*self.tails[p]).rt_next = e;
            self.tails[p] = e;
        }
        self.bitmap |= 1u128 << p;
    }

    unsafe fn unlink(&mut self, e: *mut Entity) {
        let p = prio_of(e);
        let (prev, next) = ((*e).rt_prev, (*e).rt_next);
        if prev.is_null() { self.heads[p] = next; } else { (*prev).rt_next = next; }
        if next.is_null() { self.tails[p] = prev; } else { (*next).rt_prev = prev; }
        (*e).rt_prev = ptr::null_mut();
        (*e).rt_next = ptr::null_mut();
        if self.heads[p].is_null() {
            self.bitmap &= !(1u128 << p);
        }
    }

    /// # Safety
    /// `e` est valide et hors de toute file.
    pub unsafe fn enqueue(&mut self, e: *mut Entity) {
        debug_assert!(!(*e).on_rq);
        (*e).on_rq = true;
        self.nr += 1;
        self.link(e, false);
    }

    /// Retire une entité en attente (pas la courante).
    ///
    /// # Safety
    /// `e` est dans cette file et n'est pas la courante.
    pub unsafe fn dequeue_queued(&mut self, e: *mut Entity) {
        debug_assert!(e != self.curr);
        self.unlink(e);
        (*e).on_rq = false;
        self.nr -= 1;
    }

    /// La courante quitte la file (sommeil, sortie, changement de classe).
    ///
    /// # Safety
    /// `e` est la courante.
    pub unsafe fn dequeue_curr(&mut self, e: *mut Entity) {
        debug_assert!(e == self.curr);
        self.curr = ptr::null_mut();
        (*e).on_rq = false;
        self.nr -= 1;
    }

    /// La courante rend le CPU en restant prête. Préemptée par plus prioritaire : elle reprend
    /// la tête de sa liste (POSIX) ; tranche RR épuisée ou abandon volontaire : la queue.
    ///
    /// # Safety
    /// `e` est la courante.
    pub unsafe fn put_prev(&mut self, e: *mut Entity, to_tail: bool) {
        debug_assert!(e == self.curr);
        self.curr = ptr::null_mut();
        self.link(e, !to_tail);
    }

    /// # Safety
    /// Aucune courante.
    pub unsafe fn pick_next(&mut self) -> *mut Entity {
        debug_assert!(self.curr.is_null());
        let Some(p) = self.top_prio() else { return ptr::null_mut() };
        let e = self.heads[p];
        self.unlink(e);
        self.curr = e;
        e
    }

    /// Tick : décompte la tranche RR de la courante. Renvoie `true` s'il faut replanifier.
    pub fn tick(&mut self, elapsed_ns: u64) -> bool {
        let c = self.curr;
        if c.is_null() {
            return false;
        }
        unsafe {
            if let Class::Rt { prio, rr: true } = (*c).class {
                if (*c).rr_left > elapsed_ns {
                    (*c).rr_left -= elapsed_ns;
                    return false;
                }
                (*c).rr_left = crate::RR_TIMESLICE_NS;
                // Tourniquet seulement s'il y a une autre tâche de même priorité.
                return self.bitmap & (1u128 << prio.min(99)) != 0;
            }
        }
        false
    }

    /// `e` (qui vient d'entrer) doit-elle préempter la courante ?
    pub fn should_preempt(&self, e: *const Entity) -> bool {
        self.curr.is_null() || prio_of(e) > prio_of(self.curr)
    }

    /// Candidate au vol : la tâche de plus basse priorité en attente acceptée par `filter`.
    pub fn steal_candidate(&self, filter: impl Fn(*mut Entity) -> bool) -> *mut Entity {
        let mut bits = self.bitmap;
        while bits != 0 {
            let p = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let mut e = self.tails[p];
            while !e.is_null() {
                if filter(e) {
                    return e;
                }
                e = unsafe { (*e).rt_prev };
            }
        }
        ptr::null_mut()
    }
}
