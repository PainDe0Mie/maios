//! # LLFree pour MaiOS : allocateur de frames physiques sans verrou
//!
//! Première brique de MaiMM (phase M1, voir `.claude/plans/02-maimm-gestion-memoire.md`).
//! L'architecture reprend **LLFree** (Wrenger et al., *LLFree: Scalable and Optionally-Persistent
//! Page-Frame Allocation*, USENIX ATC '23), qui remplace l'allocateur buddy de Linux par une
//! structure sans verrou ni journal :
//!
//! ```text
//!  arbres (64 Mio)   [ free: i64 | owner: u32 ]  ...       ← un CPU « réserve » un arbre
//!       │
//!  enfants (2 Mio)   [ u16 ][ u16 ] ... x32 = 1 ligne de cache par arbre
//!       │
//!  bitfield          [ 8 x u64 = 512 bits ] = 1 ligne de cache par enfant
//! ```
//!
//! * **Bitfield** : un bit par frame de 4 Kio (1 = allouée). Les 512 bits d'un enfant tiennent
//!   dans une ligne de cache, et chaque modification est un CAS sur un mot de 64 bits.
//! * **Enfants** : un compteur de frames libres par bloc de 2 Mio. Une allocation *réserve*
//!   d'abord des frames dans ce compteur (CAS), puis prend des bits. Une libération rend les bits
//!   d'abord, puis incrémente le compteur. Invariant qui en découle :
//!   `compteur(enfant) <= bits libres(enfant)`. Une réservation réussie garantit donc qu'il existe
//!   des bits libres à trouver, et un enfant dont le compteur vaut 512 est entièrement libre.
//!   On peut alors le prendre d'un seul CAS `512 -> 0` pour une page de 2 Mio.
//! * **Arbres** : 32 enfants (64 Mio). Chaque CPU réserve un arbre et y alloue en priorité. Les
//!   CPU ne se partagent presque plus de lignes de cache, et les allocations d'un même cœur restent
//!   physiquement groupées, ce qui limite la fragmentation.
//!
//! ## Différences avec l'article
//! * Les compteurs d'arbres sont des **indices** (`i64`, éventuellement négatifs de façon
//!   transitoire), pas des réservations strictes. La correction repose uniquement sur les
//!   compteurs d'enfants et le bitfield, ce qui simplifie la preuve et permet aux allocations
//!   multi-enfants de traverser les arbres.
//! * Les allocations de taille arbitraire sont prises en charge, pas seulement les puissances de
//!   deux : recherche alignée (comme un buddy) puis non alignée, sur plusieurs mots avec
//!   rollback, et au-delà de 2 Mio sur plusieurs enfants consécutifs.
//! * Une double libération est **détectée** : le bit est déjà à zéro. Elle est signalée sans
//!   corrompre les compteurs.
//!
//! La crate n'a aucune dépendance et ne touche à aucune adresse physique : elle ne manipule que des
//! numéros de frames, ce qui permet de la tester sur l'hôte avec de vrais threads concurrents
//! (`cargo test`).

#![cfg_attr(not(test), no_std)]

use core::cmp::{max, min};
use core::fmt;
use core::sync::atomic::{
    AtomicI64, AtomicU16, AtomicU32, AtomicU64,
    Ordering::{AcqRel, Acquire, Relaxed, Release},
};

/// Frames par mot du bitfield.
pub const FRAMES_PER_WORD: usize = 64;
/// Mots de bitfield par enfant (une ligne de cache).
pub const WORDS_PER_CHILD: usize = 8;
/// Frames par enfant : 512 x 4 Kio = 2 Mio, une page huge x86_64.
pub const FRAMES_PER_CHILD: usize = FRAMES_PER_WORD * WORDS_PER_CHILD;
/// Enfants par arbre (les compteurs d'un arbre tiennent dans une ligne de cache).
pub const CHILDREN_PER_TREE: usize = 32;
/// Frames par arbre : 64 Mio.
pub const FRAMES_PER_TREE: usize = FRAMES_PER_CHILD * CHILDREN_PER_TREE;

const CACHE_LINE: usize = 64;
const NO_TREE: u32 = u32::MAX;
/// Un arbre partiellement utilisé n'est choisi que s'il lui reste au moins 1/16 de frames libres,
/// sinon un CPU changerait d'arbre à chaque allocation.
const PARTIAL_TREE_MIN_FREE: usize = FRAMES_PER_TREE / 16;
/// Nombre de passes de recherche dans un enfant avant d'abandonner (courses transitoires).
const MAX_CLAIM_RETRIES: usize = 16;

/// Erreurs de l'allocateur.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Taille nulle ou plus grande que la mémoire gérée.
    InvalidSize,
    /// Plage hors de la zone gérée par cette instance.
    OutOfRange,
    /// Plus assez de frames libres (ou pas de plage contiguë assez grande).
    NoMemory,
    /// `alloc_at` : au moins une des frames demandées est déjà allouée.
    NotFree,
    /// `free` : `count` frames de la plage étaient déjà libres, la première étant `frame`.
    DoubleFree { frame: usize, count: usize },
    /// Zone de métadonnées trop petite, mal alignée ou paramètres incohérents.
    BadMetadata,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidSize => f.write_str("taille d'allocation invalide"),
            Error::OutOfRange => f.write_str("plage hors de la mémoire gérée"),
            Error::NoMemory => f.write_str("mémoire physique épuisée"),
            Error::NotFree => f.write_str("frames déjà allouées"),
            Error::DoubleFree { frame, count } => {
                write!(f, "double libération de {count} frame(s) à partir de la frame {frame:#x}")
            }
            Error::BadMetadata => f.write_str("métadonnées LLFree invalides"),
        }
    }
}

/// Métadonnées d'un arbre de 64 Mio, seules sur leur ligne de cache.
#[repr(C, align(64))]
struct Tree {
    /// Indice du nombre de frames libres (somme des enfants, cohérente au repos).
    free: AtomicI64,
    /// `0` si libre, sinon `cpu + 1` du CPU qui l'a réservé.
    owner: AtomicU32,
}

/// Arbre réservé par un CPU, seul sur sa ligne de cache.
#[repr(C, align(64))]
struct Local {
    tree: AtomicU32,
}

/// Instance LLFree couvrant `frames` frames à partir de `base` (numéros de frames 4 Kio).
pub struct LLFree<'a> {
    base: usize,
    frames: usize,
    trees: &'a [Tree],
    locals: &'a [Local],
    /// `trees.len() * CHILDREN_PER_TREE` compteurs ; ceux au-delà de la mémoire restent à 0.
    children: &'a [AtomicU16],
    /// `WORDS_PER_CHILD` mots par enfant réel.
    bitfield: &'a [AtomicU64],
}

/// Disposition des métadonnées dans la zone fournie à [`LLFree::new`].
struct Layout {
    trees: usize,
    children: usize,
    trees_off: usize,
    locals_off: usize,
    children_off: usize,
    bitfield_off: usize,
    total: usize,
}

const fn div_ceil(a: usize, b: usize) -> usize {
    (a + b - 1) / b
}

const fn align_up(a: usize, align: usize) -> usize {
    div_ceil(a, align) * align
}

/// Masque de `len` bits (1..=64) à partir du bit `lo`.
#[inline]
const fn range_mask(lo: usize, len: usize) -> u64 {
    if len >= FRAMES_PER_WORD {
        u64::MAX
    } else {
        ((1u64 << len) - 1) << lo
    }
}

impl Layout {
    const fn compute(frames: usize, locals: usize) -> Layout {
        let children = div_ceil(frames, FRAMES_PER_CHILD);
        let trees = div_ceil(children, CHILDREN_PER_TREE);
        let trees_off = 0;
        let locals_off = trees_off + trees * core::mem::size_of::<Tree>();
        let children_off = locals_off + locals * core::mem::size_of::<Local>();
        let children_bytes = trees * CHILDREN_PER_TREE * core::mem::size_of::<AtomicU16>();
        let bitfield_off = align_up(children_off + children_bytes, CACHE_LINE);
        let total = bitfield_off + children * WORDS_PER_CHILD * core::mem::size_of::<AtomicU64>();
        Layout { trees, children, trees_off, locals_off, children_off, bitfield_off, total }
    }
}

impl<'a> LLFree<'a> {
    /// Taille en octets (alignement 64) de la zone de métadonnées nécessaire pour gérer `frames`
    /// frames avec `locals` emplacements par CPU. Environ 1/32 768 de la mémoire gérée,
    /// soit 32 Kio pour 1 Gio.
    pub const fn metadata_size(frames: usize, locals: usize) -> usize {
        Layout::compute(frames, locals).total
    }

    /// Alignement requis pour la zone de métadonnées.
    pub const METADATA_ALIGN: usize = CACHE_LINE;

    /// Crée une instance dont **toutes les frames sont allouées**. On rend ensuite la mémoire
    /// utilisable avec [`LLFree::free`], ce qui permet de laisser les trous de la carte physique
    /// marqués « occupés » pour toujours.
    ///
    /// * `base` doit être aligné sur 2 Mio (multiple de [`FRAMES_PER_CHILD`]) pour que chaque
    ///   enfant corresponde à une vraie page huge.
    /// * `locals` est le nombre d'emplacements par CPU. L'index CPU passé à [`LLFree::alloc`]
    ///   est pris modulo cette valeur.
    pub fn new(meta: &'a mut [u8], base: usize, frames: usize, locals: usize) -> Result<Self, Error> {
        let layout = Layout::compute(frames, locals);
        if frames == 0
            || locals == 0
            || base % FRAMES_PER_CHILD != 0
            || meta.len() < layout.total
            || (meta.as_ptr() as usize) % CACHE_LINE != 0
            || base.checked_add(frames).is_none()
        {
            return Err(Error::BadMetadata);
        }
        let ptr = meta.as_mut_ptr();
        // SAFETY: `meta` est emprunté de façon exclusive pour `'a`, assez grand et aligné sur 64.
        // Chaque sous-tableau est initialisé par écriture brute avant d'être réinterprété, et les
        // décalages calculés par `Layout` ne se chevauchent pas. Les types atomiques ont la même
        // représentation que leurs entiers.
        unsafe {
            let trees = ptr.add(layout.trees_off) as *mut Tree;
            for i in 0..layout.trees {
                trees.add(i).write(Tree { free: AtomicI64::new(0), owner: AtomicU32::new(0) });
            }
            let locals_ptr = ptr.add(layout.locals_off) as *mut Local;
            for i in 0..locals {
                locals_ptr.add(i).write(Local { tree: AtomicU32::new(NO_TREE) });
            }
            let children_len = layout.trees * CHILDREN_PER_TREE;
            let children = ptr.add(layout.children_off) as *mut AtomicU16;
            for i in 0..children_len {
                children.add(i).write(AtomicU16::new(0));
            }
            let words = layout.children * WORDS_PER_CHILD;
            let bitfield = ptr.add(layout.bitfield_off) as *mut AtomicU64;
            for i in 0..words {
                bitfield.add(i).write(AtomicU64::new(u64::MAX));
            }
            Ok(LLFree {
                base,
                frames,
                trees: core::slice::from_raw_parts(trees, layout.trees),
                locals: core::slice::from_raw_parts(locals_ptr, locals),
                children: core::slice::from_raw_parts(children, children_len),
                bitfield: core::slice::from_raw_parts(bitfield, words),
            })
        }
    }

    /// Première frame gérée.
    pub fn base(&self) -> usize {
        self.base
    }

    /// Nombre de frames couvertes (trous compris).
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Nombre d'arbres de 64 Mio.
    pub fn tree_count(&self) -> usize {
        self.trees.len()
    }

    /// `true` si `[frame, frame + n)` est entièrement dans la zone gérée.
    pub fn contains(&self, frame: usize, n: usize) -> bool {
        frame >= self.base && n <= self.frames && frame - self.base <= self.frames - n
    }

    fn real_children(&self) -> usize {
        self.bitfield.len() / WORDS_PER_CHILD
    }

    #[inline]
    fn words(&self, child: usize) -> &[AtomicU64] {
        &self.bitfield[child * WORDS_PER_CHILD..(child + 1) * WORDS_PER_CHILD]
    }

    /// Nombre exact de frames libres au repos (somme des compteurs d'enfants).
    pub fn free_frames(&self) -> usize {
        self.children[..self.real_children()].iter().map(|c| c.load(Relaxed) as usize).sum()
    }

    /// Nombre de blocs de 2 Mio entièrement libres, donc allouables comme pages huge.
    pub fn free_huge(&self) -> usize {
        self.children[..self.real_children()]
            .iter()
            .filter(|c| c.load(Relaxed) as usize == FRAMES_PER_CHILD)
            .count()
    }

    /// Indice de frames libres d'un arbre (diagnostic).
    pub fn tree_free(&self, tree: usize) -> i64 {
        self.trees[tree].free.load(Relaxed)
    }

    /// `true` si la frame est libre (lecture instantanée, sans garantie en concurrence).
    pub fn is_free(&self, frame: usize) -> bool {
        if !self.contains(frame, 1) {
            return false;
        }
        let rel = frame - self.base;
        let word = self.bitfield[rel / FRAMES_PER_WORD].load(Acquire);
        word & (1 << (rel % FRAMES_PER_WORD)) == 0
    }

    // ───────────────────────────── allocation ─────────────────────────────

    /// Alloue `n` frames **physiquement contiguës** et renvoie le numéro de la première.
    ///
    /// * `n < 512` : dans l'arbre réservé par `cpu`, sans verrou. Les petites tailles sont
    ///   alignées naturellement quand c'est possible, comme dans un buddy allocator.
    /// * `n >= 512` : sur des blocs de 2 Mio entièrement libres et consécutifs, donc alignée
    ///   sur 2 Mio. La fin inutilisée du dernier bloc est rendue immédiatement.
    pub fn alloc(&self, cpu: usize, n: usize) -> Result<usize, Error> {
        if n == 0 || n > self.frames {
            return Err(Error::InvalidSize);
        }
        if n >= FRAMES_PER_CHILD {
            return self.alloc_large(n);
        }
        let current = self.locals[cpu % self.locals.len()].tree.load(Acquire);
        if current != NO_TREE {
            if let Some(frame) = self.alloc_in_tree(current as usize, n, false) {
                return Ok(frame);
            }
        }
        if let Some(frame) = self.reserve_and_alloc(cpu, current, n) {
            return Ok(frame);
        }
        // Dernier recours : tous les arbres, même réservés par d'autres CPU, sans se fier aux
        // indices (qui peuvent être en retard).
        for tree in 0..self.trees.len() {
            if let Some(frame) = self.alloc_in_tree(tree, n, true) {
                return Ok(frame);
            }
        }
        Err(Error::NoMemory)
    }

    /// Alloue `count` pages huge de 2 Mio contiguës (alignées sur 2 Mio).
    pub fn alloc_huge(&self, count: usize) -> Result<usize, Error> {
        if count == 0 {
            return Err(Error::InvalidSize);
        }
        self.alloc_large(count.checked_mul(FRAMES_PER_CHILD).ok_or(Error::InvalidSize)?)
    }

    /// Alloue exactement les frames `[frame, frame + n)` si elles sont toutes libres.
    /// Opération atomique du point de vue de l'appelant : en cas d'échec, rien n'est alloué.
    pub fn alloc_at(&self, frame: usize, n: usize) -> Result<(), Error> {
        if n == 0 {
            return Err(Error::InvalidSize);
        }
        if !self.contains(frame, n) {
            return Err(Error::OutOfRange);
        }
        let end = frame + n;
        let mut cur = frame;
        while cur < end {
            let rel = cur - self.base;
            let child = rel / FRAMES_PER_CHILD;
            let bit = rel % FRAMES_PER_CHILD;
            let len = min(FRAMES_PER_CHILD - bit, end - cur);
            let ok = if len == FRAMES_PER_CHILD {
                self.claim_children(child, 1)
            } else if self.reserve_child(child, len) {
                if self.claim_range(child, bit, len) {
                    self.trees[child / CHILDREN_PER_TREE].free.fetch_sub(len as i64, Relaxed);
                    true
                } else {
                    self.children[child].fetch_add(len as u16, Release);
                    false
                }
            } else {
                false
            };
            if !ok {
                if cur > frame {
                    // Rend ce qui a déjà été pris : ces frames sont à nous, l'opération ne peut
                    // pas échouer.
                    let _ = self.free(frame, cur - frame);
                }
                return Err(Error::NotFree);
            }
            cur += len;
        }
        Ok(())
    }

    /// Libère `[frame, frame + n)`. Les frames déjà libres sont signalées par
    /// [`Error::DoubleFree`]. Les autres sont bien rendues et les compteurs restent exacts.
    pub fn free(&self, frame: usize, n: usize) -> Result<(), Error> {
        if n == 0 {
            return Ok(());
        }
        if !self.contains(frame, n) {
            return Err(Error::OutOfRange);
        }
        let end = frame + n;
        let mut cur = frame;
        let mut double: Option<(usize, usize)> = None;
        while cur < end {
            let rel = cur - self.base;
            let child = rel / FRAMES_PER_CHILD;
            let first_bit = rel % FRAMES_PER_CHILD;
            let len = min(FRAMES_PER_CHILD - first_bit, end - cur);
            let words = self.words(child);
            let mut cleared = 0usize;
            let mut bit = first_bit;
            while bit < first_bit + len {
                let wi = bit / FRAMES_PER_WORD;
                let lo = bit % FRAMES_PER_WORD;
                let l = min(FRAMES_PER_WORD - lo, first_bit + len - bit);
                let mask = range_mask(lo, l);
                // Release : les écritures du propriétaire précédent sont visibles par le prochain
                // allocateur, qui lit ce mot en Acquire.
                let prev = words[wi].fetch_and(!mask, Release);
                let was_set = prev & mask;
                cleared += was_set.count_ones() as usize;
                let missing = mask & !was_set;
                if missing != 0 {
                    let f = self.base
                        + child * FRAMES_PER_CHILD
                        + wi * FRAMES_PER_WORD
                        + missing.trailing_zeros() as usize;
                    let (first, count) = double.unwrap_or((f, 0));
                    double = Some((min(first, f), count + missing.count_ones() as usize));
                }
                bit += l;
            }
            if cleared > 0 {
                // Bits d'abord, compteur ensuite : maintient compteur <= bits libres.
                self.children[child].fetch_add(cleared as u16, Release);
                self.trees[child / CHILDREN_PER_TREE].free.fetch_add(cleared as i64, Relaxed);
            }
            cur += len;
        }
        match double {
            Some((frame, count)) => Err(Error::DoubleFree { frame, count }),
            None => Ok(()),
        }
    }

    /// Parcourt les plages de frames libres dans l'ordre croissant (instantané non atomique,
    /// pour le diagnostic et `inspect_then_allocate`). `f(début, longueur)` renvoie `false`
    /// pour arrêter.
    pub fn for_each_free_run<F: FnMut(usize, usize) -> bool>(&self, mut f: F) {
        let mut run_start = 0usize;
        let mut run_len = 0usize;
        for (wi, word) in self.bitfield.iter().enumerate() {
            let w = word.load(Relaxed);
            let mut bit = 0usize;
            while bit < FRAMES_PER_WORD {
                let shifted = w >> bit;
                if shifted & 1 == 0 {
                    let zeros = min(shifted.trailing_zeros() as usize, FRAMES_PER_WORD - bit);
                    if run_len == 0 {
                        run_start = wi * FRAMES_PER_WORD + bit;
                    }
                    run_len += zeros;
                    bit += zeros;
                } else {
                    if run_len > 0 {
                        if !f(self.base + run_start, run_len) {
                            return;
                        }
                        run_len = 0;
                    }
                    bit += min((!shifted).trailing_zeros() as usize, FRAMES_PER_WORD - bit);
                }
            }
        }
        if run_len > 0 {
            f(self.base + run_start, run_len);
        }
    }

    /// Vérifie les invariants (à n'appeler qu'au repos, sans allocation concurrente) :
    /// compteur de chaque enfant égal à ses bits libres, et indice de chaque arbre égal à la
    /// somme de ses enfants.
    pub fn validate(&self) -> Result<(), (&'static str, usize)> {
        for child in 0..self.real_children() {
            let zeros: usize = self.words(child).iter().map(|w| w.load(Relaxed).count_zeros() as usize).sum();
            if zeros != self.children[child].load(Relaxed) as usize {
                return Err(("compteur d'enfant != bits libres", child));
            }
        }
        for (t, tree) in self.trees.iter().enumerate() {
            let first = t * CHILDREN_PER_TREE;
            let sum: i64 = self.children[first..first + CHILDREN_PER_TREE].iter().map(|c| c.load(Relaxed) as i64).sum();
            if sum != tree.free.load(Relaxed) {
                return Err(("indice d'arbre != somme des enfants", t));
            }
        }
        Ok(())
    }

    // ───────────────────────────── interne ─────────────────────────────

    /// Essaie d'allouer `n < 512` frames dans l'arbre `tree`. On remplit d'abord les blocs déjà
    /// entamés, et seulement ensuite les blocs entièrement libres, pour garder ces derniers
    /// disponibles comme pages huge (anti-fragmentation).
    fn alloc_in_tree(&self, tree: usize, n: usize, force: bool) -> Option<usize> {
        let t = &self.trees[tree];
        if !force && t.free.load(Relaxed) < n as i64 {
            return None;
        }
        let first = tree * CHILDREN_PER_TREE;
        let last = min(first + CHILDREN_PER_TREE, self.real_children());
        for pass in 0..2 {
            for child in first..last {
                let count = self.children[child].load(Acquire) as usize;
                let eligible = if pass == 0 {
                    count >= n && count < FRAMES_PER_CHILD
                } else {
                    count == FRAMES_PER_CHILD
                };
                if !eligible || !self.reserve_child(child, n) {
                    continue;
                }
                if let Some(bit) = self.claim_any(child, n) {
                    t.free.fetch_sub(n as i64, Relaxed);
                    return Some(self.base + child * FRAMES_PER_CHILD + bit);
                }
                // Réservation sans plage contiguë assez grande : on la rend.
                self.children[child].fetch_add(n as u16, Release);
            }
        }
        None
    }

    /// Change l'arbre réservé par `cpu`. Préférence : arbres entamés, puis arbres libres.
    fn reserve_and_alloc(&self, cpu: usize, current: u32, n: usize) -> Option<usize> {
        let ntrees = self.trees.len();
        let slot = cpu % self.locals.len();
        let me = slot as u32 + 1;
        let start = if current != NO_TREE { current as usize + 1 } else { cpu % ntrees };
        for pass in 0..2 {
            for i in 0..ntrees {
                let tree = (start + i) % ntrees;
                if tree as u32 == current {
                    continue;
                }
                let t = &self.trees[tree];
                let owner = t.owner.load(Relaxed);
                if owner != 0 && owner != me {
                    continue;
                }
                let free = t.free.load(Relaxed);
                let eligible = if pass == 0 {
                    free >= max(n, PARTIAL_TREE_MIN_FREE) as i64 && free < FRAMES_PER_TREE as i64
                } else {
                    free >= n as i64
                };
                if !eligible {
                    continue;
                }
                let took = owner == 0 && t.owner.compare_exchange(0, me, AcqRel, Relaxed).is_ok();
                if owner == 0 && !took {
                    continue;
                }
                if let Some(frame) = self.alloc_in_tree(tree, n, false) {
                    let prev = self.locals[slot].tree.swap(tree as u32, AcqRel);
                    if prev != NO_TREE && prev as usize != tree {
                        let _ = self.trees[prev as usize].owner.compare_exchange(me, 0, AcqRel, Relaxed);
                    }
                    return Some(frame);
                }
                if took {
                    let _ = t.owner.compare_exchange(me, 0, AcqRel, Relaxed);
                }
            }
        }
        None
    }

    /// Allocation de `n >= 512` frames : `ceil(n / 512)` enfants entièrement libres et
    /// consécutifs, cherchés depuis le haut de la mémoire pour rester loin des petites
    /// allocations (qui remplissent les arbres par le bas).
    fn alloc_large(&self, n: usize) -> Result<usize, Error> {
        let k = div_ceil(n, FRAMES_PER_CHILD);
        let nchildren = self.real_children();
        if k > nchildren {
            return Err(Error::NoMemory);
        }
        let mut run = 0usize;
        let mut child = nchildren;
        while child > 0 {
            child -= 1;
            if self.children[child].load(Acquire) as usize != FRAMES_PER_CHILD {
                run = 0;
                continue;
            }
            run += 1;
            if run < k {
                continue;
            }
            if self.claim_children(child, k) {
                let frame = self.base + child * FRAMES_PER_CHILD;
                let total = k * FRAMES_PER_CHILD;
                if total > n {
                    // La fin du dernier bloc nous appartient : la rendre ne peut pas échouer.
                    let _ = self.free(frame + n, total - n);
                }
                return Ok(frame);
            }
            run = 0;
        }
        Err(Error::NoMemory)
    }

    /// Prend `k` enfants entièrement libres à partir de `first` (CAS `512 -> 0` chacun, avec
    /// rollback). Par l'invariant, un compteur à 512 implique 512 bits libres, et personne ne
    /// peut plus y toucher une fois le compteur à 0 : on peut écrire le bitfield directement.
    fn claim_children(&self, first: usize, k: usize) -> bool {
        for i in 0..k {
            let full = FRAMES_PER_CHILD as u16;
            if self.children[first + i].compare_exchange(full, 0, AcqRel, Acquire).is_err() {
                for j in 0..i {
                    self.children[first + j].fetch_add(full, Release);
                }
                return false;
            }
        }
        for i in 0..k {
            for word in self.words(first + i) {
                word.store(u64::MAX, Release);
            }
            self.trees[(first + i) / CHILDREN_PER_TREE].free.fetch_sub(FRAMES_PER_CHILD as i64, Relaxed);
        }
        true
    }

    /// Réserve `n` frames dans le compteur d'un enfant.
    #[inline]
    fn reserve_child(&self, child: usize, n: usize) -> bool {
        let counter = &self.children[child];
        let mut cur = counter.load(Acquire);
        loop {
            if (cur as usize) < n {
                return false;
            }
            match counter.compare_exchange_weak(cur, cur - n as u16, AcqRel, Acquire) {
                Ok(_) => return true,
                Err(v) => cur = v,
            }
        }
    }

    /// Prend `n` bits libres contigus dans un enfant dont le compteur a déjà été réservé.
    /// Renvoie le décalage du premier bit.
    fn claim_any(&self, child: usize, n: usize) -> Option<usize> {
        let words = self.words(child);
        if n == 1 {
            return claim_one(words);
        }
        if n <= FRAMES_PER_WORD {
            if let Some(bit) = claim_aligned(words, n) {
                return Some(bit);
            }
        }
        for _ in 0..MAX_CLAIM_RETRIES {
            let mut snap = [0u64; WORDS_PER_CHILD];
            for (s, w) in snap.iter_mut().zip(words) {
                *s = w.load(Acquire);
            }
            let pos = find_run(&snap, n)?;
            if self.claim_range(child, pos, n) {
                return Some(pos);
            }
        }
        None
    }

    /// Prend exactement les bits `[pos, pos + n)` d'un enfant (CAS mot par mot, rollback si l'un
    /// d'eux est déjà pris). On ne détient jamais plus de bits que la réservation, ce qui
    /// préserve l'invariant pour les autres CPU.
    fn claim_range(&self, child: usize, pos: usize, n: usize) -> bool {
        let words = self.words(child);
        let mut claimed = [0u64; WORDS_PER_CHILD];
        let end = pos + n;
        let mut bit = pos;
        while bit < end {
            let wi = bit / FRAMES_PER_WORD;
            let lo = bit % FRAMES_PER_WORD;
            let len = min(FRAMES_PER_WORD - lo, end - bit);
            let mask = range_mask(lo, len);
            let word = &words[wi];
            let mut cur = word.load(Acquire);
            loop {
                if cur & mask != 0 {
                    for (w, m) in words.iter().zip(claimed) {
                        if m != 0 {
                            w.fetch_and(!m, Release);
                        }
                    }
                    return false;
                }
                match word.compare_exchange_weak(cur, cur | mask, AcqRel, Acquire) {
                    Ok(_) => break,
                    Err(v) => cur = v,
                }
            }
            claimed[wi] = mask;
            bit += len;
        }
        true
    }
}

/// Chemin rapide d'une frame : premier bit libre (first-fit, compacité), un CAS.
#[inline]
fn claim_one(words: &[AtomicU64]) -> Option<usize> {
    for _ in 0..MAX_CLAIM_RETRIES {
        for (i, word) in words.iter().enumerate() {
            let mut cur = word.load(Acquire);
            while cur != u64::MAX {
                let bit = (!cur).trailing_zeros() as usize;
                match word.compare_exchange_weak(cur, cur | (1 << bit), AcqRel, Acquire) {
                    Ok(_) => return Some(i * FRAMES_PER_WORD + bit),
                    Err(v) => cur = v,
                }
            }
        }
    }
    None
}

/// `n` bits (2..=64) alignés naturellement sur `n.next_power_of_two()` dans un seul mot.
fn claim_aligned(words: &[AtomicU64], n: usize) -> Option<usize> {
    let align = n.next_power_of_two();
    let base_mask = range_mask(0, n);
    for (i, word) in words.iter().enumerate() {
        let mut cur = word.load(Acquire);
        'word: loop {
            let mut off = 0;
            while off + n <= FRAMES_PER_WORD {
                let mask = base_mask << off;
                if cur & mask == 0 {
                    match word.compare_exchange(cur, cur | mask, AcqRel, Acquire) {
                        Ok(_) => return Some(i * FRAMES_PER_WORD + off),
                        Err(v) => {
                            cur = v;
                            continue 'word;
                        }
                    }
                }
                off += align;
            }
            break;
        }
    }
    None
}

/// Premier décalage d'une suite de `n` bits à zéro dans un instantané de 512 bits.
fn find_run(snap: &[u64; WORDS_PER_CHILD], n: usize) -> Option<usize> {
    let total = WORDS_PER_CHILD * FRAMES_PER_WORD;
    let mut run_start = 0usize;
    let mut run_len = 0usize;
    let mut i = 0usize;
    while i < total {
        let bit = i % FRAMES_PER_WORD;
        let shifted = snap[i / FRAMES_PER_WORD] >> bit;
        if shifted & 1 == 0 {
            let zeros = min(shifted.trailing_zeros() as usize, FRAMES_PER_WORD - bit);
            if run_len == 0 {
                run_start = i;
            }
            run_len += zeros;
            if run_len >= n {
                return Some(run_start);
            }
            i += zeros;
        } else {
            run_len = 0;
            i += min((!shifted).trailing_zeros() as usize, FRAMES_PER_WORD - bit);
        }
    }
    None
}

#[cfg(test)]
mod tests;
