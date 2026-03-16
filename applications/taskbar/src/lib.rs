#![no_std]
extern crate alloc;
extern crate window;
extern crate window_manager;
extern crate color;
extern crate shapes;
extern crate event_types;
extern crate scheduler;

use alloc::vec::Vec;
use alloc::string::String;
use color::Color;
use shapes::Coord;
use event_types::Event;

/// Taskbar: Fixed bottom panel for window/app management
/// - Shows running applications
/// - Allows task switching
/// - Provides system tray area
pub fn main(_args: Vec<String>) -> isize {
    // Get the actual screen resolution
    let wm = window_manager::WINDOW_MANAGER.get().unwrap().lock();
    let (screen_width, screen_height) = wm.get_screen_size();
    drop(wm);

    let taskbar_height = 48;
    let taskbar_y = screen_height as isize - taskbar_height as isize;

    // Create taskbar as a window at the bottom of screen
    // Note: The window manager will handle rendering, but this window is "sticky" (stays at bottom)
    let taskbar_bg = Color::new(0x2B2B2B); // Dark gray taskbar
    let mut taskbar = match window::Window::new(
        Coord::new(0, taskbar_y),
        screen_width,
        taskbar_height,
        taskbar_bg,
    ) {
        Ok(w) => w,
        Err(_e) => return -1,
    };

    // Event loop for taskbar
    loop {
        match taskbar.handle_event() {
            Ok(Some(ev)) => {
                match ev {
                    Event::MousePositionEvent(mouse_ev) => {
                        // Handle taskbar clicks: app launching, task switching, etc.
                        // For now, just detect clicks in taskbar area
                        let _x = mouse_ev.coordinate.x;
                        let _y = mouse_ev.coordinate.y;
                        
                        if !mouse_ev.left_button_hold {
                            // Left button released - process click
                            // TODO: Implement click handling for app launcher, task switcher
                        }
                    }
                    Event::ExitEvent => break,
                    _ => {}
                }
            }
            Ok(None) => {
                let _ = scheduler::schedule();
            }
            Err(_) => {
                let _ = scheduler::schedule();
            }
        }
    }

    0
}
