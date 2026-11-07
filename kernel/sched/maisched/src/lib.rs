//! MaiSched — cœur algorithmique de l'ordonnanceur de MaiOS.
//!
//! Cette crate ne dépend de rien (ni du noyau, ni d'un allocateur sur le chemin chaud) : elle est
//! testée sur l'hôte (`cargo test`), comme `llfree`. Le noyau (`task::scheduler`) l'utilise sous
//! le verrou de la file de chaque cœur et y ajoute le réveil inter-cœurs, la préemption, le vol de
//! tâches et l'équilibrage.
//!
//! * [`fair`] : EEVDF fidèle à Linux 6.12 (retard conservé, sommeil différé, `RUN_TO_PARITY`,
//!   `PREEMPT_SHORT`), arbre trié par échéance et augmenté du plus petit `vruntime` ;
//! * [`entity`] : entité intrusive, poids, **tranches adaptatives** (latence déduite du
//!   comportement, sans réglage manuel ni « boost » qui casse l'équité) ;
//! * [`rt`] : `SCHED_FIFO` / `SCHED_RR` en O(1) (masque de priorités) ;
//! * [`cpumask`] : masques de cœurs atomiques (cœurs inactifs) ;
//! * [`topology`] : distances de cache (SMT, LLC, NUMA) pour placer et voler au plus près.
//!
//! Le plan et les références sont dans `.claude/plans/03-maisched-ordonnanceur.md`.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod cpumask;
pub mod entity;
pub mod fair;
pub mod rt;
pub mod topology;
pub mod tree;

#[cfg(test)]
mod tests;

pub use entity::{weight_of_nice, Class, Entity, SliceMode, NICE_0_WEIGHT, NICE_TO_WEIGHT};
pub use fair::{flags, Dequeued, FairRq};
pub use rt::RtRq;

/// Période du tick d'ordonnancement (doit refléter `CONFIG_TIMESLICE_PERIOD_MICROSECONDS`).
pub const TICK_NS: u64 = 2_000_000;

/// Tranche de base : celle de Linux sur 8 cœurs et plus (`0,75 ms × (1 + log₂ 8)`). C'est aussi la
/// tranche des tâches de calcul pur, qui ne s'endorment jamais.
pub const BASE_SLICE_NS: u64 = 3_000_000;

/// Plus petite tranche (borne basse de Linux pour `sched_attr`) : en dessous, le coût des
/// changements de contexte domine.
pub const MIN_SLICE_NS: u64 = 100_000;

/// Plus grande tranche qu'une application peut demander.
pub const MAX_SLICE_NS: u64 = 100_000_000;

/// Tranche des tâches de fond (`SCHED_BATCH`) : moins de changements, plus de débit, comme le
/// quantum allongé des services sous Windows Server.
pub const BATCH_SLICE_NS: u64 = 12_000_000;

/// Tranche `SCHED_RR` (Linux : 100 ms).
pub const RR_TIMESLICE_NS: u64 = 100_000_000;
