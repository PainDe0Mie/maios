//! Sockets : appels système communs à toutes les familles (`socket`, `bind`, `sendmsg`…).
//!
//! Chaque famille fournit un objet ([`crate::object::FileObject`]) qui implémente aussi
//! [`SocketOps`]. Cette couche décode les structures de l'ABI (`sockaddr`, `msghdr`, `cmsghdr`,
//! `mmsghdr`), rassemble et disperse les `iovec`, gère les options génériques (`SOL_SOCKET`),
//! les descripteurs passés (`SCM_RIGHTS`) et les identités (`SCM_CREDENTIALS`).

use alloc::{sync::Arc, vec, vec::Vec};
use core::time::Duration;
use sync_preemption::PreemptionSafeMutex as Mutex;

use crate::fd::{Node, OpenFile, O_NONBLOCK, O_RDWR};
use crate::linux::errno::*;
use crate::linux::fs::get_fd;
use crate::linux::{fault, SysResult};
use crate::object::FileObject;
use crate::Thread;

pub const AF_UNIX: i32 = 1;
pub const AF_INET: i32 = 2;
pub const AF_INET6: i32 = 10;
pub const AF_NETLINK: i32 = 16;

pub const SOCK_STREAM: i32 = 1;
pub const SOCK_DGRAM: i32 = 2;
pub const SOCK_RAW: i32 = 3;
pub const SOCK_SEQPACKET: i32 = 5;
const SOCK_NONBLOCK: u64 = 0x800;
const SOCK_CLOEXEC: u64 = 0x80000;

pub const MSG_OOB: u32 = 0x1;
pub const MSG_PEEK: u32 = 0x2;
pub const MSG_CTRUNC: u32 = 0x8;
pub const MSG_TRUNC: u32 = 0x20;
pub const MSG_DONTWAIT: u32 = 0x40;
pub const MSG_EOR: u32 = 0x80;
pub const MSG_WAITALL: u32 = 0x100;
pub const MSG_NOSIGNAL: u32 = 0x4000;
pub const MSG_CMSG_CLOEXEC: u32 = 0x4000_0000;

pub const SOL_SOCKET: i32 = 1;
const SCM_RIGHTS: i32 = 1;
const SCM_CREDENTIALS: i32 = 2;
/// Descripteurs au plus par message (`SCM_MAX_FD`).
pub const SCM_MAX_FD: usize = 253;

pub const SHUT_RD: i32 = 0;
pub const SHUT_WR: i32 = 1;
pub const SHUT_RDWR: i32 = 2;

const SO_DEBUG: i32 = 1;
const SO_REUSEADDR: i32 = 2;
const SO_TYPE: i32 = 3;
const SO_ERROR: i32 = 4;
const SO_SNDBUF: i32 = 7;
const SO_RCVBUF: i32 = 8;
const SO_KEEPALIVE: i32 = 9;
const SO_REUSEPORT: i32 = 15;
const SO_LINGER: i32 = 13;
const SO_PASSCRED: i32 = 16;
const SO_PEERCRED: i32 = 17;
const SO_RCVTIMEO_OLD: i32 = 20;
const SO_SNDTIMEO_OLD: i32 = 21;
const SO_ACCEPTCONN: i32 = 30;
const SO_SNDBUFFORCE: i32 = 32;
const SO_RCVBUFFORCE: i32 = 33;
const SO_PROTOCOL: i32 = 38;
const SO_DOMAIN: i32 = 39;
const SO_RCVTIMEO_NEW: i32 = 66;
const SO_SNDTIMEO_NEW: i32 = 67;

/// Taille des tampons par défaut (`net.core.rmem_default`).
pub const DEFAULT_BUF: usize = 212_992;
/// Taille maximale (`net.core.rmem_max` × 2, comme Linux double la valeur demandée).
const MAX_BUF: usize = 4 * 1024 * 1024;
/// Morceau des envois en flux (les données sont rassemblées par morceaux).
const STREAM_CHUNK: usize = 256 * 1024;

/// Identité d'un processus (`struct ucred`).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Ucred {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
}

impl Ucred {
    /// Identité de l'appelant : pid et identités effectives **globaux** (`cred_to_ucred`).
    pub fn of(t: &Thread) -> Ucred {
        let c = t.process.creds.lock();
        Ucred { pid: t.process.pid as i32, uid: c.euid, gid: c.egid }
    }

    /// `struct ucred` vue du lecteur `t` : pid dans son espace pid (0 si invisible), identités
    /// dans son espace utilisateur.
    fn bytes_for(&self, t: &Thread) -> [u8; 12] {
        let ns = t.process.creds.lock().user_ns.clone();
        let mut b = [0u8; 12];
        let pid = t.process.vnr(self.pid as u32 as usize) as i32;
        b[0..4].copy_from_slice(&pid.to_le_bytes());
        b[4..8].copy_from_slice(&ns.from_kuid_munged(self.uid).to_le_bytes());
        b[8..12].copy_from_slice(&ns.from_kgid_munged(self.gid).to_le_bytes());
        b
    }
}

/// Options génériques, tenues par chaque socket.
#[derive(Clone)]
pub struct SockOpts {
    pub rcvtimeo: Option<Duration>,
    pub sndtimeo: Option<Duration>,
    pub rcvbuf: usize,
    pub sndbuf: usize,
    pub passcred: bool,
    pub reuseaddr: bool,
    pub reuseport: bool,
    pub keepalive: bool,
    pub linger: [i32; 2],
}

impl Default for SockOpts {
    fn default() -> Self {
        SockOpts { rcvtimeo: None, sndtimeo: None, rcvbuf: DEFAULT_BUF, sndbuf: DEFAULT_BUF, passcred: false, reuseaddr: false, reuseport: false, keepalive: false, linger: [0, 0] }
    }
}

impl SockOpts {
    pub fn rcv_deadline(&self) -> Option<time::Instant> {
        self.rcvtimeo.map(|d| time::Instant::now() + d)
    }

    pub fn snd_deadline(&self) -> Option<time::Instant> {
        self.sndtimeo.map(|d| time::Instant::now() + d)
    }
}

/// Données annexes IP d'un envoi (`ip_cmsg_send`, `ip6_datagram_send_ctl`) : valeurs pour ce
/// seul paquet, prioritaires sur les options de la socket. -1 = valeur par défaut du noyau.
#[derive(Clone, Copy, Default, Debug)]
pub struct IpCtl {
    /// `IPV6_HOPLIMIT`.
    pub hops: Option<i32>,
    /// `IPV6_TCLASS`.
    pub tclass: Option<i32>,
    /// `IP_TTL`.
    pub ttl: Option<i32>,
    /// `IP_TOS`.
    pub tos: Option<i32>,
    /// `IPV6_PKTINFO` / `IP_PKTINFO` : adresse source demandée (forme IPv6, IPv4 en `::ffff:`).
    pub src: Option<[u8; 16]>,
}

/// Message à envoyer (données déjà rassemblées).
pub struct SendMsg {
    pub data: Vec<u8>,
    /// Adresse de destination brute (`sockaddr`), pour les datagrammes.
    pub addr: Option<Vec<u8>>,
    pub fds: Vec<Arc<OpenFile>>,
    pub cred: Ucred,
    pub flags: u32,
    pub ip: IpCtl,
}

/// Résultat d'une réception.
#[derive(Default)]
pub struct RecvMeta {
    /// Octets copiés.
    pub len: usize,
    /// Taille réelle du message (datagramme tronqué : `MSG_TRUNC`).
    pub full_len: usize,
    pub addr: Option<Vec<u8>>,
    pub fds: Vec<Arc<OpenFile>>,
    pub cred: Option<Ucred>,
    /// `MSG_TRUNC`, `MSG_EOR`…
    pub flags: u32,
    /// Données annexes produites par le protocole (`IPV6_HOPLIMIT`, `IP_TTL`…), dans l'ordre de
    /// Linux : (niveau, type, données).
    pub cmsgs: Vec<(i32, i32, Vec<u8>)>,
}

/// Opérations d'une socket.
pub trait SocketOps: Send + Sync {
    fn domain(&self) -> i32;
    fn sock_type(&self) -> i32;
    fn protocol(&self) -> i32 {
        0
    }
    fn opts(&self) -> &Mutex<SockOpts>;
    /// Espace réseau de la socket (`sock_net`) ; `None` : celui de l'appelant.
    fn sock_net(&self) -> Option<Arc<crate::netns::NetNs>> {
        None
    }

    fn bind(&self, t: &Thread, addr: &[u8]) -> Result<(), i64>;
    fn connect(&self, t: &Thread, addr: &[u8], nonblock: bool) -> Result<(), i64>;
    fn listen(&self, t: &Thread, backlog: i32) -> Result<(), i64>;
    /// Connexion acceptée et adresse du pair.
    fn accept(&self, t: &Thread, nonblock: bool) -> Result<(Arc<dyn FileObject>, Vec<u8>), i64>;
    fn send(&self, t: &Thread, m: SendMsg, nonblock: bool) -> Result<usize, i64>;
    fn recv(&self, t: &Thread, buf: &mut [u8], flags: u32, nonblock: bool) -> Result<RecvMeta, i64>;
    fn shutdown(&self, how: i32) -> Result<(), i64>;
    fn sockname(&self) -> Vec<u8>;
    fn peername(&self) -> Result<Vec<u8>, i64>;
    fn is_listening(&self) -> bool;
    fn take_error(&self) -> i64 {
        0
    }
    fn peer_cred(&self) -> Option<Ucred> {
        None
    }
    /// Options propres à la famille (autres niveaux que `SOL_SOCKET`).
    fn setsockopt(&self, _level: i32, _name: i32, _val: &[u8]) -> Result<(), i64> {
        Err(ENOPROTOOPT)
    }
    fn getsockopt(&self, _level: i32, _name: i32) -> Result<Vec<u8>, i64> {
        Err(ENOPROTOOPT)
    }
    /// Envois en flux découpés en morceaux (les datagrammes partent d'un bloc).
    fn is_stream(&self) -> bool {
        self.sock_type() == SOCK_STREAM
    }
}

/// Socket désignée par `fd`.
fn sock_of(t: &Thread, fd: u64) -> Result<(Arc<OpenFile>, Arc<dyn FileObject>), i64> {
    let f = get_fd(t, fd)?;
    let o = f.object().cloned().ok_or(ENOTSOCK)?;
    if o.as_socket().is_none() {
        return Err(ENOTSOCK);
    }
    Ok((f, o))
}

pub(crate) fn install(t: &Thread, obj: Arc<dyn FileObject>, flags: u64) -> Result<usize, i64> {
    let status = O_RDWR | if flags & SOCK_NONBLOCK != 0 { O_NONBLOCK } else { 0 };
    let file = OpenFile::new(Node::Object(obj), status);
    t.process.files.lock().alloc(file, 0, flags & SOCK_CLOEXEC != 0).ok_or(EMFILE)
}

/// Nouvelle socket de la famille demandée, dans l'espace réseau de l'appelant.
fn new_socket(t: &Thread, domain: i32, ty: i32, protocol: i32) -> Result<Arc<dyn FileObject>, i64> {
    let net = crate::netns::of(&t.process);
    match domain {
        AF_UNIX => {
            if protocol != 0 && protocol != AF_UNIX {
                return Err(EPROTONOSUPPORT);
            }
            crate::unix::UnixSocket::create(net, ty).map(|s| s as Arc<dyn FileObject>)
        }
        AF_INET | AF_INET6 => crate::inet::InetSocket::create(net, domain, ty, protocol).map(|s| s as Arc<dyn FileObject>),
        AF_NETLINK => crate::netlink::NetlinkSocket::create(net, ty, protocol).map(|s| s as Arc<dyn FileObject>),
        _ => Err(EAFNOSUPPORT),
    }
}

pub fn socket(t: &Thread, domain: u64, ty: u64, protocol: u64) -> SysResult {
    if ty & !(0xf | SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return Err(EINVAL);
    }
    let obj = new_socket(t, domain as i32, (ty & 0xf) as i32, protocol as i32)?;
    install(t, obj, ty).map(|fd| fd as i64)
}

pub fn socketpair(t: &Thread, domain: u64, ty: u64, protocol: u64, sv: u64) -> SysResult {
    if domain as i32 != AF_UNIX {
        return Err(if matches!(domain as i32, AF_INET | AF_INET6) { EOPNOTSUPP } else { EAFNOSUPPORT });
    }
    if protocol != 0 && protocol as i32 != AF_UNIX {
        return Err(EPROTONOSUPPORT);
    }
    if ty & !(0xf | SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return Err(EINVAL);
    }
    let (a, b) = crate::unix::UnixSocket::pair(t, (ty & 0xf) as i32)?;
    let fa = install(t, a, ty)?;
    let fb = match install(t, b, ty) {
        Ok(f) => f,
        Err(e) => {
            t.process.files.lock().close(fa);
            return Err(e);
        }
    };
    if let Err(e) = t.mm().write_user(sv as usize, &[fa as i32, fb as i32]) {
        let mut table = t.process.files.lock();
        table.close(fa);
        table.close(fb);
        return Err(fault(e));
    }
    Ok(0)
}

pub(crate) fn read_addr(t: &Thread, addr: u64, len: u64) -> Result<Vec<u8>, i64> {
    if len > 128 {
        return Err(EINVAL);
    }
    let mut b = vec![0u8; len as usize];
    t.mm().copy_from_user(&mut b, addr as usize).map_err(fault)?;
    Ok(b)
}

/// Écrit une adresse rendue (`accept`, `getsockname`, `recvfrom`) : tronquée à la place
/// offerte, longueur réelle écrite dans `*lenp`.
fn write_addr(t: &Thread, addr: u64, lenp: u64, a: &[u8]) -> Result<(), i64> {
    if addr == 0 || lenp == 0 {
        return Ok(());
    }
    let room = t.mm().read_user::<u32>(lenp as usize).map_err(fault)? as usize;
    let n = room.min(a.len());
    t.mm().copy_to_user(addr as usize, &a[..n]).map_err(fault)?;
    t.mm().write_user::<u32>(lenp as usize, &(a.len() as u32)).map_err(fault)
}

pub fn bind(t: &Thread, fd: u64, addr: u64, len: u64) -> SysResult {
    let (_f, o) = sock_of(t, fd)?;
    let a = read_addr(t, addr, len)?;
    o.as_socket().unwrap().bind(t, &a).map(|_| 0)
}

pub fn connect(t: &Thread, fd: u64, addr: u64, len: u64) -> SysResult {
    let (f, o) = sock_of(t, fd)?;
    let a = read_addr(t, addr, len)?;
    o.as_socket().unwrap().connect(t, &a, f.nonblock()).map(|_| 0)
}

pub fn listen(t: &Thread, fd: u64, backlog: u64) -> SysResult {
    let (_f, o) = sock_of(t, fd)?;
    o.as_socket().unwrap().listen(t, backlog as i32).map(|_| 0)
}

pub fn accept4(t: &Thread, fd: u64, addr: u64, lenp: u64, flags: u64) -> SysResult {
    let (f, o) = sock_of(t, fd)?;
    accept_on(t, &o, addr, lenp, flags, f.nonblock())
}

/// `accept4` sur une socket déjà résolue (io_uring : `nonblock` pour la tentative en ligne).
pub(crate) fn accept_on(t: &Thread, o: &Arc<dyn FileObject>, addr: u64, lenp: u64, flags: u64, nonblock: bool) -> SysResult {
    if flags & !(SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return Err(EINVAL);
    }
    let s = o.as_socket().ok_or(ENOTSOCK)?;
    let (conn, peer) = s.accept(t, nonblock)?;
    let nfd = install(t, conn, flags)?;
    if let Err(e) = write_addr(t, addr, lenp, &peer) {
        t.process.files.lock().close(nfd);
        return Err(e);
    }
    Ok(nfd as i64)
}

pub fn shutdown(t: &Thread, fd: u64, how: u64) -> SysResult {
    let (_f, o) = sock_of(t, fd)?;
    let how = how as i32;
    if !(SHUT_RD..=SHUT_RDWR).contains(&how) {
        return Err(EINVAL);
    }
    o.as_socket().unwrap().shutdown(how).map(|_| 0)
}

pub fn getsockname(t: &Thread, fd: u64, addr: u64, lenp: u64) -> SysResult {
    let (_f, o) = sock_of(t, fd)?;
    let a = o.as_socket().unwrap().sockname();
    write_addr(t, addr, lenp, &a).map(|_| 0)
}

pub fn getpeername(t: &Thread, fd: u64, addr: u64, lenp: u64) -> SysResult {
    let (_f, o) = sock_of(t, fd)?;
    let a = o.as_socket().unwrap().peername()?;
    write_addr(t, addr, lenp, &a).map(|_| 0)
}

fn timeval_of(v: &[u8]) -> Result<Option<Duration>, i64> {
    if v.len() < 16 {
        return Err(EINVAL);
    }
    let s = i64::from_le_bytes(v[0..8].try_into().unwrap());
    let us = i64::from_le_bytes(v[8..16].try_into().unwrap());
    if s < 0 || !(0..1_000_000).contains(&us) {
        return Err(EDOM_OR_INVAL);
    }
    let d = Duration::new(s as u64, us as u32 * 1000);
    Ok(if d.is_zero() { None } else { Some(d) })
}
/// `EDOM` (33) pour un délai invalide, comme Linux.
const EDOM_OR_INVAL: i64 = 33;

fn int_of(v: &[u8]) -> Result<i32, i64> {
    if v.len() < 4 {
        return Err(EINVAL);
    }
    Ok(i32::from_le_bytes(v[0..4].try_into().unwrap()))
}

pub fn setsockopt(t: &Thread, fd: u64, level: u64, name: u64, val: u64, len: u64) -> SysResult {
    let (_f, o) = sock_of(t, fd)?;
    let s = o.as_socket().unwrap();
    if len > 4096 {
        return Err(EINVAL);
    }
    let mut v = vec![0u8; len as usize];
    t.mm().copy_from_user(&mut v, val as usize).map_err(fault)?;
    let (level, name) = (level as i32, name as i32);
    if level != SOL_SOCKET {
        return s.setsockopt(level, name, &v).map(|_| 0);
    }
    let mut o = s.opts().lock();
    match name {
        SO_RCVTIMEO_OLD | SO_RCVTIMEO_NEW => o.rcvtimeo = timeval_of(&v)?,
        SO_SNDTIMEO_OLD | SO_SNDTIMEO_NEW => o.sndtimeo = timeval_of(&v)?,
        // Linux double la valeur (place des métadonnées) et la borne.
        SO_RCVBUF | SO_RCVBUFFORCE => o.rcvbuf = (int_of(&v)?.max(1152) as usize * 2).min(MAX_BUF),
        SO_SNDBUF | SO_SNDBUFFORCE => o.sndbuf = (int_of(&v)?.max(2304) as usize * 2).min(MAX_BUF),
        SO_PASSCRED => o.passcred = int_of(&v)? != 0,
        SO_REUSEADDR => o.reuseaddr = int_of(&v)? != 0,
        SO_REUSEPORT => o.reuseport = int_of(&v)? != 0,
        SO_KEEPALIVE => o.keepalive = int_of(&v)? != 0,
        SO_LINGER => {
            o.linger = [int_of(&v)?, int_of(v.get(4..).unwrap_or(&[]))?];
        }
        // SO_DONTROUTE, SO_BROADCAST, SO_OOBINLINE, SO_PRIORITY, SO_RCVLOWAT, SO_SNDLOWAT,
        // SO_BINDTODEVICE, SO_TIMESTAMP(NS), SO_MARK, SO_BUSY_POLL, SO_INCOMING_CPU, SO_ZEROCOPY…
        SO_DEBUG | 5 | 6 | 10 | 12 | 18 | 19 | 25 | 29 | 35 | 36 | 42 | 46 | 49 | 60 => {} // sans effet ici
        _ => return Err(ENOPROTOOPT),
    }
    Ok(0)
}

pub fn getsockopt(t: &Thread, fd: u64, level: u64, name: u64, val: u64, lenp: u64) -> SysResult {
    let (_f, o) = sock_of(t, fd)?;
    let s = o.as_socket().unwrap();
    let (level, name) = (level as i32, name as i32);
    let int = |v: i32| v.to_le_bytes().to_vec();
    let tv = |d: Option<Duration>| {
        let d = d.unwrap_or_default();
        let mut b = (d.as_secs() as i64).to_le_bytes().to_vec();
        b.extend_from_slice(&(d.subsec_micros() as i64).to_le_bytes());
        b
    };
    let out: Vec<u8> = if level != SOL_SOCKET {
        s.getsockopt(level, name)?
    } else {
        let o = s.opts().lock();
        match name {
            SO_TYPE => int(s.sock_type()),
            SO_DOMAIN => int(s.domain()),
            SO_PROTOCOL => int(s.protocol()),
            SO_ERROR => {
                drop(o);
                int(s.take_error() as i32)
            }
            SO_ACCEPTCONN => int(s.is_listening() as i32),
            SO_RCVBUF => int(o.rcvbuf as i32),
            SO_SNDBUF => int(o.sndbuf as i32),
            SO_PASSCRED => int(o.passcred as i32),
            SO_REUSEADDR => int(o.reuseaddr as i32),
            SO_REUSEPORT => int(o.reuseport as i32),
            SO_KEEPALIVE => int(o.keepalive as i32),
            SO_LINGER => {
                let mut b = int(o.linger[0]);
                b.extend_from_slice(&o.linger[1].to_le_bytes());
                b
            }
            SO_RCVTIMEO_OLD | SO_RCVTIMEO_NEW => tv(o.rcvtimeo),
            SO_SNDTIMEO_OLD | SO_SNDTIMEO_NEW => tv(o.sndtimeo),
            SO_PEERCRED => {
                drop(o);
                s.peer_cred().ok_or(ENOTCONN)?.bytes_for(t).to_vec()
            }
            SO_DEBUG | 5 | 6 | 10 | 12 | 18 | 19 | 29 | 35 | 36 | 46 | 60 => int(0),
            49 => {
                drop(o);
                int(cpu::current_cpu().value() as i32)
            }
            _ => return Err(ENOPROTOOPT),
        }
    };
    let room = t.mm().read_user::<u32>(lenp as usize).map_err(fault)? as usize;
    let n = room.min(out.len());
    t.mm().copy_to_user(val as usize, &out[..n]).map_err(fault)?;
    t.mm().write_user::<u32>(lenp as usize, &(n as u32)).map_err(fault)?;
    Ok(0)
}

// --- Envoi et réception ------------------------------------------------------------------------

/// `struct msghdr` décodé.
struct MsgHdr {
    name: u64,
    namelen: u32,
    iov: Vec<(u64, u64)>,
    control: u64,
    controllen: u64,
}

/// Disposition des `cmsghdr` : en-tête et alignement (16 et 8 octets ; 12 et 4 pour un appel
/// i386, `struct compat_cmsghdr`).
fn cmsg_abi(t: &Thread) -> (usize, usize) {
    if crate::linux::compat::active(t) { (12, 4) } else { (16, 8) }
}

/// Décalages dans `msghdr` de `msg_namelen`, `msg_controllen` (et sa taille), `msg_flags`,
/// puis taille d'un `mmsghdr` et décalage de `msg_len`.
struct MsgOffsets {
    namelen: usize,
    controllen: usize,
    controllen_wide: bool,
    flags: usize,
    mmsg_size: u64,
    mmsg_len: usize,
}

fn msg_offsets(t: &Thread) -> MsgOffsets {
    if crate::linux::compat::active(t) {
        MsgOffsets { namelen: 4, controllen: 20, controllen_wide: false, flags: 24, mmsg_size: 32, mmsg_len: 28 }
    } else {
        MsgOffsets { namelen: 8, controllen: 40, controllen_wide: true, flags: 48, mmsg_size: 64, mmsg_len: 56 }
    }
}

fn read_msghdr(t: &Thread, p: u64) -> Result<MsgHdr, i64> {
    if crate::linux::compat::active(t) {
        // `struct compat_msghdr` : sept mots de 4 octets.
        let w = t.mm().read_user::<[u32; 7]>(p as usize).map_err(fault)?;
        return Ok(MsgHdr {
            name: w[0] as u64,
            namelen: w[1],
            iov: crate::linux::fs::iovecs(t, w[2] as u64, w[3] as u64)?,
            control: w[4] as u64,
            controllen: w[5] as u64,
        });
    }
    let mut raw = [0u8; 56];
    t.mm().copy_from_user(&mut raw, p as usize).map_err(fault)?;
    let u64_at = |o: usize| u64::from_le_bytes(raw[o..o + 8].try_into().unwrap());
    let iovlen = u64_at(24);
    Ok(MsgHdr {
        name: u64_at(0),
        namelen: u32::from_le_bytes(raw[8..12].try_into().unwrap()),
        iov: crate::linux::fs::iovecs(t, u64_at(16), iovlen)?,
        control: u64_at(32),
        controllen: u64_at(40),
    })
}

const SOL_IP: i32 = 0;
const SOL_IPV6: i32 = 41;
const IP_TOS: i32 = 1;
const IP_TTL: i32 = 2;
const IP_PKTINFO: i32 = 8;
const IPV6_PKTINFO: i32 = 50;
const IPV6_HOPLIMIT: i32 = 52;
const IPV6_TCLASS: i32 = 67;

/// Un `int` de donnée annexe : la longueur doit être exactement `CMSG_LEN(sizeof(int))` et la
/// valeur entre -1 et 255 (`ip6_datagram_send_ctl`).
fn cmsg_int(data: &[u8]) -> Result<i32, i64> {
    if data.len() != 4 {
        return Err(EINVAL);
    }
    let v = i32::from_le_bytes(data.try_into().unwrap());
    if !(-1..=255).contains(&v) {
        return Err(EINVAL);
    }
    Ok(v)
}

/// Données ancillaires d'un envoi : descripteurs, identité, options IP du paquet. Comme Linux,
/// une socket ne lit que les niveaux qui la concernent (`SOL_SOCKET`, et `SOL_IP`/`SOL_IPV6`
/// pour une socket Internet) et passe les autres.
fn parse_control(t: &Thread, m: &MsgHdr, domain: i32) -> Result<(Vec<Arc<OpenFile>>, Option<Ucred>, IpCtl), i64> {
    let mut fds = Vec::new();
    let mut cred = None;
    let mut ip = IpCtl::default();
    if m.control == 0 || m.controllen == 0 {
        return Ok((fds, cred, ip));
    }
    let inet = domain == AF_INET || domain == AF_INET6;
    if m.controllen > 64 * 1024 {
        return Err(ENOBUFS);
    }
    let mut c = vec![0u8; m.controllen as usize];
    t.mm().copy_from_user(&mut c, m.control as usize).map_err(fault)?;
    let (hdr, align) = cmsg_abi(t);
    let mut off = 0usize;
    while off + hdr <= c.len() {
        let len = if hdr == 16 {
            u64::from_le_bytes(c[off..off + 8].try_into().unwrap()) as usize
        } else {
            u32::from_le_bytes(c[off..off + 4].try_into().unwrap()) as usize
        };
        let level = i32::from_le_bytes(c[off + hdr - 8..off + hdr - 4].try_into().unwrap());
        let ty = i32::from_le_bytes(c[off + hdr - 4..off + hdr].try_into().unwrap());
        if len < hdr || off + len > c.len() {
            return Err(EINVAL);
        }
        let data = &c[off + hdr..off + len];
        if level == SOL_SOCKET && ty == SCM_RIGHTS {
            let table = t.process.files.lock();
            for ch in data.chunks_exact(4) {
                let fd = i32::from_le_bytes(ch.try_into().unwrap());
                if fd < 0 {
                    return Err(EBADF);
                }
                fds.push(table.get(fd as usize).ok_or(EBADF)?);
                if fds.len() > SCM_MAX_FD {
                    return Err(EINVAL);
                }
            }
        } else if level == SOL_SOCKET && ty == SCM_CREDENTIALS {
            if data.len() < 12 {
                return Err(EINVAL);
            }
            let word = |o: usize| u32::from_le_bytes(data[o..o + 4].try_into().unwrap());
            let pid = word(0) as i32;
            // `scm_check_creds` : sa propre identité (réelle, effective ou sauvegardée), sauf
            // avec `CAP_SYS_ADMIN` (pid), `CAP_SETUID`/`CAP_SETGID` dans son espace.
            let c = t.process.creds.lock().clone();
            let cap = |k| crate::userns::ns_capable(&c, &c.user_ns, k);
            // pid de l'espace pid de l'appelant : le sien, ou un autre avec `CAP_SYS_ADMIN` sur
            // cet espace ; gardé en numéro global.
            let pid_ns = t.process.pid_ns.clone();
            if pid != t.process.vpid as i32 && !crate::userns::ns_capable(&c, &pid_ns.user_ns, crate::userns::CAP_SYS_ADMIN) {
                return Err(EPERM);
            }
            let pid = pid_ns.global_of(pid as u32 as usize).ok_or(ESRCH)? as i32;
            let uid = c.user_ns.make_kuid(word(4)).ok_or(EINVAL)?;
            let gid = c.user_ns.make_kgid(word(8)).ok_or(EINVAL)?;
            if !(uid == c.ruid || uid == c.euid || uid == c.suid || cap(crate::userns::CAP_SETUID)) {
                return Err(EPERM);
            }
            if !(gid == c.rgid || gid == c.egid || gid == c.sgid || cap(crate::userns::CAP_SETGID)) {
                return Err(EPERM);
            }
            cred = Some(Ucred { pid, uid, gid });
        } else if level == SOL_SOCKET {
            return Err(EINVAL);
        } else if inet && level == SOL_IPV6 && domain == AF_INET6 {
            match ty {
                IPV6_HOPLIMIT => ip.hops = Some(cmsg_int(data)?),
                IPV6_TCLASS => ip.tclass = Some(cmsg_int(data)?),
                IPV6_PKTINFO => {
                    // `struct in6_pktinfo` : adresse (16 octets) puis indice d'interface.
                    if data.len() < 20 {
                        return Err(EINVAL);
                    }
                    let a: [u8; 16] = data[..16].try_into().unwrap();
                    if a != [0; 16] {
                        ip.src = Some(a);
                    }
                }
                _ => return Err(EINVAL),
            }
        } else if inet && level == SOL_IP {
            match ty {
                IP_TTL => {
                    let v = cmsg_int(data)?;
                    if v == 0 {
                        return Err(EINVAL);
                    }
                    ip.ttl = Some(v);
                }
                // `IP_TOS` : un octet (ou un `int`, accepté aussi par Linux).
                IP_TOS => match data.len() {
                    1 => ip.tos = Some(data[0] as i32),
                    4 => ip.tos = Some(i32::from_le_bytes(data.try_into().unwrap()) & 0xff),
                    _ => return Err(EINVAL),
                },
                IP_PKTINFO => {
                    // `struct in_pktinfo` : indice, adresse locale demandée, adresse d'en-tête.
                    if data.len() < 12 {
                        return Err(EINVAL);
                    }
                    let a = &data[4..8];
                    if a != [0; 4] {
                        let mut v = [0u8; 16];
                        v[10] = 0xff;
                        v[11] = 0xff;
                        v[12..].copy_from_slice(a);
                        ip.src = Some(v);
                    }
                }
                _ => return Err(EINVAL),
            }
        }
        off += (len + align - 1) & !(align - 1);
    }
    Ok((fds, cred, ip))
}

/// Ajoute une donnée annexe à `out`, qui ne doit pas dépasser `room` (`put_cmsg` de Linux) :
/// sans place pour l'en-tête, rien et `MSG_CTRUNC` ; place partielle, la donnée est coupée (et
/// `cmsg_len` le dit) avec `MSG_CTRUNC` ; l'avance est `CMSG_SPACE(len)`, bornée à la place.
fn put_cmsg(out: &mut Vec<u8>, abi: (usize, usize), room: usize, level: i32, ty: i32, data: &[u8]) -> u32 {
    let (hdr, align) = abi;
    let left = room.saturating_sub(out.len());
    if left < hdr {
        return MSG_CTRUNC;
    }
    let mut cmlen = hdr + data.len();
    let mut flags = 0;
    if left < cmlen {
        flags = MSG_CTRUNC;
        cmlen = left;
    }
    if hdr == 16 {
        out.extend_from_slice(&(cmlen as u64).to_le_bytes());
    } else {
        out.extend_from_slice(&(cmlen as u32).to_le_bytes());
    }
    out.extend_from_slice(&level.to_le_bytes());
    out.extend_from_slice(&ty.to_le_bytes());
    out.extend_from_slice(&data[..cmlen - hdr]);
    let space = (hdr + data.len() + align - 1) & !(align - 1);
    let advance = space.min(left);
    out.resize(out.len() - cmlen + advance, 0);
    flags
}

/// Rassemble au plus `max` octets des `iov` à partir de l'octet `skip`.
fn gather(t: &Thread, iov: &[(u64, u64)], skip: usize, max: usize) -> Result<Vec<u8>, i64> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    for &(base, len) in iov {
        let len = len as usize;
        if pos + len <= skip {
            pos += len;
            continue;
        }
        let from = skip.saturating_sub(pos);
        let n = (len - from).min(max - out.len());
        let start = out.len();
        out.resize(start + n, 0);
        t.mm().copy_from_user(&mut out[start..], base as usize + from).map_err(fault)?;
        pos += len;
        if out.len() >= max {
            break;
        }
    }
    Ok(out)
}

fn scatter(t: &Thread, iov: &[(u64, u64)], data: &[u8]) -> Result<(), i64> {
    let mut off = 0usize;
    for &(base, len) in iov {
        if off >= data.len() {
            break;
        }
        let n = (len as usize).min(data.len() - off);
        t.mm().copy_to_user(base as usize, &data[off..off + n]).map_err(fault)?;
        off += n;
    }
    Ok(())
}

fn total_len(iov: &[(u64, u64)]) -> Result<usize, i64> {
    iov.iter().try_fold(0usize, |a, &(_, l)| a.checked_add(l as usize).filter(|&v| v <= isize::MAX as usize).ok_or(EINVAL))
}

/// Adresse et données ancillaires d'un envoi.
#[derive(Default)]
struct Ancillary {
    addr: Option<Vec<u8>>,
    fds: Vec<Arc<OpenFile>>,
    cred: Option<Ucred>,
    ip: IpCtl,
}

/// Envoi commun à `sendto`, `sendmsg`, `sendmmsg` et `write`.
fn do_send(t: &Thread, f: &OpenFile, s: &dyn SocketOps, iov: &[(u64, u64)], anc: Ancillary, flags: u32) -> SysResult {
    let Ancillary { addr, mut fds, cred, ip } = anc;
    let nonblock = f.nonblock() || flags & MSG_DONTWAIT != 0;
    let total = total_len(iov)?;
    let cred = cred.unwrap_or_else(|| Ucred::of(t));
    if !s.is_stream() {
        let data = gather(t, iov, 0, total)?;
        return s.send(t, SendMsg { data, addr, fds, cred, flags, ip }, nonblock).map(|n| n as i64);
    }
    // Flux : par morceaux, les données ancillaires partent avec le premier.
    let mut done = 0usize;
    loop {
        let data = gather(t, iov, done, STREAM_CHUNK.min(total - done))?;
        let want = data.len();
        let r = s.send(t, SendMsg { data, addr: addr.clone(), fds: core::mem::take(&mut fds), cred, flags, ip }, nonblock);
        match r {
            Ok(n) => {
                done += n;
                if done >= total || n < want {
                    return Ok(done as i64);
                }
            }
            Err(_) if done > 0 => return Ok(done as i64),
            Err(e) => return Err(e),
        }
    }
}

pub fn sendto(t: &Thread, fd: u64, buf: u64, len: u64, flags: u64, addr: u64, alen: u64) -> SysResult {
    let (f, o) = sock_of(t, fd)?;
    let a = if addr != 0 { Some(read_addr(t, addr, alen)?) } else { None };
    do_send(t, &f, o.as_socket().unwrap(), &[(buf, len)], Ancillary { addr: a, ..Default::default() }, flags as u32)
}

pub(crate) fn sendmsg_one(t: &Thread, f: &OpenFile, s: &dyn SocketOps, msg: u64, flags: u32) -> SysResult {
    let m = read_msghdr(t, msg)?;
    let a = if m.name != 0 && m.namelen > 0 { Some(read_addr(t, m.name, m.namelen as u64)?) } else { None };
    let (fds, cred, ip) = parse_control(t, &m, s.domain())?;
    do_send(t, f, s, &m.iov, Ancillary { addr: a, fds, cred, ip }, flags)
}

pub fn sendmsg(t: &Thread, fd: u64, msg: u64, flags: u64) -> SysResult {
    let (f, o) = sock_of(t, fd)?;
    sendmsg_one(t, &f, o.as_socket().unwrap(), msg, flags as u32)
}

pub fn sendmmsg(t: &Thread, fd: u64, vec: u64, vlen: u64, flags: u64) -> SysResult {
    let (f, o) = sock_of(t, fd)?;
    let n = vlen.min(1024);
    let mut sent = 0;
    let off = msg_offsets(t);
    for i in 0..n {
        let hdr = vec + i * off.mmsg_size;
        match sendmsg_one(t, &f, o.as_socket().unwrap(), hdr, flags as u32) {
            Ok(len) => {
                t.mm().write_user::<u32>(hdr as usize + off.mmsg_len, &(len as u32)).map_err(fault)?;
                sent += 1;
            }
            Err(_) if sent > 0 => break,
            Err(e) => return Err(e),
        }
    }
    Ok(sent)
}

/// Pose les descripteurs reçus dans la table de l'appelant ; rend les `cmsghdr` à écrire et
/// `MSG_CTRUNC` si la place manque (les descripteurs en trop sont fermés, comme Linux).
fn build_control(t: &Thread, meta: &mut RecvMeta, room: usize, cloexec: bool, passcred: bool) -> (Vec<u8>, u32) {
    let mut out = Vec::new();
    let mut flags = 0;
    let abi = cmsg_abi(t);
    let (hdr, align) = abi;
    if passcred {
        let cred = meta.cred.unwrap_or_default();
        flags |= put_cmsg(&mut out, abi, room, SOL_SOCKET, SCM_CREDENTIALS, &cred.bytes_for(t));
    }
    for (level, ty, data) in core::mem::take(&mut meta.cmsgs) {
        flags |= put_cmsg(&mut out, abi, room, level, ty, &data);
    }
    let fds = core::mem::take(&mut meta.fds);
    if !fds.is_empty() {
        let avail = room.saturating_sub(out.len());
        let fit = if avail >= hdr { ((avail - hdr) / 4).min(fds.len()) } else { 0 };
        if fit < fds.len() {
            flags |= MSG_CTRUNC;
        }
        if fit > 0 {
            let mut nums = Vec::new();
            {
                let mut table = t.process.files.lock();
                for f in fds.into_iter().take(fit) {
                    match table.alloc(f, 0, cloexec) {
                        Some(n) => nums.push(n as i32),
                        None => {
                            flags |= MSG_CTRUNC;
                            break;
                        }
                    }
                }
            }
            let len = hdr + nums.len() * 4;
            if hdr == 16 {
                out.extend_from_slice(&(len as u64).to_le_bytes());
            } else {
                out.extend_from_slice(&(len as u32).to_le_bytes());
            }
            out.extend_from_slice(&SOL_SOCKET.to_le_bytes());
            out.extend_from_slice(&SCM_RIGHTS.to_le_bytes());
            for n in nums {
                out.extend_from_slice(&n.to_le_bytes());
            }
            out.resize((out.len() + align - 1) & !(align - 1), 0);
        }
    }
    (out, flags)
}

pub(crate) fn recvmsg_one(t: &Thread, f: &OpenFile, s: &dyn SocketOps, msg: u64, flags: u32) -> SysResult {
    let m = read_msghdr(t, msg)?;
    let total = total_len(&m.iov)?;
    let nonblock = f.nonblock() || flags & MSG_DONTWAIT != 0;
    let mut buf = vec![0u8; total.min(MAX_BUF)];
    let mut meta = s.recv(t, &mut buf, flags, nonblock)?;
    scatter(t, &m.iov, &buf[..meta.len])?;
    let passcred = s.opts().lock().passcred;
    let (ctl, cflags) = build_control(t, &mut meta, m.controllen as usize, flags & MSG_CMSG_CLOEXEC != 0, passcred);
    if m.control != 0 && !ctl.is_empty() {
        t.mm().copy_to_user(m.control as usize, &ctl[..ctl.len().min(m.controllen as usize)]).map_err(fault)?;
    }
    let o = msg_offsets(t);
    let clen = ctl.len().min(m.controllen as usize);
    if o.controllen_wide {
        t.mm().write_user::<u64>(msg as usize + o.controllen, &(clen as u64)).map_err(fault)?;
    } else {
        t.mm().write_user::<u32>(msg as usize + o.controllen, &(clen as u32)).map_err(fault)?;
    }
    if m.name != 0 {
        let a = meta.addr.clone().unwrap_or_default();
        let n = (m.namelen as usize).min(a.len());
        t.mm().copy_to_user(m.name as usize, &a[..n]).map_err(fault)?;
        t.mm().write_user::<u32>(msg as usize + o.namelen, &(a.len() as u32)).map_err(fault)?;
    }
    let out_flags = meta.flags | cflags;
    t.mm().write_user::<i32>(msg as usize + o.flags, &(out_flags as i32)).map_err(fault)?;
    Ok(if flags & MSG_TRUNC != 0 { meta.full_len } else { meta.len } as i64)
}

pub fn recvmsg(t: &Thread, fd: u64, msg: u64, flags: u64) -> SysResult {
    let (f, o) = sock_of(t, fd)?;
    recvmsg_one(t, &f, o.as_socket().unwrap(), msg, flags as u32)
}

pub fn recvmmsg(t: &Thread, fd: u64, vec: u64, vlen: u64, flags: u64, timeout: u64) -> SysResult {
    const MSG_WAITFORONE: u32 = 0x10000;
    let (f, o) = sock_of(t, fd)?;
    let deadline = crate::linux::fs::read_ts(t, timeout)?.map(|d| time::Instant::now() + d);
    let mut flags = flags as u32;
    let mut got = 0;
    let off = msg_offsets(t);
    for i in 0..vlen.min(1024) {
        let hdr = vec + i * off.mmsg_size;
        match recvmsg_one(t, &f, o.as_socket().unwrap(), hdr, flags & !MSG_WAITFORONE) {
            Ok(len) => {
                t.mm().write_user::<u32>(hdr as usize + off.mmsg_len, &(len as u32)).map_err(fault)?;
                got += 1;
                if flags & MSG_WAITFORONE != 0 {
                    flags |= MSG_DONTWAIT;
                }
            }
            Err(_) if got > 0 => break,
            Err(e) => return Err(e),
        }
        if deadline.map_or(false, |d| time::Instant::now() >= d) {
            break;
        }
    }
    Ok(got)
}

/// `send` sur une socket résolue (io_uring).
pub(crate) fn send_on(t: &Thread, f: &OpenFile, s: &dyn SocketOps, buf: u64, len: u64, flags: u32) -> SysResult {
    do_send(t, f, s, &[(buf, len)], Ancillary::default(), flags)
}

/// `recv` sur une socket résolue (io_uring).
pub(crate) fn recv_on(t: &Thread, f: &OpenFile, s: &dyn SocketOps, buf: u64, len: u64, flags: u32) -> SysResult {
    let nonblock = f.nonblock() || flags & MSG_DONTWAIT != 0;
    let mut kb = vec![0u8; (len as usize).min(MAX_BUF)];
    let meta = s.recv(t, &mut kb, flags, nonblock)?;
    t.mm().copy_to_user(buf as usize, &kb[..meta.len]).map_err(fault)?;
    Ok(if flags & MSG_TRUNC != 0 { meta.full_len } else { meta.len } as i64)
}

pub fn recvfrom(t: &Thread, fd: u64, buf: u64, len: u64, flags: u64, addr: u64, lenp: u64) -> SysResult {
    let (f, o) = sock_of(t, fd)?;
    let s = o.as_socket().unwrap();
    let flags = flags as u32;
    let nonblock = f.nonblock() || flags & MSG_DONTWAIT != 0;
    let mut kb = vec![0u8; (len as usize).min(MAX_BUF)];
    let meta = s.recv(t, &mut kb, flags, nonblock)?;
    t.mm().copy_to_user(buf as usize, &kb[..meta.len]).map_err(fault)?;
    if let Some(a) = &meta.addr {
        write_addr(t, addr, lenp, a)?;
    } else if addr != 0 && lenp != 0 {
        t.mm().write_user::<u32>(lenp as usize, &0).map_err(fault)?;
    }
    // Les descripteurs passés sans `recvmsg` sont perdus (fermés), comme Linux.
    Ok(if flags & MSG_TRUNC != 0 { meta.full_len } else { meta.len } as i64)
}

/// `read`/`write` sur une socket : `recv`/`send` sans drapeau.
pub fn read_socket(t: &Thread, s: &dyn SocketOps, kb: &mut [u8], nonblock: bool) -> Result<usize, i64> {
    s.recv(t, kb, 0, nonblock).map(|m| m.len)
}

pub fn write_socket(t: &Thread, s: &dyn SocketOps, kb: &[u8], nonblock: bool) -> Result<usize, i64> {
    s.send(t, SendMsg { data: kb.to_vec(), addr: None, fds: Vec::new(), cred: Ucred::of(t), flags: 0, ip: IpCtl::default() }, nonblock)
}
