//! Syscalls graphiques MaiOS : framebuffer mapping, present, vsync.
//!
//! Ces syscalls permettent aux applications de :
//! - Obtenir un accès direct au backbuffer (SYS_MAP_FRAMEBUFFER)
//! - Déclencher un flush vers l'affichage (SYS_PRESENT)
//! - Attendre le prochain VSync (SYS_VSYNC_WAIT)
//! - Entrer/sortir du mode fullscreen exclusif

use crate::error::{SyscallResult, SyscallError};
use alloc::vec::Vec;

/// Buffer d'application fenêtrée (ex. Doom dans une fenêtre MaiOS).
struct WindowAppBuffer {
    task_id: usize,
    buffer: Vec<u32>,
    width: usize,
    height: usize,
}

static WINDOW_APP_BUFFER: spin::Mutex<Option<WindowAppBuffer>> = spin::Mutex::new(None);

/// SYS_MAP_FRAMEBUFFER (0x0802)
///
/// Retourne les informations du backbuffer graphique pour rendu direct.
/// Si la tâche possède une fenêtre dédiée dans le WindowManager, renvoie
/// un buffer dédié aux dimensions de la zone cliente de la fenêtre.
/// Sinon, renvoie l'accès direct au backbuffer plein écran MGI.
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

    let my_id = task::get_my_current_task_id();

    // 1. Si la tâche possède une fenêtre dans le WindowManager :
    // Mode fenêtré automatique avec buffer aux dimensions de la fenêtre
    if let Some(wm_ref) = window_manager::WINDOW_MANAGER.get() {
        let wm = wm_ref.lock();
        if let Some(win_arc) = wm.get_window_by_task_id(my_id) {
            let win = win_arc.lock();
            let ca = win.content_area();
            let w = ca.width();
            let h = ca.height();
            drop(win);
            drop(wm);

            let mut surf_guard = WINDOW_APP_BUFFER.lock();
            let need_alloc = match &*surf_guard {
                Some(s) => s.task_id != my_id || s.width != w || s.height != h,
                None => true,
            };
            if need_alloc {
                *surf_guard = Some(WindowAppBuffer {
                    task_id: my_id,
                    buffer: alloc::vec![0u32; w * h],
                    width: w,
                    height: h,
                });
            }
            let s = surf_guard.as_mut().unwrap();
            let ptr = s.buffer.as_mut_ptr() as usize;

            let info = info_ptr as *mut u8;
            unsafe {
                core::ptr::write_unaligned(info as *mut u64, ptr as u64);
                core::ptr::write_unaligned(info.add(8) as *mut u32, w as u32);
                core::ptr::write_unaligned(info.add(12) as *mut u32, h as u32);
                core::ptr::write_unaligned(info.add(16) as *mut u32, (w * 4) as u32);
                core::ptr::write_unaligned(info.add(20) as *mut u32, 0u32);
                core::ptr::write_unaligned(info.add(24) as *mut u64, 0u64);
            }
            return Ok(0);
        }
    }

    // 2. Mode plein écran (fallback si pas de fenêtre)
    let (ptr, w, h) = mgi::get_backbuffer_info()
        .ok_or(SyscallError::NoDevice)?;

    // Écrire la struct FramebufferInfo dans l'espace du caller
    let info = info_ptr as *mut u8;
    unsafe {
        core::ptr::write_unaligned(info as *mut u64, ptr as u64);
        core::ptr::write_unaligned(info.add(8) as *mut u32, w as u32);
        core::ptr::write_unaligned(info.add(12) as *mut u32, h as u32);
        core::ptr::write_unaligned(info.add(16) as *mut u32, (w * 4) as u32);
        core::ptr::write_unaligned(info.add(20) as *mut u32, 0u32);
        core::ptr::write_unaligned(info.add(24) as *mut u64, 0u64);
    }

    Ok(0)
}

/// SYS_PRESENT (0x0803)
///
/// Déclenche un flush du backbuffer vers la fenêtre ou le frontbuffer + VirtIO-GPU.
///
/// En mode fenêtré : copie le buffer de l'app dans la zone cliente de la fenêtre
/// et rafraîchit le compositeur.
/// En mode plein écran normal : invalide tout le tilemap MGI puis appelle present().
/// En mode fullscreen exclusif : copie le buffer app → frontbuffer.
///
/// # Arguments
/// - `flags` (arg0): 0 = normal, 1 = fullscreen exclusif
///
/// # Returns
/// 0 en cas de succès.
pub fn sys_present(
    flags: u64,
    _a1: u64, _a2: u64, _a3: u64, _a4: u64, _a5: u64,
) -> SyscallResult {
    let my_id = task::get_my_current_task_id();

    // 1. Application fenêtrée : recopie de son tampon dans la zone cliente de sa fenêtre, puis
    //    validation de cette seule zone. La composition et la présentation sont faites par la
    //    tâche du compositeur, à sa cadence : l'application ne prend ni le verrou de MGI ni
    //    celui des autres fenêtres, et ne recompose plus tout l'écran à chaque image.
    if let Some(surf) = WINDOW_APP_BUFFER.lock().as_ref() {
        if surf.task_id == my_id {
            let win_arc = window_manager::WINDOW_MANAGER
                .get()
                .and_then(|wm| wm.lock().get_window_by_task_id(my_id));
            if let Some(win_arc) = win_arc {
                let damaged = {
                    let mut win = win_arc.lock();
                    let ca = win.content_area();
                    let pos = win.get_position();
                    let (win_w, _) = win.get_size();
                    let cw = surf.width.min(ca.width());
                    let ch = surf.height.min(ca.height());
                    let start_x = ca.top_left.x.max(0) as usize;
                    let start_y = ca.top_left.y.max(0) as usize;
                    let fb = win.framebuffer_mut().buffer_mut();
                    // SAFETY : `AlphaPixel` est `repr(C)` sur 4 octets (BGRA = XRGB8888 little-endian).
                    let dst = unsafe {
                        core::slice::from_raw_parts_mut(fb.as_mut_ptr() as *mut u32, fb.len())
                    };
                    for y in 0..ch {
                        let s = &surf.buffer[y * surf.width..y * surf.width + cw];
                        let o = (start_y + y) * win_w + start_x;
                        if o + cw > dst.len() {
                            break;
                        }
                        // Octet d'alpha à 0 (opaque) : l'octet X des applications est ignoré.
                        for (d, p) in dst[o..o + cw].iter_mut().zip(s.iter()) {
                            *d = *p & 0x00FF_FFFF;
                        }
                    }
                    shapes::Rectangle {
                        top_left: pos + (start_x as isize, start_y as isize),
                        bottom_right: pos + ((start_x + cw) as isize, (start_y + ch) as isize),
                    }
                };
                window_manager::damage(damaged);
                return Ok(0);
            }
        }
    }

    if flags & 1 != 0 {
        // Plein écran exclusif : le compositeur recopie le tampon de l'application
        // directement vers l'affichage.
        if !mgi::is_exclusive_fullscreen() {
            return Err(SyscallError::InvalidArgument);
        }
        window_manager::commit_exclusive();
    } else {
        // Plein écran sans fenêtre : l'application a dessiné dans le tampon arrière de MGI.
        window_manager::commit_raw_backbuffer();
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
    // Attente bloquante sur l'horloge d'images du compositeur (plus de scrutation).
    Ok(window_manager::vsync_wait())
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
