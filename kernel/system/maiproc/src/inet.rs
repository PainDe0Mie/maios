//! Sockets Internet (`AF_INET`, `AF_INET6`) : TCP, UDP et UDP-Lite.
//!
//! Deux chemins, choisis à la connexion (ou à l'envoi pour UDP) selon la destination :
//!
//! - **Chemin rapide local** (destinations 127.0.0.0/8, `::1`, `0.0.0.0`, adresses de la
//!   machine). Comme le *TCP Loopback Fast Path* de Windows (`SIO_LOOPBACK_FAST_PATH`), une
//!   connexion locale ne fabrique aucun paquet : pas d'en-têtes, de sommes de contrôle, de
//!   fenêtres ni de minuteurs. Les octets vont directement de l'envoi à la file de réception du
//!   pair (même modèle que les sockets Unix). La sémantique de TCP vue par le programme est
//!   conservée : flux ordonné, demi-fermeture, `RST` (`ECONNRESET`) quand on ferme sans avoir
//!   tout lu ou quand l'écouteur disparaît avec des connexions non acceptées.
//! - **Réseau** : smoltcp sur l'interface par défaut (`e1000`). Chaque socket a son *waker* : la
//!   pile ne réveille que les sockets dont l'état a changé (modèle des rappels
//!   `sk_data_ready`/`sk_write_space` de Linux), et les envois partent tout de suite, dans le
//!   contexte de l'appelant. Les minuteurs sont tenus par [`crate::netpoll`].
//!
//! Adresses : tout est rangé en IPv6, les adresses IPv4 sous leur forme `::ffff:a.b.c.d`. Une
//! socket `AF_INET6` liée à `::` reçoit donc aussi l'IPv4, sauf `IPV6_V6ONLY` (double pile de la
//! RFC 3493).
//!
//! Ports : tables de liaison par protocole ([`Binding`]), règles de conflit de Linux
//! (`SO_REUSEADDR`, `SO_REUSEPORT`), ports éphémères de [`crate::portsel`] (RFC 6056 durcie).
//! Les groupes `SO_REUSEPORT` répartissent les connexions et les datagrammes par un hachage du
//! flux (SipHash avec clé du noyau) : chaque processus d'un serveur a sa file d'acceptation.
//!
//! UDP-Lite (RFC 3828, `IPPROTO_UDPLITE`) : la sémantique d'UDP sur un espace de ports à lui,
//! avec une somme de contrôle qui ne couvre que les `UDPLITE_SEND_CSCOV` premiers octets (en-tête
//! compris ; 0 = tout). Le récepteur rejette, comme Linux, un datagramme partiellement couvert
//! moins que son `UDPLITE_RECV_CSCOV`. smoltcp ne connaît pas ce protocole : sur le réseau, chaque
//! socket a une socket brute IPv4 du protocole 136, l'en-tête et la somme (pseudo-en-tête + octets
//! couverts) sont faits ici, et les paquets reçus sont triés par port et vérifiés ici.
//!
//! Espaces réseau ([`crate::netns`]) : chaque socket garde l'espace où elle est née
//! (`InetSocket::ns`) ; ses tables de ports en viennent ([`Ports`]), et seul l'espace initial
//! atteint les cartes (smoltcp). Ailleurs, seul le bouclage existe, selon l'état de `lo`.
//!
//! Verrous, toujours dans cet ordre : interface smoltcp (`inner`, puis `sockets`, bloquants),
//! table des ports, état d'**une** socket. Jamais d'appel à smoltcp sous un état de socket ;
//! jamais d'`Arc` de socket relâché sous un verrou de smoltcp (son `Drop` le reprend).

use alloc::{
    collections::{BTreeMap, VecDeque},
    format,
    string::String,
    sync::{Arc, Weak},
    task::Wake,
    vec,
    vec::Vec,
};
use core::sync::atomic::Ordering;
use core::task::Waker;
use core::time::Duration;
use net::{raw, stime, tcp, udp, wire, IpAddress, IpEndpoint, IpListenEndpoint, NetworkInterface, SocketHandle};
use sync_preemption::PreemptionSafeMutex as Mutex;

use crate::linux::errno::*;
use crate::linux::fault;
use crate::netns::NetNs;
use crate::object::{ev, new_ino, object_any, FileObject, S_IFSOCK};
use crate::socket::*;
use crate::wait::{wait_event, WaitError, WaitQueue};
use crate::{netpoll, portsel, Thread};

pub const IPPROTO_IP: i32 = 0;
pub const IPPROTO_TCP: i32 = 6;
pub const IPPROTO_UDP: i32 = 17;
pub const IPPROTO_IPV6: i32 = 41;
pub const IPPROTO_UDPLITE: i32 = 136;
/// Niveau des options d'UDP-Lite (`SOL_UDPLITE` = numéro du protocole).
const SOL_UDPLITE: i32 = 136;
const UDPLITE_SEND_CSCOV: i32 = 10;
const UDPLITE_RECV_CSCOV: i32 = 11;
/// En-têtes IPv4 (sans options) et UDP / UDP-Lite.
const IPV4_HDR: usize = 20;
const UDP_HDR: usize = 8;

const SOMAXCONN: usize = 4096;
/// Plus grand datagramme UDP (65535 − en-têtes IPv4 et UDP).
const UDP_MAX: usize = 65507;
/// Tampons d'une connexion smoltcp (fenêtre annoncée, avec mise à l'échelle).
const NET_TCP_BUF: usize = 256 * 1024;
/// Sockets smoltcp en écoute par écouteur (connexions en cours d'établissement simultanées).
const NET_LISTENERS: usize = 8;
/// Établissement d'une connexion : `tcp_syn_retries` = 6 (≈ 127 s).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(127);

const TCP_NODELAY: i32 = 1;
const TCP_MAXSEG: i32 = 2;
const TCP_INFO: i32 = 11;
const TCP_CONGESTION: i32 = 13;
const IP_TTL: i32 = 2;
const IP_MTU: i32 = 14;
const IPV6_UNICAST_HOPS: i32 = 16;
const IPV6_MTU: i32 = 24;
const IPV6_V6ONLY: i32 = 26;
const IP_TOS: i32 = 1;
const IP_PKTINFO: i32 = 8;
const IP_RECVTTL: i32 = 12;
const IP_RECVTOS: i32 = 13;
const IPV6_RECVPKTINFO: i32 = 49;
const IPV6_PKTINFO: i32 = 50;
const IPV6_RECVHOPLIMIT: i32 = 51;
const IPV6_HOPLIMIT: i32 = 52;
const IPV6_RECVTCLASS: i32 = 66;
const IPV6_TCLASS: i32 = 67;

/// États TCP de Linux (`/proc/net/tcp`, `TCP_INFO`).
pub mod tcp_state {
    pub const ESTABLISHED: u8 = 1;
    pub const SYN_SENT: u8 = 2;
    pub const SYN_RECV: u8 = 3;
    pub const FIN_WAIT1: u8 = 4;
    pub const FIN_WAIT2: u8 = 5;
    pub const TIME_WAIT: u8 = 6;
    pub const CLOSE: u8 = 7;
    pub const CLOSE_WAIT: u8 = 8;
    pub const LAST_ACK: u8 = 9;
    pub const LISTEN: u8 = 10;
    pub const CLOSING: u8 = 11;
}

// --- Adresses --------------------------------------------------------------------------------

const MAPPED: [u8; 12] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff];
const LOOP6: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];

/// Adresse et port ; IPv4 rangée en `::ffff:a.b.c.d`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Addr {
    pub ip: [u8; 16],
    pub port: u16,
}

impl Addr {
    pub fn v4(ip: [u8; 4], port: u16) -> Addr {
        let mut a = [0u8; 16];
        a[..12].copy_from_slice(&MAPPED);
        a[12..].copy_from_slice(&ip);
        Addr { ip: a, port }
    }

    /// Adresse quelconque de la famille.
    pub fn any(family: i32) -> Addr {
        if family == AF_INET { Addr::v4([0; 4], 0) } else { Addr::default() }
    }

    pub fn is_v4(&self) -> bool {
        self.ip[..12] == MAPPED
    }

    pub fn v4_bytes(&self) -> [u8; 4] {
        [self.ip[12], self.ip[13], self.ip[14], self.ip[15]]
    }

    pub fn is_any(&self) -> bool {
        self.ip == [0; 16] || (self.is_v4() && self.v4_bytes() == [0; 4])
    }

    pub fn is_loopback(&self) -> bool {
        (self.is_v4() && self.ip[12] == 127) || self.ip == LOOP6
    }

    fn with_port(self, port: u16) -> Addr {
        Addr { port, ..self }
    }

    fn ip_addr(&self) -> IpAddress {
        if self.is_v4() {
            IpAddress::Ipv4(wire::Ipv4Address(self.v4_bytes()))
        } else {
            IpAddress::Ipv6(wire::Ipv6Address(self.ip))
        }
    }

    fn from_ip(ip: IpAddress, port: u16) -> Addr {
        match ip {
            IpAddress::Ipv4(a) => Addr::v4(a.0, port),
            IpAddress::Ipv6(a) => Addr { ip: a.0, port },
        }
    }

    fn endpoint(&self) -> IpEndpoint {
        IpEndpoint::new(self.ip_addr(), self.port)
    }

    /// Bouclage de la même famille (destination `0.0.0.0` ou `::` : la machine elle-même).
    fn loopback_like(&self) -> Addr {
        if self.is_v4() { Addr::v4([127, 0, 0, 1], self.port) } else { Addr { ip: LOOP6, port: self.port } }
    }

    /// `sockaddr_in` (16 octets) ou `sockaddr_in6` (28 octets).
    pub fn encode(&self, family: i32) -> Vec<u8> {
        let mut v = Vec::with_capacity(28);
        v.extend_from_slice(&(family as u16).to_le_bytes());
        v.extend_from_slice(&self.port.to_be_bytes());
        if family == AF_INET {
            v.extend_from_slice(&self.v4_bytes());
            v.extend_from_slice(&[0; 8]);
        } else {
            v.extend_from_slice(&[0; 4]);
            v.extend_from_slice(&self.ip);
            v.extend_from_slice(&[0; 4]);
        }
        v
    }

    /// Texte de `/proc/net/tcp` : adresse en mots de 32 bits de l'ordre de la machine.
    pub fn proc_hex(&self, v4: bool) -> String {
        let words: &[u8] = if v4 { &self.ip[12..] } else { &self.ip };
        let mut s = String::new();
        for w in words.chunks_exact(4) {
            s += &format!("{:08X}", u32::from_le_bytes(w.try_into().unwrap()));
        }
        s + &format!(":{:04X}", self.port)
    }
}

/// Adresses IP des interfaces (hors bouclage), avec leur interface. Prend le verrou de chaque
/// interface : jamais sous un verrou d'état de socket.
fn iface_addrs() -> Vec<(Arc<NetworkInterface>, Addr)> {
    let ifaces: Vec<Arc<NetworkInterface>> = net::get_interfaces().lock().clone();
    let mut out = Vec::new();
    for i in ifaces {
        for c in i.ip_addrs() {
            out.push((i.clone(), Addr::from_ip(c.address(), 0)));
        }
    }
    out
}

/// Destination servie par le chemin rapide dans l'espace `ns` (adresse de la table `local`).
fn is_local(ns: &NetNs, a: &Addr) -> bool {
    if ns.init {
        return a.is_any() || a.is_loopback() || iface_addrs().iter().any(|(_, x)| x.ip == a.ip);
    }
    let lo = ns.lo();
    if a.is_any() {
        lo.up
    } else if a.is_v4() {
        lo.local4(a.v4_bytes())
    } else {
        lo.local6(a.ip)
    }
}

/// Adresse source pour joindre `dst` (une destination locale).
fn source_for(dst: &Addr) -> Addr {
    if dst.is_any() || dst.is_loopback() { dst.loopback_like().with_port(0) } else { dst.with_port(0) }
}

/// Première adresse IPv4 d'une interface.
fn iface_v4(i: &NetworkInterface) -> Option<Addr> {
    i.ip_addrs().into_iter().map(|c| Addr::from_ip(c.address(), 0)).find(|a| a.is_v4())
}

// --- Tables de liaison -----------------------------------------------------------------------

/// Liaison d'une socket à un port.
#[derive(Clone)]
struct Binding {
    addr: Addr,
    v6only: bool,
    reuseaddr: bool,
    reuseport: bool,
    listening: bool,
    /// Pair d'une socket connectée (UDP : seuls ses datagrammes sont reçus).
    connected: Option<Addr>,
    sock: Weak<InetSocket>,
}

impl Binding {
    /// La liaison reçoit-elle ce qui est adressé à `ip` ?
    fn covers(&self, ip: &[u8; 16]) -> bool {
        let mapped = ip[..12] == MAPPED;
        if self.addr.ip == *ip {
            return true;
        }
        if self.addr.ip == [0; 16] {
            return !(mapped && self.v6only);
        }
        self.addr.is_v4() && self.addr.is_any() && mapped
    }

    fn overlaps(&self, o: &Binding) -> bool {
        self.covers(&o.addr.ip) || o.covers(&self.addr.ip)
    }

    fn alive(&self) -> bool {
        self.sock.strong_count() > 0
    }

    fn is(&self, s: *const InetSocket) -> bool {
        core::ptr::eq(self.sock.as_ptr(), s)
    }
}

type PortTable = BTreeMap<u16, Vec<Binding>>;

/// Tables de liaison d'un espace réseau (`inet_hashinfo` filtrée par `net`).
pub(crate) struct Ports {
    tcp: Mutex<PortTable>,
    udp: Mutex<PortTable>,
    udplite: Mutex<PortTable>,
}

impl Ports {
    pub(crate) const fn new() -> Ports {
        Ports { tcp: Mutex::new(BTreeMap::new()), udp: Mutex::new(BTreeMap::new()), udplite: Mutex::new(BTreeMap::new()) }
    }

    fn of(&self, proto: i32) -> &Mutex<PortTable> {
        match proto {
            IPPROTO_TCP => &self.tcp,
            IPPROTO_UDPLITE => &self.udplite,
            _ => &self.udp,
        }
    }
}

/// Conflit de `b` avec les liaisons du port (règles de `inet_csk_bind_conflict`).
fn conflict(list: &[Binding], b: &Binding, tcp: bool) -> bool {
    let me = b.sock.as_ptr();
    list.iter().filter(|x| x.alive() && !x.is(me)).any(|x| {
        x.overlaps(b)
            && !(x.reuseport && b.reuseport)
            && !(x.reuseaddr && b.reuseaddr && (!tcp || (!x.listening && !b.listening)))
    })
}

/// Choisit parmi des liaisons candidates : la plus précise, puis un membre du groupe
/// `SO_REUSEPORT` par hachage du flux.
fn pick<'a>(cands: Vec<&'a Binding>, dst: &Addr, src: &Addr) -> Option<&'a Binding> {
    let score = |b: &Binding| b.connected.is_some() as u8 * 2 + (b.addr.ip == dst.ip) as u8;
    let best = cands.iter().map(|b| score(b)).max()?;
    let pool: Vec<&Binding> = cands.into_iter().filter(|b| score(b) == best).collect();
    if pool.len() == 1 {
        return pool.first().copied();
    }
    let mut d = [0u8; 36];
    d[..16].copy_from_slice(&src.ip);
    d[16..18].copy_from_slice(&src.port.to_be_bytes());
    d[18..34].copy_from_slice(&dst.ip);
    d[34..36].copy_from_slice(&dst.port.to_be_bytes());
    let i = (portsel::flow_hash(&d) % pool.len() as u64) as usize;
    pool.get(i).copied()
}

/// Écouteur TCP pour `dst` (chemin rapide) dans l'espace `ns`.
fn find_listener(ns: &NetNs, dst: &Addr, src: &Addr) -> Option<Arc<InetSocket>> {
    let tab = ns.ports.tcp.lock();
    let list = tab.get(&dst.port)?;
    let cands: Vec<&Binding> = list.iter().filter(|b| b.listening && b.alive() && b.covers(&dst.ip)).collect();
    pick(cands, dst, src)?.sock.upgrade()
}

/// Destinataire UDP (ou UDP-Lite, selon la table) de `src → dst` (chemin rapide).
fn find_dgram(table: &Mutex<PortTable>, dst: &Addr, src: &Addr) -> Option<Arc<InetSocket>> {
    let tab = table.lock();
    let list = tab.get(&dst.port)?;
    let cands: Vec<&Binding> =
        list.iter().filter(|b| b.alive() && b.covers(&dst.ip) && b.connected.map_or(true, |c| c == *src)).collect();
    pick(cands, dst, src)?.sock.upgrade()
}

/// Sockets liées d'un protocole (`IPPROTO_*`, pour `/proc/net/*`) dans l'espace `ns`.
pub fn bound_sockets(ns: &NetNs, proto: i32) -> Vec<Arc<InetSocket>> {
    let tab = ns.ports.of(proto).lock();
    let mut seen: Vec<*const InetSocket> = Vec::new();
    let mut out = Vec::new();
    for b in tab.values().flatten() {
        let p = b.sock.as_ptr();
        if !seen.contains(&p) {
            seen.push(p);
            if let Some(s) = b.sock.upgrade() {
                out.push(s);
            }
        }
    }
    out
}

// --- Socket -----------------------------------------------------------------------------------

/// Réveille la file d'une socket depuis smoltcp (sans toucher au compteur de la socket : son
/// `Drop` ne doit jamais tourner sous un verrou de la pile).
struct SockWaker(Arc<WaitQueue>);

impl Wake for SockWaker {
    fn wake(self: Arc<Self>) {
        self.0.wake_all();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.wake_all();
    }
}

/// Socket de smoltcp portée par une socket Internet.
struct NetBack {
    iface: Arc<NetworkInterface>,
    h: SocketHandle,
}

enum Tcp {
    Closed,
    Listen {
        backlog: VecDeque<Arc<InetSocket>>,
        max: usize,
        /// Sockets de smoltcp en écoute sur l'interface (`self.net`).
        nl: Vec<SocketHandle>,
    },
    /// Connexion locale (chemin rapide) : le pair.
    Loop(Weak<InetSocket>),
    /// Connexion par smoltcp (`self.net`).
    Net,
}

struct Chunk {
    data: Vec<u8>,
    off: usize,
    from: Addr,
    /// Datagrammes : adresse de destination de l'en-tête, nombre de sauts restant et classe de
    /// trafic (TOS en IPv4) du paquet, pour les données annexes de réception.
    to: Addr,
    hops: u8,
    tclass: u8,
}

struct State {
    local: Option<Addr>,
    remote: Option<Addr>,
    tcp: Tcp,
    rx: VecDeque<Chunk>,
    rx_bytes: usize,
    shut_rd: bool,
    shut_wr: bool,
    /// Le pair n'écrira plus (fin de fichier une fois `rx` vidée).
    eof: bool,
    /// Le pair a fermé : `EPIPE` en écriture (après un premier envoi, voir `peer_reset`).
    peer_gone: bool,
    /// Un envoi a déjà touché le pair fermé, qui a répondu par RST.
    peer_reset: bool,
    error: i64,
    /// Connexion de serveur pas encore acceptée (réinitialisée si l'écouteur ferme).
    unaccepted: bool,
    /// smoltcp : connexion en cours d'établissement, ou établie au moins une fois.
    connecting: bool,
    /// Connexion locale non bloquante déjà faite, annoncée `EINPROGRESS` : le `connect` suivant
    /// rend 0 (puis `EISCONN`), comme une poignée de main achevée sous Linux.
    loop_inprogress: bool,
    established: bool,
}

/// Options des niveaux IP, IPv6, TCP, UDP.
#[derive(Clone, Default)]
struct XOpts {
    v6only: bool,
    nodelay: bool,
    /// UDP-Lite : couvertures demandées à l'envoi et exigée à la réception (`None` : option
    /// jamais posée, couverture totale à l'envoi, tout accepté à la réception).
    pcslen: Option<u16>,
    pcrlen: Option<u16>,
    other: BTreeMap<(i32, i32), Vec<u8>>,
}

/// Socket `AF_INET`/`AF_INET6`.
pub struct InetSocket {
    family: i32,
    ty: i32,
    /// `IPPROTO_TCP`, `IPPROTO_UDP` ou `IPPROTO_UDPLITE`.
    proto: i32,
    /// Espace réseau de naissance (`sock_net`).
    ns: Arc<NetNs>,
    st: Mutex<State>,
    wq: Arc<WaitQueue>,
    opts: Mutex<SockOpts>,
    x: Mutex<XOpts>,
    net: spin::Once<NetBack>,
    ino: u64,
    me: Weak<InetSocket>,
    waker: Waker,
}

/// Pseudo-en-tête IPv4 d'UDP-Lite : adresses, zéro, protocole, longueur du datagramme (RFC 3828
/// § 3.1 : la longueur est celle du paquet IP moins son en-tête, pas la couverture).
fn lite_pseudo(src: &[u8; 4], dst: &[u8; 4], total: usize) -> [u8; 12] {
    let mut p = [0u8; 12];
    p[..4].copy_from_slice(src);
    p[4..8].copy_from_slice(dst);
    p[9] = IPPROTO_UDPLITE as u8;
    p[10..12].copy_from_slice(&(total as u16).to_be_bytes());
    p
}

/// Somme en complément à un (RFC 1071) du pseudo-en-tête puis de `data` (repliée sur 16 bits).
fn ones_sum(pseudo: &[u8], data: &[u8]) -> u16 {
    let mut sum: u64 = 0;
    for part in [pseudo, data] {
        let mut chunks = part.chunks_exact(2);
        for c in &mut chunks {
            sum += u16::from_be_bytes([c[0], c[1]]) as u64;
        }
        if let [b] = chunks.remainder() {
            sum += (*b as u64) << 8;
        }
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    sum as u16
}

/// Somme de contrôle UDP-Lite à poser dans l'en-tête (champ à zéro dans `covered`) ; 0 devient
/// 0xffff (une somme nulle est interdite).
fn lite_checksum(src: &[u8; 4], dst: &[u8; 4], total: usize, covered: &[u8]) -> u16 {
    match !ones_sum(&lite_pseudo(src, dst, total), covered) {
        0 => 0xffff,
        s => s,
    }
}

/// Données annexes demandées à la réception (`IP_PKTINFO`, `IP_RECVTTL`, `IPV6_RECVHOPLIMIT`…).
#[derive(Clone, Copy, Default)]
struct CmsgWants {
    pktinfo4: bool,
    ttl: bool,
    tos: bool,
    pktinfo6: bool,
    hoplimit: bool,
    tclass: bool,
}

/// Données annexes d'un datagramme reçu, dans l'ordre de Linux (`ip_cmsg_recv`,
/// `ip6_datagram_recv_ctl`) : paquet IPv4 → `IP_PKTINFO`, `IP_TTL`, `IP_TOS` ; paquet IPv6 (sur
/// une socket `AF_INET6`) → `IPV6_PKTINFO`, `IPV6_HOPLIMIT`, `IPV6_TCLASS`.
fn recv_cmsgs(c: &Chunk, w: &CmsgWants, family: i32) -> Vec<(i32, i32, Vec<u8>)> {
    let mut out = Vec::new();
    let ifindex: i32 = if c.to.is_loopback() || c.from.is_loopback() { 1 } else { 2 };
    let int = |v: i32| v.to_le_bytes().to_vec();
    if c.from.is_v4() {
        if w.pktinfo4 {
            let mut p = Vec::with_capacity(12);
            p.extend_from_slice(&ifindex.to_le_bytes());
            p.extend_from_slice(&c.to.v4_bytes()); // ipi_spec_dst
            p.extend_from_slice(&c.to.v4_bytes()); // ipi_addr
            out.push((IPPROTO_IP, IP_PKTINFO, p));
        }
        if w.ttl {
            out.push((IPPROTO_IP, IP_TTL, int(c.hops as i32)));
        }
        if w.tos {
            out.push((IPPROTO_IP, IP_TOS, vec![c.tclass]));
        }
    } else if family == AF_INET6 {
        if w.pktinfo6 {
            let mut p = Vec::with_capacity(20);
            p.extend_from_slice(&c.to.ip);
            p.extend_from_slice(&ifindex.to_le_bytes());
            out.push((IPPROTO_IPV6, IPV6_PKTINFO, p));
        }
        if w.hoplimit {
            out.push((IPPROTO_IPV6, IPV6_HOPLIMIT, int(c.hops as i32)));
        }
        if w.tclass {
            out.push((IPPROTO_IPV6, IPV6_TCLASS, int(c.tclass as i32)));
        }
    }
    out
}

fn wait_err(e: WaitError) -> i64 {
    match e {
        WaitError::TimedOut => EAGAIN,
        WaitError::Interrupted => crate::signal::ERESTARTSYS,
    }
}

fn tcp_buffers() -> (tcp::SocketBuffer<'static>, tcp::SocketBuffer<'static>) {
    (tcp::SocketBuffer::new(vec![0u8; NET_TCP_BUF]), tcp::SocketBuffer::new(vec![0u8; NET_TCP_BUF]))
}

fn sigpipe(t: &Thread, flags: u32) {
    if flags & MSG_NOSIGNAL == 0 {
        if let Some(me) = crate::current_thread_arc(t) {
            crate::signal::send_to_thread(
                &me,
                crate::signal::SigInfo::from_process(crate::signal::SIGPIPE, crate::signal::SI_USER, &t.process, &t.process),
                false,
            );
        }
    }
}

/// Lecture d'un flux à travers les morceaux reçus.
fn stream_read(st: &mut State, buf: &mut [u8], peek: bool) -> usize {
    let mut got = 0usize;
    let mut idx = 0usize;
    while got < buf.len() && idx < st.rx.len() {
        let c = &mut st.rx[idx];
        let n = (c.data.len() - c.off).min(buf.len() - got);
        buf[got..got + n].copy_from_slice(&c.data[c.off..c.off + n]);
        got += n;
        if peek {
            idx += 1;
        } else {
            c.off += n;
            if c.off == c.data.len() {
                st.rx.pop_front();
            }
        }
    }
    if !peek {
        st.rx_bytes -= got.min(st.rx_bytes);
    }
    got
}

impl InetSocket {
    fn new(ns: Arc<NetNs>, family: i32, ty: i32) -> Arc<InetSocket> {
        Self::with_proto(ns, family, ty, if ty == SOCK_STREAM { IPPROTO_TCP } else { IPPROTO_UDP })
    }

    fn with_proto(ns: Arc<NetNs>, family: i32, ty: i32, proto: i32) -> Arc<InetSocket> {
        let wq = Arc::new(WaitQueue::new());
        let waker = Waker::from(Arc::new(SockWaker(wq.clone())));
        Arc::new_cyclic(|me| InetSocket {
            family,
            ty,
            proto,
            ns,
            st: Mutex::new(State {
                local: None,
                remote: None,
                tcp: Tcp::Closed,
                rx: VecDeque::new(),
                rx_bytes: 0,
                shut_rd: false,
                shut_wr: false,
                eof: false,
                peer_gone: false,
                peer_reset: false,
                error: 0,
                unaccepted: false,
                connecting: false,
                loop_inprogress: false,
                established: false,
            }),
            wq,
            opts: Mutex::new(SockOpts::default()),
            x: Mutex::new(XOpts::default()),
            net: spin::Once::new(),
            ino: new_ino(),
            me: me.clone(),
            waker,
        })
    }

    pub fn create(ns: Arc<NetNs>, family: i32, ty: i32, protocol: i32) -> Result<Arc<InetSocket>, i64> {
        match (ty, protocol) {
            (SOCK_STREAM, 0 | IPPROTO_TCP) | (SOCK_DGRAM, 0 | IPPROTO_UDP) => Ok(Self::new(ns, family, ty)),
            (SOCK_DGRAM, IPPROTO_UDPLITE) => Ok(Self::with_proto(ns, family, ty, IPPROTO_UDPLITE)),
            (SOCK_STREAM | SOCK_DGRAM, _) => Err(EPROTONOSUPPORT),
            (SOCK_RAW, _) => Err(EPROTONOSUPPORT),
            _ => Err(ESOCKTNOSUPPORT),
        }
    }

    fn is_tcp(&self) -> bool {
        self.ty == SOCK_STREAM
    }

    fn is_lite(&self) -> bool {
        self.proto == IPPROTO_UDPLITE
    }

    fn table(&self) -> &Mutex<PortTable> {
        self.ns.ports.of(self.proto)
    }

    /// Espace réseau de la socket.
    pub fn netns(&self) -> &Arc<NetNs> {
        &self.ns
    }

    /// Carte réseau joignable : seulement depuis l'espace initial.
    fn iface(&self) -> Option<Arc<NetworkInterface>> {
        if self.ns.init { net::get_default_interface() } else { None }
    }

    /// UDP-Lite : couverture (octets, en-tête compris) d'un datagramme de `total` octets
    /// (en-tête compris) envoyé par cette socket — la valeur du champ « longueur », 0 = tout
    /// (`udplite_sender_cscov`).
    fn send_cscov(&self, total: usize) -> u16 {
        match self.x.lock().pcslen {
            Some(c) if (c as usize) < total => c,
            _ => total.min(0xffff) as u16,
        }
    }

    /// UDP-Lite : le récepteur accepte-t-il un datagramme de `total` octets couvert sur `cov`
    /// octets (`udp_queue_rcv_one_skb`) ? Un datagramme entièrement couvert passe toujours.
    fn accepts_cscov(&self, cov: usize, total: usize) -> bool {
        if !self.is_lite() || cov >= total {
            return true;
        }
        match self.x.lock().pcrlen {
            None => true,
            Some(0) => false,
            Some(r) => cov >= r as usize,
        }
    }

    /// Les options qu'hérite une connexion acceptée (comme `sk_clone_lock`).
    fn inherit(&self, from: &InetSocket) {
        {
            let f = from.opts.lock();
            let mut o = self.opts.lock();
            o.rcvbuf = f.rcvbuf;
            o.sndbuf = f.sndbuf;
            o.keepalive = f.keepalive;
            o.linger = f.linger;
            o.reuseaddr = f.reuseaddr;
            o.reuseport = f.reuseport;
            o.rcvtimeo = f.rcvtimeo;
            o.sndtimeo = f.sndtimeo;
        }
        let x = from.x.lock().clone();
        *self.x.lock() = x;
    }

    fn decode(&self, raw: &[u8]) -> Result<Addr, i64> {
        if raw.len() < 2 {
            return Err(EINVAL);
        }
        let fam = u16::from_le_bytes([raw[0], raw[1]]) as i32;
        if self.family == AF_INET {
            if raw.len() < 16 {
                return Err(EINVAL);
            }
            if fam != AF_INET {
                return Err(EAFNOSUPPORT);
            }
            Ok(Addr::v4([raw[4], raw[5], raw[6], raw[7]], u16::from_be_bytes([raw[2], raw[3]])))
        } else {
            if raw.len() < 24 {
                return Err(EINVAL);
            }
            if fam != AF_INET6 {
                return Err(EAFNOSUPPORT);
            }
            let a = Addr { ip: raw[8..24].try_into().unwrap(), port: u16::from_be_bytes([raw[2], raw[3]]) };
            if a.is_v4() && self.x.lock().v6only {
                return Err(ENETUNREACH);
            }
            Ok(a)
        }
    }

    // --- Liaisons ---

    /// Lie à `addr` ; port 0 : port éphémère (vers `dst` pour une connexion).
    fn bind_port(&self, addr: Addr, dst: Option<Addr>) -> Result<Addr, i64> {
        let tcp = self.is_tcp();
        let (reuseaddr, reuseport) = {
            let o = self.opts.lock();
            (o.reuseaddr, o.reuseport)
        };
        let v6only = self.x.lock().v6only;
        let mut b = Binding { addr, v6only, reuseaddr, reuseport, listening: false, connected: dst, sock: self.me.clone() };
        let mut tab = self.table().lock();
        let port = if addr.port != 0 {
            if tab.get(&addr.port).map_or(false, |l| conflict(l, &b, tcp)) {
                return Err(EADDRINUSE);
            }
            addr.port
        } else {
            let free = |p: u16| {
                let mut c = b.clone();
                c.addr.port = p;
                tab.get(&p).map_or(true, |l| !conflict(l, &c, tcp))
            };
            match dst {
                Some(d) => portsel::connect_port(&addr.ip, &d.ip, d.port, free).ok_or(EADDRNOTAVAIL)?,
                None => portsel::bind_port(free).ok_or(EADDRINUSE)?,
            }
        };
        b.addr.port = port;
        let list = tab.entry(port).or_default();
        list.retain(|x| x.alive());
        list.push(b);
        Ok(addr.with_port(port))
    }

    /// Liaison d'une connexion acceptée (même port que l'écouteur, sans contrôle de conflit).
    fn bind_child(&self, local: Addr, remote: Addr) {
        let (reuseaddr, reuseport) = {
            let o = self.opts.lock();
            (o.reuseaddr, o.reuseport)
        };
        let v6only = self.x.lock().v6only;
        let b = Binding { addr: local, v6only, reuseaddr, reuseport, listening: false, connected: Some(remote), sock: self.me.clone() };
        let mut tab = self.table().lock();
        let list = tab.entry(local.port).or_default();
        list.retain(|x| x.alive());
        list.push(b);
    }

    fn update_binding(&self, port: u16, f: impl FnOnce(&mut Binding)) {
        let me = self as *const InetSocket;
        if let Some(b) = self.table().lock().get_mut(&port).and_then(|l| l.iter_mut().find(|b| b.is(me))) {
            f(b);
        }
    }

    /// Adresse locale d'une connexion vers `dst` (liaison automatique au besoin).
    fn local_for(&self, src: Addr, dst: Addr) -> Result<Addr, i64> {
        let cur = self.st.lock().local;
        let local = match cur {
            Some(l) => {
                let l = if l.is_any() { Addr { ip: src.ip, port: l.port } } else { l };
                self.update_binding(l.port, |b| {
                    b.addr = l;
                    b.connected = Some(dst);
                });
                l
            }
            None => self.bind_port(src, Some(dst))?,
        };
        self.st.lock().local = Some(local);
        Ok(local)
    }

    // --- Pile smoltcp ---

    /// Socket UDP de smoltcp liée au port local, si l'adresse locale couvre l'interface.
    fn udp_net_open(&self, local: Addr) -> Option<&NetBack> {
        if let Some(nb) = self.net.get() {
            return Some(nb);
        }
        let iface = self.iface()?;
        let ip = iface_v4(&iface)?;
        let v6only = self.x.lock().v6only;
        if !(local.ip == ip.ip || (local.is_any() && !(local.ip == [0; 16] && v6only))) {
            return None;
        }
        let h = if self.is_lite() {
            // UDP-Lite : socket brute du protocole 136, tri par port fait ici.
            let mut set = iface.sockets.lock();
            let mut s = raw::Socket::new(
                wire::IpVersion::Ipv4,
                wire::IpProtocol::Unknown(IPPROTO_UDPLITE as u8),
                raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY; 256], vec![0u8; DEFAULT_BUF]),
                raw::PacketBuffer::new(vec![raw::PacketMetadata::EMPTY; 256], vec![0u8; DEFAULT_BUF]),
            );
            s.register_recv_waker(&self.waker);
            set.add(s)
        } else {
            let mut set = iface.sockets.lock();
            let mut s = udp::Socket::new(
                udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 256], vec![0u8; DEFAULT_BUF]),
                udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 256], vec![0u8; DEFAULT_BUF]),
            );
            let ep = IpListenEndpoint { addr: if local.is_any() { None } else { Some(local.ip_addr()) }, port: local.port };
            s.bind(ep).ok()?;
            s.register_recv_waker(&self.waker);
            set.add(s)
        };
        netpoll::ACTIVE.fetch_add(1, Ordering::AcqRel);
        netpoll::ensure_started();
        let mut created = false;
        let nb = self.net.call_once(|| {
            created = true;
            NetBack { iface: iface.clone(), h }
        });
        if !created {
            iface.sockets.lock().remove(h);
            netpoll::ACTIVE.fetch_sub(1, Ordering::AcqRel);
        }
        Some(nb)
    }

    /// Écouteurs smoltcp d'un écouteur lié à `local` (adresse quelconque ou celle de l'interface).
    fn open_net_listeners(&self, local: Addr) -> Vec<SocketHandle> {
        let Some(iface) = self.iface() else { return Vec::new() };
        let Some(ip) = iface_v4(&iface) else { return Vec::new() };
        let v6only = self.x.lock().v6only;
        if !(local.ip == ip.ip || (local.is_any() && !(local.ip == [0; 16] && v6only))) {
            return Vec::new();
        }
        let ep = IpListenEndpoint { addr: if local.is_any() { None } else { Some(local.ip_addr()) }, port: local.port };
        let mut nl = Vec::new();
        {
            let mut set = iface.sockets.lock();
            for _ in 0..NET_LISTENERS {
                let (rx, tx) = tcp_buffers();
                let mut s = tcp::Socket::new(rx, tx);
                if s.listen(ep).is_err() {
                    break;
                }
                s.register_recv_waker(&self.waker);
                nl.push(set.add(s));
            }
        }
        if nl.is_empty() {
            return nl;
        }
        netpoll::ACTIVE.fetch_add(nl.len(), Ordering::AcqRel);
        netpoll::ensure_started();
        let h0 = nl[0];
        self.net.call_once(|| NetBack { iface, h: h0 });
        nl
    }

    /// Connexions établies par smoltcp : passées dans la file d'acceptation, écouteurs remplacés.
    fn harvest(&self) {
        let Some(nb) = self.net.get() else { return };
        let mut ready: Vec<(SocketHandle, Addr, Addr)> = Vec::new();
        {
            let mut set = nb.iface.sockets.lock();
            let local = self.st.lock().local.unwrap_or_default();
            let ep = IpListenEndpoint { addr: if local.is_any() { None } else { Some(local.ip_addr()) }, port: local.port };
            let (handles, room) = {
                let st = self.st.lock();
                match &st.tcp {
                    Tcp::Listen { nl, backlog, max } => (nl.clone(), max.saturating_sub(backlog.len())),
                    _ => return,
                }
            };
            let mut replaced = Vec::new();
            for h in handles {
                let s = set.get_mut::<tcp::Socket>(h);
                match s.state() {
                    tcp::State::Established | tcp::State::CloseWait if ready.len() < room => {
                        let l = s.local_endpoint().map(|e| Addr::from_ip(e.addr, e.port)).unwrap_or(local);
                        let r = s.remote_endpoint().map(|e| Addr::from_ip(e.addr, e.port)).unwrap_or_default();
                        ready.push((h, l, r));
                        let (rx, tx) = tcp_buffers();
                        let mut n = tcp::Socket::new(rx, tx);
                        let _ = n.listen(ep);
                        n.register_recv_waker(&self.waker);
                        replaced.push((h, set.add(n)));
                    }
                    tcp::State::Closed => {
                        let _ = s.listen(ep);
                        s.register_recv_waker(&self.waker);
                    }
                    _ => s.register_recv_waker(&self.waker),
                }
            }
            if !replaced.is_empty() {
                netpoll::ACTIVE.fetch_add(replaced.len(), Ordering::AcqRel);
                let mut st = self.st.lock();
                if let Tcp::Listen { nl, .. } = &mut st.tcp {
                    for (old, new) in replaced {
                        if let Some(x) = nl.iter_mut().find(|x| **x == old) {
                            *x = new;
                        }
                    }
                }
            }
        }
        if ready.is_empty() {
            return;
        }
        // Connexions créées hors des verrous de la pile.
        let mut children = Vec::new();
        for (h, l, r) in ready {
            let c = InetSocket::new(self.ns.clone(), self.family, SOCK_STREAM);
            c.inherit(self);
            c.net.call_once(|| NetBack { iface: nb.iface.clone(), h });
            {
                let mut cs = c.st.lock();
                cs.local = Some(l);
                cs.remote = Some(r);
                cs.tcp = Tcp::Net;
                cs.established = true;
                cs.unaccepted = true;
            }
            c.bind_child(l, r);
            {
                let mut set = nb.iface.sockets.lock();
                let s = set.get_mut::<tcp::Socket>(h);
                s.set_nagle_enabled(!c.x.lock().nodelay);
                s.register_recv_waker(&c.waker);
                s.register_send_waker(&c.waker);
            }
            children.push(c);
        }
        let mut leftover = Vec::new();
        {
            let mut st = self.st.lock();
            match &mut st.tcp {
                Tcp::Listen { backlog, .. } => backlog.extend(children),
                _ => leftover = children,
            }
        }
        drop(leftover);
        self.wq.wake_all();
    }

    // --- TCP : connexion ---

    fn connect_tcp(&self, t: &Thread, dst: Addr, nonblock: bool) -> Result<(), i64> {
        {
            let mut st = self.st.lock();
            match st.tcp {
                Tcp::Loop(_) if st.loop_inprogress => {
                    st.loop_inprogress = false;
                    return Ok(());
                }
                Tcp::Loop(_) => return Err(EISCONN),
                Tcp::Net if st.connecting => {
                    if nonblock {
                        return Err(EALREADY);
                    }
                    drop(st);
                    return self.wait_connected(t);
                }
                Tcp::Net => return Err(EISCONN),
                Tcp::Listen { .. } => return Err(EISCONN),
                Tcp::Closed if st.peer_gone || st.established => return Err(EISCONN),
                Tcp::Closed => {}
            }
        }
        if dst.port == 0 {
            return Err(ECONNREFUSED);
        }
        let dst = if dst.is_any() { dst.loopback_like() } else { dst };
        if is_local(&self.ns, &dst) {
            self.connect_loop(t, dst, nonblock)
        } else {
            self.connect_net(t, dst, nonblock)
        }
    }

    /// Chemin rapide : relie directement à une connexion du serveur.
    fn connect_loop(&self, t: &Thread, dst: Addr, nonblock: bool) -> Result<(), i64> {
        let local = self.local_for(source_for(&dst), dst)?;
        let listener = find_listener(&self.ns, &dst, &local).ok_or(ECONNREFUSED)?;
        let me = self.me.upgrade().ok_or(EINVAL)?;
        let child = InetSocket::new(listener.ns.clone(), listener.family, SOCK_STREAM);
        child.inherit(&listener);
        {
            let mut cs = child.st.lock();
            cs.local = Some(dst);
            cs.remote = Some(local);
            cs.tcp = Tcp::Loop(Arc::downgrade(&me));
            cs.unaccepted = true;
            cs.established = true;
        }
        child.bind_child(dst, local);
        {
            let mut ms = self.st.lock();
            ms.remote = Some(dst);
            ms.tcp = Tcp::Loop(Arc::downgrade(&child));
            ms.established = true;
        }
        let deadline = self.opts.lock().snd_deadline();
        let r = wait_event(&[&listener.wq], deadline, Some(t), || {
            let mut ls = listener.st.lock();
            let Tcp::Listen { backlog, max, .. } = &mut ls.tcp else {
                return Some(Err(ECONNREFUSED));
            };
            if backlog.len() >= *max {
                return if nonblock { Some(Err(EAGAIN)) } else { None };
            }
            backlog.push_back(child.clone());
            Some(Ok(()))
        });
        let r = match r {
            Ok(r) => r,
            Err(e) => Err(wait_err(e)),
        };
        if let Err(e) = r {
            // Défait la paire sans que le `Drop` de la socket du serveur nous raccroche.
            child.st.lock().tcp = Tcp::Closed;
            let mut ms = self.st.lock();
            ms.tcp = Tcp::Closed;
            ms.remote = None;
            ms.established = false;
            return Err(e);
        }
        listener.wq.wake_all();
        if nonblock {
            // Linux : un `connect` TCP non bloquant rend toujours `EINPROGRESS` (le SYN part, la
            // poignée de main s'achève ensuite), même en boucle locale ; la socket est déjà
            // inscriptible et `SO_ERROR` vaut 0. Les programmes (asyncio, `test_epoll`) l'exigent.
            self.st.lock().loop_inprogress = true;
            return Err(EINPROGRESS);
        }
        Ok(())
    }

    /// Réseau : connexion par smoltcp.
    fn connect_net(&self, t: &Thread, dst: Addr, nonblock: bool) -> Result<(), i64> {
        if self.net.get().is_some() {
            return Err(EISCONN);
        }
        let iface = self.iface().ok_or(ENETUNREACH)?;
        let src = iface_v4(&iface).filter(|_| dst.is_v4()).ok_or(ENETUNREACH)?;
        let local = self.local_for(src, dst)?;
        let (nodelay, keepalive) = (self.x.lock().nodelay, self.opts.lock().keepalive);
        let h = {
            let mut inner = iface.inner.lock();
            let mut set = iface.sockets.lock();
            let (rx, tx) = tcp_buffers();
            let mut s = tcp::Socket::new(rx, tx);
            s.set_nagle_enabled(!nodelay);
            if keepalive {
                s.set_keep_alive(Some(stime::Duration::from_secs(75)));
            }
            s.connect(inner.context(), dst.endpoint(), IpListenEndpoint { addr: Some(local.ip_addr()), port: local.port })
                .map_err(|_| EADDRNOTAVAIL)?;
            s.register_recv_waker(&self.waker);
            s.register_send_waker(&self.waker);
            set.add(s)
        };
        self.net.call_once(|| NetBack { iface: iface.clone(), h });
        netpoll::ACTIVE.fetch_add(1, Ordering::AcqRel);
        netpoll::ensure_started();
        {
            let mut st = self.st.lock();
            st.remote = Some(dst);
            st.tcp = Tcp::Net;
            st.connecting = true;
        }
        iface.poll();
        netpoll::kick();
        if nonblock {
            return Err(EINPROGRESS);
        }
        self.wait_connected(t)
    }

    fn wait_connected(&self, t: &Thread) -> Result<(), i64> {
        let nb = self.net.get().ok_or(ENOTCONN)?;
        let deadline = time::Instant::now() + CONNECT_TIMEOUT;
        let r = wait_event(&[&self.wq], Some(deadline), Some(t), || {
            let mut set = nb.iface.sockets.lock();
            let s = set.get_mut::<tcp::Socket>(nb.h);
            match s.state() {
                tcp::State::SynSent | tcp::State::SynReceived => {
                    s.register_send_waker(&self.waker);
                    None
                }
                tcp::State::Closed => Some(Err(ECONNREFUSED)),
                _ => Some(Ok(())),
            }
        });
        let res = match r {
            Ok(r) => r,
            Err(WaitError::TimedOut) => {
                nb.iface.sockets.lock().get_mut::<tcp::Socket>(nb.h).abort();
                Err(ETIMEDOUT)
            }
            Err(WaitError::Interrupted) => return Err(crate::signal::ERESTARTSYS),
        };
        let mut st = self.st.lock();
        st.connecting = false;
        match res {
            Ok(()) => st.established = true,
            Err(_) => {
                st.tcp = Tcp::Closed;
                st.remote = None;
            }
        }
        res
    }

    // --- TCP : données ---

    fn send_loop(&self, t: &Thread, m: SendMsg, nonblock: bool) -> Result<usize, i64> {
        let len = m.data.len();
        if len == 0 {
            return Ok(0);
        }
        let deadline = self.opts.lock().snd_deadline();
        let mut data = Some(m.data);
        let mut done = 0usize;
        while done < len {
            let step = wait_event(&[&self.wq], deadline, Some(t), || {
                let peer = {
                    let mut st = self.st.lock();
                    if st.error != 0 {
                        return Some(Err(core::mem::take(&mut st.error)));
                    }
                    if st.shut_wr || (st.peer_gone && st.peer_reset) {
                        return Some(Err(EPIPE));
                    }
                    if st.peer_gone {
                        // Comme Linux (CLOSE_WAIT) : l'envoi vers un pair qui a fermé réussit ;
                        // le pair répond par RST, qui pose `EPIPE` pour la suite (et SIGPIPE au
                        // prochain envoi). Les données sont perdues.
                        st.peer_reset = true;
                        st.error = EPIPE;
                        return Some(Ok(len - done));
                    }
                    match &st.tcp {
                        Tcp::Loop(w) => w.upgrade(),
                        _ => return Some(Err(ENOTCONN)),
                    }
                };
                let Some(peer) = peer else { return Some(Err(EPIPE)) };
                let cap = peer.opts.lock().rcvbuf;
                let mut ps = peer.st.lock();
                let free = cap.saturating_sub(ps.rx_bytes);
                if free == 0 && !ps.rx.is_empty() {
                    return if nonblock { Some(Err(EAGAIN)) } else { None };
                }
                let rest = len - done;
                let n = rest.min(free.max(64 * 1024));
                let chunk = if done == 0 && n == len { data.take().unwrap_or_default() } else { data.as_ref().map_or(Vec::new(), |d| d[done..done + n].to_vec()) };
                ps.rx.push_back(Chunk { data: chunk, off: 0, from: Addr::default(), to: Addr::default(), hops: 0, tclass: 0 });
                ps.rx_bytes += n;
                drop(ps);
                peer.wq.wake_all();
                Some(Ok(n))
            });
            match step {
                Ok(Ok(n)) => done += n,
                Ok(Err(EPIPE)) => {
                    sigpipe(t, m.flags);
                    return if done > 0 { Ok(done) } else { Err(EPIPE) };
                }
                Ok(Err(e)) => return if done > 0 { Ok(done) } else { Err(e) },
                Err(e) => return if done > 0 { Ok(done) } else { Err(wait_err(e)) },
            }
        }
        Ok(done)
    }

    /// Place libérée dans notre file : l'écrivain (le pair) attend sur sa propre file.
    fn consumed(&self) {
        let peer = match &self.st.lock().tcp {
            Tcp::Loop(w) => w.upgrade(),
            _ => None,
        };
        if let Some(p) = peer {
            p.wq.wake_all();
        }
    }

    fn recv_loop(&self, t: &Thread, buf: &mut [u8], flags: u32, nonblock: bool) -> Result<RecvMeta, i64> {
        let peek = flags & MSG_PEEK != 0;
        let waitall = flags & MSG_WAITALL != 0 && !peek;
        let deadline = self.opts.lock().rcv_deadline();
        let mut total = 0usize;
        loop {
            let r = wait_event(&[&self.wq], deadline, Some(t), || {
                let mut st = self.st.lock();
                if !st.rx.is_empty() {
                    return Some(Ok(stream_read(&mut st, &mut buf[total..], peek)));
                }
                if st.error != 0 {
                    return Some(Err(core::mem::take(&mut st.error)));
                }
                if st.eof || st.shut_rd {
                    return Some(Ok(0));
                }
                if !matches!(st.tcp, Tcp::Loop(_)) {
                    return Some(Err(ENOTCONN));
                }
                if nonblock {
                    return Some(Err(EAGAIN));
                }
                None
            });
            match r {
                Ok(Ok(n)) => {
                    if !peek && n > 0 {
                        self.consumed();
                    }
                    total += n;
                    if !waitall || n == 0 || total >= buf.len() {
                        break;
                    }
                }
                Ok(Err(e)) if total == 0 => return Err(e),
                Err(e) if total == 0 => return Err(wait_err(e)),
                _ => break,
            }
        }
        Ok(RecvMeta { len: total, full_len: total, ..Default::default() })
    }

    fn send_net(&self, t: &Thread, m: SendMsg, nonblock: bool) -> Result<usize, i64> {
        let nb = self.net.get().ok_or(ENOTCONN)?;
        let len = m.data.len();
        let deadline = self.opts.lock().snd_deadline();
        let mut done = 0usize;
        while done < len {
            let r = wait_event(&[&self.wq], deadline, Some(t), || {
                let mut set = nb.iface.sockets.lock();
                let s = set.get_mut::<tcp::Socket>(nb.h);
                if self.st.lock().shut_wr {
                    return Some(Err(EPIPE));
                }
                match s.state() {
                    tcp::State::SynSent | tcp::State::SynReceived => {
                        s.register_send_waker(&self.waker);
                        return if nonblock { Some(Err(EAGAIN)) } else { None };
                    }
                    _ if !s.may_send() => return Some(Err(EPIPE)),
                    _ => {}
                }
                if s.can_send() {
                    return Some(s.send_slice(&m.data[done..]).map_err(|_| EPIPE));
                }
                s.register_send_waker(&self.waker);
                if nonblock { Some(Err(EAGAIN)) } else { None }
            });
            match r {
                Ok(Ok(n)) => {
                    done += n;
                    // Émission immédiate dans le contexte de l'appelant.
                    nb.iface.poll();
                    netpoll::kick();
                }
                Ok(Err(EPIPE)) => {
                    sigpipe(t, m.flags);
                    return if done > 0 { Ok(done) } else { Err(EPIPE) };
                }
                Ok(Err(e)) => return if done > 0 { Ok(done) } else { Err(e) },
                Err(e) => return if done > 0 { Ok(done) } else { Err(wait_err(e)) },
            }
        }
        Ok(done)
    }

    fn recv_net(&self, t: &Thread, buf: &mut [u8], flags: u32, nonblock: bool) -> Result<RecvMeta, i64> {
        let nb = self.net.get().ok_or(ENOTCONN)?;
        let peek = flags & MSG_PEEK != 0;
        let waitall = flags & MSG_WAITALL != 0 && !peek;
        let deadline = self.opts.lock().rcv_deadline();
        let mut total = 0usize;
        loop {
            let r = wait_event(&[&self.wq], deadline, Some(t), || {
                let mut set = nb.iface.sockets.lock();
                let s = set.get_mut::<tcp::Socket>(nb.h);
                if s.can_recv() {
                    let r = if peek { s.peek_slice(&mut buf[total..]) } else { s.recv_slice(&mut buf[total..]) };
                    return Some(r.map_err(|_| ENOTCONN));
                }
                if self.st.lock().shut_rd {
                    return Some(Ok(0));
                }
                match s.state() {
                    tcp::State::SynSent | tcp::State::SynReceived => {}
                    tcp::State::Closed if !self.st.lock().established => return Some(Err(ENOTCONN)),
                    _ if !s.may_recv() => return Some(Ok(0)),
                    _ => {}
                }
                s.register_recv_waker(&self.waker);
                if nonblock { Some(Err(EAGAIN)) } else { None }
            });
            match r {
                Ok(Ok(n)) => {
                    if !peek && n > 0 {
                        // Fenêtre rouverte : la mise à jour part au prochain passage.
                        netpoll::kick();
                    }
                    total += n;
                    if !waitall || n == 0 || total >= buf.len() {
                        break;
                    }
                }
                Ok(Err(e)) if total == 0 => return Err(e),
                Err(e) if total == 0 => return Err(wait_err(e)),
                _ => break,
            }
        }
        Ok(RecvMeta { len: total, full_len: total, ..Default::default() })
    }

    // --- UDP ---

    fn ensure_udp_bound(&self) -> Result<Addr, i64> {
        if let Some(l) = self.st.lock().local {
            return Ok(l);
        }
        let l = self.bind_port(Addr::any(self.family), None)?;
        self.st.lock().local = Some(l);
        self.udp_net_open(l);
        Ok(l)
    }

    fn send_udp(&self, t: &Thread, m: SendMsg, nonblock: bool) -> Result<usize, i64> {
        let len = m.data.len();
        if len > UDP_MAX {
            return Err(EMSGSIZE);
        }
        let dst = match &m.addr {
            Some(raw) => self.decode(raw)?,
            None => self.st.lock().remote.ok_or(EDESTADDRREQ)?,
        };
        {
            let mut st = self.st.lock();
            if st.shut_wr {
                drop(st);
                sigpipe(t, m.flags);
                return Err(EPIPE);
            }
            if st.error != 0 && st.remote.is_some() {
                return Err(core::mem::take(&mut st.error));
            }
        }
        if dst.port == 0 {
            return Err(EINVAL);
        }
        let dst = if dst.is_any() { dst.loopback_like() } else { dst };
        let local = self.ensure_udp_bound()?;
        if is_local(&self.ns, &dst) {
            let src_ip = match m.ip.src {
                Some(a) if is_local(&self.ns, &Addr { ip: a, port: 0 }) => a,
                Some(_) => return Err(EINVAL),
                None if local.is_any() => source_for(&dst).ip,
                None => local.ip,
            };
            let src = Addr { ip: src_ip, port: local.port };
            let (hops, tclass) = self.packet_hops_class(&dst, &m.ip);
            match find_dgram(self.table(), &dst, &src) {
                Some(r) => {
                    let cap = r.opts.lock().rcvbuf;
                    // UDP-Lite : couverture choisie par l'émetteur, jugée par le récepteur.
                    let total = len + UDP_HDR;
                    let cov = match self.send_cscov(total) {
                        0 => total,
                        c => c as usize,
                    };
                    let covered = r.accepts_cscov(cov, total);
                    let mut rs = r.st.lock();
                    // UDP : un datagramme qui ne tient pas est perdu, sans erreur.
                    if covered && !rs.shut_rd && (rs.rx_bytes + len <= cap || rs.rx.is_empty()) {
                        rs.rx_bytes += len;
                        rs.rx.push_back(Chunk { data: m.data, off: 0, from: src, to: dst, hops, tclass });
                        drop(rs);
                        r.wq.wake_all();
                    }
                }
                None => {
                    // « Port inaccessible » : signalé aux seules sockets connectées (ICMP).
                    let mut st = self.st.lock();
                    if st.remote.is_some() {
                        st.error = ECONNREFUSED;
                        drop(st);
                        self.wq.wake_all();
                    }
                }
            }
            return Ok(len);
        }
        let nb = self.udp_net_open(local).ok_or(ENETUNREACH)?;
        if !dst.is_v4() {
            return Err(ENETUNREACH);
        }
        if self.is_lite() {
            return self.send_lite_net(t, nb, local, dst, &m.data, nonblock);
        }
        let deadline = self.opts.lock().snd_deadline();
        let r = wait_event(&[&self.wq], deadline, Some(t), || {
            let mut set = nb.iface.sockets.lock();
            let s = set.get_mut::<udp::Socket>(nb.h);
            match s.send_slice(&m.data, dst.endpoint()) {
                Ok(()) => Some(Ok(len)),
                Err(udp::SendError::Unaddressable) => Some(Err(EINVAL)),
                Err(udp::SendError::BufferFull) => {
                    s.register_send_waker(&self.waker);
                    if nonblock { Some(Err(EAGAIN)) } else { None }
                }
            }
        });
        let r = r.map_err(wait_err)?;
        nb.iface.poll();
        netpoll::kick();
        r
    }

    fn recv_udp(&self, t: &Thread, buf: &mut [u8], flags: u32, nonblock: bool) -> Result<RecvMeta, i64> {
        let peek = flags & MSG_PEEK != 0;
        let deadline = self.opts.lock().rcv_deadline();
        let family = self.family;
        let lite = self.is_lite();
        let want = self.cmsg_wants();
        let r = wait_event(&[&self.wq], deadline, Some(t), || {
            if lite {
                self.pump_lite();
            }
            {
                let mut st = self.st.lock();
                if st.error != 0 {
                    return Some(Err(core::mem::take(&mut st.error)));
                }
                let popped = if peek { None } else { st.rx.pop_front() };
                let c = match &popped {
                    Some(c) => Some(c),
                    None => st.rx.front(),
                };
                if let Some(c) = c {
                    let n = c.data.len().min(buf.len());
                    buf[..n].copy_from_slice(&c.data[..n]);
                    let meta = RecvMeta {
                        len: n,
                        full_len: c.data.len(),
                        addr: Some(c.from.encode(family)),
                        flags: if c.data.len() > n { MSG_TRUNC } else { 0 },
                        cmsgs: recv_cmsgs(c, &want, family),
                        ..Default::default()
                    };
                    if popped.is_some() {
                        st.rx_bytes -= meta.full_len.min(st.rx_bytes);
                    }
                    return Some(Ok(meta));
                }
                if st.shut_rd {
                    return Some(Ok(RecvMeta::default()));
                }
            }
            if let (true, Some(nb)) = (lite, self.net.get()) {
                // Rien de valable dans la socket brute : `pump_lite` a tout trié.
                nb.iface.sockets.lock().get_mut::<raw::Socket>(nb.h).register_recv_waker(&self.waker);
            } else if let Some(nb) = self.net.get() {
                let mut set = nb.iface.sockets.lock();
                let s = set.get_mut::<udp::Socket>(nb.h);
                if s.can_recv() {
                    let mut big = vec![0u8; 65536];
                    let got = if peek {
                        s.peek_slice(&mut big).map(|(n, m)| (n, m.endpoint))
                    } else {
                        s.recv_slice(&mut big).map(|(n, m)| (n, m.endpoint))
                    };
                    if let Ok((full, ep)) = got {
                        let n = full.min(buf.len());
                        buf[..n].copy_from_slice(&big[..n]);
                        return Some(Ok(RecvMeta {
                            len: n,
                            full_len: full,
                            addr: Some(Addr::from_ip(ep.addr, ep.port).encode(family)),
                            flags: if full > n { MSG_TRUNC } else { 0 },
                            ..Default::default()
                        }));
                    }
                }
                s.register_recv_waker(&self.waker);
            }
            if nonblock { Some(Err(EAGAIN)) } else { None }
        });
        r.map_err(wait_err)?
    }

    fn connect_udp(&self, dst: Addr) -> Result<(), i64> {
        let dst = if dst.is_any() { dst.loopback_like() } else { dst };
        let local = self.ensure_udp_bound()?;
        // Sans route (espace réseau sans carte) : `ip4_datagram_connect` → `ENETUNREACH`,
        // après la liaison automatique (`inet_dgram_connect`).
        if !is_local(&self.ns, &dst) && self.iface().is_none() {
            return Err(ENETUNREACH);
        }
        let src = if !local.is_any() {
            local
        } else if is_local(&self.ns, &dst) {
            source_for(&dst).with_port(local.port)
        } else {
            self.iface().and_then(|i| iface_v4(&i)).map(|a| a.with_port(local.port)).unwrap_or(local)
        };
        self.update_binding(local.port, |b| {
            b.addr = src;
            b.connected = Some(dst);
        });
        let mut st = self.st.lock();
        st.local = Some(src);
        st.remote = Some(dst);
        st.error = 0;
        Ok(())
    }

    // --- Données annexes IP ---

    /// Option entière posée par `setsockopt` (`None` : jamais posée).
    fn opt_int(&self, level: i32, name: i32) -> Option<i32> {
        self.x.lock().other.get(&(level, name)).and_then(|v| v.get(..4)).map(|b| i32::from_le_bytes(b.try_into().unwrap()))
    }

    fn opt_on(&self, level: i32, name: i32) -> bool {
        self.opt_int(level, name).map_or(false, |v| v != 0)
    }

    /// Nombre de sauts et classe de trafic d'un paquet vers `dst` : donnée annexe du paquet,
    /// sinon option de la socket, sinon 64 et 0 (-1 = défaut, comme Linux).
    fn packet_hops_class(&self, dst: &Addr, ip: &IpCtl) -> (u8, u8) {
        let pick = |cmsg: Option<i32>, opt: Option<i32>, default: i32| -> u8 {
            match cmsg.filter(|&v| v >= 0).or_else(|| opt.filter(|&v| v >= 0)) {
                Some(v) => v as u8,
                None => default as u8,
            }
        };
        if dst.is_v4() {
            (pick(ip.ttl, self.opt_int(IPPROTO_IP, IP_TTL), 64), pick(ip.tos, self.opt_int(IPPROTO_IP, IP_TOS), 0))
        } else {
            (
                pick(ip.hops, self.opt_int(IPPROTO_IPV6, IPV6_UNICAST_HOPS), 64),
                pick(ip.tclass, self.opt_int(IPPROTO_IPV6, IPV6_TCLASS), 0),
            )
        }
    }

    /// Données annexes que la socket demande à la réception (lues avant de prendre son état).
    fn cmsg_wants(&self) -> CmsgWants {
        CmsgWants {
            pktinfo4: self.opt_on(IPPROTO_IP, IP_PKTINFO),
            ttl: self.opt_on(IPPROTO_IP, IP_RECVTTL),
            tos: self.opt_on(IPPROTO_IP, IP_RECVTOS),
            pktinfo6: self.opt_on(IPPROTO_IPV6, IPV6_RECVPKTINFO),
            hoplimit: self.opt_on(IPPROTO_IPV6, IPV6_RECVHOPLIMIT),
            tclass: self.opt_on(IPPROTO_IPV6, IPV6_RECVTCLASS),
        }
    }

    // --- UDP-Lite sur le réseau (socket brute) ---

    /// Envoie un datagramme UDP-Lite par la socket brute : en-tête IPv4, en-tête UDP-Lite,
    /// somme de contrôle sur le pseudo-en-tête et les octets couverts (RFC 3828, § 3.1).
    fn send_lite_net(&self, t: &Thread, nb: &NetBack, local: Addr, dst: Addr, data: &[u8], nonblock: bool) -> Result<usize, i64> {
        let total = UDP_HDR + data.len();
        // Pas de fragmentation IPv4 dans la pile : un datagramme plus grand que la MTU serait
        // perdu en silence.
        if IPV4_HDR + total > 1500 {
            return Err(EMSGSIZE);
        }
        let src_ip = if local.is_any() { iface_v4(&nb.iface).ok_or(ENETUNREACH)?.v4_bytes() } else { local.v4_bytes() };
        let dst_ip = dst.v4_bytes();
        let ttl = self
            .x
            .lock()
            .other
            .get(&(IPPROTO_IP, IP_TTL))
            .and_then(|v| v.get(..4))
            .map(|b| i32::from_le_bytes(b.try_into().unwrap()))
            .filter(|&v| v > 0)
            .unwrap_or(64) as u8;
        let mut pkt = vec![0u8; IPV4_HDR + total];
        {
            let ip = &mut pkt[..IPV4_HDR];
            ip[0] = 0x45;
            ip[2..4].copy_from_slice(&((IPV4_HDR + total) as u16).to_be_bytes());
            ip[4..6].copy_from_slice(&(uvm::rand::random_u64() as u16).to_be_bytes());
            ip[6] = 0x40; // DF : pas de fragmentation
            ip[8] = ttl;
            ip[9] = IPPROTO_UDPLITE as u8;
            ip[12..16].copy_from_slice(&src_ip);
            ip[16..20].copy_from_slice(&dst_ip);
            // Somme de l'en-tête IPv4 : posée par smoltcp.
        }
        let cscov = self.send_cscov(total);
        {
            let u = &mut pkt[IPV4_HDR..];
            u[0..2].copy_from_slice(&local.port.to_be_bytes());
            u[2..4].copy_from_slice(&dst.port.to_be_bytes());
            u[4..6].copy_from_slice(&cscov.to_be_bytes());
            u[UDP_HDR..].copy_from_slice(data);
            let cov = if cscov == 0 { total } else { cscov as usize };
            let sum = lite_checksum(&src_ip, &dst_ip, total, &u[..cov]);
            u[6..8].copy_from_slice(&sum.to_be_bytes());
        }
        let deadline = self.opts.lock().snd_deadline();
        let r = wait_event(&[&self.wq], deadline, Some(t), || {
            let mut set = nb.iface.sockets.lock();
            let s = set.get_mut::<raw::Socket>(nb.h);
            match s.send_slice(&pkt) {
                Ok(()) => Some(Ok(data.len())),
                Err(_) => {
                    s.register_send_waker(&self.waker);
                    if nonblock { Some(Err(EAGAIN)) } else { None }
                }
            }
        });
        let r = r.map_err(wait_err)?;
        nb.iface.poll();
        netpoll::kick();
        r
    }

    /// Trie les paquets UDP-Lite reçus par la socket brute : ceux qui sont pour nous (port,
    /// adresse, pair si connectée), bien formés, à la somme juste et assez couverts passent dans
    /// la file de réception ; les autres sont jetés (RFC 3828, § 3.1 et 3.2).
    fn pump_lite(&self) {
        let Some(nb) = self.net.get() else { return };
        let (local, remote) = {
            let st = self.st.lock();
            (st.local, st.remote)
        };
        let Some(local) = local else { return };
        let mut got: Vec<(Addr, Addr, u8, u8, Vec<u8>)> = Vec::new();
        {
            let mut set = nb.iface.sockets.lock();
            let s = set.get_mut::<raw::Socket>(nb.h);
            let mut buf = vec![0u8; 65536];
            while let Ok(n) = s.recv_slice(&mut buf) {
                if let Some(d) = self.parse_lite(&buf[..n], &local, remote.as_ref()) {
                    got.push(d);
                }
            }
            s.register_recv_waker(&self.waker);
        }
        if got.is_empty() {
            return;
        }
        let cap = self.opts.lock().rcvbuf;
        {
            let mut st = self.st.lock();
            for (from, to, hops, tclass, data) in got {
                if !st.shut_rd && (st.rx_bytes + data.len() <= cap || st.rx.is_empty()) {
                    st.rx_bytes += data.len();
                    st.rx.push_back(Chunk { data, off: 0, from, to, hops, tclass });
                }
            }
        }
        self.wq.wake_all();
    }

    /// Un paquet IPv4 de la socket brute, s'il est un datagramme UDP-Lite valable pour nous.
    fn parse_lite(&self, pkt: &[u8], local: &Addr, remote: Option<&Addr>) -> Option<(Addr, Addr, u8, u8, Vec<u8>)> {
        if pkt.len() < IPV4_HDR || pkt[0] >> 4 != 4 {
            return None;
        }
        let ihl = ((pkt[0] & 0x0f) as usize) * 4;
        let ip_len = u16::from_be_bytes([pkt[2], pkt[3]]) as usize;
        if ihl < IPV4_HDR || ip_len > pkt.len() || ip_len < ihl + UDP_HDR || pkt[9] != IPPROTO_UDPLITE as u8 {
            return None;
        }
        let src_ip: [u8; 4] = pkt[12..16].try_into().ok()?;
        let dst_ip: [u8; 4] = pkt[16..20].try_into().ok()?;
        let u = &pkt[ihl..ip_len];
        let total = u.len();
        let sport = u16::from_be_bytes([u[0], u[1]]);
        let dport = u16::from_be_bytes([u[2], u[3]]);
        let dst = Addr::v4(dst_ip, dport);
        let from = Addr::v4(src_ip, sport);
        if dport != local.port || !(local.is_any() || local.ip == dst.ip) {
            return None;
        }
        if self.family == AF_INET6 && self.x.lock().v6only {
            return None;
        }
        if let Some(r) = remote {
            if *r != from {
                return None;
            }
        }
        // Couverture : 0 = tout ; sinon au moins l'en-tête, au plus le datagramme.
        let cscov = u16::from_be_bytes([u[4], u[5]]) as usize;
        let cov = if cscov == 0 { total } else { cscov };
        if cov < UDP_HDR || cov > total {
            return None;
        }
        // Somme obligatoire en UDP-Lite (0 interdit), vérifiée sur les octets couverts.
        if u16::from_be_bytes([u[6], u[7]]) == 0 || ones_sum(&lite_pseudo(&src_ip, &dst_ip, total), &u[..cov]) != 0xffff {
            return None;
        }
        if !self.accepts_cscov(cov, total) {
            return None;
        }
        Some((from, dst, pkt[8], pkt[1], u[UDP_HDR..].to_vec()))
    }

    // --- État ---

    /// État TCP au sens de Linux.
    pub fn tcp_state(&self) -> u8 {
        use tcp_state::*;
        let (net, loop_state) = {
            let st = self.st.lock();
            match &st.tcp {
                Tcp::Listen { .. } => return LISTEN,
                Tcp::Closed => return CLOSE,
                Tcp::Loop(_) => (false, if st.eof && st.shut_wr { CLOSE } else if st.eof { CLOSE_WAIT } else if st.shut_wr { FIN_WAIT2 } else { ESTABLISHED }),
                Tcp::Net => (true, 0),
            }
        };
        if !net {
            return loop_state;
        }
        let Some(nb) = self.net.get() else { return CLOSE };
        let s = nb.iface.sockets.lock().get::<tcp::Socket>(nb.h).state();
        match s {
            tcp::State::Closed => CLOSE,
            tcp::State::Listen => LISTEN,
            tcp::State::SynSent => SYN_SENT,
            tcp::State::SynReceived => SYN_RECV,
            tcp::State::Established => ESTABLISHED,
            tcp::State::FinWait1 => FIN_WAIT1,
            tcp::State::FinWait2 => FIN_WAIT2,
            tcp::State::CloseWait => CLOSE_WAIT,
            tcp::State::Closing => CLOSING,
            tcp::State::LastAck => LAST_ACK,
            tcp::State::TimeWait => TIME_WAIT,
        }
    }

    /// `(local, distante, octets en attente de lecture)` pour `/proc/net/*`.
    pub fn proc_info(&self) -> (Addr, Addr, usize) {
        let st = self.st.lock();
        (st.local.unwrap_or_else(|| Addr::any(self.family)), st.remote.unwrap_or_else(|| Addr::any(self.family)), st.rx_bytes)
    }

    pub fn family(&self) -> i32 {
        self.family
    }

    pub fn ino(&self) -> u64 {
        self.ino
    }

    fn poll_tcp(&self) -> u32 {
        let (net, listening) = {
            let st = self.st.lock();
            (matches!(st.tcp, Tcp::Net), matches!(st.tcp, Tcp::Listen { .. }))
        };
        if listening {
            self.harvest();
            let st = self.st.lock();
            return match &st.tcp {
                Tcp::Listen { backlog, .. } if !backlog.is_empty() => ev::READABLE,
                _ => 0,
            };
        }
        if net {
            if let Some(nb) = self.net.get() {
                let mut set = nb.iface.sockets.lock();
                let s = set.get_mut::<tcp::Socket>(nb.h);
                s.register_recv_waker(&self.waker);
                s.register_send_waker(&self.waker);
                let st = self.st.lock();
                let state = s.state();
                if matches!(state, tcp::State::SynSent | tcp::State::SynReceived) {
                    return 0;
                }
                if state == tcp::State::Closed && !st.established {
                    return ev::ERR | ev::HUP | ev::READABLE | ev::WRITABLE;
                }
                let mut r = 0;
                if s.can_recv() || !s.may_recv() || st.shut_rd {
                    r |= ev::READABLE;
                }
                if !s.may_recv() {
                    r |= ev::RDHUP;
                }
                if s.can_send() || st.shut_wr {
                    r |= ev::WRITABLE;
                }
                if matches!(state, tcp::State::Closed | tcp::State::TimeWait) || (!s.may_recv() && !s.may_send()) {
                    r |= ev::HUP;
                }
                if st.error != 0 {
                    r |= ev::ERR;
                }
                return r;
            }
        }
        let (mut r, peer) = {
            let st = self.st.lock();
            let mut r = 0;
            if st.error != 0 {
                r |= ev::ERR;
            }
            let peer = match &st.tcp {
                Tcp::Loop(w) => Some(w.clone()),
                // Jamais connectée (ou fermée) : `tcp_poll` donne HUP et OUT.
                _ => return r | ev::HUP | ev::WRITABLE | if st.peer_gone { ev::READABLE | ev::RDHUP } else { 0 },
            };
            if !st.rx.is_empty() || st.eof || st.shut_rd {
                r |= ev::READABLE;
            }
            if st.eof {
                r |= ev::RDHUP;
            }
            if st.shut_wr && (st.eof || st.peer_gone) {
                r |= ev::HUP;
            }
            if st.shut_wr || st.peer_gone {
                r |= ev::WRITABLE;
            }
            (r, peer)
        };
        if r & ev::WRITABLE == 0 {
            match peer.and_then(|w| w.upgrade()) {
                Some(p) => {
                    let cap = p.opts.lock().rcvbuf;
                    let ps = p.st.lock();
                    if ps.rx_bytes < cap || ps.rx.is_empty() {
                        r |= ev::WRITABLE;
                    }
                }
                None => r |= ev::WRITABLE,
            }
        }
        r
    }

    fn poll_udp(&self) -> u32 {
        let mut r = ev::WRITABLE;
        if self.is_lite() {
            self.pump_lite();
        }
        {
            let st = self.st.lock();
            if !st.rx.is_empty() || st.shut_rd {
                r |= ev::READABLE;
            }
            if st.error != 0 {
                r |= ev::ERR;
            }
        }
        if r & ev::READABLE == 0 {
            if let (true, Some(nb)) = (self.is_lite(), self.net.get()) {
                nb.iface.sockets.lock().get_mut::<raw::Socket>(nb.h).register_recv_waker(&self.waker);
            } else if let Some(nb) = self.net.get() {
                let mut set = nb.iface.sockets.lock();
                let s = set.get_mut::<udp::Socket>(nb.h);
                if s.can_recv() {
                    r |= ev::READABLE;
                } else {
                    s.register_recv_waker(&self.waker);
                }
            }
        }
        r
    }

    // --- Options ---

    fn tcp_info(&self) -> Vec<u8> {
        let mut b = vec![0u8; 104];
        b[0] = self.tcp_state();
        let local = self.st.lock().remote.map_or(true, |r| r.is_loopback());
        let mss: u32 = if local { 65483 } else { 1460 };
        b[8..12].copy_from_slice(&200_000u32.to_le_bytes()); // tcpi_rto (µs)
        b[16..20].copy_from_slice(&mss.to_le_bytes()); // tcpi_snd_mss
        b[20..24].copy_from_slice(&mss.to_le_bytes()); // tcpi_rcv_mss
        b[60..64].copy_from_slice(&(if local { 65536u32 } else { 1500 }).to_le_bytes()); // tcpi_pmtu
        b[80..84].copy_from_slice(&10u32.to_le_bytes()); // tcpi_snd_cwnd
        b[84..88].copy_from_slice(&mss.to_le_bytes()); // tcpi_advmss
        b
    }

    fn known_opt(&self, level: i32, name: i32) -> bool {
        match level {
            IPPROTO_IP => matches!(name, 1..=15 | 18..=24 | 32..=49),
            IPPROTO_IPV6 => self.family == AF_INET6 && matches!(name, 1..=26 | 49..=80),
            IPPROTO_TCP => self.is_tcp() && matches!(name, 1..=37),
            // Les options d'UDP valent aux deux niveaux (`udp_setsockopt`) ; les couvertures
            // n'existent que pour UDP-Lite (`ENOPROTOOPT` sinon, comme Linux).
            IPPROTO_UDP | SOL_UDPLITE => {
                !self.is_tcp() && (matches!(name, 1 | 100..=105) || (self.is_lite() && matches!(name, UDPLITE_SEND_CSCOV | UDPLITE_RECV_CSCOV)))
            }
            _ => false,
        }
    }

    fn default_opt(&self, level: i32, name: i32) -> Vec<u8> {
        let int = |v: i32| v.to_le_bytes().to_vec();
        let local = self.st.lock().remote.map_or(true, |r| r.is_loopback());
        match (level, name) {
            (IPPROTO_IP, IP_TTL) | (IPPROTO_IPV6, IPV6_UNICAST_HOPS) => int(64),
            (IPPROTO_IP, IP_MTU) | (IPPROTO_IPV6, IPV6_MTU) => int(if local { 65536 } else { 1500 }),
            (IPPROTO_TCP, TCP_MAXSEG) => int(if local { 65483 } else { 1460 }),
            (IPPROTO_TCP, TCP_CONGESTION) => {
                let mut v = b"cubic".to_vec();
                v.resize(16, 0);
                v
            }
            _ => int(0),
        }
    }
}

impl Drop for InetSocket {
    fn drop(&mut self) {
        let (tcp, reset) = {
            let mut st = self.st.lock();
            let reset = st.rx_bytes > 0 || st.unaccepted;
            (core::mem::replace(&mut st.tcp, Tcp::Closed), reset)
        };
        match tcp {
            Tcp::Loop(w) => {
                if let Some(p) = w.upgrade() {
                    {
                        let mut ps = p.st.lock();
                        ps.eof = true;
                        ps.peer_gone = true;
                        if reset {
                            ps.error = ECONNRESET;
                        }
                    }
                    p.wq.wake_all();
                }
            }
            Tcp::Listen { backlog, nl, .. } => {
                if let Some(nb) = self.net.get() {
                    let mut set = nb.iface.sockets.lock();
                    for h in &nl {
                        set.get_mut::<tcp::Socket>(*h).abort();
                        set.remove(*h);
                    }
                    netpoll::ACTIVE.fetch_sub(nl.len(), Ordering::AcqRel);
                }
                // Les connexions jamais acceptées sont réinitialisées (`inet_csk_listen_stop`).
                drop(backlog);
            }
            Tcp::Net => {
                if let Some(nb) = self.net.get() {
                    {
                        let mut set = nb.iface.sockets.lock();
                        let s = set.get_mut::<tcp::Socket>(nb.h);
                        if s.recv_queue() > 0 {
                            s.abort();
                        } else {
                            s.close();
                        }
                    }
                    netpoll::bury(nb.iface.clone(), nb.h);
                    nb.iface.poll();
                }
            }
            Tcp::Closed => {
                if let Some(nb) = self.net.get() {
                    if self.is_tcp() {
                        netpoll::bury(nb.iface.clone(), nb.h);
                    } else {
                        nb.iface.sockets.lock().remove(nb.h);
                        netpoll::ACTIVE.fetch_sub(1, Ordering::AcqRel);
                    }
                }
            }
        }
        let me = self as *const InetSocket;
        let port = self.st.lock().local.map(|l| l.port);
        if let Some(port) = port {
            let mut tab = self.table().lock();
            if let Some(list) = tab.get_mut(&port) {
                list.retain(|b| !b.is(me) && b.alive());
                if list.is_empty() {
                    tab.remove(&port);
                }
            }
        }
    }
}

impl SocketOps for InetSocket {
    fn domain(&self) -> i32 {
        self.family
    }

    fn sock_net(&self) -> Option<Arc<NetNs>> {
        Some(self.ns.clone())
    }

    fn sock_type(&self) -> i32 {
        self.ty
    }

    fn protocol(&self) -> i32 {
        self.proto
    }

    fn opts(&self) -> &Mutex<SockOpts> {
        &self.opts
    }

    fn bind(&self, t: &Thread, addr: &[u8]) -> Result<(), i64> {
        let a = self.decode(addr)?;
        if !a.is_any() && !is_local(&self.ns, &a) {
            return Err(EADDRNOTAVAIL);
        }
        // Ports réservés (`ip_unprivileged_port_start` = 1024) : `CAP_NET_BIND_SERVICE` dans
        // l'espace utilisateur propriétaire de l'espace réseau (`inet_port_requires_bind_service`).
        if a.port != 0 && a.port < 1024 {
            let c = t.process.creds.lock();
            if !crate::userns::ns_capable(&c, &self.ns.user_ns, crate::userns::CAP_NET_BIND_SERVICE) {
                return Err(EACCES);
            }
        }
        if self.st.lock().local.is_some() {
            return Err(EINVAL);
        }
        let bound = self.bind_port(a, None)?;
        self.st.lock().local = Some(bound);
        if !self.is_tcp() {
            self.udp_net_open(bound);
        }
        Ok(())
    }

    fn connect(&self, t: &Thread, addr: &[u8], nonblock: bool) -> Result<(), i64> {
        if addr.len() >= 2 && u16::from_le_bytes([addr[0], addr[1]]) == 0 {
            // `AF_UNSPEC` : rompt l'association (UDP).
            if !self.is_tcp() {
                let port = {
                    let mut st = self.st.lock();
                    st.remote = None;
                    st.local.map(|l| l.port)
                };
                if let Some(p) = port {
                    self.update_binding(p, |b| b.connected = None);
                }
            }
            return Ok(());
        }
        let dst = self.decode(addr)?;
        if self.is_tcp() {
            self.connect_tcp(t, dst, nonblock)
        } else {
            self.connect_udp(dst)
        }
    }

    fn listen(&self, _t: &Thread, backlog: i32) -> Result<(), i64> {
        if !self.is_tcp() {
            return Err(EOPNOTSUPP);
        }
        let max = (backlog.max(0) as usize).clamp(1, SOMAXCONN);
        {
            let mut st = self.st.lock();
            let established = st.established;
            match &mut st.tcp {
                Tcp::Listen { max: m, .. } => {
                    *m = max;
                    return Ok(());
                }
                Tcp::Closed if !established => {}
                _ => return Err(EINVAL),
            }
        }
        let cur = self.st.lock().local;
        let local = match cur {
            Some(l) => l,
            None => {
                let l = self.bind_port(Addr::any(self.family), None)?;
                self.st.lock().local = Some(l);
                l
            }
        };
        {
            let me = self as *const InetSocket;
            let mut tab = self.ns.ports.tcp.lock();
            let list = tab.get_mut(&local.port).ok_or(EINVAL)?;
            let mine = list.iter().find(|b| b.is(me)).cloned().ok_or(EINVAL)?;
            let clash = list.iter().any(|x| x.alive() && !x.is(me) && x.listening && x.overlaps(&mine) && !(x.reuseport && mine.reuseport));
            if clash {
                return Err(EADDRINUSE);
            }
            if let Some(b) = list.iter_mut().find(|b| b.is(me)) {
                b.listening = true;
            }
        }
        let nl = self.open_net_listeners(local);
        self.st.lock().tcp = Tcp::Listen { backlog: VecDeque::new(), max, nl };
        self.wq.wake_all();
        Ok(())
    }

    fn accept(&self, t: &Thread, nonblock: bool) -> Result<(Arc<dyn FileObject>, Vec<u8>), i64> {
        if !self.is_tcp() {
            return Err(EOPNOTSUPP);
        }
        let deadline = self.opts.lock().rcv_deadline();
        let r = wait_event(&[&self.wq], deadline, Some(t), || {
            self.harvest();
            let mut st = self.st.lock();
            let Tcp::Listen { backlog, .. } = &mut st.tcp else {
                return Some(Err(EINVAL));
            };
            match backlog.pop_front() {
                Some(c) => Some(Ok(c)),
                None if nonblock => Some(Err(EAGAIN)),
                None => None,
            }
        });
        let child = r.map_err(wait_err)??;
        self.wq.wake_all();
        let peer = {
            let mut cs = child.st.lock();
            cs.unaccepted = false;
            cs.remote.unwrap_or_default()
        };
        Ok((child, peer.encode(self.family)))
    }

    fn send(&self, t: &Thread, m: SendMsg, nonblock: bool) -> Result<usize, i64> {
        if !self.is_tcp() {
            return self.send_udp(t, m, nonblock);
        }
        let net = matches!(self.st.lock().tcp, Tcp::Net);
        if net { self.send_net(t, m, nonblock) } else { self.send_loop(t, m, nonblock) }
    }

    fn recv(&self, t: &Thread, buf: &mut [u8], flags: u32, nonblock: bool) -> Result<RecvMeta, i64> {
        if flags & MSG_OOB != 0 {
            return Err(EINVAL);
        }
        if !self.is_tcp() {
            return self.recv_udp(t, buf, flags, nonblock);
        }
        let net = matches!(self.st.lock().tcp, Tcp::Net);
        if net { self.recv_net(t, buf, flags, nonblock) } else { self.recv_loop(t, buf, flags, nonblock) }
    }

    fn shutdown(&self, how: i32) -> Result<(), i64> {
        let rd = how == SHUT_RD || how == SHUT_RDWR;
        let wr = how == SHUT_WR || how == SHUT_RDWR;
        let (peer, net) = {
            let mut st = self.st.lock();
            let connected = match &st.tcp {
                Tcp::Loop(_) | Tcp::Net => true,
                Tcp::Listen { .. } => rd,
                Tcp::Closed => !self.is_tcp() && st.remote.is_some(),
            };
            if !connected {
                return Err(ENOTCONN);
            }
            st.shut_rd |= rd;
            st.shut_wr |= wr;
            let peer = match &st.tcp {
                Tcp::Loop(w) => w.upgrade(),
                _ => None,
            };
            (peer, matches!(st.tcp, Tcp::Net))
        };
        if let Some(p) = peer {
            if wr {
                p.st.lock().eof = true;
            }
            p.wq.wake_all();
        }
        if net && wr {
            if let Some(nb) = self.net.get() {
                nb.iface.sockets.lock().get_mut::<tcp::Socket>(nb.h).close();
                nb.iface.poll();
            }
        }
        self.wq.wake_all();
        Ok(())
    }

    fn sockname(&self) -> Vec<u8> {
        self.st.lock().local.unwrap_or_else(|| Addr::any(self.family)).encode(self.family)
    }

    fn peername(&self) -> Result<Vec<u8>, i64> {
        let st = self.st.lock();
        let connected = match &st.tcp {
            Tcp::Loop(_) => true,
            Tcp::Net => !st.connecting,
            _ => !self.is_tcp(),
        };
        match st.remote {
            Some(r) if connected => Ok(r.encode(self.family)),
            _ => Err(ENOTCONN),
        }
    }

    fn is_listening(&self) -> bool {
        matches!(self.st.lock().tcp, Tcp::Listen { .. })
    }

    fn take_error(&self) -> i64 {
        let (e, connecting) = {
            let mut st = self.st.lock();
            (core::mem::take(&mut st.error), st.connecting)
        };
        if e != 0 || !connecting {
            return e;
        }
        // Connexion non bloquante : son issue se lit ici (`SO_ERROR`).
        let Some(nb) = self.net.get() else { return 0 };
        let state = nb.iface.sockets.lock().get::<tcp::Socket>(nb.h).state();
        let mut st = self.st.lock();
        match state {
            tcp::State::SynSent | tcp::State::SynReceived => 0,
            tcp::State::Closed => {
                st.connecting = false;
                st.tcp = Tcp::Closed;
                ECONNREFUSED
            }
            _ => {
                st.connecting = false;
                st.established = true;
                0
            }
        }
    }

    fn setsockopt(&self, level: i32, name: i32, val: &[u8]) -> Result<(), i64> {
        if !self.known_opt(level, name) {
            return Err(ENOPROTOOPT);
        }
        let int = || -> Result<i32, i64> { val.get(..4).map(|b| i32::from_le_bytes(b.try_into().unwrap())).ok_or(EINVAL) };
        match (level, name) {
            (IPPROTO_IPV6, IPV6_V6ONLY) => {
                if self.st.lock().local.is_some() {
                    return Err(EINVAL);
                }
                self.x.lock().v6only = int()? != 0;
                return Ok(());
            }
            (IPPROTO_TCP, TCP_NODELAY) => {
                let on = int()? != 0;
                self.x.lock().nodelay = on;
                if matches!(self.st.lock().tcp, Tcp::Net) {
                    if let Some(nb) = self.net.get() {
                        nb.iface.sockets.lock().get_mut::<tcp::Socket>(nb.h).set_nagle_enabled(!on);
                    }
                }
                return Ok(());
            }
            (IPPROTO_TCP, TCP_INFO) => return Err(ENOPROTOOPT),
            (IPPROTO_UDP | SOL_UDPLITE, UDPLITE_SEND_CSCOV | UDPLITE_RECV_CSCOV) => {
                // 1 à 7 : relevé à 8 (l'en-tête est toujours couvert) ; plafonné à 65535.
                let v = int()?;
                let v = if v != 0 && v < 8 { 8 } else { v.min(0xffff) } as u16;
                let mut x = self.x.lock();
                if name == UDPLITE_SEND_CSCOV {
                    x.pcslen = Some(v);
                } else {
                    x.pcrlen = Some(v);
                }
                return Ok(());
            }
            (IPPROTO_TCP, TCP_CONGESTION) => {
                let end = val.iter().position(|&b| b == 0).unwrap_or(val.len());
                if !matches!(&val[..end], b"cubic" | b"reno" | b"bbr") {
                    return Err(ENOENT);
                }
            }
            (IPPROTO_IP, IP_TTL) | (IPPROTO_IPV6, IPV6_UNICAST_HOPS) => {
                let v = int()?;
                if !(v == -1 || (1..=255).contains(&v)) {
                    return Err(EINVAL);
                }
            }
            _ => {}
        }
        self.x.lock().other.insert((level, name), val.to_vec());
        Ok(())
    }

    fn getsockopt(&self, level: i32, name: i32) -> Result<Vec<u8>, i64> {
        if !self.known_opt(level, name) {
            return Err(ENOPROTOOPT);
        }
        let int = |v: bool| (v as i32).to_le_bytes().to_vec();
        match (level, name) {
            (IPPROTO_IPV6, IPV6_V6ONLY) => return Ok(int(self.x.lock().v6only)),
            (IPPROTO_TCP, TCP_NODELAY) => return Ok(int(self.x.lock().nodelay)),
            (IPPROTO_TCP, TCP_INFO) => return Ok(self.tcp_info()),
            (IPPROTO_UDP | SOL_UDPLITE, UDPLITE_SEND_CSCOV) => return Ok((self.x.lock().pcslen.unwrap_or(0) as i32).to_le_bytes().to_vec()),
            (IPPROTO_UDP | SOL_UDPLITE, UDPLITE_RECV_CSCOV) => return Ok((self.x.lock().pcrlen.unwrap_or(0) as i32).to_le_bytes().to_vec()),
            _ => {}
        }
        if let Some(v) = self.x.lock().other.get(&(level, name)) {
            return Ok(v.clone());
        }
        Ok(self.default_opt(level, name))
    }
}

/// `struct ifreq` : nom (16 octets) puis union (24 octets : `struct ifmap` à `unsigned long`) ;
/// 32 octets en i386 (`struct compat_ifreq`).
const IFREQ: usize = 40;
const IFREQ32: usize = 32;

/// Une interface vue par les programmes (`lo`, `eth0`…).
pub struct IfInfo {
    pub name: String,
    pub index: u32,
    pub flags: u32,
    pub mtu: u32,
    pub mac: [u8; 6],
    pub v4: Option<([u8; 4], u8)>,
    /// Adresses IPv6 et longueurs de préfixe.
    pub v6: Vec<([u8; 16], u8)>,
    /// Passerelle IPv4 par défaut passant par cette interface.
    pub gw4: Option<[u8; 4]>,
    /// Adresse obtenue par DHCP (sinon statique).
    pub dhcp: bool,
    /// Octets reçus, trames reçues, octets émis, trames émises, trames jetées à l'émission.
    pub stats: [u64; 5],
}

/// Interfaces de l'espace `ns` : bouclage puis cartes (espace initial seulement).
pub fn interfaces(ns: &NetNs) -> Vec<IfInfo> {
    const UP: u32 = 0x1;
    const BROADCAST: u32 = 0x2;
    const RUNNING: u32 = 0x40;
    const MULTICAST: u32 = 0x1000;
    let lo = ns.lo();
    let mut out = vec![IfInfo {
        name: String::from("lo"),
        index: 1,
        flags: ns.lo_flags(),
        mtu: 65536,
        mac: [0; 6],
        v4: lo.v4.first().copied(),
        v6: lo.v6.clone(),
        gw4: None,
        dhcp: false,
        stats: [0; 5],
    }];
    if !ns.init {
        return out;
    }
    let ifaces: Vec<Arc<NetworkInterface>> = net::get_interfaces().lock().clone();
    for (i, iface) in ifaces.iter().enumerate() {
        let addrs = iface.ip_addrs();
        let v4 = addrs.iter().find_map(|c| match c.address() {
            IpAddress::Ipv4(a) => Some((a.0, c.prefix_len())),
            _ => None,
        });
        let v6 = addrs
            .iter()
            .filter_map(|c| match c.address() {
                IpAddress::Ipv6(a) => Some((a.0, c.prefix_len())),
                _ => None,
            })
            .collect();
        out.push(IfInfo {
            name: format!("eth{}", i),
            index: 2 + i as u32,
            flags: UP | BROADCAST | RUNNING | MULTICAST | crate::netns::IFF_LOWER_UP,
            mtu: iface.capabilities().max_transmission_unit as u32,
            mac: iface.mac_address(),
            v4,
            v6,
            gw4: iface.default_gateway_v4(),
            dhcp: iface.addr_state() == net::ADDR_DHCP,
            stats: iface.stats(),
        });
    }
    out
}

fn sockaddr_in(ip: [u8; 4]) -> [u8; 16] {
    let mut s = [0u8; 16];
    s[0..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
    s[4..8].copy_from_slice(&ip);
    s
}

fn prefix_mask(p: u8) -> [u8; 4] {
    let m: u32 = if p == 0 { 0 } else { u32::MAX << (32 - p as u32) };
    m.to_be_bytes()
}

/// `ioctl` des interfaces (`SIOCGIF*`, `SIOCSIFFLAGS`) de l'espace `ns` de la socket, comme
/// `dev_ioctl` de Linux.
pub(crate) fn if_ioctl(t: &Thread, ns: &NetNs, req: u64, arg: u64) -> Result<i64, i64> {
    const SIOCGIFNAME: u64 = 0x8910;
    const SIOCGIFCONF: u64 = 0x8912;
    const SIOCGIFFLAGS: u64 = 0x8913;
    const SIOCSIFFLAGS: u64 = 0x8914;
    const SIOCGIFADDR: u64 = 0x8915;
    const SIOCGIFBRDADDR: u64 = 0x8919;
    const SIOCGIFNETMASK: u64 = 0x891b;
    const SIOCGIFMTU: u64 = 0x8921;
    const SIOCGIFHWADDR: u64 = 0x8927;
    const SIOCGIFINDEX: u64 = 0x8933;
    let ifs = interfaces(ns);
    let compat = crate::linux::compat::active(t);
    let size = if compat { IFREQ32 } else { IFREQ };
    if req == SIOCGIFCONF {
        // `struct ifconf` { int len; pointeur } : pointeur de 4 octets en i386.
        let (room, buf) = if compat {
            let c = t.mm().read_user::<[u32; 2]>(arg as usize).map_err(fault)?;
            (c[0] as i32, c[1] as u64)
        } else {
            let mut conf = [0u8; 16];
            t.mm().copy_from_user(&mut conf, arg as usize).map_err(fault)?;
            (i32::from_le_bytes(conf[0..4].try_into().unwrap()), u64::from_le_bytes(conf[8..16].try_into().unwrap()))
        };
        let room = room.max(0) as usize;
        let mut out = Vec::new();
        for i in ifs.iter().filter(|i| i.v4.is_some()) {
            let mut r = [0u8; IFREQ];
            r[..i.name.len()].copy_from_slice(i.name.as_bytes());
            r[16..32].copy_from_slice(&sockaddr_in(i.v4.unwrap().0));
            out.extend_from_slice(&r[..size]);
        }
        let n = if buf == 0 { out.len() } else { (room / size * size).min(out.len()) };
        if buf != 0 {
            t.mm().copy_to_user(buf as usize, &out[..n]).map_err(fault)?;
        }
        t.mm().write_user::<i32>(arg as usize, &(n as i32)).map_err(fault)?;
        return Ok(0);
    }
    let mut r = [0u8; IFREQ];
    t.mm().copy_from_user(&mut r[..size], arg as usize).map_err(fault)?;
    let iface = if req == SIOCGIFNAME {
        let idx = u32::from_le_bytes(r[16..20].try_into().unwrap());
        ifs.iter().find(|i| i.index == idx).ok_or(ENODEV)?
    } else {
        let end = r[..16].iter().position(|&b| b == 0).unwrap_or(16);
        ifs.iter().find(|i| i.name.as_bytes() == &r[..end]).ok_or(ENODEV)?
    };
    if req == SIOCSIFFLAGS {
        // Seul `IFF_UP` de `lo` change (`ifconfig lo up`) ; `CAP_NET_ADMIN` dans l'espace.
        if !crate::userns::ns_capable(&t.process.creds.lock(), &ns.user_ns, crate::userns::CAP_NET_ADMIN) {
            return Err(EPERM);
        }
        let fl = u16::from_le_bytes([r[16], r[17]]) as u32;
        if iface.index != 1 {
            return if fl & crate::netns::IFF_UP != 0 { Ok(0) } else { Err(EOPNOTSUPP) };
        }
        ns.set_lo_up(fl & crate::netns::IFF_UP != 0)?;
        return Ok(0);
    }
    if req == SIOCGIFNAME {
        let mut n = [0u8; 16];
        n[..iface.name.len()].copy_from_slice(iface.name.as_bytes());
        r[..16].copy_from_slice(&n);
        t.mm().copy_to_user(arg as usize, &r[..size]).map_err(fault)?;
        return Ok(0);
    }
    let u = &mut r[16..];
    match req {
        SIOCGIFFLAGS => u[..2].copy_from_slice(&(iface.flags as u16).to_le_bytes()),
        SIOCGIFADDR => u[..16].copy_from_slice(&sockaddr_in(iface.v4.ok_or(EADDRNOTAVAIL)?.0)),
        SIOCGIFNETMASK => u[..16].copy_from_slice(&sockaddr_in(prefix_mask(iface.v4.ok_or(EADDRNOTAVAIL)?.1))),
        SIOCGIFBRDADDR => {
            let (ip, p) = iface.v4.ok_or(EADDRNOTAVAIL)?;
            let m = u32::from_be_bytes(prefix_mask(p));
            u[..16].copy_from_slice(&sockaddr_in((u32::from_be_bytes(ip) | !m).to_be_bytes()));
        }
        SIOCGIFMTU => u[..4].copy_from_slice(&iface.mtu.to_le_bytes()),
        SIOCGIFHWADDR => {
            let ty: u16 = if iface.index == 1 { 772 } else { 1 }; // ARPHRD_LOOPBACK, ARPHRD_ETHER
            u[..2].copy_from_slice(&ty.to_le_bytes());
            u[2..8].copy_from_slice(&iface.mac);
        }
        SIOCGIFINDEX => u[..4].copy_from_slice(&iface.index.to_le_bytes()),
        _ => return Err(ENOTTY),
    }
    t.mm().copy_to_user(arg as usize, &r[..size]).map_err(fault)?;
    Ok(0)
}

impl FileObject for InetSocket {
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
        if self.is_tcp() { self.poll_tcp() } else { self.poll_udp() }
    }

    fn queues<'a>(&'a self, cb: &mut dyn FnMut(&'a WaitQueue)) {
        cb(&self.wq);
    }

    fn ioctl(&self, t: &Thread, req: u64, arg: u64) -> crate::linux::SysResult {
        const SIOCOUTQ: u64 = 0x5411;
        if req == SIOCOUTQ {
            let n = match (self.net.get(), matches!(self.st.lock().tcp, Tcp::Net)) {
                (Some(nb), true) => nb.iface.sockets.lock().get::<tcp::Socket>(nb.h).send_queue(),
                _ => 0,
            };
            t.mm().write_user::<i32>(arg as usize, &(n as i32)).map_err(fault)?;
            return Ok(0);
        }
        if_ioctl(t, &self.ns, req, arg)
    }

    fn as_socket(&self) -> Option<&dyn SocketOps> {
        Some(self)
    }

    fn pending_bytes(&self) -> Option<usize> {
        // UDP (`FIONREAD`/`SIOCINQ` de Linux) : taille du **prochain datagramme**, qu'il soit dans
        // la file locale ou encore dans smoltcp (réseau réel). Le résolveur de la glibc s'en sert
        // pour allouer le tampon de la seconde réponse (A + AAAA envoyées ensemble par
        // `sendmmsg`) : 0 lui faisait lire dans un tampon vide et abandonner la résolution.
        if !self.is_tcp() {
            if self.is_lite() {
                self.pump_lite();
            }
            if let Some(c) = self.st.lock().rx.front() {
                return Some(c.data.len());
            }
            if self.is_lite() {
                return Some(0);
            }
            let Some(nb) = self.net.get() else { return Some(0) };
            let mut set = nb.iface.sockets.lock();
            return Some(set.get_mut::<udp::Socket>(nb.h).peek().map_or(0, |(d, _)| d.len()));
        }
        let (local, net) = {
            let st = self.st.lock();
            let local = if self.is_tcp() { st.rx_bytes } else { st.rx.front().map_or(0, |c| c.data.len()) };
            (local, matches!(st.tcp, Tcp::Net))
        };
        if local > 0 || !net {
            return Some(local);
        }
        let nb = self.net.get()?;
        Some(nb.iface.sockets.lock().get::<tcp::Socket>(nb.h).recv_queue())
    }

    fn fdinfo(&self) -> String {
        let st = self.st.lock();
        format!("inet-type: {}\ninet-rx-bytes: {}\n", self.ty, st.rx_bytes)
    }
}
