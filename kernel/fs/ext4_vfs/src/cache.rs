//! Caches du volume.
//!
//! * [`MetaCache`] : blocs de métadonnées (tables d'inodes, extents, dossiers). Seize parts
//!   indépendantes (la part se choisit par le numéro de bloc), chacune remplacée par l'horloge
//!   « seconde chance » (CLOCK de Corbató, l'approximation de LRU des noyaux) : une lecture ne
//!   fait que poser un bit, aucune liste n'est réordonnée sous le verrou. Aucune entrée-sortie
//!   sous un verrou : on cherche, on relâche, on lit, puis on insère.
//! * [`Gen2`] : table associative bornée à deux générations (« jeune » et « vieille ») ; une
//!   entrée retrouvée dans la vieille revient dans la jeune, et la vieille est jetée en bloc
//!   quand la jeune est pleine. Approximation de LRU en O(log n) sans liste chaînée, pour le
//!   cache des noms et le maintien en vie des inodes récents.

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use spin::Mutex;

const SHARDS: usize = 16;

struct Slot {
    key: u64,
    data: Arc<[u8]>,
    referenced: bool,
}

struct Shard {
    map: BTreeMap<u64, usize>,
    slots: Vec<Slot>,
    hand: usize,
    cap: usize,
}

pub struct MetaCache {
    shards: [Mutex<Shard>; SHARDS],
}

impl MetaCache {
    /// Cache de `cap` blocs au plus.
    pub fn new(cap: usize) -> MetaCache {
        let per = (cap / SHARDS).max(8);
        MetaCache {
            shards: core::array::from_fn(|_| {
                Mutex::new(Shard { map: BTreeMap::new(), slots: Vec::new(), hand: 0, cap: per })
            }),
        }
    }

    fn shard(&self, key: u64) -> &Mutex<Shard> {
        // Les blocs voisins (même table d'inodes, même dossier) vont dans des parts différentes.
        let h = key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 60;
        &self.shards[h as usize % SHARDS]
    }

    pub fn get(&self, key: u64) -> Option<Arc<[u8]>> {
        let mut s = self.shard(key).lock();
        let i = *s.map.get(&key)?;
        s.slots[i].referenced = true;
        Some(s.slots[i].data.clone())
    }

    pub fn insert(&self, key: u64, data: Arc<[u8]>) {
        let mut s = self.shard(key).lock();
        if let Some(&i) = s.map.get(&key) {
            s.slots[i].data = data;
            return;
        }
        if s.slots.len() < s.cap {
            let i = s.slots.len();
            s.slots.push(Slot { key, data, referenced: false });
            s.map.insert(key, i);
            return;
        }
        loop {
            let h = s.hand;
            s.hand = (h + 1) % s.slots.len();
            if s.slots[h].referenced {
                s.slots[h].referenced = false;
                continue;
            }
            let old = s.slots[h].key;
            s.map.remove(&old);
            s.slots[h] = Slot { key, data, referenced: false };
            s.map.insert(key, h);
            return;
        }
    }

    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| s.lock().slots.len()).sum()
    }
}

pub struct Gen2<K: Ord, V: Clone> {
    young: BTreeMap<K, V>,
    old: BTreeMap<K, V>,
    half: usize,
}

impl<K: Ord + Clone, V: Clone> Gen2<K, V> {
    pub const fn new(cap: usize) -> Gen2<K, V> {
        Gen2 { young: BTreeMap::new(), old: BTreeMap::new(), half: cap / 2 }
    }

    pub fn get(&mut self, k: &K) -> Option<V> {
        if let Some(v) = self.young.get(k) {
            return Some(v.clone());
        }
        let v = self.old.remove(k)?;
        self.insert(k.clone(), v.clone());
        Some(v)
    }

    pub fn insert(&mut self, k: K, v: V) {
        if self.young.len() >= self.half {
            self.old = core::mem::take(&mut self.young);
        }
        self.young.insert(k, v);
    }

    pub fn len(&self) -> usize {
        self.young.len() + self.old.len()
    }
}
