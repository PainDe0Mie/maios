#![no_std]

use modular_bitfield::{bitfield, specifiers::B3};

//NOTE: could be reduced to 9+9+4=22-bit, +2-bit padding = 3*8-bit
#[derive(Debug, Clone)]
pub struct MouseMovementRelative {
    pub x_movement: i16,
    pub y_movement: i16,
    /// Crans de molette : positif vers le haut (convention de `REL_WHEEL` de Linux et de
    /// `MousePositionEvent::scroll`) ; chaque pilote convertit celle de son matériel.
    pub scroll_movement: i8,
}

impl MouseMovementRelative {
    pub fn new(x_movement: i16, y_movement: i16, scroll_movement: i8) -> Self {
        Self {
            x_movement,
            y_movement,
            scroll_movement,
        }
    }
}

#[bitfield(bits = 8)]
#[derive(Debug, Clone)]
pub struct MouseButtons {
    pub left: bool,
    pub right: bool,
    pub middle: bool,
    pub fourth: bool,
    pub fifth: bool,
    #[skip] __: B3
}

/// Position absolue d'un pointeur (tablette, écran tactile, `virtio-tablet` de QEMU) dans
/// l'étendue annoncée par l'appareil : `0..=max_x` couvre toute la largeur de l'écran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MousePositionAbsolute {
    pub x: u32,
    pub y: u32,
    pub max_x: u32,
    pub max_y: u32,
}

impl MousePositionAbsolute {
    /// Point de l'écran `w × h` désigné (bornes comprises, comme `evdev` de X.Org et
    /// `libinput` : `max` tombe sur le dernier pixel).
    pub fn to_screen(&self, w: usize, h: usize) -> (isize, isize) {
        let scale = |v: u32, max: u32, n: usize| -> isize {
            if max == 0 || n == 0 {
                0
            } else {
                ((v.min(max) as u64 * (n as u64 - 1) + max as u64 / 2) / max as u64) as isize
            }
        };
        (scale(self.x, self.max_x, w), scale(self.y, self.max_y, h))
    }
}

#[derive(Debug, Clone)]
pub struct MouseEvent {
    pub buttons: MouseButtons,
    /// Déplacement relatif (souris) ; nul pour un pointeur absolu, sauf la molette.
    pub movement: MouseMovementRelative,
    /// Position absolue (tablette) : remplace le déplacement relatif.
    pub absolute: Option<MousePositionAbsolute>,
}

impl MouseEvent {
    pub fn new(buttons: MouseButtons, movement: MouseMovementRelative) -> MouseEvent {
        MouseEvent {
            buttons,
            movement,
            absolute: None,
        }
    }

    /// Événement d'un pointeur absolu (la molette reste dans `scroll`).
    pub fn new_absolute(buttons: MouseButtons, position: MousePositionAbsolute, scroll: i8) -> MouseEvent {
        MouseEvent {
            buttons,
            movement: MouseMovementRelative::new(0, 0, scroll),
            absolute: Some(position),
        }
    }
}
