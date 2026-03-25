# Doom on MaiOS

Port of the original Doom (1993) running natively on MaiOS via [doomgeneric](https://github.com/ozkl/doomgeneric).

## How it works

Doom uses a **software renderer** — it draws every pixel in RAM, then blits to the screen. No GPU or OpenGL needed.

MaiOS provides:
- `SYS_MAP_FRAMEBUFFER` → direct access to a BGRA8888 backbuffer
- `SYS_PRESENT` → flush to display
- `SYS_GET_EVENT` → keyboard input
- `SYS_AUDIO_WRITE` → PCM audio output
- Full libc (tlibc) with malloc, printf, FILE*, time, setjmp...

## Build

```bash
# 1. Clone doomgeneric
git clone https://github.com/ozkl/doomgeneric

# 2. Build tlibc (from repo root)
make tlibc

# 3. Build doom
make

# 4. Get DOOM1.WAD (shareware, free)
# Place DOOM1.WAD in the MaiOS disk image filesystem
```

## Run in MaiOS

```
loadc /namespaces/_executables/doom.elf
```

## Controls

| Key | Action |
|-----|--------|
| Arrow keys | Move / Turn |
| Ctrl | Fire |
| Space | Use / Open door |
| Shift | Run |
| 1-7 | Select weapon |
| Escape | Menu |
| Tab | Automap |
| Enter | Confirm |
