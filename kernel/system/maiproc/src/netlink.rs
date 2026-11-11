//! Sockets netlink (`AF_NETLINK`) : `NETLINK_ROUTE` (rtnetlink) et `NETLINK_KOBJECT_UEVENT`.
//!
//! rtnetlink est l'interface par laquelle glibc (`getifaddrs`, `if_nameindex`,
//! `getaddrinfo(AI_ADDRCONFIG)`), iproute2 (`ip link`, `ip addr`, `ip route`) et la plupart des
//! démons apprennent les interfaces, leurs adresses et les routes. Les requêtes sont servies
//! comme sous Linux (`rtnetlink_rcv_msg`) :
//!
//! - `RTM_GETLINK`, `RTM_GETADDR`, `RTM_GETROUTE` en vidage (`NLM_F_DUMP`) : une suite de
//!   messages `NLM_F_MULTI`, close par `NLMSG_DONE` ; `RTM_GETLINK` et `RTM_GETROUTE` aussi pour
//!   un seul objet (indice ou nom d'interface ; route vers une destination, `ip route get`) ;
//! - vidages des tables que MaiOS n'a pas (voisins, règles, files d'attente) : vides ;
//! - modifications (`CAP_NET_ADMIN` dans l'espace réseau, sinon `EPERM`) : seulement `lo`
//!   ([`crate::netns`]) — `RTM_NEWLINK`/`RTM_SETLINK` (`IFF_UP`), `RTM_NEWADDR`/`RTM_DELADDR`
//!   dans un espace neuf, ce que fait bubblewrap ; le reste : `EOPNOTSUPP` (configuration
//!   statique des cartes) ;
//! - `NLM_F_ACK` : accusé `NLMSG_ERROR` d'erreur 0 ; une erreur renvoie la requête entière, sauf
//!   `NETLINK_CAP_ACK` (`NLM_F_CAPPED`).
//!
//! Les vidages sont produits paresseusement, comme `netlink_dump` : chaque lecture forme le
//! datagramme suivant, rempli de messages entiers jusqu'à la taille du tampon de l'appelant
//! (bornée entre [`GOODSIZE`] et 32 Kio) ; un datagramme formé (par `MSG_PEEK` aussi) ne change
//! plus. Un tampon trop petit tronque le datagramme (`MSG_TRUNC`).
//!
//! Identifiants de port : le premier socket d'un processus prend son pid, les suivants un
//! numéro négatif unique (`netlink_autobind`) ; les réponses portent ce numéro (`nlmsg_pid`),
//! que glibc vérifie. Ils sont propres à l'espace réseau de la socket, et tout ce qu'elle
//! montre (interfaces, adresses, routes) est celui de cet espace.

use alloc::{collections::{BTreeSet, VecDeque}, format, string::String, sync::Arc, vec, vec::Vec};
use sync_preemption::PreemptionSafeMutex as Mutex;

use crate::inet::{interfaces, IfInfo};
use crate::linux::errno::*;
use crate::netns::{NetNs, IFF_LOWER_UP, IFF_UP};
use crate::object::{ev, new_ino, object_any, FileObject, S_IFSOCK};
use crate::socket::*;
use crate::wait::{wait_event, WaitError, WaitQueue};
use crate::Thread;

pub const NETLINK_ROUTE: i32 = 0;
pub const NETLINK_KOBJECT_UEVENT: i32 = 15;
const SOL_NETLINK: i32 = 270;
const NETLINK_ADD_MEMBERSHIP: i32 = 1;
const NETLINK_DROP_MEMBERSHIP: i32 = 2;
const NETLINK_CAP_ACK: i32 = 10;

const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_MULTI: u16 = 0x2;
const NLM_F_ACK: u16 = 0x4;
const NLM_F_DUMP: u16 = 0x300;
const NLM_F_CAPPED: u16 = 0x100;

const NLM_F_REPLACE: u16 = 0x100;
const NLM_F_EXCL: u16 = 0x200;

const RTM_NEWLINK: u16 = 16;
const RTM_DELLINK: u16 = 17;
const RTM_GETLINK: u16 = 18;
const RTM_SETLINK: u16 = 19;
const RTM_NEWADDR: u16 = 20;
const RTM_DELADDR: u16 = 21;
const RTM_GETADDR: u16 = 22;
const RTM_NEWROUTE: u16 = 24;
const RTM_GETROUTE: u16 = 26;
const RTM_BASE: u16 = 16;
const RTM_MAX: u16 = 127;

const IFLA_ADDRESS: u16 = 1;
const IFLA_BROADCAST: u16 = 2;
const IFLA_IFNAME: u16 = 3;
const IFLA_MTU: u16 = 4;
const IFLA_QDISC: u16 = 6;
const IFLA_TXQLEN: u16 = 13;
const IFLA_OPERSTATE: u16 = 16;
const IFLA_LINKMODE: u16 = 17;
const IFLA_GROUP: u16 = 27;
const IFLA_PROMISCUITY: u16 = 30;
const IFLA_NUM_TX_QUEUES: u16 = 31;
const IFLA_NUM_RX_QUEUES: u16 = 32;
const IFLA_CARRIER: u16 = 33;
const IFLA_MIN_MTU: u16 = 50;
const IFLA_MAX_MTU: u16 = 51;

const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFA_LABEL: u16 = 3;
const IFA_BROADCAST: u16 = 4;
const IFA_CACHEINFO: u16 = 6;
const IFA_FLAGS: u16 = 8;
const IFA_F_PERMANENT: u32 = 0x80;

const RTA_DST: u16 = 1;
const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;
const RTA_PRIORITY: u16 = 6;
const RTA_PREFSRC: u16 = 7;
const RTA_TABLE: u16 = 15;
const RT_TABLE_MAIN: u8 = 254;
const RT_TABLE_LOCAL: u8 = 255;
const RTPROT_KERNEL: u8 = 2;
const RTPROT_BOOT: u8 = 3;
const RTPROT_DHCP: u8 = 16;
const RT_SCOPE_UNIVERSE: u8 = 0;
const RT_SCOPE_LINK: u8 = 253;
const RT_SCOPE_HOST: u8 = 254;
const RTN_UNICAST: u8 = 1;
const RTN_LOCAL: u8 = 2;
const RTN_BROADCAST: u8 = 3;
const RTM_F_CLONED: u32 = 0x200;

const ARPHRD_ETHER: u16 = 1;
const ARPHRD_LOOPBACK: u16 = 772;
const IF_OPER_UNKNOWN: u8 = 0;
const IF_OPER_DOWN: u8 = 2;
const IF_OPER_UP: u8 = 6;

/// Taille d'un datagramme de vidage par défaut (`NLMSG_GOODSIZE` : une page moins la place des
/// métadonnées d'un `sk_buff`) : tient dans le tampon de 4 Kio de glibc.
pub const GOODSIZE: usize = 3776;
const MAX_DUMP_SKB: usize = 32768;

/// Identifiants de port pris : (espace réseau, protocole, port).
static PORTS: Mutex<BTreeSet<(u64, i32, u32)>> = Mutex::new(BTreeSet::new());
static ROVER: core::sync::atomic::AtomicI32 = core::sync::atomic::AtomicI32::new(-4096);

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

// --- Construction des messages ----------------------------------------------------------------

/// Un message en construction : en-tête `nlmsghdr`, en-tête de famille, attributs.
struct Msg(Vec<u8>);

impl Msg {
    fn new(ty: u16, flags: u16, seq: u32, pid: u32) -> Msg {
        let mut b = Vec::with_capacity(256);
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&ty.to_le_bytes());
        b.extend_from_slice(&flags.to_le_bytes());
        b.extend_from_slice(&seq.to_le_bytes());
        b.extend_from_slice(&pid.to_le_bytes());
        Msg(b)
    }

    fn raw(&mut self, d: &[u8]) -> &mut Self {
        self.0.extend_from_slice(d);
        self
    }

    fn attr(&mut self, ty: u16, d: &[u8]) -> &mut Self {
        let len = 4 + d.len();
        self.0.extend_from_slice(&(len as u16).to_le_bytes());
        self.0.extend_from_slice(&ty.to_le_bytes());
        self.0.extend_from_slice(d);
        self.0.resize(align4(self.0.len()), 0);
        self
    }

    fn attr_u32(&mut self, ty: u16, v: u32) -> &mut Self {
        self.attr(ty, &v.to_le_bytes())
    }

    fn attr_u8(&mut self, ty: u16, v: u8) -> &mut Self {
        self.attr(ty, &[v])
    }

    fn attr_str(&mut self, ty: u16, s: &str) -> &mut Self {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        self.attr(ty, &v)
    }

    fn done(mut self) -> Vec<u8> {
        let len = self.0.len() as u32;
        self.0[0..4].copy_from_slice(&len.to_le_bytes());
        self.0.resize(align4(self.0.len()), 0);
        self.0
    }
}

/// En-tête d'une requête.
#[derive(Clone, Copy)]
struct Hdr {
    len: usize,
    ty: u16,
    flags: u16,
    seq: u32,
}

/// Attributs (`rtattr`) d'une charge utile, à partir de `off`.
fn attrs(p: &[u8], mut off: usize) -> Vec<(u16, &[u8])> {
    let mut out = Vec::new();
    while off + 4 <= p.len() {
        let len = u16::from_le_bytes([p[off], p[off + 1]]) as usize;
        let ty = u16::from_le_bytes([p[off + 2], p[off + 3]]) & 0x3fff;
        if len < 4 || off + len > p.len() {
            break;
        }
        out.push((ty, &p[off + 4..off + len]));
        off += align4(len);
    }
    out
}

fn link_msg(i: &IfInfo, ty: u16, flags: u16, seq: u32, pid: u32) -> Vec<u8> {
    let lo = i.index == 1;
    let up = i.flags & IFF_UP != 0;
    let mut m = Msg::new(ty, flags, seq, pid);
    // ifinfomsg : famille, bourrage, type, indice, drapeaux, masque des changements.
    m.raw(&[0, 0]);
    m.raw(&(if lo { ARPHRD_LOOPBACK } else { ARPHRD_ETHER }).to_le_bytes());
    m.raw(&(i.index as i32).to_le_bytes());
    m.raw(&i.flags.to_le_bytes());
    m.raw(&0u32.to_le_bytes());
    m.attr_str(IFLA_IFNAME, &i.name);
    m.attr_u32(IFLA_TXQLEN, 1000);
    m.attr_u8(IFLA_OPERSTATE, if !up { IF_OPER_DOWN } else if lo { IF_OPER_UNKNOWN } else { IF_OPER_UP });
    m.attr_u8(IFLA_LINKMODE, 0);
    m.attr_u32(IFLA_MTU, i.mtu);
    m.attr_u32(IFLA_MIN_MTU, if lo { 0 } else { 68 });
    m.attr_u32(IFLA_MAX_MTU, if lo { 0 } else { i.mtu.max(1500) });
    m.attr_u32(IFLA_GROUP, 0);
    m.attr_u32(IFLA_PROMISCUITY, 0);
    m.attr_u32(IFLA_NUM_TX_QUEUES, 1);
    m.attr_u32(IFLA_NUM_RX_QUEUES, 1);
    m.attr_u8(IFLA_CARRIER, (i.flags & IFF_LOWER_UP != 0) as u8);
    m.attr_str(IFLA_QDISC, if !up { "noop" } else if lo { "noqueue" } else { "pfifo_fast" });
    m.attr(IFLA_ADDRESS, &i.mac);
    m.attr(IFLA_BROADCAST, if lo { &[0; 6] } else { &[0xff; 6] });
    m.done()
}

fn cacheinfo_permanent() -> [u8; 16] {
    let mut c = [0u8; 16];
    c[0..4].copy_from_slice(&u32::MAX.to_le_bytes()); // ifa_prefered
    c[4..8].copy_from_slice(&u32::MAX.to_le_bytes()); // ifa_valid
    c
}

fn addr_msgs(i: &IfInfo, family: u8, flags: u16, seq: u32, pid: u32, out: &mut Vec<Vec<u8>>) {
    let lo = i.index == 1;
    if family == 0 || family == AF_INET as u8 {
        if let Some((ip, p)) = i.v4 {
            let mut m = Msg::new(RTM_NEWADDR, flags, seq, pid);
            m.raw(&[AF_INET as u8, p, IFA_F_PERMANENT as u8, if lo { RT_SCOPE_HOST } else { RT_SCOPE_UNIVERSE }]);
            m.raw(&i.index.to_le_bytes());
            m.attr(IFA_ADDRESS, &ip);
            m.attr(IFA_LOCAL, &ip);
            if !lo {
                let mask: u32 = if p == 0 { 0 } else { u32::MAX << (32 - p as u32) };
                m.attr(IFA_BROADCAST, &(u32::from_be_bytes(ip) | !mask).to_be_bytes());
            }
            m.attr_str(IFA_LABEL, &i.name);
            m.attr_u32(IFA_FLAGS, IFA_F_PERMANENT);
            m.attr(IFA_CACHEINFO, &cacheinfo_permanent());
            out.push(m.done());
        }
    }
    if family == 0 || family == AF_INET6 as u8 {
        for &(ip, p) in &i.v6 {
            let link_local = ip[0] == 0xfe && ip[1] & 0xc0 == 0x80;
            let scope = if lo { RT_SCOPE_HOST } else if link_local { RT_SCOPE_LINK } else { RT_SCOPE_UNIVERSE };
            let mut m = Msg::new(RTM_NEWADDR, flags, seq, pid);
            m.raw(&[AF_INET6 as u8, p, IFA_F_PERMANENT as u8, scope]);
            m.raw(&i.index.to_le_bytes());
            m.attr(IFA_ADDRESS, &ip);
            m.attr(IFA_CACHEINFO, &cacheinfo_permanent());
            m.attr_u32(IFA_FLAGS, IFA_F_PERMANENT);
            out.push(m.done());
        }
    }
}

/// Une route (`rtmsg` + attributs).
struct Route {
    family: u8,
    dst: Vec<u8>,
    dst_len: u8,
    table: u8,
    proto: u8,
    scope: u8,
    ty: u8,
    oif: u32,
    gateway: Option<Vec<u8>>,
    prefsrc: Option<Vec<u8>>,
    priority: Option<u32>,
    flags: u32,
}

impl Route {
    fn msg(&self, flags: u16, seq: u32, pid: u32) -> Vec<u8> {
        let mut m = Msg::new(RTM_NEWROUTE, flags, seq, pid);
        m.raw(&[self.family, self.dst_len, 0, 0, self.table, self.proto, self.scope, self.ty]);
        m.raw(&self.flags.to_le_bytes());
        m.attr_u32(RTA_TABLE, self.table as u32);
        if let Some(p) = self.priority {
            m.attr_u32(RTA_PRIORITY, p);
        }
        if self.dst_len > 0 || self.flags & RTM_F_CLONED != 0 {
            m.attr(RTA_DST, &self.dst);
        }
        if let Some(s) = &self.prefsrc {
            m.attr(RTA_PREFSRC, s);
        }
        if let Some(g) = &self.gateway {
            m.attr(RTA_GATEWAY, g);
        }
        m.attr_u32(RTA_OIF, self.oif);
        m.done()
    }
}

fn mask4(p: u8) -> u32 {
    if p == 0 { 0 } else { u32::MAX << (32 - p as u32) }
}

/// Tables `main` et `local`, comme les remplit le noyau Linux pour cette configuration.
fn routes(ifs: &[IfInfo]) -> Vec<Route> {
    let mut out = Vec::new();
    for i in ifs.iter().filter(|i| i.index > 1) {
        if let Some((ip, p)) = i.v4 {
            if let Some(gw) = i.gw4 {
                out.push(Route {
                    family: AF_INET as u8,
                    dst: vec![0; 4],
                    dst_len: 0,
                    table: RT_TABLE_MAIN,
                    proto: if i.dhcp { RTPROT_DHCP } else { RTPROT_BOOT },
                    scope: RT_SCOPE_UNIVERSE,
                    ty: RTN_UNICAST,
                    oif: i.index,
                    gateway: Some(gw.to_vec()),
                    prefsrc: if i.dhcp { Some(ip.to_vec()) } else { None },
                    priority: None,
                    flags: 0,
                });
            }
            let net = (u32::from_be_bytes(ip) & mask4(p)).to_be_bytes();
            out.push(Route {
                family: AF_INET as u8,
                dst: net.to_vec(),
                dst_len: p,
                table: RT_TABLE_MAIN,
                proto: RTPROT_KERNEL,
                scope: RT_SCOPE_LINK,
                ty: RTN_UNICAST,
                oif: i.index,
                gateway: None,
                prefsrc: Some(ip.to_vec()),
                priority: None,
                flags: 0,
            });
        }
    }
    // Table locale : bouclage, adresses de la machine, diffusion du sous-réseau.
    let local = |dst: Vec<u8>, len: u8, ty: u8, scope: u8, oif: u32, src: Vec<u8>| Route {
        family: if dst.len() == 4 { AF_INET as u8 } else { AF_INET6 as u8 },
        dst,
        dst_len: len,
        table: RT_TABLE_LOCAL,
        proto: RTPROT_KERNEL,
        scope,
        ty,
        oif,
        gateway: None,
        prefsrc: if src.is_empty() { None } else { Some(src) },
        priority: None,
        flags: 0,
    };
    // Routes de `lo` : seulement s'il est allumé (`fib_disable_ip` à l'extinction).
    let lo = ifs.iter().find(|i| i.index == 1).filter(|i| i.flags & IFF_UP != 0);
    if lo.map_or(false, |i| i.v4.map_or(false, |(ip, _)| ip[0] == 127)) {
        out.push(local(vec![127, 0, 0, 0], 8, RTN_LOCAL, RT_SCOPE_HOST, 1, vec![127, 0, 0, 1]));
        out.push(local(vec![127, 0, 0, 1], 32, RTN_LOCAL, RT_SCOPE_HOST, 1, vec![127, 0, 0, 1]));
        out.push(local(vec![127, 255, 255, 255], 32, RTN_BROADCAST, RT_SCOPE_LINK, 1, vec![127, 0, 0, 1]));
    }
    for i in ifs.iter().filter(|i| i.index > 1) {
        if let Some((ip, p)) = i.v4 {
            out.push(local(ip.to_vec(), 32, RTN_LOCAL, RT_SCOPE_HOST, i.index, ip.to_vec()));
            let bcast = (u32::from_be_bytes(ip) | !mask4(p)).to_be_bytes();
            out.push(local(bcast.to_vec(), 32, RTN_BROADCAST, RT_SCOPE_LINK, i.index, ip.to_vec()));
        }
    }
    // IPv6 : `::1` (table principale, métrique 256, et table locale).
    let lo6 = { let mut a = vec![0u8; 16]; a[15] = 1; a };
    if !lo.map_or(false, |i| i.v6.iter().any(|(ip, _)| ip[..] == lo6[..])) {
        return out;
    }
    out.push(Route {
        family: AF_INET6 as u8,
        dst: lo6.clone(),
        dst_len: 128,
        table: RT_TABLE_MAIN,
        proto: RTPROT_KERNEL,
        scope: RT_SCOPE_UNIVERSE,
        ty: RTN_UNICAST,
        oif: 1,
        gateway: None,
        prefsrc: None,
        priority: Some(256),
        flags: 0,
    });
    out.push(local(lo6, 128, RTN_LOCAL, RT_SCOPE_HOST, 1, Vec::new()));
    out
}

/// `ip route get` : la route choisie vers `dst` (IPv4) ; `ENETUNREACH` sans route.
fn route_get(ifs: &[IfInfo], dst: [u8; 4]) -> Result<Route, i64> {
    let d = u32::from_be_bytes(dst);
    let mut r = Route {
        family: AF_INET as u8,
        dst: dst.to_vec(),
        dst_len: 32,
        table: RT_TABLE_MAIN,
        proto: RTPROT_BOOT,
        scope: RT_SCOPE_UNIVERSE,
        ty: RTN_UNICAST,
        oif: 0,
        gateway: None,
        prefsrc: None,
        priority: None,
        flags: RTM_F_CLONED,
    };
    let up = |i: &&IfInfo| i.flags & IFF_UP != 0;
    let own = ifs.iter().filter(up).find(|i| i.v4.map_or(false, |(ip, _)| ip == dst));
    let lo_up = ifs.iter().filter(up).any(|i| i.index == 1 && i.v4.map_or(false, |(ip, _)| ip[0] == 127));
    if (dst[0] == 127 && lo_up) || own.is_some() {
        r.table = RT_TABLE_LOCAL;
        r.ty = RTN_LOCAL;
        r.scope = RT_SCOPE_HOST;
        r.oif = 1;
        r.prefsrc = Some(if dst[0] == 127 { vec![127, 0, 0, 1] } else { dst.to_vec() });
        return Ok(r);
    }
    for i in ifs.iter().filter(|i| i.index > 1) {
        if let Some((ip, p)) = i.v4 {
            if u32::from_be_bytes(ip) & mask4(p) == d & mask4(p) {
                r.oif = i.index;
                r.prefsrc = Some(ip.to_vec());
                return Ok(r);
            }
        }
    }
    let i = ifs.iter().find(|i| i.gw4.is_some()).ok_or(ENETUNREACH)?;
    r.oif = i.index;
    r.gateway = i.gw4.map(|g| g.to_vec());
    r.prefsrc = i.v4.map(|(ip, _)| ip.to_vec());
    Ok(r)
}

// --- Socket -------------------------------------------------------------------------------------

struct State {
    portid: u32,
    groups: u32,
    bound: bool,
    /// Datagrammes formés, prêts à lire.
    rx: VecDeque<Vec<u8>>,
    /// Vidage en cours : messages pas encore mis en datagramme, et `NLMSG_DONE` final.
    dump: VecDeque<Vec<u8>>,
    cap_ack: bool,
    /// Pair fixé par `connect` (pid, groupes).
    peer: (u32, u32),
}

pub struct NetlinkSocket {
    ty: i32,
    proto: i32,
    /// Espace réseau de naissance (`sock_net`).
    net: Arc<NetNs>,
    st: Mutex<State>,
    wq: WaitQueue,
    opts: Mutex<SockOpts>,
    ino: u64,
}

impl NetlinkSocket {
    pub fn create(net: Arc<NetNs>, ty: i32, proto: i32) -> Result<Arc<NetlinkSocket>, i64> {
        if ty != SOCK_RAW && ty != SOCK_DGRAM {
            return Err(ESOCKTNOSUPPORT);
        }
        if proto != NETLINK_ROUTE && proto != NETLINK_KOBJECT_UEVENT {
            return Err(EPROTONOSUPPORT);
        }
        Ok(Arc::new(NetlinkSocket {
            ty,
            proto,
            net,
            st: Mutex::new(State {
                portid: 0,
                groups: 0,
                bound: false,
                rx: VecDeque::new(),
                dump: VecDeque::new(),
                cap_ack: false,
                peer: (0, 0),
            }),
            wq: WaitQueue::new(),
            opts: Mutex::new(SockOpts::default()),
            ino: new_ino(),
        }))
    }

    /// Liaison automatique (`netlink_autobind`) : le pid du processus s'il est libre, sinon un
    /// numéro négatif unique.
    fn autobind(&self, t: &Thread) {
        let mut st = self.st.lock();
        if st.bound {
            return;
        }
        let mut ports = PORTS.lock();
        // Premier port proposé : le pid de l'appelant dans son espace (`netlink_autobind`).
        let mut id = t.process.vpid as u32;
        while ports.contains(&(self.net.ino, self.proto, id)) {
            id = ROVER.fetch_sub(1, core::sync::atomic::Ordering::Relaxed) as u32;
        }
        ports.insert((self.net.ino, self.proto, id));
        st.portid = id;
        st.bound = true;
    }

    fn sockaddr(pid: u32, groups: u32) -> Vec<u8> {
        let mut a = vec![0u8; 12];
        a[0..2].copy_from_slice(&(AF_NETLINK as u16).to_le_bytes());
        a[4..8].copy_from_slice(&pid.to_le_bytes());
        a[8..12].copy_from_slice(&groups.to_le_bytes());
        a
    }

    /// Accusé ou erreur (`netlink_ack`).
    /// `err` : errno positif, 0 pour un accusé. La requête suit entière pour une erreur, sinon
    /// son seul en-tête (`NLM_F_CAPPED`).
    fn ack(&self, req: &[u8], h: &Hdr, err: i64, pid: u32, cap: bool) -> Vec<u8> {
        let mut m = Msg::new(NLMSG_ERROR, 0, h.seq, pid);
        m.raw(&(-(err as i32)).to_le_bytes());
        if err != 0 && !cap {
            m.raw(&req[..h.len]);
            return m.done();
        }
        m.raw(&req[..16]);
        let mut v = m.done();
        v[6..8].copy_from_slice(&NLM_F_CAPPED.to_le_bytes());
        v
    }

    /// Une requête rtnetlink : réponses (`Ok`, vide si rien à dire) ou erreur.
    fn route_request(&self, t: &Thread, req: &[u8], h: &Hdr, pid: u32, out: &mut Vec<Vec<u8>>, dump: &mut Vec<Vec<u8>>) -> Result<(), i64> {
        let body = &req[16..h.len];
        let kind = h.ty & 3; // NEW, DEL, GET, SET
        let is_dump = kind == 2 && h.flags & NLM_F_DUMP == NLM_F_DUMP;
        if !(RTM_BASE..=RTM_MAX).contains(&h.ty) {
            return Err(EOPNOTSUPP);
        }
        if kind != 2 {
            return self.modify(t, h, body);
        }
        let ifs = interfaces(&self.net);
        let multi = NLM_F_MULTI;
        match h.ty {
            RTM_GETLINK if is_dump => {
                for i in &ifs {
                    dump.push(link_msg(i, RTM_NEWLINK, multi, h.seq, pid));
                }
            }
            RTM_GETLINK => {
                // ifinfomsg : indice à l'octet 4 ; sinon IFLA_IFNAME.
                if body.len() < 16 {
                    return Err(EINVAL);
                }
                let index = i32::from_le_bytes(body[4..8].try_into().unwrap());
                let name = attrs(body, 16).into_iter().find(|(ty, _)| *ty == IFLA_IFNAME).map(|(_, d)| {
                    let end = d.iter().position(|&b| b == 0).unwrap_or(d.len());
                    String::from_utf8_lossy(&d[..end]).into_owned()
                });
                let found = if index > 0 {
                    ifs.iter().find(|i| i.index == index as u32)
                } else if let Some(n) = &name {
                    ifs.iter().find(|i| &i.name == n)
                } else {
                    return Err(EINVAL);
                };
                out.push(link_msg(found.ok_or(ENODEV)?, RTM_NEWLINK, 0, h.seq, pid));
            }
            RTM_GETADDR if is_dump => {
                let family = body.first().copied().unwrap_or(0);
                for i in &ifs {
                    addr_msgs(i, family, multi, h.seq, pid, dump);
                }
            }
            RTM_GETROUTE if is_dump => {
                let family = body.first().copied().unwrap_or(0);
                for r in routes(&ifs).iter().filter(|r| family == 0 || r.family == family) {
                    dump.push(r.msg(multi, h.seq, pid));
                }
            }
            RTM_GETROUTE => {
                if body.len() < 12 {
                    return Err(EINVAL);
                }
                if body[0] != AF_INET as u8 {
                    return Err(EAFNOSUPPORT);
                }
                let dst = attrs(body, 12).into_iter().find(|(ty, _)| *ty == RTA_DST).map(|(_, d)| d.to_vec());
                let dst: [u8; 4] = dst.and_then(|d| d.get(..4).map(|s| s.try_into().unwrap())).unwrap_or([0; 4]);
                out.push(route_get(&ifs, dst)?.msg(0, h.seq, pid));
            }
            // Tables que MaiOS n'a pas (voisins, règles, files d'attente, classes…) : vides.
            _ if is_dump => {}
            _ => return Err(EOPNOTSUPP),
        }
        Ok(())
    }

    /// Modification rtnetlink (`rtnl_newlink`, `inet_rtm_newaddr`…) : seul `lo` se configure.
    fn modify(&self, t: &Thread, h: &Hdr, body: &[u8]) -> Result<(), i64> {
        if !crate::userns::ns_capable(&t.process.creds.lock(), &self.net.user_ns, crate::userns::CAP_NET_ADMIN) {
            return Err(EPERM);
        }
        let ifs = interfaces(&self.net);
        match h.ty {
            RTM_NEWLINK | RTM_SETLINK | RTM_DELLINK => {
                if body.len() < 16 {
                    return Err(EINVAL);
                }
                let index = i32::from_le_bytes(body[4..8].try_into().unwrap());
                let flags = u32::from_le_bytes(body[8..12].try_into().unwrap());
                let change = u32::from_le_bytes(body[12..16].try_into().unwrap());
                let name = attrs(body, 16).into_iter().find(|(ty, _)| *ty == IFLA_IFNAME).map(|(_, d)| {
                    let end = d.iter().position(|&b| b == 0).unwrap_or(d.len());
                    d[..end].to_vec()
                });
                let found = if index > 0 {
                    ifs.iter().find(|i| i.index == index as u32)
                } else if let Some(n) = &name {
                    ifs.iter().find(|i| i.name.as_bytes() == &n[..])
                } else {
                    // Création d'une interface (`IFLA_LINKINFO` : veth, bridge…) : pas ici.
                    return Err(EOPNOTSUPP);
                };
                let i = match found {
                    Some(i) => i,
                    None if h.ty == RTM_NEWLINK && index <= 0 => return Err(EOPNOTSUPP),
                    None => return Err(ENODEV),
                };
                if h.ty == RTM_DELLINK || i.index != 1 {
                    return Err(EOPNOTSUPP);
                }
                // `rtnl_dev_combine_flags` : `ifi_change` nul vaut « tous les drapeaux ».
                if flags != 0 || change != 0 {
                    let change = if change == 0 { u32::MAX } else { change };
                    if change & IFF_UP != 0 {
                        self.net.set_lo_up(flags & IFF_UP != 0)?;
                    }
                }
                Ok(())
            }
            RTM_NEWADDR | RTM_DELADDR => {
                if body.len() < 8 {
                    return Err(EINVAL);
                }
                let (family, plen) = (body[0] as i32, body[1]);
                let index = u32::from_le_bytes(body[4..8].try_into().unwrap());
                let a = attrs(body, 8);
                let get = |ty| a.iter().find(|(t, _)| *t == ty).map(|(_, d)| *d);
                let ip = get(IFA_LOCAL).or_else(|| get(IFA_ADDRESS)).ok_or(EINVAL)?;
                let want = if family == AF_INET {
                    4
                } else if family == AF_INET6 {
                    16
                } else {
                    return Err(EAFNOSUPPORT);
                };
                if ip.len() != want {
                    return Err(EINVAL);
                }
                let i = ifs.iter().find(|i| i.index == index).ok_or(ENODEV)?;
                if i.index != 1 {
                    return Err(EOPNOTSUPP);
                }
                if h.ty == RTM_DELADDR {
                    return self.net.del_addr(ip, plen);
                }
                // Adresse déjà là : `EEXIST`, sauf `NLM_F_REPLACE` sans `NLM_F_EXCL`.
                let excl = h.flags & NLM_F_EXCL != 0 || h.flags & NLM_F_REPLACE == 0;
                self.net.add_addr(ip, plen, excl)
            }
            _ => Err(EOPNOTSUPP),
        }
    }

    /// Traite les messages d'un envoi (`netlink_rcv_skb`).
    fn process(&self, t: &Thread, data: &[u8]) {
        let (pid, cap) = {
            let st = self.st.lock();
            (st.portid, st.cap_ack)
        };
        let mut off = 0usize;
        let mut replies: Vec<Vec<u8>> = Vec::new();
        let mut dump: Vec<Vec<u8>> = Vec::new();
        while off + 16 <= data.len() {
            let req = &data[off..];
            let len = u32::from_le_bytes(req[0..4].try_into().unwrap()) as usize;
            if len < 16 || len > req.len() {
                break;
            }
            let h = Hdr {
                len,
                ty: u16::from_le_bytes([req[4], req[5]]),
                flags: u16::from_le_bytes([req[6], req[7]]),
                seq: u32::from_le_bytes(req[8..12].try_into().unwrap()),
            };
            off += align4(len);
            // Seules les requêtes sont traitées ; les messages de contrôle (`NLMSG_NOOP`,
            // `NLMSG_DONE`…) et les réponses sont ignorés, comme `netlink_rcv_skb`.
            if h.flags & NLM_F_REQUEST == 0 || h.ty < RTM_BASE {
                continue;
            }
            let is_dump = h.ty & 3 == 2 && h.flags & NLM_F_DUMP == NLM_F_DUMP;
            let r = if self.proto == NETLINK_ROUTE {
                self.route_request(t, req, &h, pid, &mut replies, &mut dump)
            } else {
                Err(EOPNOTSUPP)
            };
            match r {
                Err(e) => replies.push(self.ack(req, &h, e, pid, cap)),
                Ok(()) if is_dump => {
                    // Fin du vidage : `NLMSG_DONE` (erreur 0), lui aussi `NLM_F_MULTI`.
                    let mut d = Msg::new(NLMSG_DONE, NLM_F_MULTI, h.seq, pid);
                    d.raw(&0i32.to_le_bytes());
                    dump.push(d.done());
                }
                Ok(()) if h.flags & NLM_F_ACK != 0 => replies.push(self.ack(req, &h, 0, pid, cap)),
                Ok(()) => {}
            }
        }
        {
            let mut st = self.st.lock();
            for r in replies {
                st.rx.push_back(r);
            }
            st.dump.extend(dump);
        }
        self.wq.wake_all();
    }

    /// Forme le datagramme suivant d'un vidage, pour un tampon de `room` octets.
    fn form(st: &mut State, room: usize) {
        if !st.rx.is_empty() || st.dump.is_empty() {
            return;
        }
        let limit = room.clamp(GOODSIZE, MAX_DUMP_SKB);
        let mut skb = Vec::new();
        while let Some(m) = st.dump.front() {
            if !skb.is_empty() && skb.len() + m.len() > limit {
                break;
            }
            skb.extend_from_slice(&st.dump.pop_front().unwrap());
        }
        st.rx.push_back(skb);
    }
}

impl Drop for NetlinkSocket {
    fn drop(&mut self) {
        let st = self.st.lock();
        if st.bound {
            PORTS.lock().remove(&(self.net.ino, self.proto, st.portid));
        }
    }
}

impl SocketOps for NetlinkSocket {
    fn domain(&self) -> i32 {
        AF_NETLINK
    }

    fn sock_net(&self) -> Option<Arc<NetNs>> {
        Some(self.net.clone())
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
        if addr.len() < 12 {
            return Err(EINVAL);
        }
        if u16::from_le_bytes([addr[0], addr[1]]) as i32 != AF_NETLINK {
            return Err(EINVAL);
        }
        let pid = u32::from_le_bytes(addr[4..8].try_into().unwrap());
        let groups = u32::from_le_bytes(addr[8..12].try_into().unwrap());
        // Groupes de diffusion : seulement root pour les événements du noyau (uevent), comme
        // `netlink_allowed` ; rtnetlink les permet à tous.
        if groups != 0 && self.proto == NETLINK_KOBJECT_UEVENT && !crate::userns::current_capable(t, crate::userns::CAP_NET_ADMIN) {
            return Err(EPERM);
        }
        {
            let mut st = self.st.lock();
            if st.bound {
                if pid != 0 && pid != st.portid {
                    return Err(EINVAL);
                }
                st.groups = groups;
                return Ok(());
            }
            if pid != 0 {
                let mut ports = PORTS.lock();
                if !ports.insert((self.net.ino, self.proto, pid)) {
                    return Err(EADDRINUSE);
                }
                st.portid = pid;
                st.bound = true;
                st.groups = groups;
                return Ok(());
            }
            st.groups = groups;
        }
        self.autobind(t);
        Ok(())
    }

    fn connect(&self, t: &Thread, addr: &[u8], _nonblock: bool) -> Result<(), i64> {
        if addr.len() >= 2 && u16::from_le_bytes([addr[0], addr[1]]) == 0 {
            self.st.lock().peer = (0, 0);
            return Ok(());
        }
        if addr.len() < 12 || u16::from_le_bytes([addr[0], addr[1]]) as i32 != AF_NETLINK {
            return Err(EINVAL);
        }
        let pid = u32::from_le_bytes(addr[4..8].try_into().unwrap());
        let groups = u32::from_le_bytes(addr[8..12].try_into().unwrap());
        self.autobind(t);
        self.st.lock().peer = (pid, groups);
        Ok(())
    }

    fn listen(&self, _t: &Thread, _backlog: i32) -> Result<(), i64> {
        Err(EOPNOTSUPP)
    }

    fn accept(&self, _t: &Thread, _nonblock: bool) -> Result<(Arc<dyn FileObject>, Vec<u8>), i64> {
        Err(EOPNOTSUPP)
    }

    fn send(&self, t: &Thread, m: SendMsg, _nonblock: bool) -> Result<usize, i64> {
        if let Some(a) = &m.addr {
            if a.len() < 12 || u16::from_le_bytes([a[0], a[1]]) as i32 != AF_NETLINK {
                return Err(EINVAL);
            }
            let dst = u32::from_le_bytes(a[4..8].try_into().unwrap());
            let groups = u32::from_le_bytes(a[8..12].try_into().unwrap());
            // Seul le noyau (pid 0) écoute ici ; la diffusion vers des groupes est réservée.
            if groups != 0 && !crate::userns::current_capable(t, crate::userns::CAP_NET_ADMIN) {
                return Err(EPERM);
            }
            if dst != 0 {
                return Err(ECONNREFUSED);
            }
        }
        if m.data.len() > self.opts.lock().sndbuf {
            return Err(EMSGSIZE);
        }
        self.autobind(t);
        let len = m.data.len();
        self.process(t, &m.data);
        Ok(len)
    }

    fn recv(&self, t: &Thread, buf: &mut [u8], flags: u32, nonblock: bool) -> Result<RecvMeta, i64> {
        let peek = flags & MSG_PEEK != 0;
        let deadline = self.opts.lock().rcv_deadline();
        let r = wait_event(&[&self.wq], deadline, Some(t), || {
            let mut st = self.st.lock();
            Self::form(&mut st, buf.len());
            let d = if peek { st.rx.front().cloned() } else { st.rx.pop_front() };
            if let Some(d) = d {
                let n = d.len().min(buf.len());
                buf[..n].copy_from_slice(&d[..n]);
                return Some(Ok(RecvMeta {
                    len: n,
                    full_len: d.len(),
                    addr: Some(Self::sockaddr(0, 0)),
                    flags: if d.len() > n { MSG_TRUNC } else { 0 },
                    ..Default::default()
                }));
            }
            if nonblock { Some(Err(EAGAIN)) } else { None }
        });
        match r {
            Ok(v) => v,
            Err(WaitError::Interrupted) => Err(crate::signal::ERESTARTSYS),
            Err(WaitError::TimedOut) => Err(EAGAIN),
        }
    }

    fn shutdown(&self, _how: i32) -> Result<(), i64> {
        Err(EOPNOTSUPP)
    }

    fn sockname(&self) -> Vec<u8> {
        let st = self.st.lock();
        Self::sockaddr(st.portid, st.groups)
    }

    fn peername(&self) -> Result<Vec<u8>, i64> {
        let st = self.st.lock();
        Ok(Self::sockaddr(st.peer.0, st.peer.1))
    }

    fn is_listening(&self) -> bool {
        false
    }

    fn setsockopt(&self, level: i32, name: i32, val: &[u8]) -> Result<(), i64> {
        if level != SOL_NETLINK {
            return Err(ENOPROTOOPT);
        }
        let v = val.get(..4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).ok_or(EINVAL)?;
        let mut st = self.st.lock();
        match name {
            NETLINK_ADD_MEMBERSHIP | NETLINK_DROP_MEMBERSHIP => {
                if v == 0 || v > 32 {
                    return Err(EINVAL);
                }
                let bit = 1u32 << (v - 1);
                if name == NETLINK_ADD_MEMBERSHIP {
                    st.groups |= bit;
                } else {
                    st.groups &= !bit;
                }
            }
            NETLINK_CAP_ACK => st.cap_ack = v != 0,
            // NETLINK_PKTINFO, BROADCAST_ERROR, NO_ENOBUFS, LISTEN_ALL_NSID, EXT_ACK,
            // GET_STRICT_CHK : sans effet sur des réponses toujours complètes et ordonnées.
            3 | 4 | 5 | 8 | 11 | 12 => {}
            _ => return Err(ENOPROTOOPT),
        }
        Ok(())
    }

    fn getsockopt(&self, level: i32, name: i32) -> Result<Vec<u8>, i64> {
        if level != SOL_NETLINK {
            return Err(ENOPROTOOPT);
        }
        let st = self.st.lock();
        let v: u32 = match name {
            NETLINK_CAP_ACK => st.cap_ack as u32,
            3 | 4 | 5 | 8 | 11 | 12 => 0,
            _ => return Err(ENOPROTOOPT),
        };
        Ok(v.to_le_bytes().to_vec())
    }
}

impl FileObject for NetlinkSocket {
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
        let st = self.st.lock();
        let mut r = ev::WRITABLE;
        if !st.rx.is_empty() || !st.dump.is_empty() {
            r |= ev::READABLE;
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
        Some(self.st.lock().rx.front().map_or(0, |d| d.len()))
    }

    fn fdinfo(&self) -> String {
        let st = self.st.lock();
        format!("netlink-protocol: {}\nnetlink-portid: {}\n", self.proto, st.portid)
    }
}
