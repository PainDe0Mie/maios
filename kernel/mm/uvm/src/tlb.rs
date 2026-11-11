//! Invalidation des TLB des espaces utilisateur, locale et entre cœurs.
//!
//! ## Lot d'invalidation ([`Flush`])
//! Toute opération qui retire une entrée, restreint des droits ou change la table P1 d'une plage
//! accumule ses pages dans un [`Flush`] (au-delà de 32 pages : vidage du PCID entier, le seuil de
//! Linux `tlb_single_page_flush_ceiling`). Les frames rendues (pages et tables) ne sont
//! libérées **qu'après** l'invalidation sur tous les cœurs concernés : aucun cœur ne peut encore
//! écrire dans une page libérée par un TLB périmé, ni suivre une table P1 libérée par ses caches
//! de structures de pagination (le `mmu_gather` de Linux).
//!
//! ## IPI ciblées
//! Seuls les cœurs qui exécutent **en ce moment** un fil de l'espace ([`AddressSpace`], masque
//! des cœurs actifs) reçoivent une IPI ; les cœurs en TLB paresseuse se resynchronisent par la
//! génération TLB à leur prochain chargement. Un processus à un seul fil n'envoie donc jamais
//! d'IPI. Chaque cœur initiateur a sa requête et chaque cœur cible une boîte (bitmap des
//! initiateurs) ; un initiateur qui attend ses accusés traite sa propre boîte, si bien que deux
//! cœurs qui s'invalident mutuellement, interruptions coupées, ne se bloquent pas (le schéma
//! `smp_call_function_many` de Linux). Matériel plus récent : INVLPGB/TLBSYNC (AMD Zen 3+) et RAR
//! (Intel) diffusent l'invalidation sans IPI — la structure ici permet de les brancher à la place
//! de [`shootdown`].

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use interrupts::interrupt_handler;
use memory::paging::address_space::{self, cpu_slot, AddressSpace};

/// Au-delà, on vide tout le PCID.
pub const MAX_PAGES: usize = 32;
const MAX_CPUS: usize = 512;
const FULL: usize = usize::MAX;

/// Invalidations et libérations différées d'une opération sur un espace.
pub struct Flush<'a> {
    space: &'a AddressSpace,
    pages: [usize; MAX_PAGES],
    n: usize,
    full: bool,
    /// Une entrée a été retirée ou restreinte : les autres cœurs doivent invalider.
    remote: bool,
    put_pages: Vec<usize>,
    put_tables: Vec<usize>,
}

impl<'a> Flush<'a> {
    pub fn new(space: &'a AddressSpace) -> Flush<'a> {
        Flush { space, pages: [0; MAX_PAGES], n: 0, full: false, remote: false, put_pages: Vec::new(), put_tables: Vec::new() }
    }

    /// Entrée de `va` retirée, restreinte ou remplacée.
    pub fn page(&mut self, va: usize) {
        self.remote = true;
        if self.full {
            return;
        }
        if self.n < MAX_PAGES {
            self.pages[self.n] = va;
            self.n += 1;
        } else {
            self.full = true;
        }
    }

    /// Tout l'espace (par exemple après `fork`, qui retire le bit W de toutes ses tables P1).
    pub fn all(&mut self) {
        self.remote = true;
        self.full = true;
    }

    /// Montée de droits seule sur `va` : invalidation locale, les autres cœurs prendront au pire
    /// une faute parasite résolue par la table à jour.
    pub fn upgrade(&mut self, va: usize) {
        if self.space.is_current() {
            address_space::flush_user_page_local(va);
        }
    }

    /// Page à rendre après l'invalidation.
    pub fn put_page(&mut self, pfn: usize) {
        self.put_pages.push(pfn);
    }

    /// Table P1 à rendre après l'invalidation (référence de cet espace).
    pub fn put_table(&mut self, pfn: usize) {
        self.remote = true;
        self.put_tables.push(pfn);
    }

    fn finish(&mut self) {
        if self.remote {
            // Une table quittée doit sortir des caches de structures de pagination : `invlpg`
            // les vide pour tout le PCID, mais il faut au moins une invalidation.
            if !self.put_tables.is_empty() && self.n == 0 {
                self.full = true;
            }
            self.space.bump_tlb_gen();
            let pages = &self.pages[..self.n];
            self.space.flush_local(pages, self.full);
            shootdown(self.space, pages, self.full);
        }
        for pfn in self.put_pages.drain(..) {
            crate::refs::put_page(pfn);
        }
        for pfn in self.put_tables.drain(..) {
            crate::refs::put_table(pfn);
        }
        self.remote = false;
        self.n = 0;
        self.full = false;
    }
}

impl Drop for Flush<'_> {
    fn drop(&mut self) {
        self.finish();
    }
}

// --- IPI ---------------------------------------------------------------------------------------

#[repr(C, align(64))]
struct Request {
    space: AtomicU64,
    n: AtomicUsize,
    pages: [AtomicUsize; MAX_PAGES],
    pending: AtomicU32,
}

#[allow(clippy::declare_interior_mutable_const)]
const ZERO_USIZE: AtomicUsize = AtomicUsize::new(0);
#[allow(clippy::declare_interior_mutable_const)]
const EMPTY_REQUEST: Request = Request {
    space: AtomicU64::new(0),
    n: AtomicUsize::new(0),
    pages: [ZERO_USIZE; MAX_PAGES],
    pending: AtomicU32::new(0),
};
#[allow(clippy::declare_interior_mutable_const)]
const ZERO_U64: AtomicU64 = AtomicU64::new(0);
#[allow(clippy::declare_interior_mutable_const)]
const EMPTY_INBOX: [AtomicU64; MAX_CPUS / 64] = [ZERO_U64; MAX_CPUS / 64];

/// Requête en cours de chaque cœur initiateur.
static REQUESTS: [Request; MAX_CPUS] = [EMPTY_REQUEST; MAX_CPUS];
/// Boîte de chaque cœur cible : bit `i` = le cœur `i` attend une invalidation.
static INBOX: [[AtomicU64; MAX_CPUS / 64]; MAX_CPUS] = [EMPTY_INBOX; MAX_CPUS];
static VECTOR: AtomicU8 = AtomicU8::new(0);
static VECTOR_INIT: spin::Once<()> = spin::Once::new();

fn vector() -> u8 {
    VECTOR_INIT.call_once(|| match interrupts::register_msi_interrupt(tlb_ipi_handler) {
        Ok(v) => {
            VECTOR.store(v, Ordering::Release);
            log::warn!("MaiProc : IPI d'invalidation TLB sur le vecteur {:#x}", v);
        }
        Err(e) => log::error!("MaiProc : pas de vecteur pour l'IPI d'invalidation TLB : {}", e),
    });
    VECTOR.load(Ordering::Acquire)
}

/// Réserve le vecteur d'IPI (à appeler hors de tout verrou, avant le premier processus).
pub fn init() {
    let _ = vector();
}

interrupt_handler!(tlb_ipi_handler, _, _stack_frame, {
    handle_inbox(cpu_slot());
    interrupts::EoiBehaviour::HandlerDidNotSendEoi
});

/// Traite les invalidations demandées à `cpu` par les autres cœurs.
fn handle_inbox(cpu: usize) {
    let inbox = &INBOX[cpu % MAX_CPUS];
    for (w, word) in inbox.iter().enumerate() {
        let mut bits = word.swap(0, Ordering::AcqRel);
        while bits != 0 {
            let b = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let req = &REQUESTS[w * 64 + b];
            let n = req.n.load(Ordering::Acquire);
            let space = req.space.load(Ordering::Acquire);
            if n == FULL {
                address_space::flush_loaded(space, &[], true);
            } else {
                let mut pages = [0usize; MAX_PAGES];
                for (d, s) in pages.iter_mut().zip(req.pages.iter()).take(n) {
                    *d = s.load(Ordering::Relaxed);
                }
                address_space::flush_loaded(space, &pages[..n], false);
            }
            req.pending.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// Invalide `pages` (ou tout l'espace) sur les cœurs qui exécutent un fil de `space`, et attend
/// qu'ils l'aient fait.
fn shootdown(space: &AddressSpace, pages: &[usize], full: bool) {
    let held = irq_safety::hold_interrupts();
    let me = cpu_slot() % MAX_CPUS;
    let mut targets: [u16; 64] = [0; 64];
    let mut count = 0usize;
    let mut overflow: Vec<u16> = Vec::new();
    space.for_each_active_cpu(me, |c| {
        if count < targets.len() {
            targets[count] = c as u16;
        } else {
            overflow.push(c as u16);
        }
        count += 1;
    });
    if count == 0 {
        drop(held);
        return;
    }
    let v = vector();
    if v == 0 {
        drop(held);
        return;
    }
    let req = &REQUESTS[me];
    req.space.store(space.id(), Ordering::Relaxed);
    if full {
        req.n.store(FULL, Ordering::Relaxed);
    } else {
        for (d, &s) in req.pages.iter().zip(pages) {
            d.store(s, Ordering::Relaxed);
        }
        req.n.store(pages.len(), Ordering::Relaxed);
    }
    req.pending.store(count as u32, Ordering::Release);
    let Some(apic) = apic::get_my_apic() else {
        log::error!("MaiProc : invalidation TLB sans APIC local");
        req.pending.store(0, Ordering::Release);
        drop(held);
        return;
    };
    for &t in targets.iter().take(count.min(64)).chain(overflow.iter()) {
        let t = t as usize;
        INBOX[t][me / 64].fetch_or(1u64 << (me % 64), Ordering::SeqCst);
        match apic::ApicId::try_from(t as u32) {
            Ok(id) => apic.write().send_ipi(v, apic::LapicIpiDestination::One(id)),
            Err(_) => {
                // Cœur inconnu : accusé à sa place (il ne peut pas exécuter de fil).
                INBOX[t][me / 64].fetch_and(!(1u64 << (me % 64)), Ordering::SeqCst);
                req.pending.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }
    let mut spins: u64 = 0;
    while req.pending.load(Ordering::Acquire) != 0 {
        handle_inbox(me);
        core::hint::spin_loop();
        spins += 1;
        if spins == 1 << 28 {
            log::error!("MaiProc : invalidation TLB toujours en attente ({} accuses manquants)", req.pending.load(Ordering::Relaxed));
        }
    }
    drop(held);
}
