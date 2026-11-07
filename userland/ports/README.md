# `userland/ports/` — programmes tiers portés sur MaiOS

Programmes C (ou autres) compilés contre la libc MaiOS (`userland/libc`) et chargés par `loadc`.

| Port | Rôle |
|---|---|
| `doom/` | doomgeneric : premier test de bout en bout de l'OS (loader ELF, malloc, fichiers, framebuffer, clavier). |

Les crates Rust tierces patchées pour MaiOS (`backtrace`, `region`, `core2`, …) sont dans `libs/`.
