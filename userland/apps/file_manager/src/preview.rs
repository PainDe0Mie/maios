//! Aperçu du fichier sélectionné : texte (premières lignes), vidage hexadécimal, résumé ELF
//! (classe, type, machine, entrée, interpréteur, segments, sections), résumé de WAD Doom,
//! script, dossier (taille récursive).

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use fs_node::FileOrDir;
use mai_ui::kit::human_bytes;

use crate::fsops::{self, FileKind};

/// Contenu d'aperçu.
pub enum Preview {
    None,
    /// Lignes de texte (tronquées).
    Text { lines: Vec<String>, truncated: bool },
    /// Paires clé / valeur (résumés) suivies d'un vidage.
    Info { fields: Vec<(String, String)>, hex: Vec<String> },
}

/// Construit l'aperçu de `path`.
pub fn build(path: &str, kind: FileKind) -> Preview {
    let Some(node) = fsops::lookup(path) else { return Preview::None };
    match node {
        FileOrDir::Dir(d) => {
            let (bytes, files, dirs, cut) = fsops::tree_size_budget(&d, 20_000, 40_000_000);
            let mut fields = alloc::vec![
                ("Fichiers".to_string(), format!("{}", files)),
                ("Sous-dossiers".to_string(), format!("{}", dirs)),
                ("Taille totale".to_string(), format!("{}{}", human_bytes(bytes), if cut { " (au moins)" } else { "" })),
            ];
            let direct = d.lock().list().len();
            fields.insert(0, ("Éléments directs".to_string(), format!("{}", direct)));
            Preview::Info { fields, hex: Vec::new() }
        }
        FileOrDir::File(f) => {
            let len = f.lock().len();
            let head = fsops::read_head(&f, 64 * 1024);
            if kind.is_textual() {
                let text = String::from_utf8_lossy(&head);
                let mut lines: Vec<String> = text.lines().take(400).map(|l| l.replace('\t', "    ")).collect();
                if lines.is_empty() {
                    lines.push(String::new());
                }
                return Preview::Text { lines, truncated: len > head.len() };
            }
            let mut fields = alloc::vec![("Taille".to_string(), format!("{} ({} octets)", human_bytes(len as u64), len))];
            match kind {
                FileKind::ElfExec | FileKind::ElfDyn | FileKind::ElfObject | FileKind::MaiApp | FileKind::KernelModule => {
                    elf_fields(&head, &mut fields)
                }
                FileKind::Wad => wad_fields(&head, &mut fields),
                FileKind::Pe => fields.push(("Format".into(), "PE/COFF (Windows)".into())),
                _ => {}
            }
            Preview::Info { fields, hex: hexdump(&head[..head.len().min(256)]) }
        }
    }
}

fn hexdump(b: &[u8]) -> Vec<String> {
    b.chunks(8)
        .enumerate()
        .map(|(i, c)| {
            let mut s = format!("{:04x}  ", i * 8);
            for x in c {
                s.push_str(&format!("{:02x} ", x));
            }
            for _ in c.len()..8 {
                s.push_str("   ");
            }
            s.push(' ');
            for &x in c {
                s.push(if (0x20..0x7f).contains(&x) { x as char } else { '.' });
            }
            s
        })
        .collect()
}

fn rd16(b: &[u8], o: usize, le: bool) -> u64 {
    if o + 2 > b.len() {
        return 0;
    }
    let v = [b[o], b[o + 1]];
    (if le { u16::from_le_bytes(v) } else { u16::from_be_bytes(v) }) as u64
}
fn rd32(b: &[u8], o: usize, le: bool) -> u64 {
    if o + 4 > b.len() {
        return 0;
    }
    let v = [b[o], b[o + 1], b[o + 2], b[o + 3]];
    (if le { u32::from_le_bytes(v) } else { u32::from_be_bytes(v) }) as u64
}
fn rd64(b: &[u8], o: usize, le: bool) -> u64 {
    if o + 8 > b.len() {
        return 0;
    }
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[o..o + 8]);
    if le { u64::from_le_bytes(v) } else { u64::from_be_bytes(v) }
}

fn machine_name(m: u64) -> &'static str {
    match m {
        0x03 => "x86 (i386)",
        0x3e => "x86-64",
        0xb7 => "AArch64",
        0x28 => "ARM",
        0xf3 => "RISC-V",
        0x08 => "MIPS",
        0x14 | 0x15 => "PowerPC",
        _ => "autre",
    }
}

fn elf_fields(b: &[u8], out: &mut Vec<(String, String)>) {
    if b.len() < 52 || &b[..4] != b"\x7fELF" {
        return;
    }
    let is64 = b[4] == 2;
    let le = b[5] == 1;
    let os = match b[7] {
        0 => "System V",
        3 => "Linux (GNU)",
        9 => "FreeBSD",
        _ => "autre",
    };
    let et = rd16(b, 16, le);
    let em = rd16(b, 18, le);
    let (entry, phoff, shoff, phentsize, phnum, shnum) = if is64 {
        (rd64(b, 24, le), rd64(b, 32, le), rd64(b, 40, le), rd16(b, 54, le), rd16(b, 56, le), rd16(b, 60, le))
    } else {
        (rd32(b, 24, le), rd32(b, 28, le), rd32(b, 32, le), rd16(b, 42, le), rd16(b, 44, le), rd16(b, 48, le))
    };
    let _ = shoff;
    out.push(("Format".into(), format!("ELF {} bits, {}", if is64 { 64 } else { 32 }, if le { "petit-boutiste" } else { "gros-boutiste" })));
    out.push(("Machine".into(), machine_name(em).to_string()));
    out.push(("ABI".into(), os.to_string()));
    // Programme d'en-têtes : interpréteur (dynamique) et PIE.
    let mut interp = None;
    let mut dynamic = false;
    let mut load_segments = 0;
    for i in 0..phnum.min(64) {
        let o = (phoff + i * phentsize) as usize;
        let ptype = rd32(b, o, le);
        match ptype {
            1 => load_segments += 1,
            2 => dynamic = true,
            3 => {
                let (off, sz) = if is64 { (rd64(b, o + 8, le), rd64(b, o + 32, le)) } else { (rd32(b, o + 4, le), rd32(b, o + 16, le)) };
                let (off, sz) = (off as usize, sz as usize);
                if off + sz <= b.len() && sz > 1 {
                    interp = Some(String::from_utf8_lossy(&b[off..off + sz - 1]).into_owned());
                }
            }
            _ => {}
        }
    }
    let kind = match et {
        1 => "objet relogeable".to_string(),
        2 => "exécutable".to_string(),
        3 if interp.is_some() || !dynamic => "exécutable indépendant de la position (PIE)".to_string(),
        3 => "bibliothèque partagée".to_string(),
        4 => "image mémoire (core)".to_string(),
        t => format!("type {}", t),
    };
    out.push(("Type".into(), kind));
    if et == 2 || et == 3 {
        out.push(("Point d'entrée".into(), format!("{:#x}", entry)));
        out.push(("Liaison".into(), if dynamic || interp.is_some() { "dynamique".into() } else { "statique".to_string() }));
    }
    if let Some(i) = interp {
        out.push(("Interpréteur".into(), i));
    }
    out.push(("Segments / sections".into(), format!("{} ({} chargés) / {}", phnum, load_segments, shnum)));
}

fn wad_fields(b: &[u8], out: &mut Vec<(String, String)>) {
    if b.len() < 12 {
        return;
    }
    let kind = if &b[..4] == b"IWAD" { "IWAD (jeu complet)" } else { "PWAD (extension)" };
    out.push(("Format".into(), kind.into()));
    out.push(("Ressources (lumps)".into(), format!("{}", rd32(b, 4, true))));
    out.push(("Ouverture".into(), "Entrée : lancer Doom avec ce WAD".into()));
}
