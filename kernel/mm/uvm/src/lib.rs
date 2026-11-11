//! MaiProc — mémoire virtuelle des processus utilisateur.
//!
//! Un [`Mm`] décrit la moitié basse d'un espace d'adresses : ses zones ([`Vma`], arbre ordonné),
//! son tas (`brk`) et sa zone `mmap` (allocation descendante, base aléatoire). Les pages sont
//! créées **à la demande** par [`Mm::handle_fault`] : zéro (anonyme, pris dans le pool pré-zéroé
//! de MaiMM), copie privée d'un fichier (segments ELF), ou page d'un objet partagé ([`shm`]).
//!
//! ## `fork` sans copie ([`Mm::fork`])
//! Les tables du dernier niveau sont partagées entre parent et enfant (On-Demand-Fork,
//! EuroSys 2021 : ×65 sur le temps de `fork` d'un processus de 1 Gio) ; elles ne sont copiées
//! qu'à la première modification, et les pages ne le sont qu'à la première écriture (copie à
//! l'écriture décidée par le compteur de références de la page, [`refs`]). Un `fork` suivi
//! d'un `execve` ne copie donc ni page ni table P1.
//!
//! ## Accès et invalidation
//! Les tables de pages et les copies noyau ↔ utilisateur passent par la *physmap* : aucune faute
//! de page n'est jamais prise par le noyau sur une adresse utilisateur (validation par VMA puis
//! pré-faute), et un espace non chargé (autre processus) se manipule comme l'espace courant.
//! Toute modification passe par un lot [`tlb::Flush`] : invalidation locale, IPI aux seuls cœurs
//! qui exécutent un fil de l'espace, frames rendues après.
//!
//! Le verrou d'un `Mm` coupe la préemption mais pas les interruptions : un cœur qui l'attend
//! répond aux IPI d'invalidation de celui qui le tient.

#![no_std]
#![cfg(target_arch = "x86_64")]
#![feature(abi_x86_interrupt)]

extern crate alloc;

pub mod pagecache;
mod pt;
pub mod rand;
pub mod refs;
pub mod shm;
pub mod tlb;

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::sync::atomic::AtomicU32;
use kernel_config::memory::{PAGE_SIZE, USER_SPACE_END};
use memory::paging::address_space::AddressSpace;
use memory::paging::kernel_half::phys_to_virt;
use sync_preemption::PreemptionSafeMutex;
use tlb::Flush;

pub use pt::{PTE_PRESENT, PTE_WRITABLE};
pub use pagecache::{CachedFile, FileReader};
pub use shm::{FileIo, ShmObject};

/// Plus petite adresse qu'un processus peut projeter (comme `vm.mmap_min_addr` de Linux).
pub const MMAP_MIN_ADDR: usize = 0x1_0000;
/// Fin (exclue) de l'espace utilisable par un processus.
pub const USER_END: usize = USER_SPACE_END - PAGE_SIZE;

#[inline]
pub const fn page_down(x: usize) -> usize { x & !(PAGE_SIZE - 1) }
#[inline]
pub const fn page_up(x: usize) -> usize { (x + PAGE_SIZE - 1) & !(PAGE_SIZE - 1) }

/// Droits d'une zone (mêmes valeurs que `PROT_*` de Linux).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Prot(pub u32);

impl Prot {
    pub const NONE: Prot = Prot(0);
    pub const READ: u32 = 1;
    pub const WRITE: u32 = 2;
    pub const EXEC: u32 = 4;
    pub const RW: Prot = Prot(Self::READ | Self::WRITE);
    pub const RX: Prot = Prot(Self::READ | Self::EXEC);
    pub fn read(self) -> bool { self.0 & (Self::READ | Self::WRITE | Self::EXEC) != 0 }
    pub fn write(self) -> bool { self.0 & Self::WRITE != 0 }
    pub fn exec(self) -> bool { self.0 & Self::EXEC != 0 }
}

/// Source du contenu d'une zone projetée depuis un fichier.
pub trait PageSource: Send + Sync {
    /// Taille du fichier en octets.
    fn len(&self) -> usize;
    /// Fichier vide.
    fn is_empty(&self) -> bool { self.len() == 0 }
    /// Copie `dst.len()` octets à partir de `offset` (le fichier est assez long : l'appelant borne).
    fn read_at(&self, offset: usize, dst: &mut [u8]);
    /// Frame partagée contenant la page `index` du fichier (octets nuls au-delà de la fin), avec
    /// une référence de plus pour l'appelant : [`pagecache::CachedFile`]. `None` : pas de cache,
    /// la page est copiée.
    fn cached_page(&self, _index: usize) -> Option<usize> { None }
}

/// Fichier entièrement en mémoire (image ELF lue une fois).
pub struct MemFile(pub Arc<[u8]>);

impl PageSource for MemFile {
    fn len(&self) -> usize { self.0.len() }
    fn read_at(&self, offset: usize, dst: &mut [u8]) {
        dst.copy_from_slice(&self.0[offset..offset + dst.len()]);
    }
}

/// Contenu d'une zone.
#[derive(Clone)]
pub enum Backing {
    /// Pages privées mises à zéro à la première utilisation.
    Anon,
    /// Copie privée d'un fichier : les octets `[offset, offset + filesz)` du fichier apparaissent à
    /// l'adresse `file_va` (pas forcément alignée) ; le reste de la zone est à zéro (`.bss`).
    File { src: Arc<dyn PageSource>, offset: usize, file_va: usize, filesz: usize },
    /// Pages d'un objet partagé (`MAP_SHARED | MAP_ANONYMOUS`) ; `pgoff` : rang, dans l'objet,
    /// de la première page de la zone.
    Shared { obj: Arc<ShmObject>, pgoff: usize },
}

/// Zone de l'espace d'adresses (bornes alignées sur la page).
#[derive(Clone)]
pub struct Vma {
    pub start: usize,
    pub end: usize,
    pub prot: Prot,
    pub backing: Backing,
    /// Nom (`[stack]`, `[heap]`, chemin du fichier projeté), celui de `/proc/<pid>/maps`.
    pub name: VmaName,
    /// La zone peut devenir inscriptible (`VM_MAYWRITE` de Linux) : faux pour une projection
    /// partagée d'un fichier ouvert en lecture seule ou scellé contre l'écriture, que
    /// `mprotect(PROT_WRITE)` refuse alors (`EACCES`).
    pub may_write: bool,
}

/// Nom d'une zone : constante (`[heap]`, `[stack]`, `[anon]`…) ou chemin d'un fichier, partagé
/// par les zones issues d'une même projection (découpes de `mprotect`, `fork`).
#[derive(Clone)]
pub enum VmaName {
    Static(&'static str),
    Path(Arc<str>),
}

impl VmaName {
    pub fn as_str(&self) -> &str {
        match self {
            VmaName::Static(s) => s,
            VmaName::Path(p) => p,
        }
    }
}

impl From<&'static str> for VmaName {
    fn from(s: &'static str) -> VmaName { VmaName::Static(s) }
}

impl From<Arc<str>> for VmaName {
    fn from(p: Arc<str>) -> VmaName { VmaName::Path(p) }
}

impl PartialEq<&str> for VmaName {
    fn eq(&self, other: &&str) -> bool { self.as_str() == *other }
}

impl Vma {
    pub fn is_shared(&self) -> bool { matches!(self.backing, Backing::Shared { .. }) }
}

/// Erreurs des opérations mémoire (converties en `errno` par la couche ABI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmError {
    /// Adresse hors de toute zone (SIGSEGV / EFAULT).
    NoMapping,
    /// Droits insuffisants (SIGSEGV / EFAULT / EACCES).
    Access,
    /// Paramètres invalides (EINVAL).
    Invalid,
    /// Plus de mémoire ou d'espace d'adresses (ENOMEM).
    NoMemory,
}

/// Clé d'un futex : privée (espace + adresse) ou partagée (objet + décalage).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub enum FutexKey {
    Private { mm: usize, addr: usize },
    Shared { obj: u64, offset: usize },
}

#[derive(Clone)]
struct Inner {
    vmas: BTreeMap<usize, Vma>,
    brk_start: usize,
    brk: usize,
    /// Les `mmap` sans adresse sont placés sous cette limite, en descendant.
    mmap_top: usize,
    /// Fin de l'espace utilisable (`TASK_SIZE`) : [`USER_END`], ou juste sous 4 Gio pour un
    /// processus 32 bits (`IA32_PAGE_OFFSET` de Linux).
    task_size: usize,
    /// Pages présentes (résidentes).
    rss: usize,
}

/// Mémoire d'un processus.
pub struct Mm {
    space: Arc<AddressSpace>,
    inner: PreemptionSafeMutex<Inner>,
}

static INIT: spin::Once<Result<(), &'static str>> = spin::Once::new();

/// Prépare la mémoire des processus (compteurs de frames, vecteur d'IPI). Idempotent.
pub fn init() -> Result<(), &'static str> {
    *INIT.call_once(|| {
        refs::init()?;
        tlb::init();
        Ok(())
    })
}

impl Mm {
    /// Espace vide ; `mmap_top` borne la zone `mmap` (voir [`rand`]).
    pub fn new(mmap_top: usize) -> Result<Arc<Mm>, &'static str> {
        init()?;
        let space = AddressSpace::new()?;
        Ok(Arc::new(Mm {
            space,
            inner: PreemptionSafeMutex::new(Inner {
                vmas: BTreeMap::new(),
                brk_start: 0,
                brk: 0,
                mmap_top: page_down(mmap_top),
                task_size: USER_END,
                rss: 0,
            }),
        }))
    }

    /// Borne l'espace à `[0, task_size)` (processus 32 bits) : aucune zone ne sera posée
    /// au-delà, ni par `mmap`, ni par `brk`, ni par `mremap`.
    pub fn set_task_size(&self, task_size: usize) {
        let mut inner = self.inner.lock();
        inner.task_size = page_down(task_size.min(USER_END));
        inner.mmap_top = inner.mmap_top.min(inner.task_size);
    }

    /// Fin de l'espace utilisable.
    pub fn task_size(&self) -> usize { self.inner.lock().task_size }

    /// Espace d'adresses matériel (racine P4, PCID).
    pub fn space(&self) -> &Arc<AddressSpace> { &self.space }

    /// Pages résidentes.
    pub fn rss(&self) -> usize { self.inner.lock().rss }

    /// `fork` : espace enfant qui partage toutes les tables P1 (et donc toutes les pages) du
    /// parent, en lecture seule des deux côtés. Les zones partagées le restent ; les autres
    /// passent en copie à l'écriture. Coût : les tables P4/P3/P2 (quelques pages), quelle que
    /// soit la mémoire du processus.
    pub fn fork(&self) -> Result<Arc<Mm>, VmError> {
        let space = AddressSpace::new().map_err(|_| VmError::NoMemory)?;
        let inner = self.inner.lock();
        let child = Arc::new(Mm { space, inner: PreemptionSafeMutex::new(inner.clone()) });
        let mut fl = Flush::new(&self.space);
        // SAFETY : tables du parent sous son verrou ; celles de l'enfant ne sont vues que d'ici.
        let shared = unsafe { pt::fork_tables(self.space.p4_pfn(), child.space.p4_pfn()) };
        // Les P2 du parent ont perdu leur bit W : tous ses cœurs doivent l'oublier.
        fl.all();
        drop(fl);
        drop(inner);
        match shared {
            Some(_) => Ok(child),
            None => Err(VmError::NoMemory),
        }
    }

    /// Pose une zone à une adresse fixe (remplace ce qui s'y trouvait).
    pub fn map_fixed(&self, vma: Vma) -> Result<(), VmError> {
        if vma.start % PAGE_SIZE != 0 || vma.end % PAGE_SIZE != 0 || vma.start >= vma.end
            || vma.start < MMAP_MIN_ADDR || vma.end > USER_END
        {
            return Err(VmError::Invalid);
        }
        let mut inner = self.inner.lock();
        if vma.end > inner.task_size {
            return Err(VmError::Invalid);
        }
        let mut fl = Flush::new(&self.space);
        self.unmap_locked(&mut inner, &mut fl, vma.start, vma.end);
        inner.vmas.insert(vma.start, vma);
        Ok(())
    }

    /// `mmap` : renvoie l'adresse de la zone créée. Sans `fixed`, `hint` est une suggestion.
    pub fn mmap(&self, hint: usize, len: usize, prot: Prot, backing: Backing, fixed: bool, name: impl Into<VmaName>) -> Result<usize, VmError> {
        let name = name.into();
        if len == 0 {
            return Err(VmError::Invalid);
        }
        let len = page_up(len);
        let mut inner = self.inner.lock();
        let mut fl = Flush::new(&self.space);
        let limit = inner.task_size;
        let start = if fixed {
            if hint % PAGE_SIZE != 0 || hint < MMAP_MIN_ADDR || hint.checked_add(len).map_or(true, |e| e > limit) {
                return Err(VmError::Invalid);
            }
            self.unmap_locked(&mut inner, &mut fl, hint, hint + len);
            hint
        } else {
            let h = page_down(hint);
            if h >= MMAP_MIN_ADDR && h.checked_add(len).map_or(false, |e| e <= limit) && Self::is_free(&inner, h, h + len) {
                h
            } else {
                Self::find_gap_top_down(&inner, len).ok_or(VmError::NoMemory)?
            }
        };
        let backing = match backing {
            // Une projection de fichier par `mmap` commence au début de la zone.
            Backing::File { src, offset, filesz, .. } => Backing::File { src, offset, file_va: start, filesz },
            b => b,
        };
        inner.vmas.insert(start, Vma { start, end: start + len, prot, backing, name, may_write: true });
        Ok(start)
    }

    /// `munmap`.
    pub fn munmap(&self, start: usize, len: usize) -> Result<(), VmError> {
        if start % PAGE_SIZE != 0 || len == 0 {
            return Err(VmError::Invalid);
        }
        let end = start.checked_add(page_up(len)).ok_or(VmError::Invalid)?.min(USER_END);
        let mut inner = self.inner.lock();
        let mut fl = Flush::new(&self.space);
        self.unmap_locked(&mut inner, &mut fl, start, end);
        Ok(())
    }

    /// `mprotect`.
    /// La zone qui commence à `start` (créée par [`Mm::mmap`]) ne pourra jamais devenir
    /// inscriptible (voir [`Vma::may_write`]).
    pub fn deny_write(&self, start: usize) {
        if let Some(v) = self.inner.lock().vmas.get_mut(&start) {
            v.may_write = false;
        }
    }

    pub fn mprotect(&self, start: usize, len: usize, prot: Prot) -> Result<(), VmError> {
        if start % PAGE_SIZE != 0 {
            return Err(VmError::Invalid);
        }
        let end = start.checked_add(page_up(len)).ok_or(VmError::Invalid)?;
        if end > USER_END {
            return Err(VmError::NoMemory);
        }
        let mut inner = self.inner.lock();
        // Toute la plage doit être couverte (comme Linux : ENOMEM sinon).
        let mut cursor = start;
        for (_, v) in inner.vmas.range(..end) {
            if v.end <= cursor { continue; }
            if v.start > cursor { return Err(VmError::NoMemory); }
            cursor = v.end;
        }
        if cursor < end {
            return Err(VmError::NoMemory);
        }
        // Zones scellées (vDSO) ou qui ne peuvent devenir inscriptibles : refusé.
        if prot.write()
            && inner.vmas.range(..end).any(|(_, v)| {
                v.end > start && (!v.may_write || matches!(&v.backing, Backing::Shared { obj, .. } if obj.is_sealed()))
            })
        {
            return Err(VmError::Access);
        }
        Self::split_at(&mut inner, start);
        Self::split_at(&mut inner, end);
        let keys: Vec<usize> = inner.vmas.range(start..end).map(|(k, _)| *k).collect();
        for k in keys {
            if let Some(v) = inner.vmas.get_mut(&k) {
                v.prot = prot;
            }
        }
        // Entrées présentes : nouveaux droits (une page en copie à l'écriture reste protégée).
        let mut fl = Flush::new(&self.space);
        // SAFETY : tables de cet espace, sous le verrou.
        let ok = unsafe {
            pt::for_each_present_mut(self.space.p4_pfn(), start, end, pt::WholeTable::Visit, &mut fl, |_| {}, |fl, va, pte| {
                let new = pt::protect(*pte, prot);
                if new != *pte {
                    let restricted = (*pte & PTE_WRITABLE != 0 && new & PTE_WRITABLE == 0)
                        || (*pte & PTE_PRESENT != 0 && new & PTE_PRESENT == 0)
                        || (*pte & (1 << 63) == 0 && new & (1 << 63) != 0);
                    *pte = new;
                    if restricted { fl.page(va) } else { fl.upgrade(va) }
                }
            })
        };
        ok.ok_or(VmError::NoMemory)
    }

    /// Fixe le début du tas (fin du programme chargé, alignée).
    pub fn set_brk_base(&self, base: usize) {
        let mut inner = self.inner.lock();
        inner.brk_start = page_up(base);
        inner.brk = inner.brk_start;
    }

    /// `brk` : renvoie la nouvelle limite (l'ancienne si la demande est refusée).
    pub fn brk(&self, new_brk: usize) -> usize {
        let mut inner = self.inner.lock();
        let (start, cur) = (inner.brk_start, inner.brk);
        if new_brk < start || new_brk > inner.task_size {
            return cur;
        }
        let old_end = page_up(cur);
        let new_end = page_up(new_brk);
        if new_end == old_end {
            inner.brk = new_brk;
            return new_brk;
        }
        if new_end > old_end {
            if !Self::is_free(&inner, old_end, new_end) {
                return cur;
            }
            // Agrandit la zone du tas (ou la crée).
            let heap_vma = inner.vmas.range_mut(..old_end).next_back().filter(|(_, v)| v.end == old_end && v.name == "[heap]");
            match heap_vma {
                Some((_, v)) => v.end = new_end,
                None => {
                    inner.vmas.insert(old_end, Vma { start: old_end, end: new_end, prot: Prot::RW, backing: Backing::Anon, name: VmaName::Static("[heap]"), may_write: true });
                }
            }
        } else {
            let mut fl = Flush::new(&self.space);
            self.unmap_locked(&mut inner, &mut fl, new_end, old_end);
        }
        inner.brk = new_brk;
        new_brk
    }

    /// Vrai si `[start, start + len)` ne recouvre aucune zone (`MAP_FIXED_NOREPLACE`).
    pub fn is_free_range(&self, start: usize, len: usize) -> bool {
        let Some(end) = start.checked_add(page_up(len)) else { return false };
        Self::is_free(&self.inner.lock(), start, end)
    }

    /// `madvise(MADV_DONTNEED)` : rend les pages présentes de la plage ; les zones restent, et la
    /// prochaine faute redonne une page zéro (anonyme), le contenu d'origine (copie de fichier)
    /// ou la page de l'objet (zone partagée).
    pub fn discard(&self, start: usize, len: usize) -> Result<(), VmError> {
        if start % PAGE_SIZE != 0 {
            return Err(VmError::Invalid);
        }
        let end = start.checked_add(page_up(len)).ok_or(VmError::Invalid)?.min(USER_END);
        let mut inner = self.inner.lock();
        let mut fl = Flush::new(&self.space);
        let mut freed = 0;
        let mut released = 0;
        // SAFETY : tables de cet espace, sous le verrou.
        let ok = unsafe {
            pt::for_each_present_mut(self.space.p4_pfn(), start, end, pt::WholeTable::Release, &mut fl, |n| released += n, |fl, va, pte| {
                fl.put_page(pt::pte_pfn(*pte));
                *pte = 0;
                fl.page(va);
                freed += 1;
            })
        };
        let gone = freed + released;
        inner.rss -= gone.min(inner.rss);
        ok.ok_or(VmError::NoMemory)
    }

    /// `mincore` : un octet par page (bit 0 : présente). `NoMapping` si un trou est rencontré.
    pub fn mincore(&self, start: usize, len: usize) -> Result<Vec<u8>, VmError> {
        if start % PAGE_SIZE != 0 {
            return Err(VmError::Invalid);
        }
        let end = start.checked_add(page_up(len)).ok_or(VmError::Invalid)?;
        let inner = self.inner.lock();
        let mut out = Vec::with_capacity((end - start) / PAGE_SIZE);
        let mut va = start;
        while va < end {
            Self::find_vma(&inner, va).ok_or(VmError::NoMapping)?;
            // SAFETY : tables de cet espace, sous le verrou.
            let present = unsafe { pt::lookup(self.space.p4_pfn(), va) }.map_or(false, |p| p & PTE_PRESENT != 0);
            out.push(present as u8);
            va += PAGE_SIZE;
        }
        Ok(out)
    }

    /// Objets partagés de fichier projetés dans `[start, start + len)`, avec les rangs de pages
    /// (dans l'objet) que la plage couvre : pour `msync`. `NoMapping` si un trou est rencontré.
    pub fn shared_files_in(&self, start: usize, len: usize) -> Result<Vec<(Arc<ShmObject>, usize, usize)>, VmError> {
        if start % PAGE_SIZE != 0 {
            return Err(VmError::Invalid);
        }
        let end = start.checked_add(page_up(len)).ok_or(VmError::Invalid)?;
        let inner = self.inner.lock();
        let mut out = Vec::new();
        let mut va = start;
        while va < end {
            let v = Self::find_vma(&inner, va).ok_or(VmError::NoMapping)?;
            let hi = v.end.min(end);
            if let Backing::Shared { obj, pgoff } = &v.backing {
                if obj.is_file() {
                    out.push((obj.clone(), pgoff + (va - v.start) / PAGE_SIZE, pgoff + (hi - 1 - v.start) / PAGE_SIZE));
                }
            }
            va = hi;
        }
        Ok(out)
    }

    /// Zones partagées qui recoupent `[start, end)`, trous permis : `(début, fin, identifiant de
    /// l'objet, rang de la première page)`. Pour `shmdt`, qui retire les zones d'un segment
    /// System V à partir de son adresse d'attache.
    pub fn shared_vmas_in(&self, start: usize, end: usize) -> Vec<(usize, usize, u64, usize)> {
        let inner = self.inner.lock();
        let first = inner.vmas.range(..=start).next_back().map_or(start, |(&s, _)| s);
        inner
            .vmas
            .range(first..end)
            .filter(|(_, v)| v.end > start)
            .filter_map(|(_, v)| match &v.backing {
                Backing::Shared { obj, pgoff } => Some((v.start, v.end, obj.id(), *pgoff)),
                _ => None,
            })
            .collect()
    }

    /// `mremap` : réduit sur place, agrandit sur place si l'espace suivant est libre, sinon
    /// (avec `may_move`) **déplace les entrées de table** vers une nouvelle plage, sans copier
    /// une seule page (comme `move_page_tables` de Linux) : un `realloc` de plusieurs Mio coûte
    /// quelques écritures d'entrées.
    ///
    /// `fixed` (`MREMAP_FIXED`) impose la destination : ce qui s'y trouvait est retiré.
    pub fn mremap(&self, old: usize, old_len: usize, new_len: usize, may_move: bool, fixed: Option<usize>) -> Result<usize, VmError> {
        if old % PAGE_SIZE != 0 || new_len == 0 {
            return Err(VmError::Invalid);
        }
        if let Some(d) = fixed {
            let end = d.checked_add(page_up(new_len)).ok_or(VmError::Invalid)?;
            let old_end = old.checked_add(page_up(old_len)).ok_or(VmError::Invalid)?;
            if !may_move || d % PAGE_SIZE != 0 || d < MMAP_MIN_ADDR || end > USER_END || (d < old_end && old < end) {
                return Err(VmError::Invalid);
            }
        }
        let old_len = page_up(old_len);
        let new_len = page_up(new_len);
        let old_end = old.checked_add(old_len).ok_or(VmError::Invalid)?;
        let mut inner = self.inner.lock();
        if fixed.map_or(false, |d| d + page_up(new_len) > inner.task_size) {
            return Err(VmError::Invalid);
        }
        let mut fl = Flush::new(&self.space);
        // La plage doit tenir dans une seule zone.
        let vma = Self::find_vma(&inner, old).ok_or(VmError::NoMapping)?.clone();
        if old_end > vma.end {
            return Err(VmError::NoMapping);
        }
        if fixed.is_none() {
            if new_len <= old_len {
                self.unmap_locked(&mut inner, &mut fl, old + new_len, old_end);
                return Ok(old);
            }
            let new_end = old.checked_add(new_len).ok_or(VmError::NoMemory)?;
            if old_end == vma.end && new_end <= inner.task_size && Self::is_free(&inner, old_end, new_end) {
                inner.vmas.get_mut(&vma.start).unwrap().end = new_end;
                return Ok(old);
            }
            if !may_move {
                return Err(VmError::NoMemory);
            }
        }
        // Une réduction déplacée ne garde que le début de la plage.
        let old_end = if new_len < old_len {
            self.unmap_locked(&mut inner, &mut fl, old + new_len, old_end);
            old + new_len
        } else {
            old_end
        };
        let dest = match fixed {
            Some(d) => {
                self.unmap_locked(&mut inner, &mut fl, d, d + new_len);
                d
            }
            None => Self::find_gap_top_down(&inner, new_len).ok_or(VmError::NoMemory)?,
        };
        // Déplacement des entrées présentes (les références des pages suivent leur entrée).
        let p4 = self.space.p4_pfn();
        let mut moves: Vec<(usize, u64)> = Vec::new();
        // SAFETY : tables de cet espace, sous le verrou.
        unsafe {
            pt::for_each_present_mut(p4, old, old_end, pt::WholeTable::Visit, &mut fl, |_| {}, |fl, va, pte| {
                moves.push((va, *pte));
                *pte = 0;
                fl.page(va);
            })
            .ok_or(VmError::NoMemory)?;
            for (va, entry) in moves {
                let slot = pt::walk_create(p4, dest + (va - old), &mut fl).ok_or(VmError::NoMemory)?;
                *slot = entry;
            }
        }
        Self::split_at(&mut inner, old);
        Self::split_at(&mut inner, old_end);
        let mut moved = inner.vmas.remove(&old).ok_or(VmError::NoMapping)?;
        moved.start = dest;
        moved.end = dest + new_len;
        if let Backing::File { offset, file_va, filesz, .. } = &mut moved.backing {
            // Recalé sur le début de la zone déplacée : les octets du fichier qui précédaient
            // `old` ne la concernent plus.
            let delta = old.saturating_sub(*file_va);
            *offset += delta.min(*filesz);
            *filesz = filesz.saturating_sub(delta);
            *file_va = dest + file_va.saturating_sub(old);
        }
        inner.vmas.insert(dest, moved);
        Ok(dest)
    }

    /// Traite une faute de page à `addr`. `Ok` : l'accès peut être rejoué.
    pub fn handle_fault(&self, addr: usize, write: bool, exec: bool) -> Result<(), VmError> {
        let mut inner = self.inner.lock();
        let mut fl = Flush::new(&self.space);
        self.fault_locked(&mut inner, &mut fl, addr, write, exec)
    }

    /// Projette d'avance les pages de `[start, start + len)` (pile initiale, `MAP_POPULATE`).
    pub fn populate(&self, start: usize, len: usize, write: bool) -> Result<(), VmError> {
        let mut inner = self.inner.lock();
        let mut fl = Flush::new(&self.space);
        let mut va = page_down(start);
        while va < start + len {
            self.fault_locked(&mut inner, &mut fl, va, write, false)?;
            va += PAGE_SIZE;
        }
        Ok(())
    }

    // --- Copies noyau ↔ utilisateur --------------------------------------------------------------

    /// Copie `src` vers l'adresse utilisateur `uaddr`.
    pub fn copy_to_user(&self, uaddr: usize, src: &[u8]) -> Result<(), VmError> {
        self.with_user_range(uaddr, src.len(), true, |kva, off, n| unsafe {
            core::ptr::copy_nonoverlapping(src.as_ptr().add(off), kva as *mut u8, n);
        })
    }

    /// Copie depuis l'adresse utilisateur `uaddr` vers `dst`.
    pub fn copy_from_user(&self, dst: &mut [u8], uaddr: usize) -> Result<(), VmError> {
        let ptr = dst.as_mut_ptr();
        self.with_user_range(uaddr, dst.len(), false, |kva, off, n| unsafe {
            core::ptr::copy_nonoverlapping(kva as *const u8, ptr.add(off), n);
        })
    }

    /// Met à zéro `len` octets utilisateur.
    pub fn clear_user(&self, uaddr: usize, len: usize) -> Result<(), VmError> {
        self.with_user_range(uaddr, len, true, |kva, _, n| unsafe {
            core::ptr::write_bytes(kva as *mut u8, 0, n);
        })
    }

    /// Lit une valeur `T` (sans pointeur ni invariant) à `uaddr`.
    pub fn read_user<T: Copy + Default>(&self, uaddr: usize) -> Result<T, VmError> {
        let mut v = T::default();
        // SAFETY : `T` est `Copy`, on remplit ses octets.
        let bytes = unsafe { core::slice::from_raw_parts_mut(&mut v as *mut T as *mut u8, core::mem::size_of::<T>()) };
        self.copy_from_user(bytes, uaddr)?;
        Ok(v)
    }

    /// Écrit une valeur `T` à `uaddr`.
    pub fn write_user<T: Copy>(&self, uaddr: usize, v: &T) -> Result<(), VmError> {
        // SAFETY : lecture des octets d'une valeur `Copy`.
        let bytes = unsafe { core::slice::from_raw_parts(v as *const T as *const u8, core::mem::size_of::<T>()) };
        self.copy_to_user(uaddr, bytes)
    }

    /// Lit une chaîne terminée par zéro (sans le zéro), au plus `max` octets.
    pub fn read_cstr(&self, uaddr: usize, max: usize) -> Result<Vec<u8>, VmError> {
        let mut out = Vec::new();
        let mut addr = uaddr;
        loop {
            let chunk = (PAGE_SIZE - addr % PAGE_SIZE).min(max - out.len()).max(1);
            let mut buf = [0u8; PAGE_SIZE];
            self.copy_from_user(&mut buf[..chunk], addr)?;
            if let Some(z) = buf[..chunk].iter().position(|&b| b == 0) {
                out.extend_from_slice(&buf[..z]);
                return Ok(out);
            }
            out.extend_from_slice(&buf[..chunk]);
            if out.len() >= max {
                return Err(VmError::Invalid);
            }
            addr += chunk;
        }
    }

    /// Opération atomique sur le mot de 32 bits utilisateur `uaddr` (aligné) : `f` reçoit le mot
    /// vu par la physmap, page rendue présente (et privée en écriture si `write`). Sert aux
    /// futex (`FUTEX_WAKE_OP`, listes robustes, `FUTEX_LOCK_PI`).
    pub fn with_user_atomic_u32<R>(&self, uaddr: usize, write: bool, f: impl FnOnce(&AtomicU32) -> R) -> Result<R, VmError> {
        if uaddr % 4 != 0 {
            return Err(VmError::Invalid);
        }
        let mut f = Some(f);
        let mut out = None;
        self.with_user_range(uaddr, 4, write, |kva, _, _| {
            // SAFETY : mot aligné d'une page présente, vu par la physmap (frame stable sous le
            // verrou de l'espace).
            let a = unsafe { &*(kva as *const AtomicU32) };
            out = f.take().map(|f| f(a));
        })?;
        out.ok_or(VmError::NoMapping)
    }

    /// Clé du futex à `uaddr`. Une zone partagée donne une clé commune à tous les processus
    /// qui la projettent (objet, décalage) ; sinon, la clé est propre à cet espace, comme
    /// `get_futex_key` de Linux pour l'anonyme privé — même sans `FUTEX_PRIVATE_FLAG`.
    pub fn futex_key(&self, uaddr: usize, private: bool) -> Result<FutexKey, VmError> {
        if uaddr % 4 != 0 {
            return Err(VmError::Invalid);
        }
        let me = self as *const Mm as usize;
        if private {
            return Ok(FutexKey::Private { mm: me, addr: uaddr });
        }
        let inner = self.inner.lock();
        let vma = Self::find_vma(&inner, uaddr).ok_or(VmError::NoMapping)?;
        Ok(match &vma.backing {
            Backing::Shared { obj, pgoff } => FutexKey::Shared {
                obj: obj.id(),
                offset: (pgoff * PAGE_SIZE) + (uaddr - vma.start),
            },
            _ => FutexKey::Private { mm: me, addr: uaddr },
        })
    }

    /// Applique `f(adresse_noyau, décalage, longueur)` à chaque morceau (par page) de la plage
    /// utilisateur, après l'avoir rendue présente avec les droits voulus.
    fn with_user_range(&self, uaddr: usize, len: usize, write: bool, mut f: impl FnMut(usize, usize, usize)) -> Result<(), VmError> {
        if len == 0 {
            return Ok(());
        }
        let end = uaddr.checked_add(len).ok_or(VmError::NoMapping)?;
        if end > USER_END {
            return Err(VmError::NoMapping);
        }
        let mut inner = self.inner.lock();
        let mut fl = Flush::new(&self.space);
        let mut va = uaddr;
        let mut done = 0;
        while va < end {
            let n = (PAGE_SIZE - va % PAGE_SIZE).min(end - va);
            // SAFETY : tables de cet espace, sous le verrou.
            let mut pte = unsafe { pt::lookup(self.space.p4_pfn(), va) };
            let ok = |p: u64| p & PTE_PRESENT != 0 && (!write || p & PTE_WRITABLE != 0);
            if !pte.map_or(false, ok) {
                self.fault_locked(&mut inner, &mut fl, va, write, false)?;
                pte = unsafe { pt::lookup(self.space.p4_pfn(), va) };
            }
            let pte = pte.filter(|&p| ok(p)).ok_or(VmError::Access)?;
            let kva = phys_to_virt(pt::pte_addr(pte) + va % PAGE_SIZE);
            f(kva, done, n);
            done += n;
            va += n;
        }
        Ok(())
    }

    // --- Interne --------------------------------------------------------------------------------

    fn find_vma(inner: &Inner, addr: usize) -> Option<&Vma> {
        inner.vmas.range(..=addr).next_back().map(|(_, v)| v).filter(|v| addr < v.end)
    }

    fn is_free(inner: &Inner, start: usize, end: usize) -> bool {
        inner.vmas.range(..end).next_back().map_or(true, |(_, v)| v.end <= start)
    }

    fn find_gap_top_down(inner: &Inner, len: usize) -> Option<usize> {
        let mut top = inner.mmap_top;
        for (_, v) in inner.vmas.range(..inner.mmap_top).rev() {
            if v.end <= top && top - v.end >= len {
                return Some(top - len);
            }
            top = top.min(v.start);
        }
        if top >= MMAP_MIN_ADDR + len { Some(top - len) } else { None }
    }

    /// Coupe la zone qui contient `addr` (strictement) en deux.
    fn split_at(inner: &mut Inner, addr: usize) {
        let Some((&k, v)) = inner.vmas.range(..addr).next_back() else { return };
        if v.end <= addr {
            return;
        }
        let mut right = v.clone();
        right.start = addr;
        if let Backing::Shared { pgoff, .. } = &mut right.backing {
            *pgoff += (addr - k) / PAGE_SIZE;
        }
        inner.vmas.get_mut(&k).unwrap().end = addr;
        inner.vmas.insert(addr, right);
    }

    /// Retire `[start, end)` : zones coupées, pages rendues (après invalidation), tables P1
    /// vidées rendues aussi.
    fn unmap_locked(&self, inner: &mut Inner, fl: &mut Flush, start: usize, end: usize) {
        Self::split_at(inner, start);
        Self::split_at(inner, end);
        let keys: Vec<usize> = inner.vmas.range(start..end).map(|(k, _)| *k).collect();
        if keys.is_empty() {
            return;
        }
        for k in keys {
            inner.vmas.remove(&k);
        }
        let mut freed = 0;
        let mut released = 0;
        // Une table P1 n'est rendue en bloc que si aucune zone restante ne la touche.
        let lo = start & !(pt::P1_SPAN - 1);
        let hi = (end + pt::P1_SPAN - 1) & !(pt::P1_SPAN - 1);
        let whole_lo = if Self::is_free(inner, lo, start) { lo } else { start };
        let whole_hi = if Self::is_free(inner, end, hi.min(USER_END)) { hi.min(USER_END) } else { end };
        // SAFETY : tables de cet espace, sous le verrou.
        let r = unsafe {
            pt::for_each_present_mut(self.space.p4_pfn(), whole_lo, whole_hi, pt::WholeTable::Release, fl, |n| released += n, |fl, va, pte| {
                fl.put_page(pt::pte_pfn(*pte));
                *pte = 0;
                fl.page(va);
                freed += 1;
            })
        };
        if r.is_none() {
            log::error!("uvm : memoire insuffisante pendant un retrait de [{:#x}, {:#x})", start, end);
        }
        let gone = freed + released;
        inner.rss -= gone.min(inner.rss);
    }

    fn fault_locked(&self, inner: &mut Inner, fl: &mut Flush, addr: usize, write: bool, exec: bool) -> Result<(), VmError> {
        if addr >= USER_END {
            return Err(VmError::NoMapping);
        }
        let vma = Self::find_vma(inner, addr).ok_or(VmError::NoMapping)?;
        if (write && !vma.prot.write()) || (exec && !vma.prot.exec()) || !vma.prot.read() {
            return Err(VmError::Access);
        }
        let prot = vma.prot;
        let va = page_down(addr);
        // SAFETY : tables de cet espace, sous le verrou ; `walk_create` rend la table P1 privée.
        let pte = unsafe { pt::walk_create(self.space.p4_pfn(), va, fl) }.ok_or(VmError::NoMemory)?;
        // SAFETY : entrée de cet espace, sous le verrou.
        unsafe {
            let cur = *pte;
            if cur & PTE_PRESENT != 0 {
                if write && cur & PTE_WRITABLE == 0 && cur & pt::PTE_SHARED_MAP == 0 {
                    // Copie à l'écriture : réutilisation sur place si la page n'a plus d'autre
                    // propriétaire, copie sinon.
                    let old = pt::pte_pfn(cur);
                    if crate::refs::is_shared(old) {
                        let new = frame_allocator::alloc_raw_frames(1).ok_or(VmError::NoMemory)?;
                        core::ptr::copy_nonoverlapping(
                            phys_to_virt(old * PAGE_SIZE) as *const u8,
                            phys_to_virt(new * PAGE_SIZE) as *mut u8,
                            PAGE_SIZE,
                        );
                        *pte = pt::leaf(new * PAGE_SIZE, prot, false);
                        fl.page(va);
                        fl.put_page(old);
                    } else {
                        *pte = pt::leaf(old * PAGE_SIZE, prot, false);
                        fl.upgrade(va);
                    }
                } else {
                    // Déjà présente : droits à jour (TLB périmée ou montée de droits).
                    *pte = pt::protect(cur, prot);
                    fl.upgrade(va);
                    // Page d'un fichier projeté en partage, désormais inscriptible : à réécrire.
                    if write {
                        if let Backing::Shared { obj, pgoff } = &vma.backing {
                            obj.mark_dirty(pgoff + (va - vma.start) / PAGE_SIZE);
                        }
                    }
                }
                return Ok(());
            }
        }
        let (pfn, shared) = match &vma.backing {
            Backing::Anon => {
                let (pfn, zeroed) = frame_allocator::alloc_raw_frame_prefer_zeroed().ok_or(VmError::NoMemory)?;
                if !zeroed {
                    unsafe { core::ptr::write_bytes(phys_to_virt(pfn * PAGE_SIZE) as *mut u8, 0, PAGE_SIZE) };
                }
                (pfn, false)
            }
            Backing::File { src, offset, file_va, filesz } if !write && Self::cacheable(va, src.len(), *offset, *file_va, *filesz) => {
                // Page entière du fichier : la frame du cache, en lecture seule (copie à
                // l'écriture par le compteur de références, le cache en tenant une).
                let index = (offset + (va - file_va)) / PAGE_SIZE;
                match src.cached_page(index) {
                    Some(pfn) => {
                        unsafe { *pte = pt::leaf(pfn * PAGE_SIZE, Prot(prot.0 & !Prot::WRITE), false) };
                        inner.rss += 1;
                        return Ok(());
                    }
                    None => Self::copy_file_page(src.as_ref(), va, *offset, *file_va, *filesz)?,
                }
            }
            Backing::File { src, offset, file_va, filesz } => Self::copy_file_page(src.as_ref(), va, *offset, *file_va, *filesz)?,
            Backing::Shared { obj, pgoff } => {
                let index = pgoff + (va - vma.start) / PAGE_SIZE;
                (obj.get_page_for(index, prot.write()).ok_or(VmError::NoMemory)?, true)
            }
        };
        unsafe { *pte = pt::leaf(pfn * PAGE_SIZE, prot, shared) };
        inner.rss += 1;
        Ok(())
    }

    /// La page `va` d'une projection de fichier peut-elle être celle du cache : alignée sur une
    /// page du fichier et entièrement faite d'octets du fichier (ou de la fin du fichier, nulle
    /// au-delà). Une page qui mêle fichier et `.bss` est copiée.
    fn cacheable(va: usize, flen: usize, offset: usize, file_va: usize, filesz: usize) -> bool {
        va >= file_va && (offset + (va - file_va)) % PAGE_SIZE == 0 && (va + PAGE_SIZE <= file_va + filesz || offset + filesz >= flen)
    }

    /// Page privée copiée du fichier (le reste à zéro).
    fn copy_file_page(src: &dyn PageSource, va: usize, offset: usize, file_va: usize, filesz: usize) -> Result<(usize, bool), VmError> {
        let pfn = frame_allocator::alloc_raw_frames(1).ok_or(VmError::NoMemory)?;
        // SAFETY : frame neuve, accessible par la physmap.
        let page = unsafe { core::slice::from_raw_parts_mut(phys_to_virt(pfn * PAGE_SIZE) as *mut u8, PAGE_SIZE) };
        page.fill(0);
        let lo = va.max(file_va);
        let hi = (va + PAGE_SIZE).min(file_va + filesz);
        if lo < hi {
            let foff = offset + (lo - file_va);
            let avail = src.len().saturating_sub(foff).min(hi - lo);
            if avail > 0 {
                src.read_at(foff, &mut page[lo - va..lo - va + avail]);
            }
        }
        Ok((pfn, false))
    }
}

impl Drop for Mm {
    /// Rend toutes les pages (chaque table P1 perd la référence de cet espace) ; les tables
    /// P4/P3/P2 partent avec l'[`AddressSpace`].
    fn drop(&mut self) {
        let inner = self.inner.lock();
        // SAFETY : plus aucun fil n'utilise cet espace (le `Mm` n'a plus de propriétaire).
        unsafe { pt::release_all(self.space.p4_pfn()) };
        drop(inner);
        // Les cœurs en TLB paresseuse gardent le PCID : génération avancée pour qu'ils le
        // vident s'ils rechargeaient cet espace (ils ne le peuvent plus, l'identifiant est unique).
        self.space.bump_tlb_gen();
    }
}

/// Itère les zones (diagnostic, `/proc/self/maps`).
pub fn for_each_vma(mm: &Mm, mut f: impl FnMut(&Vma)) {
    let inner = mm.inner.lock();
    for v in inner.vmas.values() {
        f(v);
    }
}
