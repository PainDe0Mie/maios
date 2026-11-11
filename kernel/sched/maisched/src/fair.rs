//! File EEVDF (Earliest Eligible Virtual Deadline First) d'un cœur.
//!
//! Algorithme : Stoica & Abdel-Wahab, « Earliest Eligible Virtual Deadline First: A Flexible and
//! Accurate Mechanism for Proportional Share Resource Allocation » (1995), dans la forme de
//! Linux 6.6 → 6.12 (Peter Zijlstra) :
//! * `V` est la moyenne pondérée des `vruntime` des tâches en file (tâche courante comprise),
//!   tenue incrémentalement par `Σ wᵢ·(vᵢ − v₀)` et `Σ wᵢ` (aucune division hors du calcul de `V`) ;
//! * une tâche est **éligible** si son retard `V − vᵢ` est positif ou nul ;
//! * on choisit l'échéance virtuelle la plus proche parmi les éligibles ;
//! * quand une tâche quitte la file, son retard est conservé (`vlag`, borné) et restitué quand
//!   elle revient (`PLACE_LAG`), avec la correction de poids `(W + w) / W` ;
//! * une tâche qui s'endort avec un retard négatif reste dans l'arbre jusqu'à ce qu'elle
//!   redevienne éligible (`DELAY_DEQUEUE`) : dormir n'efface pas une dette ;
//! * la tâche courante garde le CPU jusqu'à la fin de sa tranche (`RUN_TO_PARITY`), sauf face à
//!   une tâche éligible à tranche plus courte et échéance plus proche (`PREEMPT_SHORT`).
//!
//! Ajouts MaiSched : tranches adaptatives (voir `Entity::adapt_slice`) et file sans allocation.

use core::ptr;
use crate::entity::{vt_lt, vt_max, vt_min, Entity};
use crate::tree::Tree;

/// Indicateurs de mise en file.
pub mod flags {
    /// Réveil après un sommeil.
    pub const WAKEUP: u32 = 1;
    /// Première mise en file d'une nouvelle tâche : échéance à mi-tranche (`PLACE_DEADLINE_INITIAL`).
    pub const INITIAL: u32 = 2;
    /// Arrivée par migration depuis un autre cœur (le retard est conservé tel quel).
    pub const MIGRATED: u32 = 4;
}

/// Résultat d'un retrait pour sommeil.
#[derive(Debug, PartialEq, Eq)]
pub enum Dequeued {
    /// La tâche a quitté la file.
    Gone,
    /// La tâche est restée dans l'arbre (retard négatif) : `DELAY_DEQUEUE`.
    Delayed,
}

pub struct FairRq {
    tree: Tree,
    /// Tâche en cours d'exécution (hors de l'arbre), ou nulle.
    curr: *mut Entity,
    /// Origine des clés : plus petit `vruntime` suivi, ne fait qu'avancer.
    zero_vruntime: u64,
    /// `Σ wᵢ · (vᵢ − zero_vruntime)` sur les entités de l'arbre.
    sum_w_vruntime: i128,
    /// `Σ wᵢ` sur les entités de l'arbre.
    sum_weight: u64,
    /// Entités en file : arbre + courante. Les entités différées comptent (elles sont dans l'arbre).
    nr_queued: u32,
    /// Entités différées (`DELAY_DEQUEUE`) encore dans l'arbre.
    nr_delayed: u32,
}

// SAFETY : manipulée uniquement sous le verrou de la file du cœur.
unsafe impl Send for FairRq {}

impl FairRq {
    pub const fn new() -> FairRq {
        FairRq {
            tree: Tree::new(),
            curr: ptr::null_mut(),
            zero_vruntime: 0,
            sum_w_vruntime: 0,
            sum_weight: 0,
            nr_queued: 0,
            nr_delayed: 0,
        }
    }

    /// Tâches réellement prêtes (différées exclues), courante comprise.
    #[inline]
    pub fn nr_runnable(&self) -> u32 { self.nr_queued - self.nr_delayed }

    #[inline]
    pub fn nr_queued(&self) -> u32 { self.nr_queued }

    #[inline]
    pub fn curr(&self) -> *mut Entity { self.curr }

    #[inline]
    pub fn tree(&self) -> &Tree { &self.tree }

    #[inline]
    fn key(&self, v: u64) -> i64 { v.wrapping_sub(self.zero_vruntime) as i64 }

    unsafe fn avg_add(&mut self, e: *mut Entity) {
        let w = (*e).weight as u64;
        self.sum_w_vruntime += self.key((*e).vruntime) as i128 * w as i128;
        self.sum_weight += w;
    }

    unsafe fn avg_sub(&mut self, e: *mut Entity) {
        let w = (*e).weight as u64;
        self.sum_w_vruntime -= self.key((*e).vruntime) as i128 * w as i128;
        self.sum_weight -= w;
    }

    /// Somme et charge, tâche courante comprise si elle est en file.
    #[inline]
    fn sums(&self) -> (i128, u64) {
        let (mut avg, mut load) = (self.sum_w_vruntime, self.sum_weight);
        let c = self.curr;
        if !c.is_null() {
            unsafe {
                if (*c).on_rq {
                    let w = (*c).weight as u64;
                    avg += self.key((*c).vruntime) as i128 * w as i128;
                    load += w;
                }
            }
        }
        (avg, load)
    }

    /// `V` : moyenne pondérée des `vruntime` en file (arrondie vers le bas).
    pub fn avg_vruntime(&self) -> u64 {
        let (avg, load) = self.sums();
        if load == 0 {
            return if self.curr.is_null() { self.zero_vruntime } else { unsafe { (*self.curr).vruntime } };
        }
        let l = load as i128;
        let q = if avg < 0 { (avg - (l - 1)) / l } else { avg / l };
        self.zero_vruntime.wrapping_add(q as i64 as u64)
    }

    /// `vruntime` éligible ⇔ `V ≥ v` ⇔ `Σ wᵢ(vᵢ − v₀) ≥ (v − v₀) · Σ wᵢ` (sans division).
    #[inline]
    pub fn vruntime_eligible(&self, v: u64) -> bool {
        let (avg, load) = self.sums();
        avg >= self.key(v) as i128 * load as i128
    }

    /// Éligibilité de l'entité `e`.
    ///
    /// # Safety
    /// `e` pointe sur une entité valide (en file ou courante), lue sous le verrou de sa file.
    #[inline]
    pub unsafe fn entity_eligible(&self, e: *const Entity) -> bool {
        self.vruntime_eligible((*e).vruntime)
    }

    /// Avance l'origine des clés (Linux `update_min_vruntime`) et recale la somme pondérée.
    fn update_zero(&mut self) {
        let mut cand: Option<u64> = None;
        unsafe {
            let c = self.curr;
            if !c.is_null() && (*c).on_rq {
                cand = Some((*c).vruntime);
            }
        }
        if let Some(m) = self.tree.min_vruntime() {
            cand = Some(match cand { Some(c) => vt_min(c, m), None => m });
        }
        if let Some(c) = cand {
            let nz = vt_max(self.zero_vruntime, c);
            let delta = nz.wrapping_sub(self.zero_vruntime) as i64;
            if delta != 0 {
                self.sum_w_vruntime -= self.sum_weight as i128 * delta as i128;
                self.zero_vruntime = nz;
            }
        }
    }

    /// Impute le temps écoulé à la tâche courante. Renvoie `true` si sa tranche est épuisée et
    /// qu'une autre tâche attend : il faut replanifier.
    pub fn update_curr(&mut self, now: u64) -> bool {
        let c = self.curr;
        if c.is_null() {
            return false;
        }
        unsafe {
            let delta = now.saturating_sub((*c).exec_start);
            if delta == 0 {
                return false;
            }
            (*c).exec_start = now;
            (*c).sum_exec += delta;
            (*c).vruntime = (*c).vruntime.wrapping_add((*c).to_virtual(delta));
            let expired = !vt_lt((*c).vruntime, (*c).deadline);
            if expired {
                // Nouvelle requête : la prochaine échéance est une tranche plus loin.
                (*c).deadline = (*c).vruntime.wrapping_add((*c).vslice());
            }
            self.update_zero();
            expired && self.nr_runnable() > 1
        }
    }

    /// Place une entité qui (re)joint la file : `v = V − lag·(W + w)/W`, échéance à une tranche.
    unsafe fn place(&mut self, e: *mut Entity, fl: u32) {
        let v = self.avg_vruntime();
        let mut lag: i64 = 0;
        let (_, load) = self.sums();
        if load > 0 {
            // La tâche qui revient rejoint une file dont elle augmente le poids total : on gonfle son
            // retard pour qu'il reste le même une fois intégrée (Linux, `place_entity`).
            let w = (*e).weight as i128;
            lag = ((*e).vlag as i128 * (load as i128 + w) / load as i128) as i64;
        }
        (*e).vruntime = v.wrapping_sub(lag as u64);
        let mut vslice = (*e).vslice();
        if fl & flags::INITIAL != 0 {
            vslice /= 2;
        }
        (*e).deadline = (*e).vruntime.wrapping_add(vslice);
    }

    /// Calcule et borne le retard d'une entité qui quitte la file : `|lag| ≤ max(2·tranche, tick)`
    /// en temps virtuel (Linux `update_entity_lag`).
    unsafe fn save_lag(&mut self, e: *mut Entity) {
        let limit = (*e).to_virtual(((*e).slice * 2).max(crate::TICK_NS)) as i64;
        let lag = self.avg_vruntime().wrapping_sub((*e).vruntime) as i64;
        (*e).vlag = lag.clamp(-limit, limit);
    }

    /// Met une entité en file. Une entité différée qui se réveille est simplement réactivée.
    ///
    /// # Safety
    /// `e` est valide, n'est dans aucune autre file, et le reste tant qu'elle est ici.
    pub unsafe fn enqueue(&mut self, e: *mut Entity, fl: u32, now: u64) {
        if (*e).delayed {
            debug_assert!((*e).in_tree);
            (*e).delayed = false;
            self.nr_delayed -= 1;
            (*e).burst_start = (*e).sum_exec;
            return;
        }
        debug_assert!(!(*e).on_rq);
        if fl & flags::WAKEUP != 0 {
            (*e).burst_start = (*e).sum_exec;
        }
        self.update_curr(now);
        self.place(e, fl);
        (*e).on_rq = true;
        self.nr_queued += 1;
        self.avg_add(e);
        self.tree.insert(e);
        self.update_zero();
    }

    /// Retrait définitif d'une entité de l'arbre (migration, sortie, différée arrivée à échéance).
    unsafe fn dequeue_from_tree(&mut self, e: *mut Entity) {
        self.save_lag(e);
        if (*e).delayed {
            (*e).delayed = false;
            self.nr_delayed -= 1;
            // `DELAY_ZERO` : une entité différée ne garde pas de crédit en partant.
            if (*e).vlag > 0 {
                (*e).vlag = 0;
            }
        }
        self.tree.remove(e);
        self.avg_sub(e);
        (*e).on_rq = false;
        self.nr_queued -= 1;
        self.update_zero();
    }

    /// La tâche courante s'endort (`sleep = true`) ou quitte la file pour de bon.
    ///
    /// # Safety
    /// `e` est la tâche courante de cette file.
    pub unsafe fn dequeue_curr(&mut self, e: *mut Entity, now: u64, sleep: bool) -> Dequeued {
        debug_assert!(e == self.curr);
        self.update_curr(now);
        if sleep {
            (*e).adapt_slice();
            if !self.entity_eligible(e) {
                // Retard négatif : elle reste dans l'arbre jusqu'à redevenir éligible.
                self.curr = ptr::null_mut();
                (*e).delayed = true;
                self.nr_delayed += 1;
                self.avg_add(e);
                self.tree.insert(e);
                self.update_zero();
                return Dequeued::Delayed;
            }
        }
        self.save_lag(e);
        (*e).on_rq = false;
        self.nr_queued -= 1;
        self.curr = ptr::null_mut();
        self.update_zero();
        Dequeued::Gone
    }

    /// Retire une entité qui n'est pas la courante (tâche bloquée de l'extérieur, tuée, migrée).
    ///
    /// # Safety
    /// `e` est dans l'arbre de cette file.
    pub unsafe fn dequeue_queued(&mut self, e: *mut Entity, now: u64) {
        debug_assert!(e != self.curr && (*e).in_tree);
        self.update_curr(now);
        self.dequeue_from_tree(e);
    }

    /// La tâche courante rend le CPU en restant prête : elle retourne dans l'arbre.
    /// `yielded` : abandon volontaire (`sched_yield`), on repousse son échéance d'une tranche.
    ///
    /// # Safety
    /// `e` est la tâche courante de cette file.
    pub unsafe fn put_prev(&mut self, e: *mut Entity, now: u64, yielded: bool) {
        debug_assert!(e == self.curr);
        self.update_curr(now);
        if yielded {
            // Abandon volontaire : l'échéance recule d'une tranche, mais jamais au-delà de deux
            // tranches devant le `vruntime`. Sans cette borne, une tâche qui attend en boucle
            // (`join`, attente active) accumulerait une échéance lointaine et, une fois au travail,
            // ne serait plus jamais jugée en fin de tranche.
            let vs = (*e).vslice();
            let pushed = vt_max((*e).vruntime, (*e).deadline).wrapping_add(vs);
            (*e).deadline = vt_min(pushed, (*e).vruntime.wrapping_add(2 * vs));
        }
        self.curr = ptr::null_mut();
        self.avg_add(e);
        self.tree.insert(e);
        self.update_zero();
    }

    /// Choisit, retire de l'arbre et installe comme courante la prochaine entité. Les entités
    /// différées rencontrées sont sorties de la file au passage ; elles sont renvoyées par
    /// `on_delayed_gone` pour que le noyau libère sa référence.
    ///
    /// # Safety
    /// Aucune entité courante (elle a été remise par `put_prev` ou retirée).
    pub unsafe fn pick_next(&mut self, now: u64, mut on_delayed_gone: impl FnMut(*mut Entity)) -> *mut Entity {
        debug_assert!(self.curr.is_null());
        loop {
            let e = self.pick_eevdf_tree();
            if e.is_null() {
                return e;
            }
            if (*e).delayed {
                self.dequeue_from_tree(e);
                on_delayed_gone(e);
                continue;
            }
            self.tree.remove(e);
            self.avg_sub(e);
            self.curr = e;
            (*e).exec_start = now;
            (*e).prev_sum_exec = (*e).sum_exec;
            self.update_zero();
            return e;
        }
    }

    /// Installe comme courante une entité déjà dans l'arbre (tâche qui tournait déjà quand le
    /// cœur a été confié à l'ordonnanceur, typiquement la tâche d'amorçage).
    ///
    /// # Safety
    /// Aucune entité courante ; `e` est dans l'arbre et non différée.
    pub unsafe fn take_as_curr(&mut self, e: *mut Entity, now: u64) {
        debug_assert!(self.curr.is_null() && (*e).in_tree && !(*e).delayed);
        self.tree.remove(e);
        self.avg_sub(e);
        self.curr = e;
        (*e).exec_start = now;
        (*e).prev_sum_exec = (*e).sum_exec;
        self.update_zero();
    }

    /// Recherche EEVDF dans l'arbre seul.
    /// Diagnostic : ce que choisirait `pick_next` (sans rien modifier).
    ///
    /// # Safety
    /// Verrou de la file tenu.
    pub unsafe fn debug_first(&self) -> alloc::string::String {
        let e = self.pick_eevdf_tree();
        alloc::format!(
            "arbre {} entites, file {} (differees {}), courante {}, choix {}",
            self.tree.len(),
            self.nr_queued,
            self.nr_delayed,
            !self.curr.is_null(),
            if e.is_null() { alloc::string::String::from("aucun") } else { alloc::format!("differee {} on_rq {}", (*e).delayed, (*e).on_rq) }
        )
    }

    fn pick_eevdf_tree(&self) -> *mut Entity {
        if self.tree.is_empty() {
            return ptr::null_mut();
        }
        let (avg, load) = self.sums();
        let zero = self.zero_vruntime;
        let e = self.tree.pick_eligible(|v| avg >= (v.wrapping_sub(zero) as i64) as i128 * load as i128);
        if e.is_null() {
            // Impossible en théorie (au moins une entité a un retard ≥ 0) ; on se rabat sur
            // l'échéance la plus proche pour rester vivant face aux arrondis.
            self.tree.first()
        } else {
            e
        }
    }

    /// La tâche courante a-t-elle encore droit à sa tranche (`RUN_TO_PARITY`) ?
    /// Protégée tant qu'elle n'a pas consommé sa tranche depuis qu'elle a obtenu le CPU.
    unsafe fn curr_protected(&self) -> bool {
        let c = self.curr;
        !c.is_null() && (*c).sum_exec.saturating_sub((*c).prev_sum_exec) < (*c).slice
    }

    /// Contrôle au tick : la courante a consommé sa tranche protégée et une autre entité éligible a
    /// une échéance plus proche (ou la courante n'est plus éligible). Filet de sécurité en plus de
    /// la fin d'échéance de `update_curr` : aucune tâche ne garde le CPU au-delà de sa part.
    pub fn tick_should_resched(&self) -> bool {
        let c = self.curr;
        if c.is_null() || self.tree.is_empty() {
            return false;
        }
        unsafe {
            if self.curr_protected() {
                return false;
            }
            let best = self.pick_eevdf_tree();
            if best.is_null() {
                return false;
            }
            !self.entity_eligible(c) || vt_lt((*best).deadline, (*c).deadline)
        }
    }

    /// Faut-il préempter la tâche courante au profit de `e`, qui vient d'entrer en file ?
    /// (Linux `check_preempt_wakeup_fair`.)
    ///
    /// # Safety
    /// `e` est dans l'arbre de cette file.
    pub unsafe fn should_preempt(&mut self, e: *mut Entity, now: u64) -> bool {
        let c = self.curr;
        if c.is_null() {
            return true;
        }
        // Les tâches de fond ne préemptent pas, et la tâche de fond courante cède toujours.
        if (*e).is_batch() {
            return false;
        }
        self.update_curr(now);
        if (*c).is_batch() {
            return true;
        }
        let cur_ok = (*c).on_rq && self.entity_eligible(c);
        // PREEMPT_SHORT : une tâche éligible à tranche plus courte et échéance plus proche lève la
        // protection de tranche de la courante.
        if (*e).slice < (*c).slice && self.entity_eligible(e) && vt_lt((*e).deadline, (*c).deadline) {
            return true;
        }
        if cur_ok && self.curr_protected() {
            return false;
        }
        let best = self.pick_eevdf_tree();
        if best != e {
            return false;
        }
        !cur_ok || vt_lt((*e).deadline, (*c).deadline)
    }

    /// Change le poids d'une entité en file sans fausser `V` (Linux `reweight_entity`) : le retard
    /// est conservé en le rapportant au nouveau poids.
    ///
    /// # Safety
    /// `e` est en file ici (arbre ou courante).
    pub unsafe fn reweight(&mut self, e: *mut Entity, weight: u32, now: u64) {
        self.update_curr(now);
        let in_tree = (*e).in_tree;
        if in_tree {
            self.tree.remove(e);
            self.avg_sub(e);
        }
        let v = self.avg_vruntime();
        let lag = v.wrapping_sub((*e).vruntime) as i64 as i128;
        let old = (*e).weight as i128;
        let rel_deadline = (*e).deadline.wrapping_sub((*e).vruntime) as i64 as i128;
        (*e).set_weight(weight);
        let new = (*e).weight as i128;
        // Le retard et l'échéance relative se rapportent au nouveau poids (`× ancien / nouveau`,
        // jusqu'à ×5917 entre nice -20 et 19), puis sont bornés comme au retrait de la file : sans
        // borne, des changements de nice répétés font diverger le retard au-delà de 2⁶³ et les
        // clés modulaires débordent.
        let limit = (*e).to_virtual(((*e).slice * 2).max(crate::TICK_NS)) as i128;
        let lag = (lag * old / new).clamp(-limit, limit);
        let rel = (rel_deadline * old / new).clamp(0, 2 * (*e).vslice() as i128);
        (*e).vruntime = v.wrapping_sub(lag as i64 as u64);
        (*e).deadline = (*e).vruntime.wrapping_add(rel as i64 as u64);
        if in_tree {
            self.avg_add(e);
            self.tree.insert(e);
        }
        self.update_zero();
    }

    /// Entité candidate au vol par un autre cœur : la dernière de l'arbre (échéance lointaine),
    /// prête (non différée) et acceptée par `filter` (affinité, tâche encore en cours de sortie de
    /// CPU…).
    pub fn steal_candidate(&self, filter: impl Fn(*mut Entity) -> bool) -> *mut Entity {
        let mut found = ptr::null_mut();
        self.tree.for_each(|e| {
            unsafe {
                if !(*e).delayed && filter(e) {
                    found = e;
                }
            }
            true
        });
        found
    }

    /// Détache une entité de l'arbre pour la migrer (le retard est conservé dans `vlag`).
    ///
    /// # Safety
    /// `e` est dans l'arbre de cette file et n'est pas différée.
    pub unsafe fn detach(&mut self, e: *mut Entity, now: u64) {
        debug_assert!(!(*e).delayed);
        self.dequeue_queued(e, now);
    }

    /// Vérification complète des invariants (tests).
    pub fn validate(&self) -> Result<(), &'static str> {
        let n = self.tree.validate()?;
        let mut sw: i128 = 0;
        let mut sl: u64 = 0;
        let mut delayed = 0u32;
        self.tree.for_each(|e| {
            unsafe {
                sw += self.key((*e).vruntime) as i128 * (*e).weight as i128;
                sl += (*e).weight as u64;
                if (*e).delayed { delayed += 1; }
            }
            true
        });
        if sw != self.sum_w_vruntime || sl != self.sum_weight {
            return Err("somme pondérée désynchronisée");
        }
        if delayed != self.nr_delayed {
            return Err("compteur de différées faux");
        }
        let curr = if self.curr.is_null() { 0 } else { 1 };
        if n as u32 + curr != self.nr_queued {
            return Err("nr_queued faux");
        }
        Ok(())
    }
}
