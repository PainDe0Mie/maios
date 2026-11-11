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
pub use interface::{IpAddress, IpCidr, NetworkInterface, SocketSet, ADDR_DHCP, ADDR_NONE, ADDR_STATIC};
pub use smoltcp::{
    iface::SocketHandle,
    phy,
    socket::{icmp, raw, tcp, udp},
    time::{self as stime, Instant},
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
/// L'interface est créée sans adresse et enregistrée tout de suite : le client DHCP avance à
/// chaque passage de `poll` (paquets reçus par l'interruption de la carte) et sur ses minuteurs
/// (tâche [`net_timer_task`]). Si aucun serveur ne répond en [`DHCP_FALLBACK_AFTER`], le repli
/// statique est posé ; un bail obtenu plus tard le remplace.
///
/// (L'ancienne attente « de 5 s » faisait 5000 `spin_loop()`, quelques microsecondes : l'offre
/// du serveur n'arrivait jamais à temps et le repli était toujours pris.)
pub fn register_device<T>(device: &'static IrqSafeMutex<T>) -> Arc<NetworkInterface>
where
    T: 'static + NetworkDevice + Send,
{
    let interface_arc = Arc::new(NetworkInterface::new_dhcp(device));
    // Premier passage : la découverte DHCP part tout de suite.
    interface_arc.poll();
    NETWORK_INTERFACES.lock().push(interface_arc.clone());
    start_timer_task();
    interface_arc
}

/// Délai avant le repli statique, faute de bail DHCP.
pub const DHCP_FALLBACK_AFTER: core::time::Duration = core::time::Duration::from_secs(5);

static TIMER_TASK: spin::Once<()> = spin::Once::new();

fn start_timer_task() {
    TIMER_TASK.call_once(|| {
        if let Err(e) = spawn::new_task_builder(net_timer_task, ())
            .name(alloc::string::String::from("net_timer"))
            .spawn()
        {
            log::error!("net : tâche des minuteurs non créée : {}", e);
        }
    });
}

/// Minuteurs des interfaces, sans tick : la tâche dort jusqu'à la prochaine échéance annoncée par
/// smoltcp (retransmission DHCP, renouvellement du bail, minuteurs TCP des sockets du noyau),
/// au plus 100 ms tant qu'une interface attend son adresse, 5 s sinon. Les paquets reçus, eux,
/// sont servis par l'interruption de la carte. (Les sockets des processus ont en plus la tâche
/// `netpoll` de MaiProc.)
fn net_timer_task(_: ()) {
    let start = ::time::Instant::now();
    loop {
        let ifaces: Vec<Arc<NetworkInterface>> = NETWORK_INTERFACES.lock().clone();
        let mut next = core::time::Duration::from_secs(5);
        for i in &ifaces {
            i.poll();
            if i.addr_state() == interface::ADDR_NONE {
                if ::time::Instant::now().duration_since(start) >= DHCP_FALLBACK_AFTER {
                    info!("DHCP : aucun serveur, repli statique {}", FALLBACK_LOCAL_IP);
                    i.set_static(FALLBACK_LOCAL_IP.parse().unwrap(), FALLBACK_GATEWAY_IP);
                    set_dns_servers(&[Ipv4Address::new(10, 0, 2, 2)], false);
                } else {
                    next = next.min(core::time::Duration::from_millis(100));
                }
            }
            if let Some(d) = i.poll_delay() {
                next = next.min(d);
            }
        }
        let _ = sleep::sleep(next.max(core::time::Duration::from_millis(1)));
    }
}

/// Remplace la liste des serveurs DNS (`replace`), ou la pose si elle est vide (repli).
pub(crate) fn set_dns_servers(servers: &[Ipv4Address], replace: bool) {
    let mut dns = DNS_SERVERS.lock();
    if replace || dns.is_empty() {
        dns.clear();
        dns.extend_from_slice(servers);
    }
}

/// Horloge des sockets de smoltcp : le temps monotone du noyau. (Le temps figé à zéro de
/// l'héritage de Theseus désactivait tous les minuteurs TCP : retransmission, ACK différé,
/// `TIME_WAIT`, sondes de fenêtre nulle.)
pub fn now() -> Instant {
    let d = ::time::Instant::now().duration_since(::time::Instant::ZERO);
    Instant::from_micros(d.as_micros() as i64)
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
