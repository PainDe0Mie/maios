//! Descripteurs d'interfaces et types de base du protocole.

use core::fmt;

/// Type d'un argument sur le fil.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArgKind {
    Int,
    Uint,
    /// Nombre à virgule fixe signé 24.8.
    Fixed,
    String,
    Object,
    /// Nouvel objet ; sans interface fixée (`wl_registry.bind`) : nom d'interface, version, id.
    NewId,
    Array,
    /// Descripteur de fichier, hors bande (`SCM_RIGHTS`).
    Fd,
}

/// Argument d'un message.
#[derive(Debug)]
pub struct ArgDesc {
    pub name: &'static str,
    pub kind: ArgKind,
    /// `allow-null` : chaîne ou objet absent (0 sur le fil).
    pub nullable: bool,
    /// Interface de l'objet (ou du nouvel objet), si elle fait partie des protocoles générés.
    pub interface: Option<&'static Interface>,
    /// Nom de l'interface déclarée, connue ou non.
    pub interface_name: Option<&'static str>,
}

/// Requête ou événement.
#[derive(Debug)]
pub struct MessageDesc {
    pub name: &'static str,
    /// Première version de l'interface qui le connaît.
    pub since: u32,
    /// Détruit l'objet qui le porte.
    pub destructor: bool,
    pub args: &'static [ArgDesc],
}

impl MessageDesc {
    /// Nombre de descripteurs portés.
    pub fn fd_count(&self) -> usize {
        self.args.iter().filter(|a| a.kind == ArgKind::Fd).count()
    }
}

/// Interface Wayland.
pub struct Interface {
    pub name: &'static str,
    pub version: u32,
    pub requests: &'static [MessageDesc],
    pub events: &'static [MessageDesc],
}

impl Interface {
    /// Même interface (les descripteurs sont uniques, mais on compare aussi le nom pour les
    /// interfaces référencées sans descripteur).
    pub fn is(&self, other: &Interface) -> bool {
        core::ptr::eq(self, other) || self.name == other.name
    }
}

impl fmt::Debug for Interface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@v{}", self.name, self.version)
    }
}

/// Nombre signé à virgule fixe 24.8 (`wl_fixed_t`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Fixed(pub i32);

impl Fixed {
    pub const fn from_int(v: i32) -> Fixed {
        Fixed(v.saturating_mul(256))
    }
    /// En 1/256 (la valeur brute).
    pub const fn raw(self) -> i32 {
        self.0
    }
    /// Partie entière, arrondie vers moins l'infini.
    pub const fn floor(self) -> i32 {
        self.0 >> 8
    }
    /// Arrondi au plus proche.
    pub const fn round(self) -> i32 {
        (self.0.saturating_add(128)) >> 8
    }
}

/// Identifiant d'objet sur le fil (0 = aucun).
pub type ObjectId = u32;

/// Premier identifiant alloué par le serveur.
pub const SERVER_ID_START: u32 = 0xff00_0000;
/// Identifiant de `wl_display`.
pub const DISPLAY_ID: u32 = 1;
