//! MaiGPU G0 : banc du pilote virtio-gpu, piloté par le harnais (`--tests gpu`).
//!
//! 1. Informations : emplacement PCI, fonctions offertes et retenues, scanouts, mode préféré,
//!    EDID, MSI-X, EVENT_IDX ; la sortie doit être branchée sur MaiCompositor.
//! 2. **Motif** : une fenêtre sans décoration de 256 × 192 avec des aplats et un dégradé où
//!    chaque colonne et chaque ligne ont leur couleur ; le harnais capture l'écran (ce que
//!    QEMU affiche, donc ce que virtio-gpu a reçu) et vérifie des points au pixel près
//!    (« gputest : pret capture motif … »), puis appuie sur Espace.
//! 3. **Partiel** : un carré de 24 × 24 change ; seuls ses octets (plus l'horloge du bureau)
//!    doivent partir vers l'appareil ; nouvelle capture vérifiée.
//! 4. **Curseur** : le harnais amène le pointeur dans la fenêtre, Espace, puis le bouge ;
//!    avec le curseur matériel, des `MOVE_CURSOR` partent et presque aucun pixel.
//! 5. **Coût** : images plein écran puis petites zones ; octets par image, latence de la
//!    soumission à la fin (fence), temps de composition.
//!
//! Sorties dans le journal noyau (`warn!`) et au terminal.

#![no_std]

extern crate alloc;
#[macro_use]
extern crate app_io;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::time::Duration;

use event_types::Event;
use framebuffer::AlphaPixel;
use keycodes_ascii::{KeyAction, Keycode};
use shapes::{Coord, Rectangle};
use time::Monotonic;

macro_rules! out {
    ($($t:tt)*) => {{
        let s = format!($($t)*);
        log::warn!("{}", s);
        println!("{}", s);
    }};
}

const W: usize = 256;
const H: usize = 192;
/// Coin de la fenêtre de test (sans décoration : contenu = fenêtre).
const WX: usize = 100;
const WY: usize = 100;
const ANCHOR: (u8, u8, u8) = (0x13, 0x57, 0x9B);
const PATCH: (u8, u8, u8) = (0xFE, 0xDC, 0xBA);
const PATCH_RECT: (usize, usize, usize) = (100, 40, 24);

struct Verdict {
    ok: u32,
    fail: u32,
}

impl Verdict {
    fn check(&mut self, cond: bool, what: &str) {
        if cond {
            self.ok += 1;
            out!("gputest : [ok] {}", what);
        } else {
            self.fail += 1;
            out!("gputest : [ECHEC] {}", what);
        }
    }
}

fn pattern(x: usize, y: usize) -> (u8, u8, u8) {
    if x < 8 && y < 8 {
        ANCHOR
    } else if y < 96 {
        if x < 128 { (0xE0, 0x30, 0x30) } else { (0x30, 0xC0, 0x40) }
    } else if y < 160 {
        if x < 128 { (0x20, 0x40, 0xE0) } else { (0xF0, 0xF0, 0x20) }
    } else {
        (x as u8, ((y - 160) * 8) as u8, 0x80)
    }
}

fn point(x: usize, y: usize, c: (u8, u8, u8)) -> String {
    format!("{},{},{:02x}{:02x}{:02x}", x, y, c.0, c.1, c.2)
}

fn now() -> time::Instant {
    time::now::<Monotonic>()
}

/// Attend que tout ce qui a été abîmé jusqu'ici soit présenté (borné à 2 s).
fn wait_presented() {
    let g = window_manager::frame_gen();
    let start = now();
    while window_manager::last_presentation().gen < g {
        if now().duration_since(start) > Duration::from_secs(2) {
            out!("gputest : image non presentee apres 2 s");
            return;
        }
        let _ = sleep::sleep(Duration::from_millis(1));
    }
}

/// Attend Espace dans la fenêtre (borné à 120 s).
fn wait_space(win: &mut window::Window) -> bool {
    let start = now();
    loop {
        while let Ok(Some(ev)) = win.handle_event() {
            if let Event::KeyboardEvent(k) = ev {
                if k.key_event.keycode == Keycode::Space && k.key_event.action == KeyAction::Pressed {
                    return true;
                }
            }
        }
        if now().duration_since(start) > Duration::from_secs(120) {
            out!("gputest : Espace attendu en vain");
            return false;
        }
        let _ = sleep::sleep(Duration::from_millis(5));
    }
}

fn delta(a: u64, b: u64) -> u64 {
    b.saturating_sub(a)
}

pub fn main(_args: Vec<String>) -> isize {
    out!("gputest : debut");
    let mut v = Verdict { ok: 0, fail: 0 };
    let Some(gpu) = virtio_gpu::get() else {
        v.check(false, "GPU virtio present (QEMU : -vga none -device virtio-vga)");
        out!("gputest : FIN {} ok, {} echec", v.ok, v.fail);
        return 1;
    };

    // ── 1. Informations ────────────────────────────────────────────────────────────────
    let info = gpu.info();
    let mode = info.modes.first().map(|m| (m.rect.width, m.rect.height)).unwrap_or((0, 0));
    out!(
        "gputest : info pci={} scanouts={} capsets={} mode={}x{} fonctions_offertes={:#x} retenues={:#x} msix={} vecteurs={:?} event_idx={} files={}+{} shm={}",
        info.location,
        info.num_scanouts,
        info.num_capsets,
        mode.0,
        mode.1,
        info.features_offered,
        info.features,
        info.msix,
        info.idt_vectors,
        info.event_idx,
        info.ctrl_queue,
        info.cursor_queue,
        info.shm_regions
    );
    if let Some(e) = info.edid.as_ref() {
        out!(
            "gputest : edid fabricant={} produit={:#06x} version={}.{} nom=\"{}\" prefere={:?}",
            e.manufacturer_str(),
            e.product,
            e.version.0,
            e.version.1,
            e.name,
            e.preferred
        );
    }
    let (sw, sh) = window_manager::screen_size();
    v.check(info.display == Some((sw as u32, sh as u32)), &format!("scanout {:?} a la resolution de l'ecran {}x{}", info.display, sw, sh));
    v.check(window_manager::has_gpu_output(), "sortie virtio-gpu branchee sur MaiCompositor");
    v.check(info.msix, "interruptions MSI-X");
    v.check(info.event_idx, "VIRTIO_F_EVENT_IDX negocie");
    v.check(info.edid.is_some(), "EDID lu");

    // ── 2. Motif ───────────────────────────────────────────────────────────────────────
    let mut win = match window::Window::with_options(String::from("gpu_test"), Coord::new(WX as isize, WY as isize), W, H, color::BLACK, false) {
        Ok(w) => w,
        Err(e) => {
            v.check(false, &format!("fenetre de test : {}", e));
            out!("gputest : FIN {} ok, {} echec", v.ok, v.fail);
            return 1;
        }
    };
    {
        let mut fb = win.framebuffer_mut();
        let buf = fb.buffer_mut();
        for y in 0..H {
            for x in 0..W {
                let (r, g, b) = pattern(x, y);
                buf[y * W + x] = AlphaPixel { red: r, green: g, blue: b, alpha: 0 };
            }
        }
    }
    let _ = win.render(None);
    wait_presented();
    let pts: Vec<(usize, usize)> = alloc::vec![
        (0, 0), (7, 7), (8, 0), (127, 50), (128, 50), (255, 95), (0, 96), (127, 159), (128, 96),
        (255, 159), (0, 160), (37, 163), (200, 191), (255, 170), (1, 191), (254, 160),
    ];
    let list: Vec<String> = pts.iter().map(|&(x, y)| point(x, y, pattern(x, y))).collect();
    out!("gputest : pret capture motif {} {} {}", W, H, list.join(" "));
    wait_space(&mut win);

    // ── 3. Mise à jour partielle ───────────────────────────────────────────────────────
    let (px, py, side) = PATCH_RECT;
    let s0 = gpu.stats();
    {
        let mut fb = win.framebuffer_mut();
        let buf = fb.buffer_mut();
        for y in py..py + side {
            for x in px..px + side {
                buf[y * W + x] = AlphaPixel { red: PATCH.0, green: PATCH.1, blue: PATCH.2, alpha: 0 };
            }
        }
    }
    let area = Rectangle {
        top_left: Coord::new(px as isize, py as isize),
        bottom_right: Coord::new((px + side) as isize, (py + side) as isize),
    };
    let _ = win.render(Some(area));
    wait_presented();
    let _ = sleep::sleep(Duration::from_millis(50));
    let s1 = gpu.stats();
    let bytes = delta(s0.transfer_bytes, s1.transfer_bytes);
    let full = (sw * sh * 4) as u64;
    out!(
        "gputest : partiel : {} octets transferes en {} presentation(s), {} rectangle(s) (carre : {} octets, ecran : {} octets)",
        bytes,
        delta(s0.presents, s1.presents),
        delta(s0.present_rects, s1.present_rects),
        side * side * 4,
        full
    );
    v.check(bytes >= (side * side * 4) as u64 && bytes < full / 20, "mise a jour partielle : seuls les octets abimes partent");
    let red = pattern(px - 1, py);
    let partial = [
        point(0, 0, ANCHOR),
        point(px, py, PATCH),
        point(px + side - 1, py + side - 1, PATCH),
        point(px + side / 2, py + side / 2, PATCH),
        point(px - 1, py, red),
        point(px + side, py, red),
        point(px, py - 1, red),
        point(px, py + side, red),
        point(200, 191, pattern(200, 191)),
    ];
    out!("gputest : pret capture partiel {} {} {}", W, H, partial.join(" "));
    wait_space(&mut win);

    // ── 4. Curseur matériel ────────────────────────────────────────────────────────────
    out!("gputest : pret curseur");
    wait_space(&mut win);
    // La ligne imprimée au terminal est présentée avant la référence : seule la souris compte.
    out!("gputest : curseur mesure");
    // Le terminal affiche la ligne de façon asynchrone : lui laisser le temps (le harnais
    // attend 1 s avant de bouger la souris).
    let _ = sleep::sleep(Duration::from_millis(400));
    wait_presented();
    let c0 = gpu.stats();
    let wm0 = window_manager::stats();
    let t0 = now();
    wait_space(&mut win);
    let moving = now().duration_since(t0);
    let c1 = gpu.stats();
    let wm1 = window_manager::stats();
    let moves = delta(c0.cursor_moves, c1.cursor_moves);
    let cbytes = delta(c0.transfer_bytes, c1.transfer_bytes);
    let sw_draws = delta(wm0.sw_cursor_draws, wm1.sw_cursor_draws);
    // Même durée sans bouger : ce qui part de toute façon (horloge du bureau…).
    let q0 = gpu.stats();
    let _ = sleep::sleep(moving);
    let q1 = gpu.stats();
    out!(
        "gputest : curseur : {} MOVE_CURSOR, {} UPDATE_CURSOR, {} octets transferes en {} ms ({} octets au repos sur la meme duree), {} mises a jour du plan du curseur, {} traces logiciels",
        moves,
        delta(c0.cursor_updates, c1.cursor_updates),
        cbytes,
        moving.as_millis(),
        delta(q0.transfer_bytes, q1.transfer_bytes),
        delta(wm0.cursor_updates, wm1.cursor_updates),
        sw_draws
    );
    v.check(moves > 0, "le curseur materiel suit la souris (MOVE_CURSOR)");
    v.check(sw_draws == 0, "aucun trace du curseur logiciel : bouger la souris n'ecrit aucun pixel");

    // ── 4 bis. Pointeur absolu (virtio-tablet) ─────────────────────────────────────────
    // Le harnais place le pointeur au centre de la fenêtre par une position absolue (QMP
    // `input-send-event`), clique, puis Espace : position exacte, clic reçu par la fenêtre,
    // curseur matériel déplacé.
    let tablet = virtio_input::devices().into_iter().find_map(|d| match d.info().kind {
        virtio_input::Kind::Tablet { x, y } => Some((d, x, y)),
        _ => None,
    });
    match tablet {
        None => out!("gputest : tablette absente (QEMU : -device virtio-tablet-pci)"),
        Some((dev, rx, ry)) => {
            let (cx, cy) = (WX + W / 2, WY + H / 2);
            // Inverse de `MousePositionAbsolute::to_screen` (bornes comprises).
            let to_abs = |p: usize, n: usize, r: (i32, i32)| -> i64 {
                r.0 as i64 + (p as i64 * (r.1 - r.0) as i64 + (n as i64 - 1) / 2) / (n as i64 - 1).max(1)
            };
            let (ax, ay) = (to_abs(cx, sw, rx), to_abs(cy, sh, ry));
            let s0 = dev.stats();
            let g0 = gpu.stats();
            out!("gputest : pret tablette {} {} ({} : {:?} {:?})", ax, ay, dev.info().name, rx, ry);
            let start = now();
            let mut clicked = None;
            let mut got_space = false;
            while now().duration_since(start) < Duration::from_secs(120) {
                while let Ok(Some(ev)) = win.handle_event() {
                    match ev {
                        Event::MousePositionEvent(m) if m.left_button_hold && clicked.is_none() => {
                            clicked = Some((m.gcoordinate.x, m.gcoordinate.y));
                        }
                        Event::KeyboardEvent(k) if k.key_event.keycode == Keycode::Space && k.key_event.action == KeyAction::Pressed => {
                            got_space = true;
                        }
                        _ => {}
                    }
                }
                if got_space {
                    break;
                }
                let _ = sleep::sleep(Duration::from_millis(5));
            }
            let pos = window_manager::pointer_position();
            let s1 = dev.stats();
            let g1 = gpu.stats();
            out!(
                "gputest : tablette : pointeur en {},{} (attendu {},{}), clic {:?}, {} evenements, {} paquets, {} jetes, {} interruptions, {} MOVE_CURSOR",
                pos.0,
                pos.1,
                cx,
                cy,
                clicked,
                delta(s0.events, s1.events),
                delta(s0.reports, s1.reports),
                delta(s0.dropped, s1.dropped),
                delta(s0.interrupts, s1.interrupts),
                delta(g0.cursor_moves, g1.cursor_moves)
            );
            v.check(pos == (cx as isize, cy as isize), "pointeur absolu : position exacte au pixel");
            v.check(clicked == Some((cx as isize, cy as isize)), "pointeur absolu : clic recu par la fenetre, au bon point");
            v.check(delta(g0.cursor_moves, g1.cursor_moves) > 0, "pointeur absolu : le curseur materiel suit");
        }
    }

    // ── 5. Coût de la présentation ─────────────────────────────────────────────────────
    let bench = |name: &str, frames: u32, rect: Option<(usize, usize, usize, usize)>| {
        let g0 = gpu.stats();
        let w0 = window_manager::stats();
        let t0 = now();
        for i in 0..frames {
            match rect {
                None => window_manager::damage_all(),
                Some((x, y, w, h)) => {
                    let dx = (i as usize * 37) % (sw - w).max(1);
                    window_manager::damage(Rectangle {
                        top_left: Coord::new((x + dx) as isize, y as isize),
                        bottom_right: Coord::new((x + dx + w) as isize, (y + h) as isize),
                    })
                }
            }
            wait_presented();
        }
        let dt = now().duration_since(t0);
        let g1 = gpu.stats();
        let w1 = window_manager::stats();
        let presents = delta(g0.presents, g1.presents).max(1);
        let lat_n = delta(g0.present_lat_count, g1.present_lat_count).max(1);
        let frames_c = delta(w0.frames, w1.frames).max(1);
        let busy = delta(g0.busy_ns, g1.busy_ns);
        let busy_pm = busy as u128 * 1000 / dt.as_nanos().max(1);
        out!(
            "gputest : cout {} : {} images en {} ms, appareil occupe {} pm, {} Kio/presentation, presentation moy {} us (max global {} us), attente avant ecriture {} us/image, composition moy {} us, notifications {} (evitees {}), interruptions {}",
            name,
            frames,
            dt.as_millis(),
            busy_pm,
            delta(g0.transfer_bytes, g1.transfer_bytes) / presents / 1024,
            delta(g0.present_lat_total_ns, g1.present_lat_total_ns) / lat_n / 1000,
            g1.present_lat_max_ns / 1000,
            delta(g0.present_wait_ns, g1.present_wait_ns) / presents / 1000,
            delta(w0.compose_ns, w1.compose_ns) / frames_c / 1000,
            delta(g0.notifies, g1.notifies),
            delta(g0.notifies_suppressed, g1.notifies_suppressed),
            delta(g0.interrupts, g1.interrupts)
        );
        (busy, dt.as_nanos() as u64, delta(g0.present_lat_total_ns, g1.present_lat_total_ns))
    };
    let (busy, dt, lat) = bench("plein_ecran", 60, None);
    // Temps actif : contient chaque présentation (soumission → fence) et tient dans la durée.
    v.check(busy > 0 && busy <= dt && busy + busy / 10 >= lat, "temps actif de l'appareil coherent (latences comprises, <= duree)");
    bench("petite_zone", 120, Some((0, 0, 64, 64)));

    // Rapport générique lu par le gestionnaire des tâches (catégorie GPU).
    match window_manager::gpu_report() {
        Some(r) => {
            out!(
                "gputest : rapport : {} | {} | {} | {} | {}x{} | memoire {} Kio | actif {} ms | presentations {}",
                r.adapter,
                r.driver,
                r.location,
                r.features,
                r.width,
                r.height,
                r.memory_bytes / 1024,
                r.busy_ns / 1_000_000,
                r.presents
            );
            let expect = sw as u64 * sh as u64 * 4 + if r.hw_cursor { 64 * 64 * 4 } else { 0 };
            v.check(
                r.adapter.starts_with("virtio-gpu") && (r.width, r.height) == (sw as u32, sh as u32) && r.memory_bytes == expect,
                "rapport du GPU pour le gestionnaire des taches (carte, mode, memoire)",
            );
        }
        None => v.check(false, "rapport du GPU pour le gestionnaire des taches"),
    }

    let s = gpu.stats();
    out!(
        "gputest : totaux commandes={} erreurs={} fins={} latence_moy={} us max={} us attentes_dormies={} sondees={}",
        s.commands,
        s.errors,
        s.completions,
        s.lat_avg_ns / 1000,
        s.lat_max_ns / 1000,
        s.waits_slept,
        s.waits_polled
    );
    v.check(s.errors == 0, "aucune erreur de l'appareil");
    drop(win);
    out!("gputest : FIN {} ok, {} echec", v.ok, v.fail);
    if v.fail == 0 { 0 } else { 1 }
}
