//! Sceaux des fichiers `memfd` (`fcntl(F_ADD_SEALS / F_GET_SEALS)`), comme `mm/memfd.c` de
//! Linux.
//!
//! Un client Wayland (foot, GTK, Qt…) remplit un `memfd`, le scelle contre la réduction
//! (`F_SEAL_SHRINK`) puis passe le descripteur au compositeur : celui-ci sait alors que la
//! mémoire projetée ne disparaîtra pas sous lui (pas de `SIGBUS` dans le serveur).
//!
//! Les sceaux appartiennent à l'inode. Un `memfd` est un fichier tmpfs anonyme, jamais relié à
//! un dossier : son `FileRef` est unique, c'est la clé du registre (comme `pagecache::key`).
//! Un `memfd` créé sans `MFD_ALLOW_SEALING` porte `F_SEAL_SEAL` dès sa création ; un fichier
//! qui n'est pas un `memfd` ne se scelle pas (`EINVAL`).

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use fs_node::{FileRef, WeakFileRef};
use spin::Mutex;

use crate::linux::errno::*;

pub const F_SEAL_SEAL: u32 = 0x1;
pub const F_SEAL_SHRINK: u32 = 0x2;
pub const F_SEAL_GROW: u32 = 0x4;
pub const F_SEAL_WRITE: u32 = 0x8;
pub const F_SEAL_FUTURE_WRITE: u32 = 0x10;
pub const F_SEAL_EXEC: u32 = 0x20;
const ALL: u32 = F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE | F_SEAL_FUTURE_WRITE | F_SEAL_EXEC;

struct Entry {
    file: WeakFileRef,
    seals: u32,
}

struct Table {
    map: BTreeMap<usize, Entry>,
    /// Taille au-delà de laquelle le prochain enregistrement purge les fichiers disparus.
    purge_at: usize,
}

static TABLE: Mutex<Table> = Mutex::new(Table { map: BTreeMap::new(), purge_at: 64 });

fn key(file: &FileRef) -> usize {
    Arc::as_ptr(file) as *const () as usize
}

/// Nouveau `memfd` avec ses sceaux initiaux (`F_SEAL_SEAL` sans `MFD_ALLOW_SEALING`,
/// `F_SEAL_EXEC` avec `MFD_NOEXEC_SEAL`).
pub fn register(file: &FileRef, seals: u32) {
    let mut t = TABLE.lock();
    if t.map.len() >= t.purge_at {
        t.map.retain(|_, e| e.file.strong_count() > 0);
        t.purge_at = (t.map.len() * 2).max(64);
    }
    let weak: WeakFileRef = Arc::downgrade(file);
    t.map.insert(key(file), Entry { file: weak, seals });
}

/// Sceaux de `file` (`None` : pas un `memfd`).
pub fn get(file: &FileRef) -> Option<u32> {
    let t = TABLE.lock();
    let e = t.map.get(&key(file))?;
    (e.file.strong_count() > 0).then_some(e.seals)
}

/// `F_ADD_SEALS` (`writable` : la description est ouverte en écriture, exigé par Linux).
pub fn add(file: &FileRef, writable: bool, seals: u32) -> Result<(), i64> {
    if seals & !ALL != 0 {
        return Err(EINVAL);
    }
    let mut t = TABLE.lock();
    let e = t.map.get_mut(&key(file)).filter(|e| e.file.strong_count() > 0).ok_or(EINVAL)?;
    if !writable {
        return Err(EPERM);
    }
    if e.seals & F_SEAL_SEAL != 0 {
        return Err(EPERM);
    }
    // `F_SEAL_WRITE` est refusé tant qu'une projection partagée en écriture existe : MaiProc ne
    // tient pas le compte des projections inscriptibles par fichier, l'objet partagé du fichier
    // (créé à la première projection partagée) en tient lieu.
    if seals & F_SEAL_WRITE != 0 && e.seals & F_SEAL_WRITE == 0 && crate::pagecache::has_shared(file) {
        return Err(EBUSY);
    }
    e.seals |= seals;
    Ok(())
}

/// Écriture de `len` octets à `off` sur un fichier de taille `size` : permise par les sceaux ?
pub fn check_write(file: &FileRef, off: usize, len: usize, size: usize) -> Result<(), i64> {
    let Some(s) = get(file) else { return Ok(()) };
    if s & (F_SEAL_WRITE | F_SEAL_FUTURE_WRITE) != 0 {
        return Err(EPERM);
    }
    if s & F_SEAL_GROW != 0 && off.saturating_add(len) > size {
        return Err(EPERM);
    }
    Ok(())
}

/// Changement de taille de `old` à `new`.
pub fn check_resize(file: &FileRef, old: usize, new: usize) -> Result<(), i64> {
    let Some(s) = get(file) else { return Ok(()) };
    if (new < old && s & F_SEAL_SHRINK != 0) || (new > old && s & F_SEAL_GROW != 0) {
        return Err(EPERM);
    }
    Ok(())
}

/// Projection partagée inscriptible (`mmap(MAP_SHARED, PROT_WRITE)`).
pub fn check_shared_write_map(file: &FileRef) -> Result<(), i64> {
    match get(file) {
        Some(s) if s & (F_SEAL_WRITE | F_SEAL_FUTURE_WRITE) != 0 => Err(EPERM),
        _ => Ok(()),
    }
}

/// Changement des droits (`F_SEAL_EXEC` fige les bits d'exécution).
pub fn check_chmod(file: &FileRef, old_mode: u32, new_mode: u32) -> Result<(), i64> {
    match get(file) {
        Some(s) if s & F_SEAL_EXEC != 0 && (old_mode ^ new_mode) & 0o111 != 0 => Err(EPERM),
        _ => Ok(()),
    }
}
