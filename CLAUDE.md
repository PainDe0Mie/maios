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

## Arborescence
- `kernel/<sous-système>/<crate>` : `boot`, `arch/{common,x86_64,aarch64}`, `mm`, `sched`, `sync`, `loader`, `fs`,
  `drivers/{bus,storage,net,input,audio,serial}`, `net`, `gfx`, `console`, `abi`, `system`, `tests`.
  Un nouveau sous-système doit être ajouté aux `members` du `Cargo.toml` racine.
- `userland/apps` (commandes et applis), `userland/tests` (tests et benchmarks), `userland/libc` (ex-tlibc),
  `userland/libmaios`, `userland/std`, `userland/ports/doom`.
- `libs/` crates no_std génériques et forks tiers ; `assets/` (DOOM1.WAD) ; `docs/` ; `maios_features/`.
- Plans : `.claude/plans/` (01 réorganisation, 02 gestionnaire mémoire MaiMM, 03 ordonnanceur MaiSched).

## Mémoire (MaiMM) et mesures
- Frames physiques : `kernel/mm/llfree` (cœur sans verrou, sans dépendance, testé sur l'hôte),
  branché dans `kernel/mm/frame_allocator` par `enable_llfree()` (appelé par `memory::init_post_heap`).
  Tests hôte : copier la crate hors du workspace et lancer `cargo test` (voir les tests dans `src/tests.rs`).
- Pool de frames pré-zéroées : `frame_allocator::allocate_zeroed_frame()`, rempli par la tâche idle
  (`memory::refill_zeroed_frames`).
- Tas noyau : `kernel/mm/kalloc` (cœur façon mimalloc : segments de 2 Mio, listes libres par page,
  libérations distantes sans verrou, liens chiffrés ; sans dépendance, testé sur l'hôte comme
  `llfree`), branché par `kernel/mm/heap` (`heap::enable_kalloc`, appelé par `captain`). Règles :
  interruptions coupées autour de tout appel à kalloc ; rien qui touche aux tables de pages dans le
  chemin de libération (c'est `heap::idle_maintenance`, dans la tâche inactive, qui rend les
  segments). Objets de 128 Kio à 1 Mio : pages larges dans les segments ; au-delà : mappage direct. Mesures : `heap_bench` (`--tests heap`).
- Mesures : `python scripts/maios_bench.py` (QEMU sans affichage : `mm_bench` + `doom -timedemo demo1`,
  comparaison à `scripts/bench_baseline.json`, `--save-baseline` pour la mettre à jour, `--shots` pour
  les captures). Après tout changement de MaiMM : relancer le harnais, une régression Doom > 5 % échoue.
- **Mesurer sous KVM** (rapide, environ 1 min par exécution, stable à 1–2 %) : `python scripts/maios_bench.py --kvm`
  lance QEMU dans WSL (Debian, `qemu-system-x86` installé, utilisateur dans le groupe `kvm`) avec
  `-cpu host` ; référence séparée `scripts/bench_baseline_kvm.json`. TCG (sans `--kvm`) n'est pas
  fiable pour les FPS de Doom (18 à 40 FPS sur la même ISO).
- Le journal série n'affiche que `warn!` et au-dessus. Le terminal ne gère pas encore les accents :
  les sorties écran des commandes restent en ASCII.

## Démarrage (Limine)
- Chargeur : **Limine 12** (protocole Limine natif, révision 6), une seule ISO hybride BIOS + UEFI
  (CD, clé USB, disque). Compilé une fois par `scripts/fetch_limine.sh` (version épinglée, SHA-256
  vérifié) dans `tools/limine/bin` (ignoré par git), appelé automatiquement par `make iso`.
  GRUB/Multiboot2 reste disponible : `make iso bootloader=grub`.
- Image : `kernel/boot/nano_core/linker_limine-x86_64.ld` (tout en moitié haute), entrée 64 bits
  `src/asm/limine/boot.asm`, requêtes et copie des réponses dans `nano_core/src/limine.rs`
  (vers `boot_info::limine::LimineInfo`, section `.boot_info`). Les modules sont une seule archive
  `modules.cpio.lz4`, extraite par `mod_mgmt` sans passer par le tas. `cfg/limine.conf` porte les
  empreintes BLAKE2b du noyau et de l'archive (posées par `make`).
- Cœurs secondaires : démarrés par Limine (MP) et libérés via `goto_address`
  (`multicore_bringup::handle_limine_aps`) ; plus de trampoline 16 bits.
- Pièges : la GDT est en lecture seule une fois nos tables en place (bit « accessed » pré-posé) ;
  dans une entrée de grande page, le bit 12 est PAT, pas une adresse (`mapper::translate_page`).
- Tester l'UEFI : ajouter à QEMU (WSL) `-drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd`.
- Après un changement du noyau, Doom doit être relié de nouveau : `bash scripts/rebuild_all.sh` (ISO → tlibc →
  doom.elf → ISO).

## Ordonnanceur (MaiSched)
- Cœur algorithmique : `kernel/sched/maisched` (EEVDF à arbre AVL intrusif augmenté, tranches
  adaptatives, temps réel O(1), topologie), sans dépendance, testé sur l'hôte comme `llfree`.
- Partie noyau : `kernel/sched/task/src/scheduler.rs` (files par cœur, réveil sans perte, tick,
  vol de tâches) ; interruptions et IPI de replanification : `kernel/sched/scheduler`.
- Règles : ne jamais modifier une entité hors du verrou de la file du cœur `task.sched.cpu` ;
  `block()` sur la tâche courante ne la retire pas (c'est `schedule()` qui le fait) ; se bloquer
  *avant* de s'inscrire à une source de réveil.
- Mesures : `sched_bench` (`--tests sched` dans le harnais), plan : `.claude/plans/03-maisched-ordonnanceur.md`.

## Processus en mode utilisateur (MaiProc)
- Modèle hybride : applis natives Rust dans l'espace partagé du noyau ; binaires Linux en anneau 3,
  un espace d'adresses par processus. Plan : `.claude/plans/04-processus-utilisateur.md`.
- Espace virtuel : P4 0–255 utilisateur ; 256–287 allocations dynamiques du noyau ; 384–387
  physmap (toute la RAM, pages de 2 Mio) ; 509 tas ; 511 image. Les P3 partagées sont créées une
  fois (`paging::kernel_half::seal`) ; pages du noyau globales ; PCID (`paging::address_space`).
- Entrées : `kernel/arch/x86_64/uentry` (IDT à talons, `syscall`, NMI paranoïaque, SMEP/SMAP,
  XSAVE ; `run_user`/`exit_user`). Mémoire : `kernel/mm/uvm`. Processus et ABI Linux :
  `kernel/system/maiproc` (`src/linux/*.rs`). `run` lance les ELF Linux autonomes en processus.
- La moitié basse n'appartient qu'aux processus : le noyau n'y mappe plus rien après
  l'initialisation (les tâches noyau gardent l'espace du dernier processus chargé, TLB
  paresseuse). Les programmes C chargés par `loadc` (Doom) sont liés dans la moitié haute
  (`-Ttext-segment=0xffff8f8000000000`, entrée P4 287) ; `loadc` refuse un lien en moitié basse.
  `paging::get_current_p4()` renvoie le P4 du noyau même quand CR3 désigne un processus.
- Règles : jamais de pointeur utilisateur déréférencé dans le noyau, tout passe par
  `Mm::copy_{to,from}_user`/`read_user`/`write_user` ; une sortie de fil (`exit_user`) ne doit
  rien posséder sur la pile (les destructeurs ne tournent pas) ; FSGSBASE requis.
- Tests : `bash userland/tests/linux_abi/build.sh` (WSL, musl et BusyBox extraits sans sudo dans
  `~/maios_pkgs`, binaires dans `extra_files/linux/`), puis
  `python scripts/maios_bench.py --kvm --tests linux` ; `--cpu host,-pcid,...` masque des
  fonctions du processeur invité. Les FPS de Doom varient de ±15 % d'une session à l'autre
  (cœurs P/E de l'hôte) : refaire 2–3 mesures avant de conclure à une régression.

## Architecture syscall
Syscalls centralisés dans `kernel/abi/maios_syscall/`. Les mappers Linux/Windows sont des shims minces qui délèguent à `maios_syscall`.

## Sprints en cours
Voir `.claude/plans/` pour les plans détaillés. Roadmap dans la mémoire (`ROADMAP.md`).
