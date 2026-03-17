#![no_std]
#![cfg_attr(target_arch = "x86_64", feature(trait_alias))]

extern crate alloc;
extern crate log;

#[cfg(target_arch = "x86_64")] extern crate nvme;
#[cfg(target_arch = "x86_64")] extern crate ahci;
#[cfg(target_arch = "x86_64")] extern crate fat32;
#[cfg(target_arch = "x86_64")] extern crate block_cache;
#[cfg(target_arch = "x86_64")] extern crate storage_device;
#[cfg(target_arch = "x86_64")] extern crate storage_manager;
#[cfg(target_arch = "x86_64")] extern crate partition_table;
#[cfg(target_arch = "x86_64")] extern crate partition_device;
#[cfg(target_arch = "x86_64")] extern crate fs_node;
#[cfg(target_arch = "x86_64")] extern crate vfs_node;
#[cfg(target_arch = "x86_64")] extern crate root;

use log::*;

#[cfg(target_arch = "x86_64")]
use {
    alloc::sync::Arc,
    alloc::vec::Vec,
    alloc::format,
    mpmc::Queue,
    event_types::Event,
    memory::MemoryManagementInfo,
    memory::PhysicalAddress,
    serial_port::{SerialPortAddress, init_serial_port, take_serial_port_basic},
    spin::Mutex,
    fs_node::{DirRef, FileOrDir},
    storage_device::StorageDeviceRef,
};

#[cfg(target_arch = "x86_64")]
pub fn early_init(
    rsdp_address: Option<PhysicalAddress>,
    kernel_mmi: &mut MemoryManagementInfo,
) -> Result<(), &'static str> {
    acpi::init(rsdp_address, &mut kernel_mmi.page_table)?;
    Ok(())
}

pub fn init(
    #[cfg(target_arch = "x86_64")] key_producer: Queue<Event>,
    #[cfg(target_arch = "x86_64")] mouse_producer: Queue<Event>,
) -> Result<(), &'static str> {

    // 1. Logger
    let serial_ports   = logger::take_early_log_writers();
    let logger_writers = IntoIterator::into_iter(serial_ports)
        .flatten()
        .filter_map(|sp| serial_port::init_serial_port(sp.base_port_address(), sp))
        .cloned();
    logger::init(None, logger_writers);
    info!("Logger initialisé.");

    // 2. Serial
    #[cfg(target_arch = "x86_64")] {
        let init_sp = |spa: SerialPortAddress| {
            if let Some(sp) = take_serial_port_basic(spa) {
                init_serial_port(spa, sp);
            } else {
                console::ignore_serial_port_input(spa as u16);
            }
        };
        init_sp(SerialPortAddress::COM1);
        init_sp(SerialPortAddress::COM2);
    }

    // 3. PS/2
    #[cfg(target_arch = "x86_64")] {
        let ps2 = ps2::init()?;
        if let Some(kb) = ps2.keyboard_ref() { keyboard::init(kb, key_producer)?; }
        if let Some(m)  = ps2.mouse_ref()    { mouse::init(m, mouse_producer)?;   }
    }

    // 4. PCI scan
    for dev in pci::pci_device_iter()? { debug!("PCI: {:X?}", dev); }

    #[cfg(target_arch = "x86_64")] let mut ixgbe_devs = Vec::new();
    #[cfg(target_arch = "x86_64")] let mut disk_idx: usize = 0;

    for dev in pci::pci_device_iter()? {
        if dev.class == 0x06 { continue; }

        // NVMe
        #[cfg(target_arch = "x86_64")]
        if dev.class == nvme::NVME_PCI_CLASS && dev.subclass == nvme::NVME_PCI_SUBCLASS {
            match nvme::init_from_pci(dev) {
                Ok(Some(nvme_drive_ref)) => {
                    info!("NVMe à {:?}", dev.location);
                    let nvme_as_storage: StorageDeviceRef = nvme_drive_ref;
                    mount_storage_device(nvme_as_storage, "nvme", &mut disk_idx);
                    continue;
                }
                Ok(None)  => {}
                Err(e)    => { error!("NVMe {:?}: {}", dev.location, e); continue; }
            }
        }

        // AHCI (SATA)
        #[cfg(target_arch = "x86_64")]
        if dev.class == ahci::AHCI_PCI_CLASS && dev.subclass == ahci::AHCI_PCI_SUBCLASS {
            match ahci::init_from_pci(dev) {
                Ok(Some(ctrl_ref)) => {
                    let drives: Vec<StorageDeviceRef> = ctrl_ref.lock().devices().collect();
                    for ahci_ref in drives {
                        mount_storage_device(ahci_ref, "sata", &mut disk_idx);
                    }
                    continue;
                }
                Ok(None)  => {}
                Err(e)    => { error!("AHCI {:?}: {}", dev.location, e); continue; }
            }
        }

        // ATA legacy
        #[cfg(target_arch = "x86_64")]
        match storage_manager::init_device(dev) {
            Ok(Some(ctrl_ref)) => {
                let drives: Vec<StorageDeviceRef> = ctrl_ref.lock().devices().collect();
                for ata_ref in drives {
                    mount_storage_device(ata_ref, "ata", &mut disk_idx);
                }
                continue;
            }
            Ok(None) => {}
            Err(e)   => { error!("ATA {:?}: {}", dev, e); continue; }
        }

        // Réseau
        #[cfg(target_arch = "x86_64")]
        if dev.class == 0x02 && dev.subclass == 0x00 {
            if dev.vendor_id == e1000::INTEL_VEND && dev.device_id == e1000::E1000_DEV {
                let nic = e1000::E1000Nic::init(dev)?;
                let iface = net::register_device(nic);
                nic.lock().init_interrupts(iface)?;
                continue;
            }
            if dev.vendor_id == ixgbe::INTEL_VEND && dev.device_id == ixgbe::INTEL_82599 {
                let nic = ixgbe::IxgbeNic::init(
                    dev, dev.location, true, None, false,
                    ixgbe::RxBufferSizeKiB::Buffer2KiB, 8, 8,
                )?;
                ixgbe_devs.push(nic);
                continue;
            }
            if dev.vendor_id == mlx5::MLX_VEND
                && (dev.device_id == mlx5::CONNECTX5_DEV || dev.device_id == mlx5::CONNECTX5_EX_DEV)
            {
                mlx5::ConnectX5Nic::init(dev, 8192, 512, 9000)?;
                continue;
            }
        }

        warn!("PCI sans driver: {:X?}", dev);
    }

    #[cfg(target_arch = "x86_64")] {
        let nics = ixgbe::IXGBE_NICS.call_once(|| ixgbe_devs);
        for nic in nics.iter() { net::register_device(nic); }
        if net::get_default_interface().is_none() { warn!("Aucun réseau."); }
    }

    #[cfg(target_arch = "x86_64")]
    if disk_idx == 0 { warn!("Aucun storage device trouvé"); }

    Ok(())
}

/// Try to detect partitions on a storage device, then mount each partition
/// (or the raw device if no partition table is found) as FAT32 in the VFS.
#[cfg(target_arch = "x86_64")]
fn mount_storage_device(device: StorageDeviceRef, prefix: &str, disk_idx: &mut usize) {
    let cached_disk: StorageDeviceRef = Arc::new(Mutex::new(
        block_cache::BlockCache::new(device.clone())
    ));

    let partitions = partition_table::detect_partitions(&cached_disk);

    if partitions.is_empty() {
        // No partition table — try mounting the raw device as FAT32
        let name = format!("{}{}", prefix, *disk_idx);
        match fat32::mount_and_get_root(cached_disk, &name) {
            Ok(r)  => { info!("FAT32 sur {}", name); mount_disk_in_vfs(r, *disk_idx); }
            Err(e) => debug!("{}: pas FAT32 ({})", name, e),
        }
        *disk_idx += 1;
    } else {
        // Mount each partition individually
        for part in &partitions {
            let part_dev = partition_device::PartitionDevice::new(
                cached_disk.clone(),
                part.start_lba,
                part.size_sectors,
            );
            let name = format!("{}{}p{}", prefix, *disk_idx, part.index);
            match fat32::mount_and_get_root(part_dev, &name) {
                Ok(r)  => { info!("FAT32 sur {} ({})", name, part.name); mount_disk_in_vfs(r, *disk_idx * 10 + part.index); }
                Err(e) => debug!("{}: pas FAT32 ({})", name, e),
            }
        }
        *disk_idx += 1;
    }
}

#[cfg(target_arch = "x86_64")]
fn mount_disk_in_vfs(fat32_root: DirRef, idx: usize) {
    let vfs_root = root::get_root();

    // Lock drop AVANT appel récursif sur vfs_root
    let disks_dir: DirRef = {
        let existing = { vfs_root.lock().get("disks") };
        match existing {
            Some(FileOrDir::Dir(d)) => d,
            _ => match vfs_node::VFSDirectory::create("disks".into(), &vfs_root) {
                Ok(d)  => d,
                Err(e) => { error!("/disks: {}", e); return; }
            },
        }
    };

    fat32_root.lock().set_parent_dir(Arc::downgrade(&disks_dir));
    let result = disks_dir.lock().insert(FileOrDir::Dir(fat32_root));
    if let Err(e) = result {
        error!("mount disk{}: {:?}", idx, e);
    } else {
        info!("/disks/{} monté", idx);
    }
}