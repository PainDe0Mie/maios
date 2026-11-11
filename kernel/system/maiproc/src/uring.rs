//! io_uring : entrées-sorties asynchrones par anneaux partagés (Axboe, *Efficient IO with
//! io_uring*, 2019), même ABI que Linux.
//!
//! Le programme dépose des requêtes (SQE) dans l'anneau de soumission et lit les résultats (CQE)
//! dans l'anneau de complétion ; les deux sont des pages communes au programme (projection
//! `MAP_SHARED`) et au noyau (physmap), sans copie ni appel système par opération. Les têtes et
//! les queues des anneaux sont sur des lignes de cache distinctes (pas de faux partage entre le
//! producteur et le consommateur).
//!
//! Exécution, comme Linux :
//! - **en ligne, sans attente** : chaque requête est d'abord tentée dans l'appel qui la soumet,
//!   en mode non bloquant (`IOCB_NOWAIT`) ;
//! - **sondage rapide** (`IORING_FEAT_FAST_POLL`) : une requête qui attendrait un fichier
//!   sondable (socket, tube, `eventfd`…) ne mobilise aucun fil ; un observateur est posé sur les
//!   files d'attente du fichier (même mécanisme qu'`epoll`), et la requête est reprise quand il
//!   devient prêt ;
//! - **travailleurs** (io-wq) : ce qui doit vraiment bloquer (terminal, `IOSQE_ASYNC`, attentes
//!   d'une connexion pleine) part dans un groupe de travailleurs propres à l'anneau, fils
//!   internes du processus (`PF_IO_WORKER`), créés à la demande et retirés après inactivité ;
//! - **`IORING_SETUP_SQPOLL`** : un fil du noyau sonde l'anneau de soumission ; le programme
//!   soumet sans aucun appel système tant que ce fil est actif (`IORING_SQ_NEED_WAKEUP` sinon).
//!
//! Gérés : chaînes (`IOSQE_IO_LINK`, `IO_HARDLINK`, `LINK_TIMEOUT`), `IO_DRAIN`,
//! `CQE_SKIP_SUCCESS`, fichiers et tampons enregistrés, tampons fournis (`BUFFER_SELECT`),
//! opérations multiples (`POLL_ADD`, `ACCEPT`, `RECV`, `TIMEOUT` à plusieurs coups), délais par
//! durée et par nombre de complétions, annulation, `eventfd` de notification, débordement sans
//! perte (`IORING_FEAT_NODROP`), `MSG_RING` entre anneaux, sondage des opérations (`PROBE`).

use alloc::{
    collections::{BTreeMap, VecDeque},
    format,
    string::String,
    sync::{Arc, Weak},
    task::Wake,
    vec,
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use core::time::Duration;
use kernel_config::memory::PAGE_SIZE;
use memory::paging::kernel_half::phys_to_virt;
use sync_irq::IrqSafeMutex;
use sync_preemption::PreemptionSafeMutex as Mutex;
use uvm::{Mm, ShmObject};

use crate::fd::{Node, OpenFile, O_RDWR};
use crate::linux::errno::*;
use crate::linux::{fault, SysResult};
use crate::object::{downcast_arc, ev, new_ino, object_any, FileObject};
use crate::poll::{for_each_queue, poll_file};
use crate::wait::{wait_event, WaitError, WaitQueue, Watcher};
use crate::{Process, Thread};

const ETIME: i64 = 62;
const EBADFD: i64 = 77;
const ECANCELED: i64 = 125;

// --- ABI --------------------------------------------------------------------------------------

const OFF_SQ_RING: u64 = 0;
const OFF_CQ_RING: u64 = 0x800_0000;
const OFF_SQES: u64 = 0x1000_0000;

const MAX_ENTRIES: u32 = 32768;
const MAX_CQ_ENTRIES: u32 = 2 * MAX_ENTRIES;

mod setup {
    pub const IOPOLL: u32 = 1 << 0;
    pub const SQPOLL: u32 = 1 << 1;
    pub const SQ_AFF: u32 = 1 << 2;
    pub const CQSIZE: u32 = 1 << 3;
    pub const CLAMP: u32 = 1 << 4;
    pub const ATTACH_WQ: u32 = 1 << 5;
    pub const R_DISABLED: u32 = 1 << 6;
    pub const SUBMIT_ALL: u32 = 1 << 7;
    pub const COOP_TASKRUN: u32 = 1 << 8;
    pub const TASKRUN_FLAG: u32 = 1 << 9;
    pub const SQE128: u32 = 1 << 10;
    pub const CQE32: u32 = 1 << 11;
    pub const SINGLE_ISSUER: u32 = 1 << 12;
    pub const DEFER_TASKRUN: u32 = 1 << 13;
    pub const NO_SQARRAY: u32 = 1 << 16;
}

mod feat {
    pub const SINGLE_MMAP: u32 = 1 << 0;
    pub const NODROP: u32 = 1 << 1;
    pub const SUBMIT_STABLE: u32 = 1 << 2;
    pub const RW_CUR_POS: u32 = 1 << 3;
    pub const CUR_PERSONALITY: u32 = 1 << 4;
    pub const FAST_POLL: u32 = 1 << 5;
    pub const POLL_32BITS: u32 = 1 << 6;
    pub const SQPOLL_NONFIXED: u32 = 1 << 7;
    pub const EXT_ARG: u32 = 1 << 8;
    pub const NATIVE_WORKERS: u32 = 1 << 9;
    pub const CQE_SKIP: u32 = 1 << 11;
    pub const LINKED_FILE: u32 = 1 << 12;
}

mod enter {
    pub const GETEVENTS: u64 = 1 << 0;
    pub const SQ_WAKEUP: u64 = 1 << 1;
    pub const SQ_WAIT: u64 = 1 << 2;
    pub const EXT_ARG: u64 = 1 << 3;
}

/// Drapeaux de l'anneau de soumission (`sq_flags`).
const SQ_NEED_WAKEUP: u32 = 1 << 0;
const SQ_CQ_OVERFLOW: u32 = 1 << 1;

/// Drapeaux d'une SQE.
mod sqe_f {
    pub const FIXED_FILE: u8 = 1 << 0;
    pub const IO_DRAIN: u8 = 1 << 1;
    pub const IO_LINK: u8 = 1 << 2;
    pub const IO_HARDLINK: u8 = 1 << 3;
    pub const ASYNC: u8 = 1 << 4;
    pub const BUFFER_SELECT: u8 = 1 << 5;
    pub const CQE_SKIP_SUCCESS: u8 = 1 << 6;
}

/// Drapeaux d'une CQE.
const CQE_F_BUFFER: u32 = 1 << 0;
const CQE_F_MORE: u32 = 1 << 1;
const CQE_BUFFER_SHIFT: u32 = 16;

mod op {
    pub const NOP: u8 = 0;
    pub const READV: u8 = 1;
    pub const WRITEV: u8 = 2;
    pub const FSYNC: u8 = 3;
    pub const READ_FIXED: u8 = 4;
    pub const WRITE_FIXED: u8 = 5;
    pub const POLL_ADD: u8 = 6;
    pub const POLL_REMOVE: u8 = 7;
    pub const SYNC_FILE_RANGE: u8 = 8;
    pub const SENDMSG: u8 = 9;
    pub const RECVMSG: u8 = 10;
    pub const TIMEOUT: u8 = 11;
    pub const TIMEOUT_REMOVE: u8 = 12;
    pub const ACCEPT: u8 = 13;
    pub const ASYNC_CANCEL: u8 = 14;
    pub const LINK_TIMEOUT: u8 = 15;
    pub const CONNECT: u8 = 16;
    pub const FALLOCATE: u8 = 17;
    pub const OPENAT: u8 = 18;
    pub const CLOSE: u8 = 19;
    pub const FILES_UPDATE: u8 = 20;
    pub const STATX: u8 = 21;
    pub const READ: u8 = 22;
    pub const WRITE: u8 = 23;
    pub const FADVISE: u8 = 24;
    pub const MADVISE: u8 = 25;
    pub const SEND: u8 = 26;
    pub const RECV: u8 = 27;
    pub const OPENAT2: u8 = 28;
    pub const EPOLL_CTL: u8 = 29;
    pub const PROVIDE_BUFFERS: u8 = 31;
    pub const REMOVE_BUFFERS: u8 = 32;
    pub const SHUTDOWN: u8 = 34;
    pub const RENAMEAT: u8 = 35;
    pub const UNLINKAT: u8 = 36;
    pub const MKDIRAT: u8 = 37;
    pub const MSG_RING: u8 = 40;
    pub const SOCKET: u8 = 45;
    pub const FTRUNCATE: u8 = 55;
    pub const BIND: u8 = 56;
    pub const LISTEN: u8 = 57;
    /// Nombre d'opérations connues de l'ABI (`IORING_OP_LAST`).
    pub const LAST: u8 = 58;
}

/// Opérations exécutées (rapportées par `IORING_REGISTER_PROBE`).
fn supported(o: u8) -> bool {
    matches!(o, 0..=29 | 31 | 32 | 34..=37 | 40 | 45 | 55..=57)
}

const POLL_ADD_MULTI: u32 = 1 << 0;
const ACCEPT_MULTISHOT: u16 = 1 << 0;
const RECV_MULTISHOT: u16 = 1 << 1;
const TIMEOUT_ABS: u32 = 1 << 0;
const TIMEOUT_UPDATE: u32 = 1 << 1;
const TIMEOUT_REALTIME: u32 = 1 << 3;
const TIMEOUT_ETIME_SUCCESS: u32 = 1 << 5;
const TIMEOUT_MULTISHOT: u32 = 1 << 6;
const LINK_TIMEOUT_UPDATE: u32 = 1 << 4;
const CANCEL_ALL: u32 = 1 << 0;
const CANCEL_FD: u32 = 1 << 1;
const CANCEL_ANY: u32 = 1 << 2;
const RWF_NOWAIT: u32 = 8;
const MSG_DONTWAIT: u32 = 0x40;

// Disposition de l'anneau commun : têtes et queues chacune sur sa ligne de cache.
const SQ_HEAD: usize = 0;
const SQ_TAIL: usize = 64;
const CQ_HEAD: usize = 128;
const CQ_TAIL: usize = 192;
const SQ_MASK: usize = 256;
const SQ_ENTRIES: usize = 260;
const SQ_FLAGS: usize = 264;
const SQ_DROPPED: usize = 268;
const CQ_MASK: usize = 272;
const CQ_ENTRIES: usize = 276;
const CQ_OVERFLOW: usize = 280;
const CQ_FLAGS: usize = 284;
const CQES: usize = 320;

/// SQE décodée (copiée à la soumission : `IORING_FEAT_SUBMIT_STABLE`).
#[derive(Clone, Copy, Default)]
struct Sqe {
    opcode: u8,
    flags: u8,
    ioprio: u16,
    fd: i32,
    off: u64,
    addr: u64,
    len: u32,
    op_flags: u32,
    user_data: u64,
    buf_index: u16,
    file_index: u32,
}

impl Sqe {
    fn decode(b: &[u8]) -> Sqe {
        let u16_at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        Sqe {
            opcode: b[0],
            flags: b[1],
            ioprio: u16_at(2),
            fd: u32_at(4) as i32,
            off: u64_at(8),
            addr: u64_at(16),
            len: u32_at(24),
            op_flags: u32_at(28),
            user_data: u64_at(32),
            buf_index: u16_at(40),
            file_index: u32_at(44),
        }
    }

    fn links(&self) -> bool {
        self.flags & (sqe_f::IO_LINK | sqe_f::IO_HARDLINK) != 0
    }

    /// L'opération porte sur le descripteur `fd`.
    fn uses_file(&self) -> bool {
        use op::*;
        matches!(
            self.opcode,
            READV | WRITEV | FSYNC | READ_FIXED | WRITE_FIXED | POLL_ADD | SYNC_FILE_RANGE | SENDMSG | RECVMSG | ACCEPT | CONNECT
                | FALLOCATE | READ | WRITE | FADVISE | SEND | RECV | SHUTDOWN | BIND | LISTEN
        )
    }
}

// --- Requêtes et travailleurs -----------------------------------------------------------------

/// Requête en cours.
struct Req {
    id: u64,
    sqe: Sqe,
    file: Option<Arc<OpenFile>>,
    /// Suite de la chaîne, lancée à la fin de la requête.
    rest: Mutex<VecDeque<Sqe>>,
    /// `LINK_TIMEOUT` attaché (la requête le désarme en finissant).
    link_timeout: Mutex<Option<Arc<Req>>>,
    /// Pour un `LINK_TIMEOUT` : la requête qu'il borne.
    lt_target: Mutex<Weak<Req>>,
    poll: Mutex<Option<Arc<PollWatch>>>,
    done: AtomicBool,
    canceled: AtomicBool,
    /// Travailleur qui l'exécute en bloquant (annulation).
    running: Mutex<Option<Arc<Thread>>>,
    /// Génération du minuteur (un minuteur périmé est ignoré).
    timer_gen: AtomicU64,
    /// Délais à plusieurs coups : coups restants (0 : sans fin).
    shots: AtomicU64,
    /// `CONNECT` : la connexion non bloquante a été lancée.
    connecting: AtomicBool,
    /// Tampon fourni pris pour la tentative en cours : `(groupe, adresse, longueur, id)`.
    pbuf: Mutex<Option<(u16, u64, u32, u16)>>,
}

enum Work {
    /// Exécution bloquante dans un travailleur.
    Run(Arc<Req>),
    /// Fichier peut-être prêt : reprise de la requête.
    Poll(Arc<Req>),
    /// Minuteur échu (génération).
    Timer(Arc<Req>, u64),
    /// Délai atteint par le nombre de complétions.
    Count(Arc<Req>),
    /// Chaînes retenues par `IO_DRAIN`.
    Chain(VecDeque<Sqe>),
}

/// Groupe de travailleurs d'un anneau (io-wq).
struct Pool {
    q: IrqSafeMutex<VecDeque<Work>>,
    wq: WaitQueue,
    idle: AtomicUsize,
    count: AtomicUsize,
    max: AtomicUsize,
    stop: AtomicBool,
    threads: Mutex<Vec<Arc<Thread>>>,
}

impl Pool {
    /// Dépose un travail (possible depuis une interruption : rien de bloquant).
    fn push(&self, w: Work) {
        self.q.lock().push_back(w);
        self.wq.wake_one();
    }
}

/// Observateur posé sur les files d'un fichier (sondage rapide, `POLL_ADD`).
struct PollWatch {
    req: Arc<Req>,
    pool: Arc<Pool>,
    file: Arc<OpenFile>,
    fired: AtomicBool,
}

impl Watcher for PollWatch {
    fn notify(self: Arc<Self>) {
        if !self.fired.swap(true, Ordering::AcqRel) {
            self.pool.push(Work::Poll(self.req.clone()));
        }
    }
}

/// Minuteur d'une requête (appelé depuis l'interruption du minuteur).
struct TimerWake {
    req: Arc<Req>,
    pool: Arc<Pool>,
    gen: u64,
}

impl Wake for TimerWake {
    fn wake(self: Arc<Self>) {
        if self.req.timer_gen.load(Ordering::Acquire) == self.gen && !self.req.done.load(Ordering::Acquire) {
            self.pool.push(Work::Timer(self.req.clone(), self.gen));
        }
    }
}

/// Fil de sondage de l'anneau de soumission (`SQPOLL`).
struct SqPoll {
    wq: WaitQueue,
    stop: AtomicBool,
    idle: Duration,
}

struct State {
    pending: BTreeMap<u64, Arc<Req>>,
    files: Vec<Option<Arc<OpenFile>>>,
    bufs: Vec<(u64, u64)>,
    pbufs: BTreeMap<u16, VecDeque<(u64, u32, u16)>>,
    eventfd: Option<Arc<OpenFile>>,
    count_timeouts: Vec<(u64, Arc<Req>)>,
    drained: VecDeque<VecDeque<Sqe>>,
    enabled: bool,
}

/// Anneau io_uring.
pub struct Ring {
    me: Weak<Ring>,
    setup: u32,
    sq_entries: u32,
    cq_entries: u32,
    cqe_size: usize,
    sqe_size: usize,
    array_off: usize,
    ring_obj: Arc<ShmObject>,
    sqe_obj: Arc<ShmObject>,
    ring_frames: Vec<usize>,
    sqe_frames: Vec<usize>,
    process: Arc<Process>,
    mm: Arc<Mm>,
    /// Complétions en débordement (anneau plein), rendues dès qu'il y a de la place.
    overflow: Mutex<VecDeque<(u64, i32, u32)>>,
    wq: WaitQueue,
    st: Mutex<State>,
    pool: Arc<Pool>,
    sq: Option<Arc<SqPoll>>,
    next_id: AtomicU64,
    inflight: AtomicUsize,
    cq_events: AtomicU64,
    ino: u64,
    /// Anneau créé par un appel i386 (`ctx->compat` de Linux) : ses requêtes lisent les
    /// structures de l'utilisateur au format i386 (`iovec`, `msghdr`), quel que soit le fil qui
    /// les sert (appelant, travailleur, fil de sondage).
    compat: bool,
}

fn res_of(r: SysResult) -> i32 {
    match r {
        Ok(v) => v as i32,
        Err(e) => -(e as i32),
    }
}

/// Code d'attente interrompue → résultat d'une requête.
fn fix_restart(res: i32, canceled: bool) -> i32 {
    if matches!(-(res as i64), crate::signal::ERESTARTSYS | crate::signal::ERESTARTNOINTR | crate::signal::ERESTARTNOHAND | crate::signal::ERESTART_RESTARTBLOCK) {
        -(if canceled { ECANCELED } else { EINTR } as i32)
    } else {
        res
    }
}

fn alloc_frames(n: usize) -> Result<Vec<usize>, i64> {
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        match frame_allocator::alloc_raw_frame_prefer_zeroed() {
            Some((pfn, zeroed)) => {
                if !zeroed {
                    // SAFETY : frame neuve, accessible par la physmap.
                    unsafe { core::ptr::write_bytes(phys_to_virt(pfn * PAGE_SIZE) as *mut u8, 0, PAGE_SIZE) };
                }
                v.push(pfn);
            }
            None => {
                // Rendues par l'objet qui les prend : ici, on les rend une à une.
                drop(ShmObject::from_frames(&v));
                return Err(ENOMEM);
            }
        }
    }
    Ok(v)
}

/// Mode d'émission d'une requête.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Tentative sans attente (soumission, reprise après sondage).
    Inline,
    /// Travailleur : l'opération peut bloquer.
    Blocking,
}

impl Ring {
    // --- Accès aux anneaux (physmap) ---

    fn ptr(frames: &[usize], off: usize) -> *mut u8 {
        (phys_to_virt(frames[off / PAGE_SIZE] * PAGE_SIZE) + off % PAGE_SIZE) as *mut u8
    }

    fn word(&self, off: usize) -> &AtomicU32 {
        // SAFETY : mot aligné de l'anneau (jamais à cheval sur deux pages), vivant tant que
        // l'objet l'est.
        unsafe { &*(Self::ptr(&self.ring_frames, off) as *const AtomicU32) }
    }

    fn sq_pending(&self) -> u32 {
        self.word(SQ_TAIL).load(Ordering::Acquire).wrapping_sub(self.word(SQ_HEAD).load(Ordering::Relaxed))
    }

    fn cq_ready(&self) -> u32 {
        self.word(CQ_TAIL).load(Ordering::Relaxed).wrapping_sub(self.word(CQ_HEAD).load(Ordering::Acquire))
    }

    fn sqe_at(&self, index: u32) -> Sqe {
        let off = index as usize * self.sqe_size;
        // SAFETY : SQE alignée sur sa taille (jamais à cheval sur deux pages).
        let b = unsafe { core::slice::from_raw_parts(Self::ptr(&self.sqe_frames, off), 64) };
        Sqe::decode(b)
    }

    fn cq_push(&self, user_data: u64, res: i32, flags: u32) -> bool {
        let head = self.word(CQ_HEAD).load(Ordering::Acquire);
        let tail = self.word(CQ_TAIL).load(Ordering::Relaxed);
        if tail.wrapping_sub(head) >= self.cq_entries {
            return false;
        }
        let off = CQES + (tail & (self.cq_entries - 1)) as usize * self.cqe_size;
        // SAFETY : CQE alignée sur sa taille, dans l'anneau.
        unsafe {
            let p = Self::ptr(&self.ring_frames, off);
            core::ptr::write_unaligned(p as *mut u64, user_data);
            core::ptr::write_unaligned(p.add(8) as *mut i32, res);
            core::ptr::write_unaligned(p.add(12) as *mut u32, flags);
            if self.cqe_size == 32 {
                core::ptr::write_bytes(p.add(16), 0, 16);
            }
        }
        self.word(CQ_TAIL).store(tail.wrapping_add(1), Ordering::Release);
        true
    }

    fn flush_overflow(&self, ov: &mut VecDeque<(u64, i32, u32)>) {
        while let Some(&(u, r, f)) = ov.front() {
            if !self.cq_push(u, r, f) {
                return;
            }
            ov.pop_front();
        }
        self.word(SQ_FLAGS).fetch_and(!SQ_CQ_OVERFLOW, Ordering::AcqRel);
    }

    /// Publie une complétion.
    fn post(&self, user_data: u64, res: i32, flags: u32) {
        {
            let mut ov = self.overflow.lock();
            if !ov.is_empty() {
                self.flush_overflow(&mut ov);
            }
            if !ov.is_empty() || !self.cq_push(user_data, res, flags) {
                // Sans perte (`IORING_FEAT_NODROP`) : gardée jusqu'à ce que l'anneau se vide.
                ov.push_back((user_data, res, flags));
                self.word(SQ_FLAGS).fetch_or(SQ_CQ_OVERFLOW, Ordering::AcqRel);
            }
        }
        let n = self.cq_events.fetch_add(1, Ordering::AcqRel) + 1;
        self.wq.wake_all();
        let (efd, due) = {
            let mut st = self.st.lock();
            let mut due = Vec::new();
            st.count_timeouts.retain(|(target, r)| {
                if *target <= n {
                    due.push(r.clone());
                    false
                } else {
                    true
                }
            });
            (st.eventfd.clone(), due)
        };
        if let Some(e) = efd.as_ref().and_then(|f| f.object_as::<crate::eventfd::EventFd>()) {
            e.signal(1);
        }
        for r in due {
            self.pool.push(Work::Count(r));
        }
    }

    // --- Travailleurs ---

    fn ensure_worker(&self, want_idle: bool) {
        let p = &self.pool;
        let count = p.count.load(Ordering::Acquire);
        if count > 0 && (!want_idle || p.idle.load(Ordering::Acquire) > 0 || count >= p.max.load(Ordering::Relaxed)) {
            return;
        }
        p.count.fetch_add(1, Ordering::AcqRel);
        let t = crate::io_worker_thread(self.process.clone(), self.mm.clone());
        p.threads.lock().push(t.clone());
        let arg = (self.me.clone(), self.pool.clone(), t);
        let name = format!("iou-wrk-{}", self.process.pid);
        if spawn::new_task_builder(worker_main, arg).name(name).spawn().is_err() {
            p.count.fetch_sub(1, Ordering::AcqRel);
        }
    }

    fn punt(&self, req: &Arc<Req>) {
        self.pool.push(Work::Run(req.clone()));
        self.ensure_worker(true);
    }

    fn process_work(&self, t: &Arc<Thread>, w: Work) {
        match w {
            Work::Run(req) => {
                if req.done.load(Ordering::Acquire) {
                    return;
                }
                *req.running.lock() = Some(t.clone());
                let r = self.issue(t, &req, Mode::Blocking);
                *req.running.lock() = None;
                if let Some((res, fl)) = r {
                    let res = fix_restart(res, req.canceled.load(Ordering::Acquire));
                    self.complete(t, &req, res, fl);
                }
            }
            Work::Poll(req) => self.on_poll(t, &req),
            Work::Timer(req, gen) => self.on_timer(t, &req, gen),
            Work::Count(req) => {
                if req.sqe.opcode == op::TIMEOUT {
                    self.complete(t, &req, 0, 0);
                }
            }
            Work::Chain(c) => self.run_chain(t, c),
        }
    }

    // --- Requêtes ---

    fn resolve(&self, t: &Thread, sqe: &Sqe) -> Option<Arc<OpenFile>> {
        if !sqe.uses_file() {
            return None;
        }
        if sqe.flags & sqe_f::FIXED_FILE != 0 {
            return self.st.lock().files.get(sqe.fd as usize).cloned().flatten();
        }
        if sqe.fd < 0 {
            return None;
        }
        t.process.files.lock().get(sqe.fd as usize)
    }

    fn new_req(&self, t: &Thread, sqe: Sqe) -> Arc<Req> {
        let req = Arc::new(Req {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            sqe,
            file: self.resolve(t, &sqe),
            rest: Mutex::new(VecDeque::new()),
            link_timeout: Mutex::new(None),
            lt_target: Mutex::new(Weak::new()),
            poll: Mutex::new(None),
            done: AtomicBool::new(false),
            canceled: AtomicBool::new(false),
            running: Mutex::new(None),
            timer_gen: AtomicU64::new(0),
            shots: AtomicU64::new(0),
            connecting: AtomicBool::new(false),
            pbuf: Mutex::new(None),
        });
        self.st.lock().pending.insert(req.id, req.clone());
        req
    }

    /// Soumet `n` SQE ; rend le nombre consommé.
    fn submit(&self, t: &Thread, n: u32) -> u32 {
        let head = self.word(SQ_HEAD).load(Ordering::Relaxed);
        let n = n.min(self.sq_pending());
        let mask = self.sq_entries - 1;
        let mut sqes = Vec::with_capacity(n as usize);
        for i in 0..n {
            let slot = head.wrapping_add(i) & mask;
            let idx = if self.setup & setup::NO_SQARRAY != 0 {
                slot
            } else {
                self.word(self.array_off + slot as usize * 4).load(Ordering::Relaxed)
            };
            if idx >= self.sq_entries {
                self.word(SQ_DROPPED).fetch_add(1, Ordering::AcqRel);
                continue;
            }
            sqes.push(self.sqe_at(idx));
        }
        // Les SQE sont copiées : le programme peut réutiliser les cases.
        self.word(SQ_HEAD).store(head.wrapping_add(n), Ordering::Release);
        let mut chain = VecDeque::new();
        for s in sqes {
            let links = s.links();
            chain.push_back(s);
            if !links {
                self.start_chain(t, core::mem::take(&mut chain));
            }
        }
        if !chain.is_empty() {
            self.start_chain(t, chain);
        }
        n
    }

    fn start_chain(&self, t: &Thread, chain: VecDeque<Sqe>) {
        let len = chain.iter().filter(|s| s.opcode != op::LINK_TIMEOUT).count();
        let drain = chain.front().map_or(false, |s| s.flags & sqe_f::IO_DRAIN != 0);
        let before = self.inflight.fetch_add(len, Ordering::AcqRel);
        if drain && before > 0 {
            // `IO_DRAIN` : après toutes les requêtes déjà en vol.
            self.st.lock().drained.push_back(chain);
            return;
        }
        self.run_chain(t, chain);
    }

    fn run_chain(&self, t: &Thread, mut chain: VecDeque<Sqe>) {
        while let Some(sqe) = chain.pop_front() {
            let lt = if chain.front().map_or(false, |n| n.opcode == op::LINK_TIMEOUT) { chain.pop_front() } else { None };
            let req = self.new_req(t, sqe);
            if let Some(lts) = lt {
                let l = self.new_req(t, lts);
                *l.lt_target.lock() = Arc::downgrade(&req);
                match self.read_timeout(t, &lts) {
                    Ok(d) => {
                        self.arm_timer(&l, d);
                        *req.link_timeout.lock() = Some(l);
                    }
                    Err(e) => {
                        self.finish(&l, -(e as i32), 0);
                    }
                }
            }
            *req.rest.lock() = core::mem::take(&mut chain);
            let r = if !self.st.lock().enabled {
                Some((-(EBADFD as i32), 0))
            } else if sqe.flags & sqe_f::ASYNC != 0 {
                self.punt(&req);
                None
            } else {
                self.issue(t, &req, Mode::Inline)
            };
            let Some((res, fl)) = r else { return };
            let ok = self.finish(&req, res, fl);
            chain = core::mem::take(&mut *req.rest.lock());
            match ok {
                Some(true) => {}
                Some(false) => {
                    self.cancel_chain(chain);
                    return;
                }
                None => return,
            }
        }
    }

    fn cancel_chain(&self, chain: VecDeque<Sqe>) {
        for s in chain {
            self.post(s.user_data, -(ECANCELED as i32), 0);
            if s.opcode != op::LINK_TIMEOUT {
                self.inflight_done();
            }
        }
    }

    fn inflight_done(&self) {
        if self.inflight.fetch_sub(1, Ordering::AcqRel) == 1 {
            let drained = core::mem::take(&mut self.st.lock().drained);
            if !drained.is_empty() {
                for c in drained {
                    self.pool.push(Work::Chain(c));
                }
                self.ensure_worker(true);
            }
        }
    }

    /// Fin d'une requête : complétion, désarmement. `None` : déjà finie ailleurs ; sinon, la
    /// chaîne continue-t-elle ?
    fn finish(&self, req: &Arc<Req>, res: i32, flags: u32) -> Option<bool> {
        if req.done.swap(true, Ordering::AcqRel) {
            return None;
        }
        self.disarm(req);
        req.timer_gen.fetch_add(1, Ordering::AcqRel);
        if let Some(lt) = req.link_timeout.lock().take() {
            self.finish(&lt, -(ECANCELED as i32), 0);
        }
        if let Some((g, a, l, b)) = req.pbuf.lock().take() {
            // Tampon pris mais inutilisé (échec) : rendu au groupe.
            if res <= 0 {
                self.st.lock().pbufs.entry(g).or_default().push_front((a, l, b));
            }
        }
        let s = &req.sqe;
        if !(s.flags & sqe_f::CQE_SKIP_SUCCESS != 0 && res >= 0) {
            self.post(s.user_data, res, flags);
        }
        self.st.lock().pending.remove(&req.id);
        if s.opcode != op::LINK_TIMEOUT {
            self.inflight_done();
        }
        let short = matches!(s.opcode, op::READ | op::WRITE | op::READ_FIXED | op::WRITE_FIXED | op::SEND | op::RECV)
            && res >= 0
            && (res as u32) < s.len;
        let etime_ok = s.opcode == op::TIMEOUT && res == -(ETIME as i32) && s.op_flags & TIMEOUT_ETIME_SUCCESS != 0;
        Some((res >= 0 && !short) || s.flags & sqe_f::IO_HARDLINK != 0 || etime_ok)
    }

    /// Fin d'une requête asynchrone, avec la suite de sa chaîne.
    fn complete(&self, t: &Thread, req: &Arc<Req>, res: i32, flags: u32) {
        let Some(ok) = self.finish(req, res, flags) else { return };
        let rest = core::mem::take(&mut *req.rest.lock());
        if rest.is_empty() {
            return;
        }
        if ok {
            self.run_chain(t, rest);
        } else {
            self.cancel_chain(rest);
        }
    }

    // --- Sondage ---

    fn disarm(&self, req: &Arc<Req>) {
        if let Some(w) = req.poll.lock().take() {
            let dyn_w: Arc<dyn Watcher> = w.clone();
            for_each_queue(&w.file, &mut |q| q.unwatch(&dyn_w));
        }
    }

    /// Inscrit un observateur sur les files du fichier de la requête.
    fn watch(&self, req: &Arc<Req>) -> Option<Arc<PollWatch>> {
        let f = req.file.clone()?;
        let w = Arc::new(PollWatch { req: req.clone(), pool: self.pool.clone(), file: f.clone(), fired: AtomicBool::new(false) });
        *req.poll.lock() = Some(w.clone());
        for_each_queue(&f, &mut |q| q.watch(w.clone()));
        self.ensure_worker(false);
        Some(w)
    }

    fn arm_poll(&self, req: &Arc<Req>, events: u32) {
        let Some(w) = self.watch(req) else { return };
        // Un réveil arrivé avant l'inscription n'est pas perdu.
        if poll_file(&w.file, None) & (events | ev::ERR | ev::HUP) != 0 {
            w.notify();
        }
    }

    /// `EAGAIN` d'une tentative : sondage (fichier sondable) ou travailleur.
    fn again(&self, req: &Arc<Req>, events: u32, mode: Mode) -> Option<(i32, u32)> {
        let f = req.file.as_ref()?;
        if mode == Mode::Blocking {
            return Some((-(EAGAIN as i32), 0));
        }
        if matches!(f.node, Node::Pipe(_) | Node::Object(_)) {
            self.arm_poll(req, events);
        } else {
            self.punt(req);
        }
        None
    }

    fn on_poll(&self, t: &Thread, req: &Arc<Req>) {
        if req.done.load(Ordering::Acquire) {
            return;
        }
        let s = &req.sqe;
        if s.opcode == op::POLL_ADD && s.len & POLL_ADD_MULTI != 0 {
            // Plusieurs coups : l'observateur reste inscrit ; il est réarmé avant la lecture de
            // l'état, si bien qu'un réveil ultérieur redonne un tour (pas de réveil perdu, pas de
            // complétions en boucle tant que rien ne change).
            if let Some(w) = req.poll.lock().clone() {
                w.fired.store(false, Ordering::Release);
            }
            if let Some(f) = &req.file {
                let mask = poll_file(f, Some(t)) & (s.op_flags | ev::ERR | ev::HUP);
                if mask != 0 {
                    self.post(s.user_data, mask as i32, CQE_F_MORE);
                }
            }
            return;
        }
        self.disarm(req);
        if let Some(r) = self.issue(t, req, Mode::Inline) {
            self.complete(t, req, r.0, r.1);
        }
    }

    // --- Minuteurs ---

    fn read_timeout(&self, t: &Thread, s: &Sqe) -> Result<Duration, i64> {
        let mut b = [0u8; 16];
        t.mm().copy_from_user(&mut b, s.addr as usize).map_err(fault)?;
        let sec = i64::from_le_bytes(b[..8].try_into().unwrap());
        let nsec = i64::from_le_bytes(b[8..].try_into().unwrap());
        if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
            return Err(EINVAL);
        }
        let d = Duration::new(sec as u64, nsec as u32);
        if s.op_flags & TIMEOUT_ABS == 0 {
            return Ok(d);
        }
        let now = if s.op_flags & TIMEOUT_REALTIME != 0 {
            time::now::<time::WallTime>()
        } else {
            time::Instant::now().duration_since(time::Instant::ZERO)
        };
        Ok(d.saturating_sub(now))
    }

    fn arm_timer(&self, req: &Arc<Req>, d: Duration) {
        let gen = req.timer_gen.fetch_add(1, Ordering::AcqRel) + 1;
        let w = Arc::new(TimerWake { req: req.clone(), pool: self.pool.clone(), gen });
        self.ensure_worker(false);
        sleep::future::sleep(d, core::task::Waker::from(w));
    }

    fn on_timer(&self, t: &Thread, req: &Arc<Req>, gen: u64) {
        if req.timer_gen.load(Ordering::Acquire) != gen || req.done.load(Ordering::Acquire) {
            return;
        }
        match req.sqe.opcode {
            op::LINK_TIMEOUT => {
                let target = req.lt_target.lock().upgrade();
                if let Some(target) = target {
                    // Le délai l'emporte : il finit (`-ETIME`) puis annule sa requête.
                    target.link_timeout.lock().take();
                    self.finish(req, -(ETIME as i32), 0);
                    self.cancel_req(t, &target);
                } else {
                    self.finish(req, -(ETIME as i32), 0);
                }
            }
            op::TIMEOUT if req.sqe.op_flags & TIMEOUT_MULTISHOT != 0 => {
                let left = req.shots.load(Ordering::Acquire);
                if left == 1 {
                    self.complete(t, req, -(ETIME as i32), 0);
                    return;
                }
                if left > 1 {
                    req.shots.fetch_sub(1, Ordering::AcqRel);
                }
                self.post(req.sqe.user_data, -(ETIME as i32), CQE_F_MORE);
                match self.read_timeout(t, &req.sqe) {
                    Ok(d) => self.arm_timer(req, d),
                    Err(e) => self.complete(t, req, -(e as i32), 0),
                }
            }
            _ => self.complete(t, req, -(ETIME as i32), 0),
        }
    }

    // --- Annulation ---

    /// Annule une requête en vol : `0` (annulée), `-EALREADY` (en cours dans un travailleur).
    fn cancel_req(&self, t: &Thread, target: &Arc<Req>) -> i32 {
        target.canceled.store(true, Ordering::Release);
        let running = target.running.lock().clone();
        if let Some(w) = running {
            w.kick(crate::WORK_EXIT);
            return -(EALREADY as i32);
        }
        self.complete(t, target, -(ECANCELED as i32), 0);
        0
    }

    fn find(&self, pred: impl Fn(&Req) -> bool) -> Vec<Arc<Req>> {
        self.st.lock().pending.values().filter(|r| !r.done.load(Ordering::Acquire) && pred(r)).cloned().collect()
    }

    // --- Tampons fournis ---

    /// Tampon de destination : celui de la SQE, ou un tampon fourni (`BUFFER_SELECT`).
    fn select_buf(&self, req: &Arc<Req>) -> Result<(u64, u64, u32), i64> {
        let s = &req.sqe;
        if s.flags & sqe_f::BUFFER_SELECT == 0 {
            return Ok((s.addr, s.len as u64, 0));
        }
        let mut pb = req.pbuf.lock();
        if pb.is_none() {
            let b = self.st.lock().pbufs.get_mut(&s.buf_index).and_then(|q| q.pop_front()).ok_or(ENOBUFS)?;
            *pb = Some((s.buf_index, b.0, b.1, b.2));
        }
        let (_, a, l, bid) = pb.unwrap();
        let len = if s.len == 0 { l } else { s.len.min(l) };
        Ok((a, len as u64, CQE_F_BUFFER | (bid as u32) << CQE_BUFFER_SHIFT))
    }

    /// Le tampon fourni est consommé (opération réussie).
    fn used_buf(&self, req: &Arc<Req>) {
        req.pbuf.lock().take();
    }

    // --- Émission ---

    /// Exécute la requête ; `None` : en attente (sondage, minuteur, travailleur).
    fn issue(&self, t: &Thread, req: &Arc<Req>, mode: Mode) -> Option<(i32, u32)> {
        // Disposition des structures de l'anneau, pas celle de l'appel en cours ; les délais
        // d'io_uring restent des `__kernel_timespec` de 64 bits (`time32` faux).
        let prev = t.in_compat.swap(self.compat, Ordering::Relaxed);
        let prev32 = t.time32.swap(false, Ordering::Relaxed);
        let r = self.issue_as(t, req, mode);
        t.in_compat.store(prev, Ordering::Relaxed);
        t.time32.store(prev32, Ordering::Relaxed);
        r
    }

    fn issue_as(&self, t: &Thread, req: &Arc<Req>, mode: Mode) -> Option<(i32, u32)> {
        let s = req.sqe;
        if s.uses_file() && req.file.is_none() {
            return Some((-(EBADF as i32), 0));
        }
        let inline = mode == Mode::Inline;
        match s.opcode {
            op::NOP => Some((0, 0)),
            op::READ | op::WRITE | op::READV | op::WRITEV | op::READ_FIXED | op::WRITE_FIXED => self.rw(t, req, mode),
            op::FSYNC | op::SYNC_FILE_RANGE | op::FALLOCATE | op::FADVISE | op::MADVISE => Some((0, 0)),
            op::POLL_ADD => {
                let f = req.file.as_ref()?;
                let events = s.op_flags | ev::ERR | ev::HUP;
                let mask = poll_file(f, Some(t)) & events;
                let multi = s.len & POLL_ADD_MULTI != 0;
                if mask != 0 && !multi {
                    return Some((mask as i32, 0));
                }
                if !multi {
                    self.arm_poll(req, events);
                    return None;
                }
                // Plusieurs coups : inscrit d'abord, puis l'état courant.
                if req.poll.lock().is_none() {
                    self.watch(req);
                }
                if mask != 0 {
                    self.post(s.user_data, mask as i32, CQE_F_MORE);
                }
                None
            }
            op::POLL_REMOVE => {
                let ud = s.addr;
                let found = self.find(|r| r.sqe.user_data == ud && r.sqe.opcode == op::POLL_ADD && r.id != req.id);
                let Some(target) = found.first() else { return Some((-(ENOENT as i32), 0)) };
                self.complete(t, target, -(ECANCELED as i32), 0);
                Some((0, 0))
            }
            op::TIMEOUT => {
                if s.len != 1 {
                    return Some((-(EINVAL as i32), 0));
                }
                let d = match self.read_timeout(t, &s) {
                    Ok(d) => d,
                    Err(e) => return Some((-(e as i32), 0)),
                };
                if s.op_flags & TIMEOUT_MULTISHOT != 0 {
                    req.shots.store(s.off, Ordering::Release);
                } else if s.off > 0 {
                    let target = self.cq_events.load(Ordering::Acquire) + s.off;
                    self.st.lock().count_timeouts.push((target, req.clone()));
                }
                self.arm_timer(req, d);
                None
            }
            op::TIMEOUT_REMOVE => {
                let ud = s.addr;
                let is_lt = s.op_flags & LINK_TIMEOUT_UPDATE != 0;
                let kind = if is_lt { op::LINK_TIMEOUT } else { op::TIMEOUT };
                let found = self.find(|r| r.sqe.user_data == ud && r.sqe.opcode == kind);
                let Some(target) = found.first() else { return Some((-(ENOENT as i32), 0)) };
                if s.op_flags & (TIMEOUT_UPDATE | LINK_TIMEOUT_UPDATE) != 0 {
                    let mut ns = target.sqe;
                    ns.addr = s.off;
                    ns.op_flags = (ns.op_flags & !TIMEOUT_ABS) | (s.op_flags & TIMEOUT_ABS);
                    return match self.read_timeout(t, &ns) {
                        Ok(d) => {
                            self.arm_timer(target, d);
                            Some((0, 0))
                        }
                        Err(e) => Some((-(e as i32), 0)),
                    };
                }
                self.st.lock().count_timeouts.retain(|(_, r)| r.id != target.id);
                self.complete(t, target, -(ECANCELED as i32), 0);
                Some((0, 0))
            }
            op::ASYNC_CANCEL => {
                let flags = s.op_flags;
                let me = req.id;
                let targets = self.find(|r| {
                    r.id != me
                        && r.sqe.opcode != op::LINK_TIMEOUT
                        && if flags & CANCEL_ANY != 0 {
                            true
                        } else if flags & CANCEL_FD != 0 {
                            r.sqe.fd == s.fd
                        } else {
                            r.sqe.user_data == s.addr
                        }
                });
                if targets.is_empty() {
                    return Some((-(ENOENT as i32), 0));
                }
                let all = flags & (CANCEL_ALL | CANCEL_ANY) != 0;
                let mut n = 0;
                let mut last = 0;
                for target in targets.iter().take(if all { usize::MAX } else { 1 }) {
                    last = self.cancel_req(t, target);
                    n += 1;
                }
                Some((if all { n } else { last }, 0))
            }
            op::LINK_TIMEOUT => Some((-(EINVAL as i32), 0)),
            op::ACCEPT | op::CONNECT | op::SEND | op::RECV | op::SENDMSG | op::RECVMSG | op::SHUTDOWN | op::BIND | op::LISTEN => {
                self.sock_op(t, req, mode)
            }
            op::SOCKET => Some((res_of(crate::socket::socket(t, s.fd as u64, s.off, s.len as u64)), 0)),
            op::OPENAT | op::OPENAT2 => {
                let (flags, mode_bits) = if s.opcode == op::OPENAT2 {
                    let mut how = [0u8; 24];
                    if s.len < 24 {
                        return Some((-(EINVAL as i32), 0));
                    }
                    if t.mm().copy_from_user(&mut how, s.off as usize).is_err() {
                        return Some((-(EFAULT as i32), 0));
                    }
                    (u64::from_le_bytes(how[..8].try_into().unwrap()), u64::from_le_bytes(how[8..16].try_into().unwrap()))
                } else {
                    (s.op_flags as u64, s.len as u64)
                };
                let r = crate::linux::fs::openat(t, s.fd as i64, s.addr, flags, mode_bits);
                Some((self.install_fixed(t, &s, r), 0))
            }
            op::CLOSE => {
                if s.file_index != 0 {
                    let mut st = self.st.lock();
                    let slot = st.files.get_mut(s.file_index as usize - 1);
                    return Some(match slot {
                        Some(x) if x.is_some() => {
                            *x = None;
                            (0, 0)
                        }
                        _ => (-(EBADF as i32), 0),
                    });
                }
                Some((res_of(crate::linux::fs::close(t, s.fd as u64)), 0))
            }
            op::FILES_UPDATE => Some((self.files_update(t, s.off as u32, s.addr, s.len), 0)),
            op::STATX => Some((res_of(crate::linux::fs::statx(t, s.fd as i64, s.addr, s.op_flags as u64, s.len as u64, s.off)), 0)),
            op::EPOLL_CTL => Some((res_of(crate::epoll::epoll_ctl(t, s.fd as u64, s.len as u64, s.off, s.addr)), 0)),
            op::PROVIDE_BUFFERS => {
                let n = s.fd.max(0) as u32;
                if n == 0 || s.len == 0 {
                    return Some((-(EINVAL as i32), 0));
                }
                let mut st = self.st.lock();
                let q = st.pbufs.entry(s.buf_index).or_default();
                for i in 0..n {
                    q.push_back((s.addr + i as u64 * s.len as u64, s.len, (s.off as u32 + i) as u16));
                }
                Some((0, 0))
            }
            op::REMOVE_BUFFERS => {
                let n = s.fd.max(0) as usize;
                let mut st = self.st.lock();
                let Some(q) = st.pbufs.get_mut(&s.buf_index) else { return Some((-(ENOENT as i32), 0)) };
                let k = n.min(q.len());
                q.drain(..k);
                Some((k as i32, 0))
            }
            op::RENAMEAT => Some((res_of(crate::linux::fs::renameat(t, s.fd as i64, s.addr, s.len as i32 as i64, s.off)), 0)),
            op::UNLINKAT => Some((res_of(crate::linux::fs::unlinkat(t, s.fd as i64, s.addr, s.op_flags as u64)), 0)),
            op::MKDIRAT => Some((res_of(crate::linux::fs::mkdirat(t, s.fd as i64, s.addr, s.len as u64)), 0)),
            op::FTRUNCATE => Some((res_of(crate::linux::fs::ftruncate(t, s.fd as u64, s.off as i64)), 0)),
            op::MSG_RING => {
                let target = t.process.files.lock().get(s.fd as usize).and_then(|f| f.object().cloned());
                match target.as_ref().and_then(downcast_arc::<Ring>) {
                    Some(r) if s.addr == 0 => {
                        r.post(s.off, s.len as i32, 0);
                        Some((0, 0))
                    }
                    Some(_) => Some((-(EINVAL as i32), 0)),
                    None => Some((-(EBADFD as i32), 0)),
                }
            }
            _ => {
                let _ = inline;
                Some((-(EINVAL as i32), 0))
            }
        }
    }

    /// `OPENAT` vers un fichier enregistré (`file_index`) : le descripteur passe dans la table.
    fn install_fixed(&self, t: &Thread, s: &Sqe, r: SysResult) -> i32 {
        let fd = match r {
            Ok(fd) => fd,
            Err(e) => return -(e as i32),
        };
        if s.file_index == 0 {
            return fd as i32;
        }
        let f = t.process.files.lock().get(fd as usize);
        let _ = crate::linux::fs::close(t, fd as u64);
        let Some(f) = f else { return -(EBADF as i32) };
        let mut st = self.st.lock();
        let slot = if s.file_index == u32::MAX {
            match st.files.iter().position(|x| x.is_none()) {
                Some(i) => i,
                None => return -(ENFILE_ as i32),
            }
        } else {
            s.file_index as usize - 1
        };
        match st.files.get_mut(slot) {
            Some(x) => {
                *x = Some(f);
                if s.file_index == u32::MAX { slot as i32 } else { 0 }
            }
            None => -(EINVAL as i32),
        }
    }

    fn files_update(&self, t: &Thread, off: u32, fds: u64, n: u32) -> i32 {
        let mut raw = vec![0u8; n as usize * 4];
        if t.mm().copy_from_user(&mut raw, fds as usize).is_err() {
            return -(EFAULT as i32);
        }
        let table = t.process.files.lock();
        let mut st = self.st.lock();
        let mut done = 0;
        for (i, c) in raw.chunks_exact(4).enumerate() {
            let fd = i32::from_le_bytes(c.try_into().unwrap());
            let Some(slot) = st.files.get_mut(off as usize + i) else { break };
            *slot = if fd < 0 { None } else { table.get(fd as usize) };
            done += 1;
        }
        done
    }

    fn rw(&self, t: &Thread, req: &Arc<Req>, mode: Mode) -> Option<(i32, u32)> {
        let s = req.sqe;
        let f = req.file.clone()?;
        let write = matches!(s.opcode, op::WRITE | op::WRITEV | op::WRITE_FIXED);
        let iov: Vec<(u64, u64)>;
        let mut cflags = 0;
        match s.opcode {
            op::READV | op::WRITEV => {
                if s.flags & sqe_f::BUFFER_SELECT != 0 {
                    return Some((-(EINVAL as i32), 0));
                }
                iov = match crate::linux::fs::iovecs(t, s.addr, s.len as u64) {
                    Ok(v) => v,
                    Err(e) => return Some((-(e as i32), 0)),
                };
            }
            op::READ_FIXED | op::WRITE_FIXED => {
                let ok = self.st.lock().bufs.get(s.buf_index as usize).map_or(false, |&(b, l)| s.addr >= b && s.addr + s.len as u64 <= b + l);
                if !ok {
                    return Some((-(EFAULT as i32), 0));
                }
                iov = vec![(s.addr, s.len as u64)];
            }
            _ => {
                let (a, l, fl) = match self.select_buf(req) {
                    Ok(x) => x,
                    Err(e) => return Some((-(e as i32), 0)),
                };
                cflags = fl;
                iov = vec![(a, l)];
            }
        }
        let pollable = matches!(f.node, Node::Pipe(_) | Node::Object(_));
        // Flux (tube, socket, terminal) : pas de position, l'offset est ignoré (`FMODE_STREAM`).
        let stream = pollable || matches!(f.node, Node::Console { .. });
        let at = if s.off == u64::MAX || stream { None } else { Some(s.off as usize) };
        let nowait_file = f.nonblock() || s.op_flags & RWF_NOWAIT != 0;
        if mode == Mode::Inline && !pollable && !matches!(f.node, Node::File { .. } | Node::Generated { .. } | Node::Null | Node::Zero | Node::Random) {
            // Terminal : lecture bloquante, dans un travailleur.
            self.punt(req);
            return None;
        }
        let r = crate::linux::fs::rw_iov(t, &f, write, &iov, at, mode == Mode::Inline || nowait_file);
        match r {
            Err(EAGAIN) if !nowait_file => self.again(req, if write { ev::OUT } else { ev::IN }, mode),
            r => {
                let res = res_of(r);
                if res > 0 {
                    self.used_buf(req);
                    Some((res, cflags))
                } else {
                    Some((res, 0))
                }
            }
        }
    }

    fn sock_op(&self, t: &Thread, req: &Arc<Req>, mode: Mode) -> Option<(i32, u32)> {
        let s = req.sqe;
        let f = req.file.clone()?;
        let Some(obj) = f.object().cloned() else { return Some((-(ENOTSOCK as i32), 0)) };
        let Some(sock) = obj.as_socket() else { return Some((-(ENOTSOCK as i32), 0)) };
        let inline = mode == Mode::Inline;
        let dontwait = if inline { MSG_DONTWAIT } else { 0 };
        let user_nb = f.nonblock() || s.op_flags & MSG_DONTWAIT != 0;
        let r: SysResult = match s.opcode {
            op::SEND => crate::socket::send_on(t, &f, sock, s.addr, s.len as u64, s.op_flags | dontwait),
            op::SENDMSG => crate::socket::sendmsg_one(t, &f, sock, s.addr, s.op_flags | dontwait),
            op::RECVMSG => crate::socket::recvmsg_one(t, &f, sock, s.addr, s.op_flags | dontwait),
            op::RECV => {
                let (a, l, fl) = match self.select_buf(req) {
                    Ok(x) => x,
                    Err(e) => return Some((-(e as i32), 0)),
                };
                let r = crate::socket::recv_on(t, &f, sock, a, l, s.op_flags | dontwait);
                let multi = s.ioprio & RECV_MULTISHOT != 0 && s.flags & sqe_f::BUFFER_SELECT != 0;
                match r {
                    Ok(n) if n > 0 => {
                        self.used_buf(req);
                        if multi {
                            // Plusieurs coups : une complétion par réception, la requête reste.
                            self.post(s.user_data, n as i32, fl | CQE_F_MORE);
                            return self.again(req, ev::IN, mode);
                        }
                        return Some((n as i32, fl));
                    }
                    r => r,
                }
            }
            op::ACCEPT => {
                let r = crate::socket::accept_on(t, &obj, s.addr, s.off, s.op_flags as u64, inline || f.nonblock());
                if s.ioprio & ACCEPT_MULTISHOT != 0 {
                    if let Ok(fd) = r {
                        self.post(s.user_data, fd as i32, CQE_F_MORE);
                        return self.again(req, ev::IN, mode);
                    }
                }
                r
            }
            op::CONNECT => {
                if req.connecting.load(Ordering::Acquire) {
                    // Issue d'une connexion non bloquante lancée plus tôt.
                    let e = sock.take_error();
                    if e != 0 {
                        Err(e)
                    } else if sock.peername().is_ok() {
                        Ok(0)
                    } else {
                        Err(EAGAIN)
                    }
                } else {
                    let addr = match crate::socket::read_addr(t, s.addr, s.off) {
                        Ok(a) => a,
                        Err(e) => return Some((-(e as i32), 0)),
                    };
                    match sock.connect(t, &addr, inline || f.nonblock()) {
                        Err(EINPROGRESS) if !f.nonblock() => {
                            req.connecting.store(true, Ordering::Release);
                            Err(EAGAIN)
                        }
                        Err(EAGAIN) if inline => {
                            self.punt(req);
                            return None;
                        }
                        r => r.map(|_| 0),
                    }
                }
            }
            op::SHUTDOWN => sock.shutdown(s.len as i32).map(|_| 0),
            op::BIND => match crate::socket::read_addr(t, s.addr, s.off) {
                Ok(a) => sock.bind(t, &a).map(|_| 0),
                Err(e) => Err(e),
            },
            op::LISTEN => sock.listen(t, s.len as i32).map(|_| 0),
            _ => Err(EINVAL),
        };
        match r {
            Err(EAGAIN) if !user_nb => {
                let events = match s.opcode {
                    op::SEND | op::SENDMSG | op::CONNECT => ev::OUT,
                    _ => ev::IN,
                };
                if mode == Mode::Blocking && !matches!(s.opcode, op::CONNECT) {
                    return Some((-(EAGAIN as i32), 0));
                }
                self.arm_poll(req, events);
                None
            }
            r => Some((res_of(r), 0)),
        }
    }

    // --- Création ---

    fn new(t: &Thread, setup: u32, sq_entries: u32, cq_entries: u32, sq_idle_ms: u32) -> Result<Arc<Ring>, i64> {
        let cqe_size = if setup & setup::CQE32 != 0 { 32 } else { 16 };
        let sqe_size = if setup & setup::SQE128 != 0 { 128 } else { 64 };
        let array_off = (CQES + cq_entries as usize * cqe_size + 63) & !63;
        let ring_bytes = if setup & setup::NO_SQARRAY != 0 { array_off } else { array_off + sq_entries as usize * 4 };
        let ring_pages = ring_bytes.div_ceil(PAGE_SIZE);
        let sqe_pages = (sq_entries as usize * sqe_size).div_ceil(PAGE_SIZE);
        let ring_frames = alloc_frames(ring_pages)?;
        let sqe_frames = match alloc_frames(sqe_pages) {
            Ok(f) => f,
            Err(e) => {
                drop(ShmObject::from_frames(&ring_frames));
                return Err(e);
            }
        };
        let ring_obj = ShmObject::from_frames(&ring_frames);
        let sqe_obj = ShmObject::from_frames(&sqe_frames);
        let mm = t.process.mm().ok_or(EINVAL)?;
        let pool = Arc::new(Pool {
            q: IrqSafeMutex::new(VecDeque::new()),
            wq: WaitQueue::new(),
            idle: AtomicUsize::new(0),
            count: AtomicUsize::new(0),
            max: AtomicUsize::new(4 * cpu::cpu_count() as usize),
            stop: AtomicBool::new(false),
            threads: Mutex::new(Vec::new()),
        });
        let sq = (setup & setup::SQPOLL != 0).then(|| {
            Arc::new(SqPoll {
                wq: WaitQueue::new(),
                stop: AtomicBool::new(false),
                idle: Duration::from_millis(if sq_idle_ms == 0 { 1000 } else { sq_idle_ms as u64 }),
            })
        });
        let ring = Arc::new_cyclic(|me| Ring {
            me: me.clone(),
            setup,
            sq_entries,
            cq_entries,
            cqe_size,
            sqe_size,
            array_off,
            ring_obj,
            sqe_obj,
            ring_frames,
            sqe_frames,
            process: t.process.clone(),
            mm: mm.clone(),
            overflow: Mutex::new(VecDeque::new()),
            wq: WaitQueue::new(),
            st: Mutex::new(State {
                pending: BTreeMap::new(),
                files: Vec::new(),
                bufs: Vec::new(),
                pbufs: BTreeMap::new(),
                eventfd: None,
                count_timeouts: Vec::new(),
                drained: VecDeque::new(),
                enabled: setup & setup::R_DISABLED == 0,
            }),
            pool,
            sq,
            next_id: AtomicU64::new(1),
            inflight: AtomicUsize::new(0),
            cq_events: AtomicU64::new(0),
            ino: new_ino(),
            compat: crate::linux::compat::active(t),
        });
        ring.word(SQ_MASK).store(sq_entries - 1, Ordering::Relaxed);
        ring.word(SQ_ENTRIES).store(sq_entries, Ordering::Relaxed);
        ring.word(CQ_MASK).store(cq_entries - 1, Ordering::Relaxed);
        ring.word(CQ_ENTRIES).store(cq_entries, Ordering::Relaxed);
        if let Some(sq) = &ring.sq {
            let th = crate::io_worker_thread(t.process.clone(), mm);
            let arg = (ring.me.clone(), sq.clone(), th);
            let name = format!("iou-sqp-{}", t.process.pid);
            spawn::new_task_builder(sqpoll_main, arg).name(name).spawn().map_err(|_| ENOMEM)?;
        }
        Ok(ring)
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        self.pool.stop.store(true, Ordering::Release);
        self.pool.wq.wake_all();
        for th in self.pool.threads.lock().drain(..) {
            th.kick(crate::WORK_EXIT);
        }
        if let Some(sq) = &self.sq {
            sq.stop.store(true, Ordering::Release);
            sq.wq.wake_all();
        }
        let pending: Vec<Arc<Req>> = core::mem::take(&mut self.st.lock().pending).into_values().collect();
        for r in pending {
            if let Some(w) = r.poll.lock().take() {
                let dyn_w: Arc<dyn Watcher> = w.clone();
                for_each_queue(&w.file, &mut |q| q.unwatch(&dyn_w));
            }
            r.timer_gen.fetch_add(1, Ordering::AcqRel);
            r.rest.lock().clear();
            r.link_timeout.lock().take();
        }
    }
}

fn worker_main(arg: (Weak<Ring>, Arc<Pool>, Arc<Thread>)) {
    let (ring, pool, t) = arg;
    t.bind_current_task();
    loop {
        pool.idle.fetch_add(1, Ordering::AcqRel);
        let deadline = time::Instant::now() + Duration::from_secs(10);
        let w = wait_event(&[&pool.wq], Some(deadline), None, || {
            if let Some(w) = pool.q.lock().pop_front() {
                return Some(Some(w));
            }
            pool.stop.load(Ordering::Acquire).then_some(None)
        });
        pool.idle.fetch_sub(1, Ordering::AcqRel);
        let w = match w {
            Ok(Some(w)) => w,
            Ok(None) => break,
            // Inactif : on garde un travailleur, les autres s'en vont.
            Err(_) if pool.count.load(Ordering::Acquire) > 1 => break,
            Err(_) => continue,
        };
        let Some(r) = ring.upgrade() else { break };
        r.process_work(&t, w);
        drop(r);
        // Fin d'une annulation (`WORK_EXIT` posé pour interrompre l'attente).
        t.work.store(0, Ordering::Release);
    }
    pool.count.fetch_sub(1, Ordering::AcqRel);
    let me = Arc::as_ptr(&t);
    pool.threads.lock().retain(|x| Arc::as_ptr(x) != me);
}

fn sqpoll_main(arg: (Weak<Ring>, Arc<SqPoll>, Arc<Thread>)) {
    let (ring, sq, t) = arg;
    t.bind_current_task();
    let mut last = time::Instant::now();
    loop {
        if sq.stop.load(Ordering::Acquire) {
            break;
        }
        let Some(r) = ring.upgrade() else { break };
        let n = r.sq_pending();
        if n > 0 {
            r.submit(&t, n);
            t.work.store(0, Ordering::Release);
            last = time::Instant::now();
            drop(r);
            task::schedule();
            continue;
        }
        if last.elapsed() < sq.idle {
            drop(r);
            task::schedule();
            continue;
        }
        // Inactif : le programme devra réveiller le fil (`IORING_ENTER_SQ_WAKEUP`).
        r.word(SQ_FLAGS).fetch_or(SQ_NEED_WAKEUP, Ordering::AcqRel);
        if r.sq_pending() > 0 {
            r.word(SQ_FLAGS).fetch_and(!SQ_NEED_WAKEUP, Ordering::AcqRel);
            continue;
        }
        drop(r);
        let ring2 = ring.clone();
        let _ = wait_event(&[&sq.wq], None, None, || {
            (sq.stop.load(Ordering::Acquire) || ring2.upgrade().map_or(true, |r| r.sq_pending() > 0)).then_some(())
        });
        if let Some(r) = ring.upgrade() {
            r.word(SQ_FLAGS).fetch_and(!SQ_NEED_WAKEUP, Ordering::AcqRel);
        }
        last = time::Instant::now();
    }
}

impl FileObject for Ring {
    object_any!();

    fn link_name(&self) -> String {
        String::from("anon_inode:[io_uring]")
    }

    fn stat(&self) -> (u32, u64) {
        (0o600, self.ino)
    }

    fn poll(&self, _t: Option<&Thread>) -> u32 {
        let mut r = 0;
        if self.cq_ready() > 0 || !self.overflow.lock().is_empty() {
            r |= ev::READABLE;
        }
        if self.sq_pending() < self.sq_entries {
            r |= ev::WRITABLE;
        }
        r
    }

    fn queues<'a>(&'a self, cb: &mut dyn FnMut(&'a WaitQueue)) {
        cb(&self.wq);
    }

    fn mmap_backing(&self, off: u64) -> Option<Arc<ShmObject>> {
        match off {
            OFF_SQ_RING | OFF_CQ_RING => Some(self.ring_obj.clone()),
            OFF_SQES => Some(self.sqe_obj.clone()),
            _ => None,
        }
    }

    fn fdinfo(&self) -> String {
        let st = self.st.lock();
        format!(
            "SqMask:\t0x{:x}\nSqHead:\t{}\nSqTail:\t{}\nCqMask:\t0x{:x}\nCqHead:\t{}\nCqTail:\t{}\nSqThread:\t{}\nUserFiles:\t{}\nUserBufs:\t{}\nPending:\t{}\nWorkers:\t{}\n",
            self.sq_entries - 1,
            self.word(SQ_HEAD).load(Ordering::Relaxed),
            self.word(SQ_TAIL).load(Ordering::Relaxed),
            self.cq_entries - 1,
            self.word(CQ_HEAD).load(Ordering::Relaxed),
            self.word(CQ_TAIL).load(Ordering::Relaxed),
            if self.sq.is_some() { 1 } else { -1 },
            st.files.len(),
            st.bufs.len(),
            st.pending.len(),
            self.pool.count.load(Ordering::Relaxed)
        )
    }
}

/// Pas de `ENFILE` dans la table commune : valeur de Linux.
const ENFILE_: i64 = 23;

// --- Appels système ---------------------------------------------------------------------------

/// `io_uring_setup(entries, params)`.
pub fn io_uring_setup(t: &Thread, entries: u64, params: u64) -> SysResult {
    let mut p = [0u8; 120];
    t.mm().copy_from_user(&mut p, params as usize).map_err(fault)?;
    let u32_at = |p: &[u8; 120], o: usize| u32::from_le_bytes(p[o..o + 4].try_into().unwrap());
    let flags = u32_at(&p, 8);
    let known = setup::SQPOLL
        | setup::SQ_AFF
        | setup::CQSIZE
        | setup::CLAMP
        | setup::ATTACH_WQ
        | setup::R_DISABLED
        | setup::SUBMIT_ALL
        | setup::COOP_TASKRUN
        | setup::TASKRUN_FLAG
        | setup::SQE128
        | setup::CQE32
        | setup::SINGLE_ISSUER
        | setup::DEFER_TASKRUN
        | setup::NO_SQARRAY;
    if flags & !known != 0 || flags & setup::IOPOLL != 0 {
        return Err(EINVAL);
    }
    if flags & setup::DEFER_TASKRUN != 0 && flags & setup::SINGLE_ISSUER == 0 {
        return Err(EINVAL);
    }
    if p[28..40].iter().any(|&b| b != 0) {
        return Err(EINVAL);
    }
    let mut n = entries as u32;
    if n == 0 || entries > u32::MAX as u64 {
        return Err(EINVAL);
    }
    if n > MAX_ENTRIES {
        if flags & setup::CLAMP == 0 {
            return Err(EINVAL);
        }
        n = MAX_ENTRIES;
    }
    let sq = n.next_power_of_two();
    let cq = if flags & setup::CQSIZE != 0 {
        let mut c = u32_at(&p, 4);
        if c == 0 {
            return Err(EINVAL);
        }
        if c > MAX_CQ_ENTRIES {
            if flags & setup::CLAMP == 0 {
                return Err(EINVAL);
            }
            c = MAX_CQ_ENTRIES;
        }
        let c = c.next_power_of_two();
        if c < sq {
            return Err(EINVAL);
        }
        c
    } else {
        2 * sq
    };
    let ring = Ring::new(t, flags, sq, cq, u32_at(&p, 16))?;
    let array_off = if flags & setup::NO_SQARRAY != 0 { 0 } else { ring.array_off as u32 };
    let features = feat::SINGLE_MMAP
        | feat::NODROP
        | feat::SUBMIT_STABLE
        | feat::RW_CUR_POS
        | feat::CUR_PERSONALITY
        | feat::FAST_POLL
        | feat::POLL_32BITS
        | feat::SQPOLL_NONFIXED
        | feat::EXT_ARG
        | feat::NATIVE_WORKERS
        | feat::CQE_SKIP
        | feat::LINKED_FILE;
    let put = |p: &mut [u8; 120], o: usize, v: u32| p[o..o + 4].copy_from_slice(&v.to_le_bytes());
    put(&mut p, 0, sq);
    put(&mut p, 4, cq);
    put(&mut p, 20, features);
    let sq_off = [SQ_HEAD as u32, SQ_TAIL as u32, SQ_MASK as u32, SQ_ENTRIES as u32, SQ_FLAGS as u32, SQ_DROPPED as u32, array_off, 0];
    let cq_off = [CQ_HEAD as u32, CQ_TAIL as u32, CQ_MASK as u32, CQ_ENTRIES as u32, CQ_OVERFLOW as u32, CQES as u32, CQ_FLAGS as u32, 0];
    for (i, v) in sq_off.iter().enumerate() {
        put(&mut p, 40 + i * 4, *v);
    }
    p[72..80].fill(0);
    for (i, v) in cq_off.iter().enumerate() {
        put(&mut p, 80 + i * 4, *v);
    }
    p[112..120].fill(0);
    t.mm().copy_to_user(params as usize, &p).map_err(fault)?;
    let file = OpenFile::new(Node::Object(ring), O_RDWR);
    t.process.files.lock().alloc(file, 0, true).map(|fd| fd as i64).ok_or(EMFILE)
}

fn ring_of(t: &Thread, fd: u64) -> Result<Arc<Ring>, i64> {
    let f = crate::linux::fs::get_fd(t, fd)?;
    f.object().and_then(downcast_arc::<Ring>).ok_or(EOPNOTSUPP)
}

/// `io_uring_enter(fd, to_submit, min_complete, flags, arg, argsz)`.
pub fn io_uring_enter(t: &Thread, fd: u64, to_submit: u64, min_complete: u64, flags: u64, arg: u64, argsz: u64) -> SysResult {
    let ring = ring_of(t, fd)?;
    if flags & !(enter::GETEVENTS | enter::SQ_WAKEUP | enter::SQ_WAIT | enter::EXT_ARG) != 0 {
        return Err(EINVAL);
    }
    if !ring.st.lock().enabled {
        return Err(EBADFD);
    }
    let mut submitted = 0u32;
    if let Some(sq) = &ring.sq {
        if flags & enter::SQ_WAKEUP != 0 {
            sq.wq.wake_all();
        }
        if flags & enter::SQ_WAIT != 0 {
            let _ = wait_event(&[&ring.wq], Some(time::Instant::now() + Duration::from_millis(10)), Some(t), || {
                (ring.sq_pending() < ring.sq_entries).then_some(())
            });
        }
        submitted = to_submit.min(u32::MAX as u64) as u32;
    } else if to_submit > 0 {
        submitted = ring.submit(t, to_submit.min(u32::MAX as u64) as u32);
    }
    if flags & enter::GETEVENTS == 0 {
        return Ok(submitted as i64);
    }
    let (sigmask, ts) = if flags & enter::EXT_ARG != 0 {
        if argsz != 24 {
            return Err(EINVAL);
        }
        let mut a = [0u8; 24];
        if arg != 0 {
            t.mm().copy_from_user(&mut a, arg as usize).map_err(fault)?;
        }
        (u64::from_le_bytes(a[..8].try_into().unwrap()), u64::from_le_bytes(a[16..24].try_into().unwrap()))
    } else {
        (arg, 0)
    };
    let deadline = crate::linux::fs::read_ts(t, ts)?.map(|d| time::Instant::now() + d);
    let min = min_complete.min(ring.cq_entries as u64) as u32;
    let wait = || -> SysResult {
        let r = wait_event(&[&ring.wq], deadline, Some(t), || {
            {
                let mut ov = ring.overflow.lock();
                if !ov.is_empty() {
                    ring.flush_overflow(&mut ov);
                }
            }
            (ring.cq_ready() >= min).then_some(())
        });
        match r {
            Ok(()) => Ok(submitted as i64),
            Err(_) if submitted > 0 => Ok(submitted as i64),
            Err(WaitError::TimedOut) => Err(ETIME),
            Err(WaitError::Interrupted) => Err(EINTR),
        }
    };
    if sigmask != 0 && flags & enter::EXT_ARG == 0 && argsz != 8 {
        return Err(EINVAL);
    }
    if sigmask != 0 {
        crate::linux::fs::with_temp_mask(t, sigmask, wait)
    } else {
        wait()
    }
}

/// `io_uring_register(fd, opcode, arg, nr_args)`.
pub fn io_uring_register(t: &Thread, fd: u64, opcode: u64, arg: u64, nr: u64) -> SysResult {
    const REGISTER_BUFFERS: u64 = 0;
    const UNREGISTER_BUFFERS: u64 = 1;
    const REGISTER_FILES: u64 = 2;
    const UNREGISTER_FILES: u64 = 3;
    const REGISTER_EVENTFD: u64 = 4;
    const UNREGISTER_EVENTFD: u64 = 5;
    const REGISTER_FILES_UPDATE: u64 = 6;
    const REGISTER_EVENTFD_ASYNC: u64 = 7;
    const REGISTER_PROBE: u64 = 8;
    const REGISTER_PERSONALITY: u64 = 9;
    const UNREGISTER_PERSONALITY: u64 = 10;
    const REGISTER_RESTRICTIONS: u64 = 11;
    const REGISTER_ENABLE_RINGS: u64 = 12;
    const REGISTER_IOWQ_AFF: u64 = 17;
    const UNREGISTER_IOWQ_AFF: u64 = 18;
    const REGISTER_IOWQ_MAX_WORKERS: u64 = 19;
    let ring = ring_of(t, fd)?;
    let mm = t.mm();
    match opcode {
        REGISTER_BUFFERS => {
            if nr == 0 || nr > 16384 {
                return Err(EINVAL);
            }
            let iov = crate::linux::fs::iovecs(t, arg, nr.min(1024))?;
            let mut st = ring.st.lock();
            if !st.bufs.is_empty() {
                return Err(EBUSY);
            }
            st.bufs = iov;
            Ok(0)
        }
        UNREGISTER_BUFFERS => {
            let mut st = ring.st.lock();
            if st.bufs.is_empty() {
                return Err(ENXIO);
            }
            st.bufs.clear();
            Ok(0)
        }
        REGISTER_FILES => {
            if nr == 0 || nr > 65536 {
                return Err(EINVAL);
            }
            let mut raw = vec![0u8; nr as usize * 4];
            mm.copy_from_user(&mut raw, arg as usize).map_err(fault)?;
            let table = t.process.files.lock();
            let mut files = Vec::with_capacity(nr as usize);
            for c in raw.chunks_exact(4) {
                let fd = i32::from_le_bytes(c.try_into().unwrap());
                files.push(if fd < 0 { None } else { Some(table.get(fd as usize).ok_or(EBADF)?) });
            }
            drop(table);
            let mut st = ring.st.lock();
            if !st.files.is_empty() {
                return Err(EBUSY);
            }
            st.files = files;
            Ok(0)
        }
        UNREGISTER_FILES => {
            let mut st = ring.st.lock();
            if st.files.is_empty() {
                return Err(ENXIO);
            }
            st.files.clear();
            Ok(0)
        }
        REGISTER_FILES_UPDATE => {
            let mut u = [0u8; 16];
            mm.copy_from_user(&mut u, arg as usize).map_err(fault)?;
            let off = u32::from_le_bytes(u[..4].try_into().unwrap());
            let fds = u64::from_le_bytes(u[8..].try_into().unwrap());
            let n = ring.files_update(t, off, fds, nr as u32);
            if n < 0 { Err(-(n as i64)) } else { Ok(n as i64) }
        }
        REGISTER_EVENTFD | REGISTER_EVENTFD_ASYNC => {
            let efd = mm.read_user::<i32>(arg as usize).map_err(fault)?;
            let f = crate::linux::fs::get_fd(t, efd as u64)?;
            if f.object_as::<crate::eventfd::EventFd>().is_none() {
                return Err(EINVAL);
            }
            let mut st = ring.st.lock();
            if st.eventfd.is_some() {
                return Err(EBUSY);
            }
            st.eventfd = Some(f);
            Ok(0)
        }
        UNREGISTER_EVENTFD => ring.st.lock().eventfd.take().map(|_| 0).ok_or(ENXIO),
        REGISTER_PROBE => {
            let n = (nr as usize).min(256);
            let mut out = vec![0u8; 16 + n * 8];
            out[0] = op::LAST - 1;
            out[1] = n.min(op::LAST as usize) as u8;
            for i in 0..n.min(op::LAST as usize) {
                let o = 16 + i * 8;
                out[o] = i as u8;
                if supported(i as u8) {
                    out[o + 2] = 1; // IO_URING_OP_SUPPORTED
                }
            }
            mm.copy_to_user(arg as usize, &out).map_err(fault)?;
            Ok(0)
        }
        REGISTER_PERSONALITY => Ok(1),
        UNREGISTER_PERSONALITY | REGISTER_RESTRICTIONS | REGISTER_IOWQ_AFF | UNREGISTER_IOWQ_AFF => Ok(0),
        REGISTER_ENABLE_RINGS => {
            let mut st = ring.st.lock();
            if st.enabled {
                return Err(EBADFD);
            }
            st.enabled = true;
            Ok(0)
        }
        REGISTER_IOWQ_MAX_WORKERS => {
            if nr != 2 {
                return Err(EINVAL);
            }
            let v: [u32; 2] = mm.read_user(arg as usize).map_err(fault)?;
            let cur = ring.pool.max.load(Ordering::Relaxed) as u32;
            if v[0] != 0 || v[1] != 0 {
                ring.pool.max.store(v[0].max(v[1]).max(1) as usize, Ordering::Relaxed);
            }
            mm.write_user(arg as usize, &[cur, cur]).map_err(fault)?;
            Ok(0)
        }
        _ => Err(EINVAL),
    }
}
