//! USB HID driver for MaiOS.
//!
//! Bridges XHCI-enumerated HID devices (class 0x03) to the existing keyboard
//! event pipeline via `Queue<Event>`.
//!
//! After XHCI enumerates devices, call [`init`] to store the keyboard queue,
//! then call [`configure_hid_devices`] to send SET_CONFIGURATION / SET_PROTOCOL
//! to each HID device and spawn a polling task for each one.

#![no_std]

extern crate alloc;

pub mod keymap;

use alloc::string::String;
use core::time::Duration;
use log::warn;
use mpmc::Queue;
use spin::Once;
use event_types::Event;
use keycodes_ascii::{KeyAction, KeyEvent};
use keymap::{hid_usage_to_keycode, hid_modifiers_to_keyboard_modifiers};

/// Global reference to the keyboard event queue (same one used by PS/2 keyboard).
static KEYBOARD_QUEUE: Once<Queue<Event>> = Once::new();

/// Initialize the USB HID subsystem.
///
/// Stores the keyboard event queue for later use by HID polling tasks.
/// This must be called before [`configure_hid_devices`].
///
/// ## Arguments
/// * `keyboard_queue` - The producer end of the keyboard event queue,
///   shared with the PS/2 keyboard driver and window manager.
pub fn init(keyboard_queue: Queue<Event>) {
    KEYBOARD_QUEUE.call_once(|| keyboard_queue);
    warn!("USB_HID: initialized, keyboard queue registered");
}

/// Scan XHCI-enumerated devices for HID interfaces, configure them,
/// and spawn a polling task for each one.
///
/// For each device with `device_class == 0` (composite, check interface) or
/// `device_class == 3` (HID), this function:
/// 1. Sends SET_CONFIGURATION(1) to activate the first configuration.
/// 2. Sends SET_PROTOCOL(0) to force boot protocol (no HID descriptor parsing).
/// 3. Logs the configured device.
///
/// The actual interrupt endpoint setup and polling task spawn is included
/// as a placeholder that will be completed once the XHCI driver exposes
/// endpoint ring management for non-EP0 endpoints.
pub fn configure_hid_devices() {
    let xhci_mutex = match xhci::get_xhci() {
        Some(m) => m,
        None => {
            warn!("USB_HID: no XHCI controller available");
            return;
        }
    };

    let _queue = match KEYBOARD_QUEUE.get() {
        Some(q) => q,
        None => {
            warn!("USB_HID: keyboard queue not initialized, call init() first");
            return;
        }
    };

    let mut xhci = xhci_mutex.lock();
    let device_count = xhci.devices.len();

    if device_count == 0 {
        warn!("USB_HID: no USB devices enumerated");
        return;
    }

    warn!("USB_HID: scanning {} device(s) for HID interfaces...", device_count);

    // Collect device info we need before mutably borrowing for control transfers.
    // We need slot_id, device_class, vendor_id, product_id for each device.
    let mut hid_devices: alloc::vec::Vec<(u8, u8, u16, u16)> = alloc::vec::Vec::new();
    for dev in xhci.devices.iter() {
        // USB HID class = 0x03.
        // device_class == 0 means "interface-defined" (composite device) — in a
        // real driver we would parse the configuration descriptor to find HID
        // interfaces. For now, we also try class==0 since many keyboards report
        // class 0 at the device level but class 3 at the interface level.
        if dev.device_class == 3 || dev.device_class == 0 {
            hid_devices.push((dev.slot_id, dev.device_class, dev.vendor_id, dev.product_id));
        }
    }

    if hid_devices.is_empty() {
        warn!("USB_HID: no HID-class devices found among {} device(s)", device_count);
        return;
    }

    for (slot_id, dev_class, vid, pid) in hid_devices.iter().copied() {
        warn!("USB_HID: configuring slot {} (class={:#04x}, VID={:#06x}, PID={:#06x})",
            slot_id, dev_class, vid, pid);

        // SET_CONFIGURATION(1): Host-to-Device, Standard, Device.
        // bmRequestType=0x00, bRequest=0x09 (SET_CONFIGURATION), wValue=1.
        match xhci.control_transfer_to_device(slot_id, 0x00, 0x09, 1, 0, 0) {
            Ok(_) => warn!("USB_HID:   slot {}: SET_CONFIGURATION(1) OK", slot_id),
            Err(e) => {
                warn!("USB_HID:   slot {}: SET_CONFIGURATION(1) failed: {}", slot_id, e);
                continue;
            }
        }

        // SET_PROTOCOL(0): Host-to-Device, Class, Interface.
        // bmRequestType=0x21, bRequest=0x0B (SET_PROTOCOL), wValue=0 (boot protocol).
        match xhci.control_transfer_to_device(slot_id, 0x21, 0x0B, 0, 0, 0) {
            Ok(_) => warn!("USB_HID:   slot {}: SET_PROTOCOL(0) boot protocol OK", slot_id),
            Err(e) => {
                warn!("USB_HID:   slot {}: SET_PROTOCOL(0) failed: {}", slot_id, e);
                // Non-fatal: some devices may not support SET_PROTOCOL.
            }
        }

        warn!("USB_HID:   slot {}: HID device configured for boot protocol", slot_id);

        // Spawn a polling task for this device.
        // The task uses GET_REPORT via control transfers as a fallback until
        // the XHCI driver supports interrupt IN endpoint ring management.
        let poll_slot_id = slot_id;
        let task_name = String::from("usb_hid_poll_") + &alloc::format!("{}", slot_id);

        // TODO: re-enable once control_transfer uses interrupt-driven completion
        // instead of busy-wait spin_loop(). The current spin poll at 125 Hz
        // burns an entire CPU core under QEMU TCG.
        warn!("USB_HID:   slot {}: poll task DISABLED (busy-wait too expensive)", slot_id);
        let _ = (poll_slot_id, task_name); // suppress unused warnings
    }
}

/// Polling task for a single USB HID keyboard device.
///
/// Periodically issues GET_REPORT (Input) via control transfers on EP0 to read
/// the 8-byte boot protocol keyboard report. Translates key press/release
/// transitions into `KeyEvent`s and pushes them to the global keyboard queue.
///
/// This function runs in its own kernel task and never returns.
fn hid_poll_task(slot_id: u8) -> isize {
    warn!("USB_HID: poll task started for slot {}", slot_id);

    let queue = match KEYBOARD_QUEUE.get() {
        Some(q) => q,
        None => {
            warn!("USB_HID: poll task slot {}: no keyboard queue!", slot_id);
            return -1;
        }
    };

    // Previous report state for detecting press/release transitions.
    let mut prev_report = [0u8; 8];

    loop {
        // Sleep 8ms between polls (~125 Hz, matching USB full-speed interrupt interval).
        // IMPORTANT: never use bare schedule(), always sleep().
        let _ = sleep::sleep(Duration::from_millis(8));

        // GET_REPORT (Input) via control transfer.
        // bmRequestType = 0xA1 (Device-to-Host, Class, Interface)
        // bRequest      = 0x01 (GET_REPORT)
        // wValue        = 0x0100 (Report Type=Input(1), Report ID=0)
        // wIndex        = 0x0000 (Interface 0)
        // wLength       = 8 (boot protocol keyboard report is 8 bytes)
        let report = {
            let xhci_mutex = match xhci::get_xhci() {
                Some(m) => m,
                None => continue,
            };
            let mut xhci = xhci_mutex.lock();

            // Check device still exists.
            if !xhci.devices.iter().any(|d| d.slot_id == slot_id) {
                warn!("USB_HID: poll task slot {}: device gone, exiting", slot_id);
                return -1;
            }

            // GET_REPORT (Input) via control transfer on EP0.
            match xhci.control_transfer_to_device(slot_id, 0xA1, 0x01, 0x0100, 0, 8) {
                Ok(data) => data,
                Err(_) => {
                    // Transfer errors are common during device init or disconnect.
                    // Just retry on the next poll cycle.
                    continue;
                }
            }
        };

        if report.len() < 8 {
            continue;
        }

        let cur_report: [u8; 8] = [
            report[0], report[1], report[2], report[3],
            report[4], report[5], report[6], report[7],
        ];

        // Build the modifier state from byte 0.
        let modifiers = hid_modifiers_to_keyboard_modifiers(cur_report[0]);

        // Detect modifier changes (byte 0) — generate press/release events for
        // modifier keys that changed.
        process_modifier_changes(prev_report[0], cur_report[0], &modifiers, queue);

        // Detect key press/release transitions in bytes 2..7.
        // Keys present in prev but NOT in cur => Released.
        for &prev_usage in &prev_report[2..8] {
            if prev_usage == 0 {
                continue;
            }
            // Check if this usage is still present in the current report.
            let still_pressed = cur_report[2..8].contains(&prev_usage);
            if !still_pressed {
                if let Some(keycode) = hid_usage_to_keycode(prev_usage) {
                    let event = Event::new_keyboard_event(
                        KeyEvent::new(keycode, KeyAction::Released, modifiers),
                    );
                    let _ = queue.push(event);
                }
            }
        }

        // Keys present in cur but NOT in prev => Pressed.
        for &cur_usage in &cur_report[2..8] {
            if cur_usage == 0 {
                continue;
            }
            let was_pressed = prev_report[2..8].contains(&cur_usage);
            if !was_pressed {
                if let Some(keycode) = hid_usage_to_keycode(cur_usage) {
                    let event = Event::new_keyboard_event(
                        KeyEvent::new(keycode, KeyAction::Pressed, modifiers),
                    );
                    let _ = queue.push(event);
                }
            }
        }

        prev_report = cur_report;
    }
}

/// Generate press/release events for modifier keys that changed between
/// the previous and current modifier bytes.
fn process_modifier_changes(
    prev_mods: u8,
    cur_mods: u8,
    modifiers: &keycodes_ascii::KeyboardModifiers,
    queue: &Queue<Event>,
) {
    if prev_mods == cur_mods {
        return;
    }

    // Each bit in the modifier byte corresponds to a specific modifier key.
    // Bit 0 = LCtrl, 1 = LShift, 2 = LAlt, 3 = LGui,
    // Bit 4 = RCtrl, 5 = RShift, 6 = RAlt, 7 = RGui.
    static MOD_USAGE_IDS: [u8; 8] = [0xE0, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7];

    let changed = prev_mods ^ cur_mods;
    for bit in 0..8u8 {
        if changed & (1 << bit) == 0 {
            continue;
        }
        let usage = MOD_USAGE_IDS[bit as usize];
        if let Some(keycode) = hid_usage_to_keycode(usage) {
            let action = if cur_mods & (1 << bit) != 0 {
                KeyAction::Pressed
            } else {
                KeyAction::Released
            };
            let event = Event::new_keyboard_event(
                KeyEvent::new(keycode, action, *modifiers),
            );
            let _ = queue.push(event);
        }
    }
}
