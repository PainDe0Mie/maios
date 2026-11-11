//! # wl_proto : cœur du protocole Wayland de MaiLand
//!
//! Phase W0 du plan `.claude/plans/06-mailand-wayland.md`. Sans dépendance, `no_std` + `alloc`,
//! testé sur l'hôte comme `llfree` (copier la crate hors du workspace, `cargo test`).
//!
//! * [`protocols`] : un module par interface, généré par `build.rs` depuis `protocols/*.xml`
//!   (l'équivalent de `wayland-scanner`) : descripteur `INTERFACE`, `req::*`, `evt::*`, valeurs
//!   des énumérations.
//! * [`wire`] : format filaire, décodage vérifié (arithmétique 64 bits, chaînes strictes,
//!   message consommé exactement) et encodage contrôlé par la signature.
//! * [`objmap`] : table d'objets dense et bornée, objets zombies.
//! * [`conn`] : tampons de connexion ; une requête n'en sort que validée (objet vivant, opcode
//!   et version, arguments, objets désignés du bon type, nouveaux identifiants libres).
//!
//! Le serveur (`kernel/gfx/mailand`) n'a plus qu'à exécuter des requêtes sûres.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod conn;
pub mod objmap;
pub mod types;
pub mod wire;

/// Interfaces générées depuis `protocols/*.xml`.
#[allow(clippy::all)]
pub mod protocols {
    include!(concat!(env!("OUT_DIR"), "/protocols.rs"));

    /// Interface de nom `name`.
    pub fn find(name: &[u8]) -> Option<&'static crate::types::Interface> {
        ALL.iter().copied().find(|i| i.name.as_bytes() == name)
    }
}

pub use conn::{InBuf, Limits, OutBuf, ProtocolError, Request, SendError, MAX_FDS_PER_SEND};
pub use objmap::{IdError, Lookup, Object, ObjectMap};
pub use types::*;
pub use wire::{Arg, Args, WireError};

#[cfg(test)]
mod tests;
