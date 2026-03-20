//! `run` — Exécuter un binaire ELF Linux depuis le système de fichiers.
//!
//! Usage :
//!   run /disk/apps/doom          — exécuter un ELF depuis le disque
//!   run --info /disk/apps/doom   — afficher les en-têtes ELF sans exécuter
//!
//! Cette commande lit un fichier ELF stocké sur le système de fichiers,
//! le charge en mémoire via `elf_loader`, puis crée un nouveau processus
//! MaiOS avec l'ABI Linux pour l'exécution des syscalls.
//!
//! C'est le pont entre le système de fichiers persistant et l'exécution
//! de binaires Linux natifs sur MaiOS (via MEB — Mai Execution Bridge).

#![no_std]
extern crate alloc;
#[macro_use]
extern crate app_io;
extern crate task;
extern crate fs_node;
extern crate path;
extern crate elf_loader;
extern crate getopts;
extern crate memory;
extern crate scheduler;
extern crate spawn;
extern crate mod_mgmt;

use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;
use fs_node::FileOrDir;
use getopts::Options;
use path::Path;

pub fn main(args: Vec<String>) -> isize {
    let mut opts = Options::new();
    opts.optflag("h", "help", "Afficher l'aide");
    opts.optflag("i", "info", "Afficher les en-têtes ELF sans exécuter");

    let matches = match opts.parse(args) {
        Ok(m) => m,
        Err(f) => {
            println!("{}", f);
            return -1;
        }
    };

    if matches.opt_present("h") || matches.free.is_empty() {
        println!("{}", opts.usage(USAGE));
        return 0;
    }

    let file_path_str = &matches.free[0];
    let show_info = matches.opt_present("i");

    // Récupérer le répertoire de travail courant
    let cwd = match task::with_current_task(|t| t.env.lock().working_dir.clone()) {
        Ok(d) => d,
        Err(_) => {
            println!("Erreur: impossible d'obtenir le répertoire courant");
            return -1;
        }
    };

    // Résoudre le chemin du fichier
    let path: &Path = file_path_str.as_ref();
    let file_ref = match path.get(&cwd) {
        Some(FileOrDir::File(f)) => f,
        Some(FileOrDir::Dir(_)) => {
            println!("Erreur: '{}' est un répertoire", file_path_str);
            return -1;
        }
        None => {
            println!("Erreur: fichier '{}' non trouvé", file_path_str);
            println!("Astuce: utilisez un chemin absolu, ex. /disk/apps/doom");
            return -1;
        }
    };

    // Lire le contenu du fichier en mémoire
    let mut file_locked = file_ref.lock();
    let file_size = file_locked.len();

    if file_size < 4 {
        println!("Erreur: fichier trop petit pour être un binaire ELF");
        return -1;
    }

    let mut elf_bytes = alloc::vec![0u8; file_size];
    match file_locked.read_at(&mut elf_bytes, 0) {
        Ok(n) if n == file_size => {}
        Ok(n) => {
            println!("Avertissement: lu {} octets sur {} attendus", n, file_size);
        }
        Err(e) => {
            println!("Erreur de lecture: {:?}", e);
            return -1;
        }
    }
    drop(file_locked);

    // Vérifier la signature ELF
    if &elf_bytes[0..4] != b"\x7fELF" {
        println!("Erreur: '{}' n'est pas un binaire ELF valide", file_path_str);
        println!("  Signature trouvée: {:02x} {:02x} {:02x} {:02x}",
            elf_bytes[0], elf_bytes[1], elf_bytes[2], elf_bytes[3]);
        return -1;
    }

    if show_info {
        print_elf_info(&elf_bytes, file_path_str);
        return 0;
    }

    // Charger le binaire ELF via elf_loader
    println!("Chargement de {} ({} octets)...", file_path_str, file_size);

    match elf_loader::load(&elf_bytes) {
        Ok(loaded) => {
            println!("ELF chargé avec succès :");
            println!("  Point d'entrée : {:#x}", loaded.entry_point);
            println!("  Segments chargés : {}", loaded.mapped_pages.len());

            // TODO: Créer un processus MaiOS avec ABI Linux et transférer
            // l'exécution au point d'entrée.
            // Pour l'instant, on vérifie seulement que le chargement fonctionne.
            // La prochaine étape est d'intégrer avec MEB (syscall Linux)
            // pour créer un vrai processus userspace.
            println!();
            println!("Le binaire a été chargé et validé.");
            println!("L'exécution native de binaires ELF Linux sera");
            println!("disponible via MEB (Mai Execution Bridge).");
            println!();
            println!("Segments mémoire mappés — le binaire est prêt pour l'exécution.");
            0
        }
        Err(e) => {
            println!("Erreur de chargement ELF: {}", e);
            -1
        }
    }
}

fn print_elf_info(bytes: &[u8], name: &str) {
    println!("═══ Informations ELF : {} ═══", name);
    println!();

    // Classe (32/64 bits)
    let class = match bytes.get(4) {
        Some(1) => "ELF32",
        Some(2) => "ELF64",
        _ => "Inconnu",
    };

    // Endianness
    let endian = match bytes.get(5) {
        Some(1) => "Little-endian",
        Some(2) => "Big-endian",
        _ => "Inconnu",
    };

    // Type
    let elf_type = if bytes.len() >= 18 {
        let t = u16::from_le_bytes([bytes[16], bytes[17]]);
        match t {
            0 => "NONE",
            1 => "REL (Relocatable)",
            2 => "EXEC (Executable)",
            3 => "DYN (Shared/PIE)",
            4 => "CORE",
            _ => "Inconnu",
        }
    } else {
        "Trop court"
    };

    // Architecture
    let machine = if bytes.len() >= 20 {
        let m = u16::from_le_bytes([bytes[18], bytes[19]]);
        match m {
            0x03 => "x86 (i386)",
            0x3E => "x86_64 (AMD64)",
            0xB7 => "AArch64 (ARM64)",
            0x28 => "ARM",
            0xF3 => "RISC-V",
            _ => "Autre",
        }
    } else {
        "Trop court"
    };

    // Point d'entrée (64-bit)
    let entry = if bytes.len() >= 32 && class == "ELF64" {
        u64::from_le_bytes([
            bytes[24], bytes[25], bytes[26], bytes[27],
            bytes[28], bytes[29], bytes[30], bytes[31],
        ])
    } else {
        0
    };

    println!("  Format       : {}", class);
    println!("  Endianness   : {}", endian);
    println!("  Type         : {}", elf_type);
    println!("  Architecture : {}", machine);
    println!("  Entrée       : {:#x}", entry);
    println!("  Taille       : {} octets", bytes.len());
}

const USAGE: &str = "Usage: run [OPTIONS] <fichier_elf> [ARGS...]
Charger et exécuter un binaire ELF Linux depuis le système de fichiers.

Exemples:
  run /disk/apps/doom           Exécuter Doom
  run --info /disk/apps/doom    Afficher les infos ELF";
