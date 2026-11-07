//! Appels système de fichiers : VFS de MaiOS, terminal, `/dev` et `/proc` produits à la volée.

use super::errno::*;
use super::{fault, user_path, SysResult};
use crate::fd::{Node, OpenFile, O_ACCMODE, O_APPEND, O_NONBLOCK, O_RDWR, O_WRONLY};
use crate::Thread;
use alloc::{format, string::String, sync::Arc, vec, vec::Vec};
use core::sync::atomic::Ordering;
use fs_node::{DirRef, FileOrDir};

pub const AT_FDCWD: i64 = -100;
pub const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
pub const AT_REMOVEDIR: u64 = 0x200;
pub const AT_EMPTY_PATH: u64 = 0x1000;

const O_CREAT: u64 = 0x40;
const O_EXCL: u64 = 0x80;
const O_TRUNC: u64 = 0x200;
const O_DIRECTORY: u64 = 0x10000;
const O_CLOEXEC: u64 = 0x80000;
const O_PATH: u64 = 0x20_0000;

const S_IFCHR: u32 = 0o020000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;

/// Morceau des copies noyau ↔ utilisateur.
const CHUNK: usize = 64 * 1024;

// --- Résolution des chemins ------------------------------------------------------------------

/// Normalise lexicalement `path` relatif à `base` (absolu) : pas de lien symbolique dans le VFS,
/// `..` remonte donc simplement d'un composant.
fn normalize(base: &str, path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let prefix = if path.starts_with('/') { None } else { Some(base) };
    for comp in prefix.into_iter().chain(core::iter::once(path)).flat_map(|s| s.split('/')) {
        match comp {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    let mut out = String::from("/");
    out.push_str(&parts.join("/"));
    out
}

fn dir_path_of(t: &Thread, dirfd: i64) -> Result<String, i64> {
    if dirfd == AT_FDCWD {
        return Ok(t.process.cwd.lock().1.clone());
    }
    let f = get_fd(t, dirfd as u64)?;
    match &f.node {
        Node::Dir { path, .. } => Ok(path.clone()),
        _ => Err(ENOTDIR),
    }
}

/// Chemin absolu normalisé de `path` vu depuis `dirfd`.
fn absolute(t: &Thread, dirfd: i64, path: &str) -> Result<String, i64> {
    if path.is_empty() {
        return Err(ENOENT);
    }
    if path.starts_with('/') {
        return Ok(normalize("/", path));
    }
    Ok(normalize(&dir_path_of(t, dirfd)?, path))
}

fn lookup(abs: &str) -> Option<FileOrDir> {
    let p: &path::Path = abs.as_ref();
    p.get(root::get_root())
}

/// Parent et nom final de `abs`.
fn split_parent(abs: &str) -> Result<(DirRef, String), i64> {
    let (dir, name) = abs.rsplit_once('/').ok_or(EINVAL)?;
    if name.is_empty() {
        return Err(EEXIST); // la racine
    }
    let dir = if dir.is_empty() { "/" } else { dir };
    match lookup(dir) {
        Some(FileOrDir::Dir(d)) => Ok((d, String::from(name))),
        Some(FileOrDir::File(_)) => Err(ENOTDIR),
        None => Err(ENOENT),
    }
}

pub(crate) fn get_fd(t: &Thread, fd: u64) -> Result<Arc<OpenFile>, i64> {
    if fd > i32::MAX as u64 {
        return Err(EBADF);
    }
    t.process.files.lock().get(fd as usize).ok_or(EBADF)
}

// --- /dev et /proc ---------------------------------------------------------------------------

enum Special {
    Node(Node),
    Dup(usize),
}

fn console_of(t: &Thread) -> Option<Node> {
    let files = t.process.files.lock();
    let input = files.get(0).and_then(|f| match &f.node {
        Node::Console { input, .. } => input.clone(),
        _ => None,
    });
    let (output, discipline) = files
        .get(1)
        .and_then(|f| match &f.node {
            Node::Console { output, discipline, .. } => Some((output.clone(), discipline.clone())),
            _ => None,
        })
        .unwrap_or((None, None));
    if input.is_none() && output.is_none() {
        return None;
    }
    Some(Node::Console { input, output, discipline })
}

fn proc_maps(t: &Thread) -> Vec<u8> {
    let mut out = String::new();
    uvm::for_each_vma(&t.process.mm, |v| {
        let r = if v.prot.read() { 'r' } else { '-' };
        let w = if v.prot.write() { 'w' } else { '-' };
        let x = if v.prot.exec() { 'x' } else { '-' };
        let name = match v.name {
            "exe" => t.process.exe.as_str(),
            n => n,
        };
        let off = match &v.backing {
            uvm::Backing::File { offset, .. } => *offset,
            uvm::Backing::Anon => 0,
        };
        out.push_str(&format!("{:012x}-{:012x} {}{}{}p {:08x} 00:00 0          {}\n", v.start, v.end, r, w, x, off, name));
    });
    out.into_bytes()
}

fn proc_meminfo() -> Vec<u8> {
    let s = frame_allocator::stats();
    let kb = |frames: usize| frames * 4;
    format!(
        "MemTotal:       {:8} kB\nMemFree:        {:8} kB\nMemAvailable:   {:8} kB\nBuffers:               0 kB\nCached:                0 kB\nSwapTotal:             0 kB\nSwapFree:              0 kB\n",
        kb(s.total_frames), kb(s.free_frames), kb(s.free_frames)
    )
    .into_bytes()
}

fn proc_cpuinfo() -> Vec<u8> {
    let mut brand = [0u8; 48];
    // SAFETY : CPUID étendu (marque) disponible sur tout x86_64.
    unsafe {
        for (i, leaf) in (0x8000_0002u32..=0x8000_0004).enumerate() {
            let r = core::arch::x86_64::__cpuid(leaf);
            for (j, w) in [r.eax, r.ebx, r.ecx, r.edx].iter().enumerate() {
                brand[i * 16 + j * 4..i * 16 + j * 4 + 4].copy_from_slice(&w.to_le_bytes());
            }
        }
    }
    let end = brand.iter().position(|&b| b == 0).unwrap_or(48);
    let name = core::str::from_utf8(&brand[..end]).unwrap_or("x86_64").trim();
    let mut out = String::new();
    for cpu in 0..cpu::cpu_count() {
        out.push_str(&format!(
            "processor\t: {}\nvendor_id\t: x86_64\nmodel name\t: {}\nflags\t\t: fpu sse sse2 fsgsbase\n\n",
            cpu, name
        ));
    }
    out.into_bytes()
}

fn proc_uptime() -> Vec<u8> {
    let d = time::Instant::now().duration_since(time::Instant::ZERO);
    format!("{}.{:02} 0.00\n", d.as_secs(), d.subsec_millis() / 10).into_bytes()
}

fn special(t: &Thread, abs: &str) -> Option<Special> {
    let gen = |data: Vec<u8>| Some(Special::Node(Node::Generated { data: Arc::from(data.into_boxed_slice()), path: String::from(abs) }));
    match abs {
        "/dev/null" => Some(Special::Node(Node::Null)),
        "/dev/zero" => Some(Special::Node(Node::Zero)),
        "/dev/random" | "/dev/urandom" => Some(Special::Node(Node::Random)),
        "/dev/tty" | "/dev/console" => console_of(t).map(Special::Node),
        "/dev/stdin" => Some(Special::Dup(0)),
        "/dev/stdout" => Some(Special::Dup(1)),
        "/dev/stderr" => Some(Special::Dup(2)),
        "/proc/self/maps" => gen(proc_maps(t)),
        "/proc/meminfo" => gen(proc_meminfo()),
        "/proc/cpuinfo" => gen(proc_cpuinfo()),
        "/proc/uptime" => gen(proc_uptime()),
        "/proc/version" => gen(format!("Linux version {} (MaiOS) #1 SMP\n", super::sys::RELEASE).into_bytes()),
        "/proc/self/comm" => {
            let c = *t.process.comm.lock();
            let end = c.iter().position(|&b| b == 0).unwrap_or(16);
            let mut v = c[..end].to_vec();
            v.push(b'\n');
            gen(v)
        }
        "/proc/mounts" | "/etc/mtab" => gen(b"rootfs / rootfs rw 0 0\n".to_vec()),
        _ => {
            let rest = abs.strip_prefix("/dev/fd/").or_else(|| abs.strip_prefix("/proc/self/fd/"))?;
            rest.parse::<usize>().ok().map(Special::Dup)
        }
    }
}

// --- Informations de fichier -------------------------------------------------------------------

struct StatInfo {
    ino: u64,
    mode: u32,
    nlink: u64,
    rdev: u64,
    size: u64,
}

fn ino_of(path: &str) -> u64 {
    // FNV-1a : stable pour un même chemin.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h | 1
}

const fn makedev(major: u64, minor: u64) -> u64 { (major << 8) | minor }

fn stat_node(node: &Node) -> StatInfo {
    match node {
        Node::Console { .. } => StatInfo { ino: 3, mode: S_IFCHR | 0o620, nlink: 1, rdev: makedev(136, 0), size: 0 },
        Node::Null => StatInfo { ino: 4, mode: S_IFCHR | 0o666, nlink: 1, rdev: makedev(1, 3), size: 0 },
        Node::Zero => StatInfo { ino: 5, mode: S_IFCHR | 0o666, nlink: 1, rdev: makedev(1, 5), size: 0 },
        Node::Random => StatInfo { ino: 6, mode: S_IFCHR | 0o666, nlink: 1, rdev: makedev(1, 9), size: 0 },
        Node::Generated { data, path } => StatInfo { ino: ino_of(path), mode: S_IFREG | 0o444, nlink: 1, rdev: 0, size: data.len() as u64 },
        Node::File { file, path } => StatInfo { ino: ino_of(path), mode: S_IFREG | 0o755, nlink: 1, rdev: 0, size: file.lock().len() as u64 },
        Node::Dir { path, .. } => StatInfo { ino: ino_of(path), mode: S_IFDIR | 0o755, nlink: 2, rdev: 0, size: 4096 },
    }
}

fn stat_fs(node: &FileOrDir, abs: &str) -> StatInfo {
    match node {
        FileOrDir::File(f) => StatInfo { ino: ino_of(abs), mode: S_IFREG | 0o755, nlink: 1, rdev: 0, size: f.lock().len() as u64 },
        FileOrDir::Dir(_) => StatInfo { ino: ino_of(abs), mode: S_IFDIR | 0o755, nlink: 2, rdev: 0, size: 4096 },
    }
}

fn write_stat(t: &Thread, buf: u64, s: &StatInfo) -> SysResult {
    let mut st = [0u8; 144];
    let put = |b: &mut [u8; 144], off: usize, v: u64, n: usize| b[off..off + n].copy_from_slice(&v.to_le_bytes()[..n]);
    let now = super::sys::realtime();
    put(&mut st, 0, 1, 8); // st_dev
    put(&mut st, 8, s.ino, 8);
    put(&mut st, 16, s.nlink, 8);
    put(&mut st, 24, s.mode as u64, 4);
    put(&mut st, 28, 0, 4); // uid
    put(&mut st, 32, 0, 4); // gid
    put(&mut st, 40, s.rdev, 8);
    put(&mut st, 48, s.size, 8);
    put(&mut st, 56, 4096, 8); // st_blksize
    put(&mut st, 64, (s.size + 511) / 512, 8); // st_blocks
    for off in [72, 88, 104] {
        put(&mut st, off, now.0 as u64, 8);
        put(&mut st, off + 8, now.1 as u64, 8);
    }
    t.process.mm.copy_to_user(buf as usize, &st).map_err(fault)?;
    Ok(0)
}

fn write_statx(t: &Thread, buf: u64, s: &StatInfo) -> SysResult {
    let mut sx = [0u8; 256];
    let put = |b: &mut [u8; 256], off: usize, v: u64, n: usize| b[off..off + n].copy_from_slice(&v.to_le_bytes()[..n]);
    let now = super::sys::realtime();
    put(&mut sx, 0, 0x7ff, 4); // stx_mask : STATX_BASIC_STATS
    put(&mut sx, 4, 4096, 4);
    put(&mut sx, 16, s.nlink, 4);
    put(&mut sx, 28, s.mode as u64, 2);
    put(&mut sx, 32, s.ino, 8);
    put(&mut sx, 40, s.size, 8);
    put(&mut sx, 48, (s.size + 511) / 512, 8);
    for off in [64, 80, 96, 112] {
        put(&mut sx, off, now.0 as u64, 8);
        put(&mut sx, off + 8, now.1 as u64, 4);
    }
    put(&mut sx, 128, s.rdev >> 8, 4);
    put(&mut sx, 132, s.rdev & 0xff, 4);
    put(&mut sx, 136, 0, 4);
    put(&mut sx, 140, 1, 4);
    t.process.mm.copy_to_user(buf as usize, &sx).map_err(fault)?;
    Ok(0)
}

/// Informations sur `path` (ou sur `dirfd` avec `AT_EMPTY_PATH`).
fn stat_at(t: &Thread, dirfd: i64, path_ptr: u64, flags: u64) -> Result<StatInfo, i64> {
    let path = user_path(t, path_ptr)?;
    if path.is_empty() {
        if flags & AT_EMPTY_PATH == 0 {
            return Err(ENOENT);
        }
        if dirfd == AT_FDCWD {
            let (_, p) = t.process.cwd.lock().clone();
            return Ok(StatInfo { ino: ino_of(&p), mode: S_IFDIR | 0o755, nlink: 2, rdev: 0, size: 4096 });
        }
        return Ok(stat_node(&get_fd(t, dirfd as u64)?.node));
    }
    let abs = absolute(t, dirfd, &path)?;
    match special(t, &abs) {
        Some(Special::Node(n)) => return Ok(stat_node(&n)),
        Some(Special::Dup(fd)) => return Ok(stat_node(&get_fd(t, fd as u64)?.node)),
        None => {}
    }
    lookup(&abs).map(|n| stat_fs(&n, &abs)).ok_or(ENOENT)
}

pub fn fstat(t: &Thread, fd: u64, buf: u64) -> SysResult {
    let f = get_fd(t, fd)?;
    write_stat(t, buf, &stat_node(&f.node))
}

pub fn fstatat(t: &Thread, dirfd: i64, path: u64, buf: u64, flags: u64) -> SysResult {
    let s = stat_at(t, dirfd, path, flags)?;
    write_stat(t, buf, &s)
}

pub fn statx(t: &Thread, dirfd: i64, path: u64, flags: u64, _mask: u64, buf: u64) -> SysResult {
    let s = stat_at(t, dirfd, path, flags)?;
    write_statx(t, buf, &s)
}

pub fn statfs(t: &Thread, buf: u64) -> SysResult {
    let s = frame_allocator::stats();
    let mut b = [0u8; 120];
    let put = |b: &mut [u8; 120], off: usize, v: u64| b[off..off + 8].copy_from_slice(&v.to_le_bytes());
    put(&mut b, 0, 0x0102_1994); // TMPFS_MAGIC
    put(&mut b, 8, 4096);
    put(&mut b, 16, s.total_frames as u64);
    put(&mut b, 24, s.free_frames as u64);
    put(&mut b, 32, s.free_frames as u64);
    put(&mut b, 40, 1 << 20);
    put(&mut b, 48, 1 << 20);
    put(&mut b, 64, 255);
    put(&mut b, 72, 4096);
    t.process.mm.copy_to_user(buf as usize, &b).map_err(fault)?;
    Ok(0)
}

// --- Ouverture, fermeture, duplication -------------------------------------------------------

pub fn openat(t: &Thread, dirfd: i64, path_ptr: u64, flags: u64, _mode: u64) -> SysResult {
    let path = user_path(t, path_ptr)?;
    let abs = absolute(t, dirfd, &path)?;
    let cloexec = flags & O_CLOEXEC != 0;
    let status = (flags as u32) & (O_ACCMODE | O_APPEND | O_NONBLOCK);
    let acc = status & O_ACCMODE;
    let wants_write = acc == O_WRONLY || acc == O_RDWR;

    let file = match special(t, &abs) {
        Some(Special::Node(node)) => {
            if matches!(node, Node::Generated { .. }) && wants_write {
                return Err(EACCES);
            }
            OpenFile::new(node, status)
        }
        Some(Special::Dup(fd)) => get_fd(t, fd as u64)?,
        None => {
            let node = match lookup(&abs) {
                Some(n) => {
                    if flags & O_CREAT != 0 && flags & O_EXCL != 0 {
                        return Err(EEXIST);
                    }
                    n
                }
                None => {
                    if flags & O_CREAT == 0 {
                        return Err(ENOENT);
                    }
                    let (parent, name) = split_parent(&abs)?;
                    let f = memfs::MemFile::create(name, &parent).map_err(|_| ENOSPC)?;
                    FileOrDir::File(f)
                }
            };
            match node {
                FileOrDir::Dir(dir) => {
                    if wants_write {
                        return Err(EISDIR);
                    }
                    OpenFile::new(Node::Dir { dir, path: abs }, status)
                }
                FileOrDir::File(file) => {
                    if flags & O_DIRECTORY != 0 {
                        return Err(ENOTDIR);
                    }
                    if flags & O_TRUNC != 0 && wants_write && flags & O_PATH == 0 {
                        file.lock().set_len(0).map_err(|_| EROFS)?;
                    }
                    OpenFile::new(Node::File { file, path: abs }, status)
                }
            }
        }
    };
    t.process.files.lock().alloc(file, 0, cloexec).map(|fd| fd as i64).ok_or(EMFILE)
}

pub fn close(t: &Thread, fd: u64) -> SysResult {
    if fd > i32::MAX as u64 || !t.process.files.lock().close(fd as usize) {
        return Err(EBADF);
    }
    Ok(0)
}

pub fn dup(t: &Thread, fd: u64) -> SysResult {
    let f = get_fd(t, fd)?;
    t.process.files.lock().alloc(f, 0, false).map(|n| n as i64).ok_or(EMFILE)
}

/// `dup2` (`legacy`) ou `dup3`.
pub fn dup3(t: &Thread, old: u64, new: u64, flags: u64, legacy: bool) -> SysResult {
    let f = get_fd(t, old)?;
    if new >= crate::fd::NOFILE as u64 {
        return Err(EBADF);
    }
    if old == new {
        return if legacy { Ok(new as i64) } else { Err(EINVAL) };
    }
    if flags & !O_CLOEXEC != 0 {
        return Err(EINVAL);
    }
    t.process.files.lock().install(new as usize, f, flags & O_CLOEXEC != 0);
    Ok(new as i64)
}

pub fn fcntl(t: &Thread, fd: u64, cmd: u64, arg: u64) -> SysResult {
    const F_DUPFD: u64 = 0;
    const F_GETFD: u64 = 1;
    const F_SETFD: u64 = 2;
    const F_GETFL: u64 = 3;
    const F_SETFL: u64 = 4;
    const F_GETLK: u64 = 5;
    const F_SETLK: u64 = 6;
    const F_SETLKW: u64 = 7;
    const F_SETOWN: u64 = 8;
    const F_GETOWN: u64 = 9;
    const F_DUPFD_CLOEXEC: u64 = 1030;
    let f = get_fd(t, fd)?;
    match cmd {
        F_DUPFD | F_DUPFD_CLOEXEC => {
            if arg >= crate::fd::NOFILE as u64 {
                return Err(EINVAL);
            }
            t.process.files.lock().alloc(f, arg as usize, cmd == F_DUPFD_CLOEXEC).map(|n| n as i64).ok_or(EMFILE)
        }
        F_GETFD => Ok(t.process.files.lock().cloexec(fd as usize).map_or(0, |c| c as i64)),
        F_SETFD => {
            t.process.files.lock().set_cloexec(fd as usize, arg & 1 != 0);
            Ok(0)
        }
        F_GETFL => Ok(f.flags() as i64),
        F_SETFL => {
            let keep = f.flags() & O_ACCMODE;
            f.flags.store(keep | (arg as u32 & (O_APPEND | O_NONBLOCK)), Ordering::Relaxed);
            Ok(0)
        }
        F_GETLK => {
            // Aucun verrou en conflit : l_type = F_UNLCK.
            t.process.mm.write_user::<i16>(arg as usize, &2).map_err(fault)?;
            Ok(0)
        }
        F_SETLK | F_SETLKW | F_SETOWN => Ok(0),
        F_GETOWN => Ok(0),
        _ => Err(EINVAL),
    }
}

// --- Lecture et écriture -------------------------------------------------------------------

/// Lit dans un tampon noyau ; `at` : position explicite (`pread`), sinon celle du fichier.
fn kread(f: &OpenFile, kb: &mut [u8], at: Option<usize>) -> Result<usize, i64> {
    if !f.readable() {
        return Err(EBADF);
    }
    let read_bytes = |data: &[u8], kb: &mut [u8], off: usize| -> usize {
        if off >= data.len() {
            return 0;
        }
        let n = kb.len().min(data.len() - off);
        kb[..n].copy_from_slice(&data[off..off + n]);
        n
    };
    match &f.node {
        Node::Console { input: Some(r), .. } => {
            if at.is_some() {
                return Err(ESPIPE);
            }
            r.read(kb).map_err(|_| EIO)
        }
        Node::Console { input: None, .. } => Err(EBADF),
        Node::Null => Ok(0),
        Node::Zero => {
            kb.fill(0);
            Ok(kb.len())
        }
        Node::Random => {
            for c in kb.chunks_mut(8) {
                let v = uvm::rand::random_u64().to_le_bytes();
                c.copy_from_slice(&v[..c.len()]);
            }
            Ok(kb.len())
        }
        Node::Dir { .. } => Err(EISDIR),
        Node::Generated { data, .. } => {
            let off = at.unwrap_or_else(|| *f.pos.lock());
            let n = read_bytes(data, kb, off);
            if at.is_none() {
                *f.pos.lock() = off + n;
            }
            Ok(n)
        }
        Node::File { file, .. } => {
            let off = at.unwrap_or_else(|| *f.pos.lock());
            let n = {
                let mut locked = file.lock();
                let len = locked.len();
                if off >= len {
                    0
                } else {
                    let want = kb.len().min(len - off);
                    locked.read_at(&mut kb[..want], off).map_err(|_| EIO)?
                }
            };
            if at.is_none() {
                *f.pos.lock() = off + n;
            }
            Ok(n)
        }
    }
}

/// Écrit un tampon noyau.
fn kwrite(t: &Thread, f: &OpenFile, kb: &[u8], at: Option<usize>) -> Result<usize, i64> {
    if !f.writable() {
        return Err(EBADF);
    }
    match &f.node {
        Node::Console { output: Some(w), .. } => {
            if at.is_some() {
                return Err(ESPIPE);
            }
            w.write_all(kb).map_err(|_| EIO)?;
            if t.process.log_output {
                t.process.log_bytes(kb);
            }
            Ok(kb.len())
        }
        Node::Console { output: None, .. } => Err(EBADF),
        Node::Null | Node::Zero | Node::Random => Ok(kb.len()),
        Node::Dir { .. } => Err(EISDIR),
        Node::Generated { .. } => Err(EBADF),
        Node::File { file, .. } => {
            let mut locked = file.lock();
            let off = match at {
                Some(o) => o,
                None if f.flags() & O_APPEND != 0 => locked.len(),
                None => *f.pos.lock(),
            };
            let n = locked.write_at(kb, off).map_err(|_| ENOSPC)?;
            drop(locked);
            if at.is_none() {
                *f.pos.lock() = off + n;
            }
            Ok(n)
        }
    }
}

/// Une lecture de terminal rend la main dès qu'une ligne (ou un octet en mode brut) est là.
fn is_stream(f: &OpenFile) -> bool { matches!(f.node, Node::Console { .. }) }

fn do_read(t: &Thread, f: &OpenFile, buf: u64, count: usize, at: Option<usize>) -> SysResult {
    if count == 0 {
        return Ok(0);
    }
    let mut kb = vec![0u8; count.min(CHUNK)];
    let mut done = 0usize;
    while done < count {
        let want = (count - done).min(kb.len());
        let n = kread(f, &mut kb[..want], at.map(|o| o + done))?;
        if n == 0 {
            break;
        }
        t.process.mm.copy_to_user(buf as usize + done, &kb[..n]).map_err(fault)?;
        done += n;
        if n < want || is_stream(f) {
            break;
        }
    }
    Ok(done as i64)
}

fn do_write(t: &Thread, f: &OpenFile, buf: u64, count: usize, at: Option<usize>) -> SysResult {
    if count == 0 {
        return Ok(0);
    }
    let mut kb = vec![0u8; count.min(CHUNK)];
    let mut done = 0usize;
    while done < count {
        let want = (count - done).min(kb.len());
        if let Err(e) = t.process.mm.copy_from_user(&mut kb[..want], buf as usize + done) {
            if done > 0 {
                break;
            }
            return Err(fault(e));
        }
        let n = kwrite(t, f, &kb[..want], at.map(|o| o + done))?;
        done += n;
        if n < want {
            break;
        }
    }
    Ok(done as i64)
}

pub fn read(t: &Thread, fd: u64, buf: u64, count: u64) -> SysResult {
    let f = get_fd(t, fd)?;
    do_read(t, &f, buf, count as usize, None)
}

pub fn write(t: &Thread, fd: u64, buf: u64, count: u64) -> SysResult {
    let f = get_fd(t, fd)?;
    do_write(t, &f, buf, count as usize, None)
}

pub fn pread(t: &Thread, fd: u64, buf: u64, count: u64, pos: i64) -> SysResult {
    if pos < 0 {
        return Err(EINVAL);
    }
    let f = get_fd(t, fd)?;
    do_read(t, &f, buf, count as usize, Some(pos as usize))
}

pub fn pwrite(t: &Thread, fd: u64, buf: u64, count: u64, pos: i64) -> SysResult {
    if pos < 0 {
        return Err(EINVAL);
    }
    let f = get_fd(t, fd)?;
    do_write(t, &f, buf, count as usize, Some(pos as usize))
}

fn iovecs(t: &Thread, iov: u64, cnt: u64) -> Result<Vec<(u64, u64)>, i64> {
    if cnt > 1024 {
        return Err(EINVAL);
    }
    let mut raw = vec![0u8; cnt as usize * 16];
    t.process.mm.copy_from_user(&mut raw, iov as usize).map_err(fault)?;
    Ok(raw
        .chunks_exact(16)
        .map(|c| (u64::from_le_bytes(c[..8].try_into().unwrap()), u64::from_le_bytes(c[8..].try_into().unwrap())))
        .collect())
}

pub fn readv(t: &Thread, fd: u64, iov: u64, cnt: u64, pos: Option<i64>) -> SysResult {
    let f = get_fd(t, fd)?;
    let mut total = 0usize;
    for (base, len) in iovecs(t, iov, cnt)? {
        if len == 0 {
            continue;
        }
        let at = pos.map(|p| p as usize + total);
        let n = match do_read(t, &f, base, len as usize, at) {
            Ok(n) => n as usize,
            Err(e) if total == 0 => return Err(e),
            Err(_) => break,
        };
        total += n;
        if n < len as usize || is_stream(&f) {
            break;
        }
    }
    Ok(total as i64)
}

pub fn writev(t: &Thread, fd: u64, iov: u64, cnt: u64, pos: Option<i64>) -> SysResult {
    let f = get_fd(t, fd)?;
    let mut total = 0usize;
    for (base, len) in iovecs(t, iov, cnt)? {
        if len == 0 {
            continue;
        }
        let at = pos.map(|p| p as usize + total);
        let n = match do_write(t, &f, base, len as usize, at) {
            Ok(n) => n as usize,
            Err(e) if total == 0 => return Err(e),
            Err(_) => break,
        };
        total += n;
        if n < len as usize {
            break;
        }
    }
    Ok(total as i64)
}

pub fn sendfile(t: &Thread, out_fd: u64, in_fd: u64, off_ptr: u64, count: u64) -> SysResult {
    let out = get_fd(t, out_fd)?;
    let inp = get_fd(t, in_fd)?;
    let mut pos = if off_ptr != 0 {
        Some(t.process.mm.read_user::<i64>(off_ptr as usize).map_err(fault)? as usize)
    } else {
        None
    };
    let mut kb = vec![0u8; (count as usize).min(CHUNK)];
    let mut done = 0usize;
    while done < count as usize {
        let want = (count as usize - done).min(kb.len());
        let n = kread(&inp, &mut kb[..want], pos)?;
        if n == 0 {
            break;
        }
        let w = kwrite(t, &out, &kb[..n], None)?;
        done += w;
        if let Some(p) = pos.as_mut() {
            *p += w;
        }
        if w < n || n < want {
            break;
        }
    }
    if let Some(p) = pos {
        t.process.mm.write_user::<i64>(off_ptr as usize, &(p as i64)).map_err(fault)?;
    }
    Ok(done as i64)
}

pub fn lseek(t: &Thread, fd: u64, off: i64, whence: u64) -> SysResult {
    let f = get_fd(t, fd)?;
    let len = match &f.node {
        Node::Console { .. } => return Err(ESPIPE),
        Node::Null | Node::Zero | Node::Random => return Ok(0),
        Node::File { file, .. } => file.lock().len() as i64,
        Node::Generated { data, .. } => data.len() as i64,
        Node::Dir { .. } => 0,
    };
    let mut pos = f.pos.lock();
    let new = match whence {
        0 => off,
        1 => *pos as i64 + off,
        2 => len + off,
        3 => if off < len { off } else { return Err(ENXIO) }, // SEEK_DATA
        4 => if off < len { len } else { return Err(ENXIO) }, // SEEK_HOLE
        _ => return Err(EINVAL),
    };
    if new < 0 {
        return Err(EINVAL);
    }
    *pos = new as usize;
    Ok(new)
}

pub fn ftruncate(t: &Thread, fd: u64, len: i64) -> SysResult {
    if len < 0 {
        return Err(EINVAL);
    }
    let f = get_fd(t, fd)?;
    match &f.node {
        Node::File { file, .. } if f.writable() => file.lock().set_len(len as usize).map(|_| 0).map_err(|_| EROFS),
        Node::Dir { .. } => Err(EISDIR),
        _ => Err(EINVAL),
    }
}

pub fn truncate_path(t: &Thread, path: u64, len: i64) -> SysResult {
    if len < 0 {
        return Err(EINVAL);
    }
    let abs = absolute(t, AT_FDCWD, &user_path(t, path)?)?;
    match lookup(&abs) {
        Some(FileOrDir::File(f)) => f.lock().set_len(len as usize).map(|_| 0).map_err(|_| EROFS),
        Some(FileOrDir::Dir(_)) => Err(EISDIR),
        None => Err(ENOENT),
    }
}

pub fn fsync(t: &Thread, fd: u64) -> SysResult {
    get_fd(t, fd).map(|_| 0)
}

// --- Terminal -------------------------------------------------------------------------------

pub fn ioctl(t: &Thread, fd: u64, req: u64, arg: u64) -> SysResult {
    const TCGETS: u64 = 0x5401;
    const TCSETS: u64 = 0x5402;
    const TCSETSW: u64 = 0x5403;
    const TCSETSF: u64 = 0x5404;
    const TIOCSCTTY: u64 = 0x540E;
    const TIOCGPGRP: u64 = 0x540F;
    const TIOCSPGRP: u64 = 0x5410;
    const TIOCGWINSZ: u64 = 0x5413;
    const TIOCSWINSZ: u64 = 0x5414;
    const FIONREAD: u64 = 0x541B;
    const FIONBIO: u64 = 0x5421;
    const FIONCLEX: u64 = 0x5450;
    const FIOCLEX: u64 = 0x5451;
    const ISIG: u32 = 0x1;
    const ICANON: u32 = 0x2;
    const ECHO: u32 = 0x8;
    const ECHOE: u32 = 0x10;
    const ECHOK: u32 = 0x20;
    const ECHOCTL: u32 = 0x200;
    const ECHOKE: u32 = 0x800;
    const IEXTEN: u32 = 0x8000;

    let f = get_fd(t, fd)?;
    match req {
        FIOCLEX | FIONCLEX => {
            t.process.files.lock().set_cloexec(fd as usize, req == FIOCLEX);
            return Ok(0);
        }
        FIONBIO => {
            let on = t.process.mm.read_user::<i32>(arg as usize).map_err(fault)? != 0;
            let fl = f.flags();
            f.flags.store(if on { fl | O_NONBLOCK } else { fl & !O_NONBLOCK }, Ordering::Relaxed);
            return Ok(0);
        }
        _ => {}
    }
    let Node::Console { discipline, .. } = &f.node else {
        return Err(ENOTTY);
    };
    let mm = &t.process.mm;
    match req {
        TCGETS => {
            let (canon, echo) = discipline.as_ref().map_or((true, true), |d| (d.canonical(), d.echo()));
            let mut lflag = ISIG | IEXTEN | ECHOCTL | ECHOKE;
            if canon { lflag |= ICANON; }
            if echo { lflag |= ECHO | ECHOE | ECHOK; }
            let mut tio = [0u8; 36];
            tio[0..4].copy_from_slice(&0x500u32.to_le_bytes()); // ICRNL | IXON
            tio[4..8].copy_from_slice(&0x5u32.to_le_bytes()); // OPOST | ONLCR
            tio[8..12].copy_from_slice(&0xbfu32.to_le_bytes()); // B38400 | CS8 | CREAD
            tio[12..16].copy_from_slice(&lflag.to_le_bytes());
            let cc = &mut tio[17..36];
            cc[0] = 3; // VINTR
            cc[1] = 0x1c; // VQUIT
            cc[2] = 0x7f; // VERASE
            cc[3] = 0x15; // VKILL
            cc[4] = 4; // VEOF
            cc[5] = 0; // VTIME
            cc[6] = 1; // VMIN
            cc[8] = 0x11; // VSTART
            cc[9] = 0x13; // VSTOP
            cc[10] = 0x1a; // VSUSP
            cc[12] = 0x12; // VREPRINT
            cc[14] = 0x17; // VWERASE
            cc[15] = 0x16; // VLNEXT
            mm.copy_to_user(arg as usize, &tio).map_err(fault)?;
            Ok(0)
        }
        TCSETS | TCSETSW | TCSETSF => {
            let lflag = mm.read_user::<u32>(arg as usize + 12).map_err(fault)?;
            if let Some(d) = discipline {
                d.set_canonical(lflag & ICANON != 0);
                d.set_echo(lflag & ECHO != 0);
            }
            Ok(0)
        }
        TIOCGWINSZ => {
            let ws: [u16; 4] = [25, 80, 0, 0];
            mm.write_user(arg as usize, &ws).map_err(fault)?;
            Ok(0)
        }
        TIOCSWINSZ | TIOCSCTTY | TIOCSPGRP => Ok(0),
        TIOCGPGRP => {
            mm.write_user::<i32>(arg as usize, &(t.process.pid as i32)).map_err(fault)?;
            Ok(0)
        }
        FIONREAD => {
            mm.write_user::<i32>(arg as usize, &0).map_err(fault)?;
            Ok(0)
        }
        _ => Err(ENOTTY),
    }
}

/// `poll` : fichiers ordinaires toujours prêts ; terminal prêt en écriture, et en lecture (la
/// lecture bloque jusqu'à une ligne).
fn poll_ready(t: &Thread, fds: u64, nfds: u64) -> Result<(i64, Vec<u8>), i64> {
    const POLLIN: i16 = 0x1;
    const POLLOUT: i16 = 0x4;
    const POLLNVAL: i16 = 0x20;
    if nfds > crate::fd::NOFILE as u64 {
        return Err(EINVAL);
    }
    let mut raw = vec![0u8; nfds as usize * 8];
    t.process.mm.copy_from_user(&mut raw, fds as usize).map_err(fault)?;
    let mut ready = 0;
    for e in raw.chunks_exact_mut(8) {
        let fd = i32::from_le_bytes(e[0..4].try_into().unwrap());
        let events = i16::from_le_bytes(e[4..6].try_into().unwrap());
        let revents = if fd < 0 {
            0
        } else {
            match t.process.files.lock().get(fd as usize) {
                None => POLLNVAL,
                Some(f) => {
                    let mut r = 0;
                    if f.readable() { r |= POLLIN; }
                    if f.writable() { r |= POLLOUT; }
                    r & (events | POLLNVAL)
                }
            }
        };
        e[6..8].copy_from_slice(&revents.to_le_bytes());
        if revents != 0 {
            ready += 1;
        }
    }
    Ok((ready, raw))
}

pub fn poll(t: &Thread, fds: u64, nfds: u64, timeout_ms: i64) -> SysResult {
    let (ready, raw) = poll_ready(t, fds, nfds)?;
    if ready == 0 && timeout_ms > 0 {
        let _ = sleep::sleep(time::Duration::from_millis(timeout_ms as u64));
    }
    t.process.mm.copy_to_user(fds as usize, &raw).map_err(fault)?;
    Ok(ready)
}

pub fn ppoll(t: &Thread, fds: u64, nfds: u64, tmo: u64) -> SysResult {
    let timeout_ms = if tmo == 0 {
        -1
    } else {
        let ts = t.process.mm.read_user::<[i64; 2]>(tmo as usize).map_err(fault)?;
        ts[0].saturating_mul(1000).saturating_add(ts[1] / 1_000_000)
    };
    poll(t, fds, nfds, timeout_ms)
}

// --- Répertoires ----------------------------------------------------------------------------

pub fn getdents64(t: &Thread, fd: u64, dirp: u64, count: u64) -> SysResult {
    const DT_DIR: u8 = 4;
    const DT_REG: u8 = 8;
    let f = get_fd(t, fd)?;
    let Node::Dir { dir, path } = &f.node else {
        return Err(ENOTDIR);
    };
    let mut names: Vec<(String, u8)> = vec![(String::from("."), DT_DIR), (String::from(".."), DT_DIR)];
    {
        let locked = dir.lock();
        for name in locked.list() {
            let ty = match locked.get(&name) {
                Some(FileOrDir::Dir(_)) => DT_DIR,
                _ => DT_REG,
            };
            names.push((name, ty));
        }
    }
    let mut pos = f.pos.lock();
    let mut out: Vec<u8> = Vec::new();
    let mut i = *pos;
    while i < names.len() {
        let (name, ty) = &names[i];
        let reclen = (19 + name.len() + 1 + 7) & !7;
        if out.len() + reclen > count as usize {
            if out.is_empty() {
                return Err(EINVAL);
            }
            break;
        }
        let ino = ino_of(&format!("{}/{}", path, name));
        let start = out.len();
        out.extend_from_slice(&ino.to_le_bytes());
        out.extend_from_slice(&((i + 1) as i64).to_le_bytes());
        out.extend_from_slice(&(reclen as u16).to_le_bytes());
        out.push(*ty);
        out.extend_from_slice(name.as_bytes());
        out.resize(start + reclen, 0);
        i += 1;
    }
    t.process.mm.copy_to_user(dirp as usize, &out).map_err(fault)?;
    *pos = i;
    Ok(out.len() as i64)
}

pub fn getcwd(t: &Thread, buf: u64, size: u64) -> SysResult {
    let path = t.process.cwd.lock().1.clone();
    let mut bytes = path.into_bytes();
    bytes.push(0);
    if (size as usize) < bytes.len() {
        return Err(ERANGE);
    }
    t.process.mm.copy_to_user(buf as usize, &bytes).map_err(fault)?;
    Ok(bytes.len() as i64)
}

pub fn chdir(t: &Thread, path: u64) -> SysResult {
    let abs = absolute(t, AT_FDCWD, &user_path(t, path)?)?;
    match lookup(&abs) {
        Some(FileOrDir::Dir(d)) => {
            *t.process.cwd.lock() = (d, abs);
            Ok(0)
        }
        Some(FileOrDir::File(_)) => Err(ENOTDIR),
        None => Err(ENOENT),
    }
}

pub fn fchdir(t: &Thread, fd: u64) -> SysResult {
    let f = get_fd(t, fd)?;
    match &f.node {
        Node::Dir { dir, path } => {
            *t.process.cwd.lock() = (dir.clone(), path.clone());
            Ok(0)
        }
        _ => Err(ENOTDIR),
    }
}

pub fn mkdirat(t: &Thread, dirfd: i64, path: u64, _mode: u64) -> SysResult {
    let abs = absolute(t, dirfd, &user_path(t, path)?)?;
    if lookup(&abs).is_some() || special(t, &abs).is_some() {
        return Err(EEXIST);
    }
    let (parent, name) = split_parent(&abs)?;
    vfs_node::VFSDirectory::create(name, &parent).map(|_| 0).map_err(|_| EROFS)
}

pub fn unlinkat(t: &Thread, dirfd: i64, path: u64, flags: u64) -> SysResult {
    let abs = absolute(t, dirfd, &user_path(t, path)?)?;
    let node = lookup(&abs).ok_or(ENOENT)?;
    match (&node, flags & AT_REMOVEDIR != 0) {
        (FileOrDir::Dir(d), true) => {
            if !d.lock().list().is_empty() {
                return Err(ENOTEMPTY);
            }
        }
        (FileOrDir::Dir(_), false) => return Err(EISDIR),
        (FileOrDir::File(_), true) => return Err(ENOTDIR),
        (FileOrDir::File(_), false) => {}
    }
    let (parent, _) = split_parent(&abs)?;
    let removed = parent.lock().remove(&node);
    removed.map(|_| 0).ok_or(EBUSY)
}

pub fn renameat(t: &Thread, olddirfd: i64, old: u64, newdirfd: i64, new: u64) -> SysResult {
    let old_abs = absolute(t, olddirfd, &user_path(t, old)?)?;
    let new_abs = absolute(t, newdirfd, &user_path(t, new)?)?;
    if old_abs == new_abs {
        return Ok(0);
    }
    let node = lookup(&old_abs).ok_or(ENOENT)?;
    let FileOrDir::File(src) = &node else {
        return Err(EXDEV); // les nœuds du VFS portent leur nom : répertoires non déplaçables
    };
    let (new_parent, new_name) = split_parent(&new_abs)?;
    if let Some(existing) = lookup(&new_abs) {
        if matches!(existing, FileOrDir::Dir(_)) {
            return Err(EISDIR);
        }
        new_parent.lock().remove(&existing);
    }
    // Copie du contenu dans un nouveau fichier, puis retrait de l'ancien.
    let data = {
        let mut l = src.lock();
        let mut d = vec![0u8; l.len()];
        let n = l.read_at(&mut d, 0).map_err(|_| EIO)?;
        d.truncate(n);
        d
    };
    let dst = memfs::MemFile::create(new_name, &new_parent).map_err(|_| ENOSPC)?;
    if !data.is_empty() {
        dst.lock().write_at(&data, 0).map_err(|_| ENOSPC)?;
    }
    let (old_parent, _) = split_parent(&old_abs)?;
    old_parent.lock().remove(&node);
    Ok(0)
}

pub fn readlinkat(t: &Thread, dirfd: i64, path: u64, buf: u64, size: u64) -> SysResult {
    let abs = absolute(t, dirfd, &user_path(t, path)?)?;
    let target = match abs.as_str() {
        "/proc/self/exe" => t.process.exe.clone(),
        "/proc/self/cwd" => t.process.cwd.lock().1.clone(),
        p => match p.strip_prefix("/proc/self/fd/").and_then(|n| n.parse::<u64>().ok()) {
            Some(fd) => match &get_fd(t, fd)?.node {
                Node::File { path, .. } | Node::Dir { path, .. } | Node::Generated { path, .. } => path.clone(),
                Node::Console { .. } => String::from("/dev/tty"),
                Node::Null => String::from("/dev/null"),
                Node::Zero => String::from("/dev/zero"),
                Node::Random => String::from("/dev/urandom"),
            },
            None => {
                return if lookup(p).is_some() || special(t, p).is_some() { Err(EINVAL) } else { Err(ENOENT) };
            }
        },
    };
    let n = target.len().min(size as usize);
    t.process.mm.copy_to_user(buf as usize, &target.as_bytes()[..n]).map_err(fault)?;
    Ok(n as i64)
}

pub fn faccessat(t: &Thread, dirfd: i64, path: u64, _mode: u64, _flags: u64) -> SysResult {
    let abs = absolute(t, dirfd, &user_path(t, path)?)?;
    if special(t, &abs).is_some() || lookup(&abs).is_some() {
        Ok(0)
    } else {
        Err(ENOENT)
    }
}

/// `chmod`, `chown`… : le VFS ne conserve ni droits ni propriétaires (tout appartient à root).
pub fn chmod_like(_t: &Thread) -> SysResult {
    Ok(0)
}
