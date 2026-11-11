//! Format filaire de virtio-gpu (VirtIO 1.3, § 5.7.6), petit-boutiste.
//!
//! Les commandes sont encodées octet par octet (pas de `repr(C, packed)` à lire de travers) :
//! en-tête commun de 24 octets, puis les champs de la commande.

use alloc::vec::Vec;

/// Commandes de la `controlq` et de la `cursorq`.
pub mod cmd {
    pub const GET_DISPLAY_INFO: u32 = 0x0100;
    pub const RESOURCE_CREATE_2D: u32 = 0x0101;
    pub const RESOURCE_UNREF: u32 = 0x0102;
    pub const SET_SCANOUT: u32 = 0x0103;
    pub const RESOURCE_FLUSH: u32 = 0x0104;
    pub const TRANSFER_TO_HOST_2D: u32 = 0x0105;
    pub const RESOURCE_ATTACH_BACKING: u32 = 0x0106;
    pub const RESOURCE_DETACH_BACKING: u32 = 0x0107;
    pub const GET_CAPSET_INFO: u32 = 0x0108;
    pub const GET_CAPSET: u32 = 0x0109;
    pub const GET_EDID: u32 = 0x010A;
    pub const UPDATE_CURSOR: u32 = 0x0300;
    pub const MOVE_CURSOR: u32 = 0x0301;
}

/// Types de réponse.
pub mod resp {
    pub const OK_NODATA: u32 = 0x1100;
    pub const OK_DISPLAY_INFO: u32 = 0x1101;
    pub const OK_CAPSET_INFO: u32 = 0x1102;
    pub const OK_CAPSET: u32 = 0x1103;
    pub const OK_EDID: u32 = 0x1104;
    pub const ERR_UNSPEC: u32 = 0x1200;
    pub const ERR_OUT_OF_MEMORY: u32 = 0x1201;
    pub const ERR_INVALID_SCANOUT_ID: u32 = 0x1202;
    pub const ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
    pub const ERR_INVALID_CONTEXT_ID: u32 = 0x1204;
    pub const ERR_INVALID_PARAMETER: u32 = 0x1205;

    /// Nom lisible d'un type de réponse.
    pub fn name(t: u32) -> &'static str {
        match t {
            OK_NODATA => "OK",
            OK_DISPLAY_INFO => "OK_DISPLAY_INFO",
            OK_CAPSET_INFO => "OK_CAPSET_INFO",
            OK_CAPSET => "OK_CAPSET",
            OK_EDID => "OK_EDID",
            ERR_UNSPEC => "ERR_UNSPEC",
            ERR_OUT_OF_MEMORY => "ERR_OUT_OF_MEMORY",
            ERR_INVALID_SCANOUT_ID => "ERR_INVALID_SCANOUT_ID",
            ERR_INVALID_RESOURCE_ID => "ERR_INVALID_RESOURCE_ID",
            ERR_INVALID_CONTEXT_ID => "ERR_INVALID_CONTEXT_ID",
            ERR_INVALID_PARAMETER => "ERR_INVALID_PARAMETER",
            0 => "aucune réponse",
            _ => "inconnue",
        }
    }

    /// Vrai pour une erreur rendue par l'appareil.
    pub fn is_err(t: u32) -> bool {
        (0x1200..0x1300).contains(&t)
    }
}

/// Fonctions propres à virtio-gpu (§ 5.7.3).
pub mod feature {
    pub const VIRGL: u64 = 1 << 0;
    pub const EDID: u64 = 1 << 1;
    pub const RESOURCE_UUID: u64 = 1 << 2;
    pub const RESOURCE_BLOB: u64 = 1 << 3;
    pub const CONTEXT_INIT: u64 = 1 << 4;
}

/// Formats (§ 5.7.6.8), nommés dans l'ordre des octets en mémoire.
pub mod format {
    pub const B8G8R8A8_UNORM: u32 = 1;
    pub const B8G8R8X8_UNORM: u32 = 2;
    pub const A8R8G8B8_UNORM: u32 = 3;
    pub const X8R8G8B8_UNORM: u32 = 4;
    pub const R8G8B8A8_UNORM: u32 = 67;
    pub const X8B8G8R8_UNORM: u32 = 68;
    pub const A8B8G8R8_UNORM: u32 = 121;
    pub const R8G8B8X8_UNORM: u32 = 134;
}

/// `VIRTIO_GPU_FLAG_FENCE` : la réponse n'arrive qu'une fois la commande achevée, avec son
/// numéro de fence.
pub const FLAG_FENCE: u32 = 1;
/// Bit de `events_read` : la configuration des affichages a changé.
pub const EVENT_DISPLAY: u32 = 1;
/// Taille de l'en-tête commun.
pub const HDR_LEN: usize = 24;
/// Scanouts au plus.
pub const MAX_SCANOUTS: usize = 16;
/// Côté d'une ressource de curseur (QEMU n'en accepte pas d'autre).
pub const CURSOR_SIDE: usize = 64;
/// Taille de la réponse de `GET_DISPLAY_INFO`.
pub const DISPLAY_INFO_LEN: usize = HDR_LEN + MAX_SCANOUTS * 24;
/// Taille de la réponse de `GET_EDID`.
pub const EDID_RESP_LEN: usize = HDR_LEN + 8 + 1024;

/// Rectangle de virtio-gpu.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn area(&self) -> u64 {
        self.width as u64 * self.height as u64
    }
    pub fn union(&self, o: &Rect) -> Rect {
        let x0 = self.x.min(o.x);
        let y0 = self.y.min(o.y);
        let x1 = (self.x + self.width).max(o.x + o.width);
        let y1 = (self.y + self.height).max(o.y + o.height);
        Rect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 }
    }
}

/// Encodeur d'une commande.
pub struct Enc(pub Vec<u8>);

impl Enc {
    /// En-tête : type, drapeaux, fence, contexte 0, anneau 0.
    pub fn new(ty: u32, flags: u32, fence: u64) -> Enc {
        let mut v = Vec::with_capacity(64);
        v.extend_from_slice(&ty.to_le_bytes());
        v.extend_from_slice(&flags.to_le_bytes());
        v.extend_from_slice(&fence.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&[0u8; 4]);
        Enc(v)
    }
    pub fn u32(mut self, x: u32) -> Enc {
        self.0.extend_from_slice(&x.to_le_bytes());
        self
    }
    pub fn u64(mut self, x: u64) -> Enc {
        self.0.extend_from_slice(&x.to_le_bytes());
        self
    }
    pub fn rect(self, r: Rect) -> Enc {
        self.u32(r.x).u32(r.y).u32(r.width).u32(r.height)
    }
}

pub fn get_display_info() -> Vec<u8> {
    Enc::new(cmd::GET_DISPLAY_INFO, 0, 0).0
}

pub fn get_edid(scanout: u32) -> Vec<u8> {
    Enc::new(cmd::GET_EDID, 0, 0).u32(scanout).u32(0).0
}

pub fn resource_create_2d(id: u32, fmt: u32, w: u32, h: u32) -> Vec<u8> {
    Enc::new(cmd::RESOURCE_CREATE_2D, 0, 0).u32(id).u32(fmt).u32(w).u32(h).0
}

pub fn resource_unref(id: u32) -> Vec<u8> {
    Enc::new(cmd::RESOURCE_UNREF, 0, 0).u32(id).u32(0).0
}

pub fn resource_detach_backing(id: u32) -> Vec<u8> {
    Enc::new(cmd::RESOURCE_DETACH_BACKING, 0, 0).u32(id).u32(0).0
}

/// `RESOURCE_ATTACH_BACKING` : en-tête puis `entries` (adresse, longueur) contiguës.
pub fn attach_backing(id: u32, entries: &[(u64, u32)]) -> Vec<u8> {
    let mut e = Enc::new(cmd::RESOURCE_ATTACH_BACKING, 0, 0).u32(id).u32(entries.len() as u32);
    for &(addr, len) in entries {
        e = e.u64(addr).u32(len).u32(0);
    }
    e.0
}

pub fn set_scanout(scanout: u32, id: u32, r: Rect) -> Vec<u8> {
    Enc::new(cmd::SET_SCANOUT, 0, 0).rect(r).u32(scanout).u32(id).0
}

pub fn transfer_to_host_2d(id: u32, r: Rect, offset: u64, flags: u32, fence: u64) -> Vec<u8> {
    Enc::new(cmd::TRANSFER_TO_HOST_2D, flags, fence).rect(r).u64(offset).u32(id).u32(0).0
}

pub fn resource_flush(id: u32, r: Rect, flags: u32, fence: u64) -> Vec<u8> {
    Enc::new(cmd::RESOURCE_FLUSH, flags, fence).rect(r).u32(id).u32(0).0
}

/// `UPDATE_CURSOR` ou `MOVE_CURSOR` : position du point chaud, ressource (0 : caché), point
/// chaud dans l'image.
pub fn cursor(ty: u32, scanout: u32, x: u32, y: u32, id: u32, hot_x: u32, hot_y: u32) -> Vec<u8> {
    Enc::new(ty, 0, 0).u32(scanout).u32(x).u32(y).u32(0).u32(id).u32(hot_x).u32(hot_y).u32(0).0
}

/// Champs de l'en-tête d'une réponse : `(type, drapeaux, fence)`.
pub fn parse_hdr(b: &[u8]) -> (u32, u32, u64) {
    if b.len() < HDR_LEN {
        return (0, 0, 0);
    }
    let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let fence = u32_at(8) as u64 | (u32_at(12) as u64) << 32;
    (u32_at(0), u32_at(4), fence)
}

/// Un mode annoncé par `GET_DISPLAY_INFO`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Mode {
    pub scanout: u32,
    pub rect: Rect,
    pub enabled: bool,
    pub flags: u32,
}

/// Les scanouts actifs (ou le premier, s'il n'y en a aucun) de la réponse `b`.
pub fn parse_display_info(b: &[u8]) -> Vec<Mode> {
    let mut out = Vec::new();
    if b.len() < DISPLAY_INFO_LEN {
        return out;
    }
    let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    for i in 0..MAX_SCANOUTS {
        let o = HDR_LEN + 24 * i;
        let m = Mode {
            scanout: i as u32,
            rect: Rect { x: u32_at(o), y: u32_at(o + 4), width: u32_at(o + 8), height: u32_at(o + 12) },
            enabled: u32_at(o + 16) != 0,
            flags: u32_at(o + 20),
        };
        if m.enabled || (i == 0 && m.rect.width != 0) {
            out.push(m);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(get_display_info().len(), 24);
        assert_eq!(resource_create_2d(1, 2, 3, 4).len(), 40);
        assert_eq!(set_scanout(0, 1, Rect::default()).len(), 48);
        assert_eq!(resource_flush(1, Rect::default(), 0, 0).len(), 48);
        assert_eq!(transfer_to_host_2d(1, Rect::default(), 0, 0, 0).len(), 56);
        assert_eq!(attach_backing(1, &[(0, 4096), (8192, 4096)]).len(), 32 + 32);
        assert_eq!(cursor(cmd::MOVE_CURSOR, 0, 1, 2, 3, 4, 5).len(), 56);
        assert_eq!(get_edid(0).len(), 32);
    }
}
