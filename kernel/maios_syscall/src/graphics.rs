//! Syscalls graphiques MaiOS : framebuffer mapping, present, vsync.
//!
//! Ces syscalls permettent aux applications de :
//! - Obtenir un accès direct au backbuffer (SYS_MAP_FRAMEBUFFER)
//! - Déclencher un flush vers l'affichage (SYS_PRESENT)
//! - Attendre le prochain VSync (SYS_VSYNC_WAIT)
//! - Entrer/sortir du mode fullscreen exclusif

use crate::error::{SyscallResult, SyscallError};

/// SYS_MAP_FRAMEBUFFER (0x0802)
///
/// Retourne les informations du backbuffer graphique pour rendu direct.
///
/// # Arguments
/// - `info_ptr` (arg0): pointeur vers une struct `FramebufferInfo` à remplir
///
/// # FramebufferInfo layout (32 octets)
/// ```text
///   offset 0:  u64  ptr       — adresse du backbuffer (BGRA8888)
///   offset 8:  u32  width     — largeur en pixels
///   offset 12: u32  height    — hauteur en pixels
///   offset 16: u32  stride    — stride en octets (width * 4)
///   offset 20: u32  format    — 0 = BGRA8888
///   offset 24: u64  reserved
/// ```
///
/// # Returns
/// 0 en cas de succès, erreur sinon.
pub fn sys_map_framebuffer(
    info_ptr: u64,
    _a1: u64, _a2: u64, _a3: u64, _a4: u64, _a5: u64,
) -> SyscallResult {
    if info_ptr == 0 {
        return Err(SyscallError::InvalidArgument);
    }

    let (ptr, w, h) = mgi::get_backbuffer_info()
        .ok_or(SyscallError::NoDevice)?;

    // Écrire la struct FramebufferInfo dans l'espace du caller
    let info = info_ptr as *mut u8;
    unsafe {
        // ptr (u64)
        core::ptr::write_unaligned(info as *mut u64, ptr as u64);
        // width (u32)
        core::ptr::write_unaligned(info.add(8) as *mut u32, w as u32);
        // height (u32)
        core::ptr::write_unaligned(info.add(12) as *mut u32, h as u32);
        // stride (u32) = width * 4 bytes per pixel
        core::ptr::write_unaligned(info.add(16) as *mut u32, (w * 4) as u32);
        // format (u32) = 0 for BGRA8888
        core::ptr::write_unaligned(info.add(20) as *mut u32, 0u32);
        // reserved
        core::ptr::write_unaligned(info.add(24) as *mut u64, 0u64);
    }

    Ok(0)
}

/// SYS_PRESENT (0x0803)
///
/// Déclenche un flush du backbuffer vers le frontbuffer + VirtIO-GPU.
///
/// En mode normal : invalide tout le tilemap MGI puis appelle present().
/// En mode fullscreen exclusif : copie le buffer app → frontbuffer.
///
/// # Arguments
/// - `flags` (arg0): 0 = normal, 1 = fullscreen exclusif (utilise le buffer enregistré)
///
/// # Returns
/// 0 en cas de succès.
pub fn sys_present(
    flags: u64,
    _a1: u64, _a2: u64, _a3: u64, _a4: u64, _a5: u64,
) -> SyscallResult {
    if flags & 1 != 0 {
        // Mode fullscreen exclusif — le WM s'en chargera au prochain tick
        if !mgi::is_exclusive_fullscreen() {
            return Err(SyscallError::InvalidArgument);
        }
        mgi::present_exclusive();
    } else {
        // Mode normal : marquer tout comme dirty et flush
        if let Some(mgi_ref) = mgi::MGI.get() {
            let mut guard = mgi_ref.lock();
            guard.invalidate_all();
            guard.present();
        }
    }

    // Flush VirtIO-GPU si disponible
    if mhc::has_display() {
        if let Some(mgi_ref) = mgi::MGI.get() {
            let guard = mgi_ref.lock();
            let (w, h) = guard.resolution();
            let pixels = unsafe {
                core::slice::from_raw_parts(
                    guard.frontbuffer_ptr() as *const u32,
                    w * h,
                )
            };
            mhc::flush_display(pixels, w as u32, h as u32);
        }
    }

    Ok(0)
}

/// SYS_VSYNC_WAIT (0x0806)
///
/// Bloque le thread courant jusqu'au prochain tick VSync (~16.67ms à 60Hz).
///
/// # Returns
/// Le numéro de frame VSync courant après le réveil.
pub fn sys_vsync_wait(
    _a0: u64, _a1: u64, _a2: u64, _a3: u64, _a4: u64, _a5: u64,
) -> SyscallResult {
    let current = mgi::vsync_counter();

    // Dormir 1ms entre chaque vérification pour ne pas monopoliser le CPU.
    // Le WM tourne à ~60fps → attente max ~16ms.
    let dur = sleep::Duration::from_millis(1);
    loop {
        let now = mgi::vsync_counter();
        if now != current {
            return Ok(now);
        }
        let _ = sleep::sleep(dur);
    }
}

/// SYS_SET_EXCLUSIVE_FULLSCREEN (non numéroté, inclus dans SYS_GET_EVENT 0x0804)
///
/// Sous-commande de SYS_GET_EVENT pour configurer le mode fullscreen.
/// Ou bien on utilise un ioctl-like sur le fd framebuffer.
///
/// Pour l'instant, exposé via SYS_GET_EVENT avec sub-command:
///   arg0 = 1 → activer fullscreen exclusif (arg1 = buffer_ptr, arg2 = width, arg3 = height)
///   arg0 = 2 → désactiver fullscreen exclusif
///   arg0 = 0 → get event normal (futur)
pub fn sys_get_event(
    subcmd: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    _a4: u64,
    _a5: u64,
) -> SyscallResult {
    match subcmd {
        1 => {
            // Activer fullscreen exclusif
            let buffer_ptr = arg1 as usize;
            let width = arg2 as usize;
            let height = arg3 as usize;
            if buffer_ptr == 0 || width == 0 || height == 0 {
                return Err(SyscallError::InvalidArgument);
            }
            mgi::set_exclusive_fullscreen(buffer_ptr, width * height);
            Ok(0)
        }
        2 => {
            // Désactiver fullscreen exclusif
            mgi::clear_exclusive_fullscreen();
            Ok(0)
        }
        0 => {
            // Read next input event from the MGI global queue.
            // arg1 = pointer to maios_event_t (16 bytes)
            //   struct maios_event_t { u32 type; u32 keycode; u32 ascii; u32 modifiers; }
            let event_ptr = arg1 as *mut u32;
            if event_ptr.is_null() {
                return Err(SyscallError::InvalidArgument);
            }
            match mgi::pop_input_event() {
                Some(ev) => {
                    unsafe {
                        core::ptr::write_unaligned(event_ptr,            ev.event_type);
                        core::ptr::write_unaligned(event_ptr.add(1),     ev.keycode);
                        core::ptr::write_unaligned(event_ptr.add(2),     ev.ascii);
                        core::ptr::write_unaligned(event_ptr.add(3),     ev.modifiers);
                    }
                    Ok(1) // 1 = event available
                }
                None => Ok(0), // 0 = no event pending
            }
        }
        _ => Err(SyscallError::InvalidArgument),
    }
}
