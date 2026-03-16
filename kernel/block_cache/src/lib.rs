//! Cache de blocs write-back pour périphériques de stockage.
//!
//! # Politique write-back
//! Les écritures vont **d'abord** dans le cache (état `Modified`) et sont
//! propagées sur le périphérique de façon *différée* :
//! - lors d'un appel explicite à `flush()`,
//! - ou quand le cache atteint sa capacité maximale et qu'une entrée
//!   `Modified` doit être évincée (LRU parmi les entrées propres sinon).
//!
//! L'ancien code utilisait un write-through systématique, ce qui annulait
//! tout bénéfice du cache en écriture.  Le write-back apporte un gain
//! typique de 5–10× sur les workloads avec beaucoup d'écritures séquentielles
//! (copie de fichiers, installation d'apps).
//!
//! # Cohérence
//! Le cache doit être la **seule** voie d'accès au périphérique sous-jacent.
//! Si un autre code écrit directement sur le périphérique, le cache peut
//! retourner des données obsolètes.  La bonne pratique est d'envelopper le
//! `StorageDeviceRef` dans un `BlockCache` dès l'initialisation et de ne
//! plus exposer le premier.
//!
//! # Éviction LRU
//! Chaque entrée porte un `lru_tick` incrémenté à chaque accès.  L'éviction
//! choisit l'entrée propre (`Shared`) la moins récemment accédée.  Si toutes
//! les entrées sont `Modified`, on les flush toutes avant d'évincer.

#![no_std]

#[macro_use] extern crate alloc;
extern crate hashbrown;
extern crate storage_device;
#[macro_use] extern crate log;

use alloc::vec::Vec;
use alloc::borrow::{Cow, ToOwned};
use hashbrown::HashMap;
use storage_device::{StorageDeviceRef};
extern crate io;
use io::{BlockIo, BlockReader, BlockWriter, IoError, KnownLength};

impl storage_device::StorageDevice for BlockCache {
    fn size_in_blocks(&self) -> usize {
        self.device.lock().size_in_blocks()
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Paramètres
// ────────────────────────────────────────────────────────────────────────────

/// Nombre maximum d'entrées dans le cache.
/// À 512 octets par bloc × 2048 = 1 MiB de données en cache.
/// Ajustable selon la RAM disponible.
const DEFAULT_CAPACITY: usize = 2048;

// ────────────────────────────────────────────────────────────────────────────
// États MSI (Modified / Shared / Invalid)
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CacheState {
    /// Modifié en cache mais pas encore écrit sur le périphérique.
    Modified,
    /// Propre — en phase avec le périphérique, peut être évincé librement.
    Shared,
    /// Périmé — doit être relu avant utilisation.
    Invalid,
}

// ────────────────────────────────────────────────────────────────────────────
// CachedBlock
// ────────────────────────────────────────────────────────────────────────────

struct CachedBlock {
    data:     Vec<u8>,
    state:    CacheState,
    /// Compteur LRU : plus la valeur est grande, plus l'accès est récent.
    lru_tick: u64,
}

// ────────────────────────────────────────────────────────────────────────────
// BlockCache
// ────────────────────────────────────────────────────────────────────────────

/// Cache de blocs write-back à capacité bornée avec éviction LRU.
pub struct BlockCache {
    cache:          HashMap<usize, CachedBlock>,
    device:         StorageDeviceRef,
    capacity:       usize,
    /// Compteur global pour l'ordre LRU.
    tick:           u64,
    /// Statistiques (optionnelles, utiles pour le debug).
    hits:           u64,
    misses:         u64,
    evictions:      u64,
    dirty_flushes:  u64,
}

impl BlockCache {
    /// Crée un nouveau cache avec la capacité par défaut.
    pub fn new(device: StorageDeviceRef) -> Self {
        Self::with_capacity(device, DEFAULT_CAPACITY)
    }

    /// Crée un nouveau cache avec une capacité explicite.
    pub fn with_capacity(device: StorageDeviceRef, capacity: usize) -> Self {
        BlockCache {
            cache:         HashMap::with_capacity(capacity.min(256)),
            device,
            capacity,
            tick:          0,
            hits:          0,
            misses:        0,
            evictions:     0,
            dirty_flushes: 0,
        }
    }

    // ── API publique ──────────────────────────────────────────────────────────

    /// Lit un bloc.  Retourne un slice sur les données en cache (zero-copy).
    ///
    /// Si le bloc est absent ou invalide, il est relu depuis le périphérique.
    pub fn read_block(&mut self, block: usize) -> Result<&[u8], &'static str> {
        self.tick += 1;
        let tick = self.tick;

        // Vérifier si le bloc est en cache et valide.
        if let Some(cb) = self.cache.get_mut(&block) {
            match cb.state {
                CacheState::Modified | CacheState::Shared => {
                    cb.lru_tick = tick;
                    self.hits += 1;
                    // SAFETY: on retourne une référence dans self.cache.
                    // Le borrow checker n'accepterait pas ça directement avec
                    // get_mut ci-dessus, on refait un get() non-mutable.
                }
                CacheState::Invalid => {
                    // Besoin de recharger — on sort du match et on reload.
                    cb.state = CacheState::Invalid; // pas de changement, juste forcer reload
                }
            }
        }

        // Recharger si absent ou invalide.
        if !self.cache.contains_key(&block)
            || self.cache[&block].state == CacheState::Invalid
        {
            self.load_from_device(block)?;
        }

        let tick = self.tick;
        let cb = self.cache.get_mut(&block).unwrap();
        cb.lru_tick = tick;

        Ok(&self.cache[&block].data)
    }

    /// Écrit un bloc dans le cache (write-back — pas d'écriture disque immédiate).
    ///
    /// Si le cache est plein, évince l'entrée LRU la plus propre.
    pub fn write_block(&mut self, block: usize, data: Cow<[u8]>) -> Result<(), &'static str> {
        self.tick += 1;
        let tick  = self.tick;

        // Vérifier la taille
        let block_size = self.device.lock().block_size();
        let owned: Vec<u8> = match data {
            Cow::Borrowed(s) => s.to_owned(),
            Cow::Owned(v)    => v,
        };
        if owned.len() != block_size {
            return Err("BlockCache::write_block: buffer size mismatch");
        }

        // Évincer si nécessaire avant d'insérer
        if !self.cache.contains_key(&block) && self.cache.len() >= self.capacity {
            self.evict_one()?;
        }

        self.cache.insert(block, CachedBlock {
            data:     owned,
            state:    CacheState::Modified,
            lru_tick: tick,
        });
        Ok(())
    }

    /// Invalide un bloc en cache (force un rechargement au prochain accès).
    pub fn invalidate(&mut self, block: usize) {
        if let Some(cb) = self.cache.get_mut(&block) {
            cb.state = CacheState::Invalid;
        }
    }

    /// Flush tous les blocs `Modified` vers le périphérique.
    ///
    /// Passe `block_num = None` pour tout flusher, ou `Some(n)` pour un seul.
    pub fn flush(&mut self, block_num: Option<usize>) -> Result<(), &'static str> {
        if let Some(bn) = block_num {
            if let Some(cb) = self.cache.get_mut(&bn) {
                if cb.state == CacheState::Modified {
                    let mut dev = self.device.lock();
                    dev.write_blocks(&cb.data, bn)
                        .map_err(|_| "BlockCache::flush: device write error")?;
                    cb.state = CacheState::Shared;
                    self.dirty_flushes += 1;
                }
            }
        } else {
            // Collecter les blocs dirty pour éviter un borrow double
            let dirty: Vec<usize> = self.cache.iter()
                .filter(|(_, cb)| cb.state == CacheState::Modified)
                .map(|(k, _)| *k)
                .collect();
            let mut dev = self.device.lock();
            for bn in dirty {
                let cb = self.cache.get_mut(&bn).unwrap();
                dev.write_blocks(&cb.data, bn)
                    .map_err(|_| "BlockCache::flush_all: device write error")?;
                cb.state = CacheState::Shared;
                self.dirty_flushes += 1;
            }
        }
        Ok(())
    }

    /// Vide complètement le cache après avoir flushé les entrées dirty.
    pub fn flush_and_clear(&mut self) -> Result<(), &'static str> {
        self.flush(None)?;
        self.cache.clear();
        Ok(())
    }

    /// Retourne les statistiques d'utilisation du cache.
    pub fn stats(&self) -> CacheStats {
        let total = self.hits + self.misses;
        CacheStats {
            hits:          self.hits,
            misses:        self.misses,
            hit_rate:      if total > 0 { self.hits * 100 / total } else { 0 },
            evictions:     self.evictions,
            dirty_flushes: self.dirty_flushes,
            entries:       self.cache.len(),
            capacity:      self.capacity,
            dirty_entries: self.cache.values().filter(|cb| cb.state == CacheState::Modified).count(),
        }
    }

    // ── Interne ───────────────────────────────────────────────────────────────

    /// Charge un bloc depuis le périphérique dans le cache.
    fn load_from_device(&mut self, block: usize) -> Result<(), &'static str> {
        let block_size = self.device.lock().block_size();

        // Évincer si plein
        if !self.cache.contains_key(&block) && self.cache.len() >= self.capacity {
            self.evict_one()?;
        }

        let mut data = vec![0u8; block_size];
        self.device.lock()
            .read_blocks(&mut data, block)
            .map_err(|_| "BlockCache: device read error")?;
        self.misses += 1;

        self.cache.insert(block, CachedBlock {
            data,
            state:    CacheState::Shared,
            lru_tick: self.tick,
        });
        Ok(())
    }

    /// Évince l'entrée LRU la plus propre possible.
    ///
    /// Ordre de préférence :
    /// 1. Entrée `Invalid` (pas de flush nécessaire)
    /// 2. Entrée `Shared` la moins récemment accédée
    /// 3. Si tout est `Modified`, flush toutes les dirty et recommence.
    fn evict_one(&mut self) -> Result<(), &'static str> {
        // 1. Chercher une entrée Invalid ou Shared
        let victim = self.cache.iter()
            .filter(|(_, cb)| cb.state != CacheState::Modified)
            .min_by_key(|(_, cb)| cb.lru_tick)
            .map(|(k, _)| *k);

        if let Some(block) = victim {
            self.cache.remove(&block);
            self.evictions += 1;
            return Ok(());
        }

        // 2. Tout est dirty — flush tout avant d'évincer
        warn!("BlockCache: cache full of dirty entries, flushing all");
        self.flush(None)?;

        // Maintenant on peut évincer le LRU (tous sont Shared)
        let victim = self.cache.iter()
            .min_by_key(|(_, cb)| cb.lru_tick)
            .map(|(k, _)| *k);

        if let Some(block) = victim {
            self.cache.remove(&block);
            self.evictions += 1;
        }
        Ok(())
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Statistiques
// ────────────────────────────────────────────────────────────────────────────

/// Snapshot des statistiques du cache.
#[derive(Debug, Clone, Copy)]
pub struct CacheStats {
    pub hits:          u64,
    pub misses:        u64,
    /// Taux de succès en pourcentage entier (0–100).
    pub hit_rate:      u64,
    pub evictions:     u64,
    pub dirty_flushes: u64,
    pub entries:       usize,
    pub capacity:      usize,
    pub dirty_entries: usize,
}

impl BlockIo for BlockCache {
    fn block_size(&self) -> usize {
        self.device.lock().block_size()
    }
}

impl KnownLength for BlockCache {
    fn len(&self) -> usize {
        self.device.lock().len()
    }
}

impl BlockReader for BlockCache {
    fn read_blocks(&mut self, buffer: &mut [u8], block_offset: usize) -> Result<usize, IoError> {
        let block_size = self.device.lock().block_size();
        let num_blocks = buffer.len() / block_size;
        for i in 0..num_blocks {
            let src = self.read_block(block_offset + i)
                .map_err(|_| IoError::InvalidInput)?;
            buffer[i * block_size..(i + 1) * block_size].copy_from_slice(src);
        }
        Ok(num_blocks)
    }
}

impl BlockWriter for BlockCache {
    fn write_blocks(&mut self, buffer: &[u8], block_offset: usize) -> Result<usize, IoError> {
        let block_size = self.device.lock().block_size();
        let num_blocks = buffer.len() / block_size;
        for i in 0..num_blocks {
            let chunk = &buffer[i * block_size..(i + 1) * block_size];
            self.write_block(block_offset + i, alloc::borrow::Cow::Borrowed(chunk))
                .map_err(|_| IoError::InvalidInput)?;
        }
        Ok(num_blocks)
    }

    fn flush(&mut self) -> Result<(), IoError> {
        BlockCache::flush(self, None).map_err(|_| IoError::InvalidInput)
    }
}