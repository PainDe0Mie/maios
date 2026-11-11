# MaiOS

**MaiOS** est un système d'exploitation écrit en Rust, à l'origine un fork de
[Theseus OS](https://github.com/theseus-os/Theseus). Son but : un système natif, rapide et moderne,
capable d'exécuter les applications Linux et, plus tard, Windows. Chaque sous-système reprend des
travaux récents, issus de la recherche (articles, conférences) ou des noyaux actuels (Linux 6.x).

- Langage : Rust (nightly-2023-10-27, épinglé dans `rust-toolchain.toml`)
- Architecture principale : x86_64 (aarch64 en secondaire)
- Démarrage : Limine 12, une seule ISO hybride BIOS + UEFI

## Où en est le projet (novembre 2026)

| Domaine | État | Contenu |
|---|---|---|
| Réorganisation | ✅ fait | arborescence `kernel/` + `userland/` + `libs/`, renommage Theseus → MaiOS |
| Démarrage | ✅ fait | Limine 12 (BIOS + UEFI), cœurs secondaires démarrés par Limine, modules dans une archive `cpio.lz4` |
| Mémoire physique (MaiMM M1) | ✅ fait | allocateur de frames **LLFree**, sans verrou, et pool de pages pré-zéroées |
| Tas noyau (MaiMM M2) | ✅ fait | **kalloc**, conçu comme mimalloc : segments, listes libres partagées, liens chiffrés |
| Ordonnanceur (MaiSched) | ✅ fait | **EEVDF** sur un arbre AVL augmenté, tranches adaptatives, temps réel O(1), vol de tâches |
| Processus utilisateur (MaiProc M1) | ✅ fait | anneau 3, un espace d'adresses par processus, ELF statiques et PIE, environ 110 appels Linux |
| Processus utilisateur (MaiProc M2) | ✅ fait | `fork` sans copie, `execve`, `wait4`, signaux, tubes, fils, futex |
| Doom | ✅ fonctionne | port natif, plus de 200 FPS sous KVM |
| Applications de bureau | ✅ refaites | gestionnaire des tâches et gestionnaire de fichiers sur la boîte à outils MaiUI |
| Chargeur dynamique, `/proc`, epoll, sockets (M3) | ✅ | glibc dynamique, vDSO, `epoll`, sockets Unix et TCP/UDP, `io_uring`, `/proc` |
| Sous-système Windows NT (M4) | 🧪 commencé | chargeur PE, premiers appels NT ; à rebâtir sur le modèle de processus de M2 |

### Ce que MaiOS sait faire aujourd'hui

- **Démarrer** sur BIOS comme sur UEFI depuis une seule ISO (CD, clé USB ou disque), avec plusieurs cœurs.
- **Exécuter des binaires Linux 64 bits statiques** (musl, glibc statique, BusyBox) en anneau 3 :
  - chaque processus a son propre espace d'adresses, avec ASLR ;
  - 154 numéros d'appels système Linux sont pris en charge.
- **Faire tourner un vrai shell Unix** (BusyBox `ash`) :
  - tubes à plusieurs étages, redirections, sous-shells ;
  - `trap`, tâches de fond avec `wait` et `kill`.
- **Gérer des processus et des fils** :
  - `fork`, `vfork`, `clone3`, `execve` (y compris les scripts `#!`), `wait4` et `waitid` ;
  - zombies, orphelins, groupes de processus et sessions ;
  - fils `pthread`, futex (y compris PI, `futex_waitv`, listes robustes).
- **Livrer des signaux comme Linux** :
  - cadre `rt_sigframe` avec l'état XSAVE, `rt_sigreturn`, `sigaltstack` ;
  - relance des appels interrompus (`SA_RESTART`) ;
  - arrêt et reprise de groupe, `alarm` et `setitimer`.
- **Lancer Doom** et des applications natives Rust : shell, terminal, commandes, gestionnaire de fenêtres.
- **Gérer le système depuis le bureau** :
  - **Gestionnaire des tâches** : arbre des processus Linux et de leurs fils, applications et
    tâches du noyau ; graphes du processeur (total et par cœur), de la mémoire et du tas ;
    compteurs de l'ordonnanceur ; terminer, tuer, suspendre, changer la priorité.
  - **Fichiers** : vues liste et grille, aperçu (texte, ELF, WAD), recherche approchée dans les
    sous-dossiers, copier/couper/coller, glisser-déposer, corbeille restaurable, annulation.
- **Réseau** : smoltcp côté noyau. **Audio** : Intel HDA.
- **Stockage (MaiSS)** : NVMe à files multiples (une par cœur, MSI-X), couche bloc façon
  blk-mq avec attente hybride, projections `MAP_SHARED` de fichiers.

### Ce qui manque encore

- Le chargeur dynamique glibc (`ld-linux.so` avec de vraies `.so`), le vDSO, un `/proc` complet.
- Côté appels système : les sockets (`socket`, `connect`, sockets Unix), `epoll`, `eventfd`,
  `memfd_create`, `inotify`, `timerfd`, `signalfd`, `pidfd`, `timer_create`.
- Ctrl-C n'atteint pas encore un programme qui lit le terminal : le terminal n'est pas relié aux
  groupes de processus.
- Un système de fichiers moderne sur disque : FAT32 seulement pour l'instant (ext4 est la suite
  de MaiSS).
- Les verrous futex PI n'élèvent pas encore la priorité de leur propriétaire.
- La compatibilité 32 bits, les espaces de noms (*namespaces*), seccomp, DRM/KMS et un serveur
  graphique X11 ou Wayland : il les faudra pour Chromium ou Steam.

## Ce qui est construit, et d'où viennent les idées

### Mémoire (MaiMM)

- **`kernel/mm/llfree`**, allocateur de frames physiques sans verrou, d'après LLFree (USENIX ATC'23) :
  - un champ de 512 bits par bloc de 2 Mio, soit une ligne de cache ;
  - une page de 2 Mio s'alloue en un seul CAS ;
  - 31 Kio de métadonnées pour 512 Mio de RAM ;
  - 12 tests sur l'hôte.
- **`kernel/mm/kalloc`**, tas noyau sur le modèle de mimalloc (MSR-TR-2019-18) :
  - segments de 2 Mio, 52 classes de taille ;
  - trois listes libres par page : allocation et libération locales sans aucune opération atomique ;
  - libérations distantes sans verrou ;
  - durcissement : liens chiffrés, détection des doubles libérations, pointeurs étrangers rejetés.
- **`kernel/mm/uvm`**, mémoire des processus :
  - VMA, fautes de page à la demande, `mremap` sans copie, ASLR (RDRAND) ;
  - **`fork` par tables partagées** (On-Demand-Fork, EuroSys'21) ;
  - copie à l'écriture décidée par un compteur de références par frame ;
  - invalidations TLB groupées, avec IPI envoyées aux seuls cœurs qui exécutent le processus.
- **PCID** : changer d'espace d'adresses ne vide pas la TLB ; pages du noyau globales.

### Ordonnanceur (MaiSched)

- **`kernel/sched/maisched`** :
  - **EEVDF** (Stoica et Abdel-Wahab, 1995 ; ordonnanceur de Linux 6.6) sur un arbre AVL intrusif augmenté ;
  - tranches adaptatives, classe temps réel en O(1), topologie et masques de cœurs ;
  - 7 tests sur l'hôte.
- En simulation, une tâche interactive contre deux tâches de calcul : latence de réveil p99 de
  **0 µs** avec les tranches adaptatives, contre **8 ms** avec la tranche fixe d'EEVDF seul.
- Partie noyau :
  - réveils sans perte, préemption paresseuse, vol de tâches entre cœurs ;
  - IPI de replanification ;
  - `apic::current_cpu()` lu par `rdtscp`, sans sortie de VM.

### Processus et ABI Linux (MaiProc)

- **`kernel/arch/x86_64/uentry`** :
  - entrée et sortie du noyau par `syscall`/`sysret` et des talons IDT ;
  - NMI paranoïaque, SMEP/SMAP, sauvegarde XSAVE ;
  - conçu pour passer plus tard à FRED.
- **`kernel/system/maiproc`** :
  - chargeur ELF (statique, PIE, `PT_INTERP`), pile System V complète (argv, envp, auxv) ;
  - table de descripteurs, tubes, `poll` et `select` bloquants ;
  - signaux, futex à 256 seaux ;
  - `/dev/{null,zero,urandom,tty}`.
- **Jalon M3** (`kernel/system/maiproc`, `kernel/mm/uvm`) :
  - **glibc dynamique** : `/lib64/ld-linux-x86-64.so.2` et les bibliothèques de Debian
    (`extra_files/sysroot`, visibles à la racine) ; `ls`, `cat` et `dash` de Debian tournent ;
  - **cache de pages des fichiers** : une page de `libc.so.6` ou de `busybox` n'existe qu'une
    fois en mémoire, projetée en lecture seule et copiée seulement si un processus l'écrit ;
  - **vDSO** : `clock_gettime` en 21 ns au lieu de 150 ns (même calcul que le noyau, TSC en
    128 bits), `gettimeofday`, `time`, `getcpu` ; pages communes scellées ;
  - **epoll** en O(prêts), comme Linux : chaque entrée s'inscrit dans les files d'attente du
    fichier ; niveau, front, `EPOLLONESHOT`, imbrication avec détection des boucles ;
  - `eventfd`, `timerfd`, `signalfd`, `pidfd` (avec `waitid(P_PIDFD)`) ;
  - **sockets Unix** : flux, datagrammes, `SOCK_SEQPACKET`, chemins et espace abstrait,
    passage de descripteurs (`SCM_RIGHTS`), identités (`SO_PEERCRED`, `SCM_CREDENTIALS`) ;
  - **`/proc` complet** : un dossier par processus (`stat`, `status`, `cmdline`, `fd/`,
    `task/`…), `/proc/stat`, `/proc/loadavg` (charge moyenne calculée par le noyau),
    `/sys/devices/system/cpu` ; `ps` et `top` de BusyBox fonctionnent ;
  - TSC synchronisé entre les cœurs au démarrage (`IA32_TSC_ADJUST`, comme `tsc_sync.c`) ;
  - **sockets TCP/UDP** (`AF_INET`, `AF_INET6`, double pile de la RFC 3493) :
    - connexions locales par un **chemin rapide sans paquets**, sur le modèle du *TCP Loopback
      Fast Path* de Windows : environ 3 Go/s entre deux processus ;
    - réseau réel par smoltcp, avec un *waker* par socket : seules les sockets dont l'état change
      sont réveillées ;
    - minuteurs TCP sans tick, réglés sur la prochaine échéance (avant, la pile tournait avec un
      temps figé à zéro) ;
    - ports éphémères selon la RFC 6056 durcie par Klein et Pinkas (USENIX Security 2022) ;
    - `SO_REUSEPORT` réparti par un hachage SipHash des flux ;
    - `/proc/net/{tcp,udp,dev,route}`, ioctl `SIOCGIF*` (`ifconfig` de BusyBox fonctionne) ;
  - **io_uring**, même ABI que Linux :
    - anneaux partagés ; tentative en ligne sans attente, puis sondage rapide par les files
      d'attente des fichiers ;
    - travailleurs internes au processus, `SQPOLL` ;
    - chaînes et `LINK_TIMEOUT`, délais, annulation, tampons fournis, opérations à plusieurs
      coups, `MSG_RING`, débordement sans perte ;
  - carte e1000 : réception à la NAPI (l'interruption masque la réception, une tâche vide
    l'anneau puis la démasque, sans réveil perdu), modération par ITR ; IRQ PCI programmées
    selon les *Interrupt Source Overrides* du MADT (polarité, déclenchement sur niveau) ;
    DHCP asynchrone, repli statique au bout de 5 s seulement.

### Stockage (MaiSS, Mai Storage System)

- **Couche bloc à files multiples** (`kernel/drivers/storage/blk`), d'après blk-mq (Bjørling et
  al., SYSTOR 2013) :
  - chaque cœur soumet dans sa file matérielle ; étiquettes sans verrou façon `sbitmap` ;
  - données décrites par segments physiques, DMA directement dans le tampon de l'appelant
    (frontière de page de NVMe respectée, tampon de rebond seulement s'il est mal aligné) ;
  - lots soumis d'un coup avec une seule cloche (*plugging*) ;
  - **attente hybride** (Yang et al., *When Poll Is Better than Interrupt*, FAST 2012) : sondage
    borné à deux fois la latence moyenne, plus long quand aucune autre tâche n'attend le cœur,
    puis interruption ;
  - chien de garde qui rattrape une interruption perdue ; statistiques par unité.
- **Pilote NVMe** (`kernel/drivers/storage/nvme`) :
  - une paire de files par cœur (jusqu'à 32), un vecteur MSI-X par file, dirigé vers son cœur ;
  - listes PRP sans copie, files en mémoire cachée ;
  - cloches fantômes (*Doorbell Buffer Config*, NVMe 1.3) : moins de sorties de VM ;
  - plusieurs espaces de noms, `Flush`, `Write Zeroes`, `Dataset Management`.
- **Vecteurs MSI-X avec argument** (`interrupts::register_msi_callback`) : un pilote à plusieurs
  files reçoit l'indice de la file, comme les *IRQ domains* de Linux.
- **Projections `MAP_SHARED` de fichiers** : une seule copie des pages par fichier pour tous les
  processus, cohérente avec `read` et `write`, réécrite par `msync`, `fsync`, un vidage toutes
  les 5 s, et au retrait de la dernière projection.
- Banc `blkbench` (à la `fio`) : latence QD1, débit séquentiel, QD32, montée en charge par cœur,
  vérification d'intégrité.

### Bureau (MaiUI et applications)

- **`kernel/gfx/mai_ui`**, boîte à outils en mode immédiat (le modèle de Dear ImGui) :
  - canevas logiciel avec découpage, opacité et bords anticrénelés (couverture analytique,
    traits par l'algorithme de Xiaolin Wu), sans flottant ;
  - souris (double-clic, glisser, molette), champ de saisie, onglets, tableaux triables,
    menus contextuels, boîtes de dialogue, notifications, graphes ;
  - icônes vectorielles et presse-papiers partagé par toutes les applications.
- **Gestionnaire des tâches** (`userland/apps/task_manager`) :
  - utilisation de chaque cœur mesurée par le temps passé dans sa tâche inactive (comme la
    ligne `idle` de `/proc/stat`) ;
  - charge moyenne sur 1, 5 et 15 minutes calculée comme `calc_load` de Linux, mais pondérée
    dans le temps par l'ordonnanceur au lieu d'un instantané ;
  - pression du processeur sur 10, 60 et 300 s, comme le PSI de Linux ;
  - équité entre les cœurs mesurée par l'indice de Jain.
- **Fichiers** (`userland/apps/file_manager`) :
  - type des fichiers reconnu par leurs premiers octets, comme `file(1)` ;
  - tri naturel (`image2` avant `image10`), comme l'Explorateur Windows ;
  - recherche approchée notée comme dans fzf ;
  - déplacement et renommage sans copie (`fs_node::move_node`) ;
  - copie incrémentale qu'on peut annuler en cours ;
  - corbeille indexée selon la spécification *Trash* de freedesktop.org.

## Statistiques

### Code (novembre 2026)

| | Lignes de Rust | Crates |
|---|---:|---:|
| Noyau (`kernel/`) | 111 196 | 187 |
| Espace utilisateur (`userland/`) | 35 152 | 100 |
| Bibliothèques génériques (`libs/`) | — | 25 |

Taille des sous-systèmes écrits pour MaiOS :

| Crate | Rôle | Lignes |
|---|---|---:|
| `kernel/system/maiproc` | processus, signaux, futex, ABI Linux | 6 264 |
| `userland/apps/file_manager` | Fichiers : opérations, recherche, aperçu | 3 669 |
| `kernel/gfx/mai_ui` | boîte à outils graphique | 2 978 |
| `userland/apps/task_manager` | gestionnaire des tâches | 2 178 |
| `kernel/sched/maisched` | ordonnanceur EEVDF | 1 905 |
| `kernel/mm/kalloc` | tas noyau | 1 876 |
| `kernel/mm/uvm` | mémoire des processus, `fork` sans copie | 1 563 |
| `kernel/mm/llfree` | frames physiques sans verrou | 1 045 |
| `kernel/arch/x86_64/uentry` | entrée et sortie du noyau | 1 035 |

### Performances

Conditions : QEMU 10 + KVM dans WSL, 4 vCPU `-cpu host`, 512 Mio de RAM.
Commande : `python scripts/maios_bench.py --kvm` ; référence dans `scripts/bench_baseline_kvm.json`.

**Tas noyau : ancien tas de Theseus contre kalloc**

| Mesure | Ancien tas | kalloc | Écart |
|---|---:|---:|---:|
| Allocation de 16 à 256 o (p50) | 48 ns | 29 ns | −40 % |
| Allocation de 4 Kio, p50 / p99 | 340 / 712 ns | 30 / 107 ns | −91 % / −85 % |
| Allocation / libération de 64 Kio (p50) | 2,5 / 5,9 µs | 31 / 48 ns | −99 % |
| Allocation / libération de 256 Kio (p50) | 9,2 / 11,1 µs | 201 / 57 ns | −98 % / −99 % |
| *churn* (débit) | 2,7 Mops/s | 28,5 Mops/s | ×10,5 |
| Allocation sur un cœur, libération sur un autre | 11,2 Mops/s | 19,6 Mops/s | ×1,75 |
| Débit sur 1 / 2 / 4 cœurs | 18,4 / 36,5 / 69,8 Mops/s | 22,7 / 45,6 / 77,5 Mops/s | +23 / +25 / +11 % |
| Mémoire gardée après tout libérer | 28,2 Mio | 4,1 Mio | −85 % |

**Frames physiques : liste avec verrou contre LLFree** (mesuré sous TCG, donc indicatif seulement)

| Mesure | Liste + verrou | LLFree | Écart |
|---|---:|---:|---:|
| Allocation d'une frame, p50 / p99 | 1033 / 1247 ns | 493 / 536 ns | −52 % / −57 % |
| Libération d'une frame (p50) | 696 ns | 175 ns | −75 % |
| Débit sur 2 cœurs | 857 kops/s | 1850 kops/s | +116 % |

**Processus**

| Mesure | Résultat |
|---|---|
| `fork` d'un processus de 64 Mio | **62 µs** : aucune page ni table de dernier niveau n'est copiée |
| `proctest`, 33 contrôles : `fork`, `MAP_SHARED`, tubes, signaux, fils, futex | 0 échec sur 4 lancements |
| Script shell BusyBox `m2.sh` : tubes, `trap`, tâches de fond, 20 `fork`+`exec` | 0 échec sur 4 lancements |

**Doom** (`doom -timedemo demo1`)

| Version | FPS |
|---|---:|
| Ancien tas | bloqué (2 lancements sur 2) |
| kalloc + MaiSched (référence) | **262,6** |
| Après MaiProc M1 et M2 | 205 à 235 |

Doom a ralenti de 12 à 28 % depuis MaiProc M1. La cause n'est pas encore trouvée ; elle est en cours d'analyse.

## Compiler et tester

La compilation passe toujours par WSL :

```bash
wsl bash -lc "make iso"
```

Après un changement du noyau, Doom doit être relié de nouveau :

```bash
wsl bash -lc "bash scripts/rebuild_all.sh"
```

Construire les tests Linux (musl et BusyBox, sans `sudo`) :

```bash
wsl bash -lc "bash userland/tests/linux_abi/build.sh"
```

Lancer les mesures et les tests sous KVM :

```bash
python scripts/maios_bench.py --kvm --tests linux,doom
```

Lancer clippy :

```bash
wsl bash -lc "make clippy ARCH=x86_64"
```

## Feuille de route

1. **Suite de MaiSS** : ext4 en lecture et écriture (journal JBD2, extents, sommes de contrôle),
   cache de pages unifié pour `read` et `write`, puis **MaiFS**, un système de fichiers natif à
   copie sur écriture.
   - Objectif : faire tourner Firefox depuis un disque ext4.
2. **MaiNet**, une pile TCP/IP native qui remplacera smoltcp :
   - traitement de bout en bout par cœur, à la IX ou Demikernel ;
   - BBR ou CUBIC, RACK-TLP, SACK ;
   - `AF_NETLINK`.
   - Objectif : `curl` et `python3` liés dynamiquement.
3. **Espaces de noms et seccomp-BPF**.
   - Objectif : faire tourner `bwrap`, et le bac à sable de Chromium.
4. **Graphisme** : DRM/KMS, un compositeur Wayland ou Xwayland, Mesa avec llvmpipe, puis un pilote GPU.
5. **Compatibilité 32 bits** : `int 0x80`, segment de code 32 bits, ABI i386.
6. **Sous-système NT (M4)** :
   - objets NT et notre propre `ntdll`, sur le modèle de processus de M2 ;
   - *Syscall User Dispatch* pour les programmes qui font des appels NT directs.
7. **Pistes de recherche retenues** :
   - FRED, à la place de l'IDT et de `syscall` ;
   - INVLPGB/TLBSYNC (AMD) et RAR (Intel) : invalidations TLB sans IPI ;
   - ASI, contre les attaques spéculatives ;
   - verrous par VMA, pour des fautes de page en parallèle ;
   - pages larges transparentes ;
   - NUMA.

Les plans détaillés sont dans `.claude/plans/`.

## Références

- Wrenger et al., *LLFree: Scalable and Optionally-Persistent Page-Frame Allocation*, USENIX ATC 2023.
- Leijen, Zorn, de Moura, *Mimalloc: Free List Sharding in Action*, MSR-TR-2019-18.
- Stoica, Abdel-Wahab, *Earliest Eligible Virtual Deadline First*, 1995 ; EEVDF dans Linux 6.6.
- Zhao et al., *On-Demand-Fork: A Microsecond Fork for Memory-Intensive and Latency-Sensitive
  Applications*, EuroSys 2021.
- Baumann et al., *A fork() in the road*, HotOS 2019.
- D. Hildenbrand, *PG_anon_exclusive* (Linux 5.19), et la série Linux « COW PTE table ».
- Collabora, `futex_waitv` (Linux 5.16), l'appel système utilisé par Wine et Proton.
- Bjørling, Axboe, Nellans, Bonnet, *Linux Block IO: Introducing Multi-queue SSD Access on
  Multi-core Systems*, SYSTOR 2013.
- Yang, Minturn, Hady, *When Poll Is Better than Interrupt*, FAST 2012.
- NVM Express Base Specification 2.0 (*Doorbell Buffer Config*, § 5.7).

## Licence

MIT, comme Theseus OS : voir [LICENSE-MIT](LICENSE-MIT).
