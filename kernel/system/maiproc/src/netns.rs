//! Espaces réseau (`CLONE_NEWNET`, `network_namespaces(7)`).
//!
//! Modèle de Linux (`struct net`) : chaque socket appartient pour toujours à l'espace où elle a
//! été créée (`sock_net`), même si son processus change d'espace ensuite (`setns`) ; c'est
//! l'espace de la socket, pas celui de l'appelant, qui choisit les tables de ports, les noms
//! abstraits Unix, les identifiants netlink et les interfaces vues par `ioctl(SIOCGIF*)`.
//!
//! - L'espace initial possède les cartes réelles (smoltcp) et un bouclage toujours actif.
//! - Un nouvel espace ne contient que `lo`, **éteint et sans adresse** comme sous Linux :
//!   rien n'y est joignable (`ENETUNREACH`, `EADDRNOTAVAIL` sur `bind(127.0.0.1)`) tant que
//!   l'espace ne l'a pas allumé (`RTM_NEWLINK` + `IFF_UP`, `SIOCSIFFLAGS`), ce que fait
//!   bubblewrap (`loopback_setup` : `RTM_NEWADDR 127.0.0.1/8`, puis `IFF_UP`). L'allumage
//!   ajoute `127.0.0.1/8` et `::1/128` (`inetdev_event`, `addrconf`), l'extinction retire les
//!   adresses IPv6 (`addrconf_ifdown`) et garde celles d'IPv4.
//! - Le bouclage d'un espace sert le chemin rapide local de [`crate::inet`] : deux espaces ne
//!   se voient jamais (tables de ports séparées). Les sockets Unix liées à un **chemin**
//!   restent joignables d'un espace à l'autre (le nœud du système de fichiers fait foi, comme
//!   sous Linux : c'est ce qui laisse passer X11 et Wayland dans un bac à sable sans réseau) ;
//!   les noms abstraits, eux, sont propres à l'espace.

use alloc::{sync::Arc, vec::Vec};

use crate::linux::errno::*;
use crate::userns::{self, UserNs};

pub const IFF_UP: u32 = 0x1;
pub const IFF_LOOPBACK: u32 = 0x8;
pub const IFF_RUNNING: u32 = 0x40;
pub const IFF_LOWER_UP: u32 = 0x1_0000;

const LO4: ([u8; 4], u8) = ([127, 0, 0, 1], 8);
const LO6: ([u8; 16], u8) = ([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1], 128);

/// État du bouclage d'un espace.
#[derive(Clone)]
pub struct Lo {
    pub up: bool,
    pub v4: Vec<([u8; 4], u8)>,
    pub v6: Vec<([u8; 16], u8)>,
}

impl Lo {
    /// `a` (IPv4) est-il une adresse locale (table `local`) ? `127.0.0.1/8` couvre tout
    /// `127.0.0.0/8` (route `local 127.0.0.0/8 dev lo`) ; une autre adresse n'est locale
    /// qu'elle-même.
    pub fn local4(&self, a: [u8; 4]) -> bool {
        self.up
            && self.v4.iter().any(|&(ip, p)| {
                ip == a || (ip[0] == 127 && a[0] == 127 && (p == 0 || (u32::from_be_bytes(ip) ^ u32::from_be_bytes(a)) >> (32 - p as u32) == 0))
            })
    }

    pub fn local6(&self, a: [u8; 16]) -> bool {
        self.up && self.v6.iter().any(|&(ip, _)| ip == a)
    }
}

/// Un espace réseau.
pub struct NetNs {
    pub ino: u64,
    /// Propriétaire (droits `CAP_NET_ADMIN`, `CAP_NET_BIND_SERVICE`… s'y jugent).
    pub user_ns: Arc<UserNs>,
    /// Espace initial : cartes réelles, bouclage fixe.
    pub init: bool,
    lo: spin::Mutex<Lo>,
    /// Tables de liaison TCP, UDP, UDP-Lite.
    pub(crate) ports: crate::inet::Ports,
}

static INIT: spin::Once<Arc<NetNs>> = spin::Once::new();

/// Espace réseau initial.
pub fn init_net() -> Arc<NetNs> {
    INIT.call_once(|| {
        Arc::new(NetNs {
            ino: userns::INIT_NET_INO,
            user_ns: userns::init_user_ns(),
            init: true,
            lo: spin::Mutex::new(Lo { up: true, v4: alloc::vec![LO4], v6: alloc::vec![LO6] }),
            ports: crate::inet::Ports::new(),
        })
    })
    .clone()
}

impl NetNs {
    /// Nouvel espace (`copy_net_ns`) : `lo` éteint, sans adresse.
    pub fn new(user_ns: &Arc<UserNs>) -> Arc<NetNs> {
        Arc::new(NetNs {
            ino: userns::alloc_ns_ino(),
            user_ns: user_ns.clone(),
            init: false,
            lo: spin::Mutex::new(Lo { up: false, v4: Vec::new(), v6: Vec::new() }),
            ports: crate::inet::Ports::new(),
        })
    }

    /// Copie de l'état du bouclage.
    pub fn lo(&self) -> Lo {
        self.lo.lock().clone()
    }

    /// Drapeaux de `lo` (`IFF_*`).
    pub fn lo_flags(&self) -> u32 {
        if self.lo.lock().up { IFF_UP | IFF_LOOPBACK | IFF_RUNNING | IFF_LOWER_UP } else { IFF_LOOPBACK }
    }

    /// Le bouclage de l'espace initial ne se reconfigure pas (il porte le chemin rapide de
    /// toute la machine) : `EOPNOTSUPP`, comme avant les espaces réseau.
    fn mutable(&self) -> Result<(), i64> {
        if self.init { Err(EOPNOTSUPP) } else { Ok(()) }
    }

    /// Allume ou éteint `lo` (`dev_change_flags`).
    pub fn set_lo_up(&self, up: bool) -> Result<(), i64> {
        if self.init && up {
            return Ok(());
        }
        self.mutable()?;
        let mut lo = self.lo.lock();
        if up == lo.up {
            return Ok(());
        }
        lo.up = up;
        if up {
            if !lo.v4.iter().any(|&(ip, _)| ip == LO4.0) {
                lo.v4.push(LO4);
            }
            if !lo.v6.iter().any(|&(ip, _)| ip == LO6.0) {
                lo.v6.push(LO6);
            }
        } else {
            lo.v6.clear();
        }
        Ok(())
    }

    /// `RTM_NEWADDR` sur `lo`. `excl` : `NLM_F_EXCL` (adresse déjà là → `EEXIST`).
    pub fn add_addr(&self, ip: &[u8], plen: u8, excl: bool) -> Result<(), i64> {
        self.mutable()?;
        let mut lo = self.lo.lock();
        match ip.len() {
            4 => {
                if plen > 32 {
                    return Err(EINVAL);
                }
                let a: [u8; 4] = ip.try_into().unwrap();
                if lo.v4.iter().any(|&(x, p)| x == a && p == plen) {
                    return if excl { Err(EEXIST) } else { Ok(()) };
                }
                lo.v4.push((a, plen));
            }
            16 => {
                if plen > 128 {
                    return Err(EINVAL);
                }
                let a: [u8; 16] = ip.try_into().unwrap();
                if lo.v6.iter().any(|&(x, _)| x == a) {
                    return if excl { Err(EEXIST) } else { Ok(()) };
                }
                lo.v6.push((a, plen));
            }
            _ => return Err(EINVAL),
        }
        Ok(())
    }

    /// `RTM_DELADDR` sur `lo`.
    pub fn del_addr(&self, ip: &[u8], plen: u8) -> Result<(), i64> {
        self.mutable()?;
        let mut lo = self.lo.lock();
        let before = lo.v4.len() + lo.v6.len();
        match ip.len() {
            4 => lo.v4.retain(|&(x, p)| !(x[..] == *ip && (plen == 0 || p == plen))),
            16 => lo.v6.retain(|&(x, _)| x[..] != *ip),
            _ => return Err(EINVAL),
        }
        if lo.v4.len() + lo.v6.len() == before {
            return Err(EADDRNOTAVAIL);
        }
        Ok(())
    }
}

/// Espace réseau du processus.
pub fn of(p: &crate::Process) -> Arc<NetNs> {
    crate::ns::of(p).net.clone()
}
