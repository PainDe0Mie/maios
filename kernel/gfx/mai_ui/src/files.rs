//! Types de fichiers reconnus par leurs premiers octets (comme `file(1)` et `libmagic`), puis
//! par leur extension : ELF (statique, dynamique, PIE, objet), PE, scripts `#!`, WebAssembly,
//! archives, images, sons, WAD de Doom… Partagé par le gestionnaire de fichiers et le bureau.

use alloc::string::String;

use crate::icons::Icon;

/// Type reconnu d'un fichier.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileKind {
    Folder,
    Volume,
    ElfExec,
    ElfDyn,
    ElfObject,
    /// Crate d'application MaiOS (fichier objet d'un namespace d'applications).
    MaiApp,
    /// Module du noyau (crate objet d'un namespace du noyau).
    KernelModule,
    Pe,
    Script,
    Wasm,
    Text,
    Code,
    Image,
    Audio,
    Archive,
    Wad,
    Pdf,
    Binary,
    Empty,
}

impl FileKind {
    pub fn label(self) -> &'static str {
        match self {
            FileKind::Folder => "Dossier",
            FileKind::Volume => "Volume",
            FileKind::ElfExec => "Exécutable ELF",
            FileKind::ElfDyn => "ELF partagé / PIE",
            FileKind::ElfObject => "Objet ELF",
            FileKind::MaiApp => "Application MaiOS",
            FileKind::KernelModule => "Module du noyau",
            FileKind::Pe => "Exécutable Windows",
            FileKind::Script => "Script",
            FileKind::Wasm => "WebAssembly",
            FileKind::Text => "Texte",
            FileKind::Code => "Code source",
            FileKind::Image => "Image",
            FileKind::Audio => "Son",
            FileKind::Archive => "Archive",
            FileKind::Wad => "Données Doom (WAD)",
            FileKind::Pdf => "Document PDF",
            FileKind::Binary => "Données binaires",
            FileKind::Empty => "Fichier vide",
        }
    }

    pub fn icon(self) -> Icon {
        match self {
            FileKind::Folder => Icon::Folder,
            FileKind::Volume => Icon::Disk,
            FileKind::ElfExec | FileKind::ElfDyn | FileKind::Pe | FileKind::Wasm => Icon::Binary,
            FileKind::MaiApp => Icon::App,
            FileKind::KernelModule | FileKind::ElfObject => Icon::Library,
            FileKind::Script | FileKind::Code => Icon::Code,
            FileKind::Text | FileKind::Empty | FileKind::Pdf => Icon::Text,
            FileKind::Image => Icon::Image,
            FileKind::Audio => Icon::Audio,
            FileKind::Archive | FileKind::Wad => Icon::Archive,
            FileKind::Binary => Icon::File,
        }
    }

    /// Ouvrable en tant que programme.
    pub fn is_runnable(self) -> bool { matches!(self, FileKind::ElfExec | FileKind::ElfDyn | FileKind::MaiApp) }

    /// Lisible dans la visionneuse de texte.
    pub fn is_textual(self) -> bool { matches!(self, FileKind::Text | FileKind::Code | FileKind::Script | FileKind::Empty) }
}

const CODE_EXT: &[&str] = &["rs", "c", "h", "cpp", "hpp", "py", "js", "ts", "toml", "json", "asm", "s", "ld", "mk", "sh", "go", "java", "lua", "html", "css", "xml", "yaml", "yml"];
const TEXT_EXT: &[&str] = &["txt", "md", "log", "cfg", "conf", "ini", "desk", "csv", "rst"];
const IMAGE_EXT: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "ppm", "tga", "svg", "ico", "webp"];
const AUDIO_EXT: &[&str] = &["wav", "mp3", "ogg", "flac", "mid", "midi"];
const ARCHIVE_EXT: &[&str] = &["zip", "tar", "gz", "xz", "lz4", "zst", "bz2", "cpio", "7z", "iso"];

pub fn extension(name: &str) -> Option<String> {
    let dot = name.rfind('.')?;
    (dot > 0 && dot + 1 < name.len()).then(|| name[dot + 1..].to_lowercase())
}

/// Reconnaît le type d'un fichier d'après ses premiers octets, puis son extension et son
/// emplacement (crates des namespaces).
pub fn classify(name: &str, path: &str, head: &[u8], len: usize) -> FileKind {
    if len == 0 {
        return FileKind::Empty;
    }
    let ext = extension(name);
    let ext = ext.as_deref().unwrap_or("");
    if path.contains("/namespaces/") && ext == "o" {
        return if path.contains("/_applications/") { FileKind::MaiApp } else { FileKind::KernelModule };
    }
    if head.len() >= 18 && &head[..4] == b"\x7fELF" {
        // e_type (décalage 16) : 1 relogeable, 2 exécutable, 3 partagé (ou PIE).
        let le = head.get(5) == Some(&1);
        let t = if le { u16::from_le_bytes([head[16], head[17]]) } else { u16::from_be_bytes([head[16], head[17]]) };
        return match t {
            1 => FileKind::ElfObject,
            2 => FileKind::ElfExec,
            3 => FileKind::ElfDyn,
            _ => FileKind::Binary,
        };
    }
    let starts = |m: &[u8]| head.len() >= m.len() && &head[..m.len()] == m;
    if starts(b"MZ") {
        return FileKind::Pe;
    }
    if starts(b"#!") {
        return FileKind::Script;
    }
    if starts(b"\0asm") {
        return FileKind::Wasm;
    }
    if starts(b"IWAD") || starts(b"PWAD") {
        return FileKind::Wad;
    }
    if starts(b"%PDF") {
        return FileKind::Pdf;
    }
    if starts(b"\x89PNG") || starts(b"\xff\xd8\xff") || starts(b"GIF8") || starts(b"BM") || starts(b"P6") {
        return FileKind::Image;
    }
    if starts(b"RIFF") || starts(b"ID3") || starts(b"OggS") || starts(b"fLaC") {
        return FileKind::Audio;
    }
    if starts(b"PK\x03\x04") || starts(b"\x1f\x8b") || starts(b"\xfd7zXZ") || starts(b"\x04\x22\x4d\x18") || starts(b"070701") || starts(b"\x28\xb5\x2f\xfd") {
        return FileKind::Archive;
    }
    if CODE_EXT.contains(&ext) {
        return FileKind::Code;
    }
    if TEXT_EXT.contains(&ext) {
        return FileKind::Text;
    }
    if IMAGE_EXT.contains(&ext) {
        return FileKind::Image;
    }
    if AUDIO_EXT.contains(&ext) {
        return FileKind::Audio;
    }
    if ARCHIVE_EXT.contains(&ext) {
        return FileKind::Archive;
    }
    if looks_textual(head) {
        FileKind::Text
    } else {
        FileKind::Binary
    }
}

/// Heuristique de texte : UTF-8 valide (à la coupure près) et presque pas de caractères de
/// contrôle (la même idée que `file(1)` et que la détection binaire de Git : un octet nul =
/// binaire).
pub fn looks_textual(head: &[u8]) -> bool {
    if head.is_empty() {
        return true;
    }
    if head.contains(&0) {
        return false;
    }
    let valid = match core::str::from_utf8(head) {
        Ok(_) => true,
        Err(e) => e.valid_up_to() + 4 >= head.len(),
    };
    let ctrl = head.iter().filter(|&&b| b < 0x20 && b != b'\n' && b != b'\r' && b != b'\t' && b != 0x0c && b != 0x1b).count();
    valid && ctrl * 100 <= head.len()
}
