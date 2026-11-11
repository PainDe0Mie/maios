//! Tests hôte : tables générées, aller-retour, refus, zombies, émission, fuzzeur à états.

use alloc::vec;
use alloc::vec::Vec;

use crate::conn::{InBuf, Limits, OutBuf, SendError};
use crate::objmap::{IdError, ObjectMap};
use crate::protocols::*;
use crate::types::*;
use crate::wire::{self, Arg, WireError};

type Fd = u32;

fn enc(object: ObjectId, iface: &Interface, opcode: u16, args: Vec<Arg<'_, Fd>>, fds: &mut Vec<Fd>) -> Vec<u8> {
    let mut out = Vec::new();
    wire::encode(&mut out, object, opcode, &iface.requests[opcode as usize], args, &mut |f| fds.push(f)).unwrap();
    out
}

/// Table : display(1), registry(2), compositor(3), surface(4), shm(5).
fn base_map() -> ObjectMap<()> {
    let mut m = ObjectMap::new(1 << 16, ());
    m.insert_client(2, &wl_registry::INTERFACE, 1, ()).unwrap();
    m.insert_client(3, &wl_compositor::INTERFACE, 6, ()).unwrap();
    m.insert_client(4, &wl_surface::INTERFACE, 6, ()).unwrap();
    m.insert_client(5, &wl_shm::INTERFACE, 2, ()).unwrap();
    m
}

fn feed(bytes: &[u8], fds: &[Fd]) -> InBuf<Fd> {
    let mut b = InBuf::<Fd>::new(&Limits::default());
    b.push_bytes(bytes).unwrap();
    for f in fds {
        b.push_fd(*f).unwrap();
    }
    b
}

#[test]
fn generated_tables() {
    assert_eq!(wl_display::INTERFACE.requests[wl_display::req::GET_REGISTRY as usize].name, "get_registry");
    assert_eq!(wl_surface::VERSION, 6);
    assert_eq!(wl_surface::INTERFACE.requests[wl_surface::req::OFFSET as usize].since, 5);
    assert!(wl_surface::INTERFACE.requests[wl_surface::req::DESTROY as usize].destructor);
    assert_eq!(wl_output::transform::_90, 1);
    assert_eq!(wl_display::error::NO_MEMORY, 2);
    assert_eq!(xdg_toplevel::state::ACTIVATED, 4);
    let attach = &wl_surface::INTERFACE.requests[wl_surface::req::ATTACH as usize];
    assert!(attach.args[0].nullable);
    assert!(attach.args[0].interface.unwrap().is(&wl_buffer::INTERFACE));
    assert!(find(b"xdg_wm_base").is_some());
    assert!(find(b"wp_fifo_v1").is_some());
    assert!(find(b"nope").is_none());
    // Interface référencée mais absente des XML embarqués (tablettes).
    let cs = &wp_cursor_shape_manager_v1::INTERFACE.requests[wp_cursor_shape_manager_v1::req::GET_TABLET_TOOL_V2 as usize];
    assert_eq!(cs.args[1].interface_name, Some("zwp_tablet_tool_v2"));
    assert!(cs.args[1].interface.is_none());
}

#[test]
fn roundtrip_attach_and_title() {
    let mut m = base_map();
    m.insert_client(6, &wl_buffer::INTERFACE, 1, ()).unwrap();
    let mut fds = vec![];
    let mut bytes = enc(4, &wl_surface::INTERFACE, wl_surface::req::ATTACH, vec![Arg::Object(6), Arg::Int(-3), Arg::Int(7)], &mut fds);
    bytes.extend(enc(4, &wl_surface::INTERFACE, wl_surface::req::COMMIT, vec![], &mut fds));
    let mut b = feed(&bytes, &fds);
    let r = b.next(&m).unwrap().unwrap();
    assert_eq!((r.object, r.opcode), (4, wl_surface::req::ATTACH));
    assert_eq!((r.args.object(0), r.args.int(1), r.args.int(2)), (6, -3, 7));
    let r = b.next(&m).unwrap().unwrap();
    assert_eq!(r.opcode, wl_surface::req::COMMIT);
    assert!(b.next(&m).unwrap().is_none());
    assert_eq!(b.pending(), 0);
}

#[test]
fn strings_are_strict() {
    let m = base_map();
    m.get(4).unwrap();
    let mut map = base_map();
    map.insert_client(6, &xdg_wm_base::INTERFACE, 6, ()).unwrap();
    map.insert_client(7, &xdg_surface::INTERFACE, 6, ()).unwrap();
    map.insert_client(8, &xdg_toplevel::INTERFACE, 6, ()).unwrap();
    let ok = enc(8, &xdg_toplevel::INTERFACE, xdg_toplevel::req::SET_TITLE, vec![Arg::Str(Some(b"Mai"))], &mut vec![]);
    assert_eq!(feed(&ok, &[]).next(&map).unwrap().unwrap().args.str(0), Some(&b"Mai"[..]));
    // `NUL` intérieur, `NUL` final manquant, chaîne obligatoire absente.
    for body in [&b"\x04\0\0\0M\0i\0"[..], &b"\x03\0\0\0Mai\0"[..], &b"\0\0\0\0"[..]] {
        let mut msg = vec![];
        msg.extend_from_slice(&8u32.to_ne_bytes());
        msg.extend_from_slice(&(((8 + body.len()) as u32) << 16 | 2).to_ne_bytes());
        msg.extend_from_slice(body);
        let e = feed(&msg, &[]).next(&map).unwrap_err();
        assert_eq!(e.code, wl_display::error::INVALID_METHOD, "{e:?}");
    }
    // Longueur immense : jamais de débordement (comparée sur 64 bits).
    let mut msg = vec![];
    msg.extend_from_slice(&8u32.to_ne_bytes());
    msg.extend_from_slice(&(12u32 << 16 | 2).to_ne_bytes());
    msg.extend_from_slice(&u32::MAX.to_ne_bytes());
    assert!(feed(&msg, &[]).next(&map).is_err());
}

#[test]
fn object_checks() {
    let m = base_map();
    // Un wl_surface (4) passé comme wl_buffer.
    let bytes = enc(4, &wl_surface::INTERFACE, wl_surface::req::ATTACH, vec![Arg::Object(4), Arg::Int(0), Arg::Int(0)], &mut vec![]);
    assert_eq!(feed(&bytes, &[]).next(&m).unwrap_err().code, wl_display::error::INVALID_OBJECT);
    // Objet inconnu.
    let bytes = enc(4, &wl_surface::INTERFACE, wl_surface::req::ATTACH, vec![Arg::Object(77), Arg::Int(0), Arg::Int(0)], &mut vec![]);
    assert_eq!(feed(&bytes, &[]).next(&m).unwrap_err().code, wl_display::error::INVALID_OBJECT);
    // Cible inconnue.
    let bytes = enc(99, &wl_surface::INTERFACE, wl_surface::req::COMMIT, vec![], &mut vec![]);
    assert_eq!(feed(&bytes, &[]).next(&m).unwrap_err().object, DISPLAY_ID);
    // Opcode inconnu, version trop basse (offset depuis la v5 sur un objet v4).
    let mut m4 = base_map();
    m4.insert_client(6, &wl_surface::INTERFACE, 4, ()).unwrap();
    let bytes = enc(6, &wl_surface::INTERFACE, wl_surface::req::OFFSET, vec![Arg::Int(1), Arg::Int(1)], &mut vec![]);
    let e = feed(&bytes, &[]).next(&m4).unwrap_err();
    assert_eq!((e.object, e.code), (6, wl_display::error::INVALID_METHOD));
    let mut bad = enc(4, &wl_surface::INTERFACE, wl_surface::req::COMMIT, vec![], &mut vec![]);
    bad[4] = 200;
    assert_eq!(feed(&bad, &[]).next(&m).unwrap_err().code, wl_display::error::INVALID_METHOD);
}

#[test]
fn new_ids_are_dense() {
    let m = base_map();
    // 6 est la place suivante, 9 laisserait un trou, 4 est pris, l'espace serveur est interdit.
    for (id, ok) in [(6, true), (9, false), (4, false), (SERVER_ID_START, false)] {
        let bytes = enc(3, &wl_compositor::INTERFACE, wl_compositor::req::CREATE_SURFACE, vec![Arg::NewId(id)], &mut vec![]);
        assert_eq!(feed(&bytes, &[]).next(&m).is_ok(), ok, "id {id}");
    }
    let mut m = base_map();
    assert_eq!(m.insert_client(9, &wl_surface::INTERFACE, 1, ()), Err(IdError::OutOfRange));
    assert_eq!(m.remove(4), Some(()));
    assert_eq!(m.remove(4), None);
    assert!(m.insert_client(4, &wl_surface::INTERFACE, 1, ()).is_ok());
    assert_eq!(m.remove(DISPLAY_ID), None);
    let mut small = ObjectMap::new(3, ());
    small.insert_client(2, &wl_registry::INTERFACE, 1, ()).unwrap();
    small.insert_client(3, &wl_registry::INTERFACE, 1, ()).unwrap();
    assert_eq!(small.insert_client(4, &wl_registry::INTERFACE, 1, ()), Err(IdError::TooMany));
    assert_eq!(small.insert_server(&wl_callback::INTERFACE, 1, ()), Err(IdError::TooMany));
}

#[test]
fn registry_bind_and_fds() {
    let m = base_map();
    let bytes = enc(
        2,
        &wl_registry::INTERFACE,
        wl_registry::req::BIND,
        vec![Arg::Uint(7), Arg::NewIdDyn { interface: b"wl_seat", version: 9, id: 6 }],
        &mut vec![],
    );
    let mut b = feed(&bytes, &[]);
    let r = b.next(&m).unwrap().unwrap();
    assert_eq!(r.args.uint(0), 7);
    assert_eq!(r.args.get(1), Some(&Arg::NewIdDyn { interface: &b"wl_seat"[..], version: 9, id: 6 }));
    // wl_shm.create_pool(new_id, fd, size).
    let mut fds = vec![];
    let bytes = enc(5, &wl_shm::INTERFACE, wl_shm::req::CREATE_POOL, vec![Arg::NewId(6), Arg::Fd(42), Arg::Int(4096)], &mut fds);
    assert_eq!(fds, vec![42]);
    let mut b = feed(&bytes, &fds);
    let mut r = b.next(&m).unwrap().unwrap();
    assert_eq!(r.args.take_fd(1), Some(42));
    assert_eq!(r.args.take_fd(1), None);
    assert_eq!(r.args.int(2), 4096);
    // Descripteur manquant.
    let e = feed(&bytes, &[]).next(&m).unwrap_err();
    assert_eq!(e.code, wl_display::error::IMPLEMENTATION);
}

#[test]
fn zombies_consume_fds() {
    let mut m = base_map();
    m.insert_client(6, &wl_shm::INTERFACE, 2, ()).unwrap();
    assert_eq!(m.kill(6), Some(()));
    let mut fds = vec![];
    let mut bytes = enc(6, &wl_shm::INTERFACE, wl_shm::req::CREATE_POOL, vec![Arg::NewId(7), Arg::Fd(1), Arg::Int(64)], &mut fds);
    bytes.extend(enc(5, &wl_shm::INTERFACE, wl_shm::req::CREATE_POOL, vec![Arg::NewId(7), Arg::Fd(2), Arg::Int(64)], &mut fds));
    let mut b = feed(&bytes, &fds);
    let mut r = b.next(&m).unwrap().unwrap();
    // La requête au zombie est sautée et son descripteur (1) consommé : le suivant reçoit 2.
    assert_eq!(r.object, 5);
    assert_eq!(r.args.take_fd(1), Some(2));
    // Un zombie désigné en argument facultatif devient « aucun ».
    m.insert_client(7, &wl_buffer::INTERFACE, 1, ()).unwrap();
    m.kill(7);
    let bytes = enc(4, &wl_surface::INTERFACE, wl_surface::req::ATTACH, vec![Arg::Object(7), Arg::Int(0), Arg::Int(0)], &mut vec![]);
    assert_eq!(feed(&bytes, &[]).next(&m).unwrap().unwrap().args.object(0), 0);
    // Le client peut réutiliser l'identifiant d'un zombie.
    assert!(m.insert_client(6, &wl_surface::INTERFACE, 1, ()).is_ok());
}

#[test]
fn partial_and_bad_headers() {
    let m = base_map();
    let bytes = enc(4, &wl_surface::INTERFACE, wl_surface::req::DAMAGE, vec![Arg::Int(1), Arg::Int(2), Arg::Int(3), Arg::Int(4)], &mut vec![]);
    let mut b = InBuf::<Fd>::new(&Limits::default());
    for chunk in bytes.chunks(3) {
        assert!(b.next(&m).unwrap().is_none());
        b.push_bytes(chunk).unwrap();
    }
    assert_eq!(b.next(&m).unwrap().unwrap().args.int(3), 4);
    // Taille 4 (< en-tête) et 10 (non multiple de 4).
    for size in [4u32, 10] {
        let mut msg = vec![];
        msg.extend_from_slice(&4u32.to_ne_bytes());
        msg.extend_from_slice(&(size << 16).to_ne_bytes());
        msg.extend_from_slice(&[0; 8]);
        assert!(feed(&msg, &[]).next(&m).is_err());
    }
    // Octets en trop dans un message.
    let mut msg = enc(4, &wl_surface::INTERFACE, wl_surface::req::COMMIT, vec![], &mut vec![]);
    msg.extend_from_slice(&[0; 4]);
    msg[4..8].copy_from_slice(&(12u32 << 16 | u32::from(wl_surface::req::COMMIT)).to_ne_bytes());
    assert!(feed(&msg, &[]).next(&m).is_err());
    // Tampon plein.
    let mut small = InBuf::<Fd>::new(&Limits { in_bytes: 16, ..Limits::default() });
    assert!(small.push_bytes(&[0; 17]).is_err());
}

#[test]
fn encoding_is_checked() {
    let mut out = Vec::new();
    let desc = &wl_surface::INTERFACE.requests[wl_surface::req::ATTACH as usize];
    assert_eq!(wire::encode::<Fd>(&mut out, 4, 1, desc, vec![Arg::Int(1)], &mut |_| {}), Err(WireError::Signature));
    assert!(out.is_empty());
    let desc = &xdg_toplevel::INTERFACE.requests[xdg_toplevel::req::SET_TITLE as usize];
    assert_eq!(wire::encode::<Fd>(&mut out, 4, 2, desc, vec![Arg::Str(None)], &mut |_| {}), Err(WireError::NullString));
    let big = vec![b'x'; 70000];
    assert_eq!(wire::encode::<Fd>(&mut out, 4, 2, desc, vec![Arg::Str(Some(&big))], &mut |_| {}), Err(WireError::TooLarge));
    assert!(out.is_empty());
}

#[test]
fn outbuf_chunks_and_fds() {
    let mut o = OutBuf::<Fd>::new(&Limits::default());
    // wl_display.delete_id (12 octets), wl_keyboard.keymap (fd, 20 octets), delete_id.
    o.event(1, &wl_display::INTERFACE, 1, wl_display::evt::DELETE_ID, vec![Arg::Uint(5)]).unwrap();
    o.event(9, &wl_keyboard::INTERFACE, 9, wl_keyboard::evt::KEYMAP, vec![Arg::Uint(1), Arg::Fd(33), Arg::Uint(100)]).unwrap();
    o.event(1, &wl_display::INTERFACE, 1, wl_display::evt::DELETE_ID, vec![Arg::Uint(6)]).unwrap();
    assert_eq!(o.len(), 12 + 16 + 12);
    assert_eq!(o.chunk(20), (&o.buf_for_test()[..12], 0));
    let (c, n) = o.chunk(1000);
    assert_eq!((c.len(), n), (40, 1));
    // Un message plus grand que la limite passe quand même seul.
    assert_eq!(o.chunk(4).0.len(), 12);
    assert_eq!(o.consume(28), vec![33]);
    assert_eq!(o.chunk(1000), (&o.buf_for_test()[..12], 0));
    assert!(o.consume(12).is_empty());
    assert!(o.is_empty());
    // Version : wl_surface.preferred_buffer_scale (v6) vers un objet v5.
    assert_eq!(
        o.event(4, &wl_surface::INTERFACE, 5, wl_surface::evt::PREFERRED_BUFFER_SCALE, vec![Arg::Int(2)]),
        Err(SendError::Version)
    );
    let mut tiny = OutBuf::<Fd>::new(&Limits { out_bytes: 16, ..Limits::default() });
    tiny.event(1, &wl_display::INTERFACE, 1, wl_display::evt::DELETE_ID, vec![Arg::Uint(5)]).unwrap();
    assert_eq!(tiny.event(1, &wl_display::INTERFACE, 1, wl_display::evt::DELETE_ID, vec![Arg::Uint(5)]), Err(SendError::Full));
    assert_eq!(tiny.len(), 12);
}

#[test]
fn outbuf_caps_fds_per_send() {
    use crate::conn::MAX_FDS_PER_SEND;
    let mut o = OutBuf::<Fd>::new(&Limits::default());
    // 30 cartes clavier (un descripteur chacune), séparées par des messages sans descripteur.
    for i in 0..30 {
        o.event(9, &wl_keyboard::INTERFACE, 9, wl_keyboard::evt::KEYMAP, vec![Arg::Uint(1), Arg::Fd(i), Arg::Uint(100)]).unwrap();
        o.event(1, &wl_display::INTERFACE, 1, wl_display::evt::DELETE_ID, vec![Arg::Uint(5)]).unwrap();
    }
    let (c, n) = o.chunk(usize::MAX);
    assert_eq!(n, MAX_FDS_PER_SEND);
    // Coupé juste avant la 29e carte : 28 paires entières.
    assert_eq!(c.len(), 28 * (16 + 12));
    let len = c.len();
    let fds = o.consume(len);
    assert_eq!(fds, (0..28).collect::<Vec<_>>());
    let (c, n) = o.chunk(usize::MAX);
    assert_eq!((c.len(), n), (2 * (16 + 12), 2));
}

impl<F> OutBuf<F> {
    fn buf_for_test(&self) -> Vec<u8> {
        self.raw().to_vec()
    }
}

// ---------------------------------------------------------------------------------------------
// Fuzzeur à états (Paalanen, wayland#8) : un réservoir d'objets vivants, des requêtes valides
// tirées des signatures (qui doivent toutes passer), puis les mêmes octets abîmés (qui ne doivent
// jamais faire paniquer). Générateur déterministe : une graine rejoue tout.
// ---------------------------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Une requête valide au hasard.
/// Requête générée : octets, descripteurs, nouvel objet éventuel, objet détruit éventuel.
type Generated = (Vec<u8>, Vec<Fd>, Option<(u32, &'static Interface)>, Option<u32>);

fn random_request(rng: &mut Rng, map: &ObjectMap<()>, next_id: u32, strbuf: &mut Vec<Vec<u8>>) -> Option<Generated> {
    let live: Vec<(u32, &'static Interface, u32)> = map.iter().map(|(id, o)| (id, o.interface, o.version)).collect();
    let (target, iface, version) = live[rng.below(live.len())];
    let usable: Vec<usize> = (0..iface.requests.len()).filter(|&i| iface.requests[i].since <= version).collect();
    if usable.is_empty() {
        return None;
    }
    let op = usable[rng.below(usable.len())];
    let desc = &iface.requests[op];
    strbuf.clear();
    for _ in desc.args {
        let len = rng.below(40);
        strbuf.push((0..len).map(|_| b'a' + (rng.below(26) as u8)).collect());
    }
    let mut created = None;
    let mut args = Vec::new();
    for (i, a) in desc.args.iter().enumerate() {
        let v = match a.kind {
            ArgKind::Int => Arg::Int(rng.next() as i32),
            ArgKind::Uint => Arg::Uint(rng.next() as u32),
            ArgKind::Fixed => Arg::Fixed(Fixed(rng.next() as i32)),
            ArgKind::String => Arg::Str(if a.nullable && rng.below(4) == 0 { None } else { Some(&strbuf[i][..]) }),
            ArgKind::Array => Arg::Array(&strbuf[i][..]),
            ArgKind::Fd => Arg::Fd(rng.next() as u32),
            ArgKind::Object => {
                let cands: Vec<u32> = live
                    .iter()
                    .filter(|(_, i, _)| a.interface_name.map_or(true, |n| n == i.name))
                    .map(|(id, _, _)| *id)
                    .collect();
                if a.nullable && (cands.is_empty() || rng.below(3) == 0) {
                    Arg::Object(0)
                } else if cands.is_empty() {
                    return None;
                } else {
                    Arg::Object(cands[rng.below(cands.len())])
                }
            }
            ArgKind::NewId => {
                let new_iface = match a.interface {
                    Some(i) => i,
                    None if a.interface_name.is_none() => ALL[rng.below(ALL.len())],
                    None => return None,
                };
                created = Some((next_id, new_iface));
                if a.interface_name.is_none() {
                    Arg::NewIdDyn { interface: new_iface.name.as_bytes(), version: 1, id: next_id }
                } else {
                    Arg::NewId(next_id)
                }
            }
        };
        args.push(v);
    }
    let mut fds = Vec::new();
    let bytes = enc(target, iface, op as u16, args, &mut fds);
    Some((bytes, fds, created, desc.destructor.then_some(target)))
}

#[test]
fn stateful_fuzz() {
    for seed in 1..=40u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut map = ObjectMap::new(4096, ());
        map.insert_client(2, &wl_registry::INTERFACE, 1, ()).unwrap();
        for (i, iface) in ALL.iter().enumerate().take(30) {
            map.insert_client(3 + i as u32, iface, iface.version, ()).unwrap();
        }
        let mut strbuf = Vec::new();
        let mut decoded = 0;
        for _ in 0..3000 {
            let next_id = (1..u32::MAX).find(|&id| map.check_client_id(id).is_ok()).unwrap();
            let Some((bytes, fds, created, destroyed)) = random_request(&mut rng, &map, next_id, &mut strbuf) else {
                continue;
            };
            // 1. Valide : doit passer, et rendre exactement les descripteurs envoyés.
            let mut b = feed(&bytes, &fds);
            {
                let mut r = b.next(&map).unwrap_or_else(|e| panic!("graine {seed} : {e:?}")).unwrap();
                let mut got = Vec::new();
                for i in 0..r.args.len() {
                    if let Some(f) = r.args.take_fd(i) {
                        got.push(f);
                    }
                }
                assert_eq!(got, fds);
            }
            assert!(b.next(&map).unwrap().is_none());
            decoded += 1;
            if let Some((id, iface)) = created {
                map.insert_client(id, iface, iface.version, ()).unwrap();
            }
            if let Some(id) = destroyed {
                if id != DISPLAY_ID && rng.below(2) == 0 {
                    map.remove(id);
                } else {
                    map.kill(id);
                }
            }
            // 2. Abîmé : octets changés, tronqué ou rallongé, descripteurs en moins.
            for _ in 0..8 {
                let mut bad = bytes.clone();
                match rng.below(4) {
                    0 => {
                        let i = rng.below(bad.len());
                        bad[i] ^= 1 << rng.below(8);
                    }
                    1 => {
                        let i = rng.below(bad.len());
                        bad[i] = rng.next() as u8;
                    }
                    2 => bad.truncate(rng.below(bad.len())),
                    _ => bad.extend((0..rng.below(16)).map(|_| rng.next() as u8)),
                }
                let nfds = rng.below(fds.len() + 1);
                let mut b = feed(&bad, &fds[..nfds]);
                for _ in 0..4 {
                    match b.next(&map) {
                        Ok(Some(_)) => {}
                        Ok(None) | Err(_) => break,
                    }
                }
            }
        }
        assert!(decoded > 1000, "graine {seed} : {decoded} requêtes seulement");
    }
}

#[test]
fn random_bytes_never_panic() {
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let map = base_map();
    for _ in 0..20000 {
        let len = rng.below(64);
        let mut bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        // En-tête plausible une fois sur deux, pour aller plus loin que la taille.
        if len >= 8 && rng.below(2) == 0 {
            let obj = 1 + rng.below(5) as u32;
            let size = ((len / 4) * 4) as u32;
            bytes[..4].copy_from_slice(&obj.to_ne_bytes());
            bytes[4..8].copy_from_slice(&(size << 16 | rng.below(12) as u32).to_ne_bytes());
        }
        let mut b = feed(&bytes, &[1, 2]);
        while let Ok(Some(_)) = b.next(&map) {}
    }
}
