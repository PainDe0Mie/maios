# MaiOS — Instructions Claude Code

## Langue
Réponds toujours en français.

## Projet
MaiOS est un fork de Theseus OS écrit en Rust. C'est un OS kernel-space complet avec scheduler, mémoire virtuelle, réseau (smoltcp), audio (Intel HDA), et une couche de compatibilité Windows NT en cours de développement.

- **Toolchain** : nightly-2023-10-27 (pinné dans `rust-toolchain.toml`)
- **Build** : toujours via WSL — `wsl bash -lc "make iso"`
- **Architecture principale** : x86_64, support aarch64 secondaire

## Règles de workflow

### Builds
- Ne jamais lancer `make` directement depuis Windows — utiliser `wsl bash -lc "make iso"`
- Le build complet prend ~10 min, attendre sans relancer

### QEMU / Debug
- Pour le moniteur QEMU, **toujours utiliser** `qemu-monitor.ps1` — ne jamais faire de commandes telnet inline
- `build-and-run.ps1` lance QEMU avec réseau + audio Intel HDA

### Git
- Pas de PR nécessaire — merger directement sur `main` ou `develop`
- Style commits sémantique : `feat:`, `fix:`, `test:`, `debug:`, `refactor:`
- Branches : `feature/*`, `fix/*`

### Linting
- `make clippy ARCH=x86_64` — lint x86_64
- `make clippy ARCH=aarch64` — lint aarch64
- CI bloque sur `-D clippy::all`

## Architecture syscall
Syscalls centralisés dans `kernel/maios_syscall/`. Les mappers Linux/Windows sont des shims minces qui délèguent à `maios_syscall`.

## Sprints en cours
Voir `.claude/plans/` pour les plans détaillés. Roadmap dans la mémoire (`ROADMAP.md`).
