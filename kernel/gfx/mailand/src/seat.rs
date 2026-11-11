//! `wl_seat`, `wl_pointer`, `wl_keyboard` : les entrées d'un client.
//!
//! Les événements viennent des fenêtres de MaiCompositor (une fenêtre par surface affichée) :
//! le gestionnaire route la souris vers la fenêtre sous le curseur (saisies implicite et
//! explicite comprises), le clavier vers la fenêtre active, et prévient une fenêtre quand elle
//! perd le pointeur (`PointerLeave`) ou gagne et perd le focus (`FocusEvent`). Chaque client a
//! donc son propre focus de pointeur et de clavier, sans état partagé à démêler.
//!
//! * Clavier : codes evdev des pilotes, carte XKB partagée ([`crate::keymap`]), modificateurs
//!   tenus par le serveur, répétition laissée au client (`repeat_info` ; les répétitions du
//!   matériel sont filtrées, comme le fait libinput).
//! * Pointeur : coordonnées en virgule fixe dans la surface, boutons evdev (`BTN_*`), molette
//!   en crans (`axis_value120` depuis la v8, `axis_discrete` avant), un `frame` par groupe.
//! * Curseur : `wl_pointer.set_cursor` (rôle « curseur ») ou `cursor-shape-v1`, appliqués
//!   seulement si le client a encore le pointeur et que le numéro d'entrée est le bon.

use alloc::{
    format,
    rc::Rc,
    string::String,
    sync::Arc,
    vec::Vec,
};
use core::cell::RefCell;
use event_types::MousePositionEvent;
use keycodes_ascii::{KeyAction, KeyEvent, KeyboardLayout};
use time::Instant;
use window_manager::{CursorImage, CursorShape};
use wl_proto::protocols::*;
use wl_proto::{Arg, Fixed, ObjectId, ProtocolError, Request};

use crate::client::{Fd, Obj, State};
use crate::surface::{Role, Surface};
use crate::{keymap, now_ms, Shared};

/// Délai et cadence de répétition annoncés (ceux de Mutter et de KWin par défaut).
const REPEAT_RATE: i32 = 25;
const REPEAT_DELAY: i32 = 600;

/// Boutons evdev, dans l'ordre des bits de [`buttons_of`].
const BUTTONS: [u32; 5] = [0x110, 0x111, 0x112, 0x113, 0x114];

/// Focus du pointeur d'un client.
pub(crate) struct PtrFocus {
    pub surface: ObjectId,
    /// Numéro de l'entrée (`set_cursor` et `set_shape` doivent le citer).
    pub serial: u32,
    pub x: Fixed,
    pub y: Fixed,
    /// Boutons signalés enfoncés.
    pub buttons: u8,
}

/// Surface de curseur choisie par `wl_pointer.set_cursor`.
pub(crate) struct CursorSurface {
    pub surface: Rc<RefCell<Surface>>,
    pub hot: (i32, i32),
}

/// Entrées d'un client.
#[derive(Default)]
pub(crate) struct Seat {
    pub pointers: Vec<ObjectId>,
    pub keyboards: Vec<ObjectId>,
    pub ptr: Option<PtrFocus>,
    /// Surface qui a le focus du clavier.
    pub kbd: Option<ObjectId>,
    /// Touches signalées enfoncées au client (codes evdev).
    keys: Vec<u16>,
    /// Derniers modificateurs envoyés (enfoncés, verrouillés).
    mods: Option<(u32, u32)>,
    /// Disposition de la carte envoyée aux claviers.
    layout: Option<KeyboardLayout>,
    pub cursor: Option<CursorSurface>,
    /// Numéro du dernier événement d'entrée de l'utilisateur (bouton, touche) : une saisie de
    /// menu (`xdg_popup.grab`) doit le citer.
    pub input_serial: u32,
}

fn buttons_of(m: &MousePositionEvent) -> u8 {
    (m.left_button_hold as u8)
        | (m.right_button_hold as u8) << 1
        | (m.middle_button_hold as u8) << 2
        | (m.fourth_button_hold as u8) << 3
        | (m.fifth_button_hold as u8) << 4
}

fn version(c: &State, id: ObjectId) -> u32 {
    c.objs.get(id).map_or(0, |o| o.version)
}

/// Requêtes de `wl_seat`.
pub(crate) fn seat_request(c: &mut State, id: ObjectId, op: u16, req: &Request<'_, Fd>, sh: &mut Shared) -> Result<(), ProtocolError> {
    let v = req.version;
    match op {
        wl_seat::req::GET_POINTER => {
            let pid = req.args.new_id(0);
            c.create(pid, &wl_pointer::INTERFACE, v, Obj::Pointer)?;
            c.seat.pointers.push(pid);
        }
        wl_seat::req::GET_KEYBOARD => {
            let kid = req.args.new_id(0);
            c.create(kid, &wl_keyboard::INTERFACE, v, Obj::Keyboard)?;
            c.seat.keyboards.push(kid);
            let layout = keycodes_ascii::current_layout();
            send_keymap(c, kid, layout);
            if v >= 4 {
                c.send(kid, wl_keyboard::evt::REPEAT_INFO, [Arg::Int(REPEAT_RATE), Arg::Int(REPEAT_DELAY)]);
            }
            // Le client a déjà le focus : ce clavier l'apprend aussitôt.
            if let Some(sid) = c.seat.kbd {
                let serial = sh.next_serial();
                let keys = keys_array(&c.seat.keys);
                c.send(kid, wl_keyboard::evt::ENTER, [Arg::Uint(serial), Arg::Object(sid), Arg::Array(&keys)]);
                if let Some((dep, locked)) = c.seat.mods {
                    c.send(kid, wl_keyboard::evt::MODIFIERS, [Arg::Uint(serial), Arg::Uint(dep), Arg::Uint(0), Arg::Uint(locked), Arg::Uint(0)]);
                }
            }
        }
        wl_seat::req::GET_TOUCH => {
            return Err(ProtocolError::new(id, wl_seat::error::MISSING_CAPABILITY, String::from("ce siege n'a pas d'ecran tactile")));
        }
        _ => {} // release : destructeur générique
    }
    Ok(())
}

/// Annonce du siège à la liaison.
pub(crate) fn describe_seat(c: &mut State, id: ObjectId) {
    let caps = wl_seat::capability::POINTER | wl_seat::capability::KEYBOARD;
    c.send(id, wl_seat::evt::CAPABILITIES, [Arg::Uint(caps)]);
    c.send(id, wl_seat::evt::NAME, [Arg::Str(Some(b"seat0"))]);
}

/// Requêtes de `wl_pointer`.
pub(crate) fn pointer_request(c: &mut State, id: ObjectId, op: u16, req: &Request<'_, Fd>, sh: &mut Shared) -> Result<(), ProtocolError> {
    match op {
        wl_pointer::req::SET_CURSOR => {
            let (serial, sid, hx, hy) = (req.args.uint(0), req.args.object(1), req.args.int(2), req.args.int(3));
            let surface = match c.objs.get(sid).map(|o| &o.data) {
                Some(Obj::Surface(s)) => Some(s.clone()),
                _ => None,
            };
            if let Some(s) = &surface {
                let mut sb = s.borrow_mut();
                if sb.role != Role::None && sb.role != Role::Cursor {
                    return Err(ProtocolError::new(id, wl_pointer::error::ROLE, format!("la surface {sid} a deja le role {:?}", sb.role)));
                }
                sb.role = Role::Cursor;
            }
            // Un numéro périmé (ou pas d'entrée en cours) : requête ignorée, comme le veut le
            // protocole (le pointeur est peut-être déjà ailleurs).
            if c.seat.ptr.as_ref().map(|p| p.serial) != Some(serial) {
                return Ok(());
            }
            c.seat.cursor = surface.map(|s| CursorSurface { surface: s, hot: (hx, hy) });
            if c.seat.cursor.is_none() {
                apply_cursor_image(c, sh, Some(Arc::new(CursorImage::new(1, 1, 0, 0))));
            } else {
                refresh_cursor(c, sh);
            }
        }
        _ => {
            // release
            c.seat.pointers.retain(|&p| p != id);
        }
    }
    Ok(())
}

/// Requêtes de `wl_keyboard` (seulement `release`).
pub(crate) fn keyboard_request(c: &mut State, id: ObjectId) {
    c.seat.keyboards.retain(|&k| k != id);
}

/// Image du curseur, si ce client tient toujours le pointeur.
pub(crate) fn apply_cursor_image(c: &State, sh: &mut Shared, img: Option<Arc<CursorImage>>) {
    let Some(p) = &c.seat.ptr else { return };
    if sh.cursor_owner == Some((c.client, p.surface)) {
        window_manager::set_cursor_image(img);
    }
}

/// Forme du curseur (`wp_cursor_shape_device_v1.set_shape`).
pub(crate) fn set_shape(c: &mut State, sh: &mut Shared, serial: u32, shape: CursorShape) {
    if c.seat.ptr.as_ref().map(|p| p.serial) != Some(serial) {
        return;
    }
    c.seat.cursor = None;
    let Some(p) = &c.seat.ptr else { return };
    if sh.cursor_owner == Some((c.client, p.surface)) {
        window_manager::set_cursor_shape(shape);
    }
}

/// Recopie la surface de curseur du client dans l'image du curseur.
pub(crate) fn refresh_cursor(c: &State, sh: &mut Shared) {
    let Some(cur) = &c.seat.cursor else { return };
    let img = cursor_image_of(&cur.surface.borrow(), cur.hot);
    apply_cursor_image(c, sh, Some(Arc::new(img)));
}

/// `commit` d'une surface au rôle de curseur : décalage du point chaud, nouvelle image.
pub(crate) fn cursor_committed(c: &mut State, sh: &mut Shared, s: &Rc<RefCell<Surface>>, offset: (i32, i32)) {
    let mine = c.seat.cursor.as_ref().is_some_and(|cur| Rc::ptr_eq(&cur.surface, s));
    if !mine {
        return;
    }
    if let Some(cur) = c.seat.cursor.as_mut() {
        cur.hot = (cur.hot.0.saturating_sub(offset.0), cur.hot.1.saturating_sub(offset.1));
    }
    refresh_cursor(c, sh);
}

/// Image du curseur à la taille de la surface (échelle, transformation et viewport appliqués :
/// un curseur dessiné à l'échelle 2 garde sa taille à l'écran). Point chaud en coordonnées de
/// la surface.
fn cursor_image_of(s: &Surface, hot: (i32, i32)) -> CursorImage {
    let (Some(buf), Some(view)) = (&s.buffer, s.view) else { return CursorImage::new(1, 1, 0, 0) };
    let (w, h) = view.size();
    let mut img = CursorImage::new(w as usize, h as usize, hot.0, hot.1);
    let (w, h) = (img.width, img.height);
    let full = region2d::Rect::new(0, 0, w as i32, h as i32);
    crate::pixels::blit(buf, &view, &region2d::Region::from_rect(full), full, &mut img.px, (w, h), (0, 0));
    img
}

/// Événement de pointeur sur la surface `sid`, en `(x, y)` (coordonnées de la surface).
pub(crate) fn pointer_event(c: &mut State, sh: &mut Shared, sid: ObjectId, x: Fixed, y: Fixed, m: &MousePositionEvent) {
    let now = now_ms(Instant::now());
    let mut events = false;
    if c.seat.ptr.as_ref().map(|p| p.surface) != Some(sid) {
        pointer_leave(c, sh);
        let serial = sh.next_serial();
        for p in c.seat.pointers.clone() {
            c.send(p, wl_pointer::evt::ENTER, [Arg::Uint(serial), Arg::Object(sid), Arg::Fixed(x), Arg::Fixed(y)]);
        }
        c.seat.ptr = Some(PtrFocus { surface: sid, serial, x, y, buttons: 0 });
        // Le client choisira son curseur ; d'ici là, la flèche.
        sh.cursor_owner = Some((c.client, sid));
        window_manager::set_cursor_image(None);
        c.seat.cursor = None;
        events = true;
    } else if let Some(p) = c.seat.ptr.as_mut() {
        if p.x != x || p.y != y {
            p.x = x;
            p.y = y;
            for ptr in c.seat.pointers.clone() {
                c.send(ptr, wl_pointer::evt::MOTION, [Arg::Uint(now), Arg::Fixed(x), Arg::Fixed(y)]);
            }
            events = true;
        }
    }
    let new = buttons_of(m);
    let old = c.seat.ptr.as_ref().map_or(0, |p| p.buttons);
    for (bit, code) in BUTTONS.iter().enumerate() {
        let mask = 1u8 << bit;
        if (new ^ old) & mask == 0 {
            continue;
        }
        let state = if new & mask != 0 { wl_pointer::button_state::PRESSED } else { wl_pointer::button_state::RELEASED };
        let serial = sh.next_serial();
        if state == wl_pointer::button_state::PRESSED {
            c.seat.input_serial = serial;
        }
        for p in c.seat.pointers.clone() {
            c.send(p, wl_pointer::evt::BUTTON, [Arg::Uint(serial), Arg::Uint(now), Arg::Uint(*code), Arg::Uint(state)]);
        }
        events = true;
    }
    if let Some(p) = c.seat.ptr.as_mut() {
        p.buttons = new;
    }
    if m.scroll != 0 {
        // Molette vers le haut : valeurs négatives (le contenu remonte).
        let clicks = -(m.scroll as i32);
        for p in c.seat.pointers.clone() {
            let v = version(c, p);
            if v >= 5 {
                c.send(p, wl_pointer::evt::AXIS_SOURCE, [Arg::Uint(wl_pointer::axis_source::WHEEL)]);
            }
            if v >= 8 {
                c.send(p, wl_pointer::evt::AXIS_VALUE120, [Arg::Uint(wl_pointer::axis::VERTICAL_SCROLL), Arg::Int(clicks * 120)]);
            } else if v >= 5 {
                c.send(p, wl_pointer::evt::AXIS_DISCRETE, [Arg::Uint(wl_pointer::axis::VERTICAL_SCROLL), Arg::Int(clicks)]);
            }
            if v >= 9 {
                c.send(
                    p,
                    wl_pointer::evt::AXIS_RELATIVE_DIRECTION,
                    [Arg::Uint(wl_pointer::axis::VERTICAL_SCROLL), Arg::Uint(wl_pointer::axis_relative_direction::IDENTICAL)],
                );
            }
            c.send(p, wl_pointer::evt::AXIS, [Arg::Uint(now), Arg::Uint(wl_pointer::axis::VERTICAL_SCROLL), Arg::Fixed(Fixed::from_int(clicks * 15))]);
        }
        events = true;
    }
    if events {
        frame(c);
    }
}

fn frame(c: &mut State) {
    for p in c.seat.pointers.clone() {
        if version(c, p) >= 5 {
            c.send(p, wl_pointer::evt::FRAME, []);
        }
    }
}

/// Le pointeur quitte la surface qu'il désignait.
pub(crate) fn pointer_leave(c: &mut State, sh: &mut Shared) {
    let Some(p) = c.seat.ptr.take() else { return };
    if c.objs.get(p.surface).is_some() {
        let serial = sh.next_serial();
        for ptr in c.seat.pointers.clone() {
            c.send(ptr, wl_pointer::evt::LEAVE, [Arg::Uint(serial), Arg::Object(p.surface)]);
        }
        frame(c);
    }
    c.seat.cursor = None;
    if sh.cursor_owner == Some((c.client, p.surface)) {
        sh.cursor_owner = None;
        window_manager::set_cursor_image(None);
    }
}

/// Envoie la carte de `layout` au clavier `kid`.
fn send_keymap(c: &mut State, kid: ObjectId, layout: KeyboardLayout) {
    c.seat.layout = Some(layout);
    match keymap::get(layout) {
        Some(k) => {
            let fd: Fd = k.file.clone();
            c.send(kid, wl_keyboard::evt::KEYMAP, [Arg::Uint(wl_keyboard::keymap_format::XKB_V1), Arg::Fd(fd), Arg::Uint(k.size)]);
        }
        None => {
            // Pas de carte : le client interprète les codes lui-même.
            let null: Fd = maiproc::fd::OpenFile::new(maiproc::fd::Node::Null, 0);
            c.send(kid, wl_keyboard::evt::KEYMAP, [Arg::Uint(wl_keyboard::keymap_format::NO_KEYMAP), Arg::Fd(null), Arg::Uint(0)]);
        }
    }
}

/// La disposition de MaiOS a changé (Ctrl+Maj+Espace) : nouvelle carte pour tous les claviers.
fn sync_layout(c: &mut State) -> bool {
    let layout = keycodes_ascii::current_layout();
    if c.seat.layout == Some(layout) || c.seat.keyboards.is_empty() {
        return false;
    }
    for k in c.seat.keyboards.clone() {
        send_keymap(c, k, layout);
    }
    c.seat.mods = None;
    true
}

fn keys_array(keys: &[u16]) -> Vec<u8> {
    keys.iter().flat_map(|&k| (k as u32).to_le_bytes()).collect()
}

/// Envoie les modificateurs s'ils ont changé.
fn send_modifiers(c: &mut State, sh: &mut Shared, m: keycodes_ascii::KeyboardModifiers) {
    let layout = c.seat.layout.unwrap_or_else(keycodes_ascii::current_layout);
    let depressed = c.seat.keys.iter().fold(0, |acc, &k| acc | keymap::modifier_of(k, layout));
    let now = (depressed, keymap::locked(m));
    if c.seat.mods == Some(now) {
        return;
    }
    c.seat.mods = Some(now);
    let serial = sh.next_serial();
    for k in c.seat.keyboards.clone() {
        c.send(k, wl_keyboard::evt::MODIFIERS, [Arg::Uint(serial), Arg::Uint(now.0), Arg::Uint(0), Arg::Uint(now.1), Arg::Uint(0)]);
    }
}

/// La surface `sid` reçoit le focus du clavier.
pub(crate) fn keyboard_enter(c: &mut State, sh: &mut Shared, sid: ObjectId) {
    if c.seat.kbd == Some(sid) {
        return;
    }
    keyboard_leave(c, sh);
    sync_layout(c);
    let (pressed, mods) = window_manager::keyboard_state();
    c.seat.keys = pressed.iter().filter(|&k| k < 0x300).collect();
    c.seat.kbd = Some(sid);
    let serial = sh.next_serial();
    let keys = keys_array(&c.seat.keys);
    for k in c.seat.keyboards.clone() {
        c.send(k, wl_keyboard::evt::ENTER, [Arg::Uint(serial), Arg::Object(sid), Arg::Array(&keys)]);
    }
    c.seat.mods = None;
    send_modifiers(c, sh, mods);
    crate::data::focus_in(c, sh);
}

/// La surface qui avait le focus du clavier le perd.
pub(crate) fn keyboard_leave(c: &mut State, sh: &mut Shared) {
    let Some(sid) = c.seat.kbd.take() else { return };
    if c.objs.get(sid).is_some() {
        let serial = sh.next_serial();
        for k in c.seat.keyboards.clone() {
            c.send(k, wl_keyboard::evt::LEAVE, [Arg::Uint(serial), Arg::Object(sid)]);
        }
    }
    c.seat.keys.clear();
}

/// Touche enfoncée ou relâchée, pour la surface qui a le focus.
pub(crate) fn key(c: &mut State, sh: &mut Shared, ev: &KeyEvent) {
    if c.seat.kbd.is_none() || ev.code == 0 {
        return;
    }
    let pressed = ev.action == KeyAction::Pressed;
    if pressed {
        if c.seat.keys.contains(&ev.code) {
            return; // répétition du matériel : le client répète lui-même
        }
        c.seat.keys.push(ev.code);
    } else {
        let Some(i) = c.seat.keys.iter().position(|&k| k == ev.code) else { return };
        c.seat.keys.swap_remove(i);
    }
    if sync_layout(c) {
        // Nouvelle carte : les modificateurs repartent de zéro chez le client.
        send_modifiers(c, sh, ev.modifiers);
    }
    let serial = sh.next_serial();
    if pressed {
        c.seat.input_serial = serial;
    }
    let state = if pressed { wl_keyboard::key_state::PRESSED } else { wl_keyboard::key_state::RELEASED };
    let now = now_ms(Instant::now());
    for k in c.seat.keyboards.clone() {
        c.send(k, wl_keyboard::evt::KEY, [Arg::Uint(serial), Arg::Uint(now), Arg::Uint(ev.code as u32), Arg::Uint(state)]);
    }
    send_modifiers(c, sh, ev.modifiers);
}

/// Une surface disparaît : plus de focus sur elle (sans `leave`, l'objet n'existe plus).
pub(crate) fn surface_gone(c: &mut State, sh: &mut Shared, sid: ObjectId) {
    if c.seat.ptr.as_ref().is_some_and(|p| p.surface == sid) {
        c.seat.ptr = None;
        if sh.cursor_owner == Some((c.client, sid)) {
            sh.cursor_owner = None;
            window_manager::set_cursor_image(None);
        }
    }
    if c.seat.kbd == Some(sid) {
        c.seat.kbd = None;
        c.seat.keys.clear();
    }
}
