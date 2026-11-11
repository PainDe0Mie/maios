//! Étiquettes des requêtes en vol, une table par file matérielle.
//!
//! Une étiquette est le numéro d'une requête dans sa file (le CID de NVMe) : elle sert d'indice
//! dans la table des emplacements, où la fin de la requête est publiée. Allocation dans un
//! tableau de bits sans verrou, avec un indice de départ par file, comme `sbitmap` de Linux
//! (Axboe, « blk-mq tag allocation », 2014) : deux cœurs qui allouent dans la même file ne
//! visent pas le même mot.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicU16, AtomicU64, AtomicU8, AtomicUsize, Ordering::*};
use sync_irq::IrqSafeMutex;
use task::TaskRef;

pub(crate) const FREE: u8 = 0;
pub(crate) const INFLIGHT: u8 = 1;
pub(crate) const DONE: u8 = 2;

/// Statut de fin d'une requête (`0` : réussite ; sinon code du pilote, par exemple le champ
/// *Status* d'une complétion NVMe).
pub type Status = u16;

pub(crate) struct Slot {
    pub(crate) state: AtomicU8,
    pub(crate) status: AtomicU16,
    /// Instant de soumission (ns de l'horloge monotone), pour la latence et le chien de garde.
    pub(crate) start_ns: AtomicU64,
    pub(crate) waiter: IrqSafeMutex<Option<TaskRef>>,
}

pub(crate) struct HwTags {
    bits: Box<[AtomicU64]>,
    pub(crate) depth: usize,
    hint: AtomicUsize,
    pub(crate) slots: Box<[Slot]>,
    pub(crate) inflight: AtomicUsize,
    /// Latence moyenne glissante (EWMA, 1/8) des requêtes de cette file, en ns : décide du
    /// sondage hybride.
    pub(crate) lat_ewma_ns: AtomicU64,
}

/// Étiquettes de toutes les files matérielles d'un contrôleur, partagées par ses unités (les
/// espaces de noms d'un NVMe utilisent les mêmes files, donc les mêmes CID).
pub struct TagSet {
    pub(crate) queues: Box<[HwTags]>,
}

impl TagSet {
    /// `nr_hw` files de `depth` étiquettes chacune.
    pub fn new(nr_hw: usize, depth: usize) -> TagSet {
        let queues = (0..nr_hw)
            .map(|_| HwTags {
                bits: (0..depth.div_ceil(64)).map(|_| AtomicU64::new(0)).collect(),
                depth,
                hint: AtomicUsize::new(0),
                slots: (0..depth)
                    .map(|_| Slot {
                        state: AtomicU8::new(FREE),
                        status: AtomicU16::new(0),
                        start_ns: AtomicU64::new(0),
                        waiter: IrqSafeMutex::new(None),
                    })
                    .collect(),
                inflight: AtomicUsize::new(0),
                lat_ewma_ns: AtomicU64::new(0),
            })
            .collect();
        TagSet { queues }
    }

    pub fn nr_hw_queues(&self) -> usize {
        self.queues.len()
    }

    /// Fin de la requête `tag` de la file `hwq`, avec son statut. Appelé par le pilote, depuis
    /// son interruption ou depuis un sondage ; sans effet sur une étiquette qui n'est pas en vol
    /// (complétion en double, ou CID invalide renvoyé par l'appareil).
    pub fn complete(&self, hwq: usize, tag: u16, status: Status) {
        let Some(q) = self.queues.get(hwq) else { return };
        let Some(slot) = q.slots.get(tag as usize) else { return };
        if slot.state.load(Acquire) != INFLIGHT {
            return;
        }
        slot.status.store(status, Relaxed);
        slot.state.store(DONE, Release);
        if let Some(t) = slot.waiter.lock().as_ref() {
            let _ = t.unblock();
        }
    }

    /// Requêtes en vol dans la file `hwq`.
    pub fn inflight(&self, hwq: usize) -> usize {
        self.queues.get(hwq).map_or(0, |q| q.inflight.load(Relaxed))
    }
}

impl HwTags {
    /// Prend une étiquette libre, sans attendre.
    pub(crate) fn alloc(&self) -> Option<u16> {
        let words = self.bits.len();
        let start = self.hint.load(Relaxed) % words;
        for k in 0..words {
            let w = (start + k) % words;
            let word = &self.bits[w];
            let mut cur = word.load(Relaxed);
            loop {
                let valid = if (w + 1) * 64 <= self.depth { u64::MAX } else { (1u64 << (self.depth % 64)) - 1 };
                let free = !cur & valid;
                if free == 0 {
                    break;
                }
                let b = free.trailing_zeros() as u64;
                match word.compare_exchange_weak(cur, cur | (1 << b), Acquire, Relaxed) {
                    Ok(_) => {
                        self.hint.store(w, Relaxed);
                        self.inflight.fetch_add(1, Relaxed);
                        return Some((w * 64 + b as usize) as u16);
                    }
                    Err(v) => cur = v,
                }
            }
        }
        None
    }

    pub(crate) fn free(&self, tag: u16) {
        let slot = &self.slots[tag as usize];
        *slot.waiter.lock() = None;
        slot.state.store(FREE, Release);
        let (w, b) = (tag as usize / 64, tag as usize % 64);
        self.bits[w].fetch_and(!(1 << b), Release);
        self.inflight.fetch_sub(1, Relaxed);
    }

    pub(crate) fn update_latency(&self, ns: u64) {
        let old = self.lat_ewma_ns.load(Relaxed);
        let new = if old == 0 { ns } else { old - old / 8 + ns / 8 };
        self.lat_ewma_ns.store(new, Relaxed);
    }
}
