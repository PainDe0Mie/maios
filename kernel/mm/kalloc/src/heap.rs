//! Pages, segments et tas par cœur : le cœur de l'allocateur.
//!
//! Règles de concurrence (celles de mimalloc) :
//! * un segment, ses pages et leurs listes `free` / `local_free` n'appartiennent qu'au tas d'un
//!   cœur, et ne sont modifiés que par ce cœur (interruptions coupées par l'appelant) ;
//! * un autre cœur ne touche qu'au mot atomique `thread_free` d'une page (empilement par CAS),
//!   et, pour une page pleine, à la liste atomique `delayed_free` du tas propriétaire ;
//! * un segment ne change de propriétaire que vide : plus aucun bloc vivant, donc plus aucune
//!   libération distante possible pendant le transfert.

use core::cell::UnsafeCell;
use core::mem::{size_of, MaybeUninit};
use core::ptr::{self, addr_of, addr_of_mut, null_mut};
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::*};

use crate::class::{self, NCLASS, SIZES};
use crate::{Kalloc, HEADER_SIZE, OS_WORDS, SEGMENT_MASK, SLICES, SLICE_SHIFT, SLICE_SIZE};

/// Un bloc libre : son premier mot est le lien (chiffré) vers le suivant.
#[repr(C)]
pub(crate) struct Block {
    next: usize,
}

// États de libération différée, dans les 2 bits faibles de `thread_free` (mimalloc).
/// La page est pleine : un cœur distant doit passer par la liste `delayed_free` du tas.
const USE_DELAYED: usize = 0;
/// Un cœur distant est en train de pousser un bloc dans `delayed_free`.
const DELAYED_FREEING: usize = 1;
/// Cas normal : les cœurs distants empilent dans `thread_free`.
const NO_DELAYED: usize = 2;
const DELAY_MASK: usize = 3;

const F_IN_USE: u8 = 1;
const F_IN_FULL: u8 = 2;

pub(crate) const NO_HEAP: u32 = u32::MAX;
/// Classe des pages larges (un seul bloc de plusieurs tranches, objets de 128 Kio à 1 Mio).
pub(crate) const LARGE_CLASS: u8 = u8::MAX;
const ALL_SLICES: u32 = u32::MAX;
const _: () = assert!(SLICES == 32);

/// Descripteur de page (une page = une suite de tranches de 64 Kio d'une même classe).
#[repr(C)]
pub(crate) struct Page {
    /// Blocs libres pour l'allocation (propriétaire seulement).
    pub free: *mut Block,
    /// Blocs libérés par le propriétaire, rendus à `free` quand elle est vide.
    pub local_free: *mut Block,
    /// Blocs libérés par d'autres cœurs | état de libération différée.
    pub thread_free: AtomicUsize,
    /// Clés de chiffrement des liens (mode sécurisé de mimalloc).
    pub keys: [usize; 2],
    /// Début et fin de la zone des blocs (contrôle de portée).
    pub start: usize,
    pub end: usize,
    pub next: *mut Page,
    pub prev: *mut Page,
    /// Blocs alloués (y compris ceux libérés à distance et pas encore récupérés).
    pub used: u32,
    /// Blocs déjà chaînés (extension paresseuse).
    pub capacity: u32,
    /// Blocs que la page peut contenir.
    pub reserved: u32,
    pub block_size: u32,
    pub class: u8,
    /// Longueur de la page en tranches (sur la première tranche).
    pub slice_count: u8,
    /// Sur une tranche intérieure : distance à la première tranche de la page.
    pub slice_offset: u8,
    pub flags: u8,
}

impl Page {
    pub(crate) const fn empty() -> Page {
        Page {
            free: null_mut(),
            local_free: null_mut(),
            thread_free: AtomicUsize::new(NO_DELAYED),
            keys: [0; 2],
            start: 0,
            end: 0,
            next: null_mut(),
            prev: null_mut(),
            used: 0,
            capacity: 0,
            reserved: 0,
            block_size: 0,
            class: 0,
            slice_count: 0,
            slice_offset: 0,
            flags: 0,
        }
    }
}

/// Page vide partagée : `free` toujours nul, ce qui fait tomber le chemin rapide dans le chemin
/// lent sans test supplémentaire. Jamais écrite.
struct SyncPage(UnsafeCell<Page>);
// SAFETY: jamais modifiée (le chemin rapide n'écrit que si `free` est non nul).
unsafe impl Sync for SyncPage {}
static EMPTY_PAGE: SyncPage = SyncPage(UnsafeCell::new(Page::empty()));

#[inline(always)]
fn empty_page() -> *mut Page {
    EMPTY_PAGE.0.get()
}

/// En-tête d'un segment de 2 Mio (dans ses `HEADER_SIZE` premiers octets).
#[repr(C)]
pub(crate) struct Segment {
    /// Adresse ^ témoin global : rejette les pointeurs qui ne viennent pas de kalloc.
    pub cookie: usize,
    /// Cœur propriétaire, ou `NO_HEAP` dans la réserve.
    pub heap: AtomicU32,
    /// Tranches libres (bit à 1).
    pub free_slices: u32,
    /// Le segment loge le tas de son cœur : il n'est jamais rendu.
    pub home: bool,
    /// Présent dans la liste des segments non pleins du tas.
    pub in_list: bool,
    pub next: *mut Segment,
    pub prev: *mut Segment,
    /// Données du système (mappage) : opaques pour kalloc.
    pub os: [usize; OS_WORDS],
    pub pages: [Page; SLICES],
    pub heap_storage: MaybeUninit<Heap>,
}

const _: () = assert!(size_of::<Segment>() <= HEADER_SIZE);

#[repr(C)]
pub(crate) struct PageQueue {
    first: *mut Page,
    last: *mut Page,
}

/// Compteurs d'un tas, écrits par son seul cœur (chargement + écriture, sans `lock`).
#[derive(Default)]
pub(crate) struct Counters {
    pub allocs: AtomicU64,
    pub frees: AtomicU64,
    pub remote_frees: AtomicU64,
    pub slow_allocs: AtomicU64,
    pub delayed_frees: AtomicU64,
    pub pages_fresh: AtomicU64,
    pub pages_retired: AtomicU64,
    pub segments_taken: AtomicU64,
    pub segments_released: AtomicU64,
}

#[inline(always)]
fn bump(c: &AtomicU64) {
    c.store(c.load(Relaxed).wrapping_add(1), Relaxed);
}

/// Tas d'un cœur.
#[repr(C)]
pub(crate) struct Heap {
    /// Page courante de chaque classe (chemin rapide : un chargement, un test).
    pub direct: [*mut Page; NCLASS],
    pub queues: [PageQueue; NCLASS],
    /// Pages pleines (hors des files : la recherche ne les parcourt plus).
    pub full: PageQueue,
    /// Pages larges en service (un objet chacune).
    pub large: PageQueue,
    /// Blocs libérés à distance dans des pages pleines (pile atomique, liens chiffrés par page).
    pub delayed_free: AtomicUsize,
    /// Segments du tas qui ont encore des tranches libres.
    pub segments: *mut Segment,
    /// Segments entièrement libres gardés en réserve locale.
    pub empty_segments: u32,
    pub cpu: u32,
    pub stats: Counters,
}

// ---------------------------------------------------------------------------
// Chiffrement des liens (mimalloc, MI_SECURE) : un débordement qui écrase un lien ne donne pas
// un pointeur utilisable, et la valeur décodée est contrôlée avant usage.
// ---------------------------------------------------------------------------

#[inline(always)]
fn encode(page: *const Page, keys: &[usize; 2], p: *mut Block) -> usize {
    let x = if p.is_null() { page as usize } else { p as usize };
    (x ^ keys[1]).rotate_left((keys[0] & 63) as u32).wrapping_add(keys[0])
}

#[inline(always)]
fn decode(page: *const Page, keys: &[usize; 2], x: usize) -> *mut Block {
    let p = x.wrapping_sub(keys[0]).rotate_right((keys[0] & 63) as u32) ^ keys[1];
    if p == page as usize { null_mut() } else { p as *mut Block }
}

#[cold]
#[inline(never)]
fn corrupted(page: *const Page) -> ! {
    panic!("kalloc: liste libre corrompue dans la page {:p} (debordement ou utilisation apres liberation)", page);
}

#[cold]
#[inline(never)]
fn invalid_free(ptr: usize, why: &str) -> ! {
    panic!("kalloc: liberation invalide de {:#x} : {}", ptr, why);
}

#[inline(always)]
unsafe fn checked(page: *const Page, b: *mut Block) -> *mut Block {
    if !b.is_null() && ((b as usize) < (*page).start || (b as usize) >= (*page).end) {
        corrupted(page);
    }
    b
}

// ---------------------------------------------------------------------------
// Adresses
// ---------------------------------------------------------------------------

#[inline(always)]
pub(crate) fn segment_of(p: usize) -> *mut Segment {
    (p & !SEGMENT_MASK) as *mut Segment
}

#[inline(always)]
pub(crate) unsafe fn page_of(seg: *mut Segment, p: usize) -> *mut Page {
    let idx = (p - seg as usize) >> SLICE_SHIFT;
    let pg = addr_of_mut!((*seg).pages).cast::<Page>().add(idx);
    pg.sub((*pg).slice_offset as usize)
}

#[inline(always)]
unsafe fn slice_index(seg: *mut Segment, page: *mut Page) -> usize {
    (page as usize - addr_of!((*seg).pages) as usize) / size_of::<Page>()
}

#[inline(always)]
unsafe fn page_ptr(seg: *mut Segment, i: usize) -> *mut Page {
    addr_of_mut!((*seg).pages).cast::<Page>().add(i)
}

// ---------------------------------------------------------------------------
// Files de pages
// ---------------------------------------------------------------------------

unsafe fn q_push_front(q: *mut PageQueue, p: *mut Page) {
    (*p).prev = null_mut();
    (*p).next = (*q).first;
    if (*q).first.is_null() {
        (*q).last = p;
    } else {
        (*(*q).first).prev = p;
    }
    (*q).first = p;
}

unsafe fn q_remove(q: *mut PageQueue, p: *mut Page) {
    if (*p).prev.is_null() {
        (*q).first = (*p).next;
    } else {
        (*(*p).prev).next = (*p).next;
    }
    if (*p).next.is_null() {
        (*q).last = (*p).prev;
    } else {
        (*(*p).next).prev = (*p).prev;
    }
    (*p).next = null_mut();
    (*p).prev = null_mut();
}

#[inline(always)]
unsafe fn queue(h: *mut Heap, c: usize) -> *mut PageQueue {
    addr_of_mut!((*h).queues).cast::<PageQueue>().add(c)
}

#[inline(always)]
unsafe fn update_direct(h: *mut Heap, c: usize) {
    let f = (*queue(h, c)).first;
    (*h).direct[c] = if f.is_null() { empty_page() } else { f };
}

// ---------------------------------------------------------------------------
// Liste des segments d'un tas
// ---------------------------------------------------------------------------

unsafe fn seg_list_push(h: *mut Heap, s: *mut Segment) {
    if (*s).in_list {
        return;
    }
    (*s).prev = null_mut();
    (*s).next = (*h).segments;
    if !(*h).segments.is_null() {
        (*(*h).segments).prev = s;
    }
    (*h).segments = s;
    (*s).in_list = true;
}

unsafe fn seg_list_remove(h: *mut Heap, s: *mut Segment) {
    if !(*s).in_list {
        return;
    }
    if (*s).prev.is_null() {
        (*h).segments = (*s).next;
    } else {
        (*(*s).prev).next = (*s).next;
    }
    if !(*s).next.is_null() {
        (*(*s).next).prev = (*s).prev;
    }
    (*s).next = null_mut();
    (*s).prev = null_mut();
    (*s).in_list = false;
}

/// Première suite de `n` tranches libres.
#[inline]
fn find_run(free: u32, n: usize) -> Option<usize> {
    let m: u32 = if n >= 32 { u32::MAX } else { (1u32 << n) - 1 };
    let mut i = 0;
    while i + n <= 32 {
        let w = free >> i;
        if w & m == m {
            return Some(i);
        }
        // Saute directement après le premier bit occupé de la fenêtre.
        let busy = !w & m;
        i += (busy.trailing_zeros() as usize) + 1;
    }
    None
}

// ---------------------------------------------------------------------------
// Tas
// ---------------------------------------------------------------------------

/// Crée le tas du cœur `cpu` dans un segment pris dans la réserve (qui devient son segment
/// « maison », jamais rendu). Nul si la réserve est vide.
pub(crate) unsafe fn heap_create(k: &Kalloc, cpu: usize) -> *mut Heap {
    let seg = k.pool_take();
    if seg.is_null() {
        return null_mut();
    }
    let h = addr_of_mut!((*seg).heap_storage).cast::<Heap>();
    ptr::write(
        h,
        Heap {
            direct: [empty_page(); NCLASS],
            queues: core::array::from_fn(|_| PageQueue { first: null_mut(), last: null_mut() }),
            full: PageQueue { first: null_mut(), last: null_mut() },
            large: PageQueue { first: null_mut(), last: null_mut() },
            delayed_free: AtomicUsize::new(0),
            segments: null_mut(),
            empty_segments: 0,
            cpu: cpu as u32,
            stats: Counters::default(),
        },
    );
    (*seg).home = true;
    (*seg).heap.store(cpu as u32, Relaxed);
    seg_list_push(h, seg);
    bump(&(*h).stats.segments_taken);
    k.heaps[cpu].store(h, Release);
    h
}

// ---------------------------------------------------------------------------
// Allocation
// ---------------------------------------------------------------------------

/// Chemin rapide : la page courante de la classe a un bloc libre. Aucune opération atomique.
#[inline(always)]
pub(crate) unsafe fn alloc(k: &Kalloc, cpu: usize, c: usize) -> Result<*mut u8, ()> {
    let mut h = k.heaps[cpu].load(Relaxed);
    if h.is_null() {
        h = heap_create(k, cpu);
        if h.is_null() {
            return Err(());
        }
    }
    let page = (*h).direct[c];
    let b = (*page).free;
    if !b.is_null() {
        pop(page, b);
        bump(&(*h).stats.allocs);
        return Ok(b as *mut u8);
    }
    alloc_slow(k, h, c)
}

#[inline(always)]
unsafe fn pop(page: *mut Page, b: *mut Block) {
    let next = checked(page, decode(page, &(*page).keys, (*b).next));
    (*page).free = next;
    (*page).used += 1;
}

#[inline(never)]
unsafe fn alloc_slow(k: &Kalloc, h: *mut Heap, c: usize) -> Result<*mut u8, ()> {
    bump(&(*h).stats.slow_allocs);
    drain_delayed(k, h);
    let page = find_page(k, h, c)?;
    let b = (*page).free;
    debug_assert!(!b.is_null());
    pop(page, b);
    bump(&(*h).stats.allocs);
    Ok(b as *mut u8)
}

/// Cherche une page de la classe `c` avec un bloc libre : récupère les libérations (locales et
/// distantes), étend paresseusement, sort les pages pleines de la file, et en dernier recours
/// crée une page. `Err` : il faut un nouveau segment.
unsafe fn find_page(k: &Kalloc, h: *mut Heap, c: usize) -> Result<*mut Page, ()> {
    let q = queue(h, c);
    let mut p = (*q).first;
    while !p.is_null() {
        let next = (*p).next;
        page_collect(p);
        if (*p).free.is_null() && (*p).capacity < (*p).reserved {
            page_extend(p);
        }
        if !(*p).free.is_null() {
            if (*q).first != p {
                q_remove(q, p);
                q_push_front(q, p);
            }
            update_direct(h, c);
            return Ok(p);
        }
        page_to_full(h, p);
        if !(*p).free.is_null() {
            // Des blocs sont arrivés pendant le passage en file pleine.
            page_unfull(h, p);
            update_direct(h, c);
            return Ok(p);
        }
        p = next;
    }
    page_fresh(k, h, c)
}

/// Récupère `thread_free` dans `local_free`, puis `local_free` dans `free` si elle est vide.
unsafe fn page_collect(p: *mut Page) {
    let mut tf = (*p).thread_free.load(Relaxed);
    while tf & !DELAY_MASK != 0 {
        match (*p).thread_free.compare_exchange_weak(tf, tf & DELAY_MASK, Acquire, Relaxed) {
            Ok(_) => {
                let head = (tf & !DELAY_MASK) as *mut Block;
                let keys = (*p).keys;
                let mut count = 1u32;
                let mut tail = checked(p, head);
                loop {
                    let n = checked(p, decode(p, &keys, (*tail).next));
                    if n.is_null() {
                        break;
                    }
                    tail = n;
                    count += 1;
                    if count > (*p).reserved {
                        corrupted(p);
                    }
                }
                if count > (*p).used {
                    corrupted(p);
                }
                (*tail).next = encode(p, &keys, (*p).local_free);
                (*p).local_free = head;
                (*p).used -= count;
                break;
            }
            Err(x) => tf = x,
        }
    }
    if (*p).free.is_null() && !(*p).local_free.is_null() {
        (*p).free = (*p).local_free;
        (*p).local_free = null_mut();
    }
}

/// Chaîne paresseusement les blocs suivants, 4 Kio à la fois (au moins 4 blocs) : une page
/// neuve ne touche que la mémoire qu'elle sert vraiment (mimalloc, « page extend »).
unsafe fn page_extend(p: *mut Page) {
    let bs = (*p).block_size as usize;
    let remaining = ((*p).reserved - (*p).capacity) as usize;
    let n = (4096 / bs).max(16).min(remaining);
    if n == 0 {
        return;
    }
    let first = (*p).start + (*p).capacity as usize * bs;
    let keys = (*p).keys;
    let mut b = first;
    for _ in 1..n {
        (*(b as *mut Block)).next = encode(p, &keys, (b + bs) as *mut Block);
        b += bs;
    }
    (*(b as *mut Block)).next = encode(p, &keys, (*p).free);
    (*p).free = first as *mut Block;
    (*p).capacity += n as u32;
}

/// Change l'état de libération différée (attend qu'un cœur distant ait fini son empilement).
unsafe fn set_delayed(p: *mut Page, state: usize) {
    loop {
        let tf = (*p).thread_free.load(Acquire);
        let cur = tf & DELAY_MASK;
        if cur == DELAYED_FREEING {
            core::hint::spin_loop();
            continue;
        }
        if cur == state {
            return;
        }
        if (*p).thread_free.compare_exchange_weak(tf, (tf & !DELAY_MASK) | state, AcqRel, Relaxed).is_ok() {
            return;
        }
    }
}

unsafe fn page_to_full(h: *mut Heap, p: *mut Page) {
    set_delayed(p, USE_DELAYED);
    let c = (*p).class as usize;
    q_remove(queue(h, c), p);
    update_direct(h, c);
    q_push_front(addr_of_mut!((*h).full), p);
    (*p).flags |= F_IN_FULL;
    // Des libérations distantes ont pu arriver juste avant USE_DELAYED.
    page_collect(p);
}

unsafe fn page_unfull(h: *mut Heap, p: *mut Page) {
    if (*p).flags & F_IN_FULL == 0 {
        return;
    }
    set_delayed(p, NO_DELAYED);
    q_remove(addr_of_mut!((*h).full), p);
    (*p).flags &= !F_IN_FULL;
    let c = (*p).class as usize;
    q_push_front(queue(h, c), p);
    update_direct(h, c);
}

/// Nouvelle page de la classe `c`, prise dans un segment du tas ou dans la réserve.
unsafe fn page_fresh(k: &Kalloc, h: *mut Heap, c: usize) -> Result<*mut Page, ()> {
    let n = class::slices(c);
    let (seg, i) = span_alloc(k, h, n)?;
    let p = page_ptr(seg, i);
    for j in 1..n {
        (*page_ptr(seg, i + j)).slice_offset = j as u8;
    }
    let bs = SIZES[c] as usize;
    let skip = if i == 0 { HEADER_SIZE } else { 0 };
    let start = seg as usize + i * SLICE_SIZE + skip;
    let area = n * SLICE_SIZE - skip;
    let reserved = area / bs;
    (*p).free = null_mut();
    (*p).local_free = null_mut();
    (*p).thread_free.store(NO_DELAYED, Relaxed);
    (*p).keys = k.page_keys(p as usize);
    (*p).start = start;
    (*p).end = start + reserved * bs;
    (*p).next = null_mut();
    (*p).prev = null_mut();
    (*p).used = 0;
    (*p).capacity = 0;
    (*p).reserved = reserved as u32;
    (*p).block_size = bs as u32;
    (*p).class = c as u8;
    (*p).slice_count = n as u8;
    (*p).slice_offset = 0;
    (*p).flags = F_IN_USE;
    page_extend(p);
    q_push_front(queue(h, c), p);
    update_direct(h, c);
    bump(&(*h).stats.pages_fresh);
    Ok(p)
}

/// Réserve `n` tranches contiguës. On préfère un segment déjà entamé à un segment vide, pour
/// que les segments vides puissent être rendus.
unsafe fn span_alloc(k: &Kalloc, h: *mut Heap, n: usize) -> Result<(*mut Segment, usize), ()> {
    let mut empty: Option<(*mut Segment, usize)> = None;
    let mut s = (*h).segments;
    while !s.is_null() {
        if let Some(i) = find_run((*s).free_slices, n) {
            if (*s).free_slices == ALL_SLICES && !(*s).home {
                if empty.is_none() {
                    empty = Some((s, i));
                }
            } else {
                take_span(h, s, i, n);
                return Ok((s, i));
            }
        }
        s = (*s).next;
    }
    if let Some((s, i)) = empty {
        (*h).empty_segments -= 1;
        take_span(h, s, i, n);
        return Ok((s, i));
    }
    let s = k.pool_take();
    if s.is_null() {
        return Err(());
    }
    (*s).heap.store((*h).cpu, Relaxed);
    seg_list_push(h, s);
    bump(&(*h).stats.segments_taken);
    take_span(h, s, 0, n);
    Ok((s, 0))
}

unsafe fn take_span(h: *mut Heap, s: *mut Segment, i: usize, n: usize) {
    let m: u32 = if n >= 32 { u32::MAX } else { ((1u32 << n) - 1) << i };
    (*s).free_slices &= !m;
    if (*s).free_slices == 0 {
        seg_list_remove(h, s);
    }
}

/// Objet de 128 Kio à 1 Mio : une page d'un seul bloc, faite de tranches contiguës d'un
/// segment (pages « large » de mimalloc). Pas de mappage ni de vidage TLB, contrairement au
/// mappage direct, et la mémoire est rendue au segment dès la libération.
pub(crate) unsafe fn alloc_large(k: &Kalloc, cpu: usize, size: usize) -> Result<*mut u8, ()> {
    let mut h = k.heaps[cpu].load(Relaxed);
    if h.is_null() {
        h = heap_create(k, cpu);
        if h.is_null() {
            return Err(());
        }
    }
    bump(&(*h).stats.slow_allocs);
    // Pages larges libérées par d'autres cœurs : rendues avant d'en prendre une autre.
    collect_large(k, h);
    // Assez de tranches même si la page tombe sur la tranche 0 (en-tête du segment).
    let n = (size + HEADER_SIZE + SLICE_SIZE - 1) / SLICE_SIZE;
    let (seg, i) = span_alloc(k, h, n)?;
    let p = page_ptr(seg, i);
    for j in 1..n {
        (*page_ptr(seg, i + j)).slice_offset = j as u8;
    }
    let skip = if i == 0 { HEADER_SIZE } else { 0 };
    let start = seg as usize + i * SLICE_SIZE + skip;
    let area = n * SLICE_SIZE - skip;
    (*p).free = null_mut();
    (*p).local_free = null_mut();
    (*p).thread_free.store(NO_DELAYED, Relaxed);
    (*p).keys = k.page_keys(p as usize);
    (*p).start = start;
    (*p).end = start + area;
    (*p).next = null_mut();
    (*p).prev = null_mut();
    (*p).used = 1;
    (*p).capacity = 1;
    (*p).reserved = 1;
    (*p).block_size = area as u32;
    (*p).class = LARGE_CLASS;
    (*p).slice_count = n as u8;
    (*p).slice_offset = 0;
    (*p).flags = F_IN_USE;
    q_push_front(addr_of_mut!((*h).large), p);
    bump(&(*h).stats.pages_fresh);
    bump(&(*h).stats.allocs);
    Ok(start as *mut u8)
}

/// Rend les pages larges dont l'objet a été libéré par un autre cœur.
unsafe fn collect_large(k: &Kalloc, h: *mut Heap) -> bool {
    let mut trim = false;
    let mut p = (*h).large.first;
    while !p.is_null() {
        let next = (*p).next;
        page_collect(p);
        if (*p).used == 0 {
            trim |= page_retire(k, h, p, true);
        }
        p = next;
    }
    trim
}

// ---------------------------------------------------------------------------
// Libération
// ---------------------------------------------------------------------------

/// Libère `ptr` depuis le cœur `cpu`. Vrai si la réserve globale dépasse son plafond (l'appelant
/// rendra des segments au système, hors section critique).
#[inline(always)]
pub(crate) unsafe fn free(k: &Kalloc, cpu: usize, ptr: usize) -> bool {
    let seg = segment_of(ptr);
    if (*seg).cookie != (seg as usize ^ k.cookie()) {
        invalid_free(ptr, "pointeur hors des segments de kalloc");
    }
    let p = page_of(seg, ptr);
    if ptr < (*p).start || ptr >= (*p).end {
        invalid_free(ptr, "hors de la zone des blocs (page deja rendue : double liberation ?)");
    }
    #[cfg(debug_assertions)]
    if (ptr - (*p).start) % (*p).block_size as usize != 0 {
        invalid_free(ptr, "pointeur au milieu d'un bloc");
    }
    let b = ptr as *mut Block;
    let owner = (*seg).heap.load(Relaxed) as usize;
    if owner == cpu {
        let h = k.heaps[cpu].load(Relaxed);
        free_local(k, h, p, b)
    } else {
        free_remote(k, p, b);
        let me = k.heaps.get(cpu).map(|x| x.load(Relaxed)).unwrap_or(null_mut());
        if !me.is_null() {
            bump(&(*me).stats.remote_frees);
        }
        false
    }
}

/// Libération par le propriétaire : empilement sans atomique dans `local_free`.
unsafe fn free_local(k: &Kalloc, h: *mut Heap, p: *mut Page, b: *mut Block) -> bool {
    check_double_free(p, b);
    if (*p).used == 0 {
        invalid_free(b as usize, "double liberation (page deja vide)");
    }
    (*b).next = encode(p, &(*p).keys, (*p).local_free);
    (*p).local_free = b;
    (*p).used -= 1;
    bump(&(*h).stats.frees);
    if (*p).flags & F_IN_FULL != 0 {
        page_unfull(h, p);
    }
    if (*p).used == 0 {
        return page_retire(k, h, p, false);
    }
    false
}

/// Détection de double libération (mimalloc, `mi_check_is_double_free`) : un bloc vivant contient
/// des données de l'utilisateur, qui ne se décodent presque jamais en lien valide. Si c'est le
/// cas, on parcourt les listes de la page pour confirmer.
#[inline(always)]
unsafe fn check_double_free(p: *mut Page, b: *mut Block) {
    let n = decode(p, &(*p).keys, (*b).next);
    if n.is_null() || ((n as usize) >= (*p).start && (n as usize) < (*p).end) {
        confirm_double_free(p, b);
    }
}

#[cold]
#[inline(never)]
unsafe fn confirm_double_free(p: *mut Page, b: *mut Block) {
    let keys = (*p).keys;
    let in_list = |mut x: *mut Block| -> bool {
        let mut guard = 0u32;
        while !x.is_null() && guard <= (*p).reserved {
            if x == b {
                return true;
            }
            x = decode(p, &keys, (*x).next);
            if !x.is_null() && ((x as usize) < (*p).start || (x as usize) >= (*p).end) {
                return false;
            }
            guard += 1;
        }
        false
    };
    if in_list((*p).free) || in_list((*p).local_free) {
        invalid_free(b as usize, "double liberation");
    }
}

/// Libération depuis un autre cœur : un CAS sur `thread_free` ; si la page est pleine, le bloc
/// passe par la liste `delayed_free` du tas propriétaire pour qu'il la remette en file.
unsafe fn free_remote(k: &Kalloc, p: *mut Page, b: *mut Block) {
    let keys = (*p).keys;
    let mut tf = (*p).thread_free.load(Relaxed);
    let use_delayed;
    loop {
        let delayed = tf & DELAY_MASK == USE_DELAYED;
        let new = if delayed {
            (tf & !DELAY_MASK) | DELAYED_FREEING
        } else {
            (*b).next = encode(p, &keys, (tf & !DELAY_MASK) as *mut Block);
            (b as usize) | (tf & DELAY_MASK)
        };
        match (*p).thread_free.compare_exchange_weak(tf, new, AcqRel, Relaxed) {
            Ok(_) => {
                use_delayed = delayed;
                break;
            }
            Err(x) => tf = x,
        }
    }
    if !use_delayed {
        return;
    }
    let seg = segment_of(b as usize);
    let owner = (*seg).heap.load(Relaxed) as usize;
    let oh = k.heaps[owner].load(Acquire);
    let mut d = (*oh).delayed_free.load(Relaxed);
    loop {
        (*b).next = encode(p, &keys, d as *mut Block);
        match (*oh).delayed_free.compare_exchange_weak(d, b as usize, Release, Relaxed) {
            Ok(_) => break,
            Err(x) => d = x,
        }
    }
    // DELAYED_FREEING -> NO_DELAYED : les libérations suivantes repassent par `thread_free`.
    let mut tf = (*p).thread_free.load(Relaxed);
    loop {
        match (*p).thread_free.compare_exchange_weak(tf, (tf & !DELAY_MASK) | NO_DELAYED, Release, Relaxed) {
            Ok(_) => break,
            Err(x) => tf = x,
        }
    }
}

/// Traite les blocs libérés à distance dans des pages pleines : ils sont libérés localement, ce
/// qui remet leur page en file.
unsafe fn drain_delayed(k: &Kalloc, h: *mut Heap) -> bool {
    if (*h).delayed_free.load(Relaxed) == 0 {
        return false;
    }
    let mut trim = false;
    let mut cur = (*h).delayed_free.swap(0, Acquire) as *mut Block;
    while !cur.is_null() {
        let b = cur;
        let seg = segment_of(b as usize);
        let p = page_of(seg, b as usize);
        cur = decode(p, &(*p).keys, (*b).next);
        set_delayed(p, NO_DELAYED);
        bump(&(*h).stats.delayed_frees);
        // Pas de contrôle de double libération ici : le lien contient celui de la liste différée.
        (*b).next = 0;
        if (*p).used == 0 {
            corrupted(p);
        }
        (*b).next = encode(p, &(*p).keys, (*p).local_free);
        (*p).local_free = b;
        (*p).used -= 1;
        if (*p).flags & F_IN_FULL != 0 {
            page_unfull(h, p);
        }
        if (*p).used == 0 {
            trim |= page_retire(k, h, p, false);
        }
    }
    trim
}

/// Une page devient vide. Hors maintenance, on garde la dernière page d'une classe (éviter de
/// rendre puis reprendre la même page en boucle, cf. `retire_expire` de mimalloc).
unsafe fn page_retire(k: &Kalloc, h: *mut Heap, p: *mut Page, force: bool) -> bool {
    if (*p).class == LARGE_CLASS {
        q_remove(addr_of_mut!((*h).large), p);
        bump(&(*h).stats.pages_retired);
        return span_free(k, h, p);
    }
    let c = (*p).class as usize;
    let q = queue(h, c);
    if !force && (*q).first == p && (*q).last == p {
        return false;
    }
    if (*p).flags & F_IN_FULL != 0 {
        q_remove(addr_of_mut!((*h).full), p);
    } else {
        q_remove(q, p);
    }
    update_direct(h, c);
    bump(&(*h).stats.pages_retired);
    span_free(k, h, p)
}

/// Rend les tranches d'une page à son segment ; un segment entièrement libre au-delà de la
/// réserve locale (un par tas) retourne dans la réserve globale.
unsafe fn span_free(k: &Kalloc, h: *mut Heap, p: *mut Page) -> bool {
    let seg = segment_of(p as usize);
    let i = slice_index(seg, p);
    let n = (*p).slice_count as usize;
    for j in 0..n {
        ptr::write(page_ptr(seg, i + j), Page::empty());
    }
    let m: u32 = if n >= 32 { u32::MAX } else { ((1u32 << n) - 1) << i };
    (*seg).free_slices |= m;
    seg_list_push(h, seg);
    if (*seg).free_slices == ALL_SLICES && !(*seg).home {
        if (*h).empty_segments >= 1 {
            return segment_release(k, h, seg);
        }
        (*h).empty_segments += 1;
    }
    false
}

unsafe fn segment_release(k: &Kalloc, h: *mut Heap, seg: *mut Segment) -> bool {
    seg_list_remove(h, seg);
    (*seg).heap.store(NO_HEAP, Relaxed);
    bump(&(*h).stats.segments_released);
    k.pool_put(seg)
}

/// Maintenance (cœur inactif) : libérations différées, récupération de toutes les libérations
/// distantes, et retour des pages et segments vides.
pub(crate) unsafe fn collect(k: &Kalloc, h: *mut Heap) -> bool {
    let mut trim = drain_delayed(k, h);
    for c in 0..NCLASS {
        let mut p = (*queue(h, c)).first;
        while !p.is_null() {
            let next = (*p).next;
            page_collect(p);
            if (*p).used == 0 {
                trim |= page_retire(k, h, p, true);
            }
            p = next;
        }
    }
    trim |= collect_large(k, h);
    let mut p = (*h).full.first;
    while !p.is_null() {
        let next = (*p).next;
        page_collect(p);
        if !(*p).free.is_null() || !(*p).local_free.is_null() {
            page_unfull(h, p);
        }
        if (*p).used == 0 {
            trim |= page_retire(k, h, p, true);
        }
        p = next;
    }
    // Plus de réserve locale : la maintenance rend aussi le segment vide gardé en secours.
    let mut s = (*h).segments;
    while !s.is_null() {
        let next = (*s).next;
        if (*s).free_slices == ALL_SLICES && !(*s).home {
            (*h).empty_segments -= 1;
            trim |= segment_release(k, h, s);
        }
        s = next;
    }
    trim
}

/// Taille réelle du bloc qui contient `ptr`.
pub(crate) unsafe fn block_size(ptr: usize) -> usize {
    let seg = segment_of(ptr);
    (*page_of(seg, ptr)).block_size as usize
}

/// Initialise l'en-tête d'un segment neuf.
pub(crate) unsafe fn segment_init(k: &Kalloc, base: usize, os: [usize; OS_WORDS]) -> *mut Segment {
    let s = base as *mut Segment;
    ptr::write_bytes(base as *mut u8, 0, size_of::<Segment>());
    (*s).cookie = base ^ k.cookie();
    (*s).heap = AtomicU32::new(NO_HEAP);
    (*s).free_slices = ALL_SLICES;
    (*s).os = os;
    for i in 0..SLICES {
        ptr::write(page_ptr(s, i), Page::empty());
    }
    s
}

pub(crate) unsafe fn segment_os(s: *mut Segment) -> [usize; OS_WORDS] {
    (*s).os
}

/// Lien de la réserve globale (le segment est vide : on réutilise `next`).
pub(crate) unsafe fn segment_next(s: *mut Segment) -> *mut *mut Segment {
    addr_of_mut!((*s).next)
}

/// Remet à zéro l'état de propriété d'un segment vide avant de le donner à un autre tas.
pub(crate) unsafe fn segment_reset(s: *mut Segment) {
    (*s).next = null_mut();
    (*s).prev = null_mut();
    (*s).in_list = false;
    (*s).home = false;
    (*s).heap.store(NO_HEAP, Relaxed);
}

pub(crate) unsafe fn heap_counters(h: *mut Heap) -> &'static Counters {
    &(*h).stats
}
