//! `flock(2)` : verrous consultatifs d'un fichier entier (BSD), tenus par une **description de
//! fichier ouverte** (`OpenFile`), comme sous Linux : partagés par `dup` et `fork`, relâchés à
//! la dernière fermeture de la description, indépendants d'un processus à l'autre.
//!
//! Le fichier est désigné par son inode `(st_dev, st_ino)` quand le système de fichiers le
//! connaît (deux liens durs d'un fichier tmpfs sont le même fichier), sinon par l'adresse de son
//! nœud. Un verrou est soit exclusif (un seul détenteur), soit partagé (plusieurs). Changer de
//! sorte n'est pas atomique, comme sous Linux : l'ancien verrou est relâché avant d'attendre le
//! nouveau. L'attente est interrompue par un signal (`EINTR`, ou relance avec `SA_RESTART`) ; `LOCK_NB` rend `EWOULDBLOCK`.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use spin::Mutex;

use crate::fd::{Node, OpenFile};
use crate::wait::{wait_event, WaitError, WaitQueue};
use crate::Thread;

const LOCK_SH: u64 = 1;
const LOCK_EX: u64 = 2;
const LOCK_NB: u64 = 4;
const LOCK_UN: u64 = 8;

/// Identité d'un fichier verrouillé.
type Key = (u64, u64);

#[derive(Default)]
struct State {
    exclusive: Option<usize>,
    shared: Vec<usize>,
}

struct Table {
    files: BTreeMap<Key, State>,
    /// Description → fichier qu'elle verrouille (une description ne désigne qu'un fichier).
    owners: BTreeMap<usize, Key>,
}

static TABLE: Mutex<Table> = Mutex::new(Table { files: BTreeMap::new(), owners: BTreeMap::new() });
static WAITERS: WaitQueue = WaitQueue::new();

pub(crate) fn key_of(f: &OpenFile) -> Option<Key> {
    let meta = |m: Option<fs_node::Metadata>| m.map(|m| (m.dev, m.ino));
    match &f.node {
        Node::File { file, .. } => {
            meta(fs_node::FsNode::metadata(&*file.lock())).or(Some((u64::MAX, alloc::sync::Arc::as_ptr(file) as *const u8 as u64)))
        }
        Node::Dir { dir, .. } => {
            meta(fs_node::FsNode::metadata(&*dir.lock())).or(Some((u64::MAX, alloc::sync::Arc::as_ptr(dir) as *const u8 as u64)))
        }
        // Tubes, sockets, terminaux : Linux les verrouille aussi (inode anonyme).
        _ => Some((u64::MAX - 1, f as *const OpenFile as u64)),
    }
}

fn owner_of(f: &OpenFile) -> usize {
    f as *const OpenFile as usize
}

/// Relâche le verrou de `owner` ; vrai s'il en tenait un.
fn release(t: &mut Table, owner: usize) -> bool {
    let Some(key) = t.owners.remove(&owner) else { return false };
    if let Some(st) = t.files.get_mut(&key) {
        if st.exclusive == Some(owner) {
            st.exclusive = None;
        }
        st.shared.retain(|&o| o != owner);
        if st.exclusive.is_none() && st.shared.is_empty() {
            t.files.remove(&key);
        }
    }
    true
}

/// Tentative sans attente ; vrai si le verrou est pris.
fn try_take(t: &mut Table, key: Key, owner: usize, exclusive: bool) -> bool {
    let st = t.files.entry(key).or_default();
    let free = if exclusive {
        st.exclusive.map_or(true, |o| o == owner) && st.shared.iter().all(|&o| o == owner)
    } else {
        st.exclusive.map_or(true, |o| o == owner)
    };
    if !free {
        if st.exclusive.is_none() && st.shared.is_empty() {
            t.files.remove(&key);
        }
        return false;
    }
    st.shared.retain(|&o| o != owner);
    if exclusive {
        st.exclusive = Some(owner);
    } else {
        st.exclusive = None;
        st.shared.push(owner);
    }
    t.owners.insert(owner, key);
    true
}

pub fn flock(t: &Thread, f: &OpenFile, op: u64) -> Result<i64, i64> {
    use crate::linux::errno::{EAGAIN as EWOULDBLOCK, EINVAL};
    use crate::signal::ERESTARTSYS;
    let owner = owner_of(f);
    let kind = op & !LOCK_NB;
    if kind == LOCK_UN {
        if release(&mut TABLE.lock(), owner) {
            WAITERS.wake_all();
        }
        return Ok(0);
    }
    let exclusive = match kind {
        LOCK_SH => false,
        LOCK_EX => true,
        _ => return Err(EINVAL),
    };
    let key = key_of(f).ok_or(EINVAL)?;
    {
        let mut tab = TABLE.lock();
        // Même sorte déjà tenue : rien à faire.
        if tab.owners.get(&owner) == Some(&key) {
            let st = &tab.files[&key];
            if (exclusive && st.exclusive == Some(owner)) || (!exclusive && st.shared.contains(&owner)) {
                return Ok(0);
            }
        }
        if try_take(&mut tab, key, owner, exclusive) {
            f.flocked.store(true, Ordering::Release);
            return Ok(0);
        }
        // Conversion : l'ancien verrou est rendu avant d'attendre (comme `flock_lock_inode`).
        let had = release(&mut tab, owner);
        drop(tab);
        if had {
            WAITERS.wake_all();
        }
    }
    if op & LOCK_NB != 0 {
        return Err(EWOULDBLOCK);
    }
    match wait_event(&[&WAITERS], None, Some(t), || try_take(&mut TABLE.lock(), key, owner, exclusive).then_some(())) {
        Ok(()) => {
            f.flocked.store(true, Ordering::Release);
            Ok(0)
        }
        // Relancé après le gestionnaire si `SA_RESTART`, `EINTR` sinon (comme Linux).
        Err(WaitError::Interrupted) | Err(WaitError::TimedOut) => Err(ERESTARTSYS),
    }
}

/// Dernière fermeture d'une description : son verrou est relâché.
pub(crate) fn on_close(f: &OpenFile) {
    if f.flocked.load(Ordering::Acquire) && release(&mut TABLE.lock(), owner_of(f)) {
        WAITERS.wake_all();
    }
}
