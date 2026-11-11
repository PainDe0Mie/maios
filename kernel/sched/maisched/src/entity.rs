//! Entité d'ordonnancement : l'état que l'ordonnanceur garde pour chaque tâche.
//!
//! L'entité est **intrusive** : elle vit dans la structure de la tâche (comme `sched_entity` dans
//! Linux), et les files d'attente ne font que la chaîner par pointeurs. Conséquences :
//! * aucune allocation sur le chemin chaud (mise en file, retrait, choix), donc rien d'alloué
//!   en contexte d'interruption ;
//! * retrait en O(log n) sans recherche (on part du nœud lui-même) ;
//! * une seule propriété : l'entité appartient à la file du cœur `cpu`, et n'est modifiée que
//!   sous le verrou de cette file.

use core::ptr;

/// Poids d'une tâche de nice 0 (Linux `NICE_0_LOAD`, résolution réduite).
pub const NICE_0_WEIGHT: u32 = 1024;

/// Poids par valeur de nice [-20, 19] : chaque cran change la part de CPU d'environ 10 %
/// (facteur 1,25 entre deux crans). Table identique à `sched_prio_to_weight` de Linux, pour que les
/// applications portées gardent le même comportement relatif.
pub const NICE_TO_WEIGHT: [u32; 40] = [
    88761, 71755, 56483, 46273, 36291,
    29154, 23254, 18705, 14949, 11916,
     9548,  7620,  6100,  4904,  3906,
     3121,  2501,  1991,  1586,  1277,
     1024,   820,   655,   526,   423,
      335,   272,   215,   172,   137,
      110,    87,    70,    56,    45,
       36,    29,    23,    18,    15,
];

/// Poids d'une valeur de nice (bornée à [-20, 19]).
#[inline]
pub fn weight_of_nice(nice: i8) -> u32 {
    NICE_TO_WEIGHT[(nice.clamp(-20, 19) + 20) as usize]
}

/// Classe d'ordonnancement, par priorité stricte décroissante : `Rt` > `Fair` > `Idle`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// Temps partagé EEVDF. `batch` : tâche de fond, tranche longue et jamais de préemption au
    /// réveil (débit plutôt que latence, comme `SCHED_BATCH`).
    Fair { batch: bool },
    /// Temps réel POSIX, priorité 1 (basse) à 99 (haute). `rr` : tourniquet entre tâches de même
    /// priorité (`SCHED_RR`), sinon FIFO (`SCHED_FIFO`).
    Rt { prio: u8, rr: bool },
    /// Tâche inactive d'un cœur : ne s'exécute que si rien d'autre n'est prêt.
    Idle,
}

/// Mode de calcul de la tranche demandée à EEVDF.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SliceMode {
    /// Tranche déduite du comportement : voir [`Entity::adapt_slice`].
    Adaptive,
    /// Tranche fixée par l'application (équivalent de `sched_attr.sched_runtime` de Linux 6.12).
    Fixed(u64),
}

/// État d'ordonnancement d'une tâche. Tous les temps virtuels sont des `u64` comparés en
/// arithmétique modulaire (`vt_lt`), comme dans Linux : aucun débordement à craindre.
pub struct Entity {
    // --- chaînage dans l'arbre EEVDF (privé à `tree`) ---
    pub(crate) left: *mut Entity,
    pub(crate) right: *mut Entity,
    pub(crate) parent: *mut Entity,
    pub(crate) height: i32,
    /// Augmentation : plus petit `vruntime` du sous-arbre (élague la recherche d'éligibilité).
    pub(crate) min_vruntime: u64,

    // --- chaînage dans la file temps réel (privé à `rt`) ---
    pub(crate) rt_next: *mut Entity,
    pub(crate) rt_prev: *mut Entity,

    /// Identifiant unique (départage les égalités de façon stable).
    pub id: u64,
    /// Pointeur opaque vers le propriétaire (la tâche), géré par le noyau.
    pub owner: usize,
    pub class: Class,
    pub slice_mode: SliceMode,

    /// Poids (proportion de CPU) et son inverse en virgule fixe : `vmul = (1024 << 32) / poids`,
    /// pour convertir un temps réel en temps virtuel par une multiplication.
    pub weight: u32,
    pub(crate) vmul: u64,

    /// Temps virtuel consommé.
    pub vruntime: u64,
    /// Échéance virtuelle : `vruntime au placement + tranche / poids`.
    pub deadline: u64,
    /// Retard (« lag ») conservé quand la tâche quitte la file : `V - vruntime`, borné.
    pub vlag: i64,
    /// Tranche demandée, en ns réelles.
    pub slice: u64,

    /// Temps réel cumulé passé sur un CPU (ns).
    pub sum_exec: u64,
    /// Horodatage du début de la période de calcul en cours (ns).
    pub exec_start: u64,
    /// `sum_exec` au moment où la tâche a obtenu le CPU (protection de tranche).
    pub(crate) prev_sum_exec: u64,
    /// `sum_exec` au dernier réveil : la rafale en cours est `sum_exec - burst_start`.
    pub(crate) burst_start: u64,
    /// Moyenne mobile exponentielle (1/8) de la durée des rafales de calcul entre deux sommeils.
    pub burst_ewma: u64,
    /// Nombre de rafales observées (pour amorcer la moyenne).
    pub(crate) bursts: u32,

    /// Tranche restante d'une tâche `SCHED_RR` (ns).
    pub rr_left: u64,

    /// Présente dans une file (arbre, liste temps réel, ou en cours d'exécution).
    pub on_rq: bool,
    /// Endormie mais laissée dans l'arbre jusqu'à épuisement de son retard négatif (Linux 6.12
    /// `DELAY_DEQUEUE`) : un sommeil ne sert pas à effacer une dette de CPU.
    pub delayed: bool,
    pub(crate) in_tree: bool,
}

// SAFETY : les pointeurs bruts ne sont suivis que sous le verrou de la file propriétaire.
unsafe impl Send for Entity {}
unsafe impl Sync for Entity {}

impl Entity {
    pub const fn new(id: u64, class: Class) -> Entity {
        Entity {
            left: ptr::null_mut(),
            right: ptr::null_mut(),
            parent: ptr::null_mut(),
            height: 0,
            min_vruntime: 0,
            rt_next: ptr::null_mut(),
            rt_prev: ptr::null_mut(),
            id,
            owner: 0,
            class,
            slice_mode: SliceMode::Adaptive,
            weight: NICE_0_WEIGHT,
            vmul: 1u64 << 32,
            vruntime: 0,
            deadline: 0,
            vlag: 0,
            slice: crate::BASE_SLICE_NS,
            sum_exec: 0,
            exec_start: 0,
            prev_sum_exec: 0,
            burst_start: 0,
            burst_ewma: 0,
            bursts: 0,
            rr_left: crate::RR_TIMESLICE_NS,
            on_rq: false,
            delayed: false,
            in_tree: false,
        }
    }

    /// Change le poids. À n'appeler que hors file, ou sous le verrou via `FairRq::reweight`.
    pub fn set_weight(&mut self, weight: u32) {
        let w = weight.max(1);
        self.weight = w;
        self.vmul = ((NICE_0_WEIGHT as u64) << 32) / w as u64;
    }

    pub fn set_nice(&mut self, nice: i8) {
        self.set_weight(weight_of_nice(nice));
    }

    /// Temps réel -> temps virtuel (`delta * 1024 / poids`) par multiplication en virgule fixe.
    #[inline]
    pub fn to_virtual(&self, delta_ns: u64) -> u64 {
        if self.weight == NICE_0_WEIGHT {
            return delta_ns;
        }
        ((delta_ns as u128 * self.vmul as u128) >> 32) as u64
    }

    /// Tranche demandée en temps virtuel.
    #[inline]
    pub fn vslice(&self) -> u64 {
        self.to_virtual(self.slice)
    }

    /// Dans l'arbre EEVDF (diagnostic).
    pub fn in_tree(&self) -> bool {
        self.in_tree
    }

    pub fn is_fair(&self) -> bool {
        matches!(self.class, Class::Fair { .. })
    }

    pub fn is_batch(&self) -> bool {
        matches!(self.class, Class::Fair { batch: true })
    }

    /// Tranches adaptatives : la tranche demandée suit la durée typique des rafales de calcul.
    ///
    /// Dans EEVDF (Stoica et al. 1995), la tranche demandée ne change pas la *part* de CPU d'une
    /// tâche, seulement la *granularité* : une petite tranche donne des échéances proches, donc une
    /// faible latence, au prix de tours plus courts. Linux 6.12 laisse l'application la régler à la
    /// main (`sched_attr`) ; Windows donne des « boosts » de priorité heuristiques qui cassent
    /// l'équité. MaiSched la déduit du comportement, comme le critère de latence de `scx_lavd`
    /// (sched_ext, 2024) le fait à partir des fréquences de réveil et des durées d'exécution :
    /// une tâche interactive qui calcule 200 µs puis dort obtient une tranche de ~300 µs, et passe
    /// devant les tâches de calcul dès son réveil, sans jamais dépasser sa part équitable.
    ///
    /// Appelée quand la tâche s'endort ; la rafale qui se termine nourrit la moyenne mobile.
    pub fn adapt_slice(&mut self) {
        let burst = self.sum_exec.saturating_sub(self.burst_start);
        self.burst_ewma = if self.bursts == 0 { burst } else { (7 * self.burst_ewma + burst) / 8 };
        self.bursts = self.bursts.saturating_add(1);
        self.recompute_slice();
    }

    pub fn recompute_slice(&mut self) {
        self.slice = match (self.slice_mode, self.class) {
            (SliceMode::Fixed(ns), _) => ns.clamp(crate::MIN_SLICE_NS, crate::MAX_SLICE_NS),
            (_, Class::Fair { batch: true }) => crate::BATCH_SLICE_NS,
            _ if self.bursts == 0 => crate::BASE_SLICE_NS,
            // 1,5 × la rafale typique : la plupart des rafales tiennent dans une seule tranche.
            _ => (self.burst_ewma + self.burst_ewma / 2).clamp(crate::MIN_SLICE_NS, crate::BASE_SLICE_NS),
        };
    }
}

/// `a < b` en temps virtuel modulaire.
#[inline(always)]
pub fn vt_lt(a: u64, b: u64) -> bool {
    (a.wrapping_sub(b) as i64) < 0
}

/// Plus petit des deux temps virtuels.
#[inline(always)]
pub fn vt_min(a: u64, b: u64) -> u64 {
    if vt_lt(a, b) { a } else { b }
}

/// Plus grand des deux temps virtuels.
#[inline(always)]
pub fn vt_max(a: u64, b: u64) -> u64 {
    if vt_lt(a, b) { b } else { a }
}
