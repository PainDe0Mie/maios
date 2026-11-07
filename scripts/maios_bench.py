#!/usr/bin/env python3
"""Harnais de mesure MaiOS (MaiMM, phase M0).

Démarre l'ISO dans QEMU sans affichage, ouvre le terminal via le moniteur QEMU puis lance :
  0. `sched_bench` : changement de contexte, réveil, préemption, équité, équilibrage (MaiSched) ;
  1. `mm_bench -n N` : latences p50/p99 de l'allocateur de frames et montée en charge multi-cœurs ;
  1b. `heap_bench -n N` : tas noyau (latences par taille, churn, libérations distantes, montée en
     charge, mémoire rendue) ;
  2. `run doom -timedemo demo1` : Doom rejoue la démo 1 le plus vite possible. Le résultat
     (« timed N gametics in M realtics ») est recopié de stderr vers le port série par la libc.

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
# Le démarrage est terminé quand captain passe la main ; l'interface finit de s'installer ensuite.
BOOT_DONE = r"captain::init\(\): initialization done"
QEMU_NAME = "maios-bench"


def wsl_path(path):
    """Chemin Windows (lecteur E:) vu depuis WSL (/mnt/e/...)."""
    path = os.path.abspath(path)
    drive, rest = os.path.splitdrive(path)
    return "/mnt/" + drive[0].lower() + rest.replace("\\", "/")
ANSI = re.compile(r"\x1b\[[0-9;]*m")

KEYS = {" ": "spc", "/": "slash", ".": "dot", "-": "minus", "_": "shift-minus", "=": "equal"}


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
        cmd = [
            args.qemu, "-name", QEMU_NAME, "-cdrom", path(args.iso), "-boot", "d",
            "-m", args.memory, "-smp", str(args.smp), "-no-reboot", "-no-shutdown",
            "-serial", "file:" + path(self.serial), "-serial", "null",
            "-display", "none", "-monitor", f"tcp:127.0.0.1:{MONITOR_PORT},server,nowait",
        ]
        # KVM : le processeur réel (rdtscp, invariant TSC). TCG : Broadwell, le plus ancien modèle
        # qui a rdtscp.
        cmd += ["-cpu", args.cpu or ("host" if self.wsl else "Broadwell")]
        disk = os.path.join(ROOT, "fat32.img")
        if os.path.exists(disk):
            cmd += ["-drive", f"format=raw,file={path(disk)},if=ide,snapshot=on"]
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

    def type(self, text):
        for ch in text:
            self.monitor("sendkey " + KEYS.get(ch, ch))
            time.sleep(0.04)
        self.monitor("sendkey ret")

    def screenshot(self, png_path):
        """Capture l'écran QEMU (PPM via le moniteur) et l'enregistre en PNG."""
        ppm = png_path[:-4] + ".ppm"
        self.monitor("screendump " + (wsl_path(ppm) if self.wsl else ppm.replace("\\", "/")))
        for _ in range(20):
            time.sleep(0.25)
            if os.path.exists(ppm) and os.path.getsize(ppm) > 0:
                break
        time.sleep(0.5)
        try:
            data = open(ppm, "rb").read()
        except OSError:
            return
        parts = data.split(b"\n", 3)
        w, h = map(int, parts[1].split())
        px = parts[3]
        raw = b"".join(b"\x00" + px[y * w * 3:(y + 1) * w * 3] for y in range(h))

        def chunk(tag, body):
            return struct.pack(">I", len(body)) + tag + body + struct.pack(">I", zlib.crc32(tag + body) & 0xFFFFFFFF)

        png = (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
               + chunk(b"IDAT", zlib.compress(raw, 6)) + chunk(b"IEND", b""))
        with open(png_path, "wb") as f:
            f.write(png)
        os.remove(ppm)

    def log(self):
        """Journal série sans les codes couleur ANSI du logger."""
        try:
            with open(self.serial, "rb") as f:
                return ANSI.sub("", f.read().decode(errors="replace"))
        except FileNotFoundError:
            return ""

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
        raise TimeoutError(f"délai dépassé ({timeout} s) en attendant /{pattern}/")

    def stop(self):
        try:
            self.monitor("quit")
        except OSError:
            pass
        try:
            self.proc.wait(10)
        except subprocess.TimeoutExpired:
            self.kill()

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
    raise TimeoutError(f"délai dépassé ({timeout} s) en attendant /{pattern}/")


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
    return res


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
        if key.startswith("linux.") and key.endswith(".fails") and cur:
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
    p.add_argument("--tests", default="mm,doom", help="liste parmi mm,sched,heap,linux,doom")
    p.add_argument("--iterations", type=int, default=20000)
    p.add_argument("--runs", type=int, default=1, help="exécutions (médiane)")
    p.add_argument("--boot-wait", type=float, default=120.0, help="délai max du démarrage (s)")
    p.add_argument("--settle", type=float, default=None, help="pause après le démarrage (défaut : 3 s KVM, 15 s TCG)")
    p.add_argument("--timeout", type=float, default=300.0)
    p.add_argument("--tolerance", type=float, default=5.0, help="régression Doom tolérée (%%)")
    p.add_argument("--save-baseline", action="store_true")
    p.add_argument("--baseline", default=None, help="défaut : bench_baseline.json (TCG) ou bench_baseline_kvm.json (--kvm)")
    p.add_argument("--json", help="écrit aussi les résultats dans ce fichier")
    p.add_argument("--shots", action="store_true", help="captures d'écran à chaque étape (build/bench_shots)")
    args = p.parse_args()
    args.tests = [t.strip() for t in args.tests.split(",") if t.strip()]
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
