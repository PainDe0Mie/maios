#![no_std]
//! Defines traits for Files and Directories within the virtual filesystem. These files and directories mimic 
//! that of a standard unix virtual filesystem, where directories follow a hierarchical system
//! and all directories have a parent directory (except for the special root directory). 
//! All files must be contained within directories. 
//! 
//! Note that both File and Directory extend from FsNode, which is a trait that defines
//! common methods for both Files and Directories to enhance code reuse 
//! 
//! Some functions return an enum FileOrDir; this allows us to seamlessly call functions on the return types of
//! other filesystem functions, and then we simply match on the FSnode to extract the concrete type
//! to perform the desired function

#[macro_use] extern crate alloc;
extern crate spin;
extern crate memory;
extern crate io;

use core::fmt;
use alloc::string::String;
use alloc::vec::Vec;
use spin::Mutex;
use alloc::sync::{Arc, Weak};
use memory::MappedPages;
use io::{ByteReader, ByteWriter, KnownLength};

/// A reference to any type that implements the [`File`] trait,
/// which can only represent a File (not a Directory).
pub type FileRef = Arc<Mutex<dyn File + Send>>;
/// A weak reference to any type that implements the [`File`] trait,
/// which can only represent a File (not a Directory).
pub type WeakFileRef = Weak<Mutex<dyn File + Send>>;
/// A reference to any type that implements the [`Directory`] trait,
/// which can only represent a Directory (not a File).
pub type DirRef =  Arc<Mutex<dyn Directory + Send>>;
/// A weak reference to any type that implements the [`Directory`] trait,
/// which can only represent a Directory (not a File).
pub type WeakDirRef = Weak<Mutex<dyn Directory + Send>>;


/// Type d'un nœud, au sens POSIX (`S_IFMT`). Un lien symbolique, un tube nommé ou un fichier
/// de périphérique d'un système de fichiers sur disque est un [`File`] dont les
/// [`Metadata`] disent le vrai type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    File,
    Dir,
    Symlink,
    CharDev,
    BlockDev,
    Fifo,
    Socket,
}

/// Date (secondes depuis 1970, nanosecondes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Timespec {
    pub sec: i64,
    pub nsec: u32,
}

/// Informations d'un nœud (`stat`) que tient un système de fichiers qui les conserve (ext4).
#[derive(Debug, Clone, Copy)]
pub struct Metadata {
    pub kind: NodeKind,
    /// Bits de droits (`0o7777`).
    pub perm: u16,
    pub uid: u32,
    pub gid: u32,
    /// Numéro d'inode, unique dans le volume `dev`.
    pub ino: u64,
    /// Identifiant du volume (`st_dev`).
    pub dev: u64,
    /// Périphérique désigné (`st_rdev`) : (majeur, mineur).
    pub rdev: (u32, u32),
    pub nlink: u32,
    pub size: u64,
    /// Place occupée, en secteurs de 512 octets.
    pub blocks: u64,
    pub blksize: u32,
    pub atime: Timespec,
    pub mtime: Timespec,
    pub ctime: Timespec,
    pub btime: Option<Timespec>,
}

/// Entrée d'un dossier telle que la liste le système de fichiers, sans ouvrir l'enfant.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub ino: u64,
    pub kind: Option<NodeKind>,
}

/// Nouvelle date d'un nœud (`utimensat`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeSet {
    /// Inchangée (`UTIME_OMIT`).
    Omit,
    /// Maintenant (`UTIME_NOW`).
    Now,
    At(Timespec),
}

/// Changement de métadonnées demandé par `chmod`, `chown` ou `utimensat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetaChange {
    /// Bits de droits (`0o7777`).
    Perm(u16),
    /// Propriétaire et groupe (`None` : inchangé, le `-1` de `chown`).
    Owner { uid: Option<u32>, gid: Option<u32> },
    Times { atime: TimeSet, mtime: TimeSet },
}

/// Erreur des opérations qu'un système de fichiers ne sait pas faire.
pub const NOT_SUPPORTED: &str = "operation non prise en charge par ce systeme de fichiers";

/// A trait that covers any filesystem node, both files and directories.
pub trait FsNode {
    /// Recursively gets the absolute pathname as a String
    fn get_absolute_path(&self) -> String {
        let mut path = self.get_name();
        if let Some(cur_dir) =  self.get_parent_dir()  {
            let parent_path = &cur_dir.lock().get_absolute_path();
            // Check if the parent path is root 
            if parent_path == "/" {
                path.insert_str(0,  parent_path);
                return path;
            }
            path.insert_str(0, &format!("{parent_path}/"));
            return path;
        }
        path
    }

    /// Returns the string name of the node
    fn get_name(&self) -> String;

    /// Returns the parent directory of the current node.
    fn get_parent_dir(&self) -> Option<DirRef>;

    /// Sets this node's parent directory.
    /// This is useful for ensuring correctness when inserting or removing 
    /// files or directories from their parent directory.
    fn set_parent_dir(&mut self, new_parent: WeakDirRef);

    /// Change le nom porté par le nœud, sans toucher à son répertoire parent : pour renommer ou
    /// déplacer un nœud déjà inséré, passer par [`move_node`], qui met aussi le parent à jour.
    /// Par défaut, le système de fichiers ne sait pas renommer.
    fn set_name(&mut self, _name: String) -> Result<(), &'static str> {
        Err("renommage non pris en charge par ce systeme de fichiers")
    }

    /// Le nœud peut changer de nom et de dossier parent (`set_name`, `set_parent_dir`, puis
    /// `remove`/`insert`) sans perdre son contenu. Vrai pour les systèmes de fichiers en
    /// mémoire ; faux par défaut, car ailleurs `remove` peut détruire les données (FAT32 libère
    /// les clusters) et `insert` créer une entrée vide.
    fn is_movable(&self) -> bool { false }

    /// Informations conservées par le système de fichiers ; `None` s'il n'en garde pas
    /// (systèmes en mémoire : l'appelant fabrique alors des valeurs par défaut).
    fn metadata(&self) -> Option<Metadata> { None }

    /// Le nœud appartient à un système de fichiers monté en lecture seule (`EROFS`).
    fn read_only(&self) -> bool { false }

    /// Change une métadonnée (droits, propriétaire, dates) ; [`NOT_SUPPORTED`] par défaut
    /// (l'appelant fait alors comme si le changement avait eu lieu, faute de mieux).
    fn set_metadata(&mut self, _change: MetaChange) -> Result<(), &'static str> {
        Err(NOT_SUPPORTED)
    }
}

// Trait for files, implementors of File must also implement FsNode
pub trait File : FsNode + ByteReader + ByteWriter + KnownLength {
    /// Returns a view of this file as an immutable memory-mapped region.
    fn as_mapping(&self) -> Result<&MappedPages, &'static str>;

    /// Change la taille du fichier (`truncate`) : coupe, ou complète de zéros.
    /// Par défaut, la taille n'est pas modifiable.
    fn set_len(&mut self, _len: usize) -> Result<(), &'static str> {
        Err("taille du fichier non modifiable")
    }

    /// Cible d'un lien symbolique ; `None` si le fichier n'en est pas un.
    fn read_link(&self) -> Option<String> { None }

    /// Nouveau nom du **même** fichier (lien dur, `link(2)`) : un nœud qui partage le contenu
    /// et l'inode de celui-ci, à insérer ensuite dans `parent`. `None` si le système de
    /// fichiers ne connaît pas les liens durs.
    fn new_link(&self, _name: String, _parent: WeakDirRef) -> Option<FileRef> { None }
}

/// Trait for directories, implementors of Directory must also implement FsNode
pub trait Directory : FsNode {
    /// Gets either the file or directory in this `Directory`  on its name.
    fn get(&self, name: &str) -> Option<FileOrDir>;

    /// Like [`Directory::get()`], but only looks for **files** matching the given `name` in this `Directory`.
    fn get_file(&self, name: &str) -> Option<FileRef> {
        match self.get(name) {
            Some(FileOrDir::File(f)) => Some(f),
            _ => None,
        }
    }

    /// Like [`Directory::get()`], but only looks for **directories** matching the given `name` in this `Directory`.
    fn get_dir(&self, name: &str) -> Option<DirRef> {
        match self.get(name) {
            Some(FileOrDir::Dir(d)) => Some(d),
            _ => None,
        }
    }

    /// Inserts the given new file or directory into this directory.
    /// If an existing node has the same name, that node is replaced and returned.
    /// 
    /// Note that this function **does not** set the given `node`'s parent directory;
    /// that should be set when the `node` was originally created, before calling this function. 
    /// However, if a node is replaced, that old node's parent directory will be cleared
    /// to reflect that it is no longer in this directory.
    /// 
    /// The lock on `node` must not be held because it will be acquired within this function.
    fn insert(&mut self, node: FileOrDir) -> Result<Option<FileOrDir>, &'static str>;

    /// Removes a file or directory from this directory and returns it if found.
    /// Also, the returned node's parent directory reference is cleared.
    /// 
    /// The lock on `node` must not be held because it will be acquired within this function.
    fn remove(&mut self, node: &FileOrDir) -> Option<FileOrDir>;

    /// Lists the names of the nodes in this directory.
    fn list(&self) -> Vec<String>;

    /// Entrées avec leur numéro d'inode et leur type, lues sans ouvrir les enfants
    /// (`getdents64`) ; `None` si le système de fichiers ne sait pas les donner.
    fn entries(&self) -> Option<Vec<DirEntry>> { None }

    /// Comme [`Directory::get`], avec la cible de l'enfant s'il est un lien symbolique, lue
    /// **sans verrouiller l'enfant** : la résolution des chemins ne doit jamais attendre le
    /// verrou d'un fichier, qu'un appelant peut tenir pendant qu'il résout un autre chemin
    /// (`mod_mgmt` le fait en chargeant les crates). Par défaut, pas de lien.
    fn get_with_link(&self, name: &str) -> Option<(FileOrDir, Option<String>)> {
        self.get(name).map(|n| (n, None))
    }

    /// Crée un fichier vide `name` avec les droits `perm`, du type que ce système de fichiers
    /// fabrique. `None` : le dossier n'a pas de fabrique, l'appelant choisit le type de nœud
    /// (fichiers en mémoire). Le nom ne doit pas exister.
    fn create_file(&mut self, _name: &str, _perm: u16) -> Option<Result<FileRef, &'static str>> { None }

    /// Crée un sous-dossier ; même convention que [`Directory::create_file`].
    fn create_dir(&mut self, _name: &str, _perm: u16) -> Option<Result<DirRef, &'static str>> { None }

    /// Crée un lien symbolique `name -> target` ; `None` si le système de fichiers n'en a pas.
    fn create_symlink(&mut self, _name: &str, _target: &str) -> Option<Result<FileRef, &'static str>> { None }

    /// Crée un nœud spécial (`mknod`) : tube nommé (`Fifo`), socket du domaine Unix (`Socket`),
    /// périphérique (`CharDev`/`BlockDev`, `rdev` = (majeur, mineur)). Le nœud ne porte pas de
    /// données : c'est celui qui l'ouvre qui lui donne son sens. `None` si le système de fichiers
    /// ne sait pas en créer.
    fn create_special(&mut self, _name: &str, _kind: NodeKind, _perm: u16, _rdev: (u32, u32)) -> Option<Result<FileRef, &'static str>> { None }
}

/// Noms de fichiers en octets quelconques dans des `String`.
///
/// Un nom Unix est une suite d'octets sans `/` ni `\0`, pas forcément de l'UTF-8 (PEP 383 de
/// Python, `surrogateescape`). Le VFS garde des `String` : chaque octet qui n'appartient pas à
/// une séquence UTF-8 valide devient le point de code privé U+10FF00 + octet (fin du plan 16,
/// « Supplementary Private Use Area-B »), et un vrai caractère de cette plage est lui-même
/// échappé octet par octet. `encode(decode(octets)) == octets` pour toute suite d'octets :
/// les noms passent sans perte du disque (ext4) ou de l'appelant (`open`, `getdents64`,
/// `readlink`) au VFS et retour, et un nom valide en UTF-8 reste la même chaîne.
pub mod rawname {
    use alloc::{string::String, vec::Vec};

    const BASE: u32 = 0x10FF00;

    fn escape(out: &mut String, b: u8) {
        out.push(char::from_u32(BASE + b as u32).unwrap());
    }

    fn push_valid(out: &mut String, s: &str) {
        for c in s.chars() {
            if (c as u32) >= BASE {
                let mut buf = [0u8; 4];
                for &b in c.encode_utf8(&mut buf).as_bytes() {
                    escape(out, b);
                }
            } else {
                out.push(c);
            }
        }
    }

    /// Octets → chaîne (sans perte).
    pub fn decode(mut bytes: &[u8]) -> String {
        if let Ok(s) = core::str::from_utf8(bytes) {
            if !s.chars().any(|c| (c as u32) >= BASE) {
                return String::from(s);
            }
        }
        let mut out = String::with_capacity(bytes.len());
        loop {
            match core::str::from_utf8(bytes) {
                Ok(s) => {
                    push_valid(&mut out, s);
                    return out;
                }
                Err(e) => {
                    let (good, rest) = bytes.split_at(e.valid_up_to());
                    // SAFETY : `valid_up_to` délimite un préfixe UTF-8 valide.
                    push_valid(&mut out, unsafe { core::str::from_utf8_unchecked(good) });
                    let bad = e.error_len().unwrap_or(rest.len());
                    for &b in &rest[..bad] {
                        escape(&mut out, b);
                    }
                    bytes = &rest[bad..];
                }
            }
        }
    }

    /// Chaîne → octets (inverse de [`decode`]).
    pub fn encode(s: &str) -> Vec<u8> {
        if !s.chars().any(|c| (c as u32) >= BASE) {
            return s.as_bytes().to_vec();
        }
        let mut out = Vec::with_capacity(s.len());
        for c in s.chars() {
            let v = c as u32;
            if v >= BASE {
                out.push((v - BASE) as u8);
            } else {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
        out
    }
}

/// Allows us to return a generic type that can be matched by the caller to extract the underlying type
#[derive(Clone)]
pub enum FileOrDir {
    File(FileRef),
    Dir(DirRef),
}

impl fmt::Debug for FileOrDir {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		write!(f, "{}", self.get_absolute_path())
	}
}

// Allows us to call methods directly on an enum so we don't have to match on the underlying type
impl FsNode for FileOrDir {
    
    fn get_absolute_path(&self) -> String {
        match self {
            FileOrDir::File(file) => file.lock().get_absolute_path(),
            FileOrDir::Dir(dir) => dir.lock().get_absolute_path(),
        }
    }

    fn get_name(&self) -> String {
        match self {
            FileOrDir::File(file) => file.lock().get_name(),
            FileOrDir::Dir(dir) => dir.lock().get_name(),
        }
    }

    fn get_parent_dir(&self) -> Option<DirRef> {
        match self {
            FileOrDir::File(file) => file.lock().get_parent_dir(),
            FileOrDir::Dir(dir) => dir.lock().get_parent_dir(),
        }
    }

    fn set_parent_dir(&mut self, new_parent: WeakDirRef) {
        match self {
            FileOrDir::File(file) => file.lock().set_parent_dir(new_parent),
            FileOrDir::Dir(dir) => dir.lock().set_parent_dir(new_parent),
        }
    }

    fn set_name(&mut self, name: String) -> Result<(), &'static str> {
        match self {
            FileOrDir::File(file) => file.lock().set_name(name),
            FileOrDir::Dir(dir) => dir.lock().set_name(name),
        }
    }

    fn is_movable(&self) -> bool {
        match self {
            FileOrDir::File(file) => file.lock().is_movable(),
            FileOrDir::Dir(dir) => dir.lock().is_movable(),
        }
    }

    fn metadata(&self) -> Option<Metadata> {
        match self {
            FileOrDir::File(file) => file.lock().metadata(),
            FileOrDir::Dir(dir) => dir.lock().metadata(),
        }
    }

    fn read_only(&self) -> bool {
        match self {
            FileOrDir::File(file) => file.lock().read_only(),
            FileOrDir::Dir(dir) => dir.lock().read_only(),
        }
    }

    fn set_metadata(&mut self, change: MetaChange) -> Result<(), &'static str> {
        match self {
            FileOrDir::File(file) => file.lock().set_metadata(change),
            FileOrDir::Dir(dir) => dir.lock().set_metadata(change),
        }
    }
}

impl KnownLength for FileOrDir {
    /// Returns the length (size) in bytes of this `FileOrDir`.
    /// 
    /// Directories currently return `0`.
    fn len(&self) -> usize {
        match &self {
            FileOrDir::File(f) => f.lock().len(),
            FileOrDir::Dir(_) => 0,
        }
    }
}

impl FileOrDir {
    /// Returns `true` if this is a `File`, `false` if it is a `Directory`.
    pub fn is_file(&self) -> bool {
        match &self {
            FileOrDir::File(_) => true,
            FileOrDir::Dir(_) => false,
        }
    }

    /// Returns `true` if this is a `Directory`, `false` if it is a `File`.
    pub fn is_dir(&self) -> bool {
        match &self {
            FileOrDir::File(_) => false,
            FileOrDir::Dir(_) => true,
        }
    }

    /// Cible si le nœud est un lien symbolique.
    pub fn read_link(&self) -> Option<String> {
        match &self {
            FileOrDir::File(f) => f.lock().read_link(),
            FileOrDir::Dir(_) => None,
        }
    }

    /// Type POSIX du nœud.
    pub fn kind(&self) -> NodeKind {
        match self.metadata() {
            Some(m) => m.kind,
            None if self.is_dir() => NodeKind::Dir,
            None => NodeKind::File,
        }
    }
}

/// Erreur de [`move_node`] quand un des systèmes de fichiers ne sait pas déplacer sans copie.
pub const NOT_MOVABLE: &str = "deplacement impossible sur ce systeme de fichiers (copier puis supprimer)";

/// Même répertoire (comparaison des adresses, sans les tables virtuelles).
pub fn same_dir(a: &DirRef, b: &DirRef) -> bool {
    core::ptr::eq(Arc::as_ptr(a) as *const u8, Arc::as_ptr(b) as *const u8)
}

/// `dir` est `ancestor` ou l'un de ses descendants.
pub fn is_within(dir: &DirRef, ancestor: &DirRef) -> bool {
    let mut cur = Some(dir.clone());
    // Profondeur bornée : protège d'un cycle accidentel dans les liens de parenté.
    for _ in 0..4096 {
        match cur {
            Some(d) => {
                if same_dir(&d, ancestor) {
                    return true;
                }
                cur = d.lock().get_parent_dir();
            }
            None => return false,
        }
    }
    false
}

/// Déplace et/ou renomme `node`, enfant de `from`, en `to/new_name`, sans copier son contenu
/// (comme `rename(2)` à l'intérieur d'un même système de fichiers).
///
/// Un nœud existant de même nom dans `to` est remplacé et renvoyé ; c'est à l'appelant de
/// vérifier avant les règles de remplacement qu'il veut (`EISDIR`, `ENOTEMPTY`…). En cas d'échec,
/// le nœud est remis à sa place d'origine, sous son ancien nom.
///
/// Aucun verrou n'est tenu sur deux nœuds à la fois : pas d'ordre de verrouillage à respecter.
pub fn move_node(node: &FileOrDir, from: &DirRef, to: &DirRef, new_name: &str) -> Result<Option<FileOrDir>, &'static str> {
    if new_name.is_empty() || new_name == "." || new_name == ".." || new_name.contains('/') {
        return Err("nom invalide");
    }
    if let FileOrDir::Dir(d) = node {
        if is_within(to, d) {
            return Err("impossible de deplacer un dossier dans lui-meme");
        }
    }
    let old_name = node.get_name();
    if old_name == new_name && same_dir(from, to) {
        return Ok(None);
    }
    // Les deux dossiers et le nœud doivent savoir se déplacer sans perte : sinon l'appelant
    // copie puis supprime (comme `rename(2)` qui renvoie `EXDEV` entre deux systèmes de
    // fichiers).
    if !node.is_movable() || !from.lock().is_movable() || !to.lock().is_movable() {
        return Err(NOT_MOVABLE);
    }
    let mut moved = from.lock().remove(node).ok_or("element introuvable dans son dossier")?;
    let restore = |mut n: FileOrDir| {
        let _ = n.set_name(old_name.clone());
        n.set_parent_dir(Arc::downgrade(from));
        let _ = from.lock().insert(n);
    };
    if old_name != new_name {
        if let Err(e) = moved.set_name(String::from(new_name)) {
            restore(moved);
            return Err(e);
        }
    }
    moved.set_parent_dir(Arc::downgrade(to));
    let res = to.lock().insert(moved.clone());
    match res {
        Ok(replaced) => Ok(replaced),
        Err(e) => {
            restore(moved);
            Err(e)
        }
    }
}
