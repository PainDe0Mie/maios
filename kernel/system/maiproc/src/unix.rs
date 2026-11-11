//! Sockets du domaine Unix (`AF_UNIX`) : flux, datagrammes et paquets ordonnés
//! (`SOCK_SEQPACKET`), adresses par chemin, abstraites (`\0nom`) ou automatiques, passage de
//! descripteurs (`SCM_RIGHTS`) et d'identités (`SCM_CREDENTIALS`, `SO_PEERCRED`).
//!
//! Modèle de Linux (`net/unix/af_unix.c`) : chaque socket a une file de réception de messages
//! ; un envoi dépose directement dans la file du destinataire (pas de tampon d'émission), borné
//! par le `SO_RCVBUF` de celui-ci. Les données d'un flux se lisent d'un seul tenant à travers les
//! messages, mais une lecture s'arrête après un message qui portait des descripteurs (pour ne
//! pas mélanger les données ancillaires de deux envois).
//!
//! Règles de verrouillage : jamais deux verrous d'état de socket tenus en même temps (on prend
//! la référence du pair, on relâche, puis on verrouille le pair). Les attentes se font sur la
//! file de la socket elle-même : un lecteur réveille la file de son pair quand il libère de la
//! place, un écrivain celle du destinataire quand il dépose des données.

use alloc::{
    collections::{BTreeMap, VecDeque},
    format,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use sync_preemption::PreemptionSafeMutex as Mutex;

use crate::fd::OpenFile;
use crate::linux::errno::*;
use crate::object::{ev, new_ino, object_any, FileObject, S_IFSOCK};
use crate::socket::*;
use crate::wait::{wait_event, WaitError, WaitQueue, Watcher};
use crate::Thread;

/// Plus grand `backlog` (`net.core.somaxconn`).
const SOMAXCONN: usize = 4096;
/// Longueur de `sun_path`.
const SUN_PATH: usize = 108;

/// Adresse d'une socket Unix.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum UnixAddr {
    /// Chemin absolu dans le VFS.
    Path(String),
    /// Espace abstrait (nom sans le `\0` initial).
    Abstract(Vec<u8>),
}

impl UnixAddr {
    /// `struct sockaddr_un` (longueur exacte, comme Linux).
    fn encode(addr: Option<&UnixAddr>) -> Vec<u8> {
        let mut v = (AF_UNIX as u16).to_le_bytes().to_vec();
        match addr {
            None => {}
            Some(UnixAddr::Path(p)) => {
                v.extend_from_slice(&fs_node::rawname::encode(p));
                v.push(0);
            }
            Some(UnixAddr::Abstract(n)) => {
                v.push(0);
                v.extend_from_slice(n);
            }
        }
        v
    }

    /// Décode un `sockaddr_un` ; `Ok(None)` : adresse vide (liaison automatique).
    fn decode(t: &Thread, raw: &[u8]) -> Result<Option<UnixAddr>, i64> {
        if raw.len() < 2 {
            return Err(EINVAL);
        }
        if u16::from_le_bytes([raw[0], raw[1]]) as i32 != AF_UNIX {
            return Err(EINVAL);
        }
        let path = &raw[2..raw.len().min(2 + SUN_PATH)];
        if path.is_empty() {
            return Ok(None);
        }
        if path[0] == 0 {
            return Ok(Some(UnixAddr::Abstract(path[1..].to_vec())));
        }
        let end = path.iter().position(|&b| b == 0).unwrap_or(path.len());
        let s = fs_node::rawname::decode(&path[..end]);
        let abs = crate::linux::fs::absolute(t, crate::linux::fs::AT_FDCWD, &s)?;
        Ok(Some(UnixAddr::Path(abs)))
    }
}

/// Clé d'une socket liée : l'inode du fichier de socket (comme Linux, qui lie la socket au
/// nœud et non au nom : un conteneur qui montre la socket ailleurs la joint quand même, même
/// depuis un autre espace réseau), ou le nom abstrait **dans son espace réseau** (numéro de
/// l'espace, nom), propre à chaque espace comme `unix_find_abstract` (`net_eq`).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum BindKey {
    Inode(usize),
    Abstract(u64, Vec<u8>),
}

fn node_key(f: &fs_node::FileRef) -> usize {
    Arc::as_ptr(f) as *const () as usize
}

/// Sockets liées.
static BOUND: Mutex<BTreeMap<BindKey, Weak<UnixSocket>>> = Mutex::new(BTreeMap::new());

/// Vrai si `node` est le fichier d'une socket liée (pour `stat` : `S_IFSOCK`).
pub fn is_socket_node(node: &fs_node::FileOrDir) -> bool {
    let fs_node::FileOrDir::File(f) = node else { return false };
    BOUND.lock().get(&BindKey::Inode(node_key(f))).map_or(false, |w| w.strong_count() > 0)
}

/// Racine depuis laquelle résoudre un chemin de socket : celle du processus, ou celle de
/// l'espace initial pour un service du noyau.
fn root_of(t: Option<&Thread>) -> crate::mount::Loc {
    match t {
        Some(t) => t.process.fs.root(),
        None => crate::mount::init_root(),
    }
}

/// Fichier de socket désigné par `path`.
fn socket_file(root: &crate::mount::Loc, path: &str) -> Result<fs_node::FileRef, i64> {
    match crate::mount::resolve(root, path, true)?.target {
        crate::mount::Target::Node(fs_node::FileOrDir::File(f)) => Ok(f),
        _ => Err(ECONNREFUSED),
    }
}

struct Msg {
    data: Vec<u8>,
    /// Octets déjà lus (flux).
    off: usize,
    fds: Vec<Arc<OpenFile>>,
    cred: Ucred,
    from: Option<UnixAddr>,
}

enum Conn {
    Unconnected,
    Listening { backlog: VecDeque<Arc<UnixSocket>>, max: usize, cred: Ucred },
    /// Pair (flux) ou destination par défaut (datagrammes).
    Connected(Weak<UnixSocket>),
}

struct State {
    local: Option<UnixAddr>,
    /// Clé de liaison, pour la socket qui a fait `bind` (pas celles nées d'un `accept`).
    key: Option<BindKey>,
    conn: Conn,
    rx: VecDeque<Msg>,
    rx_bytes: usize,
    shut_rd: bool,
    shut_wr: bool,
    /// Le pair n'écrira plus (fermeture ou `SHUT_WR`) : fin de fichier une fois `rx` vidée.
    eof: bool,
    /// Le pair a disparu (fermé) : `EPIPE` en écriture.
    peer_gone: bool,
    error: i64,
    peer_cred: Option<Ucred>,
}

/// Deux objets créés ensemble (`socketpair`).
pub type ObjectPair = (Arc<dyn FileObject>, Arc<dyn FileObject>);

/// Socket `AF_UNIX`.
pub struct UnixSocket {
    ty: i32,
    /// Espace réseau de naissance (`sock_net`) : espace des noms abstraits.
    net: Arc<crate::netns::NetNs>,
    st: Mutex<State>,
    wq: WaitQueue,
    opts: Mutex<SockOpts>,
    ino: u64,
    me: Weak<UnixSocket>,
}

impl UnixSocket {
    fn new(net: Arc<crate::netns::NetNs>, ty: i32) -> Arc<UnixSocket> {
        Arc::new_cyclic(|me| UnixSocket {
            ty,
            net,
            st: Mutex::new(State {
                local: None,
                key: None,
                conn: Conn::Unconnected,
                rx: VecDeque::new(),
                rx_bytes: 0,
                shut_rd: false,
                shut_wr: false,
                eof: false,
                peer_gone: false,
                error: 0,
                peer_cred: None,
            }),
            wq: WaitQueue::new(),
            opts: Mutex::new(SockOpts::default()),
            ino: new_ino(),
            me: me.clone(),
        })
    }

    pub fn create(net: Arc<crate::netns::NetNs>, ty: i32) -> Result<Arc<UnixSocket>, i64> {
        match ty {
            SOCK_STREAM | SOCK_DGRAM | SOCK_SEQPACKET => Ok(Self::new(net, ty)),
            SOCK_RAW => Err(EPROTONOSUPPORT),
            _ => Err(ESOCKTNOSUPPORT),
        }
    }

    /// `socketpair` : deux sockets reliées.
    pub fn pair(t: &Thread, ty: i32) -> Result<ObjectPair, i64> {
        let net = crate::netns::of(&t.process);
        let a = Self::create(net.clone(), ty)?;
        let b = Self::create(net, ty)?;
        let cred = Ucred::of(t);
        {
            let mut sa = a.st.lock();
            sa.conn = Conn::Connected(Arc::downgrade(&b));
            sa.peer_cred = Some(cred);
        }
        {
            let mut sb = b.st.lock();
            sb.conn = Conn::Connected(Arc::downgrade(&a));
            sb.peer_cred = Some(cred);
        }
        Ok((a, b))
    }

    fn seqpacket(&self) -> bool {
        self.ty == SOCK_SEQPACKET
    }

    fn connection_oriented(&self) -> bool {
        self.ty != SOCK_DGRAM
    }

    fn peer(&self) -> Option<Arc<UnixSocket>> {
        match &self.st.lock().conn {
            Conn::Connected(w) => w.upgrade(),
            _ => None,
        }
    }

    fn rcvbuf(&self) -> usize {
        self.opts.lock().rcvbuf
    }

    /// Libère `n` octets de la file : réveille les écrivains (la file du pair, ou tous ceux qui
    /// visent cette socket pour les datagrammes).
    fn consumed(&self, n: usize) {
        if n == 0 {
            return;
        }
        if let Some(p) = self.peer() {
            p.wq.wake_all();
        }
        // Datagrammes non connectés : les écrivains attendent sur notre file.
        self.wq.wake_all();
    }

    /// Lie à `addr` (ou à une adresse abstraite automatique).
    fn bind_to(&self, t: Option<&Thread>, addr: Option<UnixAddr>) -> Result<(), i64> {
        if self.st.lock().local.is_some() {
            return Err(EINVAL);
        }
        // Chemin : le fichier de socket est créé hors de tout verrou (il ne doit pas exister).
        let made = match &addr {
            Some(UnixAddr::Path(p)) => {
                let root = root_of(t);
                if crate::mount::resolve(&root, p, false).is_ok() {
                    return Err(EADDRINUSE);
                }
                let (dir, name) = p.rsplit_once('/').ok_or(EINVAL)?;
                let dir = if dir.is_empty() { "/" } else { dir };
                let f = crate::mount::resolve(&root, dir, true)?;
                let parent = match f.target {
                    crate::mount::Target::Node(fs_node::FileOrDir::Dir(d)) => d,
                    crate::mount::Target::Node(_) => return Err(ENOTDIR),
                    crate::mount::Target::Virtual(..) => return Err(EACCES),
                };
                if f.mnt.readonly() {
                    return Err(EROFS);
                }
                if let Some(t) = t {
                    crate::linux::fs::check_access(t, &fs_node::FileOrDir::Dir(parent.clone()), crate::creds::MAY_WRITE | crate::creds::MAY_EXEC)?;
                }
                let perm = 0o777 & !t.map_or(0, |t| t.process.umask.load(core::sync::atomic::Ordering::Relaxed)) as u16;
                // Nœud de socket du système de fichiers (`S_IFSOCK`), sinon fichier vide.
                let special = parent.lock().create_special(name, fs_node::NodeKind::Socket, perm, (0, 0));
                let file = match special {
                    Some(r) => r.map_err(|_| EADDRINUSE)?,
                    None => memfs::MemFile::create(String::from(name), &parent).map_err(|_| EACCES)?,
                };
                if let Some(t) = t {
                    let (uid, gid) = {
                        let c = t.process.creds.lock();
                        (c.fsuid, c.fsgid)
                    };
                    let _ = fs_node::FsNode::set_metadata(&mut fs_node::FileOrDir::File(file.clone()), fs_node::MetaChange::Owner { uid: Some(uid), gid: Some(gid) });
                }
                Some(file)
            }
            _ => None,
        };
        let mut st = self.st.lock();
        if st.local.is_some() {
            return Err(EINVAL);
        }
        let mut bound = BOUND.lock();
        let addr = match addr {
            Some(a) => a,
            None => {
                // Liaison automatique : 5 chiffres hexadécimaux dans l'espace abstrait.
                let mut found = None;
                for _ in 0..64 {
                    let n = (uvm::rand::random_u64() & 0xfffff) as u32;
                    let a = UnixAddr::Abstract(format!("{:05x}", n).into_bytes());
                    if bound.get(&BindKey::Abstract(self.net.ino, format!("{:05x}", n).into_bytes())).map_or(true, |w| w.strong_count() == 0) {
                        found = Some(a);
                        break;
                    }
                }
                found.ok_or(ENOSPC)?
            }
        };
        let key = match (&addr, &made) {
            (UnixAddr::Path(_), Some(f)) => BindKey::Inode(node_key(f)),
            (UnixAddr::Abstract(n), _) => BindKey::Abstract(self.net.ino, n.clone()),
            _ => return Err(EINVAL),
        };
        if bound.get(&key).map_or(false, |w| w.strong_count() > 0) {
            return Err(EADDRINUSE);
        }
        bound.insert(key.clone(), self.me.clone());
        st.local = Some(addr);
        st.key = Some(key);
        Ok(())
    }

    /// Socket liée à `addr`, pour s'y connecter ou lui envoyer.
    fn find(&self, t: &Thread, addr: &UnixAddr) -> Result<Arc<UnixSocket>, i64> {
        let key = match addr {
            // Fichier retiré (`unlink`) : le nom n'est plus joignable.
            UnixAddr::Path(p) => {
                let root = root_of(Some(t));
                let f = socket_file(&root, p)?;
                crate::linux::fs::check_access(t, &fs_node::FileOrDir::File(f.clone()), crate::creds::MAY_WRITE)?;
                BindKey::Inode(node_key(&f))
            }
            UnixAddr::Abstract(n) => BindKey::Abstract(self.net.ino, n.clone()),
        };
        BOUND.lock().get(&key).and_then(|w| w.upgrade()).ok_or(ECONNREFUSED)
    }

    /// Connexion d'un flux à un serveur.
    fn connect_stream(&self, t: &Thread, addr: &UnixAddr, nonblock: bool) -> Result<(), i64> {
        {
            let st = self.st.lock();
            match st.conn {
                Conn::Connected(_) => return Err(EISCONN),
                Conn::Listening { .. } => return Err(EINVAL),
                Conn::Unconnected => {}
            }
        }
        let server = self.find(t, addr)?;
        if server.ty != self.ty {
            return Err(EPROTOTYPE);
        }
        let me = self.me.upgrade().ok_or(EINVAL)?;
        // Socket du côté serveur, prête avant d'entrer dans la file d'attente du serveur.
        let child = Self::new(self.net.clone(), self.ty);
        {
            let mut cs = child.st.lock();
            cs.conn = Conn::Connected(Arc::downgrade(&me));
            cs.peer_cred = Some(Ucred::of(t));
        }
        let deadline = self.opts.lock().snd_deadline();
        // Place dans la file d'attente du serveur (attente si elle est pleine). Un seul verrou de
        // socket à la fois : celui du serveur ici, le nôtre après.
        let r = wait_event(&[&server.wq], deadline, Some(t), || {
            let mut ss = server.st.lock();
            let local = ss.local.clone();
            let Conn::Listening { backlog, max, cred } = &mut ss.conn else {
                return Some(Err(ECONNREFUSED));
            };
            if backlog.len() >= *max {
                return if nonblock { Some(Err(EAGAIN)) } else { None };
            }
            let server_cred = *cred;
            backlog.push_back(child.clone());
            Some(Ok((server_cred, local)))
        });
        let res = match r {
            Ok(Ok((server_cred, local))) => {
                child.st.lock().local = local;
                {
                    let mut ms = self.st.lock();
                    ms.conn = Conn::Connected(Arc::downgrade(&child));
                    ms.peer_cred = Some(server_cred);
                }
                server.wq.wake_all();
                Ok(())
            }
            Ok(Err(e)) => Err(e),
            Err(WaitError::TimedOut) => Err(EAGAIN),
            Err(WaitError::Interrupted) => Err(crate::signal::ERESTARTSYS),
        };
        if res.is_err() {
            // La socket du serveur disparaît sans avoir été reliée : son `Drop` ne doit pas
            // marquer la nôtre comme raccrochée.
            child.st.lock().conn = Conn::Unconnected;
        }
        res
    }

    /// Envoi sur une connexion (flux, paquets).
    fn send_connected(&self, t: &Thread, mut m: SendMsg, nonblock: bool) -> Result<usize, i64> {
        let len = m.data.len();
        let atomic = self.seqpacket();
        if atomic && len > self.opts.lock().sndbuf.max(DEFAULT_BUF) * 4 {
            return Err(EMSGSIZE);
        }
        if len == 0 && !atomic {
            return Ok(0);
        }
        let deadline = self.opts.lock().snd_deadline();
        let mut done = 0usize;
        let mut fds = core::mem::take(&mut m.fds);
        while done < len || (atomic && done == 0) {
            let step = wait_event(&[&self.wq], deadline, Some(t), || {
                let (peer, shut, gone) = {
                    let st = self.st.lock();
                    let p = match &st.conn {
                        Conn::Connected(w) => w.upgrade(),
                        _ => return Some(Err(ENOTCONN)),
                    };
                    (p, st.shut_wr, st.peer_gone)
                };
                if shut || gone {
                    return Some(Err(EPIPE));
                }
                let Some(peer) = peer else { return Some(Err(EPIPE)) };
                let cap = peer.rcvbuf();
                let mut ps = peer.st.lock();
                if ps.shut_rd {
                    return Some(Err(EPIPE));
                }
                let free = cap.saturating_sub(ps.rx_bytes);
                let rest = len - done;
                // Un message au moins passe toujours dans une file vide (pas d'interblocage).
                let fits = if atomic { free >= rest || ps.rx.is_empty() } else { free > 0 || ps.rx.is_empty() };
                if !fits {
                    return if nonblock { Some(Err(EAGAIN)) } else { None };
                }
                let n = if atomic { rest } else { rest.min(free.max(4096)) };
                ps.rx.push_back(Msg { data: m.data[done..done + n].to_vec(), off: 0, fds: core::mem::take(&mut fds), cred: m.cred, from: None });
                ps.rx_bytes += n;
                drop(ps);
                peer.wq.wake_all();
                Some(Ok(n))
            });
            match step {
                Ok(Ok(n)) => {
                    done += n;
                    if atomic {
                        break;
                    }
                }
                Ok(Err(EPIPE)) => {
                    if m.flags & MSG_NOSIGNAL == 0 {
                        if let Some(me) = crate::current_thread_arc(t) {
                            crate::signal::send_to_thread(
                                &me,
                                crate::signal::SigInfo::from_process(crate::signal::SIGPIPE, crate::signal::SI_USER, &t.process, &t.process),
                                false,
                            );
                        }
                    }
                    return if done > 0 { Ok(done) } else { Err(EPIPE) };
                }
                Ok(Err(e)) => return if done > 0 { Ok(done) } else { Err(e) },
                Err(WaitError::TimedOut) => return if done > 0 { Ok(done) } else { Err(EAGAIN) },
                Err(WaitError::Interrupted) => return if done > 0 { Ok(done) } else { Err(crate::signal::ERESTARTSYS) },
            }
        }
        Ok(done)
    }

    /// Envoi d'un datagramme.
    fn send_dgram(&self, t: &Thread, m: SendMsg, nonblock: bool) -> Result<usize, i64> {
        let target = match &m.addr {
            Some(raw) => {
                let a = UnixAddr::decode(t, raw)?.ok_or(EINVAL)?;
                let s = self.find(t, &a)?;
                if s.ty != SOCK_DGRAM {
                    return Err(EPROTOTYPE);
                }
                s
            }
            None => match &self.st.lock().conn {
                Conn::Connected(w) => w.upgrade().ok_or(ECONNREFUSED)?,
                _ => return Err(ENOTCONN),
            },
        };
        if self.st.lock().shut_wr {
            return Err(EPIPE);
        }
        let len = m.data.len();
        if len > self.opts.lock().sndbuf.max(DEFAULT_BUF) {
            return Err(EMSGSIZE);
        }
        let from = self.st.lock().local.clone();
        let deadline = self.opts.lock().snd_deadline();
        let mut msg = Some(Msg { data: m.data, off: 0, fds: m.fds, cred: m.cred, from });
        let cap = target.rcvbuf();
        // Le destinataire réveille sa propre file en lisant : on y attend.
        let r = wait_event(&[&target.wq], deadline, Some(t), || {
            let mut ts = target.st.lock();
            if ts.shut_rd {
                return Some(Err(ECONNREFUSED));
            }
            if ts.rx_bytes + len > cap && !ts.rx.is_empty() {
                return if nonblock { Some(Err(EAGAIN)) } else { None };
            }
            let msg = msg.take()?;
            ts.rx_bytes += len;
            ts.rx.push_back(msg);
            Some(Ok(()))
        });
        match r {
            Ok(Ok(())) => {
                target.wq.wake_all();
                Ok(len)
            }
            Ok(Err(e)) => Err(e),
            Err(WaitError::TimedOut) => Err(EAGAIN),
            Err(WaitError::Interrupted) => Err(crate::signal::ERESTARTSYS),
        }
    }

    /// Réception sur un flux : lit à travers les messages.
    fn recv_stream(&self, buf: &mut [u8], flags: u32, st: &mut State, meta: &mut RecvMeta) -> usize {
        let peek = flags & MSG_PEEK != 0;
        let mut got = 0usize;
        let mut idx = 0usize;
        while got < buf.len() && idx < st.rx.len() {
            let msg = &mut st.rx[idx];
            if got > 0 && !msg.fds.is_empty() {
                break; // les descripteurs du message suivant attendent la prochaine lecture
            }
            if got == 0 {
                meta.cred = Some(msg.cred);
                if peek {
                    meta.fds = msg.fds.clone();
                } else {
                    meta.fds = core::mem::take(&mut msg.fds);
                }
            }
            let avail = msg.data.len() - msg.off;
            let n = avail.min(buf.len() - got);
            buf[got..got + n].copy_from_slice(&msg.data[msg.off..msg.off + n]);
            got += n;
            let had_fds = !meta.fds.is_empty();
            if peek {
                idx += 1;
            } else {
                msg.off += n;
                if msg.off == msg.data.len() {
                    st.rx.pop_front();
                }
            }
            if had_fds {
                break;
            }
        }
        if !peek {
            st.rx_bytes -= got.min(st.rx_bytes);
        }
        got
    }
}

impl Drop for UnixSocket {
    fn drop(&mut self) {
        let (peer, local) = {
            let mut st = self.st.lock();
            let peer = match core::mem::replace(&mut st.conn, Conn::Unconnected) {
                Conn::Connected(w) => w.upgrade(),
                _ => None,
            };
            st.local = None;
            (peer, st.key.take())
        };
        if let Some(p) = peer {
            if self.connection_oriented() {
                let mut ps = p.st.lock();
                ps.eof = true;
                ps.peer_gone = true;
            }
            p.wq.wake_all();
        }
        if let Some(a) = local {
            let mut bound: sync_preemption::PreemptionSafeMutexGuard<'_, BTreeMap<BindKey, Weak<UnixSocket>>> = BOUND.lock();
            if bound.get(&a).map_or(false, |w| core::ptr::eq(w.as_ptr(), self as *const UnixSocket)) {
                bound.remove(&a);
            }
        }
    }
}

// --- Extrémité noyau ----------------------------------------------------------------------------
//
// Un service du noyau (le serveur Wayland MaiLand) écoute sur un chemin comme un programme, mais
// sans appel système : il prend les messages et leurs descripteurs directement dans la file de
// réception, et dépose ses réponses dans celle du client. Il n'attend jamais : il s'inscrit
// comme observateur ([`Watcher`]) des files des sockets et se fait réveiller.

/// Socket d'écoute d'un service du noyau.
pub struct KernelListener {
    sock: Arc<UnixSocket>,
}

impl KernelListener {
    /// Lie une socket de flux à `path` (qui ne doit pas exister) et écoute. `cred` est l'identité
    /// rendue aux clients par `SO_PEERCRED`.
    pub fn bind(path: &str, cred: Ucred, backlog: usize) -> Result<KernelListener, i64> {
        let sock = UnixSocket::new(crate::netns::init_net(), SOCK_STREAM);
        sock.bind_to(None, Some(UnixAddr::Path(String::from(path))))?;
        sock.st.lock().conn = Conn::Listening { backlog: VecDeque::new(), max: backlog.clamp(1, SOMAXCONN), cred };
        Ok(KernelListener { sock })
    }

    /// Connexion en attente, sans bloquer.
    pub fn accept(&self) -> Option<KernelConn> {
        let child = {
            let mut st = self.sock.st.lock();
            let Conn::Listening { backlog, .. } = &mut st.conn else { return None };
            backlog.pop_front()?
        };
        // Place libérée dans la file d'attente : connecteurs bloqués.
        self.sock.wq.wake_all();
        Some(KernelConn { sock: child })
    }

    /// Prévient `w` à chaque nouvelle connexion.
    pub fn watch(&self, w: Arc<dyn Watcher>) {
        self.sock.wq.watch(w);
    }

    pub fn unwatch(&self, w: &Arc<dyn Watcher>) {
        self.sock.wq.unwatch(w);
    }

    /// Reprend la socket d'écoute qu'un client a passée par descripteur
    /// (`wp_security_context_manager_v1.create_listener`) : le service accepte désormais ses
    /// connexions à la place du client. `None` : pas une socket Unix de flux qui écoute.
    pub fn adopt(f: &OpenFile) -> Option<KernelListener> {
        let sock = crate::object::downcast_arc::<UnixSocket>(f.object()?)?;
        if sock.ty != SOCK_STREAM || !matches!(sock.st.lock().conn, Conn::Listening { .. }) {
            return None;
        }
        Some(KernelListener { sock })
    }
}

/// Réception d'une extrémité noyau.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelRecv {
    /// Octets copiés.
    Data(usize),
    /// Rien pour l'instant.
    Empty,
    /// Le client n'écrira plus (fermeture ou `SHUT_WR`), file vide.
    Eof,
}

/// Connexion acceptée par un service du noyau (côté serveur d'un flux).
pub struct KernelConn {
    sock: Arc<UnixSocket>,
}

impl KernelConn {
    /// Copie dans `buf` les octets reçus, sans bloquer ; les descripteurs d'un message vont dans
    /// `fds` dès que son premier octet est pris (comme `recvmsg`, qui les livre avec le premier
    /// octet). Réveille le client s'il attendait de la place.
    pub fn recv(&self, buf: &mut [u8], fds: &mut Vec<Arc<OpenFile>>) -> KernelRecv {
        let (got, eof) = {
            let mut st = self.sock.st.lock();
            let mut got = 0usize;
            while got < buf.len() {
                let Some(msg) = st.rx.front_mut() else { break };
                fds.append(&mut msg.fds);
                let n = (msg.data.len() - msg.off).min(buf.len() - got);
                buf[got..got + n].copy_from_slice(&msg.data[msg.off..msg.off + n]);
                msg.off += n;
                got += n;
                if msg.off == msg.data.len() {
                    st.rx.pop_front();
                }
            }
            st.rx_bytes -= got.min(st.rx_bytes);
            (got, st.rx.is_empty() && (st.eof || st.shut_rd))
        };
        if got > 0 {
            self.sock.consumed(got);
            KernelRecv::Data(got)
        } else if eof {
            KernelRecv::Eof
        } else {
            KernelRecv::Empty
        }
    }

    /// Place libre dans la file du client (`usize::MAX` si elle est vide : un message y entre
    /// toujours). `EPIPE` : le client est parti.
    pub fn send_room(&self) -> Result<usize, i64> {
        let peer = {
            let st = self.sock.st.lock();
            if st.peer_gone || st.shut_wr {
                return Err(EPIPE);
            }
            match &st.conn {
                Conn::Connected(w) => w.upgrade().ok_or(EPIPE)?,
                _ => return Err(EPIPE),
            }
        };
        let cap = peer.rcvbuf();
        let ps = peer.st.lock();
        if ps.shut_rd {
            return Err(EPIPE);
        }
        Ok(if ps.rx.is_empty() { usize::MAX } else { cap.saturating_sub(ps.rx_bytes) })
    }

    /// Dépose `data` (et ses descripteurs) dans la file du client, d'un seul message, sans
    /// attendre ni vérifier la place (voir [`send_room`](Self::send_room) : seul le serveur écrit
    /// dans cette file, et le client ne fait qu'en libérer). Jamais de `SIGPIPE`.
    pub fn send(&self, data: Vec<u8>, fds: Vec<Arc<OpenFile>>, cred: Ucred) -> Result<(), i64> {
        let peer = match &self.sock.st.lock().conn {
            Conn::Connected(w) => w.upgrade().ok_or(EPIPE)?,
            _ => return Err(EPIPE),
        };
        {
            let mut ps = peer.st.lock();
            if ps.shut_rd {
                return Err(EPIPE);
            }
            ps.rx_bytes += data.len();
            ps.rx.push_back(Msg { data, off: 0, fds, cred, from: None });
        }
        peer.wq.wake_all();
        Ok(())
    }

    /// Identité du client au moment du `connect`.
    pub fn peer_cred(&self) -> Option<Ucred> {
        self.sock.st.lock().peer_cred
    }

    /// Prévient `w` quand des octets arrivent, quand le client libère de la place dans sa file,
    /// et quand il raccroche.
    pub fn watch(&self, w: Arc<dyn Watcher>) {
        self.sock.wq.watch(w);
    }

    /// Retire l'observateur `w`.
    pub fn unwatch(&self, w: &Arc<dyn Watcher>) {
        self.sock.wq.unwatch(w);
    }

    /// Ferme la connexion (le client lit la fin de fichier, puis `EPIPE` en écriture).
    pub fn shutdown(&self) {
        let _ = SocketOps::shutdown(&*self.sock, SHUT_RDWR);
    }
}

impl SocketOps for UnixSocket {
    fn domain(&self) -> i32 {
        AF_UNIX
    }

    fn sock_net(&self) -> Option<Arc<crate::netns::NetNs>> {
        Some(self.net.clone())
    }

    fn sock_type(&self) -> i32 {
        self.ty
    }

    fn opts(&self) -> &Mutex<SockOpts> {
        &self.opts
    }

    fn bind(&self, t: &Thread, addr: &[u8]) -> Result<(), i64> {
        let a = UnixAddr::decode(t, addr)?;
        self.bind_to(Some(t), a)
    }

    fn connect(&self, t: &Thread, addr: &[u8], nonblock: bool) -> Result<(), i64> {
        // `AF_UNSPEC` : rompt l'association d'un datagramme.
        if addr.len() >= 2 && u16::from_le_bytes([addr[0], addr[1]]) == 0 && !self.connection_oriented() {
            self.st.lock().conn = Conn::Unconnected;
            return Ok(());
        }
        let a = UnixAddr::decode(t, addr)?.ok_or(EINVAL)?;
        if self.connection_oriented() {
            return self.connect_stream(t, &a, nonblock);
        }
        let target = self.find(t, &a)?;
        if target.ty != SOCK_DGRAM {
            return Err(EPROTOTYPE);
        }
        self.st.lock().conn = Conn::Connected(Arc::downgrade(&target));
        Ok(())
    }

    fn listen(&self, t: &Thread, backlog: i32) -> Result<(), i64> {
        if !self.connection_oriented() {
            return Err(EOPNOTSUPP);
        }
        let max = (backlog.max(0) as usize).clamp(1, SOMAXCONN);
        let mut st = self.st.lock();
        if st.local.is_none() {
            return Err(EINVAL);
        }
        match &mut st.conn {
            Conn::Listening { max: m, .. } => *m = max,
            Conn::Connected(_) => return Err(EINVAL),
            Conn::Unconnected => st.conn = Conn::Listening { backlog: VecDeque::new(), max, cred: Ucred::of(t) },
        }
        drop(st);
        self.wq.wake_all();
        Ok(())
    }

    fn accept(&self, t: &Thread, nonblock: bool) -> Result<(Arc<dyn FileObject>, Vec<u8>), i64> {
        if !self.connection_oriented() {
            return Err(EOPNOTSUPP);
        }
        let deadline = self.opts.lock().rcv_deadline();
        let r = wait_event(&[&self.wq], deadline, Some(t), || {
            let mut st = self.st.lock();
            let Conn::Listening { backlog, .. } = &mut st.conn else {
                return Some(Err(EINVAL));
            };
            match backlog.pop_front() {
                Some(c) => Some(Ok(c)),
                None if nonblock => Some(Err(EAGAIN)),
                None => None,
            }
        });
        let child = match r {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => return Err(e),
            Err(WaitError::TimedOut) => return Err(EAGAIN),
            Err(WaitError::Interrupted) => return Err(crate::signal::ERESTARTSYS),
        };
        // Place libérée dans la file d'attente : connecteurs bloqués.
        self.wq.wake_all();
        let peer_addr = child.peer().and_then(|p| p.st.lock().local.clone());
        Ok((child, UnixAddr::encode(peer_addr.as_ref())))
    }

    fn send(&self, t: &Thread, m: SendMsg, nonblock: bool) -> Result<usize, i64> {
        if self.connection_oriented() {
            if m.addr.is_some() && !matches!(self.st.lock().conn, Conn::Connected(_)) {
                return Err(EOPNOTSUPP);
            }
            self.send_connected(t, m, nonblock)
        } else {
            self.send_dgram(t, m, nonblock)
        }
    }

    fn recv(&self, t: &Thread, buf: &mut [u8], flags: u32, nonblock: bool) -> Result<RecvMeta, i64> {
        if flags & MSG_OOB != 0 {
            return Err(EOPNOTSUPP);
        }
        let deadline = self.opts.lock().rcv_deadline();
        let waitall = flags & MSG_WAITALL != 0 && self.ty == SOCK_STREAM && flags & MSG_PEEK == 0;
        let mut meta = RecvMeta::default();
        let mut total = 0usize;
        loop {
            let r = wait_event(&[&self.wq], deadline, Some(t), || {
                let mut st = self.st.lock();
                if !st.rx.is_empty() {
                    let n = if self.ty == SOCK_STREAM {
                        self.recv_stream(&mut buf[total..], flags, &mut st, &mut meta)
                    } else {
                        // Un message par lecture, tronqué si besoin.
                        let peek = flags & MSG_PEEK != 0;
                        let msg = if peek { None } else { st.rx.pop_front() };
                        let m = match &msg {
                            Some(m) => m,
                            None => st.rx.front().unwrap(),
                        };
                        let n = m.data.len().min(buf.len());
                        buf[..n].copy_from_slice(&m.data[..n]);
                        meta.full_len = m.data.len();
                        if m.data.len() > n {
                            meta.flags |= MSG_TRUNC;
                        }
                        if self.seqpacket() {
                            meta.flags |= MSG_EOR;
                        }
                        meta.cred = Some(m.cred);
                        meta.addr = Some(UnixAddr::encode(m.from.as_ref()));
                        let len = m.data.len();
                        match msg {
                            Some(m) => {
                                meta.fds = m.fds;
                                st.rx_bytes -= len.min(st.rx_bytes);
                            }
                            None => meta.fds = st.rx.front().unwrap().fds.clone(),
                        }
                        n
                    };
                    return Some(Ok(n));
                }
                if st.eof || st.shut_rd {
                    return Some(Ok(0));
                }
                if self.connection_oriented() && !matches!(st.conn, Conn::Connected(_)) {
                    return Some(Err(if matches!(st.conn, Conn::Listening { .. }) { EINVAL } else { ENOTCONN }));
                }
                if nonblock {
                    return Some(Err(EAGAIN));
                }
                None
            });
            match r {
                Ok(Ok(n)) => {
                    if flags & MSG_PEEK == 0 {
                        self.consumed(n.max(1));
                    }
                    total += n;
                    if !waitall || n == 0 || total >= buf.len() || !meta.fds.is_empty() {
                        break;
                    }
                }
                Ok(Err(e)) => {
                    if total > 0 {
                        break;
                    }
                    return Err(e);
                }
                Err(WaitError::TimedOut) => {
                    if total > 0 {
                        break;
                    }
                    return Err(EAGAIN);
                }
                Err(WaitError::Interrupted) => {
                    if total > 0 {
                        break;
                    }
                    return Err(crate::signal::ERESTARTSYS);
                }
            }
        }
        meta.len = total;
        if self.ty == SOCK_STREAM {
            meta.full_len = total;
            if meta.addr.is_none() {
                meta.addr = Some(UnixAddr::encode(None));
            }
        }
        Ok(meta)
    }

    fn shutdown(&self, how: i32) -> Result<(), i64> {
        let peer = {
            let mut st = self.st.lock();
            if self.connection_oriented() && !matches!(st.conn, Conn::Connected(_)) {
                return Err(ENOTCONN);
            }
            if how == SHUT_RD || how == SHUT_RDWR {
                st.shut_rd = true;
            }
            if how == SHUT_WR || how == SHUT_RDWR {
                st.shut_wr = true;
            }
            match &st.conn {
                Conn::Connected(w) => w.upgrade(),
                _ => None,
            }
        };
        if let Some(p) = peer {
            if self.connection_oriented() {
                let mut ps = p.st.lock();
                if how == SHUT_WR || how == SHUT_RDWR {
                    ps.eof = true;
                }
                if how == SHUT_RD || how == SHUT_RDWR {
                    ps.shut_wr = true;
                }
            }
            p.wq.wake_all();
        }
        self.wq.wake_all();
        Ok(())
    }

    fn sockname(&self) -> Vec<u8> {
        UnixAddr::encode(self.st.lock().local.as_ref())
    }

    fn peername(&self) -> Result<Vec<u8>, i64> {
        let peer = match &self.st.lock().conn {
            Conn::Connected(w) => w.upgrade().ok_or(ENOTCONN)?,
            _ => return Err(ENOTCONN),
        };
        let a = peer.st.lock().local.clone();
        Ok(UnixAddr::encode(a.as_ref()))
    }

    fn is_listening(&self) -> bool {
        matches!(self.st.lock().conn, Conn::Listening { .. })
    }

    fn take_error(&self) -> i64 {
        core::mem::take(&mut self.st.lock().error)
    }

    fn peer_cred(&self) -> Option<Ucred> {
        self.st.lock().peer_cred
    }

    fn is_stream(&self) -> bool {
        self.ty == SOCK_STREAM
    }
}

impl FileObject for UnixSocket {
    object_any!();

    fn link_name(&self) -> String {
        format!("socket:[{}]", self.ino)
    }

    fn stat(&self) -> (u32, u64) {
        (S_IFSOCK | 0o777, self.ino)
    }

    fn read(&self, t: &Thread, kb: &mut [u8], nonblock: bool) -> Result<usize, i64> {
        read_socket(t, self, kb, nonblock)
    }

    fn write(&self, t: &Thread, kb: &[u8], nonblock: bool) -> Result<usize, i64> {
        write_socket(t, self, kb, nonblock)
    }

    fn poll(&self, _t: Option<&Thread>) -> u32 {
        let (mut r, peer) = {
            let st = self.st.lock();
            let mut r = 0;
            let peer = match &st.conn {
                Conn::Listening { backlog, .. } => {
                    return if backlog.is_empty() { 0 } else { ev::READABLE };
                }
                Conn::Connected(w) => Some(w.clone()),
                Conn::Unconnected => None,
            };
            if !st.rx.is_empty() || st.eof || st.shut_rd {
                r |= ev::READABLE;
            }
            if st.eof {
                r |= ev::RDHUP;
            }
            if (st.eof && st.shut_wr) || st.peer_gone || (st.shut_rd && st.shut_wr) {
                r |= ev::HUP;
            }
            if self.connection_oriented() && peer.is_none() {
                // Flux jamais connecté : écriture possible (elle échouera), raccroché.
                r |= ev::WRITABLE | ev::HUP;
            }
            if st.shut_wr {
                r |= ev::WRITABLE;
            }
            (r, peer)
        };
        match peer.map(|w| w.upgrade()) {
            Some(Some(p)) => {
                let cap = p.rcvbuf();
                let ps = p.st.lock();
                if ps.rx_bytes < cap || ps.rx.is_empty() {
                    r |= ev::WRITABLE;
                }
            }
            Some(None) => r |= ev::WRITABLE | if self.connection_oriented() { ev::HUP } else { 0 },
            None if !self.connection_oriented() => r |= ev::WRITABLE,
            None => {}
        }
        r
    }

    fn queues<'a>(&'a self, cb: &mut dyn FnMut(&'a WaitQueue)) {
        cb(&self.wq);
    }

    fn as_socket(&self) -> Option<&dyn SocketOps> {
        Some(self)
    }

    fn pending_bytes(&self) -> Option<usize> {
        let st = self.st.lock();
        Some(if self.ty == SOCK_STREAM { st.rx.iter().map(|m| m.data.len() - m.off).sum() } else { st.rx.front().map_or(0, |m| m.data.len()) })
    }

    fn fdinfo(&self) -> String {
        let st = self.st.lock();
        format!("unix-type: {}\nunix-rx-bytes: {}\n", self.ty, st.rx_bytes)
    }
}
