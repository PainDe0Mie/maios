/**
 * maios.h — MaiOS native syscall interface for C programs.
 *
 * Provides access to graphics (framebuffer, present, vsync),
 * audio (PCM write), and input (keyboard events).
 *
 * Usage:
 *   #include "maios.h"
 *   maios_framebuffer_info_t fb;
 *   maios_map_framebuffer(&fb);
 *   // Draw pixels into fb.ptr as BGRA8888...
 *   maios_present(0);
 */

#ifndef _MAIOS_H
#define _MAIOS_H

#include "stdint.h"

#ifdef __cplusplus
extern "C" {
#endif

/* ═══════════════════════════════════════════════════════════════════════════
 * Syscall numbers (MaiOS native ABI, 0x08xx range)
 * ═══════════════════════════════════════════════════════════════════════════ */

#define SYS_CREATE_WINDOW       0x0800
#define SYS_DESTROY_WINDOW      0x0801
#define SYS_MAP_FRAMEBUFFER     0x0802
#define SYS_PRESENT             0x0803
#define SYS_GET_EVENT           0x0804
#define SYS_AUDIO_WRITE         0x0805
#define SYS_VSYNC_WAIT          0x0806

/* ═══════════════════════════════════════════════════════════════════════════
 * Framebuffer
 * ═══════════════════════════════════════════════════════════════════════════ */

/** Pixel format constants */
#define MAIOS_PIXEL_FORMAT_BGRA8888  0

/** Framebuffer info returned by maios_map_framebuffer(). */
typedef struct {
    uint64_t ptr;       /**< Pointer to backbuffer (BGRA8888 pixels) */
    uint32_t width;     /**< Width in pixels */
    uint32_t height;    /**< Height in pixels */
    uint32_t stride;    /**< Stride in bytes (width * 4) */
    uint32_t format;    /**< Pixel format (0 = BGRA8888) */
    uint64_t reserved;
} __attribute__((packed)) maios_framebuffer_info_t;

/**
 * Map the graphics backbuffer and fill `info`.
 * After this call, you can write BGRA8888 pixels directly to info->ptr.
 * Returns 0 on success, negative on error.
 */
static inline int maios_map_framebuffer(maios_framebuffer_info_t *info) {
    /* In Theseus single-address-space, we call the kernel function directly.
     * For loadc-loaded ELF programs, this will be resolved at link time
     * against the tlibc stub that invokes the kernel. */
    extern int _maios_syscall(int nr, uint64_t a0, uint64_t a1, uint64_t a2,
                              uint64_t a3, uint64_t a4, uint64_t a5);
    return _maios_syscall(SYS_MAP_FRAMEBUFFER, (uint64_t)info, 0, 0, 0, 0, 0);
}

/**
 * Flush the backbuffer to the display.
 * @param flags  0 = normal mode, 1 = fullscreen exclusive
 * Returns 0 on success.
 */
static inline int maios_present(int flags) {
    extern int _maios_syscall(int nr, uint64_t a0, uint64_t a1, uint64_t a2,
                              uint64_t a3, uint64_t a4, uint64_t a5);
    return _maios_syscall(SYS_PRESENT, (uint64_t)flags, 0, 0, 0, 0, 0);
}

/**
 * Block until the next VSync (~16.67ms at 60Hz).
 * Returns the current frame counter.
 */
static inline uint64_t maios_vsync_wait(void) {
    extern int _maios_syscall(int nr, uint64_t a0, uint64_t a1, uint64_t a2,
                              uint64_t a3, uint64_t a4, uint64_t a5);
    return (uint64_t)_maios_syscall(SYS_VSYNC_WAIT, 0, 0, 0, 0, 0, 0);
}

/* ═══════════════════════════════════════════════════════════════════════════
 * Input events
 * ═══════════════════════════════════════════════════════════════════════════ */

/** Event types */
#define MAIOS_EVENT_NONE        0
#define MAIOS_EVENT_KEY_PRESS   1
#define MAIOS_EVENT_KEY_RELEASE 2

/** Modifier flags */
#define MAIOS_MOD_SHIFT   1
#define MAIOS_MOD_CTRL    2
#define MAIOS_MOD_ALT     4

/** Input event structure (16 bytes, matches kernel RawInputEvent). */
typedef struct {
    uint32_t type;      /**< MAIOS_EVENT_KEY_PRESS or MAIOS_EVENT_KEY_RELEASE */
    uint32_t keycode;   /**< Scancode-based keycode */
    uint32_t ascii;     /**< ASCII character (0 if not printable) */
    uint32_t modifiers; /**< Bitmask of MAIOS_MOD_* */
} maios_event_t;

/**
 * Poll for the next input event.
 * Returns 1 if an event was available (written to `ev`), 0 if none.
 */
static inline int maios_get_event(maios_event_t *ev) {
    extern int _maios_syscall(int nr, uint64_t a0, uint64_t a1, uint64_t a2,
                              uint64_t a3, uint64_t a4, uint64_t a5);
    return _maios_syscall(SYS_GET_EVENT, 0, (uint64_t)ev, 0, 0, 0, 0);
}

/* ═══════════════════════════════════════════════════════════════════════════
 * Keycodes (matching Theseus keycodes_ascii::Keycode)
 *
 * These are scancode set 1 based. For Doom, we map these to DOOM keys
 * in the doomgeneric platform layer.
 * ═══════════════════════════════════════════════════════════════════════════ */

#define MAIOS_KEY_ESCAPE     1
#define MAIOS_KEY_1          2
#define MAIOS_KEY_2          3
#define MAIOS_KEY_3          4
#define MAIOS_KEY_4          5
#define MAIOS_KEY_5          6
#define MAIOS_KEY_6          7
#define MAIOS_KEY_7          8
#define MAIOS_KEY_8          9
#define MAIOS_KEY_9          10
#define MAIOS_KEY_0          11
#define MAIOS_KEY_MINUS      12
#define MAIOS_KEY_EQUALS     13
#define MAIOS_KEY_BACKSPACE  14
#define MAIOS_KEY_TAB        15
#define MAIOS_KEY_Q          16
#define MAIOS_KEY_W          17
#define MAIOS_KEY_E          18
#define MAIOS_KEY_R          19
#define MAIOS_KEY_T          20
#define MAIOS_KEY_Y          21
#define MAIOS_KEY_U          22
#define MAIOS_KEY_I          23
#define MAIOS_KEY_O          24
#define MAIOS_KEY_P          25
#define MAIOS_KEY_ENTER      28
#define MAIOS_KEY_CTRL       29
#define MAIOS_KEY_A          30
#define MAIOS_KEY_S          31
#define MAIOS_KEY_D          32
#define MAIOS_KEY_F          33
#define MAIOS_KEY_G          34
#define MAIOS_KEY_H          35
#define MAIOS_KEY_J          36
#define MAIOS_KEY_K          37
#define MAIOS_KEY_L          38
#define MAIOS_KEY_LSHIFT     42
#define MAIOS_KEY_Z          44
#define MAIOS_KEY_X          45
#define MAIOS_KEY_C          46
#define MAIOS_KEY_V          47
#define MAIOS_KEY_B          48
#define MAIOS_KEY_N          49
#define MAIOS_KEY_M          50
#define MAIOS_KEY_RSHIFT     54
#define MAIOS_KEY_ALT        56
#define MAIOS_KEY_SPACE      57
#define MAIOS_KEY_F1         59
#define MAIOS_KEY_F2         60
#define MAIOS_KEY_F3         61
#define MAIOS_KEY_F4         62
#define MAIOS_KEY_F5         63
#define MAIOS_KEY_F6         64
#define MAIOS_KEY_F7         65
#define MAIOS_KEY_F8         66
#define MAIOS_KEY_F9         67
#define MAIOS_KEY_F10        68
#define MAIOS_KEY_F11        87
#define MAIOS_KEY_F12        88
#define MAIOS_KEY_UP         72
#define MAIOS_KEY_DOWN       80
#define MAIOS_KEY_LEFT       75
#define MAIOS_KEY_RIGHT      77

/* ═══════════════════════════════════════════════════════════════════════════
 * Audio
 * ═══════════════════════════════════════════════════════════════════════════ */

/**
 * Write PCM audio data to the kernel mixer.
 * @param buf   Pointer to interleaved i16 LE stereo PCM data at 48kHz
 * @param len   Length in bytes (must be multiple of 4)
 * Returns number of bytes written, or negative on error.
 */
static inline int maios_audio_write(const void *buf, uint32_t len) {
    extern int _maios_syscall(int nr, uint64_t a0, uint64_t a1, uint64_t a2,
                              uint64_t a3, uint64_t a4, uint64_t a5);
    return _maios_syscall(SYS_AUDIO_WRITE, (uint64_t)buf, (uint64_t)len, 0, 0, 0, 0);
}

/* ═══════════════════════════════════════════════════════════════════════════
 * Fullscreen exclusive mode
 * ═══════════════════════════════════════════════════════════════════════════ */

/**
 * Enable fullscreen exclusive mode — maps a user buffer directly.
 * @param buf     Pointer to BGRA8888 pixel buffer
 * @param width   Buffer width in pixels
 * @param height  Buffer height in pixels
 */
static inline int maios_set_fullscreen(void *buf, uint32_t width, uint32_t height) {
    extern int _maios_syscall(int nr, uint64_t a0, uint64_t a1, uint64_t a2,
                              uint64_t a3, uint64_t a4, uint64_t a5);
    return _maios_syscall(SYS_GET_EVENT, 1, (uint64_t)buf, (uint64_t)width,
                          (uint64_t)height, 0, 0);
}

/** Disable fullscreen exclusive mode. */
static inline int maios_clear_fullscreen(void) {
    extern int _maios_syscall(int nr, uint64_t a0, uint64_t a1, uint64_t a2,
                              uint64_t a3, uint64_t a4, uint64_t a5);
    return _maios_syscall(SYS_GET_EVENT, 2, 0, 0, 0, 0, 0);
}

/* ═══════════════════════════════════════════════════════════════════════════
 * Timing helpers
 * ═══════════════════════════════════════════════════════════════════════════ */

/** Sleep for the given number of milliseconds. */
static inline void maios_sleep_ms(uint32_t ms) {
    extern int usleep(unsigned int usec);
    usleep(ms * 1000);
}

/** Get monotonic time in milliseconds. */
static inline uint64_t maios_get_time_ms(void) {
    extern long clock(void);
    return (uint64_t)(clock() / 1000);  /* clock() returns microseconds */
}

#ifdef __cplusplus
}
#endif

#endif /* _MAIOS_H */
