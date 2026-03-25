/**
 * doomgeneric_maios.c — MaiOS platform layer for doomgeneric.
 *
 * doomgeneric (https://github.com/ozkl/doomgeneric) requires exactly
 * 4 functions to be implemented per platform:
 *
 *   DG_Init()       — initialize display
 *   DG_DrawFrame()  — blit Doom's framebuffer to the screen
 *   DG_SleepMs()    — sleep for N milliseconds
 *   DG_GetKey()     — return the next key event (press/release)
 *   DG_GetTicksMs() — return monotonic time in milliseconds
 *   DG_SetWindowTitle() — set window title (no-op for us)
 *
 * Doom renders internally at 320×200 (SCREENWIDTH × SCREENHEIGHT).
 * We scale up to fill the MaiOS framebuffer using nearest-neighbor.
 *
 * Build:
 *   x86_64-elf-gcc -nostdlib -nostartfiles -mno-red-zone \
 *     -I../../tlibc/include \
 *     doomgeneric/*.c doomgeneric_maios.c \
 *     ../../tlibc/target/x86_64-unknown-theseus/release/libtlibc.a \
 *     -o doom.elf -Wl,--emit-relocs
 *
 * Run in MaiOS:
 *   loadc /namespaces/_executables/doom.elf
 */

#include "maios.h"
#include <string.h>
#include <stdlib.h>
#include <stdint.h>

/* doomgeneric expects these to be defined by the platform */
#include "doomgeneric.h"
#include "doomkeys.h"

/* ═══════════════════════════════════════════════════════════════════════════
 * State
 * ═══════════════════════════════════════════════════════════════════════════ */

static maios_framebuffer_info_t fb_info;
static uint32_t *framebuffer_ptr;
static uint32_t  screen_w, screen_h;
static uint32_t  scale_x, scale_y;

/* Key event queue for DG_GetKey() */
#define MAX_PENDING_KEYS 32
static struct {
    unsigned char key;
    unsigned char pressed;
} pending_keys[MAX_PENDING_KEYS];
static int pending_key_count = 0;

/* ═══════════════════════════════════════════════════════════════════════════
 * Keycode translation: MaiOS scancode → Doom key
 * ═══════════════════════════════════════════════════════════════════════════ */

static unsigned char translate_key(uint32_t maios_keycode, uint32_t ascii) {
    /* If it's a printable ASCII character, Doom uses the ASCII value directly */
    if (ascii >= 0x20 && ascii <= 0x7E) {
        /* Doom wants lowercase for letter keys */
        if (ascii >= 'A' && ascii <= 'Z')
            return ascii + 32;
        return (unsigned char)ascii;
    }

    switch (maios_keycode) {
        case MAIOS_KEY_ESCAPE:    return KEY_ESCAPE;
        case MAIOS_KEY_ENTER:     return KEY_ENTER;
        case MAIOS_KEY_TAB:       return KEY_TAB;
        case MAIOS_KEY_BACKSPACE: return KEY_BACKSPACE;
        case MAIOS_KEY_CTRL:      return KEY_FIRE;       /* Ctrl = Fire */
        case MAIOS_KEY_SPACE:     return KEY_USE;        /* Space = Use/Open */
        case MAIOS_KEY_LSHIFT:
        case MAIOS_KEY_RSHIFT:    return KEY_RSHIFT;     /* Shift = Run */
        case MAIOS_KEY_ALT:       return KEY_LALT;       /* Alt = Strafe */
        case MAIOS_KEY_UP:        return KEY_UPARROW;
        case MAIOS_KEY_DOWN:      return KEY_DOWNARROW;
        case MAIOS_KEY_LEFT:      return KEY_LEFTARROW;
        case MAIOS_KEY_RIGHT:     return KEY_RIGHTARROW;
        case MAIOS_KEY_F1:        return KEY_F1;
        case MAIOS_KEY_F2:        return KEY_F2;
        case MAIOS_KEY_F3:        return KEY_F3;
        case MAIOS_KEY_F4:        return KEY_F4;
        case MAIOS_KEY_F5:        return KEY_F5;
        case MAIOS_KEY_F6:        return KEY_F6;
        case MAIOS_KEY_F7:        return KEY_F7;
        case MAIOS_KEY_F8:        return KEY_F8;
        case MAIOS_KEY_F9:        return KEY_F9;
        case MAIOS_KEY_F10:       return KEY_F10;
        case MAIOS_KEY_F11:       return KEY_F11;
        case MAIOS_KEY_F12:       return KEY_F12;
        case MAIOS_KEY_MINUS:     return KEY_MINUS;
        case MAIOS_KEY_EQUALS:    return KEY_EQUALS;
        default:                  return 0;
    }
}

/* ═══════════════════════════════════════════════════════════════════════════
 * doomgeneric platform interface
 * ═══════════════════════════════════════════════════════════════════════════ */

void DG_Init(void) {
    /* Map the MaiOS framebuffer */
    if (maios_map_framebuffer(&fb_info) < 0) {
        /* Fatal: can't get framebuffer */
        return;
    }

    framebuffer_ptr = (uint32_t *)(uintptr_t)fb_info.ptr;
    screen_w = fb_info.width;
    screen_h = fb_info.height;

    /* Calculate integer scale factor (nearest-neighbor upscale) */
    scale_x = screen_w / DOOMGENERIC_RESX;
    scale_y = screen_h / DOOMGENERIC_RESY;
    if (scale_x == 0) scale_x = 1;
    if (scale_y == 0) scale_y = 1;
    /* Use the smaller scale to maintain aspect ratio */
    if (scale_x > scale_y) scale_x = scale_y;
    else                    scale_y = scale_x;
}

void DG_DrawFrame(void) {
    if (!framebuffer_ptr) return;

    /* Doom's internal buffer: DG_ScreenBuffer is DOOMGENERIC_RESX × DOOMGENERIC_RESY
     * in XRGB8888 format (same as BGRA8888 on little-endian x86). */
    uint32_t *src = DG_ScreenBuffer;
    uint32_t  sx = scale_x;
    uint32_t  sy = scale_y;

    /* Center the scaled image on screen */
    uint32_t offset_x = (screen_w - DOOMGENERIC_RESX * sx) / 2;
    uint32_t offset_y = (screen_h - DOOMGENERIC_RESY * sy) / 2;

    /* Clear borders to black (only first frame, or always — cheap enough) */
    /* Nearest-neighbor scale blit */
    for (uint32_t doom_y = 0; doom_y < DOOMGENERIC_RESY; doom_y++) {
        uint32_t *src_row = &src[doom_y * DOOMGENERIC_RESX];
        uint32_t  dst_y_base = offset_y + doom_y * sy;

        for (uint32_t doom_x = 0; doom_x < DOOMGENERIC_RESX; doom_x++) {
            uint32_t pixel = src_row[doom_x];
            uint32_t dst_x_base = offset_x + doom_x * sx;

            /* Write scaled pixel block */
            for (uint32_t ry = 0; ry < sy; ry++) {
                uint32_t row_offset = (dst_y_base + ry) * screen_w + dst_x_base;
                for (uint32_t rx = 0; rx < sx; rx++) {
                    framebuffer_ptr[row_offset + rx] = pixel;
                }
            }
        }
    }

    /* Flush to display */
    maios_present(0);
}

void DG_SleepMs(uint32_t ms) {
    maios_sleep_ms(ms);
}

uint32_t DG_GetTicksMs(void) {
    return (uint32_t)maios_get_time_ms();
}

int DG_GetKey(int *pressed, unsigned char *doomKey) {
    /* First, drain MaiOS input queue into our pending buffer */
    while (pending_key_count < MAX_PENDING_KEYS) {
        maios_event_t ev;
        if (maios_get_event(&ev) <= 0) break;

        if (ev.type == MAIOS_EVENT_KEY_PRESS || ev.type == MAIOS_EVENT_KEY_RELEASE) {
            unsigned char dk = translate_key(ev.keycode, ev.ascii);
            if (dk != 0) {
                pending_keys[pending_key_count].key = dk;
                pending_keys[pending_key_count].pressed =
                    (ev.type == MAIOS_EVENT_KEY_PRESS) ? 1 : 0;
                pending_key_count++;
            }
        }
    }

    /* Return the oldest pending key event */
    if (pending_key_count > 0) {
        *pressed = pending_keys[0].pressed;
        *doomKey = pending_keys[0].key;
        /* Shift remaining events down */
        pending_key_count--;
        for (int i = 0; i < pending_key_count; i++) {
            pending_keys[i] = pending_keys[i + 1];
        }
        return 1;
    }

    return 0;
}

void DG_SetWindowTitle(const char *title) {
    /* No-op — MaiOS doesn't have per-window titles from C programs yet */
    (void)title;
}
