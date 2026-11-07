//! `run` — Exécuter un binaire ELF Linux ou natif depuis le système de fichiers.
//!
//! Usage :
//!   run ~/doom                  — exécuter Doom depuis /disk/apps/ (ouvert dans une fenêtre MaiOS)
//!   run doom                    — recherche automatique dans les dossiers standards
//!   run /disk/apps/doom         — chemin absolu
//!   run -f ~/doom               — exécuter en plein écran exclusif
//!   run --info ~/doom           — afficher les en-têtes ELF sans exécuter
//!
//! Deux cas :
//! - **Programme C lié pour MaiOS** (tlibc, `--emit-relocs` : sections `.rela.*` et
//!   `.symtab`, comme Doom) : le chargement est délégué à `loadc`, qui relie le
//!   programme aux instances du noyau (allocateur, syscalls…). Sans cette étape, le
//!   programme utiliserait ses propres copies non initialisées et planterait au
//!   premier `malloc`.
//! - **Binaire autonome** : chargé via `elf_loader` puis exécuté avec l'ABI Linux
//!   (MEB — Mai Execution Bridge).
//!
//! Dans les deux cas, une fenêtre MaiOS est associée à la tâche du programme
//! (sauf en mode plein écran). Les arguments après le nom sont transmis au programme.

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
extern crate window;
extern crate window_inner;
extern crate shapes;
extern crate color;
extern crate time;
extern crate sleep;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use fs_node::{DirRef, FileOrDir, FileRef};
use getopts::Options;
use path::Path;
use shapes::Coord;
use window::Window;
use time::Duration;

pub fn main(args: Vec<String>) -> isize {
    let mut opts = Options::new();
    opts.optflag("h", "help", "Afficher l'aide");
    opts.optflag("i", "info", "Afficher les en-têtes ELF sans exécuter");
    opts.optflag("f", "fullscreen", "Lancer en plein écran direct sans fenêtre");
    opts.optflag("w", "window", "Lancer dans une fenêtre MaiOS (comportement par défaut)");
    opts.optflag("s", "serial", "Copier la sortie du processus dans le journal du noyau (tests)");
    // Les options placées après le programme lui appartiennent (ex. `run doom -warp 1 1`).
    opts.parsing_style(getopts::ParsingStyle::StopAtFirstFree);

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

    let raw_target = &matches.free[0];
    let prog_args: Vec<String> = matches.free[1..].to_vec();
    let show_info = matches.opt_present("i");
    let fullscreen = matches.opt_present("f");
    let cwd = match task::with_current_task(|t| t.env.lock().working_dir.clone()) {
        Ok(cwd) => cwd,
        Err(_) => {
            println!("Erreur: impossible de récupérer le répertoire courant");
            return -1;
        }
    };

    // Resoudre le fichier avec developpement du tilde '~' et recherche intelligente
    let (resolved_path, file_ref) = match resolve_executable_path(raw_target, &cwd) {
        Some(res) => res,
        None => {
            let name_only = raw_target.rsplit('/').next().unwrap_or(raw_target);
            println!("Erreur: executable '{}' non trouve", raw_target);
            println!("  Emplacements verifies :");
            println!("    - {}", raw_target);
            println!("    - /disk/apps/{}", name_only);
            println!("    - /disk/{}", name_only);
            println!("    - /extra_files/{}", name_only);
            println!("    - /namespaces/_executables/{}", name_only);
            return -1;
        }
    };

    // Nom d'affichage pour la fenêtre et la tâche
    let app_name = extract_app_name(&resolved_path);

    // Programme C lié pour MaiOS : `loadc` le relie aux symboles du noyau.
    if !show_info && has_theseus_relocations(&file_ref) {
        return run_with_loadc(&resolved_path, &app_name, prog_args, fullscreen, &cwd);
    }

    // Lire le contenu du fichier
    let mut file_locked = file_ref.lock();
    let file_size = file_locked.len();

    if file_size < 4 {
        println!("Erreur: fichier '{}' trop petit", resolved_path);
        return -1;
    }

    let mut elf_bytes = alloc::vec![0u8; file_size];
    if let Err(e) = file_locked.read_at(&mut elf_bytes, 0) {
        println!("Erreur de lecture: {:?}", e);
        return -1;
    }
    drop(file_locked);

    // Vérifier la signature ELF
    if &elf_bytes[0..4] != b"\x7fELF" {
        println!("Erreur: '{}' n'est pas un binaire ELF valide", resolved_path);
        return -1;
    }

    if show_info {
        print_elf_info(&elf_bytes, &resolved_path);
        return 0;
    }

    // Processus MaiOS en mode utilisateur (anneau 3, espace d'adresses propre) : le chemin
    // normal pour un binaire Linux autonome.
    #[cfg(target_arch = "x86_64")]
    if maiproc::available() {
        return run_process(raw_target, &resolved_path, elf_bytes, prog_args, &cwd, matches.opt_present("s"));
    }

    println!("Chargement de {} ({} octets)...", resolved_path, file_size);

    // Charger les segments PT_LOAD en mémoire
    let loaded = match elf_loader::load(&elf_bytes) {
        Ok(l) => l,
        Err(e) => {
            println!("Erreur de chargement ELF: {}", e);
            return -1;
        }
    };

    let entry = loaded.entry_point.value();
    let segments = loaded.segments;

    println!("Binaire charge (entree: {:#x}, segments: {})", entry, segments.len());

    let mut window = open_app_window(&app_name, fullscreen);

    // ── Lancement du processus ELF ────────────────────────────────────
    let task_label = format!("app_{}", app_name);

    let spawn_res = spawn::new_task_builder(move |_: ()| -> isize {
        // Definir l'ABI Linux (ExecMode::Linux = 1) pour le routage des syscalls
        let _ = task::with_current_task(|t| {
            t.0.exec_mode.store(1, core::sync::atomic::Ordering::Release);
        });

        // Garder les segments memoire alloues pendant toute l'execution
        let _keep_alive = segments;

        // Sauter au point d'entree de l'application
        let entry_fn: extern "C" fn() -> isize = unsafe {
            core::mem::transmute(entry)
        };
        entry_fn()
    }, ())
    .name(task_label)
    .spawn();

    let child_task = match spawn_res {
        Ok(t) => t,
        Err(e) => {
            println!("Erreur au lancement de la tache : {}", e);
            return -1;
        }
    };

    let child_id = child_task.id;

    // Associer la fenetre au child_task pour que SYS_MAP_FRAMEBUFFER et SYS_PRESENT la trouvent
    if let Some(ref mut win) = window {
        win.set_task_id(Some(child_id));
        let _ = win.render(None);
    }

    supervise(child_task, window, &app_name)
}

/// Lance `path` comme processus Linux en mode utilisateur et attend sa fin.
#[cfg(target_arch = "x86_64")]
fn run_process(argv0: &str, path: &str, image: Vec<u8>, args: Vec<String>, cwd: &DirRef, log_output: bool) -> isize {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(argv0.to_string());
    argv.extend(args);
    let cwd_path = cwd.lock().get_absolute_path();
    let envp = alloc::vec![
        "PATH=/disk/bin:/disk/apps:/extra_files:/bin:/usr/bin".to_string(),
        "HOME=/".to_string(),
        "USER=root".to_string(),
        "LOGNAME=root".to_string(),
        "SHELL=/disk/bin/sh".to_string(),
        "TERM=dumb".to_string(),
        "LANG=C".to_string(),
        format!("PWD={}", cwd_path),
    ];
    let req = maiproc::SpawnRequest {
        path: path.to_string(),
        image: alloc::sync::Arc::from(image.into_boxed_slice()),
        argv,
        envp,
        cwd: cwd.clone(),
        cwd_path,
        stdio: app_io::streams().ok(),
        ppid: task::get_my_current_task_id(),
        log_output,
    };
    let child = match maiproc::spawn(req) {
        Ok(c) => c,
        Err(e) => {
            println!("Erreur : impossible de lancer '{}' : {}", path, e);
            return -1;
        }
    };
    let status = match child.join() {
        Ok(task::ExitValue::Completed(s)) => s,
        Ok(task::ExitValue::Killed(reason)) => {
            println!("[{}] processus {} tue par le noyau ({:?})", path, child.id, reason);
            return -1;
        }
        Err(e) => {
            println!("Erreur : attente du processus : {}", e);
            return -1;
        }
    };
    let s = status as i32;
    if s & 0x7f != 0 {
        println!("[{}] {}", path, maiproc::describe_status(status));
        128 + (s & 0x7f) as isize
    } else {
        ((s >> 8) & 0xff) as isize
    }
}

/// Ouvre la fenêtre MaiOS de l'application (aucune en mode plein écran).
fn open_app_window(app_name: &str, fullscreen: bool) -> Option<Window> {
    if fullscreen {
        return None;
    }
    // Pour Doom et les apps graphiques, taille interne 640x400 (zoom x2 de 320x200)
    let content_w: usize = 640;
    let content_h: usize = 400;
    let win_w: usize = content_w + 2; // + bordures gauche/droite
    let win_h: usize = content_h + 33; // + barre de titre et bordure

    let win_title = format!("MaiOS - {}", app_name);
    match Window::with_title(win_title, Coord::new(120, 60), win_w, win_h, color::BLACK) {
        Ok(mut w) => {
            let _ = w.render(None);
            println!("Fenetre ouverte : {}x{} pour {}", win_w, win_h, app_name);
            Some(w)
        }
        Err(e) => {
            println!("Avertissement: mode plein ecran (fenetre indisponible: {})", e);
            None
        }
    }
}

/// Fait vivre la fenêtre jusqu'à la fin du programme, puis la ferme.
fn supervise(child: task::JoinableTaskRef, mut window: Option<Window>, app_name: &str) -> isize {
    println!("Execution de '{}' en cours (PID {})...", app_name, child.id);

    while !child.has_exited() {
        if let Some(ref mut win) = window {
            while let Ok(Some(_)) = win.handle_event() {}
        }
        let _ = sleep::sleep(Duration::from_millis(16));
    }

    // Nettoyage de la fenetre (Drop automatique de Window)
    drop(window);

    println!("'{}' s'est termine.", app_name);
    0
}

/// Lance un programme C lié pour MaiOS via l'application `loadc`, dans sa propre
/// tâche, avec la fenêtre MaiOS associée à cette tâche.
fn run_with_loadc(
    path: &str,
    app_name: &str,
    mut prog_args: Vec<String>,
    fullscreen: bool,
    cwd: &DirRef,
) -> isize {
    // Doom : fournir l'IWAD automatiquement s'il n'est pas précisé.
    if app_name.eq_ignore_ascii_case("doom") && !prog_args.iter().any(|a| a == "-iwad") {
        match find_doom_wad(path, cwd) {
            Some(wad) => {
                println!("IWAD : {}", wad);
                prog_args.push("-iwad".to_string());
                prog_args.push(wad);
            }
            None => println!("Avertissement: DOOM1.WAD introuvable (ajoutez -iwad <chemin>)"),
        }
    }

    let ns_dir = match task::with_current_task(|t| t.namespace.dir().clone()) {
        Ok(d) => d,
        Err(_) => {
            println!("Erreur: impossible de lire le namespace courant");
            return -1;
        }
    };
    let loadc_path = match ns_dir.get_file_starting_with("loadc-") {
        Some(f) => f.lock().get_absolute_path(),
        None => {
            println!("Erreur: l'application 'loadc' est introuvable");
            return -1;
        }
    };

    println!("Chargement de {} via loadc (liaison aux symboles du noyau)...", path);

    let mut loadc_args = Vec::with_capacity(prog_args.len() + 1);
    loadc_args.push(path.to_string());
    loadc_args.extend(prog_args);

    let spawn_res = mod_mgmt::create_application_namespace(None)
        .and_then(|ns| spawn::new_application_task_builder(loadc_path.as_ref(), Some(ns)))
        .and_then(|b| {
            b.argument(loadc_args)
                .name(format!("app_{}", app_name))
                .block()
                .spawn()
        });
    let child = match spawn_res {
        Ok(t) => t,
        Err(e) => {
            println!("Erreur au lancement de loadc : {}", e);
            return -1;
        }
    };

    // Le programme écrit dans le même terminal que `run`.
    if let Ok(streams) = app_io::streams() {
        app_io::insert_child_streams(child.id, streams);
    }

    // La fenêtre doit appartenir à la tâche qui exécute le programme, pour que
    // SYS_MAP_FRAMEBUFFER et SYS_PRESENT la retrouvent.
    let mut window = open_app_window(app_name, fullscreen);
    if let Some(ref mut win) = window {
        win.set_task_id(Some(child.id));
        let _ = win.render(None);
    }

    if child.unblock().is_err() {
        println!("Erreur: impossible de démarrer la tâche de '{}'", app_name);
        return -1;
    }

    supervise(child, window, app_name)
}

/// Cherche un DOOM1.WAD valide (signature `IWAD`/`PWAD`) à côté de l'exécutable
/// puis dans les dossiers standards. Un fichier invalide (ex. page HTML issue
/// d'un téléchargement raté) est ignoré au lieu de faire échouer Doom.
fn find_doom_wad(elf_path: &str, cwd: &DirRef) -> Option<String> {
    let dir = elf_path.rsplit_once('/').map(|(d, _)| d).unwrap_or(".");
    let candidates = [
        format!("{}/DOOM1.WAD", dir),
        format!("{}/doom1.wad", dir),
        "/disk/apps/DOOM1.WAD".to_string(),
        "/extra_files/DOOM1.WAD".to_string(),
    ];
    candidates.into_iter().find(|c| {
        let p: &Path = c.as_ref();
        let file = match p.get(cwd) {
            Some(FileOrDir::File(f)) => f,
            _ => return false,
        };
        let mut magic = [0u8; 4];
        let ok = file.lock().read_at(&mut magic, 0).ok() == Some(4)
            && (&magic == b"IWAD" || &magic == b"PWAD");
        if !ok {
            println!("Avertissement: {} n'est pas un WAD valide, ignore", c);
        }
        ok
    })
}

/// Vrai si l'ELF a été lié pour MaiOS avec `--emit-relocs` : il contient des
/// sections de relocation (`SHT_RELA`) et une table des symboles (`SHT_SYMTAB`),
/// que `loadc` utilise pour relier le programme aux instances du noyau.
/// Seuls l'en-tête et la table des sections sont lus, pas le fichier entier.
fn has_theseus_relocations(file: &FileRef) -> bool {
    const SHT_SYMTAB: u32 = 2;
    const SHT_RELA: u32 = 4;

    let mut f = file.lock();
    let mut hdr = [0u8; 64];
    if f.read_at(&mut hdr, 0).ok() != Some(hdr.len()) || &hdr[0..4] != b"\x7fELF" || hdr[4] != 2 {
        return false;
    }
    let shoff = u64::from_le_bytes([
        hdr[0x28], hdr[0x29], hdr[0x2A], hdr[0x2B], hdr[0x2C], hdr[0x2D], hdr[0x2E], hdr[0x2F],
    ]) as usize;
    let shentsize = u16::from_le_bytes([hdr[0x3A], hdr[0x3B]]) as usize;
    let shnum = u16::from_le_bytes([hdr[0x3C], hdr[0x3D]]) as usize;
    if shoff == 0 || shentsize < 8 || shnum == 0 || shnum > 4096 {
        return false;
    }

    let mut table = alloc::vec![0u8; shentsize * shnum];
    if f.read_at(&mut table, shoff).ok() != Some(table.len()) {
        return false;
    }
    let (mut rela, mut symtab) = (false, false);
    for sh in table.chunks_exact(shentsize) {
        match u32::from_le_bytes([sh[4], sh[5], sh[6], sh[7]]) {
            SHT_SYMTAB => symtab = true,
            SHT_RELA => rela = true,
            _ => {}
        }
    }
    rela && symtab
}

/// Résout un chemin d'exécutable avec développement du tilde `~` et recherche des extensions.
fn resolve_executable_path(raw_path: &str, cwd: &DirRef) -> Option<(String, FileRef)> {
    // 1. Développer ~ et ~/
    let expanded = if raw_path == "~" {
        "/disk/apps".to_string()
    } else if let Some(rest) = raw_path.strip_prefix("~/") {
        format!("/disk/apps/{}", rest)
    } else if let Some(rest) = raw_path.strip_prefix("~") {
        format!("/disk/apps/{}", rest)
    } else {
        raw_path.to_string()
    };

    // 2. Préparer les variantes de chemin à tester
    let mut candidates = Vec::new();

    if expanded.contains('/') || expanded.starts_with('.') {
        candidates.push(expanded.clone());
        if !expanded.ends_with(".elf") && !expanded.ends_with(".exe") {
            candidates.push(format!("{}.elf", expanded));
            candidates.push(format!("{}.exe", expanded));
        }
    } else {
        // Nom simple sans chemin, ex: "doom"
        candidates.push(format!("./{}", expanded));
        candidates.push(format!("./{}.elf", expanded));
        candidates.push(format!("/disk/apps/{}", expanded));
        candidates.push(format!("/disk/apps/{}.elf", expanded));
        candidates.push(format!("/disk/apps/{}.exe", expanded));
        candidates.push(format!("/disk/{}", expanded));
        candidates.push(format!("/disk/{}.elf", expanded));
        candidates.push(format!("/extra_files/{}", expanded));
        candidates.push(format!("/extra_files/{}.elf", expanded));
        candidates.push(format!("/namespaces/_executables/{}", expanded));
        candidates.push(format!("/namespaces/_executables/{}.elf", expanded));
    }

    // 3. Tester chaque candidat dans le VFS
    let mut found: Vec<(String, FileRef)> = Vec::new();
    for cand in candidates {
        let p: &Path = cand.as_ref();
        if let Some(FileOrDir::File(f)) = p.get(cwd) {
            found.push((cand, f));
        }
    }

    // 4. Préférer une version liée pour MaiOS (avec relocations) : une copie
    //    entièrement strippée ne peut pas être reliée au noyau.
    if let Some(i) = found.iter().position(|(_, f)| has_theseus_relocations(f)) {
        return Some(found.swap_remove(i));
    }
    found.into_iter().next()
}

/// Extrait un nom lisible d'application à partir de son chemin (ex. "/disk/apps/doom.elf" -> "Doom")
fn extract_app_name(path_str: &str) -> String {
    let filename = path_str.rsplit('/').next().unwrap_or(path_str);
    let base = filename.strip_suffix(".elf")
        .or_else(|| filename.strip_suffix(".exe"))
        .unwrap_or(filename);

    if base.is_empty() {
        return "App".to_string();
    }

    // Capitaliser la première lettre
    let mut chars = base.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
        None => "App".to_string(),
    }
}

fn print_elf_info(bytes: &[u8], name: &str) {
    println!("═══ Informations ELF : {} ═══", name);
    println!();

    let class = match bytes.get(4) {
        Some(1) => "ELF32",
        Some(2) => "ELF64",
        _ => "Inconnu",
    };

    let endian = match bytes.get(5) {
        Some(1) => "Little-endian",
        Some(2) => "Big-endian",
        _ => "Inconnu",
    };

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
    println!("  Entree       : {:#x}", entry);
    println!("  Taille       : {} octets", bytes.len());
}

const USAGE: &str = "Usage: run [OPTIONS] <fichier_elf> [ARGS...]
Charger et exécuter un binaire ELF Linux ou natif dans une fenêtre MaiOS.

Options:
  -w, --window       Exécuter dans une fenêtre MaiOS dédiée (défaut)
  -f, --fullscreen   Exécuter en plein écran exclusif direct
  -i, --info         Afficher les en-têtes ELF sans exécuter
  -h, --help         Afficher cette aide

Exemples rapides:
  run ~/doom                    Exécuter Doom (ouvre une fenêtre 640x400)
  run doom                      Recherche automatique (/disk/apps/, /extra_files/...)
  run /disk/apps/doom.elf       Chemin direct
  run -f ~/doom                 Exécuter en plein écran
  run doom -warp 1 1            Les arguments après le nom vont au programme";
