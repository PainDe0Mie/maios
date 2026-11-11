#!/usr/bin/env python3
"""Harnais de mesure MaiOS (MaiMM, phase M0).

Démarre l'ISO dans QEMU sans affichage, ouvre le terminal via le moniteur QEMU puis lance :
  0. `sched_bench` : changement de contexte, réveil, préemption, équité, équilibrage (MaiSched) ;
  1. `mm_bench -n N` : latences p50/p99 de l'allocateur de frames et montée en charge multi-cœurs ;
  1b. `heap_bench -n N` : tas noyau (latences par taille, churn, libérations distantes, montée en
     charge, mémoire rendue) ;
  2. `run doom -timedemo demo1` : Doom rejoue la démo 1 le plus vite possible. Le résultat
     (« timed N gametics in M realtics ») est recopié de stderr vers le port série par la libc.
  3. `--tests gpu` (MaiGPU G0, implique `--gpu virtio`) : `gpu_test` vérifie le pilote
     virtio-gpu (motif et mise à jour partielle au pixel près sur captures, curseur matériel,
     coût de la présentation).

`--gpu virtio` remplace la carte VGA standard par `virtio-vga` : Limine démarre sur sa partie
VGA, puis MaiCompositor présente par le scanout virtio (captures = ce que le GPU a reçu).

Les résultats sont relevés sur le port série et comparés à une référence enregistrée
(`scripts/bench_baseline.json`). Code de sortie non nul si :
  * le noyau panique, ou un test dépasse son délai ;
  * le score Doom (FPS) régresse de plus de --tolerance %.

Exemples :
  python scripts/maios_bench.py                      # mesure et compare à la référence
  python scripts/maios_bench.py --save-baseline      # enregistre la mesure comme référence
  python scripts/maios_bench.py --tests doom --runs 3

Uniquement la bibliothèque standard Python. QEMU doit être installé (chemin par défaut Windows,
ou --qemu).

Sous Windows, `--kvm` lance QEMU dans WSL avec KVM (le noyau tourne sur le vrai processeur) :
démarrage en quelques secondes et mesures stables, contrairement à TCG dont le bruit rend les FPS
de Doom inexploitables. Prérequis dans WSL : `qemu-system-x86` installé et l'utilisateur dans le
groupe `kvm`.
  python scripts/maios_bench.py --kvm --tests heap,doom --runs 3
"""
import argparse
import json
import os
import re
import socket
import subprocess
import sys
import tempfile
import time
import struct
import zlib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_QEMU = r"C:\Program Files\qemu\qemu-system-x86_64.exe" if os.name == "nt" else "qemu-system-x86_64"
BASELINE = os.path.join(ROOT, "scripts", "bench_baseline.json")
# Sous KVM les chiffres n'ont rien à voir avec TCG : référence séparée.
BASELINE_KVM = os.path.join(ROOT, "scripts", "bench_baseline_kvm.json")
MONITOR_PORT = 4557
# QMP (seulement avec --tablet) : `input-send-event` pour les positions absolues.
QMP_PORT = 4560
# Le démarrage est terminé quand captain passe la main ; l'interface finit de s'installer ensuite.
BOOT_DONE = r"captain::init\(\): initialization done"
QEMU_NAME = "maios-bench"


def wsl_path(path):
    """Chemin Windows (lecteur E:) vu depuis WSL (/mnt/e/...)."""
    path = os.path.abspath(path)
    drive, rest = os.path.splitdrive(path)
    return "/mnt/" + drive[0].lower() + rest.replace("\\", "/")
ANSI = re.compile(r"\x1b\[[0-9;]*m")

# Touches `sendkey` du moniteur QEMU (disposition US) pour `Qemu.type`.
KEYS = {" ": "spc", "/": "slash", ".": "dot", "-": "minus", "_": "shift-minus", "=": "equal",
        ":": "shift-semicolon", ",": "comma", ";": "semicolon", "'": "apostrophe",
        '"': "shift-apostrophe", "(": "shift-9", ")": "shift-0", "&": "shift-7", "*": "shift-8",
        ">": "shift-dot", "<": "shift-comma", "|": "shift-backslash", "\\": "backslash",
        "$": "shift-4", "!": "shift-1", "#": "shift-3", "@": "shift-2", "%": "shift-5",
        "^": "shift-6", "+": "shift-equal", "?": "shift-slash", "[": "bracket_left",
        "]": "bracket_right", "{": "shift-bracket_left", "}": "shift-bracket_right",
        "`": "grave_accent", "~": "shift-grave_accent"}


class Qemu:
    def __init__(self, args):
        self.wsl = args.kvm
        self.distro = args.distro
        # Sous WSL, les fichiers échangés avec QEMU (journal série, captures) doivent être sur un
        # disque Windows monté : on les met dans build/.
        tmp = os.path.join(ROOT, "build") if self.wsl else tempfile.gettempdir()
        self.serial = os.path.join(tmp, "maios_bench_com1.log")
        if os.path.exists(self.serial):
            os.remove(self.serial)
        path = wsl_path if self.wsl else (lambda x: x)
        # Journal série : QEMU écrit chaque octet par un `write()` à part. Sur un disque Windows
        # monté (drvfs, 9P) cela coûte ~230 µs par octet, pendant lesquels le vCPU qui a fait
        # `out` reste arrêté : tout `warn!` du noyau en paraissait lent. Sous WSL, QEMU écrit donc
        # dans l'ext4 de la distribution, et `log()` recopie les octets nouveaux dans build/.
        self.serial_src = self.serial
        self._raw = bytearray()
        qemu_serial = path(self.serial)
        if self.wsl:
            home = subprocess.run(["wsl", "-d", args.distro, "--exec", "sh", "-c",
                                   "mkdir -p ~/.maios_bench && rm -f ~/.maios_bench/com1.log && echo $HOME"],
                                  capture_output=True, text=True).stdout.strip()
            if home.startswith("/"):
                qemu_serial = home + "/.maios_bench/com1.log"
                self.serial_src = "\\\\wsl.localhost\\" + args.distro + qemu_serial.replace("/", "\\")
        cmd = [
            args.qemu, "-name", QEMU_NAME, "-cdrom", path(args.iso), "-boot", "d",
            "-m", args.memory, "-smp", str(args.smp), "-no-reboot", "-no-shutdown",
            "-serial", "file:" + qemu_serial, "-serial", "null",
            "-display", "none", "-monitor", f"tcp:127.0.0.1:{MONITOR_PORT},server,nowait",
            # Second moniteur, libre pendant l'exécution : `qemu-monitor.ps1 -Port 4558 -Cmd nmi`.
            "-monitor", f"tcp:127.0.0.1:{MONITOR_PORT + 1},server,nowait",
        ]
        # KVM : le processeur réel (rdtscp, invariant TSC). TCG : Broadwell, le plus ancien modèle
        # qui a rdtscp.
        cmd += ["-cpu", args.cpu or ("host" if self.wsl else "Broadwell")]
        disk = os.path.join(ROOT, "fat32.img")
        if os.path.exists(disk):
            cmd += ["-drive", f"format=raw,file={path(disk)},if=ide,snapshot=on"]
        # MaiSS : racine Linux ext4 (scripts/mkrootfs.sh) en NVMe ; les écritures éventuelles
        # vont dans un fichier temporaire de QEMU (snapshot), l'image ne change jamais.
        if any(t in args.tests for t in ("ext4", "python", "wayland", "apps", "x11", "bwrap")):
            if not os.path.exists(args.rootfs):
                sys.exit(f"{args.rootfs} absent : lancer d'abord `wsl bash scripts/mkrootfs.sh`")
            cmd += ["-drive", f"format=raw,file={path(args.rootfs)},if=none,id=rootfs,snapshot=on",
                    "-device", "nvme,serial=maios-root,drive=rootfs"]
        if args.gpu == "virtio":
            # Partie VGA pour Limine (BIOS VBE), puis scanout virtio. Mode préféré annoncé
            # (GET_DISPLAY_INFO, EDID) = résolution de cfg/limine.conf.
            cmd += ["-vga", "none", "-device", "virtio-vga,xres=1920,yres=1080"]
        if args.tablet:
            # Pointeur absolu (virtio-input) ; QMP pour lui envoyer des positions.
            cmd += ["-device", "virtio-tablet-pci", "-qmp", f"tcp:127.0.0.1:{QMP_PORT},server,nowait"]
        self._qmp = None
        accel = "kvm" if self.wsl else args.accel
        if accel:
            cmd += ["-accel", accel]
        if self.wsl:
            cmd = ["wsl", "-d", args.distro, "--exec"] + cmd
        self.proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        time.sleep(2)
        if self.proc.poll() is not None:
            sys.exit("QEMU s'est arrêté : " + self.proc.stderr.read().decode(errors="replace"))
        # Le port du moniteur peut s'ouvrir après 2 s sur une machine chargée : on réessaie, et
        # on n'abandonne jamais un QEMU orphelin derrière nous.
        for attempt in range(30):
            try:
                self.mon = socket.create_connection(("127.0.0.1", MONITOR_PORT))
                break
            except ConnectionRefusedError:
                if attempt == 29 or self.proc.poll() is not None:
                    self.kill()
                    raise
                time.sleep(1)
        self.mon.settimeout(0.2)
        self.drain(0.5)

    def drain(self, seconds):
        end = time.time() + seconds
        while time.time() < end:
            try:
                if not self.mon.recv(65536):
                    break
            except socket.timeout:
                pass

    def monitor(self, cmd):
        self.mon.sendall((cmd + "\n").encode())
        self.drain(0.1)

    def qmp(self, command, arguments=None):
        """Commande QMP (connexion ouverte à la première) ; rend la réponse."""
        if self._qmp is None:
            sock = socket.create_connection(("127.0.0.1", QMP_PORT), timeout=10)
            f = sock.makefile("rwb")
            f.readline()  # salutation
            f.write(b'{"execute":"qmp_capabilities"}\n')
            f.flush()
            f.readline()
            self._qmp = (sock, f)
        _, f = self._qmp
        msg = {"execute": command}
        if arguments is not None:
            msg["arguments"] = arguments
        f.write((json.dumps(msg) + "\n").encode())
        f.flush()
        while True:
            r = json.loads(f.readline())
            if "return" in r or "error" in r:  # les événements asynchrones sont sautés
                return r

    def type(self, text):
        for ch in text:
            key = KEYS.get(ch) or ("shift-" + ch.lower() if ch.isupper() else ch)
            self.monitor("sendkey " + key)
            time.sleep(0.04)
        self.monitor("sendkey ret")

    def grab(self, png_path):
        """Capture l'écran QEMU (PPM via le moniteur) : `(largeur, hauteur, octets RGB)`, ou
        `None`. Le PPM est effacé ; `png_path` sert à nommer le fichier temporaire."""
        ppm = png_path[:-4] + ".ppm"
        self.monitor("screendump " + (wsl_path(ppm) if self.wsl else ppm.replace("\\", "/")))
        for _ in range(20):
            time.sleep(0.25)
            if os.path.exists(ppm) and os.path.getsize(ppm) > 0:
                break
        # QEMU écrit le fichier par morceaux (6 Mio en 1920x1080, lent sur une machine
        # chargée) : on relit jusqu'à avoir tous les pixels annoncés par l'en-tête.
        for _ in range(40):
            time.sleep(0.25)
            try:
                data = open(ppm, "rb").read()
            except OSError:
                continue
            parts = data.split(b"\n", 3)
            if len(parts) == 4:
                w, h = map(int, parts[1].split())
                if len(parts[3]) >= w * h * 3:
                    os.remove(ppm)
                    return w, h, parts[3][:w * h * 3]
        return None

    def screenshot(self, png_path, image=None):
        """Capture l'écran QEMU (ou `image`, rendue par `grab`) et l'enregistre en PNG."""
        image = image or self.grab(png_path)
        if not image:
            return
        w, h, px = image
        raw = b"".join(b"\x00" + px[y * w * 3:(y + 1) * w * 3] for y in range(h))

        def chunk(tag, body):
            return struct.pack(">I", len(body)) + tag + body + struct.pack(">I", zlib.crc32(tag + body) & 0xFFFFFFFF)

        png = (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
               + chunk(b"IDAT", zlib.compress(raw, 6)) + chunk(b"IEND", b""))
        with open(png_path, "wb") as f:
            f.write(png)

    def log(self):
        """Journal série sans les codes couleur ANSI du logger."""
        if self.serial_src == self.serial:
            try:
                with open(self.serial, "rb") as f:
                    return ANSI.sub("", f.read().decode(errors="replace"))
            except FileNotFoundError:
                return ""
        # Sous WSL : on lit la suite du fichier de QEMU et on la recopie dans build/.
        try:
            with open(self.serial_src, "rb") as f:
                f.seek(len(self._raw))
                new = f.read()
        except OSError:
            new = b""
        if new:
            self._raw += new
            with open(self.serial, "ab") as f:
                f.write(new)
        return ANSI.sub("", self._raw.decode(errors="replace"))

    def wait_for(self, pattern, timeout):
        """Attend `pattern` (regex) dans le journal série ; s'arrête net si le noyau panique."""
        rx = re.compile(pattern)
        end = time.time() + timeout
        while time.time() < end:
            text = self.log()
            if re.search(r"\bPANIC\b|panicked at|EXCEPTION:|[Kk]ernel panic", text):
                raise RuntimeError("panique du noyau détectée sur le port série")
            m = rx.search(text)
            if m:
                return m
            time.sleep(1)
        self.nmi_dump()
        raise TimeoutError(f"délai dépassé ({timeout} s) en attendant /{pattern}/")

    def nmi_dump(self):
        """Délai dépassé : un NMI fait afficher à chaque cœur sa tâche, son `rip` et sa pile
        (`exceptions_full::nmi_backtrace`) ; les lignes restent dans le journal série."""
        try:
            before = len(self.log())
            self.monitor("nmi")
            time.sleep(3)
            dump = self.log()[before:]
            if dump.strip():
                print("   NMI (piles des coeurs) :")
                for line in dump.splitlines()[-80:]:
                    print("   ! " + line)
        except OSError:
            pass

    def stop(self):
        try:
            self.monitor("quit")
        except OSError:
            pass
        try:
            self.proc.wait(10)
        except subprocess.TimeoutExpired:
            self.kill()
        self.log()  # dernière recopie du journal série dans build/

    def kill(self):
        """Tue notre QEMU (et seulement lui : repéré par son -name dans WSL)."""
        if self.wsl:
            subprocess.run(["wsl", "-d", self.distro, "--exec", "pkill", "-f", "--", f"-name {QEMU_NAME} "],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.proc.kill()


def parse_mm_bench(text):
    """Extrait les lignes `[mm_bench] clé=valeur ...` en dictionnaire plat."""
    res = {}
    for line in text.splitlines():
        if "[mm_bench]" not in line:
            continue
        kv = dict(re.findall(r"(\w+)=(\S+)", line))
        if "test" in kv and "op" in kv:
            for k in ("p50_ns", "p99_ns", "mean_ns"):
                res[f"mm.{kv['test']}.{kv['op']}.{k}"] = int(kv[k])
        elif kv.get("test") == "scale" and "kops" in kv:
            res[f"mm.scale.{kv['cpus']}cpu.kops"] = int(kv["kops"])
        elif kv.get("test") == "scale" and kv.get("status") == "stuck":
            res["mm.scale.stuck_at_cpus"] = int(kv["cpus"])
        elif "allocator" in kv:
            res["mm.allocator"] = kv["allocator"]
        elif "leak" in kv:
            res["mm.leak_frames"] = int(kv["leak"])
    return res


def parse_sched_bench(text):
    """Extrait les lignes `[sched_bench] test=... clé=valeur` en dictionnaire plat."""
    res = {}
    for line in text.splitlines():
        if "[sched_bench]" not in line:
            continue
        kv = dict(re.findall(r"(\w+)=(\S+)", line))
        test = kv.pop("test", None)
        if test is None:
            continue
        if kv.get("status") == "stuck":
            res[f"sched.{test}.stuck"] = 1
            continue
        for k, v in kv.items():
            if k != "n" and re.fullmatch(r"-?\d+", v):
                res[f"sched.{test}.{k}"] = int(v)
    return res


def parse_heap_bench(text):
    """Extrait les lignes `[heap_bench] test=... clé=valeur` en dictionnaire plat."""
    res = {}
    for line in text.splitlines():
        if "[heap_bench]" not in line:
            continue
        kv = dict(re.findall(r"(\w+)=(\S+)", line))
        test = kv.pop("test", None)
        if test == "lat":
            for k in ("p50_ns", "p99_ns"):
                res[f"heap.lat.{kv['size']}.{kv['op']}.{k}"] = int(kv[k])
        elif test == "scale":
            res[f"heap.scale.{kv['cpus']}cpu.kops"] = int(kv["kops"])
        elif test in ("churn", "xfree"):
            res[f"heap.{test}.kops"] = int(kv["kops"])
        elif test == "rss":
            for k in ("peak_kib", "overhead_pct", "retained_kib"):
                res[f"heap.rss.{k}"] = int(kv[k])
    return res


def wait_after(q, offset, pattern, timeout):
    """Attend `pattern` dans le journal série après `offset` ; renvoie la correspondance."""
    rx = re.compile(pattern)
    end = time.time() + timeout
    while time.time() < end:
        text = q.log()
        if re.search(r"\bPANIC\b|panicked at|EXCEPTION:|[Kk]ernel panic", text):
            raise RuntimeError("panique du noyau détectée sur le port série")
        m = rx.search(text[offset:])
        if m:
            return m
        time.sleep(0.5)
    q.nmi_dump()
    raise TimeoutError(f"délai dépassé ({timeout} s) en attendant /{pattern}/")


def run_ns_tests(q, args):
    """Conteneurs (plan 07, S3) : capacités, espaces utilisateur/montage/UTS/IPC, pivot_root."""
    base = "/extra_files/linux"
    return {"linux.nstest.fails": run_counted(
        q, args, f"run -s {base}/nstest", r"nstest : FIN (\d+) ok, (\d+) echec", r"nstest : debut \(pid (\d+)")}


def run_linux_tests(q, args):
    """MaiProc : binaires Linux statiques (musl, BusyBox) en mode utilisateur.

    Chaque commande passe par `run -s` (sortie copiée dans le journal série) ; un échec compte
    dans `linux.*.fails` (bloquant)."""
    res = {}
    base = "/extra_files/linux"
    for name in ("maitest", "maitest_pie"):
        offset = len(q.log())
        q.type(f"run -s {base}/{name} alpha beta")
        m = wait_after(q, offset, r"maitest : (OK|ECHEC) \((\d+) echec", args.timeout)
        res[f"linux.{name}.fails"] = int(m.group(2))
        wait_after(q, offset, r"maiproc : pid \d+ .* termine", args.timeout)
    # Faute : le processus doit être tué par SIGSEGV, sans toucher au noyau.
    offset = len(q.log())
    q.type(f"run -s {base}/maitest segv")
    wait_after(q, offset, r"termine : tue par le signal 11", args.timeout)
    res["linux.segv.fails"] = 0
    # Code de sortie transmis.
    offset = len(q.log())
    q.type(f"run -s {base}/maitest exit 42")
    m = wait_after(q, offset, r"termine : code (\d+)", args.timeout)
    res["linux.exit_code.fails"] = 0 if m.group(1) == "42" else 1
    # BusyBox (glibc statique) : quelques applets.
    checks = [
        ("echo bonjour-busybox", r"\] bonjour-busybox"),
        ("uname -a", r"\] Linux maios 6\.12\.0-maios"),
        (f"ls {base}", r"\] .*maitest"),
        ("cat /proc/meminfo", r"\] MemTotal:"),
    ]
    fails = 0
    for cmd, pattern in checks:
        offset = len(q.log())
        q.type(f"run -s {base}/busybox {cmd}")
        try:
            wait_after(q, offset, pattern, 30)
        except TimeoutError:
            print(f"busybox {cmd} : sortie attendue absente (/{pattern}/)")
            fails += 1
        wait_after(q, offset, r"maiproc : pid \d+ .* termine", args.timeout)
    res["linux.busybox.fails"] = fails
    # Jalon M2 : fork, execve, wait4, signaux, tubes, fils, futex.
    res["linux.proctest.fails"] = run_counted(
        q, args, f"run -s {base}/proctest", r"proctest : (OK|ECHEC) \((\d+) echec", r"proctest : jalon M2 \(pid (\d+)")
    # Shell de BusyBox : tubes, redirections, sous-shells, trap, tâches de fond.
    res["linux.sh.fails"] = run_counted(
        q, args, f"run -s {base}/busybox sh {base}/m2.sh", r"sh : termine", r"sh : debut \(pid (\d+)")
    # Jalon M3 : vDSO, epoll, eventfd/timerfd/signalfd, pidfd, sockets Unix, /proc ; musl
    # statique puis glibc dynamique (ld-linux-x86-64.so.2, libc.so.6).
    # Variantes i386 (glibc i386, plan 07 S2) seulement si `build.sh` les a produites.
    i386 = os.path.exists(os.path.join(ROOT, "extra_files", "linux", "m4test_i386"))
    for name in ("m3test", "m3test_glibc") + (("m3test_i386",) if i386 else ()):
        res[f"linux.{name}.fails"] = run_counted(
            q, args, f"run -s {base}/{name}", r"m3test : (OK|ECHEC) \((\d+) echec", r"m3test : jalon M3 \(pid (\d+)")
    # Suite de M3 : TCP/UDP (chemin rapide local, IPv6, double pile), io_uring ; la version
    # musl éprouve aussi le réseau réel (écho par l'hôte de QEMU, smoltcp sur e1000).
    echo = start_echo_server(q)
    try:
        time.sleep(1)
        res["linux.m3net.fails"] = run_counted(
            q, args, f"run -s {base}/m3net ext {ECHO_PORT}", r"m3net : (OK|ECHEC) \((\d+) echec", r"m3net : jalon M3 reseau et io_uring \(pid (\d+)")
    finally:
        stop_echo_server(q, echo)
    for name in ("m3net_glibc",) + (("m3net_i386",) if i386 else ()):
        res[f"linux.{name}.fails"] = run_counted(
            q, args, f"run -s {base}/{name}", r"m3net : (OK|ECHEC) \((\d+) echec", r"m3net : jalon M3 reseau et io_uring \(pid (\d+)")
    # inotify, minuteurs POSIX, priorités, renameat2, FIONREAD d'UDP ; musl puis glibc.
    for name in ("m4test", "m4test_glibc") + (("m4test_i386",) if i386 else ()):
        res[f"linux.{name}.fails"] = run_counted(
            q, args, f"run -s {base}/{name}", r"m4test : (OK|ECHEC) \((\d+) echec", r"m4test : inotify, minuteurs, priorites \(pid (\d+)")
    # ABI i386 (plan 07, S2) : programme 32 bits autonome (TLS, fils, signaux, fork/exec…).
    res["linux.ia32test.fails"] = run_counted(
        q, args, f"run -s {base}/ia32test", r"ia32test : FIN (\d+) ok, (\d+) echec", r"ia32test : debut \(pid (\d+)")
    res.update(run_ns_tests(q, args))
    # Programmes Debian liés dynamiquement à la glibc, et applets de BusyBox qui lisent /proc.
    checks = [
        (f"{base}/glibc/ls /proc/self", r"\] .*(maps|status)"),
        (f"{base}/glibc/cat /proc/version", r"\] Linux version 6\.12\.0-maios"),
        (f"{base}/glibc/dash -c exit", None),
        (f"{base}/busybox ps", r"\] +PID +USER"),
        (f"{base}/busybox top -b -n 1", r"\] +PID +PPID"),
    ]
    fails = 0
    for cmd, pattern in checks:
        offset = len(q.log())
        q.type(f"run -s {cmd}")
        try:
            if pattern:
                wait_after(q, offset, pattern, 30)
            m = wait_after(q, offset, r"maiproc : pid \d+ .* termine : (.*) \(RSS", args.timeout)
            if "code 0" not in m.group(1):
                print(f"{cmd} : {m.group(1)}")
                fails += 1
        except TimeoutError:
            print(f"{cmd} : sortie attendue absente (/{pattern}/)")
            tail = q.log()[offset:]
            for line in tail.splitlines()[-12:]:
                print("   | " + line)
            fails += 1
    res["linux.dynamic.fails"] = fails
    return res


# Serveur d'écho TCP et UDP sur l'hôte de QEMU (joignable par l'invité en 10.0.2.2:ECHO_PORT,
# réseau utilisateur de QEMU) : éprouve la pile smoltcp et la carte e1000 de bout en bout.
ECHO_PORT = 18765
ECHO_SERVER = r"""
import socket, sys, threading
port = int(sys.argv[1])
def tcp():
    s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", port)); s.listen(8)
    while True:
        c, _ = s.accept()
        def serve(c=c):
            while True:
                d = c.recv(65536)
                if not d: break
                c.sendall(d)
            c.close()
        threading.Thread(target=serve, daemon=True).start()
def udp():
    u = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); u.bind(("127.0.0.1", port))
    while True:
        d, a = u.recvfrom(65536); u.sendto(d, a)
threading.Thread(target=tcp, daemon=True).start()
udp()
"""


# Même serveur en Perl pour WSL (Debian minimal : Perl est là, pas Python).
ECHO_SERVER_PERL = r"""
# maios-echo
use IO::Socket::INET; use IO::Select;
my $p = shift;
my $t = IO::Socket::INET->new(LocalAddr => "127.0.0.1", LocalPort => $p, Listen => 8, ReuseAddr => 1, Proto => "tcp") or die;
my $u = IO::Socket::INET->new(LocalAddr => "127.0.0.1", LocalPort => $p, Proto => "udp") or die;
my $sel = IO::Select->new($t, $u);
while (my @r = $sel->can_read) {
    for my $h (@r) {
        if ($h == $t) { $sel->add($t->accept); }
        elsif ($h == $u) { my $a = $u->recv(my $d, 65536); $u->send($d, 0, $a); }
        else { my $n = sysread($h, my $d, 65536); if (!$n) { $sel->remove($h); close $h; } else { syswrite($h, $d); } }
    }
}
"""


def start_echo_server(q):
    """Serveur d'écho dans le même espace réseau que QEMU (WSL sous KVM, Windows sinon)."""
    if q.wsl:
        stop_echo_server(q)
        cmd = ["wsl", "-d", "Debian", "--exec", "perl", "-e", ECHO_SERVER_PERL, str(ECHO_PORT)]
    else:
        cmd = [sys.executable, "-c", ECHO_SERVER, str(ECHO_PORT)]
    return subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def stop_echo_server(q, proc=None):
    if proc:
        proc.kill()
    if q.wsl:
        # `wsl.exe` tué ne tue pas forcément le Perl lancé dans WSL.
        subprocess.run(["wsl", "-d", "Debian", "--exec", "pkill", "-f", "maios-echo"], capture_output=True)


def run_ext4_tests(q, args):
    """MaiSS : volume ext4 en NVMe monté au démarrage et prêté à la racine ; programmes Debian
    liés dynamiquement (bash, perl, coreutils) lus depuis le disque, et SHA-256 de tout `/usr`."""
    res = {}
    log = q.log()
    m = re.search(r"MaiSS : ext4 (\S+) monte sur /mnt/(\S+)", log)
    res["ext4.mount.fails"] = 0 if m and "racine Linux" in log else 1
    if res["ext4.mount.fails"]:
        print("ext4 : volume non monte ou racine Linux non pretee")
        for line in [l for l in log.splitlines() if "ext4" in l or "MaiSS" in l][-10:]:
            print("   | " + line)
        return res
    offset = len(q.log())
    res["ext4.test.fails"] = run_counted(
        q, args, "run -s /bin/bash /opt/maios/ext4test.sh", r"ext4test : FIN (\d+) ok, (\d+) echec", r"ext4test : debut \(pid (\d+)")
    d = re.search(r"ext4test : debit (\d+) Mio/s", q.log()[offset:])
    if d:
        res["ext4.sha256_mib_s"] = int(d.group(1))
    return res


def run_python_tests(q, args):
    """Python 3 de Debian lu depuis la racine ext4 : modules C, fils, sous-processus, sockets,
    asyncio, puis une sélection de la suite de régression de CPython (`/opt/maios/pytest.sh`)."""
    res = {}
    offset = len(q.log())
    extra = " " + args.pytests if args.pytests else ""
    res["python.test.fails"] = run_counted(
        q, args, "run -s /bin/bash /opt/maios/pytest.sh" + extra, r"pytest : FIN (\d+) ok, (\d+) echec", r"pytest : debut \(pid (\d+)")
    m = re.search(r"pytest : temps demarrage (\d+) ms", q.log()[offset:])
    if m:
        res["python.startup_ms"] = int(m.group(1))
    text = q.log()[offset:]
    for line in text.splitlines():
        if " | " in line and line.lstrip().startswith("test_"):
            print("   " + line.strip())
    return res


def run_wayland_tests(q, args):
    """MaiLand : client libwayland de Debian (`/opt/maios/wlhello`, banc de conformité et de
    latence), puis `wayland-info` (wayland-utils) qui interroge toutes les globales."""
    res = {}
    offset = len(q.log())
    during = None
    if args.shots:
        png = os.path.join(ROOT, "build", "bench_shots", "3g_wayland_fenetre.png")
        during = (r"wltest : fenetre affichee", lambda: q.screenshot(png))
    res["wayland.test.fails"] = run_counted(
        q, args, "run -s /opt/maios/wlhello 3", r"wltest : FIN (\d+) ok, (\d+) echec", r"wltest : debut \(pid (\d+)", during)
    text = q.log()[offset:]
    m = re.search(r"wltest : aller-retour ([\d.]+) us", text)
    if m:
        res["wayland.roundtrip_us"] = float(m.group(1))
    m = re.search(r"wltest : \d+ images en \d+ ms \(([\d.]+) i/s\)", text)
    if m:
        res["wayland.frame_fps"] = float(m.group(1))
    res["wayland.input.fails"] = run_counted(
        q, args, "run -s /opt/maios/wlinput", r"wlinput : FIN (\d+) ok, (\d+) echec", r"wlinput : debut \(pid (\d+)",
        wlinput_steps(q, args))
    verdict = {"fails": 0}
    res["wayland.timing.fails"] = run_counted(
        q, args, "run -s /opt/maios/wltime", r"wltime : FIN (\d+) ok, (\d+) echec", r"wltime : debut \(pid (\d+)",
        wltime_steps(q, args, verdict))
    res["wayland.pixels.fails"] = verdict["fails"]
    verdict = {"fails": 0}
    res["wayland.sub.fails"] = run_counted(
        q, args, "run -s /opt/maios/wlsub", r"wlsub : FIN (\d+) ok, (\d+) echec", r"wlsub : debut \(pid (\d+)",
        wlsub_steps(q, args, verdict)) + verdict["fails"]
    res["wayland.sec.fails"] = run_counted(
        q, args, "run -s /opt/maios/wlsec", r"wlsec : FIN (\d+) ok, (\d+) echec", r"wlsec : debut \(pid (\d+)")
    text = q.log()
    m = re.search(r"wltime : presentation : \d+/\d+ presentees, intervalle moyen ([\d.]+) ms, latence max ([\d.]+) ms", text)
    if m:
        res["wayland.present_interval_ms"] = float(m.group(1))
        res["wayland.present_latency_max_ms"] = float(m.group(2))
    offset = len(q.log())
    q.type("run -s /usr/bin/wayland-info")
    try:
        wait_after(q, offset, r"maiproc : pid \d+ .* termine", args.timeout)
    except TimeoutError as e:
        print(f"wayland-info : {e}")
    text = q.log()[offset:]
    want = ["wl_compositor", "wl_subcompositor", "wl_shm", "wl_output", "xdg_wm_base", "wl_seat", "wl_data_device_manager",
            "zxdg_decoration_manager_v1", "wp_cursor_shape_manager_v1", "wp_presentation", "wp_fifo_manager_v1",
            "wp_commit_timing_manager_v1", "wp_viewporter", "wp_fractional_scale_manager_v1",
            "wp_single_pixel_buffer_manager_v1", "xdg_activation_v1",
            "zwp_primary_selection_device_manager_v1", "wp_security_context_manager_v1"]
    missing = [w for w in want if f"interface: '{w}'" not in text]
    print("   wayland-info : " + ("toutes les globales" if not missing else "manque " + ", ".join(missing)))
    res["wayland.info.fails"] = len(missing)
    return res


# Clients Wayland réels (phase W4) : nom de la capture → commande dans la racine Debian.
APPS = {
    "foot": "foot",
    "gtk4": "gtk4-widget-factory",
    "sdl3": "/usr/libexec/installed-tests/SDL3/testsprite --renderer software",
    "qt6": "/usr/lib/x86_64-linux-gnu/qt6/examples/widgets/widgets/calculator/bin/calculator",
    "firefox": "firefox-esr --new-instance --no-remote about:blank",
}


def run_apps_tests(q, args):
    """MaiLand W4 : lance chaque client réel de `--apps` par `/opt/maios/apprun.sh` (environnement
    d'une session Wayland, rendu logiciel), capture l'écran après `--app-wait` secondes, montre
    ce que le client a écrit, puis attend sa fin (tué au bout de `--app-wait` + 10 s)."""
    res = {}
    names = [a.strip() for a in args.apps.split(",") if a.strip()]
    os.makedirs(os.path.join(ROOT, "build", "bench_shots"), exist_ok=True)
    for name in names:
        cmd = APPS.get(name, name)
        label = re.sub(r"[^A-Za-z0-9_]+", "_", name)[:24]
        offset = len(q.log())
        tr = f"--strace {args.app_strace} " if args.app_strace else ""
        q.type(f"run -s /bin/bash /opt/maios/apprun.sh {tr}{int(args.app_wait) + 10} {cmd}")
        try:
            wait_after(q, offset, r"apprun : debut", 60)
        except TimeoutError as e:
            print(f"{name} : {e}")
            res[f"apps.{label}.fails"] = 1
            continue
        end = time.time() + args.app_wait
        while time.time() < end and not re.search(r"apprun : FIN", q.log()[offset:]):
            time.sleep(0.5)
        q.screenshot(os.path.join(ROOT, "build", "bench_shots", f"4_{label}.png"))
        try:
            wait_after(q, offset, r"apprun : FIN \S+ code -?\d+", args.app_wait + 40)
        except TimeoutError as e:
            print(f"{name} : {e}")
        text = q.log()[offset:]
        m = re.search(r"apprun : FIN \S+ code (-?\d+)", text)
        killed = "tue apres" in text
        res[f"apps.{label}.fails"] = 0 if killed else 1
        print(f"   {name} : " + ("toujours vivant apres %d s (capture 4_%s.png)" % (args.app_wait, label) if killed
                                 else "termine, code " + (m.group(1) if m else "?")))
        for line in text.splitlines()[-args.app_lines:]:
            print("   | " + line)
        fresh_terminal(q)
    return res


def run_x11_tests(q, args):
    """Plan 07, S1 : Xwayland dans MaiLand (`/opt/maios/xtest.sh` : extensions, GLX logiciel,
    xrandr, entrées X pilotées par le moniteur, fin propre du serveur)."""
    res = {}

    def entrees():
        # Pointeur dans le contenu de la fenêtre Xwayland, clic (focus MaiLand), puis touches.
        ps2_move(q, -3000, -3000)
        ps2_move(q, 400, 300)
        time.sleep(0.5)
        for c in ("mouse_button 1", "mouse_button 0", "sendkey q", "sendkey shift-a"):
            q.monitor(c)
        if args.shots:
            q.screenshot(os.path.join(ROOT, "build", "bench_shots", "5a_x11_xev.png"))

    offset = len(q.log())
    res["x11.test.fails"] = run_counted(
        q, args, "run -s /bin/bash /opt/maios/xtest.sh", r"xtest : FIN (\d+) ok, (\d+) echec",
        r"xtest : debut \(pid (\d+)", [(r"xtest : pret entrees", entrees)])
    text = q.log()[offset:]
    m = re.search(r"xtest : demarrage de Xwayland (\d+) ms", text)
    if m:
        res["x11.xwayland_start_ms"] = float(m.group(1))
    m = re.search(r"xtest : glxgears ([\d.]+) i/s", text)
    if m:
        res["x11.glxgears_fps"] = float(m.group(1))
    return res


def ps2_move(q, dx, dy):
    """Déplace la souris PS/2 de QEMU de `(dx, dy)`.

    La file PS/2 de QEMU ne tient que 16 octets (4 paquets) et un paquet porte au plus ±127 :
    le reste d'un grand déplacement attend dans QEMU et s'ajoute à la commande suivante (un
    `mouse_move 400 300` après `-2000 -2000` repartait vers le coin). Un paquet par commande,
    le temps que l'invité le lise (`q.monitor` attend 0,1 s)."""
    while dx or dy:
        sx, sy = max(-127, min(127, dx)), max(-127, min(127, dy))
        q.monitor(f"mouse_move {sx} {sy}")
        dx, dy = dx - sx, dy - sy


def wltime_steps(q, args, verdict):
    """Étapes de `wltime` : captures vérifiées au pixel près (voir `pixel_steps`)."""
    return pixel_steps(q, args, verdict, "wltime", ("rotation", "recadrage", "pixel"), "3j")


def pixel_steps(q, args, verdict, prog, names, prefix):
    """Étapes d'un client vérifié sur captures : à chaque « <prog> : pret capture <nom> <L> <H>
    <points> », capture de l'écran, vérification au pixel près, puis espace pour que le client
    continue.

    Chaque point est `x,y,RRGGBB` relatif au coin du contenu de la fenêtre ; le premier,
    `0,0`, sert d'ancre : c'est le premier pixel de l'écran (ordre de lecture) qui a sa couleur.
    `verdict["fails"]` compte les points faux et les captures manquées ; `verdict["origin"]`
    garde le coin du contenu trouvé (pour placer le pointeur ensuite)."""
    def capture(name):
        def run():
            if name == names[0]:
                # Pointeur dans le coin haut droit (barre de titre vide) : il ne cache rien.
                ps2_move(q, 3000, -3000)
            lines = re.findall(rf"{prog} : pret capture {name} (\d+) (\d+) ([^\r\n]*)", q.log())
            png = os.path.join(ROOT, "build", "bench_shots", f"{prefix}_wayland_{name}.png")
            image = q.grab(png) if lines else None
            if not image:
                print(f"   [ECHEC] capture {name} impossible")
                verdict["fails"] += 1
                q.monitor("sendkey spc")
                return
            if args.shots:
                q.screenshot(png, image)
            w, h, px = image
            points = []
            for p in lines[-1][2].split():
                x, y, c = p.split(",")
                points.append((int(x), int(y), tuple(bytes.fromhex(c))))
            anchor = bytes(points[0][2])
            start = 0
            while True:
                i = px.find(anchor, start)
                if i < 0 or i % 3 == 0:
                    break
                start = i + 1
            if i < 0:
                print(f"   [ECHEC] capture {name} : couleur d'ancre {anchor.hex()} absente de l'ecran")
                verdict["fails"] += 1
                q.monitor("sendkey spc")
                return
            ox, oy = (i // 3) % w, (i // 3) // w
            verdict["origin"] = (ox, oy)
            bad = []
            for x, y, want in points:
                sx, sy = ox + x, oy + y
                got = tuple(px[(sy * w + sx) * 3:(sy * w + sx) * 3 + 3]) if 0 <= sx < w and 0 <= sy < h else None
                if got != want:
                    bad.append(f"({x},{y}) {bytes(want).hex()} -> {bytes(got).hex() if got else 'hors ecran'}")
            if bad:
                print(f"   [ECHEC] capture {name} (contenu en {ox},{oy}) : " + "; ".join(bad))
                verdict["fails"] += len(bad)
            else:
                print(f"   [ok] capture {name} : {len(points)} pixels exacts (contenu en {ox},{oy})")
            q.monitor("sendkey spc")
        return run

    return [(rf"{prog} : pret capture {n} ", capture(n)) for n in names]


def run_gpu_tests(q, args):
    """MaiGPU G0 : `gpu_test` (pilote virtio-gpu). Captures vérifiées au pixel près du motif
    et de la mise à jour partielle, pointeur amené dans la fenêtre puis bougé (curseur
    matériel : aucun pixel ne doit partir), puis coût de la présentation."""
    res = {}
    verdict = {"fails": 0}
    steps = pixel_steps(q, args, verdict, "gputest", ("motif", "partiel"), "3m")

    def curseur():
        # Coin haut gauche, puis dans la fenêtre de test (100..356 × 100..292).
        ps2_move(q, -3000, -3000)
        ps2_move(q, 220, 200)
        time.sleep(0.5)
        q.monitor("sendkey spc")

    def mesure():
        # gpu_test prend sa référence 0,4 s après sa ligne (rendu du terminal).
        time.sleep(1)
        for i in range(24):
            ps2_move(q, 6 if i % 2 == 0 else -6, 4 if i % 4 < 2 else -4)
        time.sleep(0.5)
        q.monitor("sendkey spc")

    def tablette():
        # Position absolue (0..32767 sur chaque axe) au centre de la fenêtre, clic gauche.
        m = re.search(r"gputest : pret tablette (\d+) (\d+)", q.log()[offset:])
        ax, ay = int(m.group(1)), int(m.group(2))
        abs_ev = [{"type": "abs", "data": {"axis": "x", "value": ax}},
                  {"type": "abs", "data": {"axis": "y", "value": ay}}]
        for events in (abs_ev,
                       [{"type": "btn", "data": {"down": True, "button": "left"}}],
                       [{"type": "btn", "data": {"down": False, "button": "left"}}]):
            r = q.qmp("input-send-event", {"events": events})
            if "error" in r:
                print("   QMP input-send-event : " + str(r["error"]))
            time.sleep(0.3)
        q.monitor("sendkey spc")

    steps += [(r"gputest : pret curseur", curseur), (r"gputest : curseur mesure", mesure),
              (r"gputest : pret tablette", tablette)]
    offset = len(q.log())
    res["gpu.fails"] = run_counted(q, args, "gpu_test", r"gputest : FIN (\d+) ok, (\d+) echec",
                                   r"gputest : debut \(pid (\d+)", steps) + verdict["fails"]
    text = q.log()[offset:]
    if "gputest : tablette absente" in text:
        print("   [ECHEC] pointeur virtio-tablet absent")
        res["gpu.fails"] += 1
    for name in ("plein_ecran", "petite_zone"):
        m = re.search(rf"gputest : cout {name} : (\d+) images en (\d+) ms, appareil occupe (\d+) pm, (\d+) Kio/presentation, "
                      rf"presentation moy (\d+) us .*?attente avant ecriture (\d+) us/image, composition moy (\d+) us", text)
        if m:
            res[f"gpu.{name}.busy_pm"] = int(m.group(3))
            res[f"gpu.{name}.kib_per_present"] = int(m.group(4))
            res[f"gpu.{name}.present_us"] = int(m.group(5))
            res[f"gpu.{name}.wait_us"] = int(m.group(6))
            res[f"gpu.{name}.compose_us"] = int(m.group(7))
    m = re.search(r"gputest : partiel : (\d+) octets", text)
    if m:
        res["gpu.partial_bytes"] = int(m.group(1))
    m = re.search(r"gputest : curseur : (\d+) MOVE_CURSOR, \d+ UPDATE_CURSOR, (\d+) octets transferes en \d+ ms "
                  r"\((\d+) octets au repos.*?, (\d+) traces logiciels", text)
    if m:
        res["gpu.cursor_moves"] = int(m.group(1))
        res["gpu.cursor_bytes"] = int(m.group(2))
        res["gpu.cursor_idle_bytes"] = int(m.group(3))
        res["gpu.cursor_sw_draws"] = int(m.group(4))
    for line in text.splitlines():
        if any(k in line for k in ("gputest : info", "gputest : edid", "gputest : cout", "gputest : rapport", "gputest : tablette")):
            print("   " + line.split("gputest : ", 1)[1])
    return res


def wlsub_steps(q, args, verdict):
    """Étapes de `wlsub` (sous-surfaces) : captures au pixel près, puis le pointeur amené en
    deux points du contenu (« wlsub : pret pointeur X Y ») : coin de l'écran, puis déplacement
    relatif jusqu'au coin du contenu trouvé par la dernière capture."""
    names = ("cache", "sync", "pile", "ordre", "desync", "alpha", "position")
    steps = pixel_steps(q, args, verdict, "wlsub", names, "3k")

    def pointer(n):
        def run():
            pts = re.findall(r"wlsub : pret pointeur (\d+) (\d+)", q.log())
            ox, oy = verdict.get("origin", (0, 0))
            if len(pts) >= n:
                x, y = map(int, pts[n - 1])
                ps2_move(q, -3000, -3000)
                ps2_move(q, ox + x, oy + y)
                time.sleep(0.5)
            q.monitor("sendkey spc")
        return run
    steps += [(r"wlsub : pret pointeur 110 20", pointer(1)), (r"wlsub : pret pointeur 70 80", pointer(2))]
    steps += pixel_steps(q, args, verdict, "wlsub", ("detruite",), "3k")
    return steps


def wlinput_steps(q, args):
    """Étapes de `wlinput` : à chaque marqueur du client, les entrées de l'utilisateur simulées
    par le moniteur (souris PS/2 relative : `mouse_move dx dy [molette]`, `mouse_button` masque
    1 gauche, 2 droit, 4 milieu)."""
    def mon(*cmds):
        def run():
            for c in cmds:
                q.monitor(c)
        return run

    def move(dx, dy):
        ps2_move(q, dx, dy)

    def survole():
        if args.shots:
            q.screenshot(os.path.join(ROOT, "build", "bench_shots", "3i_wayland_menu.png"))
        # Clic hors du menu : il se ferme.
        move(-150, -150)
        mon("mouse_button 1", "mouse_button 0")()

    def souris():
        # Coin haut gauche, puis au milieu de la fenêtre agrandie ; clic gauche ; molette.
        move(-2000, -1200)
        move(400, 300)
        mon("mouse_button 1", "mouse_button 0", "mouse_move 0 0 1")()
        if args.shots:
            q.screenshot(os.path.join(ROOT, "build", "bench_shots", "3h_wayland_souris.png"))

    return [
        (r"wlinput : pret clavier", mon("sendkey q", "sendkey shift-a")),
        (r"wlinput : pret souris", souris),
        (r"wlinput : pret menu", mon("mouse_button 2")),
        (r"wlinput : menu ouvert", mon("mouse_button 0")),
        (r"wlinput : menu survole", survole),
    ]


def run_bwrap_tests(q, args):
    """Plan 07, S3 : le vrai bubblewrap de Debian (`/opt/maios/bwraptest.sh`), en root puis en
    utilisateur 1000 (espace utilisateur non privilégié, comme pressure-vessel)."""
    return {"bwrap.test.fails": run_counted(
        q, args, "run -s /bin/bash /opt/maios/bwraptest.sh", r"bwraptest : FIN (\d+) ok, (\d+) echec",
        r"bwraptest : debut \(pid (\d+)")}


def run_counted(q, args, cmd, done, start, during=None):
    """Lance `cmd`, attend `done`, compte les lignes [ECHEC] (et les absences de [ok] attendues),
    puis attend la fin du processus de tête (PID lu dans la ligne `start`). `during` :
    `(motif, action)` ou liste de tels couples, chaque action lancée dans l'ordre quand son motif
    apparaît (capture, touches et souris pendant le test)."""
    offset = len(q.log())
    q.type(cmd)
    steps = [during] if isinstance(during, tuple) else (during or [])
    for pattern, action in steps:
        try:
            wait_after(q, offset, pattern, min(args.timeout, 60))
        except TimeoutError:
            break
        action()
    try:
        m = wait_after(q, offset, done, args.timeout)
    except TimeoutError as e:
        print(f"{cmd} : {e}")
        tail = q.log()[offset:]
        for line in tail.splitlines()[-25:]:
            print("   | " + line)
        fresh_terminal(q)
        return 1
    text = q.log()[offset:]
    for line in text.splitlines():
        if "[ECHEC]" in line or "[ok]" in line:
            print("   " + line.split("] ", 1)[-1] if line.startswith("[pid") else "   " + line)
    fails = text.count("[ECHEC]")
    if m.lastindex and m.lastindex >= 2 and m.group(2).isdigit():
        fails = max(fails, int(m.group(2)))
    pid = re.search(start, text)
    if pid:
        try:
            wait_after(q, offset, rf"maiproc : pid {pid.group(1)} .* termine", args.timeout)
        except TimeoutError:
            print(f"{cmd} : fin du processus {pid.group(1)} non vue")
            fails += 1
            fresh_terminal(q)
    return fails


def fresh_terminal(q):
    """Un processus bloqué garde le terminal : les commandes suivantes partent dans un nouveau
    terminal, sinon chacune attendrait son délai pour rien."""
    q.monitor("sendkey ctrl-alt-t")
    time.sleep(4)


def run_once(args):
    q = Qemu(args)
    results = {}
    shots = os.path.join(ROOT, "build", "bench_shots")
    os.makedirs(shots, exist_ok=True)

    def shot(name):
        if args.shots:
            q.screenshot(os.path.join(shots, name + ".png"))

    try:
        q.wait_for(BOOT_DONE, args.boot_wait)
        # Le bureau et les tâches de fond finissent de démarrer après captain.
        time.sleep(args.settle if args.settle is not None else (3 if args.kvm else 15))
        shot("1_boot")
        q.monitor("sendkey ctrl-alt-t")
        time.sleep(4)
        shot("2_terminal")
        if "mm" in args.tests:
            offset = len(q.log())
            q.type(f"mm_bench -n {args.iterations}")
            q.wait_for(r"\[mm_bench\] (done|error=)", args.timeout)
            results.update(parse_mm_bench(q.log()[offset:]))
            shot("3_mm_bench")
            if "mm.allocator" not in results:
                raise RuntimeError("mm_bench n'a produit aucun résultat")
        if "sched" in args.tests:
            offset = len(q.log())
            q.type("sched_bench")
            q.wait_for(r"\[sched_bench\] (done|error=)", args.timeout)
            results.update(parse_sched_bench(q.log()[offset:]))
            shot("3b_sched_bench")
            if not any(k.startswith("sched.") for k in results):
                raise RuntimeError("sched_bench n'a produit aucun résultat")
        if "heap" in args.tests:
            offset = len(q.log())
            q.type(f"heap_bench -n {args.iterations}")
            q.wait_for(r"\[heap_bench\] (done|error=)", args.timeout)
            results.update(parse_heap_bench(q.log()[offset:]))
            shot("3c_heap_bench")
            if not any(k.startswith("heap.") for k in results):
                raise RuntimeError("heap_bench n'a produit aucun résultat")
        if "linux" in args.tests:
            results.update(run_linux_tests(q, args))
            shot("3d_linux")
        elif "ns" in args.tests:
            results.update(run_ns_tests(q, args))
        if "ext4" in args.tests:
            results.update(run_ext4_tests(q, args))
            shot("3e_ext4")
        if "python" in args.tests:
            results.update(run_python_tests(q, args))
            shot("3f_python")
        if "wayland" in args.tests:
            results.update(run_wayland_tests(q, args))
            shot("3g_wayland")
        if "apps" in args.tests:
            results.update(run_apps_tests(q, args))
        if "x11" in args.tests:
            results.update(run_x11_tests(q, args))
        if "bwrap" in args.tests:
            results.update(run_bwrap_tests(q, args))
        if "gpu" in args.tests:
            results.update(run_gpu_tests(q, args))
            shot("3m_gpu")
        if "doom" in args.tests:
            q.type("run doom -timedemo demo1")
            m = q.wait_for(r"timed (\d+) gametics in (\d+) realtics(?: \(([\d.]+) fps\))?", args.timeout)
            gametics, realtics = int(m.group(1)), int(m.group(2))
            results["doom.gametics"] = gametics
            results["doom.realtics"] = realtics
            results["doom.fps"] = round(gametics * 35.0 / max(realtics, 1), 2)
            shot("4_doom")
    except Exception:
        q.screenshot(os.path.join(shots, "echec.png"))
        print("capture d'écran de l'échec : " + os.path.join(shots, "echec.png"))
        raise
    finally:
        q.stop()
    return results


def merge_runs(runs):
    """Médiane de chaque métrique numérique sur plusieurs exécutions."""
    out = {}
    for key in runs[0]:
        vals = [r[key] for r in runs if key in r]
        if isinstance(vals[0], (int, float)):
            vals.sort()
            out[key] = vals[len(vals) // 2]
        else:
            out[key] = vals[0]
    return out


def compare(current, baseline, tolerance):
    """Affiche le tableau des écarts et renvoie la liste des régressions bloquantes."""
    failures = []
    print(f"\n{'métrique':<34}{'référence':>12}{'mesure':>12}{'écart':>9}")
    for key in sorted(set(current) | set(baseline)):
        cur, ref = current.get(key), baseline.get(key)
        if not isinstance(cur, (int, float)) or not isinstance(ref, (int, float)) or ref == 0:
            print(f"{key:<34}{str(ref):>12}{str(cur):>12}")
            continue
        delta = (cur - ref) * 100.0 / ref
        print(f"{key:<34}{ref:>12}{cur:>12}{delta:>+8.1f}%")
        # Pour les FPS et le débit, plus haut est meilleur ; pour les latences, plus bas.
        higher_is_better = key.endswith((".fps", ".kops", ".jain_x1000", ".min_pct"))
        regression = -delta if higher_is_better else delta
        if key == "doom.fps" and regression > tolerance:
            failures.append(f"{key} régresse de {regression:.1f} % (> {tolerance} %)")
    for key, cur in current.items():
        if key.startswith(("linux.", "ext4.", "python.", "wayland.", "gpu.")) and key.endswith(".fails") and cur:
            failures.append(f"{key} = {cur} (contrôle Linux en échec)")
    return failures


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--iso", default=os.path.join(ROOT, "build", "MaiOS.iso"))
    p.add_argument("--qemu", default=DEFAULT_QEMU)
    p.add_argument("--accel", default=None, help="ex. whpx ou kvm (défaut : TCG)")
    p.add_argument("--kvm", action="store_true", help="QEMU dans WSL avec KVM (rapide et stable)")
    p.add_argument("--distro", default="Debian", help="distribution WSL pour --kvm")
    p.add_argument("--cpu", default=None, help="modèle QEMU (défaut : host sous KVM, Broadwell sous TCG) ; ex. host,-pcid,-xsave")
    p.add_argument("--smp", type=int, default=4)
    p.add_argument("--memory", default="512M")
    p.add_argument("--tests", default="mm,doom", help="liste parmi mm,sched,heap,linux,ns,ext4,python,wayland,apps,x11,bwrap,gpu,doom")
    p.add_argument("--tablet", action="store_true",
                   help="ajoute un pointeur absolu virtio-tablet (et QMP) ; implicite avec --tests gpu")
    p.add_argument("--gpu", choices=("std", "virtio"), default="std",
                   help="carte graphique : std (VGA, framebuffer de Limine) ou virtio (virtio-vga, MaiGPU)")
    p.add_argument("--apps", default="foot,gtk4,sdl3,qt6,firefox", help="clients de --tests apps (noms de APPS ou commandes)")
    p.add_argument("--app-wait", type=float, default=25.0, help="secondes avant la capture d'un client de --tests apps")
    p.add_argument("--app-lines", type=int, default=40, help="lignes du journal montrées par client")
    p.add_argument("--app-strace", default="", help="trace des appels système des clients de --tests apps "
                   "(« errors:motif » ou « all:motif », voir /proc/maios/strace)")
    p.add_argument("--pytests", default="", help="modules de la suite de CPython pour --tests python (sinon la liste de pytest.sh)")
    p.add_argument("--rootfs", default=os.path.join(ROOT, "build", "rootfs.ext4"), help="image ext4 de --tests ext4")
    p.add_argument("--iterations", type=int, default=20000)
    p.add_argument("--runs", type=int, default=1, help="exécutions (médiane)")
    p.add_argument("--boot-wait", type=float, default=120.0, help="délai max du démarrage (s)")
    p.add_argument("--settle", type=float, default=None, help="pause après le démarrage (défaut : 3 s KVM, 15 s TCG)")
    p.add_argument("--timeout", type=float, default=120.0)
    p.add_argument("--tolerance", type=float, default=5.0, help="régression Doom tolérée (%%)")
    p.add_argument("--save-baseline", action="store_true")
    p.add_argument("--baseline", default=None, help="défaut : bench_baseline.json (TCG) ou bench_baseline_kvm.json (--kvm)")
    p.add_argument("--json", help="écrit aussi les résultats dans ce fichier")
    p.add_argument("--shots", action="store_true", help="captures d'écran à chaque étape (build/bench_shots)")
    args = p.parse_args()
    args.tests = [t.strip() for t in args.tests.split(",") if t.strip()]
    if "gpu" in args.tests:
        args.gpu = "virtio"
        args.tablet = True
    if args.baseline is None:
        args.baseline = BASELINE_KVM if args.kvm else BASELINE
    if args.kvm and args.qemu == DEFAULT_QEMU:
        args.qemu = "qemu-system-x86_64"

    runs = []
    for i in range(args.runs):
        print(f"exécution {i + 1}/{args.runs}…", flush=True)
        try:
            runs.append(run_once(args))
        except (RuntimeError, TimeoutError) as e:
            print(f"ÉCHEC : {e}")
            return 2
    current = merge_runs(runs)
    current["_meta"] = {"smp": args.smp, "memory": args.memory, "accel": "kvm" if args.kvm else (args.accel or "tcg"), "runs": args.runs,
                        "gpu": args.gpu,
                        "date": time.strftime("%Y-%m-%d %H:%M:%S")}

    if args.json:
        with open(args.json, "w", encoding="utf-8") as f:
            json.dump(current, f, indent=2, ensure_ascii=False)

    if args.save_baseline:
        with open(args.baseline, "w", encoding="utf-8") as f:
            json.dump(current, f, indent=2, ensure_ascii=False)
        print(f"référence enregistrée dans {args.baseline}")
        for k, v in sorted(current.items()):
            if k != "_meta":
                print(f"  {k:<34}{v:>12}")
        return 0

    if not os.path.exists(args.baseline):
        print("aucune référence : relancer avec --save-baseline")
        for k, v in sorted(current.items()):
            if k != "_meta":
                print(f"  {k:<34}{v:>12}")
        return 0
    with open(args.baseline, encoding="utf-8") as f:
        baseline = json.load(f)
    baseline.pop("_meta", None)
    meta = current.pop("_meta")
    failures = compare(current, baseline, args.tolerance)
    current["_meta"] = meta
    if failures:
        print("\nRÉGRESSION :\n  " + "\n  ".join(failures))
        return 1
    print("\nOK : aucune régression bloquante.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
