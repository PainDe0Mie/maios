/// The maximum resolution `(width, height)` of the graphical framebuffer, in pixels,
/// **requested from the bootloader at compile time** (embedded in the multiboot header).
///
/// ## Important: this is a hint, not a guarantee
/// The bootloader may provide any resolution it sees fit. The *actual* framebuffer
/// dimensions are reported at runtime via [`boot_info::FramebufferInfo`] and must
/// always be used instead of this constant for any rendering logic.
///
/// ## Why can't this be fully runtime-adaptive?
/// On BIOS/multiboot2 systems, the requested resolution is embedded in the binary
/// itself (`multiboot_header.asm`). It must be set before the kernel runs.
/// UEFI systems are already fully adaptive: the bootloader negotiates the resolution
/// directly with the GOP driver and reports it to us at runtime.
///
/// ## Tuning for your hardware
/// Set this to the highest resolution you want to support. The bootloader will
/// pick the closest available mode. Common values:
/// - `(1920, 1080)` — Full HD (recommended for modern hardware)
/// - `(2560, 1440)` — QHD
/// - `(1280, 1024)` — conservative/VM default (previous value)
///
/// This must also be mirrored in:
/// `kernel/nano_core/src/asm/bios/multiboot_header.asm`
pub const FRAMEBUFFER_MAX_RESOLUTION: (u16, u16) = (1920, 1080);

/// Minimum acceptable framebuffer width in pixels.
///
/// If the bootloader provides a framebuffer narrower than this,
/// the kernel may fall back to text mode or refuse to use it.
pub const FRAMEBUFFER_MIN_WIDTH: u16 = 640;

/// Minimum acceptable framebuffer height in pixels.
pub const FRAMEBUFFER_MIN_HEIGHT: u16 = 480;

/// Preferred number of bits per pixel.
///
/// 32 bpp (ARGB8888 / XRGB8888) is the standard for modern framebuffers.
/// 24 bpp saves memory but is rarely supported natively and requires packing.
pub const FRAMEBUFFER_PREFERRED_BPP: u8 = 32;
