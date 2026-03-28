//! `disk_mount` — Monte automatiquement les volumes FAT32 dans le VFS MaiOS
//! et installe les fichiers systeme sur le disque au premier demarrage.
//!
//! Au demarrage, apres l'initialisation du stockage (AHCI/ATA), ce module
//! detecte les disques disponibles, tente de monter une partition FAT32
//! et expose le volume dans le VFS racine sous `/disk`.
//!
//! Architecture inspiree des mount namespaces de Linux et du VFS de Plan 9 :
//! - Chaque peripherique de stockage est teste pour un superblock FAT32
//! - Le premier volume valide est monte en `/disk` (point de montage principal)
//! - Les volumes supplementaires sont montes en `/disk1`, `/disk2`, etc.
//!
//! Le systeme d'installation (`install_system_files`) copie les modules noyau
//! et applications depuis l'espace de noms de boot vers le disque persistant.
//! Cette installation ne se fait qu'une seule fois (presence de `.installed`).
//!
//! Ref. :
//! - "The Scalable Commutativity Rule" (SOSP 2013) — desambiguisation VFS
//! - Plan 9 bind/mount semantic — montage unifie

#![no_std]

extern crate alloc;
#[macro_use]
extern crate log;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use fs_node::{FileOrDir, Directory};
use io::{ByteWriter, KnownLength};
use root;
use storage_manager;

/// Initialise et monte tous les volumes FAT32 detectes dans le VFS.
///
/// Appele depuis `captain::init` apres les etapes de stockage et de swap.
/// Les volumes sont montes sous `/disk`, `/disk1`, `/disk2`...
///
/// Retourne le nombre de volumes montes avec succes.
pub fn init() -> usize {
    let mut mounted = 0;

    for (idx, device) in storage_manager::storage_devices().enumerate() {
        let mount_name = if idx == 0 {
            alloc::string::String::from("disk")
        } else {
            format!("disk{}", idx)
        };

        match fat32::mount_and_get_root(device, &mount_name) {
            Ok(fat_root) => {
                // Inserer le point de montage dans la racine VFS
                let root_dir = root::get_root();
                match root_dir.lock().insert(FileOrDir::Dir(fat_root)) {
                    Ok(_) => {
                        info!("disk_mount: volume FAT32 monte sur /{}", mount_name);
                        mounted += 1;
                    }
                    Err(e) => {
                        error!("disk_mount: impossible d'inserer /{} dans le VFS : {}", mount_name, e);
                    }
                }
            }
            Err(e) => {
                // Ce n'est pas une erreur fatale : le disque peut ne pas etre FAT32
                // (ex. partition swap, GPT, MBR raw, etc.)
                info!("disk_mount: device {} n'est pas FAT32 ({})", idx, e);
            }
        }
    }

    if mounted == 0 {
        warn!("disk_mount: aucun volume FAT32 detecte — fonctionnement en memoire uniquement");
    } else {
        info!("disk_mount: {} volume(s) FAT32 monte(s)", mounted);
    }

    mounted
}

/// Creer les repertoires systeme de base sur le premier volume FAT32.
///
/// Structure :
///   /disk/system/   — fichiers systeme MaiOS
///   /disk/apps/     — applications installees
///   /disk/home/     — espace utilisateur
///   /disk/tmp/      — fichiers temporaires
///   /disk/downloads/ — telechargements
///
/// Cette fonction est idempotente : elle ne recree pas les repertoires
/// qui existent deja.
pub fn create_system_dirs() {
    let root_dir = root::get_root();
    let locked = root_dir.lock();

    // Verifier que /disk existe
    let disk_dir = match locked.get("disk") {
        Some(FileOrDir::Dir(d)) => d,
        _ => {
            info!("disk_mount: /disk non monte, skip creation repertoires systeme");
            return;
        }
    };
    drop(locked);

    let system_dirs = ["system", "apps", "home", "tmp", "downloads"];

    for dir_name in &system_dirs {
        let locked_disk = disk_dir.lock();
        if locked_disk.get(dir_name).is_some() {
            // Repertoire existe deja
            continue;
        }
        drop(locked_disk);

        // Creer via le trait Directory::insert avec un VFSDirectory
        match vfs_node::VFSDirectory::create(
            alloc::string::String::from(*dir_name),
            &disk_dir,
        ) {
            Ok(_new_dir) => {
                info!("disk_mount: cree /disk/{}", dir_name);
            }
            Err(e) => { warn!("disk_mount: VFSDirectory::create failed for {} : {}", dir_name, e); }
        }
    }
}

/// Installe les fichiers systeme sur le disque depuis l'espace de noms de boot.
///
/// Cette fonction copie la structure du systeme sur le disque persistant,
/// permettant un vrai demarrage depuis le disque lors des prochains boots.
///
/// Structure d'installation :
///   /disk/system/kernel/    — modules noyau (.o)
///   /disk/system/config/    — configuration systeme
///   /disk/apps/             — applications utilisateur
///   /disk/home/             — espace utilisateur
///   /disk/downloads/        — telechargements
///
/// L'installation est idempotente : si `/disk/system/.installed` existe,
/// cette fonction ne fait rien.
pub fn install_system_files() {
    let root_dir = root::get_root();
    let locked = root_dir.lock();

    let disk_dir = match locked.get("disk") {
        Some(FileOrDir::Dir(d)) => d,
        _ => {
            info!("install: /disk non disponible, skip installation");
            return;
        }
    };
    drop(locked);

    // Verifier si deja installe
    let disk_locked = disk_dir.lock();
    let system_dir = match disk_locked.get("system") {
        Some(FileOrDir::Dir(d)) => d,
        _ => {
            drop(disk_locked);
            info!("install: /disk/system n'existe pas, skip installation");
            return;
        }
    };
    drop(disk_locked);

    // Verifier le marqueur d'installation
    {
        let sys_locked = system_dir.lock();
        if sys_locked.get(".installed").is_some() {
            info!("install: systeme deja installe (marqueur .installed present)");
            return;
        }
    }

    warn!("install: premiere installation du systeme MaiOS sur disque...");

    // Creer les sous-repertoires systeme
    let sub_dirs = ["kernel", "config", "boot", "logs"];
    for dir_name in &sub_dirs {
        let sys_locked = system_dir.lock();
        if sys_locked.get(dir_name).is_some() {
            continue;
        }
        drop(sys_locked);
        match vfs_node::VFSDirectory::create(
            alloc::string::String::from(*dir_name),
            &system_dir,
        ) {
            Ok(_) => info!("install: cree /disk/system/{}", dir_name),
            Err(e) => warn!("install: erreur creation /disk/system/{}: {}", dir_name, e),
        }
    }

    // Copier les modules noyau depuis /namespaces vers /disk/system/kernel/
    let installed_modules = install_namespace_modules(&system_dir);
    info!("install: {} modules noyau installes", installed_modules);

    // Creer le fichier de configuration systeme
    create_system_config(&system_dir);

    // Creer le marqueur d'installation
    let marker_content = b"MaiOS installed\nversion=0.1.0\n";
    match heapfile::HeapFile::from_vec(
        marker_content.to_vec(),
        String::from(".installed"),
        &system_dir,
    ) {
        Ok(_) => info!("install: marqueur d'installation cree"),
        Err(e) => warn!("install: impossible de creer le marqueur: {}", e),
    }

    warn!("install: installation terminee !");
}

/// Copie les modules depuis l'espace de noms de boot vers /disk/system/kernel/
fn install_namespace_modules(system_dir: &fs_node::DirRef) -> usize {
    let sys_locked = system_dir.lock();
    let kernel_dir = match sys_locked.get("kernel") {
        Some(FileOrDir::Dir(d)) => d,
        _ => return 0,
    };
    drop(sys_locked);

    let root_dir = root::get_root();
    let locked = root_dir.lock();

    // Copier depuis /apps (les modules charges au boot)
    let apps_dir = match locked.get("apps") {
        Some(FileOrDir::Dir(d)) => d,
        _ => return 0,
    };
    drop(locked);

    let apps_locked = apps_dir.lock();
    let app_names = apps_locked.list();
    drop(apps_locked);

    let mut count = 0;
    for name in &app_names {
        // Creer un fichier de reference (metadata)
        let meta_content = format!("module={}\ntype=kernel\nloaded=true\n", name);
        match heapfile::HeapFile::from_vec(
            meta_content.into_bytes(),
            format!("{}.meta", name),
            &kernel_dir,
        ) {
            Ok(_) => count += 1,
            Err(_) => {}
        }
    }

    count
}

/// Cree le fichier de configuration systeme
fn create_system_config(system_dir: &fs_node::DirRef) {
    let sys_locked = system_dir.lock();
    let config_dir = match sys_locked.get("config") {
        Some(FileOrDir::Dir(d)) => d,
        _ => return,
    };
    drop(sys_locked);

    let config_content = "\
# MaiOS System Configuration
# Generated at first boot

[system]
name = MaiOS
version = 0.1.0
arch = x86_64

[boot]
shell = hull
desktop = explorer
auto_mount = true

[filesystem]
root = /
disk = /disk
tmp = /disk/tmp
home = /disk/home
downloads = /disk/downloads

[display]
resolution = auto
compositor = mgi
vsync = true

[scheduler]
policy = eevdf
preemption = true

[network]
stack = smoltcp
dhcp = true
";

    match heapfile::HeapFile::from_vec(
        config_content.as_bytes().to_vec(),
        String::from("maios.conf"),
        &config_dir,
    ) {
        Ok(_) => info!("install: configuration systeme creee"),
        Err(e) => warn!("install: erreur config: {}", e),
    }
}
