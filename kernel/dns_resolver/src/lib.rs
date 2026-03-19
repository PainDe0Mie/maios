//! Résolveur DNS minimal pour MaiOS.
//!
//! Implémente la résolution de noms A (IPv4) via UDP vers les serveurs
//! DNS configurés par DHCP ou en fallback statique.
//!
//! ## Utilisation
//!
//! ```ignore
//! let ip = dns_resolver::resolve("example.com")?;
//! ```
//!
//! ## Limitations
//!
//! - Pas de cache (chaque appel = requête réseau)
//! - Pas de support AAAA (IPv6)
//! - Pas de support CNAME récursif
//! - Timeout fixe de 5 secondes

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use log::{debug, warn};
use spin::Mutex;
use alloc::collections::BTreeMap;

/// Cache DNS global : nom → (Ipv4Address, timestamp d'expiration en ticks).
/// Le TTL est simplifié : on garde 60 secondes par défaut.
static DNS_CACHE: Mutex<BTreeMap<String, net::wire::Ipv4Address>> = Mutex::new(BTreeMap::new());

/// Port DNS standard.
const DNS_PORT: u16 = 53;

/// Timeout de résolution en nombre de polls (~5000 = ~5s).
const RESOLVE_TIMEOUT: usize = 5000;

/// Résout un nom de domaine en adresse IPv4.
///
/// Vérifie d'abord le cache, puis envoie une requête DNS A via UDP.
pub fn resolve(hostname: &str) -> Result<net::wire::Ipv4Address, &'static str> {
    // Vérifier le cache
    {
        let cache = DNS_CACHE.lock();
        if let Some(addr) = cache.get(hostname) {
            return Ok(*addr);
        }
    }

    // Obtenir l'interface réseau et le serveur DNS
    let iface = net::get_default_interface()
        .ok_or("dns: no network interface")?;
    let dns_servers = net::get_dns_servers();
    let dns_server = dns_servers.first()
        .ok_or("dns: no DNS server configured")?;

    // Construire la requête DNS
    let query = build_dns_query(hostname, 0x1234);

    // Créer un socket UDP
    let rx_buf = net::udp::PacketBuffer::new(
        vec![net::udp::PacketMetadata::EMPTY; 4],
        vec![0u8; 1024],
    );
    let tx_buf = net::udp::PacketBuffer::new(
        vec![net::udp::PacketMetadata::EMPTY; 4],
        vec![0u8; 512],
    );
    let udp_socket = net::udp::Socket::new(rx_buf, tx_buf);
    let socket = iface.clone().add_socket(udp_socket);

    // Bind sur un port éphémère
    let local_port = net::get_ephemeral_port();
    socket.lock().bind(local_port)
        .map_err(|_| "dns: failed to bind UDP socket")?;

    // Envoyer la requête
    let endpoint = net::IpEndpoint::new(
        net::wire::IpAddress::Ipv4(*dns_server),
        DNS_PORT,
    );
    socket.lock().send_slice(&query, endpoint)
        .map_err(|_| "dns: failed to send query")?;

    // Attendre la réponse
    let mut response_buf = [0u8; 512];
    for _ in 0..RESOLVE_TIMEOUT {
        iface.poll();

        if socket.lock().can_recv() {
            let (size, _src) = socket.lock().recv_slice(&mut response_buf)
                .map_err(|_| "dns: recv error")?;

            let result = parse_dns_response(&response_buf[..size]);
            if let Some(addr) = result {
                // Mettre en cache
                DNS_CACHE.lock().insert(String::from(hostname), addr);
                debug!("dns: {} → {}", hostname, addr);
                return Ok(addr);
            }
        }

        core::hint::spin_loop();
    }

    warn!("dns: timeout resolving {}", hostname);
    Err("dns: timeout")
}

/// Vide le cache DNS.
pub fn clear_cache() {
    DNS_CACHE.lock().clear();
}

// =============================================================================
// Construction de requête DNS
// =============================================================================

/// Construit une requête DNS A pour le hostname donné.
///
/// Format simplifié :
///   Header (12 octets) + Question section
fn build_dns_query(hostname: &str, id: u16) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);

    // Header
    buf.extend_from_slice(&id.to_be_bytes());     // ID
    buf.extend_from_slice(&[0x01, 0x00]);          // Flags: RD=1 (recursion desired)
    buf.extend_from_slice(&[0x00, 0x01]);          // QDCOUNT = 1
    buf.extend_from_slice(&[0x00, 0x00]);          // ANCOUNT = 0
    buf.extend_from_slice(&[0x00, 0x00]);          // NSCOUNT = 0
    buf.extend_from_slice(&[0x00, 0x00]);          // ARCOUNT = 0

    // Question: encode le nom en labels
    for label in hostname.split('.') {
        let len = label.len().min(63) as u8;
        buf.push(len);
        buf.extend_from_slice(&label.as_bytes()[..len as usize]);
    }
    buf.push(0); // Terminator

    // Type A (1), Class IN (1)
    buf.extend_from_slice(&[0x00, 0x01]); // QTYPE = A
    buf.extend_from_slice(&[0x00, 0x01]); // QCLASS = IN

    buf
}

// =============================================================================
// Parsing de réponse DNS
// =============================================================================

/// Parse une réponse DNS et extrait la première adresse A (IPv4).
fn parse_dns_response(data: &[u8]) -> Option<net::wire::Ipv4Address> {
    if data.len() < 12 {
        return None;
    }

    // Vérifier que c'est une réponse (bit QR=1)
    if data[2] & 0x80 == 0 {
        return None;
    }

    // Vérifier RCODE = 0 (No error)
    if data[3] & 0x0F != 0 {
        return None;
    }

    let ancount = u16::from_be_bytes([data[6], data[7]]) as usize;
    if ancount == 0 {
        return None;
    }

    // Sauter la section question
    let mut pos = 12;
    pos = skip_dns_name(data, pos)?;
    pos += 4; // QTYPE + QCLASS

    // Parser les réponses
    for _ in 0..ancount {
        if pos >= data.len() { break; }

        // Nom (peut être un pointeur de compression)
        pos = skip_dns_name(data, pos)?;

        if pos + 10 > data.len() { break; }

        let rtype = u16::from_be_bytes([data[pos], data[pos + 1]]);
        let _rclass = u16::from_be_bytes([data[pos + 2], data[pos + 3]]);
        // let _ttl = u32::from_be_bytes([data[pos+4], data[pos+5], data[pos+6], data[pos+7]]);
        let rdlength = u16::from_be_bytes([data[pos + 8], data[pos + 9]]) as usize;
        pos += 10;

        if rtype == 1 && rdlength == 4 && pos + 4 <= data.len() {
            // Type A — IPv4
            return Some(net::wire::Ipv4Address::new(
                data[pos], data[pos + 1], data[pos + 2], data[pos + 3],
            ));
        }

        pos += rdlength;
    }

    None
}

/// Saute un nom DNS (avec support des pointeurs de compression).
/// Retourne la position après le nom, ou None si le format est invalide.
fn skip_dns_name(data: &[u8], mut pos: usize) -> Option<usize> {
    let mut jumped = false;
    let mut return_pos = 0;

    loop {
        if pos >= data.len() { return None; }

        let len = data[pos] as usize;

        if len == 0 {
            // Fin du nom
            if jumped {
                return Some(return_pos);
            } else {
                return Some(pos + 1);
            }
        }

        if len & 0xC0 == 0xC0 {
            // Pointeur de compression
            if pos + 1 >= data.len() { return None; }
            if !jumped {
                return_pos = pos + 2;
                jumped = true;
            }
            let offset = ((len & 0x3F) << 8) | (data[pos + 1] as usize);
            pos = offset;
            continue;
        }

        // Label normal
        pos += 1 + len;
    }
}
