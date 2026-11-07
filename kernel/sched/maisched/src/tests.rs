//! Tests hôte du cœur MaiSched : arbre contre modèle naïf, propriétés d'EEVDF (équité, bornes du
//! retard, latence), sommeil différé, classe temps réel.

use crate::entity::{vt_lt, Class, Entity, SliceMode};
use crate::fair::{flags, Dequeued, FairRq};
use crate::rt::RtRq;
use crate::tree::Tree;
use std::boxed::Box;
use std::vec::Vec;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 { self.next() % n }
}

fn fair_entity(id: u64, nice: i8) -> Box<Entity> {
    let mut e = Box::new(Entity::new(id, Class::Fair { batch: false }));
    e.set_nice(nice);
    e
}

// ---------------------------------------------------------------------------
// Arbre
// ---------------------------------------------------------------------------

#[test]
fn tree_matches_model() {
    let mut rng = Rng(0x9E3779B97F4A7C15);
    let mut ents: Vec<Box<Entity>> = (0..300).map(|i| fair_entity(i, 0)).collect();
    let mut tree = Tree::new();
    for round in 0..20_000 {
        let i = rng.below(ents.len() as u64) as usize;
        let e: *mut Entity = &mut *ents[i];
        unsafe {
            if (*e).in_tree {
                tree.remove(e);
            } else {
                // Valeurs proches de 0 pour exercer l'arithmétique modulaire (passage sous zéro).
                (*e).deadline = (rng.below(1_000_000) as i64 - 500_000) as u64;
                (*e).vruntime = (rng.below(1_000_000) as i64 - 500_000) as u64;
                tree.insert(e);
            }
        }
        if round % 97 == 0 {
            tree.validate().unwrap();
        }
        // Choix éligible contre recherche exhaustive.
        let threshold = (rng.below(1_000_000) as i64 - 500_000) as u64;
        let got = tree.pick_eligible(|v| !vt_lt(threshold, v));
        let mut best: *mut Entity = core::ptr::null_mut();
        for b in ents.iter_mut() {
            let p: *mut Entity = &mut **b;
            unsafe {
                if (*p).in_tree && !vt_lt(threshold, (*p).vruntime) {
                    if best.is_null()
                        || vt_lt((*p).deadline, (*best).deadline)
                        || ((*p).deadline == (*best).deadline && (*p).id < (*best).id)
                    {
                        best = p;
                    }
                }
            }
        }
        assert_eq!(got, best, "round {round}");
    }
    tree.validate().unwrap();
}

// ---------------------------------------------------------------------------
// Simulateur d'un cœur : tick périodique, réveils, préemption au réveil.
// ---------------------------------------------------------------------------

struct Sim {
    rq: FairRq,
    now: u64,
}

impl Sim {
    fn new() -> Sim { Sim { rq: FairRq::new(), now: 1_000 } }

    fn resched(&mut self) {
        unsafe {
            let c = self.rq.curr();
            if !c.is_null() {
                self.rq.put_prev(c, self.now, false);
            }
            self.rq.pick_next(self.now, |_| {});
        }
    }
}

#[test]
fn shares_follow_weights() {
    let mut s = Sim::new();
    let nices = [0i8, 0, 5, -3];
    let mut ents: Vec<Box<Entity>> = nices.iter().enumerate().map(|(i, &n)| fair_entity(i as u64, n)).collect();
    for e in ents.iter_mut() {
        unsafe { s.rq.enqueue(&mut **e, flags::INITIAL, s.now) };
    }
    s.resched();
    let total = 20_000_000_000u64; // 20 s simulées
    let tick = crate::TICK_NS;
    let mut max_lag_ns: i64 = 0;
    while s.now < total {
        s.now += tick;
        if s.rq.update_curr(s.now) {
            s.resched();
        }
        s.rq.validate().unwrap();
        // Borne du retard (EEVDF : |lag| ≤ tranche max + granularité du tick), en ns réelles.
        let v = s.rq.avg_vruntime();
        for e in ents.iter() {
            let lag_v = v.wrapping_sub(e.vruntime) as i64;
            let lag_ns = lag_v * e.weight as i64 / 1024;
            max_lag_ns = max_lag_ns.max(lag_ns.abs());
        }
    }
    let wsum: u64 = ents.iter().map(|e| e.weight as u64).sum();
    let tsum: u64 = ents.iter().map(|e| e.sum_exec).sum();
    for e in ents.iter() {
        let expect = e.weight as f64 / wsum as f64;
        let got = e.sum_exec as f64 / tsum as f64;
        assert!((got - expect).abs() < 0.005, "nice/poids {} : part {got:.4}, attendue {expect:.4}", e.weight);
    }
    println!("retard max observé : {max_lag_ns} ns");
    assert!(max_lag_ns <= (crate::BASE_SLICE_NS + 2 * tick) as i64, "retard max {max_lag_ns} ns");
}

/// Une tâche interactive (0,2 ms de calcul toutes les 5 ms) face à deux tâches de calcul :
/// avec les tranches adaptatives, elle doit passer quasi immédiatement à chaque réveil.
#[test]
fn interactive_task_gets_low_latency() {
    let mut s = Sim::new();
    let mut hogs: Vec<Box<Entity>> = (0..2).map(|i| fair_entity(i, 0)).collect();
    let mut inter = fair_entity(10, 0);
    let ip: *mut Entity = &mut *inter;
    for h in hogs.iter_mut() {
        unsafe { s.rq.enqueue(&mut **h, flags::INITIAL, s.now) };
    }
    unsafe { s.rq.enqueue(ip, flags::INITIAL, s.now) };
    s.resched();

    let step = 20_000u64; // 20 µs
    let tick = crate::TICK_NS;
    let burst = 200_000u64;
    let period = 5_000_000u64;
    let mut asleep = false;
    let mut wake_at = 0u64;
    let mut woke = 0u64;
    let mut waiting = false;
    let mut burst_from = 0u64;
    let mut lat: Vec<u64> = Vec::new();
    let mut next_tick = s.now + tick;
    while s.now < 4_000_000_000 {
        s.now += step;
        unsafe {
            if s.rq.curr() == ip {
                if waiting {
                    lat.push(s.now - step - woke);
                    waiting = false;
                    burst_from = (*ip).sum_exec;
                }
                s.rq.update_curr(s.now);
                if (*ip).sum_exec - burst_from >= burst {
                    // Juste après sa rafale, elle a une petite dette (elle a couru alors que V
                    // n'avançait qu'au tiers) : sommeil différé, effacé bien avant le prochain réveil.
                    s.rq.dequeue_curr(ip, s.now, true);
                    asleep = true;
                    wake_at = s.now + period - burst;
                    s.rq.pick_next(s.now, |_| {});
                }
            }
            if asleep && s.now >= wake_at {
                asleep = false;
                s.rq.enqueue(ip, flags::WAKEUP, s.now);
                woke = s.now;
                waiting = true;
                if s.rq.should_preempt(ip, s.now) {
                    s.resched();
                }
            }
        }
        if s.now >= next_tick {
            next_tick += tick;
            if s.rq.update_curr(s.now) {
                s.resched();
            }
        }
    }
    s.rq.validate().unwrap();
    // On ignore la phase d'apprentissage (premières rafales).
    let mut l: Vec<u64> = lat[20..].to_vec();
    l.sort_unstable();
    let p99 = l[l.len() * 99 / 100];
    println!("tranches adaptatives : p50 {} ns, p99 {p99} ns, tranche {} ns", l[l.len() / 2], inter.slice);
    assert!(inter.slice < 500_000, "tranche adaptée attendue ~300 µs, obtenue {}", inter.slice);
    assert!(p99 <= 2 * step, "latence de réveil p99 {p99} ns");
    // Les tâches de calcul se partagent le reste équitablement.
    let (a, b) = (hogs[0].sum_exec as f64, hogs[1].sum_exec as f64);
    assert!((a - b).abs() / (a + b) < 0.01);
}

/// Sans tranches adaptatives (tranche fixe de 3 ms), la même tâche attend nettement plus :
/// c'est le gain mesuré de MaiSched sur EEVDF « nu ».
#[test]
fn fixed_slice_has_higher_latency() {
    let mut s = Sim::new();
    let mut hogs: Vec<Box<Entity>> = (0..2).map(|i| fair_entity(i, 0)).collect();
    let mut inter = fair_entity(10, 0);
    inter.slice_mode = SliceMode::Fixed(crate::BASE_SLICE_NS);
    inter.recompute_slice();
    let ip: *mut Entity = &mut *inter;
    for h in hogs.iter_mut() {
        unsafe { s.rq.enqueue(&mut **h, flags::INITIAL, s.now) };
    }
    unsafe { s.rq.enqueue(ip, flags::INITIAL, s.now) };
    s.resched();
    let step = 20_000u64;
    let tick = crate::TICK_NS;
    let (mut asleep, mut wake_at, mut woke, mut waiting, mut burst_from) = (false, 0u64, 0u64, false, 0u64);
    let mut lat: Vec<u64> = Vec::new();
    let mut next_tick = s.now + tick;
    while s.now < 4_000_000_000 {
        s.now += step;
        unsafe {
            if s.rq.curr() == ip {
                if waiting {
                    lat.push(s.now - step - woke);
                    waiting = false;
                    burst_from = (*ip).sum_exec;
                }
                s.rq.update_curr(s.now);
                if (*ip).sum_exec - burst_from >= 200_000 {
                    s.rq.dequeue_curr(ip, s.now, true);
                    asleep = true;
                    wake_at = s.now + 4_800_000;
                    s.rq.pick_next(s.now, |_| {});
                }
            }
            if asleep && s.now >= wake_at {
                asleep = false;
                s.rq.enqueue(ip, flags::WAKEUP, s.now);
                woke = s.now;
                waiting = true;
                if s.rq.should_preempt(ip, s.now) {
                    s.resched();
                }
            }
        }
        if s.now >= next_tick {
            next_tick += tick;
            if s.rq.update_curr(s.now) {
                s.resched();
            }
        }
    }
    let mut l: Vec<u64> = lat[20..].to_vec();
    l.sort_unstable();
    let p99 = l[l.len() * 99 / 100];
    println!("tranche fixe 3 ms : p50 {} ns, p99 {p99} ns", l[l.len() / 2]);
    assert!(p99 > 500_000, "attendu une latence plus élevée avec tranche fixe, p99 {p99}");
}

#[test]
fn delayed_dequeue_keeps_debt() {
    let mut s = Sim::new();
    let mut a = fair_entity(1, 0);
    let mut b = fair_entity(2, 0);
    let (ap, bp): (*mut Entity, *mut Entity) = (&mut *a, &mut *b);
    unsafe {
        s.rq.enqueue(ap, flags::INITIAL, s.now);
        s.rq.enqueue(bp, flags::INITIAL, s.now);
        s.resched();
        let first = s.rq.curr();
        // La première tâche calcule 2 ms (au-delà de son dû) puis s'endort : retard négatif.
        s.now += 2_000_000;
        let r = s.rq.dequeue_curr(first, s.now, true);
        assert_eq!(r, Dequeued::Delayed);
        assert!((*first).delayed && (*first).on_rq);
        s.rq.validate().unwrap();
        let mut gone = Vec::new();
        let next = s.rq.pick_next(s.now, |e| gone.push(e));
        assert_ne!(next, first);
        // L'autre calcule ; la différée finit par être retirée quand elle redevient éligible.
        for _ in 0..10 {
            s.now += 1_000_000;
            if s.rq.update_curr(s.now) || true {
                let c = s.rq.curr();
                s.rq.put_prev(c, s.now, false);
                s.rq.pick_next(s.now, |e| gone.push(e));
            }
            if !gone.is_empty() {
                break;
            }
        }
        assert_eq!(gone, vec![first]);
        assert!(!(*first).on_rq && !(*first).delayed);
        assert!((*first).vlag <= 0, "DELAY_ZERO : pas de crédit après un départ différé");
        s.rq.validate().unwrap();
        // Réveil d'une différée avant son retrait : simple réactivation.
        s.rq.enqueue(first, flags::WAKEUP, s.now);
        s.rq.validate().unwrap();
    }
}

#[test]
fn reweight_and_yield_keep_invariants() {
    let mut s = Sim::new();
    let mut ents: Vec<Box<Entity>> = (0..8).map(|i| fair_entity(i, (i as i8) - 4)).collect();
    for e in ents.iter_mut() {
        unsafe { s.rq.enqueue(&mut **e, flags::INITIAL, s.now) };
    }
    s.resched();
    let mut rng = Rng(42);
    for round in 0..50_000 {
        s.now += rng.below(3_000_000);
        let op = rng.below(4);
        unsafe {
            match op {
                0 => {
                    let i = rng.below(8) as usize;
                    let p: *mut Entity = &mut *ents[i];
                    if (*p).on_rq {
                        s.rq.reweight(p, crate::weight_of_nice(rng.below(40) as i8 - 20), s.now);
                    }
                }
                1 => {
                    let c = s.rq.curr();
                    if !c.is_null() {
                        s.rq.put_prev(c, s.now, true);
                        s.rq.pick_next(s.now, |_| {});
                    }
                }
                _ => {
                    if s.rq.update_curr(s.now) {
                        s.resched();
                    }
                }
            }
        }
        if let Err(e) = s.rq.validate() {
            panic!("{e} (opération {op}, tour {round})");
        }
    }
}

// ---------------------------------------------------------------------------
// Temps réel
// ---------------------------------------------------------------------------

#[test]
fn rt_priority_order_and_rr() {
    let mut rq = RtRq::new();
    let mut a = Box::new(Entity::new(1, Class::Rt { prio: 10, rr: false }));
    let mut b = Box::new(Entity::new(2, Class::Rt { prio: 50, rr: true }));
    let mut c = Box::new(Entity::new(3, Class::Rt { prio: 50, rr: true }));
    let (ap, bp, cp): (*mut Entity, *mut Entity, *mut Entity) = (&mut *a, &mut *b, &mut *c);
    unsafe {
        rq.enqueue(ap);
        rq.enqueue(bp);
        rq.enqueue(cp);
        assert_eq!(rq.pick_next(), bp);
        // Tranche RR épuisée avec une autre tâche de même priorité : tourniquet.
        assert!(rq.tick(crate::RR_TIMESLICE_NS));
        rq.put_prev(bp, true);
        assert_eq!(rq.pick_next(), cp);
        rq.dequeue_curr(cp);
        assert_eq!(rq.should_preempt(ap), false || rq.curr().is_null());
        assert_eq!(rq.pick_next(), bp);
        rq.dequeue_curr(bp);
        assert_eq!(rq.pick_next(), ap);
        rq.dequeue_curr(ap);
        assert!(rq.pick_next().is_null());
        assert_eq!(rq.nr_runnable(), 0);
    }
}
