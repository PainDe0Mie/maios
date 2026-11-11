//! `blkbench` : banc d'E/S de MaiSS, à la manière de `fio`.
//!
//! * `qd1` : lectures aléatoires de 4 Kio, une à la fois : latence moyenne, p50, p99 (le cas
//!   où l'attente hybride compte le plus) ;
//! * `seq` : lecture séquentielle par requêtes de 1 Mio (débit, listes PRP) ;
//! * `qd32` : lots de 32 lectures aléatoires soumis d'un coup (une cloche par lot) ;
//! * `scale` : une tâche par cœur en QD1, chacune dans sa file matérielle (IOPS totales) ;
//! * `verify` (`--write`) : écritures de tailles et d'alignements variés dans les 16 derniers Mio
//!   de l'unité (tampons alignés, à cheval sur des pages, mal alignés pour forcer le rebond),
//!   relues et comparées, puis `flush`.
//!
//! Résultats : lignes `[blkbench] test=... clé=valeur` dans le journal (relevées par
//! `scripts/maios_bench.py`), et `[blkbench] done` ou `[blkbench] error=...` à la fin.

#![no_std]
#[macro_use]
extern crate app_io;
extern crate alloc;

use alloc::{format, string::String, sync::Arc, vec, vec::Vec};
use blk::{BlockDevice, Op, Request, Segment};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn report(line: String) {
    log::warn!("[blkbench] {}", line);
    println!("{}", line);
}

fn us(ns: u64) -> String {
    format!("{}.{}", ns / 1000, (ns % 1000) / 100)
}

fn qd1(dev: &BlockDevice, n: usize) -> Result<(), &'static str> {
    let bs = dev.block_size() as u64;
    let span = (dev.capacity * bs / 4096).max(1);
    let mut buf = vec![0u8; 4096];
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut lat: Vec<u64> = Vec::with_capacity(n);
    let t0 = time::Instant::now();
    for _ in 0..n {
        let lba = (rng.next() % span) * 4096 / bs;
        let s = time::Instant::now();
        dev.read(lba, &mut buf).map_err(|e| e.as_str())?;
        lat.push(s.elapsed().as_nanos() as u64);
    }
    let total = t0.elapsed().as_nanos() as u64;
    lat.sort_unstable();
    let mean = lat.iter().sum::<u64>() / n as u64;
    report(format!(
        "test=qd1 ops={} iops={} lat_us={} p50_us={} p99_us={}",
        n,
        n as u64 * 1_000_000_000 / total.max(1),
        us(mean),
        us(lat[n / 2]),
        us(lat[n * 99 / 100])
    ));
    Ok(())
}

fn seq(dev: &BlockDevice, mib: usize) -> Result<(), &'static str> {
    let bs = dev.block_size();
    let chunk = 1 << 20;
    let total = (mib << 20).min(dev.capacity as usize * bs / chunk * chunk);
    let mut buf = vec![0u8; chunk];
    let t0 = time::Instant::now();
    let mut off = 0;
    while off < total {
        dev.read((off / bs) as u64, &mut buf).map_err(|e| e.as_str())?;
        off += chunk;
    }
    let ns = t0.elapsed().as_nanos() as u64;
    report(format!("test=seq mib={} mbps={}", total >> 20, (total as u64 * 1000 / ns.max(1)) as u64));
    Ok(())
}

fn qd32(dev: &BlockDevice, batches: usize) -> Result<(), &'static str> {
    const QD: usize = 32;
    let bs = dev.block_size() as u64;
    let span = (dev.capacity * bs / 4096).max(1);
    let buf = vec![0u8; QD * 4096];
    let segs: Vec<Vec<Segment>> = (0..QD)
        .map(|i| blk::segments_of(buf.as_ptr() as usize + i * 4096, 4096))
        .collect::<Result<_, _>>()
        .map_err(|e| e.as_str())?;
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let t0 = time::Instant::now();
    for _ in 0..batches {
        let rqs: Vec<Request> = segs
            .iter()
            .map(|s| Request { op: Op::Read, unit: dev.unit, lba: (rng.next() % span) * 4096 / bs, blocks: (4096 / bs) as u32, segs: s, fua: false })
            .collect();
        dev.submit_batch(&rqs).map_err(|e| e.as_str())?;
    }
    let ns = t0.elapsed().as_nanos() as u64;
    report(format!("test=qd32 ops={} iops={}", batches * QD, (batches * QD) as u64 * 1_000_000_000 / ns.max(1)));
    Ok(())
}

struct Shared {
    dev: Arc<BlockDevice>,
    ops: AtomicU64,
    failed: AtomicBool,
    finished: AtomicU64,
    per: [AtomicU64; 64],
    where_: [AtomicU64; 64],
}

fn scale_worker((sh, n, seed, idx): (Arc<Shared>, usize, u64, usize)) {
    let bs = sh.dev.block_size() as u64;
    let span = (sh.dev.capacity * bs / 4096).max(1);
    let mut buf = vec![0u8; 4096];
    let mut rng = Rng(seed | 1);
    sh.where_[idx % 64].store(1, Relaxed);
    for _ in 0..n {
        sh.where_[idx % 64].store(2, Relaxed);
        if sh.dev.read((rng.next() % span) * 4096 / bs, &mut buf).is_err() {
            sh.failed.store(true, Relaxed);
            break;
        }
        sh.where_[idx % 64].store(3, Relaxed);
        sh.ops.fetch_add(1, Relaxed);
        sh.per[idx % 64].fetch_add(1, Relaxed);
    }
    sh.where_[idx % 64].store(4, Relaxed);
    sh.finished.fetch_add(1, Relaxed);
}

fn scale(dev: &Arc<BlockDevice>, n: usize) -> Result<(), &'static str> {
    let cpus: Vec<cpu::CpuId> = cpu::cpus().collect();
    #[allow(clippy::declare_interior_mutable_const)]
    const Z: AtomicU64 = AtomicU64::new(0);
    let sh = Arc::new(Shared { dev: dev.clone(), ops: AtomicU64::new(0), failed: AtomicBool::new(false), finished: AtomicU64::new(0), per: [Z; 64], where_: [Z; 64] });
    let t0 = time::Instant::now();
    let mut tasks = Vec::new();
    for (i, c) in cpus.iter().enumerate() {
        tasks.push(
            spawn::new_task_builder(scale_worker, (sh.clone(), n, 0x1234_5678 * (i as u64 + 1), i))
                .name(format!("blkbench_{}", c.value()))
                .pin_on_cpu(*c)
                .spawn()?,
        );
    }
    // Attente surveillée : si une tâche n'avance plus, on montre où elle en est.
    let mut last = u64::MAX;
    let mut still = 0;
    while sh.finished.load(Relaxed) < cpus.len() as u64 {
        let w = time::Instant::now();
        while w.elapsed().as_millis() < 1000 && sh.finished.load(Relaxed) < cpus.len() as u64 {
            core::hint::spin_loop();
        }
        let ops = sh.ops.load(Relaxed);
        if ops == last {
            still += 1;
            if still == 3 {
                let per: Vec<(u64, u64)> = (0..cpus.len()).map(|i| (sh.per[i].load(Relaxed), sh.where_[i].load(Relaxed))).collect();
                report(format!("test=scale stuck ops={} per_task(ops,etape)={:?} finies={}", ops, per, sh.finished.load(Relaxed)));
                for part in blk::debug_state(dev).split("] ") {
                    report(format!("etat {}", part));
                }
                for part in task::scheduler::debug_cpus().split("] ") {
                    report(format!("sched {}", part));
                }
                for t in &tasks {
                    report(format!("tache {}", task::scheduler::debug_task(t)));
                }
                return Err("blkbench : montée en charge bloquée");
            }
        } else {
            still = 0;
        }
        last = ops;
    }
    for t in tasks {
        t.join()?;
    }
    let ns = t0.elapsed().as_nanos() as u64;
    if sh.failed.load(Relaxed) {
        return Err("blkbench : erreur d'E/S pendant la montée en charge");
    }
    let ops = sh.ops.load(Relaxed);
    report(format!("test=scale cpus={} queues={} ops={} iops={}", cpus.len(), dev.nr_hw_queues(), ops, ops * 1_000_000_000 / ns.max(1)));
    Ok(())
}

fn verify(dev: &BlockDevice) -> Result<(), &'static str> {
    let bs = dev.block_size();
    let region = 16usize << 20;
    if (dev.capacity as usize) * bs < 2 * region {
        return Err("blkbench : unité trop petite pour la vérification");
    }
    let base = dev.capacity - (region / bs) as u64;
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    // (taille en blocs, décalage dans le tampon) : aligné, à cheval sur des pages (liste PRP),
    // gros (découpé), et mal aligné (rebond).
    let cases: [(usize, usize); 7] = [(1, 0), (8, 0), (9, 512), (64, 4096 - 512), (512, 0), (3000, 64), (5, 3)];
    let mut at = 0u64;
    let mut checked = 0usize;
    for round in 0..4 {
        for &(blocks_512, shift) in &cases {
            let len = (blocks_512 * 512).div_ceil(bs) * bs;
            let mut wbuf = vec![0u8; len + shift];
            for b in wbuf.iter_mut() {
                *b = rng.next() as u8;
            }
            let lba = base + at;
            dev.write(lba, &wbuf[shift..shift + len]).map_err(|e| e.as_str())?;
            let mut rbuf = vec![0u8; len + shift + 1];
            dev.read(lba, &mut rbuf[shift + 1..shift + 1 + len]).map_err(|e| e.as_str())?;
            if rbuf[shift + 1..shift + 1 + len] != wbuf[shift..shift + len] {
                report(format!("test=verify round={} blocks={} shift={} mismatch=1", round, len / bs, shift));
                return Err("blkbench : données relues différentes");
            }
            checked += len;
            at = (at + (len / bs) as u64 + 1) % (region / bs / 2) as u64;
        }
    }
    dev.flush().map_err(|e| e.as_str())?;
    report(format!("test=verify bytes={} ok=1", checked));
    Ok(())
}

fn run(args: &[String]) -> Result<(), &'static str> {
    let mut name: Option<String> = None;
    let mut n = 4000usize;
    let mut write = false;
    let mut only: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-n" => n = it.next().and_then(|v| v.parse().ok()).ok_or("blkbench : -n attend un nombre")?,
            "--write" => write = true,
            "--only" => only = it.next().cloned(),
            "--poll" => blk::set_hybrid_poll_max_ns(it.next().and_then(|v| v.parse().ok()).ok_or("blkbench : --poll attend des ns")?),
            "-h" | "--help" => {
                println!("blkbench [unite] [-n N] [--write] [--only qd1|seq|qd32|scale|verify] [--poll NS]");
                return Ok(());
            }
            other => name = Some(String::from(other)),
        }
    }
    let devs = blk::devices();
    if devs.is_empty() {
        return Err("blkbench : aucune unité MaiSS (lancer QEMU avec -device nvme)");
    }
    let dev = match name {
        Some(n) => blk::find(&n).ok_or("blkbench : unité inconnue")?,
        None => devs[0].clone(),
    };
    report(format!("dev={} blocks={} bs={} queues={} poll_max_ns={}", dev.name, dev.capacity, dev.block_size(), dev.nr_hw_queues(), blk::hybrid_poll_max_ns()));
    let want = |t: &str| only.as_deref().map_or(true, |o| o == t);
    if write && want("verify") {
        verify(&dev)?;
    }
    if want("qd1") {
        qd1(&dev, n)?;
    }
    if want("seq") {
        seq(&dev, 64)?;
    }
    if want("qd32") {
        qd32(&dev, (n / 32).max(1))?;
    }
    if want("scale") {
        scale(&dev, n / 2)?;
    }
    report(format!("stats {}", blk::describe(&dev)));
    report(format!("watchdog_rescues={}", blk::watchdog_rescues()));
    Ok(())
}

pub fn main(args: Vec<String>) -> isize {
    match run(&args) {
        Ok(()) => {
            report(String::from("done"));
            0
        }
        Err(e) => {
            report(format!("error={}", e));
            1
        }
    }
}
