//! MaiProc — mémoire virtuelle des processus utilisateur.
//!
//! Un [`Mm`] décrit la moitié basse d'un espace d'adresses : ses zones ([`Vma`], arbre ordonné),
//! son tas (`brk`) et sa zone `mmap` (allocation descendante, base aléatoire). Les pages sont
//! créées **à la demande** par [`Mm::handle_fault`] : zéro (anonyme, pris dans le pool pré-zéroé
//! de MaiMM), ou copie privée d'un fichier (segments ELF), complétée de zéros.
//!
//! Les tables de pages et les copies noyau ↔ utilisateur passent par la *physmap* : aucune faute
//! de page n'est jamais prise par le noyau sur une adresse utilisateur (validation par VMA puis
//! pré-faute), aucun `stac`/`clac`, et un espace non chargé (autre processus) se manipule de la
//! même façon que l'espace courant.
//!
//! Invalidation : un retrait ou une réduction de droits invalide l'entrée localement si l'espace
//! est chargé sur ce cœur, et incrémente la génération TLB de l'espace ([`AddressSpace::bump_tlb_gen`]),
//! qui fait vider le PCID des autres cœurs au prochain chargement.

#![no_std]
#![cfg(target_arch = "x86_64")]

extern crate alloc;

mod pt;
pub mod rand;

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use kernel_config::memory::{PAGE_SIZE, USER_SPACE_END};
use memory::paging::address_space::{self, AddressSpace};
use memory::paging::kernel_half::phys_to_virt;
use sync_irq::IrqSafeMutex;

pub use pt::{PTE_PRESENT, PTE_WRITABLE};

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
    /// Copie `dst.len()` octets à partir de `offset` (le fichier est assez long : l'appelant borne).
    fn read_at(&self, offset: usize, dst: &mut [u8]);
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
    /// Pages mises à zéro à la première utilisation.
    Anon,
    /// Copie privée d'un fichier : les octets `[offset, offset + filesz)` du fichier apparaissent à
    /// l'adresse `file_va` (pas forcément alignée) ; le reste de la zone est à zéro (`.bss`).
    File { src: Arc<dyn PageSource>, offset: usize, file_va: usize, filesz: usize },
}

/// Zone de l'espace d'adresses (bornes alignées sur la page).
#[derive(Clone)]
pub struct Vma {
    pub start: usize,
    pub end: usize,
    pub prot: Prot,
    pub backing: Backing,
    /// Nom pour le diagnostic (`[stack]`, `[heap]`, chemin…).
    pub name: &'static str,
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

struct Inner {
    vmas: BTreeMap<usize, Vma>,
    brk_start: usize,
    brk: usize,
    /// Les `mmap` sans adresse sont placés sous cette limite, en descendant.
    mmap_top: usize,
    /// Pages présentes (résidentes).
    rss: usize,
}

/// Mémoire d'un processus.
pub struct Mm {
    space: Arc<AddressSpace>,
    inner: IrqSafeMutex<Inner>,
}

impl Mm {
    /// Espace vide ; `mmap_top` borne la zone `mmap` (voir [`rand::Layout`]).
    pub fn new(mmap_top: usize) -> Result<Arc<Mm>, &'static str> {
        let space = AddressSpace::new()?;
        Ok(Arc::new(Mm {
            space,
            inner: IrqSafeMutex::new(Inner {
                vmas: BTreeMap::new(),
                brk_start: 0,
                brk: 0,
                mmap_top: page_down(mmap_top),
                rss: 0,
            }),
        }))
    }

    /// Espace d'adresses matériel (racine P4, PCID).
    pub fn space(&self) -> &Arc<AddressSpace> { &self.space }

    /// Pages résidentes.
    pub fn rss(&self) -> usize { self.inner.lock().rss }

    /// Pose une zone à une adresse fixe (remplace ce qui s'y trouvait).
    pub fn map_fixed(&self, vma: Vma) -> Result<(), VmError> {
        if vma.start % PAGE_SIZE != 0 || vma.end % PAGE_SIZE != 0 || vma.start >= vma.end
            || vma.start < MMAP_MIN_ADDR || vma.end > USER_END
        {
            return Err(VmError::Invalid);
        }
        let mut inner = self.inner.lock();
        self.unmap_locked(&mut inner, vma.start, vma.end);
        inner.vmas.insert(vma.start, vma);
        Ok(())
    }

    /// `mmap` : renvoie l'adresse de la zone créée. Sans `fixed`, `hint` est une suggestion.
    pub fn mmap(&self, hint: usize, len: usize, prot: Prot, backing: Backing, fixed: bool, name: &'static str) -> Result<usize, VmError> {
        if len == 0 {
            return Err(VmError::Invalid);
        }
        let len = page_up(len);
        let mut inner = self.inner.lock();
        let start = if fixed {
            if hint % PAGE_SIZE != 0 || hint < MMAP_MIN_ADDR || hint.checked_add(len).map_or(true, |e| e > USER_END) {
                return Err(VmError::Invalid);
            }
            self.unmap_locked(&mut inner, hint, hint + len);
            hint
        } else {
            let h = page_down(hint);
            if h >= MMAP_MIN_ADDR && h.checked_add(len).map_or(false, |e| e <= USER_END) && Self::is_free(&inner, h, h + len) {
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
        inner.vmas.insert(start, Vma { start, end: start + len, prot, backing, name });
        Ok(start)
    }

    /// `munmap`.
    pub fn munmap(&self, start: usize, len: usize) -> Result<(), VmError> {
        if start % PAGE_SIZE != 0 || len == 0 {
            return Err(VmError::Invalid);
        }
        let end = start.checked_add(page_up(len)).ok_or(VmError::Invalid)?.min(USER_END);
        let mut inner = self.inner.lock();
        self.unmap_locked(&mut inner, start, end);
        Ok(())
    }

    /// `mprotect`.
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
        Self::split_at(&mut inner, start);
        Self::split_at(&mut inner, end);
        let keys: Vec<usize> = inner.vmas.range(start..end).map(|(k, _)| *k).collect();
        for k in keys {
            if let Some(v) = inner.vmas.get_mut(&k) {
                v.prot = prot;
            }
        }
        // Entrées présentes : nouveaux droits (une restriction invalide la TLB).
        let space = &self.space;
        let current = space.is_current();
        // SAFETY : tables de cet espace, sous le verrou.
        unsafe {
            pt::for_each_present(space.p4_pfn(), start, end, |va, pte| {
                *pte = pt::with_prot(*pte, prot);
                if current { address_space::flush_user_page_local(va); }
            });
        }
        space.bump_tlb_gen();
        Ok(())
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
        if new_brk < start || new_brk > USER_END {
            return cur;
        }
        let old_end = page_up(cur);
        let new_end = page_up(new_brk);
        if new_end > old_end {
            if !Self::is_free(&inner, old_end, new_end) {
                return cur;
            }
            // Agrandit la zone du tas (ou la crée).
            let heap_vma = inner.vmas.range_mut(..old_end).next_back().filter(|(_, v)| v.end == old_end && v.name == "[heap]");
            match heap_vma {
                Some((_, v)) => v.end = new_end,
                None => {
                    inner.vmas.insert(old_end, Vma { start: old_end, end: new_end, prot: Prot::RW, backing: Backing::Anon, name: "[heap]" });
                }
            }
        } else if new_end < old_end {
            self.unmap_locked(&mut inner, new_end, old_end);
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
    /// prochaine faute redonne une page zéro (anonyme) ou le contenu d'origine (copie de fichier).
    pub fn discard(&self, start: usize, len: usize) -> Result<(), VmError> {
        if start % PAGE_SIZE != 0 {
            return Err(VmError::Invalid);
        }
        let end = start.checked_add(page_up(len)).ok_or(VmError::Invalid)?.min(USER_END);
        let mut inner = self.inner.lock();
        let current = self.space.is_current();
        let mut freed = 0;
        // SAFETY : tables de cet espace, sous le verrou.
        unsafe {
            pt::for_each_present(self.space.p4_pfn(), start, end, |va, pte| {
                let pfn = pt::pte_addr(*pte) / PAGE_SIZE;
                *pte = 0;
                if current { address_space::flush_user_page_local(va); }
                frame_allocator::free_raw_frames(pfn, 1);
                freed += 1;
            });
        }
        inner.rss -= freed.min(inner.rss);
        if freed > 0 {
            self.space.bump_tlb_gen();
        }
        Ok(())
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
        // La plage doit tenir dans une seule zone.
        let vma = Self::find_vma(&inner, old).ok_or(VmError::NoMapping)?.clone();
        if old_end > vma.end {
            return Err(VmError::NoMapping);
        }
        if fixed.is_none() {
            if new_len <= old_len {
                self.unmap_locked(&mut inner, old + new_len, old_end);
                return Ok(old);
            }
            let new_end = old.checked_add(new_len).ok_or(VmError::NoMemory)?;
            if old_end == vma.end && new_end <= USER_END && Self::is_free(&inner, old_end, new_end) {
                inner.vmas.get_mut(&vma.start).unwrap().end = new_end;
                return Ok(old);
            }
            if !may_move {
                return Err(VmError::NoMemory);
            }
        }
        // Une réduction déplacée ne garde que le début de la plage.
        let old_end = if new_len < old_len {
            self.unmap_locked(&mut inner, old + new_len, old_end);
            old + new_len
        } else {
            old_end
        };
        let dest = match fixed {
            Some(d) => {
                self.unmap_locked(&mut inner, d, d + new_len);
                d
            }
            None => Self::find_gap_top_down(&inner, new_len).ok_or(VmError::NoMemory)?,
        };
        // Déplacement des entrées présentes.
        let p4 = self.space.p4_pfn();
        let current = self.space.is_current();
        let mut moves: Vec<(usize, u64)> = Vec::new();
        // SAFETY : tables de cet espace, sous le verrou.
        unsafe {
            pt::for_each_present(p4, old, old_end, |va, pte| {
                moves.push((va, *pte));
                *pte = 0;
                if current { address_space::flush_user_page_local(va); }
            });
            for (va, entry) in moves {
                let slot = pt::walk_create(p4, dest + (va - old)).ok_or(VmError::NoMemory)?;
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
        self.space.bump_tlb_gen();
        Ok(dest)
    }

    /// Traite une faute de page à `addr`. `Ok` : l'accès peut être rejoué.
    pub fn handle_fault(&self, addr: usize, write: bool, exec: bool) -> Result<(), VmError> {
        let mut inner = self.inner.lock();
        self.fault_locked(&mut inner, addr, write, exec)
    }

    /// Projette d'avance les pages de `[start, start + len)` (pile initiale, `MAP_POPULATE`).
    pub fn populate(&self, start: usize, len: usize, write: bool) -> Result<(), VmError> {
        let mut inner = self.inner.lock();
        let mut va = page_down(start);
        while va < start + len {
            self.fault_locked(&mut inner, va, write, false)?;
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
        let mut va = uaddr;
        let mut done = 0;
        while va < end {
            let n = (PAGE_SIZE - va % PAGE_SIZE).min(end - va);
            // SAFETY : tables de cet espace, sous le verrou.
            let mut pte = unsafe { pt::lookup(self.space.p4_pfn(), va) };
            let ok = |p: u64| p & PTE_PRESENT != 0 && (!write || p & PTE_WRITABLE != 0);
            if !pte.map_or(false, ok) {
                self.fault_locked(&mut inner, va, write, false)?;
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
        inner.vmas.get_mut(&k).unwrap().end = addr;
        inner.vmas.insert(addr, right);
    }

    /// Retire `[start, end)` : zones coupées, pages rendues, TLB invalidée.
    fn unmap_locked(&self, inner: &mut Inner, start: usize, end: usize) {
        Self::split_at(inner, start);
        Self::split_at(inner, end);
        let keys: Vec<usize> = inner.vmas.range(start..end).map(|(k, _)| *k).collect();
        if keys.is_empty() {
            return;
        }
        for k in keys {
            inner.vmas.remove(&k);
        }
        let current = self.space.is_current();
        let mut freed = 0;
        // SAFETY : tables de cet espace, sous le verrou ; les frames n'appartiennent qu'à lui.
        unsafe {
            pt::for_each_present(self.space.p4_pfn(), start, end, |va, pte| {
                let pfn = pt::pte_addr(*pte) / PAGE_SIZE;
                *pte = 0;
                if current { address_space::flush_user_page_local(va); }
                frame_allocator::free_raw_frames(pfn, 1);
                freed += 1;
            });
        }
        inner.rss -= freed.min(inner.rss);
        self.space.bump_tlb_gen();
    }

    fn fault_locked(&self, inner: &mut Inner, addr: usize, write: bool, exec: bool) -> Result<(), VmError> {
        if addr >= USER_END {
            return Err(VmError::NoMapping);
        }
        let vma = Self::find_vma(inner, addr).ok_or(VmError::NoMapping)?.clone();
        if (write && !vma.prot.write()) || (exec && !vma.prot.exec()) || !vma.prot.read() {
            return Err(VmError::Access);
        }
        let va = page_down(addr);
        // SAFETY : tables de cet espace, sous le verrou.
        let pte = unsafe { pt::walk_create(self.space.p4_pfn(), va) }.ok_or(VmError::NoMemory)?;
        unsafe {
            if *pte & PTE_PRESENT != 0 {
                // Déjà présente : droits à jour (TLB périmée ou montée de droits après mprotect).
                *pte = pt::with_prot(*pte, vma.prot);
                if self.space.is_current() { address_space::flush_user_page_local(va); }
                return Ok(());
            }
        }
        let pfn = match &vma.backing {
            Backing::Anon => {
                let (pfn, zeroed) = frame_allocator::alloc_raw_frame_prefer_zeroed().ok_or(VmError::NoMemory)?;
                if !zeroed {
                    unsafe { core::ptr::write_bytes(phys_to_virt(pfn * PAGE_SIZE) as *mut u8, 0, PAGE_SIZE) };
                }
                pfn
            }
            Backing::File { src, offset, file_va, filesz } => {
                let pfn = frame_allocator::alloc_raw_frames(1).ok_or(VmError::NoMemory)?;
                let page = unsafe { core::slice::from_raw_parts_mut(phys_to_virt(pfn * PAGE_SIZE) as *mut u8, PAGE_SIZE) };
                page.fill(0);
                let lo = va.max(*file_va);
                let hi = (va + PAGE_SIZE).min(file_va + filesz);
                if lo < hi {
                    let foff = offset + (lo - file_va);
                    let avail = src.len().saturating_sub(foff).min(hi - lo);
                    if avail > 0 {
                        src.read_at(foff, &mut page[lo - va..lo - va + avail]);
                    }
                }
                pfn
            }
        };
        unsafe { *pte = pt::leaf(pfn * PAGE_SIZE, vma.prot) };
        inner.rss += 1;
        Ok(())
    }
}

impl Drop for Mm {
    /// Rend toutes les pages ; les tables partent avec l'[`AddressSpace`].
    fn drop(&mut self) {
        let mut inner = self.inner.lock();
        self.unmap_locked(&mut inner, MMAP_MIN_ADDR.min(0), USER_END);
        // `unmap_locked` ne parcourt les tables que s'il y avait des zones : on termine par un
        // balayage complet pour ne rien laisser.
        unsafe {
            pt::for_each_present(self.space.p4_pfn(), 0, USER_END, |_, pte| {
                let pfn = pt::pte_addr(*pte) / PAGE_SIZE;
                *pte = 0;
                frame_allocator::free_raw_frames(pfn, 1);
            });
        }
    }
}

/// Itère les zones (diagnostic, `/proc/self/maps`).
pub fn for_each_vma(mm: &Mm, mut f: impl FnMut(&Vma)) {
    let inner = mm.inner.lock();
    for v in inner.vmas.values() {
        f(v);
    }
}
