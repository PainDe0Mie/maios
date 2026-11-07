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
# Place a valid IWAD/PWAD at the repository root as DOOM1.WAD.
# Build from WSL; the target prepares tlibc automatically.
make doom
```

## Run in MaiOS

```
run doom
# or: run /disk/apps/doom.elf
```

Copy `ports/doom/doom.elf` and the root `DOOM1.WAD` into `/disk/apps/` before running Doom.

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
