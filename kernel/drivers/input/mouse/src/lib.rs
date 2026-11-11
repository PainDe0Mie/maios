//! A basic driver for a mouse connected to the legacy PS/2 port.

#![no_std]
#![feature(abi_x86_interrupt)]

use log::error;
use spin::{Mutex, Once};
use time::{Duration, Instant};
use mpmc::Queue;
use event_types::Event;
use x86_64::structures::idt::InterruptStackFrame;
use mouse_data::{MouseButtons, MouseEvent, MouseMovementRelative};
use ps2::{PS2Mouse, MousePacket};

/// The first PS/2 port for the mouse is connected directly to IRQ 0xC.
/// Because we perform the typical PIC remapping, the remapped IRQ vector number is 0x2C.
const PS2_MOUSE_IRQ: u8 = interrupts::IRQ_BASE_OFFSET + 0xC;

static MOUSE: Once<MouseInterruptParams> = Once::new();

struct MouseInterruptParams {
    mouse: PS2Mouse<'static>,
    queue: Queue<Event>,
}

/// Initialize the PS/2 mouse driver and register its interrupt handler.
/// 
/// ## Arguments
/// * `mouse`: a wrapper around mouse functionality and id, used by the mouse interrupt handler.
/// * `mouse_queue_producer`: the queue onto which the mouse interrupt handler
///    will push new mouse events when a mouse action occurs.
pub fn init(mut mouse: PS2Mouse<'static>, mouse_queue_producer: Queue<Event>) -> Result<(), &'static str> {
    // Set MouseId to the highest possible one
    if let Err(e) = mouse.set_mouse_id() {
        error!("Failed to set the mouse id: {e}");
        return Err("Failed to set the mouse id");
    }

    // Register the interrupt handler
    interrupts::register_interrupt(PS2_MOUSE_IRQ, ps2_mouse_handler).map_err(|e| {
        error!("PS/2 mouse IRQ {PS2_MOUSE_IRQ:#X} was already in use by handler {e:#X}! Sharing IRQs is currently unsupported.");
        "PS/2 mouse IRQ was already in use! Sharing IRQs is currently unsupported."
    })?;

    // Final step: set the producer end of the mouse event queue.
    // Also add the mouse struct for access during interrupts.
    MOUSE.call_once(|| MouseInterruptParams { mouse, queue: mouse_queue_producer });
    Ok(())
}

/// Paquet en cours d'assemblage, un octet à la fois (comme `psmouse` de Linux).
struct Assembler {
    bytes: [u8; 4],
    count: usize,
    /// Arrivée du dernier octet reçu.
    last: Instant,
}

static ASSEMBLER: Mutex<Assembler> = Mutex::new(Assembler { bytes: [0; 4], count: 0, last: Instant::ZERO });

/// Au-delà de ce silence entre deux octets d'un paquet, le paquet commencé est abandonné : le
/// premier octet suivant ouvre un nouveau paquet (`psmouse` de Linux : une demi-seconde ; les
/// octets d'un même paquet arrivent à moins d'une milliseconde d'intervalle).
const PACKET_GAP: Duration = Duration::from_millis(50);

/// Octets lus au plus par interruption (le reste attend la suivante).
const MAX_BYTES_PER_IRQ: usize = 256;

/// Gestionnaire de l'IRQ 12 (souris PS/2).
///
/// Lit **un octet à la fois**, seulement quand le contrôleur signale un octet de la souris
/// (`output_buffer_full` et `mouse_output_buffer_full`) : un octet du clavier n'est jamais pris,
/// et le port de données n'est jamais lu à vide (un octet volé décalait tous les paquets
/// suivants, qui échouaient tous au contrôle du bit 3 : souris figée). Resynchronisation : le
/// premier octet d'un paquet doit avoir son bit 3 à un et aucun débordement ; sinon il est
/// sauté. Un paquet commencé et resté incomplet plus de [`PACKET_GAP`] est abandonné.
extern "x86-interrupt" fn ps2_mouse_handler(_stack_frame: InterruptStackFrame) {
    if let Some(MouseInterruptParams { mouse, queue }) = MOUSE.get() {
        // Un seul cœur assemble à la fois (l'IRQ 12 ne va qu'à un cœur ; prudence).
        if let Some(mut asm) = ASSEMBLER.try_lock() {
            let len = mouse.packet_len();
            for _ in 0..MAX_BYTES_PER_IRQ {
                let Some(byte) = mouse.try_read_byte() else { break };
                let now = Instant::now();
                if asm.count > 0 && now.duration_since(asm.last) > PACKET_GAP {
                    asm.count = 0;
                }
                asm.last = now;
                // Premier octet : bit 3 toujours à un ; un débordement annoncé (bits 6 et 7)
                // avec un gros mouvement est rare, mais un octet de mouvement pris pour un
                // premier octet a souvent ces bits : on les refuse aussi.
                if asm.count == 0 && (byte & 0x08 == 0 || byte & 0xC0 != 0) {
                    continue;
                }
                let i = asm.count;
                asm.bytes[i] = byte;
                asm.count += 1;
                if asm.count == len {
                    asm.count = 0;
                    let packet = mouse.packet_from_bytes(asm.bytes);
                    let _ = handle_mouse_input(packet, queue);
                }
            }
        }
    }
    interrupts::eoi(PS2_MOUSE_IRQ);
}


/// enqueue a Mouse Event according to the data
fn handle_mouse_input(mouse_packet: MousePacket, queue: &Queue<Event>) -> Result<(), &'static str> {
    let buttons = Buttons::from(&mouse_packet).0;
    let movement = Movement::from(&mouse_packet).0;

    let mouse_event = MouseEvent::new(buttons, movement);
    let event = Event::MouseMovementEvent(mouse_event);

    let r = queue.push(event).map_err(|_| "failed to enqueue the mouse event");
    event_types::notify_input();
    r
}

// both MouseMovementRelative and MousePacketBits4 are in different crates, so we need a newtype wrapper:
struct Movement(MouseMovementRelative);
impl From<&MousePacket> for Movement {
    fn from(mouse_packet: &MousePacket) -> Self {
        // PS/2 : `z` négatif vers le haut ; MaiOS (et `REL_WHEEL` de Linux) : positif vers le
        // haut, d'où l'opposé (comme `psmouse`).
        Self(MouseMovementRelative::new(
            mouse_packet.x_movement(),
            mouse_packet.y_movement(),
            mouse_packet.scroll_movement().saturating_neg()
        ))
    }
}

// both MouseButtons and MousePacketBits4 are in different crates, so we need a newtype wrapper:
struct Buttons(MouseButtons);
impl From<&MousePacket> for Buttons {
    fn from(mouse_packet: &MousePacket) -> Self {
        Self(MouseButtons::new()
            .with_left(mouse_packet.button_left())
            .with_right(mouse_packet.button_right())
            .with_middle(mouse_packet.button_middle())
            .with_fourth(mouse_packet.button_4())
            .with_fifth(mouse_packet.button_5())
        )
    }
}
