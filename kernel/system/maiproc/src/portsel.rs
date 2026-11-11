//! Choix des ports éphémères (`connect` sans `bind`, `bind` au port 0).
//!
//! Algorithme 4 de la RFC 6056 (« double hachage », celui de Linux) avec les corrections de
//! Klein et Pinkas (*Device Tracking via Linux's New TCP Source Port Selection Algorithm*,
//! USENIX Security 2022), reprises par Linux 5.18 :
//!
//! - le port de départ est `F(adresse locale, adresse distante, port distant)` avec une clé
//!   secrète : deux destinations différentes ne révèlent rien l'une de l'autre, et un
//!   observateur ne peut pas prédire le port d'une connexion ;
//! - la perturbation `table[G(...)]`, qui évite de réutiliser tout de suite le même port vers la
//!   même destination, vient d'une table **grande** (2^14 entrées, contre 256 avant l'article :
//!   les collisions permettaient de suivre un appareil d'un réseau à l'autre) ;
//! - après usage, l'entrée avance d'un pas **aléatoire** (et non `+ 2`, que l'attaque mesurait).
//!
//! Les fonctions F et G sont SipHash-2-4 avec deux clés tirées au démarrage.

use alloc::{vec, vec::Vec};
use sync_preemption::PreemptionSafeMutex as Mutex;

/// `net.ipv4.ip_local_port_range` par défaut.
pub const LOW: u16 = 32768;
pub const HIGH: u16 = 60999;
const TABLE_BITS: u32 = 14;

struct Keys {
    f: (u64, u64),
    g: (u64, u64),
    table: Vec<u32>,
}

static KEYS: Mutex<Option<Keys>> = Mutex::new(None);

fn with_keys<R>(f: impl FnOnce(&mut Keys) -> R) -> R {
    let mut k = KEYS.lock();
    let k = k.get_or_insert_with(|| {
        let r = uvm::rand::random_u64;
        Keys { f: (r(), r()), g: (r(), r()), table: vec![0; 1 << TABLE_BITS] }
    });
    f(k)
}

#[inline]
fn sipround(v: &mut [u64; 4]) {
    v[0] = v[0].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(13) ^ v[0];
    v[0] = v[0].rotate_left(32);
    v[2] = v[2].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(16) ^ v[2];
    v[0] = v[0].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(21) ^ v[0];
    v[2] = v[2].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(17) ^ v[2];
    v[2] = v[2].rotate_left(32);
}

/// SipHash-2-4 (Aumasson et Bernstein, 2012).
pub fn siphash(key: (u64, u64), data: &[u8]) -> u64 {
    let mut v = [key.0 ^ 0x736f6d6570736575, key.1 ^ 0x646f72616e646f6d, key.0 ^ 0x6c7967656e657261, key.1 ^ 0x7465646279746573];
    let mut chunks = data.chunks_exact(8);
    for c in &mut chunks {
        let m = u64::from_le_bytes(c.try_into().unwrap());
        v[3] ^= m;
        sipround(&mut v);
        sipround(&mut v);
        v[0] ^= m;
    }
    let rest = chunks.remainder();
    let mut last = [0u8; 8];
    last[..rest.len()].copy_from_slice(rest);
    let m = u64::from_le_bytes(last) | ((data.len() as u64 & 0xff) << 56);
    v[3] ^= m;
    sipround(&mut v);
    sipround(&mut v);
    v[0] ^= m;
    v[2] ^= 0xff;
    for _ in 0..4 {
        sipround(&mut v);
    }
    v[0] ^ v[1] ^ v[2] ^ v[3]
}

/// Hachage d'un flux avec une clé du noyau (répartition `SO_REUSEPORT`).
pub fn flow_hash(data: &[u8]) -> u64 {
    let k = with_keys(|k| k.g);
    siphash((k.1, k.0), data)
}

/// Port pour `connect` vers `(raddr, rport)` depuis `laddr` ; `free(p)` dit si `p` est libre.
pub fn connect_port(laddr: &[u8; 16], raddr: &[u8; 16], rport: u16, mut free: impl FnMut(u16) -> bool) -> Option<u16> {
    let mut d = [0u8; 34];
    d[..16].copy_from_slice(laddr);
    d[16..32].copy_from_slice(raddr);
    d[32..34].copy_from_slice(&rport.to_be_bytes());
    let (offset, index, perturb) = with_keys(|k| {
        let index = (siphash(k.g, &d) & ((1 << TABLE_BITS) - 1)) as usize;
        (siphash(k.f, &d) as u32, index, k.table[index])
    });
    let range = (HIGH - LOW) as u32 + 1;
    let base = offset.wrapping_add(perturb);
    for i in 0..range {
        let port = LOW + ((base.wrapping_add(i)) % range) as u16;
        if free(port) {
            let step = 1 + i + (uvm::rand::random_u64() & 7) as u32;
            with_keys(|k| k.table[index] = k.table[index].wrapping_add(step));
            return Some(port);
        }
    }
    None
}

/// Port pour `bind(…, port 0)` : départ aléatoire.
pub fn bind_port(mut free: impl FnMut(u16) -> bool) -> Option<u16> {
    let range = (HIGH - LOW) as u32 + 1;
    let start = uvm::rand::random_u64() as u32;
    (0..range).map(|i| LOW + (start.wrapping_add(i) % range) as u16).find(|&p| free(p))
}
