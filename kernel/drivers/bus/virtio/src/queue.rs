//! Virtqueue séparée (VirtIO 1.3, § 2.7).
//!
//! Disposition, dans une seule allocation [`Dma`] :
//!
//! ```text
//!  0                 table des descripteurs : size × 16 octets (alignement 16)
//!  16·size           anneau « avail » : flags, idx, ring[size], used_event   (alignement 2)
//!  align4(…)         anneau « used »  : flags, idx, ring[size]{id, len}, avail_event (alignement 4)
//! ```
//!
//! La structure n'est **pas** synchronisée : le pilote la garde sous un verrou sûr en
//! interruption (elle est vidée par l'interruption et par le sondage).
//!
//! Notifications (§ 2.7.10) : avec `EVENT_IDX`, on ne sonne que si l'indice publié par
//! l'appareil (`avail_event`) a été franchi depuis la dernière sonnerie (`need_event`, la règle
//! de Linux et de NVMe) ; sinon, seulement si l'appareil n'a pas posé `VIRTQ_USED_F_NO_NOTIFY`.
//! Interruptions (§ 2.7.7) : avec `EVENT_IDX`, `used_event` vaut le dernier indice vu, et
//! l'appareil n'interrompt qu'au franchissement ; après l'avoir réarmé, on revérifie l'anneau
//! (une fin arrivée entre la dernière lecture et l'écriture de `used_event` n'interromprait
//! pas).

use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{fence, Ordering};

use crate::Dma;

const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;
const AVAIL_F_NO_INTERRUPT: u16 = 1;
const USED_F_NO_NOTIFY: u16 = 1;

/// Un tampon d'une chaîne : lisible par l'appareil, ou inscriptible (`write`).
#[derive(Clone, Copy, Debug)]
pub struct Buf {
    pub phys: u64,
    pub len: u32,
    pub write: bool,
}

/// Plus assez de descripteurs libres.
#[derive(Debug)]
pub struct QueueFull;

pub struct SplitQueue {
    _mem: Dma,
    index: u16,
    size: u16,
    desc: usize,
    avail: usize,
    used: usize,
    event_idx: bool,
    notify_addr: usize,
    free_head: u16,
    num_free: u16,
    /// Nombre de descripteurs de la chaîne commencée en chaque tête (0 : libre).
    chain_len: Vec<u16>,
    /// Copie de `avail.idx` (seul le pilote l'écrit).
    avail_idx: u16,
    /// `avail.idx` à la dernière sonnerie.
    kicked_idx: u16,
    last_used: u16,
    /// Physique des trois parties, pour la configuration commune.
    pub desc_phys: u64,
    pub avail_phys: u64,
    pub used_phys: u64,
}

// SAFETY : les adresses désignent la `Dma` possédée et un registre MMIO projeté pour la vie de
// l'appareil ; l'accès est sérialisé par le verrou du pilote.
unsafe impl Send for SplitQueue {}

#[inline]
fn need_event(event: u16, new: u16, old: u16) -> bool {
    new.wrapping_sub(event).wrapping_sub(1) < new.wrapping_sub(old)
}

impl SplitQueue {
    /// Crée une file de `size` entrées (puissance de deux), sonnée en écrivant `index` à
    /// `notify_addr`.
    pub fn new(index: u16, size: u16, event_idx: bool, notify_addr: usize) -> Result<SplitQueue, &'static str> {
        if size == 0 || !size.is_power_of_two() {
            return Err("virtio : taille de file invalide");
        }
        let n = size as usize;
        let avail_off = 16 * n;
        let used_off = (avail_off + 6 + 2 * n + 3) & !3;
        let total = used_off + 6 + 8 * n;
        let mem = Dma::new(total)?;
        let base = mem.va();
        let phys = mem.phys();
        let q = SplitQueue {
            index,
            size,
            desc: base,
            avail: base + avail_off,
            used: base + used_off,
            event_idx,
            notify_addr,
            free_head: 0,
            num_free: size,
            chain_len: vec![0; n],
            avail_idx: 0,
            kicked_idx: 0,
            last_used: 0,
            desc_phys: phys,
            avail_phys: phys + avail_off as u64,
            used_phys: phys + used_off as u64,
            _mem: mem,
        };
        // Descripteurs libres chaînés : i → i + 1.
        for i in 0..size {
            q.write_desc(i, 0, 0, 0, i.wrapping_add(1));
        }
        Ok(q)
    }

    #[inline]
    pub fn size(&self) -> u16 {
        self.size
    }

    #[inline]
    pub fn index(&self) -> u16 {
        self.index
    }

    #[inline]
    pub fn num_free(&self) -> u16 {
        self.num_free
    }

    fn write_desc(&self, i: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let p = self.desc + 16 * i as usize;
        // SAFETY : entrée `i < size` de notre table de descripteurs.
        unsafe {
            (p as *mut u64).write_volatile(addr);
            ((p + 8) as *mut u32).write_volatile(len);
            ((p + 12) as *mut u16).write_volatile(flags);
            ((p + 14) as *mut u16).write_volatile(next);
        }
    }

    fn desc_next(&self, i: u16) -> u16 {
        // SAFETY : entrée `i < size` de notre table.
        unsafe { ((self.desc + 16 * i as usize + 14) as *const u16).read_volatile() }
    }

    #[inline]
    fn used_idx(&self) -> u16 {
        // SAFETY : champ `idx` de l'anneau « used », écrit par l'appareil.
        unsafe { ((self.used + 2) as *const u16).read_volatile() }
    }

    /// Publie la chaîne `bufs` (les tampons lisibles d'abord, puis les inscriptibles) et rend sa
    /// tête, qui reviendra par [`Self::pop_used`]. Ne sonne pas : voir [`Self::kick_needed`].
    pub fn add(&mut self, bufs: &[Buf]) -> Result<u16, QueueFull> {
        let n = bufs.len() as u16;
        if n == 0 || n > self.num_free {
            return Err(QueueFull);
        }
        let head = self.free_head;
        let mut cur = head;
        for (k, b) in bufs.iter().enumerate() {
            let next = self.desc_next(cur);
            let last = k + 1 == bufs.len();
            let flags = if last { 0 } else { DESC_F_NEXT } | if b.write { DESC_F_WRITE } else { 0 };
            self.write_desc(cur, b.phys, b.len, flags, if last { 0 } else { next });
            if last {
                self.free_head = next;
            } else {
                cur = next;
            }
        }
        self.num_free -= n;
        self.chain_len[head as usize] = n;
        let slot = self.avail_idx % self.size;
        // SAFETY : `ring[slot]` de l'anneau « avail » ; l'indice n'est publié qu'après.
        unsafe { ((self.avail + 4 + 2 * slot as usize) as *mut u16).write_volatile(head) };
        self.avail_idx = self.avail_idx.wrapping_add(1);
        // Les descripteurs et l'entrée de l'anneau avant l'indice (§ 2.7.13.3.1).
        fence(Ordering::Release);
        // SAFETY : champ `idx` de l'anneau « avail ».
        unsafe { ((self.avail + 2) as *mut u16).write_volatile(self.avail_idx) };
        Ok(head)
    }

    /// Vrai s'il faut sonner l'appareil pour ce qui a été publié depuis la dernière sonnerie
    /// (et le note comme sonné).
    pub fn kick_needed(&mut self) -> bool {
        let new = self.avail_idx;
        let old = self.kicked_idx;
        if new == old {
            return false;
        }
        // L'indice publié avant de lire ce que demande l'appareil (§ 2.7.13.4).
        fence(Ordering::SeqCst);
        self.kicked_idx = new;
        if self.event_idx {
            let n = self.size as usize;
            // SAFETY : `avail_event`, après `ring[size]` de l'anneau « used ».
            let ev = unsafe { ((self.used + 4 + 8 * n) as *const u16).read_volatile() };
            need_event(ev, new, old)
        } else {
            // SAFETY : champ `flags` de l'anneau « used ».
            let flags = unsafe { (self.used as *const u16).read_volatile() };
            flags & USED_F_NO_NOTIFY == 0
        }
    }

    /// Sonne l'appareil (une écriture MMIO : une sortie de VM).
    pub fn notify(&self) {
        // SAFETY : registre de notification projeté de cette file.
        unsafe { (self.notify_addr as *mut u16).write_volatile(self.index) }
    }

    /// Une fin : `(tête, octets écrits par l'appareil)`. Les descripteurs de la chaîne sont
    /// rendus.
    pub fn pop_used(&mut self) -> Option<(u16, u32)> {
        if self.used_idx() == self.last_used {
            return None;
        }
        // L'entrée après l'indice (§ 2.7.14).
        fence(Ordering::Acquire);
        let slot = (self.last_used % self.size) as usize;
        let e = self.used + 4 + 8 * slot;
        // SAFETY : `ring[slot]` de l'anneau « used ».
        let (id, len) = unsafe { ((e as *const u32).read_volatile(), ((e + 4) as *const u32).read_volatile()) };
        self.last_used = self.last_used.wrapping_add(1);
        let head = id as u16;
        if head >= self.size || self.chain_len[head as usize] == 0 {
            log::error!("virtio : file {} : fin pour une tête invalide ou libre ({})", self.index, id);
            return None;
        }
        // Rendre la chaîne : sa queue pointe vers l'ancienne tête libre.
        let n = self.chain_len[head as usize];
        self.chain_len[head as usize] = 0;
        let mut last = head;
        for _ in 1..n {
            last = self.desc_next(last);
        }
        self.write_desc(last, 0, 0, 0, self.free_head);
        self.free_head = head;
        self.num_free += n;
        Some((head, len))
    }

    /// Demande une interruption à la prochaine fin. Rend vrai s'il y a déjà des fins à
    /// prendre (l'appelant revide alors l'anneau).
    pub fn enable_interrupts(&mut self) -> bool {
        if self.event_idx {
            let n = self.size as usize;
            // SAFETY : `used_event`, après `ring[size]` de l'anneau « avail ».
            unsafe { ((self.avail + 4 + 2 * n) as *mut u16).write_volatile(self.last_used) };
        } else {
            // SAFETY : champ `flags` de l'anneau « avail ».
            unsafe { (self.avail as *mut u16).write_volatile(0) };
        }
        fence(Ordering::SeqCst);
        self.used_idx() != self.last_used
    }

    /// Ne demande plus d'interruption (indicatif : l'appareil peut encore interrompre).
    pub fn disable_interrupts(&mut self) {
        if !self.event_idx {
            // SAFETY : champ `flags` de l'anneau « avail ».
            unsafe { (self.avail as *mut u16).write_volatile(AVAIL_F_NO_INTERRUPT) };
        }
        // Avec EVENT_IDX, `used_event` laissé en arrière ne déclenche qu'au prochain franchissement.
    }

    /// Fins en attente (sans les prendre).
    pub fn has_used(&self) -> bool {
        self.used_idx() != self.last_used
    }
}
