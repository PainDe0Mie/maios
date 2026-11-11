//! Pseudo-terminaux : `/dev/ptmx` (maître) et `/dev/pts/<n>` (esclave), avec la discipline de
//! ligne **N_TTY** de Linux entre les deux.
//!
//! Le maître écrit l'*entrée* du terminal : chaque octet passe par la discipline (`c_iflag` :
//! `ICRNL`, `INLCR`, `IGNCR`, `ISTRIP`, `IXON` ; `c_lflag` : `ISIG` → `SIGINT`/`SIGQUIT`/
//! `SIGTSTP` au groupe de premier plan, `ICANON` → édition de ligne `VERASE`, `VWERASE`,
//! `VKILL`, `VEOF`, `VEOL`, `VREPRINT`, `VLNEXT`, écho `ECHO`/`ECHOE`/`ECHOK`/`ECHONL`/
//! `ECHOCTL`/`ECHOKE`). L'esclave lit des lignes entières en mode canonique, ou selon
//! `VMIN`/`VTIME` en mode brut. Ce que l'esclave écrit (et l'écho) passe par `c_oflag`
//! (`OPOST`, `ONLCR`, `OCRNL`, `ONOCR`, `ONLRET`) avant d'être lu par le maître.
//!
//! Contrôle des travaux : un terminal devient le terminal de contrôle d'une session
//! (`TIOCSCTTY`, ou ouverture sans `O_NOCTTY` par un chef de session qui n'en a pas) ; un
//! processus d'arrière-plan qui lit reçoit `SIGTTIN`, qui écrit avec `TOSTOP`, `SIGTTOU`
//! (`EIO` si le signal est ignoré ou bloqué, comme Linux). Raccrochage : la fermeture du maître
//! envoie `SIGHUP` puis `SIGCONT` au chef de session et au groupe de premier plan ; l'esclave
//! lit ensuite la fin de fichier et ses écritures échouent (`EIO`). La fermeture du dernier
//! esclave fait lire `EIO` au maître une fois sa file vide.
//!
//! Instances (`PtsFs`, `struct pts_fs_info` de Linux ≥ 4.7) : chaque montage de `devpts` est une
//! instance à part (`newinstance` implicite), avec ses numéros d'esclaves, son `ptmx`
//! (`/dev/pts/ptmx`, droits `ptmxmode`) et ses options (`uid`, `gid`, `mode`, `max`) ; une
//! liaison (`--dev-bind`) partage l'instance. `/dev/ptmx` (5:2) ouvre l'instance montée sur
//! `pts` dans son propre dossier (`devpts_acquire`). Les esclaves d'un conteneur (bubblewrap
//! `--dev`) ne se voient donc pas depuis l'hôte, et réciproquement.

use alloc::{collections::BTreeMap, collections::VecDeque, format, string::String, sync::Arc, sync::Weak, vec::Vec};
use sync_preemption::PreemptionSafeMutex as Mutex;

use crate::linux::errno::*;
use crate::linux::{fault, SysResult};
use crate::object::{ev, object_any, FileObject};
use crate::signal::{self, SigInfo};
use crate::wait::{wait_event, WaitError, WaitQueue};
use crate::{Process, Thread};

// --- termios ------------------------------------------------------------------------------------

const NCCS: usize = 19;
const VINTR: usize = 0;
const VQUIT: usize = 1;
const VERASE: usize = 2;
const VKILL: usize = 3;
const VEOF: usize = 4;
const VTIME: usize = 5;
const VMIN: usize = 6;
const VSTART: usize = 8;
const VSTOP: usize = 9;
const VSUSP: usize = 10;
const VEOL: usize = 11;
const VREPRINT: usize = 12;
const VWERASE: usize = 14;
const VLNEXT: usize = 15;
const VEOL2: usize = 16;

// c_iflag
const ISTRIP: u32 = 0o40;
const INLCR: u32 = 0o100;
const IGNCR: u32 = 0o200;
const ICRNL: u32 = 0o400;
const IXON: u32 = 0o2000;
const IXANY: u32 = 0o4000;
const IUTF8: u32 = 0o40000;
// c_oflag
const OPOST: u32 = 0o1;
const ONLCR: u32 = 0o4;
const OCRNL: u32 = 0o10;
const ONOCR: u32 = 0o20;
const ONLRET: u32 = 0o40;
// c_cflag
const B38400: u32 = 0o17;
const CS8: u32 = 0o60;
const CREAD: u32 = 0o200;
const HUPCL: u32 = 0o2000;
// c_lflag
const ISIG: u32 = 0o1;
const ICANON: u32 = 0o2;
const ECHO: u32 = 0o10;
const ECHOE: u32 = 0o20;
const ECHOK: u32 = 0o40;
const ECHONL: u32 = 0o100;
const NOFLSH: u32 = 0o200;
const TOSTOP: u32 = 0o400;
const ECHOCTL: u32 = 0o1000;
const ECHOKE: u32 = 0o4000;
const IEXTEN: u32 = 0o100000;

/// `struct termios2` de Linux (le `struct termios` du noyau en est le début, 36 octets).
#[derive(Clone, Copy)]
struct Termios {
    iflag: u32,
    oflag: u32,
    cflag: u32,
    lflag: u32,
    line: u8,
    cc: [u8; NCCS],
    ispeed: u32,
    ospeed: u32,
}

impl Termios {
    /// `tty_std_termios` de Linux.
    fn standard() -> Termios {
        let mut cc = [0u8; NCCS];
        cc[VINTR] = 0x03;
        cc[VQUIT] = 0x1c;
        cc[VERASE] = 0x7f;
        cc[VKILL] = 0x15;
        cc[VEOF] = 0x04;
        cc[VTIME] = 0;
        cc[VMIN] = 1;
        cc[VSTART] = 0x11;
        cc[VSTOP] = 0x13;
        cc[VSUSP] = 0x1a;
        cc[VREPRINT] = 0x12;
        cc[13] = 0x0f; // VDISCARD
        cc[VWERASE] = 0x17;
        cc[VLNEXT] = 0x16;
        Termios {
            iflag: ICRNL | IXON | IUTF8,
            oflag: OPOST | ONLCR,
            cflag: B38400 | CS8 | CREAD | HUPCL,
            lflag: ISIG | ICANON | ECHO | ECHOE | ECHOK | ECHOCTL | ECHOKE | IEXTEN,
            line: 0,
            cc,
            ispeed: 38400,
            ospeed: 38400,
        }
    }

    fn to_bytes(self) -> [u8; 44] {
        let mut b = [0u8; 44];
        b[0..4].copy_from_slice(&self.iflag.to_le_bytes());
        b[4..8].copy_from_slice(&self.oflag.to_le_bytes());
        b[8..12].copy_from_slice(&self.cflag.to_le_bytes());
        b[12..16].copy_from_slice(&self.lflag.to_le_bytes());
        b[16] = self.line;
        b[17..36].copy_from_slice(&self.cc);
        b[36..40].copy_from_slice(&self.ispeed.to_le_bytes());
        b[40..44].copy_from_slice(&self.ospeed.to_le_bytes());
        b
    }

    fn from_bytes(b: &[u8], keep_speed: &Termios) -> Termios {
        let u = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let mut cc = [0u8; NCCS];
        cc.copy_from_slice(&b[17..36]);
        let (ispeed, ospeed) = if b.len() >= 44 { (u(36), u(40)) } else { (keep_speed.ispeed, keep_speed.ospeed) };
        Termios { iflag: u(0), oflag: u(4), cflag: u(8), lflag: u(12), line: b[16], cc, ispeed, ospeed }
    }

    fn canon(&self) -> bool {
        self.lflag & ICANON != 0
    }

    /// Le caractère de contrôle `i` est-il actif (`_POSIX_VDISABLE` = 0) et égal à `c` ?
    fn is(&self, i: usize, c: u8) -> bool {
        self.cc[i] != 0 && self.cc[i] == c
    }
}

// --- État d'un terminal -------------------------------------------------------------------------

/// Taille de la file d'entrée (`N_TTY_BUF_SIZE`).
const INPUT_MAX: usize = 4096;
/// Sortie en attente du maître au-delà de laquelle l'esclave attend.
const OUTPUT_MAX: usize = 64 * 1024;

struct State {
    tio: Termios,
    winsize: [u16; 4],
    /// Mode canonique : lignes complètes (une ligne vide = fin de fichier `VEOF`).
    lines: VecDeque<Vec<u8>>,
    lines_bytes: usize,
    /// Ligne en cours d'édition.
    partial: Vec<u8>,
    /// Mode brut : octets reçus.
    raw: VecDeque<u8>,
    /// `VLNEXT` vu : l'octet suivant est pris tel quel.
    lnext: bool,
    /// Colonne de la sortie (`ONOCR`, effacement des tabulations).
    column: usize,
    /// Sortie à lire par le maître.
    out: VecDeque<u8>,
    /// Sortie suspendue (`VSTOP` avec `IXON`, `TCOOFF`).
    stopped: bool,
    /// Mode paquet (`TIOCPKT`).
    packet: bool,
    locked: bool,
    exclusive: bool,
    master_open: bool,
    slaves: usize,
    slave_ever: bool,
    /// Le maître a fermé (ou `vhangup`) : l'esclave est raccroché.
    hung_up: bool,
    /// Session dont c'est le terminal de contrôle, groupe de premier plan.
    sid: usize,
    pgrp: usize,
}

impl State {
    fn input_len(&self) -> usize {
        self.lines_bytes + self.partial.len() + self.raw.len()
    }

    fn input_room(&self) -> bool {
        self.input_len() < INPUT_MAX
    }

    /// Octets lisibles par l'esclave maintenant.
    fn readable(&self) -> usize {
        if self.tio.canon() {
            self.lines.front().map_or(0, |l| l.len().max(1))
        } else {
            self.raw.len()
        }
    }

    /// Écrit dans la sortie un octet traité par `c_oflag`.
    fn put_out(&mut self, c: u8) {
        let o = self.tio.oflag;
        if o & OPOST != 0 {
            match c {
                b'\n' if o & ONLCR != 0 => {
                    self.out.push_back(b'\r');
                    self.out.push_back(b'\n');
                    self.column = 0;
                    return;
                }
                b'\n' => {
                    if o & ONLRET != 0 {
                        self.column = 0;
                    }
                }
                b'\r' if o & ONOCR != 0 && self.column == 0 => return,
                b'\r' if o & OCRNL != 0 => {
                    self.out.push_back(b'\n');
                    if o & ONLRET != 0 {
                        self.column = 0;
                    }
                    return;
                }
                b'\r' => self.column = 0,
                b'\t' => self.column = (self.column | 7) + 1,
                0x08 => self.column = self.column.saturating_sub(1),
                c if c >= 0x20 && (c & 0xC0) != 0x80 => self.column += 1,
                _ => {}
            }
        }
        self.out.push_back(c);
    }

    /// Écho d'un octet reçu (`ECHOCTL` : les caractères de contrôle s'affichent `^X`).
    fn echo(&mut self, c: u8) {
        if self.tio.lflag & ECHOCTL != 0 && (c < 0x20 && c != b'\t' && c != b'\n' || c == 0x7f) {
            self.put_out(b'^');
            self.put_out(c ^ 0x40);
        } else {
            self.put_out(c);
        }
    }

    /// Efface à l'écran le dernier caractère de la ligne (`ECHOE`).
    fn erase_echo(&mut self, removed: &[u8]) {
        let ctl = |c: u8| c < 0x20 || c == 0x7f;
        let width = match removed {
            [c] if *c == b'\t' => 1,
            [c] if ctl(*c) && self.tio.lflag & ECHOCTL != 0 => 2,
            [c] if ctl(*c) => 0,
            _ => 1,
        };
        for _ in 0..width {
            self.put_out(0x08);
            self.put_out(b' ');
            self.put_out(0x08);
        }
    }

    /// Retire le dernier caractère de la ligne en cours (une séquence UTF-8 entière avec `IUTF8`).
    fn erase_one(&mut self) -> Option<Vec<u8>> {
        if self.partial.is_empty() {
            return None;
        }
        let mut start = self.partial.len() - 1;
        if self.tio.iflag & IUTF8 != 0 {
            while start > 0 && (self.partial[start] & 0xC0) == 0x80 {
                start -= 1;
            }
        }
        Some(self.partial.split_off(start))
    }

    fn finish_line(&mut self, line: Vec<u8>) {
        self.lines_bytes += line.len();
        self.lines.push_back(line);
    }

    fn flush_input(&mut self) {
        self.lines.clear();
        self.lines_bytes = 0;
        self.partial.clear();
        self.raw.clear();
        self.lnext = false;
    }

    /// Passage canonique ↔ brut : ce qui est en file reste lisible.
    fn retune(&mut self, was_canon: bool) {
        let now = self.tio.canon();
        if was_canon && !now {
            for l in self.lines.drain(..) {
                self.raw.extend(l);
            }
            self.lines_bytes = 0;
            let p = core::mem::take(&mut self.partial);
            self.raw.extend(p);
        } else if !was_canon && now && !self.raw.is_empty() {
            let r: Vec<u8> = self.raw.drain(..).collect();
            self.finish_line(r);
        }
    }
}

/// Options d'une instance de devpts (`pts_mount_opts`).
#[derive(Clone)]
struct PtsOpts {
    /// Propriétaire et groupe imposés aux esclaves (identités globales) ; sinon ceux du créateur.
    uid: Option<u32>,
    gid: Option<u32>,
    /// `uid=`/`gid=` tels que donnés (pour `mountinfo`).
    uid_text: Option<u32>,
    gid_text: Option<u32>,
    /// Droits des esclaves (`mode`, 0600 par défaut) et de `ptmx` (`ptmxmode`, 0000).
    mode: u32,
    ptmxmode: u32,
    /// Nombre d'esclaves au plus (`max`).
    max: u32,
}

impl PtsOpts {
    fn defaults() -> PtsOpts {
        PtsOpts { uid: None, gid: None, uid_text: None, gid_text: None, mode: 0o600, ptmxmode: 0, max: PTY_MAX }
    }
}

/// `NR_UNIX98_PTY_MAX`.
const PTY_MAX: u32 = 1 << 20;
/// `kernel.pty.max` et `kernel.pty.reserve` : les instances autres que l'initiale ne prennent
/// pas les `PTY_RESERVE` derniers terminaux.
const PTY_LIMIT: u32 = 4096;
const PTY_RESERVE: u32 = 1024;
/// Terminaux existants, toutes instances (`pty_count`).
static PTY_COUNT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Une instance de devpts.
pub struct PtsFs {
    /// Instance de l'espace initial (montée sur `/dev/pts` au démarrage).
    initial: bool,
    ptys: Mutex<BTreeMap<u32, Weak<Pty>>>,
    opts: Mutex<PtsOpts>,
    /// Espace utilisateur du monteur (`s_user_ns`) : son root possède `ptmx`.
    user_ns: Arc<crate::userns::UserNs>,
}

static INIT_PTS: spin::Once<Arc<PtsFs>> = spin::Once::new();

/// Instance initiale : `gid=5,mode=620,ptmxmode=000`, comme le `/dev/pts` de systemd.
pub fn init_pts() -> Arc<PtsFs> {
    INIT_PTS
        .call_once(|| {
            let mut opts = PtsOpts::defaults();
            opts.gid = Some(TTY_GID);
            opts.gid_text = Some(TTY_GID);
            opts.mode = 0o620;
            Arc::new(PtsFs { initial: true, ptys: Mutex::new(BTreeMap::new()), opts: Mutex::new(opts), user_ns: crate::userns::init_user_ns() })
        })
        .clone()
}

impl PtsFs {
    /// Nouvelle instance (`devpts_fill_super`) selon les options `data` du monteur.
    pub fn new(data: &str, creds: &crate::creds::Creds) -> Result<Arc<PtsFs>, i64> {
        let mut opts = PtsOpts::defaults();
        parse_opts(&mut opts, data, creds)?;
        Ok(Arc::new(PtsFs { initial: false, ptys: Mutex::new(BTreeMap::new()), opts: Mutex::new(opts), user_ns: creds.user_ns.clone() }))
    }

    /// `mount -o remount` (`devpts_reconfigure`) : options remises aux défauts puis relues.
    pub fn remount(&self, data: &str, creds: &crate::creds::Creds) -> Result<(), i64> {
        let mut o = PtsOpts::defaults();
        parse_opts(&mut o, data, creds)?;
        *self.opts.lock() = o;
        Ok(())
    }

    /// Options affichées par `mountinfo` (`devpts_show_options`).
    pub fn opts_text(&self) -> String {
        let o = self.opts.lock();
        let mut s = String::new();
        if let Some(u) = o.uid_text {
            s.push_str(&format!("uid={},", u));
        }
        if let Some(g) = o.gid_text {
            s.push_str(&format!("gid={},", g));
        }
        s.push_str(&format!("mode={:03o},ptmxmode={:03o}", o.mode, o.ptmxmode));
        if o.max < PTY_MAX {
            s.push_str(&format!(",max={}", o.max));
        }
        s
    }

    fn find(&self, index: u32) -> Option<Arc<Pty>> {
        self.ptys.lock().get(&index).and_then(|w| w.upgrade())
    }

    /// Instance initiale (celle du `/dev/pts` de démarrage) ?
    pub fn is_initial(&self) -> bool {
        self.initial
    }

    /// Indices des esclaves existants.
    fn indices(&self) -> Vec<u32> {
        self.ptys.lock().iter().filter(|(_, w)| w.strong_count() > 0).map(|(i, _)| *i).collect()
    }

    /// Propriétaire de `ptmx` : root de l'espace du monteur, sinon root global (`mknod_ptmx`).
    fn ptmx_owner(&self) -> (u32, u32) {
        (self.user_ns.make_kuid(0).unwrap_or(0), self.user_ns.make_kgid(0).unwrap_or(0))
    }
}

/// `parse_mount_options` de devpts : `uid`, `gid` (projetés dans l'espace du monteur), `mode`,
/// `ptmxmode` (octal, `S_IALLUGO`), `max`, `newinstance` (toujours vrai) ; autre → `EINVAL`.
fn parse_opts(o: &mut PtsOpts, data: &str, creds: &crate::creds::Creds) -> Result<(), i64> {
    for opt in data.split(',').filter(|x| !x.is_empty()) {
        let (k, v) = opt.split_once('=').unwrap_or((opt, ""));
        let num = || v.parse::<u32>().map_err(|_| EINVAL);
        let oct = || u32::from_str_radix(v, 8).map(|m| m & 0o7777).map_err(|_| EINVAL);
        match k {
            "uid" => {
                let n = num()?;
                o.uid = Some(creds.user_ns.make_kuid(n).ok_or(EINVAL)?);
                o.uid_text = Some(n);
            }
            "gid" => {
                let n = num()?;
                o.gid = Some(creds.user_ns.make_kgid(n).ok_or(EINVAL)?);
                o.gid_text = Some(n);
            }
            "mode" => o.mode = oct()?,
            "ptmxmode" => o.ptmxmode = oct()?,
            "max" => {
                let n = num()?;
                if n > PTY_MAX {
                    return Err(EINVAL);
                }
                o.max = n;
            }
            "newinstance" => {}
            _ => return Err(EINVAL),
        }
    }
    Ok(())
}

/// `generic_permission` d'un nœud de devpts (droits `mode`, propriétaire `uid:gid`) pour
/// l'accès demandé par `flags`, avec `CAP_DAC_OVERRIDE` si propriétaire et groupe sont
/// projetés dans l'espace de l'appelant (`capable_wrt_inode_uidgid`).
fn may_open(t: &Thread, mode: u32, uid: u32, gid: u32, flags: u32) -> Result<(), i64> {
    let want = match flags & crate::fd::O_ACCMODE {
        0 => 4,
        1 => 2,
        _ => 6,
    };
    let c = t.process.creds.lock();
    let bits = if c.fsuid == uid {
        (mode >> 6) & 7
    } else if c.fsgid == gid || c.groups.contains(&gid) {
        (mode >> 3) & 7
    } else {
        mode & 7
    };
    if bits & want == want {
        return Ok(());
    }
    let cap = crate::userns::bit(crate::userns::CAP_DAC_OVERRIDE);
    if c.cap_eff & cap != 0 && c.user_ns.has_kuid(uid) && c.user_ns.has_kgid(gid) {
        return Ok(());
    }
    Err(EACCES)
}

/// Un pseudo-terminal.
pub struct Pty {
    pub index: u32,
    /// Instance de devpts qui le numérote.
    fs: Arc<PtsFs>,
    /// Droits de l'esclave (`mode` de l'instance à la création).
    mode: u32,
    st: Mutex<State>,
    /// Lecteurs de l'esclave (entrée prête) et écrivains du maître (place dans l'entrée).
    input_wq: WaitQueue,
    /// Lecteurs du maître (sortie prête) et écrivains de l'esclave (place dans la sortie).
    output_wq: WaitQueue,
    owner: (u32, u32),
}

/// Groupe `tty` de Debian : propriétaire de groupe des esclaves (`grantpt`).
const TTY_GID: u32 = 5;
const PTS_MAJOR: u32 = 136;
const INO_PTMX: u64 = 0x7000_0000;
const INO_PTS: u64 = 0x7000_0100;

fn linux_dev(major: u32, minor: u32) -> u64 {
    let (ma, mi) = (major as u64, minor as u64);
    ((ma & 0xfff) << 8) | ((ma & !0xfff) << 32) | (mi & 0xff) | ((mi & !0xff) << 12)
}

/// Envoie `sig` à tous les processus du groupe `pgrp`.
fn signal_pgrp(pgrp: usize, sig: i32) {
    if pgrp == 0 {
        return;
    }
    for p in crate::all_processes() {
        if p.pgid.load(core::sync::atomic::Ordering::Relaxed) == pgrp {
            signal::send_to_process(&p, SigInfo::new(sig, signal::SI_KERNEL));
        }
    }
}

impl Pty {
    /// Nouveau terminal de l'instance `fs` (`devpts_new_index` puis `devpts_pty_new`) :
    /// esclave à `uid`/`gid` de l'instance, sinon à ceux (`fsuid`/`fsgid`) du créateur.
    fn new(fs: &Arc<PtsFs>, creator: (u32, u32)) -> Result<Arc<Pty>, i64> {
        use core::sync::atomic::Ordering;
        let opts = fs.opts.lock().clone();
        let mut g = fs.ptys.lock();
        g.retain(|_, w| w.strong_count() > 0);
        let limit = if fs.initial { PTY_LIMIT } else { PTY_LIMIT - PTY_RESERVE };
        if PTY_COUNT.fetch_add(1, Ordering::AcqRel) >= limit || g.len() as u32 >= opts.max {
            PTY_COUNT.fetch_sub(1, Ordering::AcqRel);
            return Err(ENOSPC);
        }
        let Some(index) = (0u32..PTY_MAX).find(|i| !g.contains_key(i)) else {
            PTY_COUNT.fetch_sub(1, Ordering::AcqRel);
            return Err(ENOSPC);
        };
        let owner = (opts.uid.unwrap_or(creator.0), opts.gid.unwrap_or(creator.1));
        let p = Arc::new(Pty {
            index,
            fs: fs.clone(),
            mode: opts.mode,
            st: Mutex::new(State {
                tio: Termios::standard(),
                winsize: [0; 4],
                lines: VecDeque::new(),
                lines_bytes: 0,
                partial: Vec::new(),
                raw: VecDeque::new(),
                lnext: false,
                column: 0,
                out: VecDeque::new(),
                stopped: false,
                packet: false,
                locked: true,
                exclusive: false,
                master_open: true,
                slaves: 0,
                slave_ever: false,
                hung_up: false,
                sid: 0,
                pgrp: 0,
            }),
            input_wq: WaitQueue::new(),
            output_wq: WaitQueue::new(),
            owner,
        });
        g.insert(index, Arc::downgrade(&p));
        Ok(p)
    }

    fn wake(&self) {
        self.input_wq.wake_all();
        self.output_wq.wake_all();
    }

    /// Entrée venue du maître : la discipline de ligne, octet par octet. Rend le nombre d'octets
    /// acceptés (la file d'entrée peut se remplir) et les signaux à envoyer.
    fn receive(&self, st: &mut State, data: &[u8], sigs: &mut Vec<i32>) -> usize {
        let mut used = 0;
        for &b in data {
            if !st.input_room() && !(st.tio.canon() && st.partial.len() >= INPUT_MAX - 1) {
                break;
            }
            used += 1;
            let tio = st.tio;
            let mut c = b;
            if tio.iflag & ISTRIP != 0 {
                c &= 0x7f;
            }
            if st.lnext {
                st.lnext = false;
                self.store(st, c, true);
                continue;
            }
            if c == b'\r' {
                if tio.iflag & IGNCR != 0 {
                    continue;
                }
                if tio.iflag & ICRNL != 0 {
                    c = b'\n';
                }
            } else if c == b'\n' && tio.iflag & INLCR != 0 {
                c = b'\r';
            }
            if tio.iflag & IXON != 0 {
                if tio.is(VSTOP, c) {
                    st.stopped = true;
                    continue;
                }
                if tio.is(VSTART, c) {
                    st.stopped = false;
                    continue;
                }
                if st.stopped && tio.iflag & IXANY != 0 {
                    st.stopped = false;
                }
            }
            if tio.lflag & ISIG != 0 {
                let sig = if tio.is(VINTR, c) {
                    Some(signal::SIGINT)
                } else if tio.is(VQUIT, c) {
                    Some(3) // SIGQUIT
                } else if tio.is(VSUSP, c) {
                    Some(20) // SIGTSTP
                } else {
                    None
                };
                if let Some(s) = sig {
                    if tio.lflag & NOFLSH == 0 {
                        st.flush_input();
                        st.out.clear();
                    }
                    if tio.lflag & ECHO != 0 {
                        st.echo(c);
                    }
                    sigs.push(s);
                    continue;
                }
            }
            if tio.lflag & IEXTEN != 0 && tio.is(VLNEXT, c) {
                st.lnext = true;
                if tio.lflag & ECHO != 0 && tio.lflag & ECHOCTL != 0 {
                    st.put_out(b'^');
                    st.put_out(0x08);
                }
                continue;
            }
            if tio.canon() {
                if tio.is(VERASE, c) {
                    if let Some(gone) = st.erase_one() {
                        if tio.lflag & ECHO != 0 {
                            if tio.lflag & ECHOE != 0 {
                                st.erase_echo(&gone);
                            } else {
                                st.echo(c);
                            }
                        }
                    }
                    continue;
                }
                if tio.lflag & IEXTEN != 0 && tio.is(VWERASE, c) {
                    // Espaces, puis le mot.
                    let mut seen_word = false;
                    while let Some(&last) = st.partial.last() {
                        let space = last == b' ' || last == b'\t';
                        if space && seen_word {
                            break;
                        }
                        seen_word |= !space;
                        let gone = st.erase_one().unwrap_or_default();
                        if tio.lflag & ECHO != 0 && tio.lflag & ECHOE != 0 {
                            st.erase_echo(&gone);
                        }
                    }
                    continue;
                }
                if tio.is(VKILL, c) {
                    if tio.lflag & ECHO != 0 && tio.lflag & ECHOKE != 0 && tio.lflag & ECHOE != 0 {
                        while let Some(gone) = st.erase_one() {
                            st.erase_echo(&gone);
                        }
                    } else {
                        st.partial.clear();
                        if tio.lflag & ECHO != 0 {
                            st.echo(c);
                            if tio.lflag & ECHOK != 0 {
                                st.put_out(b'\n');
                            }
                        }
                    }
                    continue;
                }
                if tio.lflag & IEXTEN != 0 && tio.is(VREPRINT, c) {
                    if tio.lflag & ECHO != 0 {
                        st.echo(c);
                        st.put_out(b'\n');
                        let p = st.partial.clone();
                        for x in p {
                            st.echo(x);
                        }
                    }
                    continue;
                }
                if tio.is(VEOF, c) {
                    let line = core::mem::take(&mut st.partial);
                    st.finish_line(line);
                    continue;
                }
            }
            self.store(st, c, false);
        }
        used
    }

    /// Range un octet de donnée (fin de ligne comprise) et son écho.
    fn store(&self, st: &mut State, c: u8, literal: bool) {
        let tio = st.tio;
        if tio.canon() {
            let eol = !literal && (c == b'\n' || tio.is(VEOL, c) || tio.is(VEOL2, c));
            if st.partial.len() < INPUT_MAX - 1 || eol {
                st.partial.push(c);
            }
            if tio.lflag & ECHO != 0 {
                st.echo(c);
            } else if c == b'\n' && tio.lflag & ECHONL != 0 {
                st.put_out(b'\n');
            }
            if eol {
                let line = core::mem::take(&mut st.partial);
                st.finish_line(line);
            }
        } else {
            st.raw.push_back(c);
            if tio.lflag & ECHO != 0 {
                st.echo(c);
            } else if c == b'\n' && tio.lflag & ECHONL != 0 {
                st.put_out(b'\n');
            }
        }
    }

    /// Le terminal est-il celui de contrôle de `p` ?
    fn is_ctty_of(&self, p: &Process) -> bool {
        p.ctty.lock().as_ref().and_then(|w| w.upgrade()).map_or(false, |c| core::ptr::eq(&*c, self))
    }

    /// Contrôle des travaux avant une lecture (`SIGTTIN`) ou une écriture avec `TOSTOP`
    /// (`SIGTTOU`) d'un processus d'arrière-plan sur son terminal de contrôle.
    fn job_control(&self, t: &Thread, sig: i32) -> Result<(), i64> {
        let pgrp = self.st.lock().pgrp;
        let me = &t.process;
        if pgrp == 0 || !self.is_ctty_of(me) || me.pgid.load(core::sync::atomic::Ordering::Relaxed) == pgrp {
            return Ok(());
        }
        let blocked = t.sigmask.load(core::sync::atomic::Ordering::Relaxed) & (1 << (sig - 1)) != 0;
        if blocked || signal::ignored(me, sig) {
            // Lecture : `EIO` ; écriture : permise (Linux `tty_check_change`).
            return if sig == 22 { Ok(()) } else { Err(EIO) };
        }
        signal_pgrp(me.pgid.load(core::sync::atomic::Ordering::Relaxed), sig);
        Err(signal::ERESTARTSYS)
    }

    // --- Côté esclave ---

    fn slave_read(&self, t: &Thread, kb: &mut [u8], nonblock: bool) -> Result<usize, i64> {
        if kb.is_empty() {
            return Ok(0);
        }
        self.job_control(t, 21)?; // SIGTTIN
        let (vmin, vtime) = {
            let st = self.st.lock();
            (st.tio.cc[VMIN] as usize, st.tio.cc[VTIME] as u64)
        };
        // `VTIME` (dixièmes de seconde) : délai total si `VMIN` = 0, délai entre deux octets
        // (armé au premier reçu) si `VMIN` > 0.
        let tick = core::time::Duration::from_millis(vtime * 100);
        let mut deadline: Option<time::Instant> = (vmin == 0 && vtime > 0).then(|| time::Instant::now() + tick);
        let mut last_len = 0usize;
        /// Rendu par l'attente : premier octet arrivé, armer le minuteur entre octets.
        const START_TIMER: i64 = i64::MIN;
        loop {
            let r = wait_event(&[&self.input_wq], deadline, Some(t), || {
                let mut st = self.st.lock();
                if st.tio.canon() {
                    if let Some(line) = st.lines.front_mut() {
                        if line.is_empty() {
                            st.lines.pop_front();
                            return Some(Ok(0));
                        }
                        let n = kb.len().min(line.len());
                        kb[..n].copy_from_slice(&line[..n]);
                        line.drain(..n);
                        if line.is_empty() {
                            st.lines.pop_front();
                        }
                        st.lines_bytes -= n;
                        return Some(Ok(n));
                    }
                } else {
                    let need = if vmin == 0 { 1 } else { vmin.min(kb.len()) };
                    let have = st.raw.len();
                    if have >= need || (have > 0 && (vmin == 0 || deadline.map_or(false, |d| time::Instant::now() >= d))) {
                        let n = kb.len().min(have);
                        for (d, s) in kb.iter_mut().zip(st.raw.drain(..n)) {
                            *d = s;
                        }
                        return Some(Ok(n));
                    }
                    if vmin == 0 && vtime == 0 {
                        return Some(Ok(0));
                    }
                    if vmin > 0 && vtime > 0 && have > 0 && deadline.is_none() {
                        return Some(Err(START_TIMER));
                    }
                }
                if st.hung_up {
                    return Some(Ok(0));
                }
                if nonblock {
                    return Some(Err(EAGAIN));
                }
                None
            });
            match r {
                Ok(Ok(n)) => {
                    if n > 0 {
                        self.input_wq.wake_all(); // place pour le maître
                    }
                    return Ok(n);
                }
                Ok(Err(START_TIMER)) => {
                    last_len = self.st.lock().raw.len();
                    deadline = Some(time::Instant::now() + tick);
                    continue;
                }
                Ok(Err(e)) => return Err(e),
                Err(WaitError::Interrupted) => return Err(signal::ERESTARTSYS),
                Err(WaitError::TimedOut) => {
                    let have = self.st.lock().raw.len();
                    // Rien de neuf pendant `VTIME` : on rend ce qui est là ; sinon on réarme.
                    if vmin == 0 || have == last_len {
                        let mut st = self.st.lock();
                        let n = kb.len().min(st.raw.len());
                        for (d, s) in kb.iter_mut().zip(st.raw.drain(..n)) {
                            *d = s;
                        }
                        return Ok(n);
                    }
                    last_len = have;
                    deadline = Some(time::Instant::now() + tick);
                    continue;
                }
            }
        }
    }

    fn slave_write(&self, t: &Thread, kb: &[u8], nonblock: bool) -> Result<usize, i64> {
        if self.st.lock().tio.lflag & TOSTOP != 0 {
            self.job_control(t, 22)?; // SIGTTOU
        }
        let mut done = 0;
        while done < kb.len() {
            let r = wait_event(&[&self.output_wq], None, Some(t), || {
                let mut st = self.st.lock();
                if st.hung_up {
                    return Some(Err(EIO));
                }
                if !st.stopped && st.out.len() < OUTPUT_MAX {
                    let n = (kb.len() - done).min(OUTPUT_MAX - st.out.len()).max(1);
                    for &c in &kb[done..done + n] {
                        st.put_out(c);
                    }
                    return Some(Ok(n));
                }
                if nonblock {
                    return Some(Err(EAGAIN));
                }
                None
            });
            match r {
                Ok(Ok(n)) => {
                    done += n;
                    self.output_wq.wake_all();
                }
                Ok(Err(e)) => return if done > 0 { Ok(done) } else { Err(e) },
                Err(_) => return if done > 0 { Ok(done) } else { Err(signal::ERESTARTSYS) },
            }
        }
        Ok(done)
    }

    // --- Côté maître ---

    fn master_read(&self, t: &Thread, kb: &mut [u8], nonblock: bool) -> Result<usize, i64> {
        if kb.is_empty() {
            return Ok(0);
        }
        let r = wait_event(&[&self.output_wq], None, Some(t), || {
            let mut st = self.st.lock();
            if !st.out.is_empty() {
                let (skip, room) = if st.packet { (1, kb.len() - 1) } else { (0, kb.len()) };
                if skip == 1 {
                    kb[0] = 0; // TIOCPKT_DATA
                }
                let n = room.min(st.out.len());
                for (d, s) in kb[skip..].iter_mut().zip(st.out.drain(..n)) {
                    *d = s;
                }
                return Some(Ok(n + skip));
            }
            if st.slave_ever && st.slaves == 0 {
                return Some(Err(EIO));
            }
            if nonblock {
                return Some(Err(EAGAIN));
            }
            None
        });
        match r {
            Ok(Ok(n)) => {
                self.output_wq.wake_all();
                Ok(n)
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err(signal::ERESTARTSYS),
        }
    }

    fn master_write(&self, t: &Thread, kb: &[u8], nonblock: bool) -> Result<usize, i64> {
        let mut done = 0;
        while done < kb.len() {
            let mut sigs = Vec::new();
            let r = wait_event(&[&self.input_wq], None, Some(t), || {
                let mut st = self.st.lock();
                let n = self.receive(&mut st, &kb[done..], &mut sigs);
                if n > 0 {
                    return Some(Ok(n));
                }
                if nonblock {
                    return Some(Err(EAGAIN));
                }
                None
            });
            let pgrp = self.st.lock().pgrp;
            for s in sigs {
                signal_pgrp(pgrp, s);
            }
            match r {
                Ok(Ok(n)) => {
                    done += n;
                    self.wake();
                }
                Ok(Err(e)) => return if done > 0 { Ok(done) } else { Err(e) },
                Err(_) => return if done > 0 { Ok(done) } else { Err(signal::ERESTARTSYS) },
            }
        }
        Ok(done)
    }

    // --- ioctl communs ---

    fn ioctl(&self, t: &Thread, master: bool, req: u64, arg: u64) -> SysResult {
        const TCGETS: u64 = 0x5401;
        const TCSETS: u64 = 0x5402;
        const TCSETSW: u64 = 0x5403;
        const TCSETSF: u64 = 0x5404;
        const TCSBRK: u64 = 0x5409;
        const TCXONC: u64 = 0x540A;
        const TCFLSH: u64 = 0x540B;
        const TIOCEXCL: u64 = 0x540C;
        const TIOCNXCL: u64 = 0x540D;
        const TIOCSCTTY: u64 = 0x540E;
        const TIOCGPGRP: u64 = 0x540F;
        const TIOCSPGRP: u64 = 0x5410;
        const TIOCOUTQ: u64 = 0x5411;
        const TIOCSTI: u64 = 0x5412;
        const TIOCGWINSZ: u64 = 0x5413;
        const TIOCSWINSZ: u64 = 0x5414;
        const FIONREAD: u64 = 0x541B;
        const TIOCPKT: u64 = 0x5420;
        const TIOCNOTTY: u64 = 0x5422;
        const TIOCSETD: u64 = 0x5423;
        const TIOCGETD: u64 = 0x5424;
        const TCSBRKP: u64 = 0x5425;
        const TIOCSBRK: u64 = 0x5427;
        const TIOCCBRK: u64 = 0x5428;
        const TIOCGSID: u64 = 0x5429;
        const TCGETS2: u64 = 0x802C_542A;
        const TCSETS2: u64 = 0x402C_542B;
        const TCSETSW2: u64 = 0x402C_542C;
        const TCSETSF2: u64 = 0x402C_542D;
        const TIOCGPTN: u64 = 0x8004_5430;
        const TIOCSPTLCK: u64 = 0x4004_5431;
        const TIOCGDEV: u64 = 0x8004_5432;
        const TIOCVHANGUP: u64 = 0x5437;
        const TIOCGPKT: u64 = 0x8004_5438;
        const TIOCGPTLCK: u64 = 0x8004_5439;
        const TIOCGEXCL: u64 = 0x8004_5440;
        const TIOCGPTPEER: u64 = 0x5441;
        const TIOCGLCKTRMIOS: u64 = 0x5456;
        const TIOCSLCKTRMIOS: u64 = 0x5457;
        const TIOCMGET: u64 = 0x5415;

        let mm = t.mm();
        let put_i32 = |v: i32| mm.write_user::<i32>(arg as usize, &v).map_err(fault).map(|_| 0);
        match req {
            TCGETS | TCGETS2 => {
                let b = self.st.lock().tio.to_bytes();
                let n = if req == TCGETS { 36 } else { 44 };
                mm.copy_to_user(arg as usize, &b[..n]).map_err(fault)?;
                Ok(0)
            }
            TCSETS | TCSETSW | TCSETSF | TCSETS2 | TCSETSW2 | TCSETSF2 => {
                let n = if matches!(req, TCSETS | TCSETSW | TCSETSF) { 36 } else { 44 };
                let mut b = [0u8; 44];
                mm.copy_from_user(&mut b[..n], arg as usize).map_err(fault)?;
                if !master {
                    self.job_control(t, 22)?;
                }
                {
                    let mut st = self.st.lock();
                    if matches!(req, TCSETSF | TCSETSF2) {
                        st.flush_input();
                    }
                    let was = st.tio.canon();
                    let old = st.tio;
                    st.tio = Termios::from_bytes(&b[..n], &old);
                    st.retune(was);
                    if st.tio.iflag & IXON == 0 {
                        st.stopped = false;
                    }
                }
                self.wake();
                Ok(0)
            }
            TIOCGWINSZ => {
                let ws = self.st.lock().winsize;
                mm.write_user(arg as usize, &ws).map_err(fault)?;
                Ok(0)
            }
            TIOCSWINSZ => {
                let ws = mm.read_user::<[u16; 4]>(arg as usize).map_err(fault)?;
                let (changed, pgrp) = {
                    let mut st = self.st.lock();
                    let c = st.winsize != ws;
                    st.winsize = ws;
                    (c, st.pgrp)
                };
                if changed {
                    signal_pgrp(pgrp, 28); // SIGWINCH
                }
                Ok(0)
            }
            FIONREAD => {
                let st = self.st.lock();
                let n = if master { st.out.len() } else if st.tio.canon() { st.lines_bytes } else { st.raw.len() };
                drop(st);
                put_i32(n as i32)
            }
            TIOCOUTQ => {
                let n = if master { 0 } else { self.st.lock().out.len() };
                put_i32(n as i32)
            }
            TCFLSH => {
                let mut st = self.st.lock();
                match arg {
                    0 => st.flush_input(),
                    1 => st.out.clear(),
                    2 => {
                        st.flush_input();
                        st.out.clear();
                    }
                    _ => return Err(EINVAL),
                }
                drop(st);
                self.wake();
                Ok(0)
            }
            TCXONC => {
                let mut st = self.st.lock();
                match arg {
                    0 => st.stopped = true,
                    1 => st.stopped = false,
                    2 | 3 => {
                        let c = st.tio.cc[if arg == 2 { VSTOP } else { VSTART }];
                        st.put_out(c);
                    }
                    _ => return Err(EINVAL),
                }
                drop(st);
                self.wake();
                Ok(0)
            }
            TCSBRK | TCSBRKP | TIOCSBRK | TIOCCBRK => Ok(0),
            TIOCEXCL | TIOCNXCL => {
                self.st.lock().exclusive = req == TIOCEXCL;
                Ok(0)
            }
            TIOCGEXCL => {
                let e = self.st.lock().exclusive;
                put_i32(e as i32)
            }
            TIOCGETD => put_i32(self.st.lock().tio.line as i32),
            TIOCSETD => {
                let d = mm.read_user::<i32>(arg as usize).map_err(fault)?;
                if d != 0 {
                    return Err(EINVAL); // seule N_TTY
                }
                Ok(0)
            }
            TIOCSCTTY => self.make_ctty(t, arg == 1).map(|_| 0),
            TIOCNOTTY => {
                if !self.is_ctty_of(&t.process) {
                    return Err(ENOTTY);
                }
                disassociate(&t.process);
                Ok(0)
            }
            TIOCGPGRP => {
                if !master && !self.is_ctty_of(&t.process) {
                    return Err(ENOTTY);
                }
                // Groupe de premier plan vu de l'espace pid de l'appelant (`pid_vnr`).
                let g = self.st.lock().pgrp;
                put_i32(t.process.vnr(g) as i32)
            }
            TIOCSPGRP => {
                if !self.is_ctty_of(&t.process) {
                    return Err(ENOTTY);
                }
                let g = mm.read_user::<i32>(arg as usize).map_err(fault)?;
                if g < 0 {
                    return Err(EINVAL);
                }
                // Numéro de l'appelant → numéro global du groupe.
                let g = t.process.pid_ns.global_of(g as usize).ok_or(ESRCH)?;
                let sid = t.process.sid.load(core::sync::atomic::Ordering::Relaxed);
                let exists = crate::all_processes().iter().any(|p| p.pgid.load(core::sync::atomic::Ordering::Relaxed) == g);
                if !exists {
                    return Err(ESRCH);
                }
                let same = crate::all_processes().iter().any(|p| {
                    p.pgid.load(core::sync::atomic::Ordering::Relaxed) == g && p.sid.load(core::sync::atomic::Ordering::Relaxed) == sid
                });
                if !same {
                    return Err(EPERM);
                }
                self.job_control(t, 22)?;
                self.st.lock().pgrp = g;
                Ok(0)
            }
            TIOCGSID => {
                if !master && !self.is_ctty_of(&t.process) {
                    return Err(ENOTTY);
                }
                let s = self.st.lock().sid;
                if s == 0 {
                    return Err(ENOTTY);
                }
                put_i32(t.process.vnr(s) as i32)
            }
            TIOCSTI => {
                // Injecter une entrée : réservé au terminal de contrôle de l'appelant, sinon
                // `CAP_SYS_ADMIN` dans l'espace initial (`tiocsti`).
                if !self.is_ctty_of(&t.process) && !crate::userns::capable(&t.process.creds.lock(), crate::userns::CAP_SYS_ADMIN) {
                    return Err(EPERM);
                }
                let c = mm.read_user::<u8>(arg as usize).map_err(fault)?;
                let mut sigs = Vec::new();
                {
                    let mut st = self.st.lock();
                    self.receive(&mut st, &[c], &mut sigs);
                }
                let pgrp = self.st.lock().pgrp;
                for s in sigs {
                    signal_pgrp(pgrp, s);
                }
                self.wake();
                Ok(0)
            }
            TIOCVHANGUP => {
                if t.process.creds.lock().euid != 0 {
                    return Err(EPERM);
                }
                self.hangup();
                Ok(0)
            }
            TIOCGPTN if master => put_i32(self.index as i32),
            TIOCSPTLCK if master => {
                let v = mm.read_user::<i32>(arg as usize).map_err(fault)?;
                self.st.lock().locked = v != 0;
                Ok(0)
            }
            TIOCGPTLCK if master => {
                let l = self.st.lock().locked;
                put_i32(l as i32)
            }
            TIOCPKT if master => {
                let v = mm.read_user::<i32>(arg as usize).map_err(fault)?;
                self.st.lock().packet = v != 0;
                Ok(0)
            }
            TIOCGPKT if master => {
                let p = self.st.lock().packet;
                put_i32(p as i32)
            }
            TIOCGPTPEER if master => {
                // `openpty` de la glibc : l'esclave ouvert directement depuis le maître.
                let flags = arg as u32;
                let me = self.self_arc().ok_or(EIO)?;
                let node = open_slave(t, &me, flags)?;
                let f = crate::fd::OpenFile::new(node, flags & (crate::fd::O_ACCMODE | crate::fd::O_NONBLOCK));
                let cloexec = flags & 0o2000000 != 0;
                t.process.files.lock().alloc(f, 0, cloexec).map(|fd| fd as i64).ok_or(EMFILE)
            }
            TIOCGDEV if !master => put_i32(linux_dev(PTS_MAJOR, self.index) as i32),
            TIOCGLCKTRMIOS => {
                mm.copy_to_user(arg as usize, &[0u8; 36]).map_err(fault)?;
                Ok(0)
            }
            TIOCSLCKTRMIOS => {
                if t.process.creds.lock().euid != 0 {
                    return Err(EPERM);
                }
                Ok(0)
            }
            TIOCMGET => Err(ENOTTY),
            _ => Err(ENOTTY),
        }
    }

    fn self_arc(&self) -> Option<Arc<Pty>> {
        self.fs.find(self.index).filter(|p| core::ptr::eq(&**p, self))
    }

    /// Fait du terminal celui de contrôle de la session de l'appelant (`TIOCSCTTY`).
    fn make_ctty(&self, t: &Thread, steal: bool) -> Result<(), i64> {
        let p = &t.process;
        let sid = p.sid.load(core::sync::atomic::Ordering::Relaxed);
        if self.is_ctty_of(p) {
            return Ok(());
        }
        // Chef de session sans terminal de contrôle.
        if sid != p.pid || p.ctty.lock().as_ref().and_then(|w| w.upgrade()).is_some() {
            return Err(EPERM);
        }
        let me = self.self_arc().ok_or(EIO)?;
        {
            let mut st = self.st.lock();
            if st.sid != 0 && st.sid != sid {
                let owner_alive = crate::find_process(st.sid).map_or(false, |o| !o.is_zombie());
                if owner_alive && !(steal && p.creds.lock().euid == 0) {
                    return Err(EPERM);
                }
            }
            st.sid = sid;
            st.pgrp = p.pgid.load(core::sync::atomic::Ordering::Relaxed);
        }
        // Tous les processus de la session partagent ce terminal de contrôle.
        for q in crate::all_processes() {
            if q.sid.load(core::sync::atomic::Ordering::Relaxed) == sid {
                *q.ctty.lock() = Some(Arc::downgrade(&me));
            }
        }
        Ok(())
    }

    /// Raccroche : `SIGHUP` puis `SIGCONT` au chef de session et au groupe de premier plan ;
    /// l'esclave lit la fin de fichier, ses écritures échouent.
    fn hangup(&self) {
        let (sid, pgrp) = {
            let mut st = self.st.lock();
            st.hung_up = true;
            let r = (st.sid, st.pgrp);
            st.sid = 0;
            st.pgrp = 0;
            r
        };
        self.wake();
        if sid != 0 {
            if let Some(leader) = crate::find_process(sid) {
                signal::send_to_process(&leader, SigInfo::new(1, signal::SI_KERNEL));
                signal::send_to_process(&leader, SigInfo::new(18, signal::SI_KERNEL));
            }
            for q in crate::all_processes() {
                if q.sid.load(core::sync::atomic::Ordering::Relaxed) == sid && self.is_ctty_of(&q) {
                    *q.ctty.lock() = None;
                }
            }
        }
        if pgrp != 0 && pgrp != sid {
            signal_pgrp(pgrp, 1);
            signal_pgrp(pgrp, 18);
        }
    }
}

/// Le processus abandonne son terminal de contrôle (`TIOCNOTTY`, sortie d'un chef de session) ;
/// un chef de session le retire à toute sa session, et son groupe de premier plan reçoit
/// `SIGHUP` puis `SIGCONT`.
pub fn disassociate(p: &Process) {
    let Some(tty) = p.ctty.lock().take().and_then(|w| w.upgrade()) else { return };
    let sid = p.sid.load(core::sync::atomic::Ordering::Relaxed);
    if sid != p.pid {
        return;
    }
    let pgrp = {
        let mut st = tty.st.lock();
        if st.sid != sid {
            return;
        }
        let g = st.pgrp;
        st.sid = 0;
        st.pgrp = 0;
        g
    };
    signal_pgrp(pgrp, 1);
    signal_pgrp(pgrp, 18);
    for q in crate::all_processes() {
        if q.sid.load(core::sync::atomic::Ordering::Relaxed) == sid {
            *q.ctty.lock() = None;
        }
    }
}

// --- Objets de fichier --------------------------------------------------------------------------

/// Extrémité maître (`/dev/ptmx`).
pub struct PtyMaster(Arc<Pty>);
/// Une ouverture de l'esclave (`/dev/pts/<n>`).
pub struct PtySlave(Arc<Pty>);

impl Drop for Pty {
    fn drop(&mut self) {
        PTY_COUNT.fetch_sub(1, core::sync::atomic::Ordering::AcqRel);
    }
}

impl Drop for PtyMaster {
    fn drop(&mut self) {
        self.0.st.lock().master_open = false;
        self.0.hangup();
    }
}

impl Drop for PtySlave {
    fn drop(&mut self) {
        {
            let mut st = self.0.st.lock();
            st.slaves -= 1;
        }
        self.0.wake();
    }
}

impl FileObject for PtyMaster {
    object_any!();

    fn link_name(&self) -> String {
        String::from("/dev/ptmx")
    }

    fn stat(&self) -> (u32, u64) {
        (crate::object::S_IFCHR | 0o666, INO_PTMX)
    }

    fn stat_extra(&self) -> (u64, u32, u32) {
        (linux_dev(5, 2), 0, TTY_GID)
    }

    fn read(&self, t: &Thread, kb: &mut [u8], nonblock: bool) -> Result<usize, i64> {
        self.0.master_read(t, kb, nonblock)
    }

    fn write(&self, t: &Thread, kb: &[u8], nonblock: bool) -> Result<usize, i64> {
        self.0.master_write(t, kb, nonblock)
    }

    fn poll(&self, _t: Option<&Thread>) -> u32 {
        let st = self.0.st.lock();
        let mut r = 0;
        if !st.out.is_empty() {
            r |= ev::READABLE;
        }
        if st.input_room() {
            r |= ev::WRITABLE;
        }
        if st.slave_ever && st.slaves == 0 {
            r |= ev::HUP | ev::READABLE;
        }
        r
    }

    fn queues<'a>(&'a self, cb: &mut dyn FnMut(&'a WaitQueue)) {
        cb(&self.0.input_wq);
        cb(&self.0.output_wq);
    }

    fn ioctl(&self, t: &Thread, req: u64, arg: u64) -> SysResult {
        self.0.ioctl(t, true, req, arg)
    }

    fn pending_bytes(&self) -> Option<usize> {
        Some(self.0.st.lock().out.len())
    }

    fn fdinfo(&self) -> String {
        format!("tty-index:\t{}\n", self.0.index)
    }
}

impl FileObject for PtySlave {
    object_any!();

    fn link_name(&self) -> String {
        format!("/dev/pts/{}", self.0.index)
    }

    fn stat(&self) -> (u32, u64) {
        (crate::object::S_IFCHR | self.0.mode, INO_PTS + self.0.index as u64)
    }

    fn stat_extra(&self) -> (u64, u32, u32) {
        (linux_dev(PTS_MAJOR, self.0.index), self.0.owner.0, self.0.owner.1)
    }

    fn read(&self, t: &Thread, kb: &mut [u8], nonblock: bool) -> Result<usize, i64> {
        self.0.slave_read(t, kb, nonblock)
    }

    fn write(&self, t: &Thread, kb: &[u8], nonblock: bool) -> Result<usize, i64> {
        self.0.slave_write(t, kb, nonblock)
    }

    fn poll(&self, _t: Option<&Thread>) -> u32 {
        let st = self.0.st.lock();
        let mut r = 0;
        if st.readable() > 0 {
            r |= ev::READABLE;
        }
        if st.hung_up {
            r |= ev::HUP | ev::READABLE;
        } else if !st.stopped && st.out.len() < OUTPUT_MAX {
            r |= ev::WRITABLE;
        }
        r
    }

    fn queues<'a>(&'a self, cb: &mut dyn FnMut(&'a WaitQueue)) {
        cb(&self.0.input_wq);
        cb(&self.0.output_wq);
    }

    fn ioctl(&self, t: &Thread, req: u64, arg: u64) -> SysResult {
        self.0.ioctl(t, false, req, arg)
    }

    fn pending_bytes(&self) -> Option<usize> {
        let st = self.0.st.lock();
        Some(if st.tio.canon() { st.lines_bytes } else { st.raw.len() })
    }
}

/// Objet de `stat` seul (chemins `/dev/ptmx` et `/dev/pts/<n>` vus sans être ouverts).
struct PtyStat {
    mode: u32,
    ino: u64,
    rdev: u64,
    uid: u32,
    gid: u32,
}

impl FileObject for PtyStat {
    object_any!();

    fn link_name(&self) -> String {
        String::new()
    }

    fn stat(&self) -> (u32, u64) {
        (self.mode, self.ino)
    }

    fn stat_extra(&self) -> (u64, u32, u32) {
        (self.rdev, self.uid, self.gid)
    }

    fn poll(&self, _t: Option<&Thread>) -> u32 {
        0
    }

    fn queues<'a>(&'a self, _cb: &mut dyn FnMut(&'a WaitQueue)) {}
}

// --- Chemins ------------------------------------------------------------------------------------

/// Nœud de devpts désigné par un chemin canonique.
enum PtsPath {
    Root,
    Ptmx,
    Slave(u32),
}

/// `/dev/pts`, `/dev/pts/ptmx`, `/dev/pts/<n>` (sans zéro en tête ni signe, comme les noms que
/// crée devpts).
fn pts_path(c: &str) -> Option<PtsPath> {
    match c.strip_prefix("/dev/pts")? {
        "" | "/" => Some(PtsPath::Root),
        "/ptmx" => Some(PtsPath::Ptmx),
        r => {
            let n = r.strip_prefix('/')?;
            if n.is_empty() || (n.len() > 1 && n.starts_with('0')) || !n.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            n.parse().ok().map(PtsPath::Slave)
        }
    }
}

/// Ce que `stat`, `access`… voient de `/dev/pts/ptmx` et `/dev/pts/<n>` de l'instance `fs`
/// (sans rien ouvrir).
pub fn stat_node(fs: &Arc<PtsFs>, c: &str) -> Option<crate::fd::Node> {
    let o: Arc<dyn FileObject> = match pts_path(c)? {
        PtsPath::Root => return None,
        PtsPath::Ptmx => {
            let (uid, gid) = fs.ptmx_owner();
            Arc::new(PtyStat { mode: crate::object::S_IFCHR | fs.opts.lock().ptmxmode, ino: 2, rdev: linux_dev(5, 2), uid, gid })
        }
        PtsPath::Slave(n) => {
            let p = fs.find(n)?;
            Arc::new(PtyStat { mode: crate::object::S_IFCHR | p.mode, ino: INO_PTS + p.index as u64, rdev: linux_dev(PTS_MAJOR, p.index), uid: p.owner.0, gid: p.owner.1 })
        }
    };
    Some(crate::fd::Node::Object(o))
}

/// Ouvre un esclave ; sans `O_NOCTTY`, un chef de session sans terminal de contrôle l'acquiert.
fn open_slave(t: &Thread, p: &Arc<Pty>, flags: u32) -> Result<crate::fd::Node, i64> {
    const O_NOCTTY: u32 = 0o400;
    {
        let mut st = p.st.lock();
        if st.locked || !st.master_open {
            return Err(EIO);
        }
        if st.exclusive && t.process.creds.lock().euid != 0 {
            return Err(EBUSY);
        }
        st.slaves += 1;
        st.slave_ever = true;
        st.hung_up = false;
    }
    let node = crate::fd::Node::Object(Arc::new(PtySlave(p.clone())));
    if flags & O_NOCTTY == 0 {
        let pr = &t.process;
        if pr.sid.load(core::sync::atomic::Ordering::Relaxed) == pr.pid && pr.ctty.lock().as_ref().and_then(|w| w.upgrade()).is_none() && p.st.lock().sid == 0 {
            let _ = p.make_ctty(t, false);
        }
    }
    Ok(node)
}

/// Nouveau terminal de l'instance `fs` (`ptmx_open`) : rend le maître.
pub fn open_ptmx(t: &Thread, fs: &Arc<PtsFs>) -> Result<crate::fd::Node, i64> {
    let c = t.process.creds.lock();
    let creator = (c.fsuid, c.fsgid);
    drop(c);
    Ok(crate::fd::Node::Object(Arc::new(PtyMaster(Pty::new(fs, creator)?))))
}

/// `open` du nœud de devpts `c` de l'instance `fs` : `ptmx` (droits `ptmxmode`) crée un
/// terminal, `<n>` ouvre l'esclave (droits de l'esclave) ; `None` si le nœud n'existe pas.
pub fn open_pts(t: &Thread, fs: &Arc<PtsFs>, c: &str, flags: u32) -> Option<Result<crate::fd::Node, i64>> {
    match pts_path(c)? {
        PtsPath::Root => None,
        PtsPath::Ptmx => {
            let (uid, gid) = fs.ptmx_owner();
            let mode = fs.opts.lock().ptmxmode;
            Some(may_open(t, mode, uid, gid, flags).and_then(|_| open_ptmx(t, fs)))
        }
        PtsPath::Slave(n) => {
            let p = fs.find(n)?;
            Some(may_open(t, p.mode, p.owner.0, p.owner.1, flags).and_then(|_| open_slave(t, &p, flags)))
        }
    }
}

/// `/dev/tty` d'un processus dont le terminal de contrôle est un pseudo-terminal.
pub fn open_ctty(t: &Thread, flags: u32) -> Option<Result<crate::fd::Node, i64>> {
    let p = t.process.ctty.lock().as_ref().and_then(|w| w.upgrade())?;
    Some(open_slave(t, &p, flags | 0o400))
}

/// Esclave `n` de l'instance initiale, ouvert par un nœud 136:n créé hors de devpts (`mknod`).
pub fn open_init_slave(t: &Thread, n: u32, flags: u32) -> Option<Result<crate::fd::Node, i64>> {
    let p = init_pts().find(n)?;
    Some(open_slave(t, &p, flags))
}

/// Entrées de `/dev/pts` de l'instance `fs`.
pub fn dir_entries(fs: &Arc<PtsFs>) -> Vec<(String, u8)> {
    const DT_CHR: u8 = 2;
    let mut v: Vec<(String, u8)> = fs.indices().into_iter().map(|i| (format!("{}", i), DT_CHR)).collect();
    v.push((String::from("ptmx"), DT_CHR));
    v
}
