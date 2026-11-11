//! Cartes XKB envoyées aux claviers (`wl_keyboard.keymap`) et masques de modificateurs.
//!
//! Les cartes sont compilées hors ligne par `xkbcli compile-keymap` (règles evdev de
//! xkb-data, `keymaps/gen.sh`) : exactement le texte qu'un compositeur obtient de
//! `xkb_keymap_get_as_string`, sans embarquer libxkbcommon dans le noyau. Une carte par
//! disposition de MaiOS (QWERTY → `us`, AZERTY → `fr`).
//!
//! Chaque carte est écrite une fois dans un fichier anonyme de tmpfs, ouvert en **lecture
//! seule** et partagé par tous les clients (comme Mutter depuis `wl_keyboard` v7 : le client
//! projette en `MAP_PRIVATE`, un client ne peut pas modifier la carte des autres).
//!
//! Le serveur tient lui-même l'état des modificateurs, comme `xkb_state` le ferait pour ces
//! cartes : touches enfoncées → modificateurs « enfoncés », Verr. Maj et Verr. Num →
//! modificateurs « verrouillés ». Les masques sont ceux des modificateurs réels des cartes
//! compilées (`modifier_map`) : Shift 1, Lock 2, Control 4, Mod1 (Alt) 8, Mod2 (NumLock) 16,
//! Mod4 (Super) 64, Mod5 (AltGr de `fr`, LevelThree) 128.

use alloc::{format, string::String, sync::Arc};
use keycodes_ascii::{evdev, KeyboardLayout, KeyboardModifiers};
use log::error;
use maiproc::fd::{Node, OpenFile};
use spin::Once;

const US: &[u8] = include_bytes!("../keymaps/us.xkb");
const FR: &[u8] = include_bytes!("../keymaps/fr.xkb");

const SHIFT: u32 = 1;
const LOCK: u32 = 2;
const CONTROL: u32 = 4;
const MOD1: u32 = 8;
const MOD2: u32 = 16;
const MOD4: u32 = 64;
const MOD5: u32 = 128;

/// Carte partagée : fichier en lecture seule et taille annoncée (zéro final compris).
pub(crate) struct Keymap {
    pub file: Arc<OpenFile>,
    pub size: u32,
}

static MAPS: [Once<Option<Keymap>>; 2] = [Once::new(), Once::new()];

fn index(layout: KeyboardLayout) -> usize {
    match layout {
        KeyboardLayout::Qwerty => 0,
        KeyboardLayout::Azerty => 1,
    }
}

/// Carte de la disposition `layout` (créée au premier usage). `None` si tmpfs refuse.
pub(crate) fn get(layout: KeyboardLayout) -> Option<&'static Keymap> {
    let i = index(layout);
    MAPS[i]
        .call_once(|| {
            let text = if i == 0 { US } else { FR };
            match make(text, layout) {
                Ok(k) => Some(k),
                Err(e) => {
                    error!("MaiLand : carte clavier {} impossible : {e}", layout.short_name());
                    None
                }
            }
        })
        .as_ref()
}

fn make(text: &[u8], layout: KeyboardLayout) -> Result<Keymap, String> {
    let file = tmpfs::anon_file(0o444).map_err(String::from)?;
    let mut data = alloc::vec::Vec::with_capacity(text.len() + 1);
    data.extend_from_slice(text);
    data.push(0);
    let n = file.lock().write_at(&data, 0).map_err(|e| format!("{e:?}"))?;
    if n != data.len() {
        return Err(format!("{n} octets ecrits sur {}", data.len()));
    }
    let path = format!("/memfd:mailand-keymap-{} (deleted)", layout.short_name());
    let of = OpenFile::new(Node::File { file, path }, 0 /* O_RDONLY */);
    Ok(Keymap { file: of, size: data.len() as u32 })
}

/// Masque du modificateur qu'active la touche `code` dans la carte `layout` (0 : aucun).
pub(crate) fn modifier_of(code: u16, layout: KeyboardLayout) -> u32 {
    match code {
        evdev::KEY_LEFTSHIFT | evdev::KEY_RIGHTSHIFT => SHIFT,
        evdev::KEY_LEFTCTRL | evdev::KEY_RIGHTCTRL => CONTROL,
        evdev::KEY_LEFTALT => MOD1,
        evdev::KEY_RIGHTALT => match layout {
            KeyboardLayout::Qwerty => MOD1,
            KeyboardLayout::Azerty => MOD5,
        },
        evdev::KEY_LEFTMETA | evdev::KEY_RIGHTMETA => MOD4,
        _ => 0,
    }
}

/// Modificateurs verrouillés d'après l'état du pilote (Verr. Maj, Verr. Num).
pub(crate) fn locked(m: KeyboardModifiers) -> u32 {
    let mut v = 0;
    if m.is_caps_lock() {
        v |= LOCK;
    }
    if m.is_num_lock() {
        v |= MOD2;
    }
    v
}
