#![no_std]

extern crate alloc;

use alloc::{sync::Arc, vec::Vec};

use log::info;
use smoltcp::wire::Ipv4Address;
use spin::Mutex;
use sync_irq::IrqSafeMutex;

mod device;
mod interface;
mod socket;

pub use device::{DeviceCapabilities, NetworkDevice};
pub use interface::{IpAddress, IpCidr, NetworkInterface, SocketSet};
pub use smoltcp::{
    phy,
    socket::{icmp, tcp, udp},
    time::Instant,
    wire::{self, IpEndpoint, IpListenEndpoint},
};
pub use socket::{LockedSocket, Socket};

/// Fallback statique si DHCP échoue.
const FALLBACK_LOCAL_IP: &str = "10.0.2.15/24";
const FALLBACK_GATEWAY_IP: IpAddress = IpAddress::Ipv4(Ipv4Address::new(10, 0, 2, 2));

static NETWORK_INTERFACES: Mutex<Vec<Arc<NetworkInterface>>> = Mutex::new(Vec::new());

/// Serveurs DNS obtenus via DHCP. Protégé par un Mutex.
static DNS_SERVERS: Mutex<Vec<Ipv4Address>> = Mutex::new(Vec::new());

/// Enregistre un périphérique réseau et démarre DHCP.
///
/// L'interface est créée sans IP. Un polling DHCP de 5 secondes max tente
/// d'obtenir une adresse. En cas d'échec, un fallback statique est utilisé.
pub fn register_device<T>(device: &'static IrqSafeMutex<T>) -> Arc<NetworkInterface>
where
    T: 'static + NetworkDevice + Send,
{
    // Créer l'interface sans IP — DHCP va la configurer
    let interface = NetworkInterface::new_dhcp(device);
    let interface_arc = Arc::new(interface);

    // Tenter DHCP pendant 5 secondes max (5000 polls à ~1ms chacun)
    let mut dhcp_success = false;
    for _ in 0..5000 {
        if let Some((_ip, _gw, dns)) = interface_arc.poll_dhcp() {
            // Stocker les DNS servers
            let mut dns_lock = DNS_SERVERS.lock();
            dns_lock.clear();
            for srv in dns.iter().flatten() {
                dns_lock.push(*srv);
            }
            dhcp_success = true;
            break;
        }
        // Petit yield — on est encore en early boot, pas de sleep disponible
        core::hint::spin_loop();
    }

    if !dhcp_success {
        info!("DHCP: timeout, using static fallback {}", FALLBACK_LOCAL_IP);
        // Fallback : configurer statiquement
        {
            let mut inner = interface_arc.inner.lock();
            inner.update_ip_addrs(|addrs| {
                addrs.clear();
                addrs.push(FALLBACK_LOCAL_IP.parse().unwrap()).unwrap();
            });
            match FALLBACK_GATEWAY_IP {
                IpAddress::Ipv4(addr) => { inner.routes_mut().add_default_ipv4_route(addr).ok(); }
                IpAddress::Ipv6(addr) => { inner.routes_mut().add_default_ipv6_route(addr).ok(); }
            }
        }
        // Fallback DNS = gateway
        let mut dns_lock = DNS_SERVERS.lock();
        if dns_lock.is_empty() {
            dns_lock.push(Ipv4Address::new(10, 0, 2, 2));
        }
    }

    NETWORK_INTERFACES.lock().push(interface_arc.clone());
    interface_arc
}

/// Returns a list of available interfaces behind a mutex.
pub fn get_interfaces() -> &'static Mutex<Vec<Arc<NetworkInterface>>> {
    &NETWORK_INTERFACES
}

/// Returns the first available interface.
pub fn get_default_interface() -> Option<Arc<NetworkInterface>> {
    NETWORK_INTERFACES.lock().first().cloned()
}

/// Retourne les serveurs DNS configurés (via DHCP ou fallback).
pub fn get_dns_servers() -> Vec<Ipv4Address> {
    DNS_SERVERS.lock().clone()
}

/// Returns a port in the range reserved for private, dynamic, and ephemeral
/// ports.
pub fn get_ephemeral_port() -> u16 {
    use rand::Rng;

    const RANGE_START: u16 = 49152;
    const RANGE_END: u16 = u16::MAX;

    let mut rng = random::init_rng::<rand_chacha::ChaChaRng>().unwrap();
    rng.gen_range(RANGE_START..=RANGE_END)
}
