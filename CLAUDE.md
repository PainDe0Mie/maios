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
- `run.ps1 -RunOnly -Profile <nom>` émule une machine type (`-ListProfiles` : moderne, ancien,
  retro… ; `-Ram`, `-Cpus`, `-Cpu`, `-Machine`, `-Gpu` pour ajuster) ; WHPX par défaut sous Windows
  (`kernel-irqchip=off`) : tester aussi là ce qui touche au processeur (CPUID, XSAVE).

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
  `drivers/{bus,storage,net,input,audio,serial,gpu}`, `net`, `gfx`, `console`, `abi`, `system`, `tests`.
  Un nouveau sous-système doit être ajouté aux `members` du `Cargo.toml` racine.
- `userland/apps` (commandes et applis), `userland/tests` (tests et benchmarks), `userland/libc` (ex-tlibc),
  `userland/libmaios`, `userland/std`, `userland/ports/doom`.
- `libs/` crates no_std génériques et forks tiers ; `assets/` (DOOM1.WAD) ; `docs/` ; `maios_features/`.
- Plans : `.claude/plans/` (01 réorganisation, 02 gestionnaire mémoire MaiMM, 03 ordonnanceur MaiSched, 04 processus MaiProc, 05 stockage MaiSS, 06 MaiLand, 07 Steam, 08 GPU MaiGPU).

## Mémoire (MaiMM) et mesures
- Frames physiques : `kernel/mm/llfree` (cœur sans verrou, sans dépendance, testé sur l'hôte),
  branché dans `kernel/mm/frame_allocator` par `enable_llfree()` (appelé par `memory::init_post_heap`).
  Tests hôte : copier la crate hors du workspace et lancer `cargo test` (voir les tests dans `src/tests.rs`).
- Pool de frames pré-zéroées : `frame_allocator::allocate_zeroed_frame()`, rempli par la tâche idle
  (`memory::refill_zeroed_frames`) : mise à zéro par la physmap (`movnti` + `sfence`, sans
  mappage), arrêtée sous une réserve de frames libres (`RESERVE_FRAMES`). Création de tables de
  pages faillible : `Table::try_next_table_create`, et `Mapper::prepare_tables` crée toutes les
  tables avant d'écrire la moindre PTE (plus de panique « no frames available » en mémoire pleine).
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
- Journal asynchrone (`kernel/console/logger`, façon `nbcon` de Linux) : après
  `log_flusher::start()` (fin de `captain::init`, le démarrage reste synchrone), `warn!` copie le
  message dans un anneau de 64 Kio et la tâche `log_flusher` l'émet par rafales du FIFO du 16550
  (115 200 bauds, `try_out_bytes`, jamais d'attente de la ligne). `error!` est émis avant de
  rendre la main ; anneau plein → le producteur vide lui-même (aucune perte) ;
  `emergency_print` (NMI, exceptions) appelle `logger::flush_emergency()`. Règle : **jamais de
  journal sous un verrou de file de MaiSched** (la sonnette réveille une tâche).
- Harnais sous WSL : QEMU écrit COM1 dans `~/.maios_bench/com1.log` (ext4 de la distribution),
  recopié dans `build/maios_bench_com1.log` par `Qemu.log()`. Sur `/mnt/e` (drvfs), chaque octet
  coûtait ~230 µs de vCPU arrêté : c'était l'essentiel des 31 ms d'un fork + exit mesurés.

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
  `block()` sur la tâche courante ne la retire pas (c'est son `schedule()` qui le fait, jamais une
  préemption : une tâche préemptée entre `block()` et son inscription reste en file) ; se bloquer
  *avant* de s'inscrire à une source de réveil.
- Numéro de cœur : toujours lu **après** avoir coupé préemption ou interruptions (`hold_preemption`
  incrémente puis lit, comme `get_cpu()`). Lu avant, une interruption pouvait migrer la tâche et
  `schedule()` travaillait sur la file d'un autre cœur (entité courante écrasée → tâche prête jamais
  choisie, `kill -9` sans effet ; tâche marquée « en cours » sur le mauvais cœur → panique).
- Diagnostic : `schedule()` vérifie que la file connaît `prev` comme courante et qu'aucune entité ne
  reste courante après `put_prev_task` ; `switch_to` que la commutation précédente est terminée ;
  `finish_task_switch` que la tâche rendue était marquée sur ce cœur. Chaque tâche garde sa
  dernière prise et sa dernière libération de cœur (`task::trace_string`, aussi dans le NMI).
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
- Taille de la zone XSAVE (`uentry/src/fpu.rs`, `standard_area_size`) : calculée composant par
  composant (`CPUID.(0xD,i)`, décalage + taille, comme `xstate_calculate_size` de Linux), jamais
  sur la seule `CPUID.(0xD,0).EBX` : QEMU sous WHPX la rend périmée (576 pour XCR0 = 7 alors que
  le processeur écrit AVX à 576) et `XSAVE` débordait sur l'objet voisin du tas (Firefox
  plantait le noyau sous WHPX seulement).
- La moitié basse n'appartient qu'aux processus : le noyau n'y mappe plus rien après
  l'initialisation (les tâches noyau gardent l'espace du dernier processus chargé, TLB
  paresseuse). Les programmes C chargés par `loadc` (Doom) sont liés dans la moitié haute
  (`-Ttext-segment=0xffff8f8000000000`, entrée P4 287) ; `loadc` refuse un lien en moitié basse.
  `paging::get_current_p4()` renvoie le P4 du noyau même quand CR3 désigne un processus.
- Règles : jamais de pointeur utilisateur déréférencé dans le noyau, tout passe par
  `Mm::copy_{to,from}_user`/`read_user`/`write_user` ; une sortie de fil (`exit_user`) ne doit
  rien posséder sur la pile (les destructeurs ne tournent pas) ; FSGSBASE requis.
- M2 (`fork`, `execve`, signaux, tubes, fils, futex) : `fork` partage les tables P1 en lecture
  seule (On-Demand-Fork, `uvm/src/pt.rs`), copie à l'écriture décidée par le compteur de
  références des frames (`uvm/src/refs.rs`, 0 = un seul propriétaire). Toute modification de
  tables passe par un `tlb::Flush` : frames rendues **après** invalidation, IPI aux seuls cœurs
  qui exécutent un fil de l'espace (masque `active` de l'`AddressSpace`, retiré quand le cœur
  passe à une tâche noyau). Le verrou d'un `Mm` coupe la préemption, **pas** les interruptions
  (un cœur qui l'attend doit servir l'IPI) : ne jamais toucher à la mémoire d'un processus sous
  un `IrqSafeMutex` (files de signaux, files d'attente), et les fautes utilisateur sont traitées
  interruptions actives.
- Processus (`maiproc/src/lib.rs`) : pid = id de la tâche du premier fil, zombie jusqu'au
  `wait4` du parent (ou `ProcessHandle::wait` pour un parent natif comme `run`). Signaux
  (`signal.rs`) livrés par le crochet de retour de `uentry` (mot `work` du fil, cadre
  `rt_sigframe` + XSAVE) ; attentes (`wait.rs`) : se bloquer, s'inscrire, revérifier, toujours
  en boucle ; codes de relance `ERESTART*` convertis au retour selon `SA_RESTART`.
- M3 : glibc dynamique (`extra_files/sysroot` monté à la racine par `mount_sysroot` : `/lib64`,
  `/lib`), cache de pages des fichiers (`uvm/src/pagecache.rs`, registre `maiproc/src/pagecache.rs` ;
  pages du cache projetées en lecture seule, copie à l'écriture par le compteur de références),
  vDSO (`maiproc/src/vdso.rs` : assembleur copié tel quel, `[vvar]` lu relativement à `rip` ;
  pages scellées), objets noyau (`object.rs` : `FileObject`, `Node::Object`) : `epoll.rs`
  (observateurs permanents des `WaitQueue`), `eventfd.rs`, `timerfd.rs`, `signalfd.rs`,
  `pidfd.rs`, `socket.rs` + `unix.rs` ; `/proc` produit à la lecture (`procfs.rs`,
  `Node::VirtDir`), charge moyenne (`loadavg.rs`). Règles : jamais deux verrous d'état de socket
  à la fois ; une frame du cache doit être comptée (`uvm::init()` avant la première page).
- Réseau de M3 :
  - `inet.rs` : TCP/UDP. Le chemin rapide local passe sans paquets ; le réseau réel passe par
    smoltcp, avec un *waker* par socket. Liaisons et règles de `SO_REUSEADDR`/`SO_REUSEPORT`.
  - `portsel.rs` : ports éphémères (RFC 6056 durcie, SipHash).
  - `netpoll.rs` : minuteurs sur la prochaine échéance de smoltcp ; connexions fermées menées à
    leur fin.
  - Ordre des verrous : interface smoltcp (`inner` puis `sockets`, bloquants, puis la carte,
    `IrqSafeMutex` pris aussi par son interruption : jamais tenu en attendant un verrou
    bloquant, sinon interblocage avec un fil interrompu qui tient `sockets`), table des ports,
    état d'une socket. Jamais d'appel à smoltcp sous un état de socket, jamais d'`Arc` de socket
    relâché sous un verrou de la pile.
- io_uring (`uring.rs`) :
  - anneaux dans des `ShmObject::from_frames`, lus et écrits par la physmap ;
  - requêtes tentées en ligne sans attente (`nowait` de `rw_iov`), puis sondage par observateur
    de `WaitQueue` ;
  - travailleurs : fils internes (`io_worker_thread`), annulés par `WORK_EXIT`.
- Carte e1000 : NAPI (`poll_interface` se bloque avant de revérifier `RX_PENDING` ; la boucle
  de `deferred_interrupt_tasks` tolère une action déjà bloquée). L'énumération PCI coupe INTx
  partout : un pilote à INTx doit le rétablir (`pci_enable_intx(true)`). DHCP asynchrone
  (`net::net_timer_task`, repli statique après 5 s). Un `ReceiveBuffer` rendu à la réserve
  retrouve sa pleine capacité (`nic_buffers`, champ `capacity`) ; une trame refusée est jetée
  sans bloquer l'anneau (`nic_queues`). Compteurs réels dans `/proc/net/dev`
  (`NetworkInterface::stats`).
- Stockage (MaiSS, plan 05) : couche bloc `kernel/drivers/storage/blk` (files par cœur,
  étiquettes, attente hybride, chien de garde), pilote `nvme` (files + MSI-X par cœur, PRP sans
  copie, cloches fantômes). Vecteurs MSI-X avec argument : `interrupts::register_msi_callback`.
  `MAP_SHARED` de fichiers : `uvm::ShmObject::for_file` + registre `maiproc::pagecache::shared`.
  Banc : `blkbench [--write] [--only ...] [--poll NS]` ; QEMU : `-drive driver=null-co,...` pour
  mesurer la pile seule, `-device nvme,serial=...,drive=...`.
- ext4 (MaiSS) : cœur `kernel/fs/ext4` sans dépendance (superbloc, groupes 32/64 octets,
  `meta_bg`, extents de toute profondeur, cartes de blocs ext2/3, htree aux trois hachages,
  `inline_data`, attributs étendus, `metadata_csum` CRC32C matériel, rejeu du journal JBD2
  **en mémoire**), testé sur l'hôte : `bash kernel/fs/ext4/tests/mkimages.sh /tmp/ext4img` puis
  `EXT4_IMAGES=/tmp/ext4img cargo +stable test --release` dans une copie hors du workspace.
  Noyau : `kernel/fs/ext4_vfs` (lecture par `blk`, cache des métadonnées CLOCK en 16 parts,
  identité des inodes — même `Arc` pour un même inode, exigé par `maiproc::pagecache` et par
  `ld.so` qui compare `(st_dev, st_ino)` —, cache des noms négatifs compris), monté par
  `disk_mount::init` sous `/mnt/<étiquette>` ; la première racine Linux (`usr` + `etc`) prête ses
  dossiers à `/` avant `mount_sysroot` (sauf `dev`, `proc`, `sys`, `tmp`, `run`, `mnt`). Lecture
  seule (`EROFS`).
- VFS : `fs_node` porte `Metadata` (`FsNode::metadata`, `read_only`), les liens
  (`File::read_link`) et `Directory::entries` (inode et type sans ouvrir les enfants).
  Résolution : `path::resolve::resolve` (liens suivis, 40 au plus, `..` physique dans les
  cibles) ; MaiProc passe par `linux::fs::walk(abs, follow)` (`ENOENT`/`ENOTDIR`/`ELOOP`).
  Règle : la résolution ne verrouille **jamais** un fichier (`mod_mgmt` tient le verrou d'un
  fichier pendant qu'il résout d'autres chemins) ; c'est le dossier qui donne la cible des
  liens (`Directory::get_with_link` ; la racine la lit une fois, à l'insertion).
  Écriture : `FsNode::set_metadata(MetaChange)` (droits, propriétaire, horodatages `TimeSet`),
  `Directory::create_file`/`create_dir`/`create_symlink` et `File::new_link` (`None` = ce système
  de fichiers ne sait pas faire ; MaiProc retombe alors sur memfs).
- tmpfs (`kernel/fs/tmpfs`) : `/tmp` (1777) et `/dev/shm`, montés par `mount_sysroot` ; aussi le
  support de `memfd_create` (`tmpfs::anon_file`). Pages creuses (`BTreeMap` page → frame, lues
  et écrites par la physmap, frames pré-zéroées d'abord), quota par volume (moitié de la RAM) et
  réserve système (`memory::RESERVE_FRAMES`, 16 Mio, partagée avec le pool pré-zéroé) → `ENOSPC`
  au lieu d'épuiser la mémoire, avec un `warn!` qui dit lequel des deux a refusé ; inodes complets (droits, uid/gid,
  `nlink`, horodatages *relatime*), liens durs (`Arc<Inode>` partagé), liens symboliques.
- Identités et droits (`maiproc/src/creds.rs`) : uid/gid réels, effectifs, sauvegardés, de
  système de fichiers, groupes ; règles Linux des `set*id`, bits set-user-ID/set-group-ID à
  l'`execve`, contrôle d'accès `may_access` (`access` juge avec les identités réelles,
  `AT_EACCESS` et `open` avec les effectives). Un nœud créé appartient à `fsuid:fsgid`.
- Verrous de fichiers : `flock.rs` (BSD, par description ouverte, clé `(st_dev, st_ino)`) et
  `reclock.rs` (`fcntl` POSIX par processus et OFD par description : plages coupées et
  fusionnées, `F_GETLK`, `EDEADLK` par parcours de la chaîne d'attente ; les verrous POSIX
  tombent à **toute** fermeture d'un descripteur du fichier et à la sortie).
- Temps : `clock_settime`/`settimeofday` publient le nouvel instant du démarrage dans `[vvar]`
  sous son *seqlock* (`vdso::set_epoch`). Horloges CPU (`clockid` négatifs de
  `clock_getcpuclockid`/`pthread_getcpuclockid`) ; `ITIMER_VIRTUAL`/`ITIMER_PROF` tenus par la
  tâche `maiproc_cputimer` (`cputimer.rs`), qui dort jusqu'au plus tôt possible des échéances
  (reste ÷ min(fils, cœurs)) — aucun verrou pris en interruption.
- Noms de fichiers en octets quelconques (PEP 383) : `fs_node::rawname` (octet invalide ↔
  U+10FF00 + octet, sans perte). Décoder à **chaque** entrée d'octets venue de l'appelant ou du
  disque (`user_path`, argv/envp d'`execve`, noms et liens d'ext4, `sockaddr_un`), réencoder à
  chaque sortie (`getdents64`, `readlink`, `getcwd`, pile initiale) ; jamais `from_utf8_lossy`.
- Nœuds spéciaux : `Directory::create_special` (tmpfs : `Body::Special`), `mknod` de tubes
  nommés, sockets et (root) périphériques ; `fifo.rs` : registre `(st_dev, st_ino)` → tube,
  ouverture qui attend l'arrivée d'un partenaire (compteurs d'ouvertures), `ENXIO` en écriture
  non bloquante sans lecteur.
- Pseudo-terminaux (`pty.rs`) : `/dev/ptmx`, `/dev/pts/<n>`, discipline N_TTY complète (termios
  et termios2, édition de ligne, écho, `ISIG`, `IXON`, `VMIN`/`VTIME`), terminal de contrôle
  (`Process::ctty`, `TIOCSCTTY`, `setsid`), `SIGTTIN`/`SIGTTOU`, raccrochage, `TIOCGPTPEER`.
- Mémoire épuisée (`oom.rs`) : une faute sans frame rend d'abord les caches de fichiers
  inutilisés (`pagecache::shrink`), sinon tue le plus gros processus (`SIGKILL`), attend sa mort
  et rejoue la faute — jamais un `SIGSEGV` au fil qui faute par hasard.
- Cache de pages en lecture : les `read()` des fichiers immuables (ext4, `read_only()`) passent
  par `pagecache::get(file).read` (fichiers ≤ `MAX_READ_CACHED`) ; lecture anticipée dans
  `uvm::CachedFile::fill` (fenêtre de 4 pages doublée sur accès séquentiel, 32 au plus, une seule
  requête à l'appareil).
- Sceaux des `memfd` (`seals.rs`, registre par `FileRef`) : `F_ADD_SEALS`/`F_GET_SEALS`,
  contrôlés à l'écriture, au changement de taille, au `mmap` partagé inscriptible et au `chmod`.
  `Vma::may_write` (`VM_MAYWRITE`) : une projection partagée d'un fichier ouvert en lecture
  seule ou scellé ne redevient jamais inscriptible (`mprotect` → `EACCES`). `readahead` et
  `POSIX_FADV_WILLNEED` lisent d'avance par `CachedFile::prefetch`. `/proc/<pid>/exe` et les
  noms de `/proc/<pid>/maps` (`uvm::VmaName`) sont les chemins physiques ; `comm` est propre à
  chaque fil (`Thread::comm`, celui du fil chef nomme le processus).
- inotify (`inotify.rs`) : surveillances par inode `(st_dev, st_ino)` (`linux::fs::inode_key`),
  événements émis par les crochets de `linux::fs` (ouverture, lecture, écriture, fermeture —
  `Drop` d'`OpenFile` —, attributs, création, suppression, déplacement avec cookie), au fichier
  et à son dossier (avec le nom) ; registre « chemin surveillé → inode » pour trouver le dossier
  sans parcours du VFS ; coût nul sans surveillance (`inotify::active()`). Tout nouvel appel qui
  modifie un fichier appelle le crochet correspondant. Les applis natives n'émettent rien.
- Minuteurs POSIX (`ptimer.rs`, `timer_create`…) : horloges murales sur `sleep::future`, horloges
  de temps processeur par la tâche de `cputimer` ; un seul signal en file par minuteur, le
  dépassement est posé dans `si_overrun` au retrait (`signal::dequeue_from` → `ptimer::on_dequeue`).
  `send_to_thread`/`send_to_process` rendent `true` si le signal a été mis en file.
- Priorités (`linux/prio.rs`) : `getpriority`/`setpriority` (nice par fil → `set_priority` de
  MaiSched ; baisser le nice demande root), `ioprio_get`/`ioprio_set` (`Thread::ioprio`, 0 par
  défaut comme Linux 6). `renameat2` : `RENAME_NOREPLACE` ; échange et whiteout → `EINVAL`.
- `FIONREAD` d'une socket UDP = taille du **prochain datagramme** (aussi dans smoltcp, par
  `peek`) : le résolveur de la glibc envoie A et AAAA ensemble (`sendmmsg`) et dimensionne le
  tampon de la seconde réponse avec ; 0 cassait toute résolution `AF_UNSPEC` (Firefox, Python).
  Banc : `m4test` (musl et glibc, `--tests linux`), validé aussi sur le noyau Linux de WSL ;
  accès Internet : `run -s /bin/bash /opt/maios/nettest.sh` (DNS, TCP, HTTP, HTTPS).
- Arguments 32 bits : Linux tronque chaque argument à son type C ; un appel fait par
  `syscall()` laisse des bits hauts quelconques. `linux::u32_args` donne, par numéro, les
  arguments `unsigned int`/drapeaux tronqués avant l'aiguillage ; un argument signé (dossier,
  délai, horloge, pid) se lit par `i32a`. Tout nouvel appel à drapeaux `u32` y est ajouté.
- Terminal (`console.rs`) : jamais d'attente active. `stdio` porte des `Waker` réveillés par
  chaque écriture et la fin de fichier ; `ImmutableRead::try_read`/`poll_read` ; MaiProc dort sur
  `CONSOLE_WQ` (signaux, `O_NONBLOCK`, `FIONREAD`, `poll` exact). Le verrou externe de `stdio`
  ne coupe pas les interruptions (un lecteur natif qui attend gelait sinon son cœur, puis toute
  la machine au premier IPI d'invalidation de TLB).
- `sendfile`/`splice`/`copy_file_range` (`copy_fds`) : une erreur après des octets transmis rend
  leur nombre ; la position d'entrée n'avance que des octets écrits.
- netlink (`netlink.rs`, `AF_NETLINK`) : `NETLINK_ROUTE` (`RTM_GETLINK`/`GETADDR`/`GETROUTE` en
  vidage paresseux façon `netlink_dump`, `GETLINK` et `ip route get` unitaires, `NLM_F_ACK`,
  `NLMSG_ERROR` complet ou `NLM_F_CAPPED`, modifications → `EPERM`/`EOPNOTSUPP`) et
  `NETLINK_KOBJECT_UEVENT` (aucun événement : pas de branchement à chaud) ; ports par
  `netlink_autobind`. Sert `getifaddrs`, `if_nameindex`, `AI_ADDRCONFIG` et iproute2.
- Données annexes IP (`socket.rs` `IpCtl`, `put_cmsg` à la Linux : coupe et `MSG_CTRUNC`) :
  envoi `IPV6_HOPLIMIT`/`TCLASS`/`PKTINFO`, `IP_TTL`/`TOS`/`PKTINFO` ; réception selon
  `IPV6_RECVHOPLIMIT`/`RECVTCLASS`/`RECVPKTINFO`, `IP_RECVTTL`/`RECVTOS`/`IP_PKTINFO`.
- UDP-Lite (RFC 3828, `IPPROTO_UDPLITE`) dans `inet.rs` : ports à part, `UDPLITE_SEND_CSCOV` /
  `RECV_CSCOV` ; réseau par une socket brute smoltcp du protocole 136 (en-têtes et somme faits
  ici, MTU sans fragmentation → `EMSGSIZE`).
- Mesures : `echo on > /proc/maios/syscalls` (puis `off`, `reset`) ; la lecture donne nombre,
  total, moyenne et maximum par appel système, pour les fautes de page et pour les phases de la
  vie d'un processus (`fork`→premier pas, `exit`→zombie, zombie→`wait4`, vie, CPU)
  (`sysstats.rs`, compteurs par cœur).
- Harnais : délai dépassé → NMI automatique (piles de chaque cœur dans le journal) ; second
  moniteur QEMU libre pendant l'exécution : `qemu-monitor.ps1 -Port 4558 -Cmd nmi`.
- Affinité : `task_struct::CpuMask` (`sched.cpus_allowed`, hérité par `clone`/`fork`), respecté
  par `select_cpu`, le vol et l'équilibrage ; `task::scheduler::set_affinity` migre aussitôt une
  tâche en file, relance le cœur d'une tâche qui tourne ; une tâche courante qui n'a plus droit à
  son cœur est sortie de la file par `put_prev_task` (`MIGRATE_PREV`) et remise en file ailleurs
  par `finish_task_switch` une fois qu'elle n'y tourne plus.
- Racine Linux de test : `wsl bash scripts/mkrootfs.sh` (fichiers des paquets de la Debian de
  WSL + bibliothèques, `fakeroot`, `mke2fs -d`) → `build/rootfs.ext4` ; puis
  `python scripts/maios_bench.py --kvm --tests ext4` (NVMe, `run -s /bin/bash
  /opt/maios/ext4test.sh` : liens, droits, perl, awk, `EROFS`, SHA-256 de tout `/usr`).
- Python 3 : `wsl bash scripts/fetch_debs.sh` télécharge sans root (signatures apt vérifiées)
  les paquets absents de la Debian de WSL (`python3`, `python3-venv`,
  `libpython3.13-testsuite`) dans `~/maios_pkgs/debs` ; `mkrootfs.sh` les extrait et précompile
  les `.pyc`. `python scripts/maios_bench.py --kvm --tests python --timeout 1200` lance
  `/opt/maios/pytest.sh` (`scripts/rootfs/pytest.sh` : temps de démarrage et de fork + exec,
  vérifications rapides, puis une sélection de la suite de régression de CPython, un module par
  processus). C'est le banc de conformité de l'ABI : chaque échec y est un écart à Linux.
- Règle des interruptions : rien de ce qu'une interruption touche (réveil, `runstate`) ne doit
  passer par un verrou ordinaire que le code interrompu pourrait tenir. `AtomicCell` de crossbeam
  d'un type qui n'est pas atomique nativement prend un **verrou global** : jamais dans un chemin
  d'interruption (d'où `AtomicRunState` et `AtomicClass` dans `task_struct`). Une tâche neuve
  finit sa commutation (`finish_task_switch`) avant tout verrou. Un verrou pris quelque part
  interruptions ou préemption coupées doit être un `IrqSafeMutex` **partout** : sinon un détenteur
  préempté ne reprend jamais la main pendant que les autres cœurs tournent dessus (machine figée,
  vu avec `TASK_LIST`, pris par `init_current_task` et par `reap_if_orphaned`).
- Diagnostic d'un blocage : `nmi` dans le moniteur QEMU → chaque cœur affiche sa tâche, son `rip`
  résolu et les adresses de sa pile (`exceptions_full::nmi_backtrace`) ;
  `task::scheduler::debug_cpus()` / `debug_task()`.
- Tests réseau : `m3net` (TCP/UDP, IPv6, io_uring). Le harnais lance un serveur d'écho en Perl
  dans WSL, joignable par l'invité en 10.0.2.2.
- Tests : `bash userland/tests/linux_abi/build.sh` (WSL, musl et BusyBox extraits sans sudo dans
  `~/maios_pkgs`, binaires dans `extra_files/linux/`), puis
  `python scripts/maios_bench.py --kvm --tests linux` (maitest, proctest pour M2, `sh` de
  BusyBox sur `m2.sh`) ; `--cpu host,-pcid,...` masque des
  fonctions du processeur invité. Les FPS de Doom varient de ±15 % d'une session à l'autre
  (cœurs P/E de l'hôte) : refaire 2–3 mesures avant de conclure à une régression.

## Compositeur (MaiCompositor)
- `kernel/gfx/window_manager` : une seule tâche, `mai_compositor`, compose et présente. Les
  `refresh_*` du gestionnaire, `Window::render` et `sys_present` ne font que déclarer une zone
  abîmée (`window_manager::damage`, région de `kernel/gfx/region2d`, testée sur l'hôte comme
  `llfree`) puis sonner (`doorbell.rs`). Pilotée par les événements : les pilotes d'entrée
  appellent `event_types::notify_input()` après chaque dépôt, plus de scrutation à 1 ms.
- Image (`compose.rs`) : opaque de l'avant vers l'arrière (fenêtre moins ses coins,
  `WindowInner::corner_radius`, posé par la décoration), puis coins et fenêtres `translucent`
  de l'arrière vers l'avant, puis contour d'aperçu ; seuls les rectangles abîmés vont à
  l'affichage. Curseur = plan à part, jamais dans le tampon arrière.
- Horloge d'images à 60 Hz, arrêtée au repos ; le compteur VSync (`mgi::signal_vblank`) n'avance
  qu'une fois par image ; `window_manager::vsync_wait` se bloque sur une `WaitQueue`.
- Règles : le compositeur ne fait que des `try_lock` sur les fenêtres et le bureau (une fenêtre
  occupée remet l'image de 0,5 ms, sans perdre la zone) ; ne jamais composer ni prendre le
  verrou de MGI depuis une application ; la boucle ne doit jamais se terminer sur une erreur.
  Bureau : `window_manager::with_desktop(_mut)` puis `damage`. Statistiques : `window_manager::stats()`.

## GPU (MaiGPU)
- Plan : `.claude/plans/08-gpu.md` (réponse « possible ? pilotes NVIDIA ? », sources, jalons
  G0 à G7). G0 (fait) : virtio-gpu 2D, sortie de MaiCompositor par le GPU.
- Transport `kernel/drivers/bus/virtio` (réutilisable par tout appareil virtio) :
  - `PciTransport` : capacités modernes projetées une fois par BAR sur l'union des régions
    (deux projections des mêmes frames MMIO sont refusées) ; `negotiate` (§ 3.1.1, rend
    `(offertes, retenues)`) ; `setup_queue` ; `enable_msix` (fonction masquée, vecteurs liés,
    puis `msix_unmask`) ; `device_read32` relit tant que `config_generation` change.
  - `SplitQueue` : descripteurs libres chaînés, EVENT_IDX (`kick_needed` = `need_event` ;
    `enable_interrupts` réarme `used_event` puis rend vrai s'il faut revider).
  - `pci::VirtioCapInfo` porte `id`, `extra` (`notify_off_multiplier`, ou moitié haute du
    décalage d'une région de mémoire partagée) et `length_hi`.
- Pilote `kernel/drivers/gpu/virtio_gpu` (`probe` depuis `device_manager`) :
  - Fonctions retenues : EDID et EVENT_IDX (VIRGL, BLOB et CONTEXT_INIT attendent G1/G3).
  - MSI-X : entrée 0 configuration (tâche `virtio_gpu_events`, `set_display_hook`),
    1 `controlq`, 2 `cursorq`.
  - Commandes en **cases** préparées (une page : commande, réponse à 2 Kio) ; commande longue
    = `Dma` apportée (`attach_backing`).
  - Attente : se bloquer, s'inscrire, revérifier, secours de 50 ms, perdue après 5 s ;
    sondage sans MSI-X ou si l'on ne peut pas dormir.
  - Une rafale (`command_async(…, kick: false)` puis la dernière à `true`) ne sonne qu'une
    fois.
- `display.rs`, `Display: mgi::Scanout` :
  - Ressource B8G8R8X8 à la résolution de MGI ; *backing* = frames de `memory::create_mapping`
    (segments fusionnés, 16 384 au plus) ; `SET_SCANOUT`.
  - `present` : un `TRANSFER_TO_HOST_2D` par rectangle (décalage = coin du rectangle dans le
    *backing*, 32 rectangles au plus, voisins fusionnés), puis un `RESOURCE_FLUSH` de
    l'enveloppe avec fence. Rien n'est attendu là.
  - Curseur matériel : ressource 64 × 64 B8G8R8A8 ; `TRANSFER` attendu avant `UPDATE_CURSOR` ;
    `MOVE_CURSOR` garde la ressource (QEMU cache le curseur si elle vaut 0) ; position = point
    chaud.
- MGI et compositeur :
  - `Mgi::attach_scanout` fait de la mémoire du scanout le tampon avant.
  - `captain` crée le scanout après `device_manager::init` (`window_manager::attach_scanout`,
    branché par la tâche du compositeur, qui abîme tout l'écran).
  - **Règle** : attendre la sortie avant toute écriture du tampon avant (`front_write`, hors du
    verrou de MGI), noter chaque rectangle écrit (`wrote`) ; la boucle envoie la rafale
    (`flush_output`).
  - Curseur sur le plan matériel si l'image tient en 64 × 64 (sinon curseur logiciel, plan
    matériel caché ; `stats().sw_cursor_draws`).
  - L'ancien prototype de `mhc` (sondage actif, recopie de tout l'écran toutes les 33 ms) est
    supprimé ; `mhc` ne garde que son GPU logiciel.
- Observabilité : `mgi::GpuReport` (`Scanout::report`, lu sans le verrou de MGI par
  `window_manager::gpu_report()`) : carte, pilote, fonctions, mode, mémoire, temps actif de
  l'appareil (au moins une commande en vol sur la `controlq`, `ChanInner::inflight`),
  présentations, octets, latences. Le gestionnaire des tâches en fait la catégorie **GPU**
  de la vue Performance (sans GPU : charge de la composition). Un futur pilote (Intel, AMD…)
  n'a qu'à implémenter `report`.
- Lancement : `run.ps1 -Gpu virtio|virtio-gl|std` (virtio par défaut sur `moderne`, `mono`,
  `costaud` ; VGA sur les profils anciens), `build-and-run.ps1 -Gpu` (défaut `virtio-gl`).
  Le QEMU 11.1 de Windows a `virtio-vga-gl` (`venus`, `blob`, `hostmem`) et SDL `gl=on` :
  la 3D de G3 s'y testera sans téléchargement.
- **Pointeur absolu** (`kernel/drivers/input/virtio_input`, `-device virtio-tablet-pci`,
  ajouté par `run.ps1` sauf `-NoTablet`) : avec une souris PS/2 relative et le curseur
  matériel, QEMU recale le pointeur de l'hôte sur celui de l'invité à chaque `MOVE_CURSOR`
  (`dpy_mouse_set`) sans tenir compte de la mise à l'échelle de la fenêtre : le pointeur
  fuyait hors de la fenêtre. La tablette supprime recalage et capture. Pilote : configuration
  `select`/`subsel` octet par octet (`PciTransport::device_read8/write8/read_bytes`), `eventq`
  de tampons de 8 octets (codes `evdev`) vidée dans l'interruption MSI-X, un `MouseEvent`
  par `SYN_REPORT` (`SYN_DROPPED` jette le paquet) dans la file de la souris PS/2.
  `mouse_data::MouseEvent::absolute` (`MousePositionAbsolute::to_screen`, bornes comprises) ;
  MaiCompositor : `Motion::Abs` dans `pump_input`. Les claviers virtio sont laissés au PS/2.
  Test : phase « tablette » de `gpu_test` (harnais : `--tablet`, implicite avec `--tests gpu`,
  QMP `input-send-event` sur le port 4560).
- Tests : `python scripts/maios_bench.py --kvm --tests gpu --shots`.
  - `--tests gpu` implique `--gpu virtio` : `-vga none -device virtio-vga,xres=1920,yres=1080` ;
    Limine démarre sur la partie VGA, les captures montrent ensuite le scanout virtio.
  - Ce que vérifie `gpu_test` : motif et mise à jour partielle au pixel près, curseur matériel
    sans tracé logiciel, coût plein écran et petite zone, temps actif cohérent et rapport du
    GPU (carte, mode, mémoire) pour le gestionnaire des tâches.
  - `--gpu virtio` marche avec les autres bancs.
  - L'écran fait 1920 × 1072 (mode VBE donné par Limine) : le scanout suit MGI, pas le mode
    préféré de virtio.
  - `screendump` ne montre pas le curseur matériel.
- Mesures sous KVM (G0) :
  - Plein écran (8040 Kio) : ~0,95 ms de la soumission à la fence ; composition ~2,3 ms.
  - Petite zone : ~0,17 ms.
  - Mise à jour partielle d'un carré 24 × 24 : 2304 octets transférés, une notification par
    image.
- Sous WSL (QEMU 10.0 de Debian 13) : virtio-gpu 2D seulement. virgl/Venus (G3) demandent
  `qemu-system-modules-opengl` et `libvirglrenderer1` (téléchargement à faire approuver) ; pas
  de `/dev/dri` ni d'`udmabuf` dans WSL ; seul Vulkan de l'hôte : lavapipe.

## Wayland (MaiLand)
- Plan : `.claude/plans/06-mailand-wayland.md` (serveur Wayland dans le noyau, branché sur
  MaiCompositor ; sources : Nitpicker ACSAC 2005, bogues de libwayland, horloges de KWin/Mutter,
  `FrameTimeline` d'Android, `fifo-v1`/`commit-timing-v1`).
- `kernel/gfx/wl_proto` (W0) : cœur sans dépendance, testé sur l'hôte
  (`cp -r kernel/gfx/wl_proto ~/t && cd ~/t && cargo test`, aussi en debug : le fuzzeur doit
  attraper les débordements). `build.rs` génère depuis `protocols/*.xml` (copiés de
  `libwayland-dev` 1.23.1 et `wayland-protocols` 1.44 : `apt-get download` + `dpkg-deb -x`) un
  module par interface : `INTERFACE`, `req::*`, `evt::*`, énumérations (`wl_output::transform::_90`).
- Serveur `kernel/gfx/mailand` (W1), démarré par `captain` après `window_manager::init` : une
  tâche `mailand`, socket `/run/user/0/wayland-0` (`maiproc::runtime_dir`), extrémité noyau
  `maiproc::unix::{KernelListener, KernelConn}` (jamais d'attente : observateurs des files ;
  `send` seulement après `send_room`). Une `xdg_toplevel` = une `window::Window` (crochet
  `WindowInner::on_event` qui sonne la tâche). Décorations selon `xdg-decoration` : barre du
  système en `server_side`, aucune en `client_side` **ou sans objet de décoration** (mode par
  défaut du protocole ; GTK/Firefox dessinent les leurs) ; `Window::set_decorated` change à
  chaud, le contenu ne bouge pas, `DirectContent` suit (`xdg.rs`, `deco_of`). Les clients de
  test (`wlhello`, `wltime`, `wlsub`) demandent `server_side` (captures au pixel près) ; pixels lus par
  `ShmObject::read_at` (aucune page créée) et convertis (Wayland : alpha 255 = opaque,
  prémultiplié ; MaiOS : alpha 0 = opaque, non prémultiplié). Bornes par client : objets et
  files (`Limits`), pixels de fenêtres (8 écrans). Tests : `wsl bash userland/tests/wayland/build.sh`,
  `mkrootfs.sh`, puis `python scripts/maios_bench.py --kvm --tests wayland` (`wlhello`,
  `wlinput`, `wltime`, `wayland-info`).
- W2 : `seat.rs` (pointeur, clavier, curseurs), `keymap.rs` (cartes XKB `us`/`fr` précompilées,
  fichier tmpfs en lecture seule partagé), `popup.rs` + `xdg.rs` (menus, saisie, décorations,
  agrandissement, `ping`), `data.rs` (presse-papiers ↔ `mai_ui::clipboard`). Routage de
  MaiCompositor : `set_pointer_grab`, saisie implicite, `PointerLeave`, `FocusEvent`,
  surimpressions `add_overlay`. `wlinput` est piloté par le harnais : chaque marqueur
  « wlinput : pret … » déclenche `sendkey`/`mouse_move`/`mouse_button` du moniteur
  (`wlinput_steps`) ; souris PS/2 relative, sans accélération. **La file PS/2 de QEMU ne tient
  que 16 octets (4 paquets de ±127)** : le reste d'un grand `mouse_move` attend dans QEMU et
  s'ajoute à la commande suivante ; déplacer par pas de 127 au plus (`move` du harnais).
  `mouse_button` : 1 gauche, 2 droit, 4 milieu ; `mouse_move 0 0 1` = un cran vers le haut.
- W3 : horloge de présentation. MaiCompositor numérote les demandes d'image
  (`window_manager::frame_gen`, génération augmentée sous le verrou de `DAMAGE`), publie chaque
  image présentée (`last_presentation` : numéro, instant monotone, génération couverte), la
  prochaine échéance (`next_frame_ns`) et appelle `set_present_hook` après chaque image. MaiLand
  (`surface.rs`) fige chaque `commit` en mise à jour de contenu, appliquée en file quand elle est
  prête (barrière `fifo` levée, instant `commit-timing` atteignable) ; rappels `frame` et
  `wp_presentation_feedback` (`timing.rs`) partent à la présentation de leur génération, avec son
  instant réel (`CLOCK_MONOTONIC`) ; `discarded` si remplacée avant son image.
  `view.rs` (testé sur l'hôte comme `llfree`, avec une copie de `region2d`) : transformation,
  échelle, `wp_viewport`, en virgule fixe Q32 ; `pixels.rs` : copie directe ou filtre bilinéaire
  prémultiplié (tuiles par bandes). Coordonnées des fenêtres = pixels de surface.
  **Composition directe** : un tampon `wl_shm` montré tel quel est lu par le compositeur au
  moment de composer (`window_inner::{ContentPlane, DirectContent}`, `compose::put_window`) ; les
  coins arrondis du bas restent copiés dans la fenêtre. `wl_buffer.release` quand le dernier
  plan qui le lit disparaît (`shm::ReleaseToken`, file `RELEASES` vidée par la tâche `mailand`).
  `wltime` (piloté par le harnais : captures vérifiées au pixel près, `wltime_steps`, espace pour
  continuer) mesure présentation, fifo, commit-timing, viewport, tampons d'un pixel, erreurs.
- W4 (clients réels) : sous-surfaces (`subsurface.rs` : cache des modes synchronisés, arbre
  composé dans le tampon de la racine par `pixels::compose`, chaque surface gardant son tampon
  par `Surface::hold` ; composition directe seulement sans enfant ; pointeur par `pick` sur la
  région d'entrée), `activation.rs` (`xdg_activation_v1`, jetons privilégiés seulement pour le
  client qui a le focus), sélection primaire (`data.rs`, `Kind::Primary`), `security.rs`
  (`security-context-v1` : socket adoptée par `KernelListener::adopt`, globales privilégiées
  cachées aux connexions du bac à sable). Bancs `wlsub` (piloté, captures, pointeur) et
  `wlsec`. Clients réels : `wsl bash scripts/fetch_debs.sh` (foot, GTK 4, SDL 3, Qt 6,
  firefox-esr ; fichier `INSTALLED` pour ce que WSL a déjà), `mkrootfs.sh` (caches GSettings,
  gdk-pixbuf, MIME, fontconfig, icônes ; `/etc/machine-id`), puis
  `python scripts/maios_bench.py --kvm --tests apps --apps foot,gtk4,sdl3,qt6,firefox --memory 4G`
  (`scripts/rootfs/apprun.sh`, captures `build/bench_shots/4_*.png`).
- Déboguer un programme Linux : `echo "errors motif" > /proc/maios/strace` (ou `all`, puis
  `off`) journalise ses appels système (`strace.rs`, motif cherché dans le `comm` du
  processus, chemins lus ; mode `net` : appels de sockets avec adresses et options) ; harnais : `--app-strace errors:motif`. Un plantage (signal « core »)
  écrit l'origine du signal et la dernière faute du fil en `fichier+décalage` (décalage dans le
  fichier : ajouter l'écart `p_vaddr − p_offset` du segment avant `objdump`).
- Conteneurs (plan 07, S3 ; pour bwrap/pressure-vessel et le bac à sable de Chromium) :
  - Identités **globales** dans le noyau (`Creds`, propriétaires, IPC), traduites par l'espace
    utilisateur de l'appelant aux frontières (`userns.rs` : `make_kuid` à l'entrée,
    `from_kuid_munged` à la sortie, 65534 sans carte). Tout nouvel appel qui lit ou rend un
    uid/gid traduit. Privilèges : `userns::capable` (espace initial : heure, `mknod`, netlink…)
    ou `ns_capable(creds, espace propriétaire de l'objet, CAP_*)` ; plus jamais `euid == 0`.
  - Chemins : `mount.rs` résout dans l'espace de montage depuis la **racine du processus**
    (`t.process.fs`, `FsRef` : racine + dossier courant en positions (montage, nœud, chemin)).
    Dans `linux/fs.rs` : `walk(t, abs, follow)`, `walk_mnt` (avec le montage : `ro`, `nodev`,
    `noexec`, `nosuid`), `split_parent(t, …)` (rend `EROFS` sur montage en lecture seule),
    `lookup_any` (nœud du VFS, nœud produit de `/proc`/`/sys`, ou `devpts`). Ne jamais résoudre
    un chemin d'un processus par `root::get_root()` ni `path::Path::get` : cela ignore
    `chroot`, `pivot_root` et les montages.
  - `/proc`, `/sys`, `/dev/pts` sont des montages virtuels (`VirtFs`) : la suite du chemin va,
    en chemin canonique, à `procfs`/`pty`. `/dev` est un tmpfs de périphériques ouverts par
    (majeur, mineur) (`char_device`). Les sockets Unix sont liées par inode (`BindKey`).
  - Espaces pid (`pidns.rs`, modèle `struct pid`) : le noyau ne connaît que des numéros
    **globaux** (`Process::pid`, `Thread::tid`, `pgid`, `sid`, `ppid` = identifiants de tâche) ;
    chaque espace descendant a sa table locale ↔ globale. Sortie : `Process::vnr(global)` /
    `PidNs::nr_of` (0 si invisible) ; entrée : `find_process_v`/`find_thread_v(t, nr)` — jamais
    `find_process` avec un pid venu de l'utilisateur. `getpid`/`gettid` : `vpid`/`vtid` (mot des
    futex PI et robustes, `CLONE_*SETTID`). Un `Pid` (`Arc`) est tenu par le fil, le processus,
    les membres du groupe et de la session ; le dernier détachement rend les numéros. Init de
    l'espace (`unkillable`) : orphelins (`find_new_reaper`, *subreaper* d'abord), signaux sans
    gestionnaire ignorés sauf `SIGKILL`/`SIGSTOP` d'un espace ancêtre ou du noyau
    (`signal::send_to_process_as(.., from_outside)`), sa fin tue l'espace (`zap_pid_ns`) puis
    `fork` y rend `ENOMEM`. Chaque `proc` montre l'espace de son superbloc (`Super::pid_ns`) :
    `procfs::*(t, ns, …)`. `siginfo` (`si_pid`, `si_uid`) traduit pour le destinataire ;
    `kill` vérifie les droits (`kill_permitted`, `CAP_KILL`). `PR_SET_PDEATHSIG` (à la fin du
    processus parent), `PR_SET_CHILD_SUBREAPER`, `CLONE_PIDFD`, `clone3` `set_tid`,
    `ns_last_pid`, `NS_GET_PID_*`.
  - Espaces réseau (`netns.rs`, modèle `struct net`) : chaque socket garde l'espace où elle est
    née (`sock_net` : `InetSocket::ns`, `UnixSocket::net`, `NetlinkSocket::net`,
    `SocketOps::sock_net`), jamais celui de l'appelant au moment de l'appel. Par espace : tables
    de ports TCP/UDP/UDP-Lite (`inet::Ports`), noms abstraits Unix (`BindKey::Abstract(ino, nom)`),
    ports netlink, `lo`. Seul l'espace initial atteint les cartes (`InetSocket::iface`) ; un
    nouvel espace n'a que `lo`, **éteint et sans adresse** (`ENETUNREACH`, `EADDRNOTAVAIL`)
    jusqu'à `RTM_NEWLINK`+`IFF_UP` ou `SIOCSIFFLAGS` (ajoute `127.0.0.1/8` et `::1`) ;
    `RTM_NEWADDR`/`DELADDR` sur `lo` (séquence `loopback_setup` de bwrap). Les sockets Unix par
    chemin traversent les espaces (X11, Wayland). `/proc/net/*` : espace de l'appelant ;
    `ioctl(SIOCGIF*)` : espace de la socket. Ports < 1024 : `CAP_NET_BIND_SERVICE` dans le
    propriétaire de l'espace (`EACCES`).
  - `openat2` (`linux/fs.rs`, `mount::resolve_scoped`, `mount::Scope`) : `RESOLVE_IN_ROOT`,
    `BENEATH`, `NO_XDEV`, `NO_SYMLINKS`, `NO_MAGICLINKS` dans le résolveur à pile
    (`walk_stack` : plancher de `..`, liens absolus) ; liens magiques de `/proc` repérés par
    `procfs::links_in`. Les appels de montage passent par `mount::lookup_magic` (suivent
    `/proc/<pid>/{exe,cwd,root,fd/<n>}`), pas l'ouverture (`/proc/self/fd/N` y reste un
    partage de la description).
  - devpts par montage (`pty::PtsFs`, porté par `Super::pts`) : `Lookup::Pts(instance, chemin)`,
    `/dev/ptmx` 5:2 → instance montée sur `pts` dans son dossier (`ptmx_instance`). Ne jamais
    revenir à une table globale des terminaux.
  - `execve` (`Creds::on_exec`) : sous `no_new_privs`, permis borné à l'ancien et identités
    effectives ramenées aux réelles (bwrap `--cap-drop ALL` en root en dépend).
  - bubblewrap réel : `python scripts/maios_bench.py --kvm --tests bwrap`
    (`scripts/rootfs/bwraptest.sh`, root puis uid 1000 par `setpriv` ; la racine a un
    utilisateur `maios` 1000). À valider sur Linux avant d'accuser MaiOS :
    `BWRAP=<bwrap extrait> bash bwraptest.sh` en uid 1000, et `unshare -r` pour la partie root.
  - Banc : `python scripts/maios_bench.py --kvm --tests ns` (`nstest` : capacités, `unshare -r`,
    cartes, tmpfs, liaisons `ro`, montages verrouillés, socket par un autre chemin, UTS/IPC,
    séquence `pivot_root` de bubblewrap, espace pid avec son init et son `/proc`,
    `clone(NEWUSER|NEWPID|NEWNS)`, subreaper, PDEATHSIG, `clone3(CLONE_PIDFD)`, espace réseau :
    `lo` éteint, `loopback_setup` de bwrap, isolation des ports et des noms abstraits, socket
    d'avant l'`unshare`, chemin Unix partagé, `openat2`, instances de devpts, `execve` sous
    `no_new_privs`). Pas encore : espace temps (`EINVAL`), propagation des montages partagés.
- Steam (plan 07) : S1 Xwayland. `Xwayland -geometry WxH -shm` (*rootful*, une fenêtre MaiLand)
  marche avec les paquets de `fetch_debs.sh` (`xwayland x11-apps x11-utils xterm mesa-utils`…) ;
  banc `python scripts/maios_bench.py --kvm --tests x11 --memory 4G` (`scripts/rootfs/xtest.sh`).
  IPC System V : `maiproc/src/ipc.rs` (mémoire partagée ; `shm_nattch` = références de
  l'objet tenues par les zones, moins celle de la table tant que le segment n'est pas détruit).
  S2 (binaires i386) : entrées `int 0x80` et `syscall` 32 bits (`uentry`), segments et TLS
  par fil (`uentry/src/seg.rs`, GDT 12..14), ELF32 et `TASK_SIZE32`, table
  `maiproc/src/linux/compat.rs`, cadres `signal32.rs`. Règle : une fonction commune qui lit
  ou écrit une structure de l'utilisateur teste `compat::active(t)` (disposition i386) et,
  pour un `timespec`/`timeval`, passe par `compat::get_timespec`/`put_timespec` ; un nouvel
  appel « time32 » va dans `compat::is_time32`. Banc : `ia32test` (`--tests linux`).
  vDSO i386 (`vdso.rs`, `vdso32_blob`, `.code32`, `AT_SYSINFO`) : `__kernel_vsyscall` par
  `syscall` sur AMD, octets remplacés par un saut vers `int 0x80` sur Intel. glibc i386 :
  `wsl bash scripts/fetch_i386.sh` puis `build.sh` (`m3test_i386`, `m3net_i386`,
  `m4test_i386`, liés par `ld --sysroot` contre `~/maios_pkgs/multilib`).
- Pilotes PS/2 : la souris assemble ses paquets **octet par octet** (resynchronisation sur le
  bit 3 du premier octet, paquet abandonné après 50 ms de silence), ne lit le port 0x60 que si
  le contrôleur annonce un octet de la souris ; le clavier seulement un octet du clavier
  (`PS2Keyboard::has_scancode`). Molette : `scroll_movement` positif vers le haut partout
  (`z` PS/2 inversé, signé sur 4 bits pour l'identifiant 4).
- Règles : `InBuf::next` ne rend qu'une requête validée (objet vivant, opcode et `since`,
  arguments stricts, objets du bon type, nouvel identifiant dense et libre) ; identifiants du
  client denses et table bornée (`Limits::objects`) ; objet du client détruit par le serveur =
  zombie (`ObjectMap::kill`), ses requêtes en route sont sautées **après** avoir consommé leurs
  descripteurs ; `OutBuf` borné (1 Mio) et découpé sur les frontières de messages avec leurs
  descripteurs.

## Bureau (explorateur)
- `userland/apps/explorer` : bureau (fond, horloge, raccourcis), barre des tâches
  (`taskbar.rs`) et menu MaiOS (`menu.rs`, bouton « M » ou Super seule) ; ces deux-là sont des
  surimpressions `translucent` de MaiCompositor (`add_overlay`), le menu prend
  `set_pointer_grab` + `set_keyboard_grab` tant qu'il est ouvert. Canevas (`paint.rs`) :
  opérateur « over » sur destination transparente, traits anticrénelés (`stroke`, `ring`, logo).
- Gestionnaire : `set_reserved_bottom` (bande de la barre) → `work_area()` pour agrandir, ancrer
  et les bornes Wayland ; `windows()` (liste pour la barre, par `WindowInner::serial`) ;
  `set_desktop_events` (pointeur hors des fenêtres, clavier sans fenêtre active, Super seule) ;
  `set_desktop_ring`/`notify_desktop` sonnent l'explorateur (événement du bureau, focus,
  réduction, fermeture, titre, `app_id`) : il dort sur un `Doorbell`, sans scrutation.
- Contenu du bureau (`desk.rs`) : **tout** `root:/desktop` (sauf les noms cachés) —
  raccourcis `.desk` et lanceurs `.desktop`, puis dossiers, puis fichiers ; type par
  `mai_ui::files::classify` (partagé avec Fichiers), icônes de `mai_ui::icons`, vignettes des
  PNG/XPM. Un clic ouvre : appli, dossier ou fichier dans Fichiers (`file_manager <chemin>` :
  un fichier est choisi et ouvert), ELF par `run` + `apprun.sh`, WAD par Doom. Relu quand
  l'empreinte du dossier (noms, types, tailles) change, vérifiée chaque seconde avec
  l'horloge (les applis natives n'émettent pas d'événements du VFS). `cp` (`-r`, vers un
  dossier), `mkdir` (`-p`) et `touch` créent par `Directory::create_file`/`create_dir`, sinon
  memfs/`vfs_node`, comme MaiProc ; le shell natif n'a pas de redirection `>`.
- Fenêtres identifiées par `WindowInner::app_id` (Wayland : `set_app_id` ; natives :
  `Window::set_app_id("maios.terminal" | "maios.files" | "maios.taskmanager")`). Catalogue
  (`catalog.rs`) : `/desktop/*.desk` (clés `id=`, `icon=`…) + `/usr/share/applications/*.desktop`
  de la racine ext4, lancés par `run` + `apprun.sh`. Icônes : `kernel/gfx/mai_icons` (PNG
  complet avec inflate maison, XPM, `.desktop`, mise à l'échelle ; sans dépendance, testé sur
  l'hôte : `python kernel/gfx/mai_icons/tests/mkvectors.py <dir>` puis
  `MAI_ICONS_VECTORS=<dir> cargo +stable test --release` dans une copie hors du workspace).
- Alimentation : `acpi::power::{poweroff, reboot}` (`_S5_` lu dans la DSDT, `PM1a/b_CNT`,
  `SCI_EN` par le port SMI ; redémarrage : `RESET_REG`, 0xCF9, 8042, triple faute).

## Applications graphiques (MaiUI)
- Boîte à outils `kernel/gfx/mai_ui` : `canvas` (dessin anticrénelé avec découpage, écrit
  directement dans le tampon de la fenêtre ; texte CP437 avec accents), `input` (`Pointer` :
  clics, double-clic, glisser, molette ; `key_char`, `ctrl_letter`), `kit` (palette `pal`,
  onglets, `TextField`, `table_header`, `Scroll`, `Menu`, `Dialog`, `Toast`, `History`),
  `icons`, `clipboard` (partagé par toutes les applications : crate du noyau chargée une fois).
  Les anciens `draw`/`widgets` restent pour les applis pas encore portées.
- Modèle : mode immédiat. L'application garde son état, redessine tout quand `dirty`, et
  range pendant le dessin les zones à tester pour le pointeur. Les coordonnées de la souris
  (`MousePositionEvent::coordinate`) sont celles du tampon de la fenêtre, comme le canevas.
- Gestionnaire des tâches : `userland/apps/task_manager` (`sample.rs` : échantillonnage ;
  utilisation par cœur, PSI et charge moyenne viennent des compteurs `idle_ns`,
  `contended_ns` et `runnable_ns` de `task::scheduler::stats()`). Fichiers :
  `userland/apps/file_manager` (`fsops.rs`, `search.rs`, `preview.rs`).
- VFS : `FsNode::set_name` et `fs_node::move_node` déplacent ou renomment sans copie
  (`renameat` de MaiProc s'en sert), seulement si le nœud et les deux dossiers sont
  `is_movable()` (memfs, heapfile, vfs_node, racine). FAT32 ne l'est pas : son `remove`
  efface l'entrée et libère les clusters, son `insert` crée une entrée vide. Ailleurs,
  `move_node` renvoie `NOT_MOVABLE` et l'appelant copie puis supprime (comme `EXDEV`).
  Le `remove` de FAT32 renvoie toujours `None` : vérifier la suppression par un `lookup`. Ne jamais tenir le verrou d'un dossier pendant le
  `get()` d'un enfant (`task_fs` reprend des verrous du VFS ; un `get()` FAT32 relit le
  dossier en PIO) : lister les noms, relâcher, puis prendre chaque enfant.
- Captures des applis sous KVM : moniteur QEMU (`screendump`, `sendkey`) comme dans
  `scripts/maios_bench.py` ; Maj+F10 ouvre les menus contextuels (la touche Menu n'arrive
  pas depuis QEMU).

## Architecture syscall
Syscalls centralisés dans `kernel/abi/maios_syscall/`. Les mappers Linux/Windows sont des shims minces qui délèguent à `maios_syscall`.

## Sprints en cours
Voir `.claude/plans/` pour les plans détaillés. Roadmap dans la mémoire (`ROADMAP.md`).
