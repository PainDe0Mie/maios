use alloc::{sync::Arc, vec, vec::Vec};
use core::marker::PhantomData;

use log::info;
use smoltcp::{iface, phy::DeviceCapabilities, socket::AnySocket, wire};
pub use smoltcp::{
    iface::SocketSet,
    wire::{IpAddress, IpCidr},
};
use smoltcp::socket::dhcpv4;
use sync_block::Mutex;
use sync_irq::IrqSafeMutex;

use crate::{device::DeviceWrapper, NetworkDevice, Socket};

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
}

impl NetworkInterface {
    /// Crée une interface avec une IP statique et un gateway.
    pub(crate) fn new<T>(device: &'static IrqSafeMutex<T>, ip: IpCidr, gateway: IpAddress) -> Self
    where
        T: NetworkDevice,
    {
        let hardware_addr = wire::EthernetAddress(device.lock().mac_address()).into();

        let mut wrapper = DeviceWrapper {
            inner: &mut *device.lock(),
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
        }
    }

    /// Crée une interface sans IP — sera configurée par DHCP.
    pub(crate) fn new_dhcp<T>(device: &'static IrqSafeMutex<T>) -> Self
    where
        T: NetworkDevice,
    {
        let hardware_addr = wire::EthernetAddress(device.lock().mac_address()).into();

        let mut wrapper = DeviceWrapper {
            inner: &mut *device.lock(),
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
        }
    }

    /// Poll le DHCP socket et applique la configuration si disponible.
    /// Retourne `Some((ip, gateway, dns_servers))` si DHCP vient de se configurer.
    pub fn poll_dhcp(&self) -> Option<(wire::Ipv4Address, wire::Ipv4Address, [Option<wire::Ipv4Address>; 3])> {
        let dhcp_handle = self.dhcp_handle?;

        let mut inner = self.inner.lock();
        let mut wrapper = DeviceWrapper {
            inner: &mut *self.device.lock(),
        };
        let mut sockets = self.sockets.lock();

        // Poll first
        inner.poll(smoltcp::time::Instant::ZERO, &mut wrapper, &mut sockets);

        // Accéder au socket DHCP par handle
        let dhcp: &mut dhcpv4::Socket = sockets.get_mut(dhcp_handle);
        match dhcp.poll() {
            Some(dhcpv4::Event::Configured(config)) => {
                let ip_cidr = config.address;
                let gateway = config.router.unwrap_or(wire::Ipv4Address::UNSPECIFIED);

                // Collecter les DNS servers
                let mut dns = [None; 3];
                for (i, srv) in config.dns_servers.iter().enumerate() {
                    if i < 3 {
                        dns[i] = Some(*srv);
                    }
                }

                // Appliquer la configuration IP
                inner.update_ip_addrs(|addrs| {
                    addrs.clear();
                    addrs.push(IpCidr::Ipv4(ip_cidr)).unwrap();
                });
                inner.routes_mut().add_default_ipv4_route(gateway)
                    .expect("route storage exhausted");

                info!("DHCP: got IP {}, gateway {}, DNS {:?}",
                    ip_cidr, gateway, dns);

                Some((ip_cidr.address(), gateway, dns))
            }
            Some(dhcpv4::Event::Deconfigured) => {
                info!("DHCP: lease expired, deconfigured");
                inner.update_ip_addrs(|addrs| addrs.clear());
                None
            }
            None => None,
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
        let mut inner = self.inner.lock();
        let mut wrapper = DeviceWrapper {
            inner: &mut *self.device.lock(),
        };
        let mut sockets = self.sockets.lock();

        inner.poll(smoltcp::time::Instant::ZERO, &mut wrapper, &mut sockets)
    }

    pub fn capabilities(&self) -> DeviceCapabilities {
        self.device.lock().capabilities()
    }
}
