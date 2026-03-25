//! `disk_mount` — Monte automatiquement les volumes FAT32 dans le VFS MaiOS.
//!
//! Au démarrage, après l'initialisation du stockage (AHCI/ATA), ce module
//! détecte les disques disponibles, tente de monter une partition FAT32
//! et expose le volume dans le VFS racine sous `/disk`.
//!
//! Architecture inspirée des mount namespaces de Linux et du VFS de Plan 9 :
//! - Chaque périphérique de stockage est testé pour un superblock FAT32
//! - Le premier volume valide est monté en `/disk` (point de montage principal)
//! - Les volumes supplémentaires sont montés en `/disk1`, `/disk2`, etc.
//!
//! Réf. :
//! - "The Scalable Commutativity Rule" (SOSP 2013) — désambiguïsation VFS
//! - Plan 9 bind/mount semantic — montage unifié

#![no_std]

extern crate alloc;
#[macro_use]
extern crate log;

use alloc::format;
use fs_node::FileOrDir;
use root;
use storage_manager;

/// Initialise et monte tous les volumes FAT32 détectés dans le VFS.
///
/// Appelé depuis `captain::init` après les étapes de stockage et de swap.
/// Les volumes sont montés sous `/disk`, `/disk1`, `/disk2`...
///
/// Retourne le nombre de volumes montés avec succès.
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
                // Insérer le point de montage dans la racine VFS
                let root_dir = root::get_root();
                match root_dir.lock().insert(FileOrDir::Dir(fat_root)) {
                    Ok(_) => {
                        info!("disk_mount: volume FAT32 monté sur /{}", mount_name);
                        mounted += 1;
                    }
                    Err(e) => {
                        error!("disk_mount: impossible d'insérer /{} dans le VFS : {}", mount_name, e);
                    }
                }
            }
            Err(e) => {
                // Ce n'est pas une erreur fatale : le disque peut ne pas être FAT32
                // (ex. partition swap, GPT, MBR raw, etc.)
                info!("disk_mount: device {} n'est pas FAT32 ({})", idx, e);
            }
        }
    }

    if mounted == 0 {
        warn!("disk_mount: aucun volume FAT32 détecté — fonctionnement en mémoire uniquement");
    } else {
        info!("disk_mount: {} volume(s) FAT32 monté(s)", mounted);
    }

    mounted
}

/// Créer les répertoires système de base sur le premier volume FAT32.
///
/// Structure :
///   /disk/system/   — fichiers système MaiOS
///   /disk/apps/     — applications installées
///   /disk/home/     — espace utilisateur
///   /disk/tmp/      — fichiers temporaires
///
/// Cette fonction est idempotente : elle ne recrée pas les répertoires
/// qui existent déjà.
pub fn create_system_dirs() {
    let root_dir = root::get_root();
    let locked = root_dir.lock();

    // Vérifier que /disk existe
    let disk_dir = match locked.get("disk") {
        Some(FileOrDir::Dir(d)) => d,
        _ => {
            info!("disk_mount: /disk non monté, skip création répertoires système");
            return;
        }
    };
    drop(locked);

    let system_dirs = ["system", "apps", "home", "tmp"];

    for dir_name in &system_dirs {
        let locked_disk = disk_dir.lock();
        if locked_disk.get(dir_name).is_some() {
            // Répertoire existe déjà
            continue;
        }
        drop(locked_disk);

        // Créer via le trait Directory::insert avec un VFSDirectory
        let new_dir = match vfs_node::VFSDirectory::create(
            alloc::string::String::from(*dir_name),
            &disk_dir,
        ) {
            Ok(d) => d,
            Err(e) => { warn!("disk_mount: VFSDirectory::create failed for {} : {}", dir_name, e); continue; }
        };
        let mut locked_disk = disk_dir.lock();
        match locked_disk.insert(FileOrDir::Dir(new_dir)) {
            Ok(_) => info!("disk_mount: créé /disk/{}", dir_name),
            Err(e) => warn!("disk_mount: impossible de créer /disk/{} : {}", dir_name, e),
        }
    }
}
