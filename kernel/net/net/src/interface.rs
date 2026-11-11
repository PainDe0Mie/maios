use alloc::{sync::Arc, vec, vec::Vec};
use core::marker::PhantomData;
use core::sync::atomic::{AtomicU8, Ordering};

use log::info;
use smoltcp::{iface, phy::DeviceCapabilities, socket::AnySocket, wire};
pub use smoltcp::{
    iface::SocketSet,
    wire::{IpAddress, IpCidr},
};
use smoltcp::socket::dhcpv4;
use sync_block::Mutex;
use sync_irq::IrqSafeMutex;

use crate::{device::{DeviceWrapper, Stats}, NetworkDevice, Socket};

/// A network interface.
///
/// This is a wrapper around a network device which provides higher level
/// abstractions such as polling sockets.
pub struct NetworkInterface {
    pub inner: Mutex<iface::Interface>,
    device: &'static IrqSafeMutex<dyn crate::NetworkDevice>,
    pub sockets: Mutex<SocketSet<'static>>,
    /// Handle du socket DHCP, si initialisé via `new_dhcp()`.
    dhcp_handle: Option<smoltcp::iface::SocketHandle>,
    /// État de l'adressage : [`ADDR_NONE`], [`ADDR_DHCP`] ou [`ADDR_STATIC`].
    addr_state: AtomicU8,
    /// Trames et octets reçus et émis.
    stats: Stats,
}

/// Pas encore d'adresse (DHCP en cours).
pub const ADDR_NONE: u8 = 0;
/// Adresse obtenue par DHCP (bail en cours).
pub const ADDR_DHCP: u8 = 1;
/// Adresse statique : fixée à la création, ou repli faute de serveur DHCP.
pub const ADDR_STATIC: u8 = 2;

impl NetworkInterface {
    /// Crée une interface avec une IP statique et un gateway.
    pub(crate) fn new<T>(device: &'static IrqSafeMutex<T>, ip: IpCidr, gateway: IpAddress) -> Self
    where
        T: NetworkDevice,
    {
        let hardware_addr = wire::EthernetAddress(device.lock().mac_address()).into();

        let boot_stats = Stats::default();
        let mut wrapper = DeviceWrapper {
            inner: &mut *device.lock(),
            stats: &boot_stats,
        };

        let mut config = iface::Config::new(hardware_addr);
        config.random_seed = random::next_u64();

        let mut interface =
            iface::Interface::new(config, &mut wrapper, smoltcp::time::Instant::ZERO);
        interface.update_ip_addrs(|ip_addrs| {
            ip_addrs.push(ip).unwrap();
        });
        match gateway {
            IpAddress::Ipv4(addr) => interface.routes_mut().add_default_ipv4_route(addr),
            IpAddress::Ipv6(addr) => interface.routes_mut().add_default_ipv6_route(addr),
        }
        .expect("btree map route storage exhausted");

        Self {
            inner: Mutex::new(interface),
            device,
            sockets: Mutex::new(SocketSet::new(Vec::new())),
            dhcp_handle: None,
            addr_state: AtomicU8::new(ADDR_STATIC),
            stats: Stats::default(),
        }
    }

    /// Crée une interface sans IP — sera configurée par DHCP.
    pub(crate) fn new_dhcp<T>(device: &'static IrqSafeMutex<T>) -> Self
    where
        T: NetworkDevice,
    {
        let hardware_addr = wire::EthernetAddress(device.lock().mac_address()).into();

        let boot_stats = Stats::default();
        let mut wrapper = DeviceWrapper {
            inner: &mut *device.lock(),
            stats: &boot_stats,
        };

        let mut config = iface::Config::new(hardware_addr);
        config.random_seed = random::next_u64();

        let interface =
            iface::Interface::new(config, &mut wrapper, smoltcp::time::Instant::ZERO);

        let mut sockets = SocketSet::new(Vec::new());

        // Ajouter un socket DHCP pour l'acquisition automatique d'IP.
        let dhcp_socket = dhcpv4::Socket::new();
        let dhcp_handle = sockets.add(dhcp_socket);

        Self {
            inner: Mutex::new(interface),
            device,
            sockets: Mutex::new(sockets),
            dhcp_handle: Some(dhcp_handle),
            addr_state: AtomicU8::new(ADDR_NONE),
            stats: Stats::default(),
        }
    }

    /// État de l'adressage ([`ADDR_NONE`], [`ADDR_DHCP`], [`ADDR_STATIC`]).
    pub fn addr_state(&self) -> u8 {
        self.addr_state.load(Ordering::Acquire)
    }

    /// Fixe une adresse statique (repli quand aucun serveur DHCP ne répond). Le client DHCP
    /// continue : un bail obtenu plus tard remplace cette adresse.
    pub(crate) fn set_static(&self, ip: IpCidr, gateway: IpAddress) {
        let mut inner = self.inner.lock();
        inner.update_ip_addrs(|addrs| {
            addrs.clear();
            let _ = addrs.push(ip);
        });
        match gateway {
            IpAddress::Ipv4(addr) => { let _ = inner.routes_mut().add_default_ipv4_route(addr); }
            IpAddress::Ipv6(addr) => { let _ = inner.routes_mut().add_default_ipv6_route(addr); }
        }
        self.addr_state.store(ADDR_STATIC, Ordering::Release);
    }

    /// Événements du client DHCP (smoltcp `dhcpv4`), traités à chaque passage de [`poll`] :
    /// le bail est obtenu, renouvelé ou perdu au fil des paquets reçus, sans boucle d'attente au
    /// démarrage. Retourne `true` si la configuration a changé.
    ///
    /// [`poll`]: Self::poll
    fn dhcp_events(&self, inner: &mut iface::Interface, sockets: &mut SocketSet<'static>) -> bool {
        let Some(h) = self.dhcp_handle else { return false };
        let dhcp: &mut dhcpv4::Socket = sockets.get_mut(h);
        match dhcp.poll() {
            Some(dhcpv4::Event::Configured(config)) => {
                inner.update_ip_addrs(|addrs| {
                    addrs.clear();
                    let _ = addrs.push(IpCidr::Ipv4(config.address));
                });
                match config.router {
                    Some(gw) => { let _ = inner.routes_mut().add_default_ipv4_route(gw); }
                    None => { inner.routes_mut().remove_default_ipv4_route(); }
                }
                let dns: Vec<wire::Ipv4Address> = config.dns_servers.iter().copied().collect();
                info!("DHCP : bail obtenu, adresse {}, passerelle {:?}, DNS {:?}", config.address, config.router, dns);
                crate::set_dns_servers(&dns, true);
                self.addr_state.store(ADDR_DHCP, Ordering::Release);
                true
            }
            Some(dhcpv4::Event::Deconfigured) => {
                // Bail expiré ou perdu : le client repart en découverte, et la tâche du réseau
                // reprendra le repli statique si aucun serveur ne répond.
                if self.addr_state.load(Ordering::Acquire) == ADDR_DHCP {
                    log::warn!("DHCP : bail perdu, adresse retiree");
                    inner.update_ip_addrs(|addrs| addrs.clear());
                    inner.routes_mut().remove_default_ipv4_route();
                    self.addr_state.store(ADDR_NONE, Ordering::Release);
                }
                true
            }
            None => false,
        }
    }

    /// Adds a socket to the interface.
    pub fn add_socket<T>(self: Arc<Self>, socket: T) -> Socket<T>
    where
        T: AnySocket<'static>,
    {
        let handle = self.sockets.lock().add(socket);
        Socket {
            handle,
            interface: self,
            phantom_data: PhantomData,
        }
    }

    /// Polls the sockets associated with the interface.
    ///
    /// Returns a boolean indicating whether the readiness of any socket may
    /// have changed.
    pub fn poll(&self) -> bool {
        // Ordre : `inner`, `sockets`, puis la carte. Le verrou de la carte coupe les
        // interruptions et son gestionnaire d'interruption le prend : il ne doit jamais être tenu
        // pendant l'attente d'un verrou bloquant. Pris avant `sockets`, il bloquait tout : un fil
        // tenant `sockets` (`recv`) interrompu par la carte attendait la carte, tenue par ce
        // sondage qui attendait `sockets` (et les invalidations TLB visant ce cœur avec).
        let mut inner = self.inner.lock();
        let mut sockets = self.sockets.lock();
        let mut wrapper = DeviceWrapper {
            inner: &mut *self.device.lock(),
            stats: &self.stats,
        };

        let ready = inner.poll(crate::now(), &mut wrapper, &mut sockets);
        self.dhcp_events(&mut inner, &mut sockets) || ready
    }

    /// Délai avant la prochaine échéance d'un minuteur de socket (retransmission, ACK différé,
    /// `TIME_WAIT`…) ; `None` : aucune. Sert à dormir jusqu'à l'échéance au lieu de sonder à
    /// intervalle fixe.
    pub fn poll_delay(&self) -> Option<core::time::Duration> {
        let mut inner = self.inner.lock();
        let sockets = self.sockets.lock();
        inner.poll_delay(crate::now(), &sockets).map(|d| core::time::Duration::from_micros(d.total_micros()))
    }

    /// Adresses IP de l'interface.
    pub fn ip_addrs(&self) -> Vec<IpCidr> {
        self.inner.lock().ip_addrs().to_vec()
    }

    /// Passerelle IPv4 par défaut (route `0.0.0.0/0`), s'il y en a une.
    pub fn default_gateway_v4(&self) -> Option<[u8; 4]> {
        let mut gw = None;
        self.inner.lock().routes_mut().update(|routes| {
            for r in routes.iter() {
                if let (IpCidr::Ipv4(c), IpAddress::Ipv4(a)) = (r.cidr, r.via_router) {
                    if c.prefix_len() == 0 {
                        gw = Some(a.0);
                    }
                }
            }
        });
        gw
    }

    /// Compteurs `(octets reçus, trames reçues, octets émis, trames émises, trames jetées à
    /// l'émission)`.
    pub fn stats(&self) -> [u64; 5] {
        use core::sync::atomic::Ordering::Relaxed;
        let s = &self.stats;
        [
            s.rx_bytes.load(Relaxed),
            s.rx_packets.load(Relaxed),
            s.tx_bytes.load(Relaxed),
            s.tx_packets.load(Relaxed),
            s.tx_dropped.load(Relaxed),
        ]
    }

    /// Adresse matérielle (MAC).
    pub fn mac_address(&self) -> [u8; 6] {
        self.device.lock().mac_address()
    }

    pub fn capabilities(&self) -> DeviceCapabilities {
        self.device.lock().capabilities()
    }
}
