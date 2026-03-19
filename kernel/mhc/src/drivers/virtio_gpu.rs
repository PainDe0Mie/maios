//! MHC VirtIO-GPU Driver — Real GPU driver for QEMU/KVM.
//!
//! ## VirtIO-GPU Specification
//!
//! Implements VirtIO GPU Device (device type 16) per the VirtIO 1.2 spec:
//! - PCI vendor 0x1AF4, device 0x1050 (transitional) or 0x1040+16=0x1050 (modern)
//! - Two virtqueues: controlq (index 0) and cursorq (index 1)
//! - Supports 2D (scanout, transfer, resource management)
//! - Optionally supports 3D via virgl (OpenGL) or venus (Vulkan)
//!
//! ## Initialization sequence
//!
//! Per VirtIO 1.2 §3.1.1 (Driver Requirements: Device Initialization):
//!   1. Reset device (write 0 to device_status)
//!   2. Write ACKNOWLEDGE to device_status
//!   3. Write ACKNOWLEDGE|DRIVER to device_status
//!   4. Read device features; write driver features
//!   5. Write FEATURES_OK; re-read to confirm it is still set
//!   6. Set up virtqueues
//!   7. Write DRIVER_OK
//!
//! ## References
//!
//! - VirtIO Spec 1.2, §4.1 (PCI Transport), §5.7 (GPU Device)
//! - QEMU virtio-gpu sources: hw/display/virtio-gpu.c

#![allow(dead_code)]

use alloc::sync::Arc;
use spin::Mutex;

use crate::command::CommandBuffer;
use crate::device::*;
use crate::fence::{FenceId, FencePool};
use crate::memory::{GpuAddress, GpuAllocation, GpuMemFlags};
use crate::shader::ShaderModule;

// ---------------------------------------------------------------------------
// VirtIO-GPU protocol constants
// ---------------------------------------------------------------------------

/// VirtIO-GPU command types (spec §5.7.6.7).
#[repr(u32)]
#[derive(Clone, Copy, Debug)]
pub enum VirtioGpuCmd {
    GetDisplayInfo        = 0x0100,
    ResourceCreate2d      = 0x0101,
    ResourceUnref         = 0x0102,
    SetScanout            = 0x0103,
    ResourceFlush         = 0x0104,
    TransferToHost2d      = 0x0105,
    ResourceAttachBacking = 0x0106,
    ResourceDetachBacking = 0x0107,
    GetCapsetInfo         = 0x0108,
    GetCapset             = 0x0109,
    GetEdid               = 0x010A,
    CtxCreate             = 0x0200,
    CtxDestroy            = 0x0201,
    CtxAttachResource     = 0x0202,
    CtxDetachResource     = 0x0203,
    ResourceCreate3d      = 0x0204,
    TransferToHost3d      = 0x0205,
    TransferFromHost3d    = 0x0206,
    SubmitCmd3d           = 0x0207,
    ResourceMapBlob       = 0x0208,
    ResourceUnmapBlob     = 0x0209,
    UpdateCursor          = 0x0300,
    MoveCursor            = 0x0301,
}

/// VirtIO-GPU response types.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VirtioGpuResp {
    OkNodata             = 0x1100,
    OkDisplayInfo        = 0x1101,
    OkCapsetInfo         = 0x1102,
    OkCapset             = 0x1103,
    OkEdid               = 0x1104,
    OkResourceUuid       = 0x1105,
    OkMapInfo            = 0x1106,
    ErrUnspec            = 0x1200,
    ErrOutOfMemory       = 0x1201,
    ErrInvalidScanoutId  = 0x1202,
    ErrInvalidResourceId = 0x1203,
    ErrInvalidContextId  = 0x1204,
    ErrInvalidParameter  = 0x1205,
}

/// VirtIO-GPU pixel formats.
#[repr(u32)]
#[derive(Clone, Copy, Debug)]
pub enum VirtioGpuFormat {
    B8G8R8A8Unorm = 1,
    B8G8R8X8Unorm = 2,
    A8R8G8B8Unorm = 3,
    X8R8G8B8Unorm = 4,
    R8G8B8A8Unorm = 67,
    X8B8G8R8Unorm = 68,
    A8B8G8R8Unorm = 121,
    R8G8B8X8Unorm = 134,
}

// ---------------------------------------------------------------------------
// VirtIO device status bits (spec §2.1)
// ---------------------------------------------------------------------------

const VIRTIO_STATUS_ACKNOWLEDGE:        u8 = 1;
const VIRTIO_STATUS_DRIVER:             u8 = 2;
const VIRTIO_STATUS_DRIVER_OK:          u8 = 4;
const VIRTIO_STATUS_FEATURES_OK:        u8 = 8;
const VIRTIO_STATUS_DEVICE_NEEDS_RESET: u8 = 64;
const VIRTIO_STATUS_FAILED:             u8 = 128;

// VirtIO GPU feature bits (spec §5.7.3)
const VIRTIO_GPU_F_VIRGL:          u32 = 1 << 0;
const VIRTIO_GPU_F_EDID:           u32 = 1 << 1;
const VIRTIO_GPU_F_RESOURCE_UUID:  u32 = 1 << 2;
const VIRTIO_GPU_F_RESOURCE_BLOB:  u32 = 1 << 3;

// Standard VirtIO feature bit (spec §6)
const VIRTIO_F_VERSION_1: u32 = 1; // bit 32 → high feature word bit 0

// Virtqueue descriptor flags (spec §2.7.5)
const VIRTQ_DESC_F_NEXT:  u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

// VirtIO PCI common config register offsets (spec §4.1.4.3)
const OFF_DEVICE_FEATURE_SEL: usize = 0x00;
const OFF_DEVICE_FEATURE:     usize = 0x04;
const OFF_DRIVER_FEATURE_SEL: usize = 0x08;
const OFF_DRIVER_FEATURE:     usize = 0x0c;
const OFF_CONFIG_MSIX_VEC:    usize = 0x10;
const OFF_NUM_QUEUES:         usize = 0x12;
const OFF_DEVICE_STATUS:      usize = 0x14;
const OFF_CONFIG_GEN:         usize = 0x15;
const OFF_QUEUE_SELECT:       usize = 0x16;
const OFF_QUEUE_SIZE:         usize = 0x18;
const OFF_QUEUE_MSIX_VEC:     usize = 0x1a;
const OFF_QUEUE_ENABLE:       usize = 0x1c;
const OFF_QUEUE_NOTIFY_OFF:   usize = 0x1e;
const OFF_QUEUE_DESC:         usize = 0x20;
const OFF_QUEUE_DRIVER:       usize = 0x28;
const OFF_QUEUE_DEVICE:       usize = 0x30;

// Descriptor table entry size in bytes
const VIRTQ_DESC_SIZE: usize = 16;

// ---------------------------------------------------------------------------
// VirtIO-GPU wire structures (repr(C) for direct memory mapping)
// ---------------------------------------------------------------------------

#[repr(C)]
struct VirtioGpuCtrlHdr {
    type_:    u32,
    flags:    u32,
    fence_id: u64,
    ctx_id:   u32,
    ring_idx: u8,
    _pad:     [u8; 3],
}

#[repr(C)]
#[derive(Copy, Clone)]
struct VirtioGpuRect {
    x: u32, y: u32, width: u32, height: u32,
}

#[repr(C)]
struct VirtioGpuDisplayOne {
    r:       VirtioGpuRect,
    enabled: u32,
    flags:   u32,
}

#[repr(C)]
struct VirtioGpuRespDisplayInfo {
    hdr:    VirtioGpuCtrlHdr,
    pmodes: [VirtioGpuDisplayOne; 16],
}
#[repr(C)]
struct VirtioGpuResourceCreate2d {
    hdr:         VirtioGpuCtrlHdr,
    resource_id: u32,
    format:      u32,
    width:       u32,
    height:      u32,
}

#[repr(C)]
struct VirtioGpuMemEntry {
    addr:    u64,
    length:  u32,
    padding: u32,
}

#[repr(C)]
struct VirtioGpuResourceAttachBacking {
    hdr:         VirtioGpuCtrlHdr,
    resource_id: u32,
    nr_entries:  u32,
    entries:     [VirtioGpuMemEntry; 1],
}

#[repr(C)]
struct VirtioGpuSetScanout {
    hdr:         VirtioGpuCtrlHdr,
    r:           VirtioGpuRect,
    scanout_id:  u32,
    resource_id: u32,
}

#[repr(C)]
struct VirtioGpuTransferToHost2d {
    hdr:         VirtioGpuCtrlHdr,
    r:           VirtioGpuRect,
    offset:      u64,
    resource_id: u32,
    padding:     u32,
}

#[repr(C)]
struct VirtioGpuResourceFlush {
    hdr:         VirtioGpuCtrlHdr,
    r:           VirtioGpuRect,
    resource_id: u32,
    padding:     u32,
}

#[repr(C)]
struct VirtioGpuRespOkNodata {
    hdr: VirtioGpuCtrlHdr,
}

/// MHC-managed VirtIO-GPU display resource.
pub struct VirtioScanout {
    /// Resource ID registered with the VirtIO-GPU device.
    pub resource_id: u32,
    /// Virtual address of the DMA pixel backing buffer (BGRA8888).
    pub backing_va:  usize,
    /// Physical address of the DMA pixel backing buffer.
    pub backing_pa:  u64,
    /// Scanout width in pixels.
    pub width:  u32,
    /// Scanout height in pixels.
    pub height: u32,
    /// Pre-allocated DMA command buffer (virtual address).
    cmd_va:  usize,
    /// Pre-allocated DMA command buffer (physical address).
    cmd_pa:  u64,
    /// Size of the pre-allocated command buffer.
    cmd_len: usize,
}



// ---------------------------------------------------------------------------
// Hardware state (kept alive after probe)
// ---------------------------------------------------------------------------

/// Live hardware state for an initialized VirtIO-GPU device.
///
/// All pointer-like fields are stored as `usize` (virtual addresses) so that
/// the struct is `Send + Sync` without unsafe impls.  The underlying memory
/// regions are intentionally leaked (they must live as long as the device).
struct VirtioHwState {
    /// Virtual address of the VirtIO common-config register page.
    common_cfg_va: usize,
    /// Virtual address of the notify doorbell region (0 = unavailable).
    notify_va: usize,
    /// Per-queue multiplier for the notify offset (spec §4.1.4.4).
    notify_multiplier: u32,

    /// Controlq: virtual address of the descriptor table.
    desc_va:   usize,
    /// Controlq: virtual address of the driver (available) ring.
    avail_va:  usize,
    /// Controlq: virtual address of the device (used) ring.
    used_va:   usize,
    /// Controlq: physical address of the descriptor table.
    desc_pa:   u64,
    /// Controlq: physical address of the driver ring.
    avail_pa:  u64,
    /// Controlq: physical address of the device ring.
    used_pa:   u64,

    /// Queue size in use.
    q_size: u16,
    /// Next available ring index (driver side, wraps freely).
    avail_idx: u16,
    /// Last consumed used ring index (driver side).
    last_used_idx: u16,
    /// Simple bump allocator for descriptor indices.
    next_desc: u16,
    /// Notify queue offset (used for the doorbell calculation).
    notify_queue_off: u16,
}

// ---------------------------------------------------------------------------
// VirtIO-GPU device
// ---------------------------------------------------------------------------

/// VirtIO-GPU device driver.
///
/// After a successful `probe()` the device is fully operational and
/// registered in the MHC device registry.
pub struct VirtioGpuDevice {
    caps:         GpuCapabilities,
    fences:       FencePool,
    initialized:  bool,
    num_scanouts: u32,
    has_virgl:    bool,
    has_blob:     bool,
    /// Mutable hardware state (behind a Mutex for Send + Sync).
    hw: Mutex<Option<VirtioHwState>>,
}

impl VirtioGpuDevice {
    /// Create a new (not yet probed) VirtIO-GPU device instance.
    pub fn new() -> Self {
        VirtioGpuDevice {
            caps: GpuCapabilities {
                max_workgroup_size:          [256, 256, 64],
                max_workgroup_invocations:   256,
                max_shared_memory:           32768,
                supports_compute:            false,
                supports_graphics:           true,
                supports_unified_memory:     false,
                max_queues:                  2,
                shader_formats:              ShaderFormats::empty(),
                compute_units:               0,
                device_memory_bytes:         0,
            },
            fences:       FencePool::new(1),
            initialized:  false,
            num_scanouts: 0,
            has_virgl:    false,
            has_blob:     false,
            hw:           Mutex::new(None),
        }
    }

    // -----------------------------------------------------------------------
    // MMIO register helpers
    // -----------------------------------------------------------------------

    #[inline(always)]
    unsafe fn r8(base: usize, off: usize) -> u8 {
        core::ptr::read_volatile((base + off) as *const u8)
    }
    #[inline(always)]
    unsafe fn w8(base: usize, off: usize, v: u8) {
        core::ptr::write_volatile((base + off) as *mut u8, v);
    }
    #[inline(always)]
    unsafe fn r16(base: usize, off: usize) -> u16 {
        core::ptr::read_volatile((base + off) as *const u16)
    }
    #[inline(always)]
    unsafe fn w16(base: usize, off: usize, v: u16) {
        core::ptr::write_volatile((base + off) as *mut u16, v);
    }
    #[inline(always)]
    unsafe fn r32(base: usize, off: usize) -> u32 {
        core::ptr::read_volatile((base + off) as *const u32)
    }
    #[inline(always)]
    unsafe fn w32(base: usize, off: usize, v: u32) {
        core::ptr::write_volatile((base + off) as *mut u32, v);
    }
    #[inline(always)]
    unsafe fn w64(base: usize, off: usize, v: u64) {
        core::ptr::write_volatile((base + off) as *mut u64, v);
    }

    // -----------------------------------------------------------------------
    // Virtqueue helpers
    // -----------------------------------------------------------------------

    /// Write one descriptor to the descriptor table.
    ///
    /// # Safety
    /// `desc_va` must point to a valid, writable descriptor table with at
    /// least `idx+1` entries.
    unsafe fn write_desc(
        desc_va: usize,
        idx:     u16,
        addr:    u64,
        len:     u32,
        flags:   u16,
        next:    u16,
    ) {
        let ptr = (desc_va + idx as usize * VIRTQ_DESC_SIZE) as *mut u64;
        // addr (8 bytes), len (4 bytes), flags (2 bytes), next (2 bytes)
        ptr.write_volatile(addr);
        let len_flags_next = (ptr as *mut u32).add(2);
        len_flags_next.write_volatile(len);
        let flags_next = (ptr as *mut u16).add(6);
        flags_next.write_volatile(flags);
        flags_next.add(1).write_volatile(next);
    }

    /// Append a descriptor head to the available ring and advance avail_idx.
    ///
    /// # Safety
    /// `avail_va` must point to a valid available ring for a queue of `q_size`.
    unsafe fn push_avail(avail_va: usize, avail_idx: u16, q_size: u16, desc_head: u16) {
        // Available ring layout: flags(u16), idx(u16), ring[q_size](u16), ...
        let slot = (avail_idx % q_size) as usize;
        let ring_ptr = (avail_va + 4 + slot * 2) as *mut u16;
        ring_ptr.write_volatile(desc_head);
        // Memory barrier before updating idx
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        let idx_ptr = (avail_va + 2) as *mut u16;
        idx_ptr.write_volatile(avail_idx.wrapping_add(1));
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    }

    /// Poll the used ring for a new entry. Returns `(desc_head, bytes_written)`.
    ///
    /// # Safety
    /// `used_va` must point to a valid device (used) ring.
    unsafe fn poll_used(used_va: usize, last_used: u16, q_size: u16) -> Option<(u16, u32)> {
        // Used ring layout: flags(u16), idx(u16), ring[q_size * {id(u32),len(u32)}]
        let device_idx = (used_va + 2) as *const u16;
        let dev_idx = core::ptr::read_volatile(device_idx);
        if dev_idx == last_used {
            return None;
        }
        let slot = (last_used % q_size) as usize;
        let entry_ptr = (used_va + 4 + slot * 8) as *const u32;
        let id  = entry_ptr.read_volatile() as u16;
        let len = entry_ptr.add(1).read_volatile();
        Some((id, len))
    }

    // -----------------------------------------------------------------------
    // Probe
    // -----------------------------------------------------------------------

    /// Probe for a VirtIO-GPU device on the PCI bus and initialize it.
    ///
    /// Returns `Ok(())` if a VirtIO-GPU was found and made operational.
    pub fn probe(&mut self) -> Result<(), &'static str> {
        // -------------------------------------------------------------------
        // Step 1: Enumerate PCI — find VirtIO-GPU
        // -------------------------------------------------------------------
        let device = pci::pci_device_iter()?
            .find(|d| {
                d.vendor_id == 0x1AF4
                    && (d.device_id == 0x1050 || d.device_id == 0x1040 + 16)
            })
            .ok_or("MHC/VirtIO-GPU: no VirtIO-GPU PCI device found")?;

        log::info!("MHC/VirtIO-GPU: found at PCI {}", device.location);

        // Enable bus-mastering so the device can DMA.
        device.pci_set_command_bus_master_bit();

        // -------------------------------------------------------------------
        // Step 2: Parse VirtIO PCI capabilities
        // -------------------------------------------------------------------
        let virtio_caps = device.location.get_virtio_caps();
        if virtio_caps.is_empty() {
            return Err("MHC/VirtIO-GPU: no VirtIO PCI capabilities found (not a modern device?)");
        }

        let common_cap = virtio_caps
            .iter()
            .find(|c| c.cfg_type == pci::VIRTIO_PCI_CAP_COMMON_CFG)
            .ok_or("MHC/VirtIO-GPU: missing COMMON_CFG capability")?;

        let notify_cap = virtio_caps
            .iter()
            .find(|c| c.cfg_type == pci::VIRTIO_PCI_CAP_NOTIFY_CFG);

        // -------------------------------------------------------------------
        // Step 3: Map the common-config BAR region
        // -------------------------------------------------------------------
        let common_bar_phys = device.determine_mem_base(common_cap.bar as usize)?;
        let common_phys = kernel_memory::PhysicalAddress::new(
            common_bar_phys.value() + common_cap.bar_offset as usize,
        )
        .ok_or("MHC/VirtIO-GPU: invalid common config physical address")?;

        let common_mapped =
            kernel_memory::map_frame_range(common_phys, common_cap.length as usize, kernel_memory::MMIO_FLAGS)?;
        let cfg = common_mapped.start_address().value();

        // Leak the mapping — it must live forever (device lifetime = OS lifetime).
        core::mem::forget(common_mapped);

        // -------------------------------------------------------------------
        // Step 4: Map the notify BAR (doorbell)
        // -------------------------------------------------------------------
        let (notify_va, notify_multiplier) = if let Some(nc) = notify_cap {
            // The notify cap is followed by a 4-byte notify_off_multiplier.
            // In the PCI cap list it is encoded right after the standard cap
            // fields (at cap_addr+16 in the raw config space).  For simplicity
            // we use a hard-coded common value of 4 (QEMU default).
            // A production driver would parse the extra field from config space.
            let nb_phys = device
                .determine_mem_base(nc.bar as usize)?;
            let nb_phys_off = kernel_memory::PhysicalAddress::new(
                nb_phys.value() + nc.bar_offset as usize,
            )
            .ok_or("MHC/VirtIO-GPU: invalid notify BAR address")?;

            let nb_mapped =
                kernel_memory::map_frame_range(nb_phys_off, (nc.length as usize).max(4096), kernel_memory::MMIO_FLAGS)?;
            let va = nb_mapped.start_address().value();
            core::mem::forget(nb_mapped);
            (va, 4u32) // 4 bytes per notify offset is the QEMU default
        } else {
            (0usize, 0u32)
        };

        // -------------------------------------------------------------------
        // Step 5: VirtIO initialization sequence (spec §3.1.1)
        // -------------------------------------------------------------------
        unsafe {
            // 5a. Reset
            Self::w8(cfg, OFF_DEVICE_STATUS, 0);

            // 5b. ACKNOWLEDGE
            Self::w8(cfg, OFF_DEVICE_STATUS, VIRTIO_STATUS_ACKNOWLEDGE);

            // 5c. ACKNOWLEDGE | DRIVER
            Self::w8(cfg, OFF_DEVICE_STATUS,
                VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER);

            // 5d. Read device features (low 32 bits = GPU features)
            Self::w32(cfg, OFF_DEVICE_FEATURE_SEL, 0);
            let dev_feat_lo = Self::r32(cfg, OFF_DEVICE_FEATURE);

            let has_virgl = (dev_feat_lo & VIRTIO_GPU_F_VIRGL) != 0;
            let has_edid  = (dev_feat_lo & VIRTIO_GPU_F_EDID)  != 0;
            let has_blob  = (dev_feat_lo & VIRTIO_GPU_F_RESOURCE_BLOB) != 0;

            log::info!(
                "MHC/VirtIO-GPU: device features: virgl={} edid={} blob={}",
                has_virgl, has_edid, has_blob
            );

            // 5e. Write driver features: VIRTIO_F_VERSION_1 (bit 32 = high word bit 0)
            //     We only request the basic GPU features that are available.
            Self::w32(cfg, OFF_DRIVER_FEATURE_SEL, 0);
            // Accept whatever GPU features the device offers (no filtering needed).
            Self::w32(cfg, OFF_DRIVER_FEATURE, dev_feat_lo);

            Self::w32(cfg, OFF_DRIVER_FEATURE_SEL, 1);
            // Request VIRTIO_F_VERSION_1 (bit 32 → high-word bit 0).
            Self::w32(cfg, OFF_DRIVER_FEATURE, VIRTIO_F_VERSION_1);

            // 5f. FEATURES_OK
            Self::w8(cfg, OFF_DEVICE_STATUS,
                VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK);

            let status_check = Self::r8(cfg, OFF_DEVICE_STATUS);
            if status_check & VIRTIO_STATUS_FEATURES_OK == 0 {
                Self::w8(cfg, OFF_DEVICE_STATUS, VIRTIO_STATUS_FAILED);
                return Err("MHC/VirtIO-GPU: device rejected feature negotiation");
            }

            self.has_virgl = has_virgl;
            self.has_blob  = has_blob;
        }

        // -------------------------------------------------------------------
        // Step 6: Set up controlq (queue index 0)
        // -------------------------------------------------------------------
        const QUEUE_IDX: u16 = 0;
        const MAX_QUEUE_SIZE: u16 = 64;

        let (q_size, notify_queue_off) = unsafe {
            Self::w16(cfg, OFF_QUEUE_SELECT, QUEUE_IDX);
            let qmax = Self::r16(cfg, OFF_QUEUE_SIZE);
            if qmax == 0 {
                Self::w8(cfg, OFF_DEVICE_STATUS, VIRTIO_STATUS_FAILED);
                return Err("MHC/VirtIO-GPU: controlq size is 0");
            }
            let qs = qmax.min(MAX_QUEUE_SIZE);
            let notify_off = Self::r16(cfg, OFF_QUEUE_NOTIFY_OFF);
            (qs, notify_off)
        };
        log::info!("MHC/VirtIO-GPU: controlq size={}", q_size);

        // Allocate DMA memory for the descriptor table + available ring + used ring.
        // All three sections are packed into one physically-contiguous allocation.
        let desc_size  = VIRTQ_DESC_SIZE * q_size as usize;
        let avail_size = 6 + 2 * q_size as usize;
        let used_size  = 6 + 8 * q_size as usize;

        // Align each section to 64 bytes (spec recommendation).
        let avail_off = (desc_size + 63) & !63;
        let used_off  = (avail_off + avail_size + 63) & !63;
        let total     = used_off + used_size;

        let (vq_mapped, vq_phys) =
            kernel_memory::create_contiguous_mapping(total, kernel_memory::DMA_FLAGS)?;
        let vq_va = vq_mapped.start_address().value();

        // Zero the queue memory so all descriptors and ring entries start clean.
        unsafe { core::ptr::write_bytes(vq_va as *mut u8, 0, total); }

        let desc_va  = vq_va;
        let avail_va = vq_va + avail_off;
        let used_va  = vq_va + used_off;

        let desc_pa  = vq_phys.value() as u64;
        let avail_pa = desc_pa + avail_off as u64;
        let used_pa  = desc_pa + used_off as u64;

        core::mem::forget(vq_mapped);

        // Write queue configuration to device.
        unsafe {
            Self::w16(cfg, OFF_QUEUE_SELECT,   QUEUE_IDX);
            Self::w16(cfg, OFF_QUEUE_SIZE,     q_size);
            Self::w16(cfg, OFF_QUEUE_MSIX_VEC, 0xFFFF); // no MSI-X
            Self::w64(cfg, OFF_QUEUE_DESC,     desc_pa);
            Self::w64(cfg, OFF_QUEUE_DRIVER,   avail_pa);
            Self::w64(cfg, OFF_QUEUE_DEVICE,   used_pa);
            Self::w16(cfg, OFF_QUEUE_ENABLE,   1);
        }

        // -------------------------------------------------------------------
        // Step 7: DRIVER_OK — device is fully operational
        // -------------------------------------------------------------------
        unsafe {
            Self::w8(cfg, OFF_DEVICE_STATUS,
                VIRTIO_STATUS_ACKNOWLEDGE
                | VIRTIO_STATUS_DRIVER
                | VIRTIO_STATUS_FEATURES_OK
                | VIRTIO_STATUS_DRIVER_OK);

            let final_status = Self::r8(cfg, OFF_DEVICE_STATUS);
            if final_status & VIRTIO_STATUS_DEVICE_NEEDS_RESET != 0 {
                return Err("MHC/VirtIO-GPU: device signalled DEVICE_NEEDS_RESET");
            }
            log::info!("MHC/VirtIO-GPU: device status after init: {:#x}", final_status);
        }

        // -------------------------------------------------------------------
        // Step 8: Send VIRTIO_GPU_CMD_GET_DISPLAY_INFO and poll for response
        // -------------------------------------------------------------------
        let num_scanouts = self.send_get_display_info(
            cfg,
            notify_va,
            notify_multiplier,
            notify_queue_off,
            desc_va, avail_va, used_va,
            q_size,
        ).unwrap_or_else(|e| {
            log::warn!("MHC/VirtIO-GPU: GET_DISPLAY_INFO failed: {} — assuming 1 scanout", e);
            1
        });

        // -------------------------------------------------------------------
        // Finalize
        // -------------------------------------------------------------------
        *self.hw.lock() = Some(VirtioHwState {
            common_cfg_va:    cfg,
            notify_va,
            notify_multiplier,
            desc_va,
            avail_va,
            used_va,
            desc_pa,
            avail_pa,
            used_pa,
            q_size,
            avail_idx:        1, // we used index 0 for GET_DISPLAY_INFO
            last_used_idx:    1,
            next_desc:        2, // we used descriptors 0 and 1
            notify_queue_off,
        });

        self.initialized  = true;
        self.num_scanouts = num_scanouts;
        self.caps.supports_compute  = self.has_virgl;
        self.caps.supports_graphics = true;
        self.caps.max_queues        = 2;

        log::info!(
            "MHC/VirtIO-GPU: initialized — {} scanout(s), virgl={}",
            num_scanouts, self.has_virgl
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // GET_DISPLAY_INFO helper (polled, used only during probe)
    // -----------------------------------------------------------------------


    // -------------------------------------------------------------------
    // Runtime virtqueue helpers
    // -------------------------------------------------------------------

    /// Send a command/response pair on the controlq and poll for completion.
    ///
    /// Always reuses descriptor slots 0 and 1 because the driver polls
    /// synchronously — the device has processed the previous command by the
    /// time we submit the next one.
    fn send_polled_2desc(
        &self,
        hw:        &mut VirtioHwState,
        cmd_pa:    u64,
        cmd_size:  u32,
        resp_pa:   u64,
        resp_size: u32,
    ) -> Result<(), &'static str> {
        unsafe {
            Self::write_desc(hw.desc_va, 0, cmd_pa,  cmd_size,  VIRTQ_DESC_F_NEXT,  1);
            Self::write_desc(hw.desc_va, 1, resp_pa, resp_size, VIRTQ_DESC_F_WRITE, 0);
            Self::push_avail(hw.avail_va, hw.avail_idx, hw.q_size, 0);
            hw.avail_idx = hw.avail_idx.wrapping_add(1);
            if hw.notify_va != 0 {
                let off = hw.notify_queue_off as usize * hw.notify_multiplier as usize;
                core::ptr::write_volatile((hw.notify_va + off) as *mut u16, 0);
            }
            let mut n = 0u64;
            loop {
                if Self::poll_used(hw.used_va, hw.last_used_idx, hw.q_size).is_some() {
                    hw.last_used_idx = hw.last_used_idx.wrapping_add(1);
                    break;
                }
                n += 1;
                if n > 50_000_000 { return Err("VirtIO-GPU: command timed out"); }
                core::hint::spin_loop();
            }
        }
        Ok(())
    }

    // -------------------------------------------------------------------
    // Display resource lifecycle
    // -------------------------------------------------------------------

    /// Allocate a VirtIO-GPU 2D resource, attach DMA backing memory, and
    /// set it as scanout 0.  Returns a `VirtioScanout` handle on success.
    ///
    /// The DMA backing buffer is intentionally leaked — it lives as long
    /// as the kernel.
    pub fn setup_display_resource(
        &self,
        width: u32,
        height: u32,
    ) -> Result<VirtioScanout, &'static str> {
        if !self.initialized {
            return Err("VirtIO-GPU: device not initialized");
        }
        const RES_ID: u32 = 10; // avoids OVMF-created resource IDs
        const BPP: usize  = 4;
        let sz = width as usize * height as usize * BPP;

        // Physically contiguous DMA backing buffer.
        let (bk_map, bk_phys) =
            kernel_memory::create_contiguous_mapping(sz, kernel_memory::DMA_FLAGS)
                .map_err(|_| "VirtIO-GPU: scanout DMA alloc failed")?;
        let bk_va = bk_map.start_address().value();
        let bk_pa = bk_phys.value() as u64;
        unsafe { core::ptr::write_bytes(bk_va as *mut u8, 0, sz); }
        core::mem::forget(bk_map);

        // Shared command + response buffer.
        let hdr_sz = core::mem::size_of::<VirtioGpuResourceCreate2d>()
            .max(core::mem::size_of::<VirtioGpuResourceAttachBacking>())
            .max(core::mem::size_of::<VirtioGpuSetScanout>());
        let rsp_sz = core::mem::size_of::<VirtioGpuRespOkNodata>();
        let (hdr_map, hdr_phys) =
            kernel_memory::create_contiguous_mapping(hdr_sz + rsp_sz, kernel_memory::DMA_FLAGS)
                .map_err(|_| "VirtIO-GPU: cmd DMA alloc failed")?;
        let hdr_va = hdr_map.start_address().value();
        let hdr_pa = hdr_phys.value() as u64;
        let rsp_pa = hdr_pa + hdr_sz as u64;
        core::mem::forget(hdr_map);

        let mut lk = self.hw.lock();
        let hw = lk.as_mut().ok_or("VirtIO-GPU: hw state missing")?;

        // Step 1 — RESOURCE_CREATE_2D
        unsafe {
            core::ptr::write_bytes(hdr_va as *mut u8, 0, hdr_sz + rsp_sz);
            let c = hdr_va as *mut VirtioGpuResourceCreate2d;
            (*c).hdr.type_   = VirtioGpuCmd::ResourceCreate2d as u32;
            (*c).resource_id = RES_ID;
            (*c).format      = VirtioGpuFormat::B8G8R8A8Unorm as u32;
            (*c).width       = width;
            (*c).height      = height;
        }
        self.send_polled_2desc(
            hw, hdr_pa,
            core::mem::size_of::<VirtioGpuResourceCreate2d>() as u32,
            rsp_pa, rsp_sz as u32,
        )?;
        log::debug!("MHC/VirtIO-GPU: RESOURCE_CREATE_2D id={} {}x{}", RES_ID, width, height);

        // Step 2 — RESOURCE_ATTACH_BACKING
        unsafe {
            core::ptr::write_bytes(hdr_va as *mut u8, 0, hdr_sz + rsp_sz);
            let c = hdr_va as *mut VirtioGpuResourceAttachBacking;
            (*c).hdr.type_       = VirtioGpuCmd::ResourceAttachBacking as u32;
            (*c).resource_id     = RES_ID;
            (*c).nr_entries      = 1;
            (*c).entries[0].addr   = bk_pa;
            (*c).entries[0].length = sz as u32;
        }
        self.send_polled_2desc(
            hw, hdr_pa,
            core::mem::size_of::<VirtioGpuResourceAttachBacking>() as u32,
            rsp_pa, rsp_sz as u32,
        )?;
        log::debug!("MHC/VirtIO-GPU: RESOURCE_ATTACH_BACKING pa={:#x} sz={}", bk_pa, sz);

        // Step 3 — SET_SCANOUT
        unsafe {
            core::ptr::write_bytes(hdr_va as *mut u8, 0, hdr_sz + rsp_sz);
            let c = hdr_va as *mut VirtioGpuSetScanout;
            (*c).hdr.type_   = VirtioGpuCmd::SetScanout as u32;
            (*c).r           = VirtioGpuRect { x: 0, y: 0, width, height };
            (*c).scanout_id  = 0;
            (*c).resource_id = RES_ID;
        }
        self.send_polled_2desc(
            hw, hdr_pa,
            core::mem::size_of::<VirtioGpuSetScanout>() as u32,
            rsp_pa, rsp_sz as u32,
        )?;
        log::info!("MHC/VirtIO-GPU: display ready — {}x{} BGRA8888 scanout 0", width, height);

        // Pre-allocate a persistent DMA buffer for runtime commands
        // (TRANSFER_TO_HOST_2D, RESOURCE_FLUSH).  Reused every frame.
        let rt_cmd_max = core::mem::size_of::<VirtioGpuTransferToHost2d>()
            .max(core::mem::size_of::<VirtioGpuResourceFlush>());
        let rt_rsp_sz = core::mem::size_of::<VirtioGpuRespOkNodata>();
        let rt_total  = rt_cmd_max + rt_rsp_sz;
        let (rt_map, rt_phys) =
            kernel_memory::create_contiguous_mapping(rt_total, kernel_memory::DMA_FLAGS)
                .map_err(|_| "VirtIO-GPU: runtime cmd DMA alloc failed")?;
        let rt_va = rt_map.start_address().value();
        let rt_pa = rt_phys.value() as u64;
        core::mem::forget(rt_map);

        Ok(VirtioScanout {
            resource_id: RES_ID,
            backing_va: bk_va, backing_pa: bk_pa,
            width, height,
            cmd_va: rt_va, cmd_pa: rt_pa, cmd_len: rt_total,
        })
    }

    /// Copy `pixels` (BGRA8888 u32, `width * height` entries) to the VirtIO-GPU
    /// scanout backing buffer and issue TRANSFER_TO_HOST_2D + RESOURCE_FLUSH.
    ///
    /// Uses the pre-allocated DMA command buffer stored in `scanout` — no
    /// per-frame allocations.
    pub fn update_scanout(
        &self,
        scanout: &VirtioScanout,
        pixels:  &[u32],
    ) -> Result<(), &'static str> {
        if !self.initialized { return Err("VirtIO-GPU: not initialized"); }
        let n = (scanout.width * scanout.height) as usize;
        if pixels.len() < n { return Err("VirtIO-GPU: pixel buffer too small"); }

        // Blit to DMA backing memory.
        unsafe {
            core::ptr::copy_nonoverlapping(
                pixels.as_ptr(),
                scanout.backing_va as *mut u32,
                n,
            );
        }

        let t2d_sz  = core::mem::size_of::<VirtioGpuTransferToHost2d>();
        let rfl_sz  = core::mem::size_of::<VirtioGpuResourceFlush>();
        let rsp_sz  = core::mem::size_of::<VirtioGpuRespOkNodata>();
        let cmd_max = t2d_sz.max(rfl_sz);

        // Reuse pre-allocated DMA command buffer from scanout.
        let cv  = scanout.cmd_va;
        let cpa = scanout.cmd_pa;
        let rpa = cpa + cmd_max as u64;

        let full = VirtioGpuRect { x: 0, y: 0, width: scanout.width, height: scanout.height };
        let mut lk = self.hw.lock();
        let hw = lk.as_mut().ok_or("VirtIO-GPU: hw state missing")?;

        // TRANSFER_TO_HOST_2D
        unsafe {
            core::ptr::write_bytes(cv as *mut u8, 0, cmd_max + rsp_sz);
            let c = cv as *mut VirtioGpuTransferToHost2d;
            (*c).hdr.type_   = VirtioGpuCmd::TransferToHost2d as u32;
            (*c).r           = full;
            (*c).offset      = 0;
            (*c).resource_id = scanout.resource_id;
        }
        self.send_polled_2desc(hw, cpa, t2d_sz as u32, rpa, rsp_sz as u32)?;

        // RESOURCE_FLUSH
        unsafe {
            core::ptr::write_bytes(cv as *mut u8, 0, cmd_max + rsp_sz);
            let c = cv as *mut VirtioGpuResourceFlush;
            (*c).hdr.type_   = VirtioGpuCmd::ResourceFlush as u32;
            (*c).r           = full;
            (*c).resource_id = scanout.resource_id;
        }
        self.send_polled_2desc(hw, cpa, rfl_sz as u32, rpa, rsp_sz as u32)?;

        Ok(())
    }

    fn send_get_display_info(
        &self,
        cfg:               usize,
        notify_va:         usize,
        notify_multiplier: u32,
        notify_queue_off:  u16,
        desc_va:  usize,
        avail_va: usize,
        used_va:  usize,
        q_size:   u16,
    ) -> Result<u32, &'static str> {
        // Allocate command + response buffers in DMA memory.
        let cmd_size  = core::mem::size_of::<VirtioGpuCtrlHdr>();
        let resp_size = core::mem::size_of::<VirtioGpuRespDisplayInfo>();

        let (cmd_mapped, cmd_phys) =
            kernel_memory::create_contiguous_mapping(cmd_size + resp_size, kernel_memory::DMA_FLAGS)?;
        let cmd_va = cmd_mapped.start_address().value();

        // Zero buffers.
        unsafe { core::ptr::write_bytes(cmd_va as *mut u8, 0, cmd_size + resp_size); }

        // Write the command header.
        unsafe {
            let hdr = cmd_va as *mut VirtioGpuCtrlHdr;
            (*hdr).type_ = VirtioGpuCmd::GetDisplayInfo as u32;
            (*hdr).flags = 0;
        }

        let cmd_pa  = cmd_phys.value() as u64;
        let resp_pa = cmd_pa + cmd_size as u64;
        let resp_va = cmd_va + cmd_size;

        // Descriptor 0: command (device reads)
        // Descriptor 1: response (device writes), chained from desc 0
        unsafe {
            Self::write_desc(desc_va, 0, cmd_pa,  cmd_size  as u32, VIRTQ_DESC_F_NEXT,  1);
            Self::write_desc(desc_va, 1, resp_pa, resp_size as u32, VIRTQ_DESC_F_WRITE, 0);
            Self::push_avail(avail_va, 0, q_size, 0 /* desc head */);
        }

        // Kick the device via the notify doorbell.
        if notify_va != 0 && notify_multiplier != 0 {
            let doorbell_off = notify_queue_off as usize * notify_multiplier as usize;
            unsafe { core::ptr::write_volatile((notify_va + doorbell_off) as *mut u16, 0 /* queue idx */); }
        }

        // Poll used ring (spin with timeout ~50 ms at ~1 GHz ≈ 50M iterations).
        let mut n = 0u64;
        let response = loop {
            unsafe {
                if let Some((_id, _len)) = Self::poll_used(used_va, 0, q_size) {
                    break Some(());
                }
            }
            n += 1;
            if n > 50_000_000 {
                core::mem::forget(cmd_mapped);
                return Err("GET_DISPLAY_INFO timed out");
            }
            core::hint::spin_loop();
        };

        // Parse response.
        let mut active_scanouts = 0u32;
        if response.is_some() {
            unsafe {
                let resp = resp_va as *const VirtioGpuRespDisplayInfo;
                let resp_type = (*resp).hdr.type_;
                if resp_type == VirtioGpuResp::OkDisplayInfo as u32 {
                    for i in 0..16usize {
                        if (*resp).pmodes[i].enabled != 0 {
                            let r = &(*resp).pmodes[i].r;
                            log::info!(
                                "MHC/VirtIO-GPU: scanout {} — {}x{} at ({},{})",
                                i, r.width, r.height, r.x, r.y
                            );
                            active_scanouts += 1;
                        }
                    }
                } else {
                    log::warn!("MHC/VirtIO-GPU: GET_DISPLAY_INFO resp type={:#x}", resp_type);
                }
            }
        }

        core::mem::forget(cmd_mapped);

        Ok(active_scanouts.max(1))
    }
}

impl Default for VirtioGpuDevice {
    fn default() -> Self { Self::new() }
}

// ---------------------------------------------------------------------------
// GpuDevice trait implementation
// ---------------------------------------------------------------------------

impl GpuDevice for VirtioGpuDevice {
    fn name(&self) -> &str { "VirtIO-GPU" }

    fn vendor(&self) -> GpuVendor { GpuVendor::VirtIO }

    fn capabilities(&self) -> &GpuCapabilities { &self.caps }

    fn create_queue(&self, kind: QueueKind) -> Result<QueueHandle, GpuError> {
        if !self.initialized {
            return Err(GpuError::DeviceNotFound);
        }
        match kind {
            QueueKind::Graphics | QueueKind::Universal | QueueKind::Transfer => {
                Ok(QueueHandle(0))
            }
            QueueKind::Compute => {
                if self.has_virgl {
                    Ok(QueueHandle(0))
                } else {
                    Err(GpuError::Unsupported("compute requires virgl/venus 3D support"))
                }
            }
        }
    }

    fn destroy_queue(&self, _queue: QueueHandle) -> Result<(), GpuError> { Ok(()) }

    fn submit(&self, _queue: QueueHandle, _cmds: &CommandBuffer) -> Result<FenceId, GpuError> {
        if !self.initialized {
            return Err(GpuError::DeviceNotFound);
        }
        // TODO: Encode commands as VirtIO-GPU protocol and submit via controlq.
        let fence = self.fences.alloc_fence();
        self.fences.signal(fence);
        Ok(fence)
    }

    fn wait_fence(&self, fence: FenceId, timeout_ns: u64) -> Result<(), GpuError> {
        self.fences.wait(fence, timeout_ns).map_err(|_| GpuError::Timeout)
    }

    fn poll_fence(&self, fence: FenceId) -> bool {
        self.fences.poll(fence)
    }

    fn alloc(&self, _size: usize, _flags: GpuMemFlags) -> Result<GpuAllocation, GpuError> {
        if !self.initialized {
            return Err(GpuError::DeviceNotFound);
        }
        Err(GpuError::Unsupported("VirtIO-GPU alloc not yet implemented"))
    }

    fn free(&self, _alloc: GpuAllocation) -> Result<(), GpuError> {
        Err(GpuError::Unsupported("VirtIO-GPU free not yet implemented"))
    }

    fn dispatch_compute(
        &self,
        _queue: QueueHandle,
        _shader: &ShaderModule,
        _workgroups: [u32; 3],
        _bindings: &[BufferBinding],
    ) -> Result<FenceId, GpuError> {
        if !self.has_virgl {
            return Err(GpuError::Unsupported(
                "compute dispatch requires virgl/venus 3D support",
            ));
        }
        Err(GpuError::Unsupported("VirtIO-GPU compute not yet implemented"))
    }

    fn blit_to_scanout(
        &self,
        _queue: QueueHandle,
        _src: GpuAddress,
        _src_stride: u32,
        _src_width: u32,
        _src_height: u32,
    ) -> Result<FenceId, GpuError> {
        // The main display path goes through mhc::flush_display() which calls
        // update_scanout() directly.  This trait impl returns an immediately-
        // signalled fence so GpuDevice callers get a valid handle.
        if !self.initialized { return Err(GpuError::DeviceNotFound); }
        let fid = self.fences.alloc_fence();
        self.fences.signal(fid);
        Ok(fid)
    }
}

// ---------------------------------------------------------------------------
// Module-level device singleton (set by try_init)
// ---------------------------------------------------------------------------

static VIRTIO_GPU: spin::Once<Arc<VirtioGpuDevice>> = spin::Once::new();

/// Return a reference to the VirtIO-GPU device, if one was found.
pub fn device() -> Option<&'static VirtioGpuDevice> {
    VIRTIO_GPU.get().map(|a| a.as_ref())
}

// ---------------------------------------------------------------------------
// Public init entry point
// ---------------------------------------------------------------------------

/// Attempt to probe and register a VirtIO-GPU device.
///
/// Returns `Some(device_id)` if a device was found and initialized successfully,
/// `None` if no VirtIO-GPU device is present.
pub fn try_init() -> Option<usize> {
    let mut device = VirtioGpuDevice::new();
    match device.probe() {
        Ok(()) => {
            let arc = Arc::new(device);
            VIRTIO_GPU.call_once(|| arc.clone());
            let id = crate::device::register_device(arc);
            log::info!("MHC/VirtIO-GPU: registered as device_id={}", id);
            Some(id)
        }
        Err(msg) => {
            log::debug!("MHC/VirtIO-GPU: {}", msg);
            None
        }
    }
}
