//! Chargeur ELF x86_64 et i386 : exécutables statiques (`ET_EXEC`), position-indépendants
//! (`ET_DYN`, PIE et `static-pie`) et interpréteur de programme (`PT_INTERP`, ex. `ld-musl`,
//! `ld-linux`). Un ELF32 `EM_386` donne un processus 32 bits (mode compatible, espace borné
//! sous 4 Gio, PIE à `0x400000` comme `ELF_ET_DYN_BASE` des tâches ia32 de Linux).
//!
//! Comme Linux (`fs/binfmt_elf.c`) : chaque `PT_LOAD` devient une zone privée projetée depuis le
//! fichier, la première page commençant à `page_down(p_offset)` (les octets du fichier qui
//! précèdent le segment sont visibles), le reste après `p_filesz` est à zéro (`.bss`) ; un segment
//! qui recouvre la page d'un autre la remplace. Le PIE est placé à `ELF_ET_DYN_BASE` décalé au
//! hasard (28 bits de pages), l'interpréteur dans la zone `mmap`, dans une réservation de sa
//! taille totale (les trous restent `PROT_NONE`, comme une projection unique).

use alloc::{string::String, sync::Arc, vec::Vec};
use kernel_config::memory::PAGE_SIZE;
use uvm::{page_down, page_up, Backing, Mm, PageSource, Prot, Vma, MMAP_MIN_ADDR, USER_END};

const PT_LOAD: u32 = 1;
const PT_INTERP: u32 = 3;
const PT_PHDR: u32 = 6;
const PT_GNU_STACK: u32 = 0x6474_e551;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const EM_X86_64: u16 = 62;
const EM_386: u16 = 3;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;
/// Base des PIE (2/3 de l'espace utilisateur, comme `ELF_ET_DYN_BASE` de Linux x86_64).
const ELF_ET_DYN_BASE: usize = 0x5555_5555_4000;
/// Aléa d'adresse du PIE, en bits de pages (`mmap_rnd_bits` par défaut de Linux 64 bits).
const PIE_RANDOM_BITS: u32 = 28;
/// Base et aléa des PIE 32 bits (`ELF_ET_DYN_BASE` ia32, `mmap_rnd_compat_bits` = 8).
const ELF_ET_DYN_BASE32: usize = 0x40_0000;
const PIE_RANDOM_BITS32: u32 = 8;

#[derive(Clone, Copy, Debug)]
pub struct Phdr {
    pub p_type: u32,
    pub p_flags: u32,
    pub p_offset: u64,
    pub p_vaddr: u64,
    pub p_filesz: u64,
    pub p_memsz: u64,
    pub p_align: u64,
}

/// En-têtes d'un fichier ELF validé.
pub struct Image {
    /// ELF32 `EM_386` (processus 32 bits).
    pub is32: bool,
    pub etype: u16,
    pub entry: u64,
    pub phoff: u64,
    pub phentsize: u16,
    pub phnum: u16,
    pub phdrs: Vec<Phdr>,
}

#[inline]
fn rd<const N: usize>(b: &[u8], off: usize) -> Option<[u8; N]> {
    b.get(off..off + N)?.try_into().ok()
}
#[inline]
fn u16_at(b: &[u8], off: usize) -> Option<u16> { rd::<2>(b, off).map(u16::from_le_bytes) }
#[inline]
fn u32_at(b: &[u8], off: usize) -> Option<u32> { rd::<4>(b, off).map(u32::from_le_bytes) }
#[inline]
fn u64_at(b: &[u8], off: usize) -> Option<u64> { rd::<8>(b, off).map(u64::from_le_bytes) }

impl Image {
    /// Analyse les en-têtes contenus dans `b` (début du fichier, au moins la table des
    /// segments) ; `file_len` : taille du fichier entier.
    pub fn parse(b: &[u8], file_len: usize) -> Result<Image, &'static str> {
        if b.len() < 52 || &b[0..4] != b"\x7fELF" {
            return Err("pas un fichier ELF");
        }
        let is32 = match (b[4], b[5]) {
            (2, 1) => false,
            (1, 1) => true,
            _ => return Err("ELF ni 64 ni 32 bits petit-boutiste"),
        };
        let etype = u16_at(b, 16).ok_or("en-tete tronque")?;
        let machine = u16_at(b, 18).ok_or("en-tete tronque")?;
        if machine != if is32 { EM_386 } else { EM_X86_64 } {
            return Err("architecture ni x86_64 ni i386");
        }
        if etype != ET_EXEC && etype != ET_DYN {
            return Err("ni executable ni objet partage");
        }
        let (entry, phoff, phentsize, phnum, want_ent) = if is32 {
            (
                u32_at(b, 24).ok_or("en-tete tronque")? as u64,
                u32_at(b, 28).ok_or("en-tete tronque")? as u64,
                u16_at(b, 42).ok_or("en-tete tronque")?,
                u16_at(b, 44).ok_or("en-tete tronque")?,
                32,
            )
        } else {
            if b.len() < 64 {
                return Err("en-tete tronque");
            }
            (
                u64_at(b, 24).ok_or("en-tete tronque")?,
                u64_at(b, 32).ok_or("en-tete tronque")?,
                u16_at(b, 54).ok_or("en-tete tronque")?,
                u16_at(b, 56).ok_or("en-tete tronque")?,
                56,
            )
        };
        if phentsize as usize != want_ent || phnum == 0 || phnum > 128 {
            return Err("table des segments invalide");
        }
        let mut phdrs = Vec::with_capacity(phnum as usize);
        for i in 0..phnum as usize {
            let o = phoff as usize + i * want_ent;
            let ph = if is32 {
                Phdr {
                    p_type: u32_at(b, o).ok_or("segment tronque")?,
                    p_offset: u32_at(b, o + 4).ok_or("segment tronque")? as u64,
                    p_vaddr: u32_at(b, o + 8).ok_or("segment tronque")? as u64,
                    p_filesz: u32_at(b, o + 16).ok_or("segment tronque")? as u64,
                    p_memsz: u32_at(b, o + 20).ok_or("segment tronque")? as u64,
                    p_flags: u32_at(b, o + 24).ok_or("segment tronque")?,
                    p_align: u32_at(b, o + 28).ok_or("segment tronque")? as u64,
                }
            } else {
                Phdr {
                    p_type: u32_at(b, o).ok_or("segment tronque")?,
                    p_flags: u32_at(b, o + 4).ok_or("segment tronque")?,
                    p_offset: u64_at(b, o + 8).ok_or("segment tronque")?,
                    p_vaddr: u64_at(b, o + 16).ok_or("segment tronque")?,
                    p_filesz: u64_at(b, o + 32).ok_or("segment tronque")?,
                    p_memsz: u64_at(b, o + 40).ok_or("segment tronque")?,
                    p_align: u64_at(b, o + 48).ok_or("segment tronque")?,
                }
            };
            if is32 && ph.p_vaddr.checked_add(ph.p_memsz).map_or(true, |e| e > 1 << 32) {
                return Err("segment 32 bits au-dela de 4 Gio");
            }
            if ph.p_type == PT_LOAD {
                if ph.p_filesz > ph.p_memsz {
                    return Err("segment : taille fichier > taille memoire");
                }
                if ph.p_offset.checked_add(ph.p_filesz).map_or(true, |e| e as usize > file_len) {
                    return Err("segment hors du fichier");
                }
                if ph.p_offset as usize % PAGE_SIZE != ph.p_vaddr as usize % PAGE_SIZE {
                    return Err("segment mal aligne (offset/adresse)");
                }
            }
            phdrs.push(ph);
        }
        if !phdrs.iter().any(|p| p.p_type == PT_LOAD) {
            return Err("aucun segment chargeable");
        }
        Ok(Image { is32, etype, entry, phoff, phentsize, phnum, phdrs })
    }

    /// Chemin de l'interpréteur (`PT_INTERP`), lu dans le fichier.
    pub fn interp(&self, src: &dyn PageSource) -> Option<String> {
        let ph = self.phdrs.iter().find(|p| p.p_type == PT_INTERP)?;
        let (off, len) = (ph.p_offset as usize, (ph.p_filesz as usize).min(4096));
        if off.checked_add(len)? > src.len() {
            return None;
        }
        let mut raw = alloc::vec![0u8; len];
        src.read_at(off, &mut raw);
        let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
        core::str::from_utf8(&raw[..end]).ok().map(String::from)
    }

    /// Octets d'en-tête nécessaires (jusqu'à la fin de la table des segments).
    pub fn header_span(b: &[u8]) -> Option<usize> {
        if *b.get(4)? == 1 {
            let phoff = u32_at(b, 28)? as usize;
            let phnum = u16_at(b, 44)? as usize;
            return phoff.checked_add(phnum.checked_mul(32)?);
        }
        let phoff = u64_at(b, 32)? as usize;
        let phnum = u16_at(b, 56)? as usize;
        phoff.checked_add(phnum.checked_mul(56)?)
    }

    fn loads(&self) -> impl Iterator<Item = &Phdr> {
        self.phdrs.iter().filter(|p| p.p_type == PT_LOAD && p.p_memsz > 0)
    }

    /// Étendue `[min, max)` des segments, alignée sur la page.
    fn span(&self) -> (usize, usize) {
        let lo = self.loads().map(|p| page_down(p.p_vaddr as usize)).min().unwrap_or(0);
        let hi = self.loads().map(|p| page_up((p.p_vaddr + p.p_memsz) as usize)).max().unwrap_or(0);
        (lo, hi)
    }

    fn max_align(&self) -> usize {
        self.loads()
            .map(|p| p.p_align as usize)
            .filter(|a| a.is_power_of_two())
            .max()
            .unwrap_or(PAGE_SIZE)
            .clamp(PAGE_SIZE, 1 << 30)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Main,
    Interpreter,
}

/// Résultat du chargement d'une image.
pub struct Loaded {
    /// Décalage appliqué aux adresses du fichier.
    pub bias: usize,
    pub entry: usize,
    /// Adresse de la table des segments en mémoire (`AT_PHDR`).
    pub phdr: usize,
    pub phent: usize,
    pub phnum: usize,
    /// Fin du dernier segment (début du tas).
    pub brk_base: usize,
    /// `PT_GNU_STACK` exécutable.
    pub exec_stack: bool,
}

fn prot_of(flags: u32) -> Prot {
    let mut p = 0;
    if flags & PF_R != 0 { p |= Prot::READ; }
    if flags & PF_W != 0 { p |= Prot::WRITE; }
    if flags & PF_X != 0 { p |= Prot::EXEC; }
    Prot(p)
}

/// Charge `img` (contenu lu dans `src`, en général le cache de pages du fichier) dans `mm`.
/// `name` : chemin du fichier, tel que `/proc/<pid>/maps` le montre.
pub fn load(mm: &Mm, img: &Image, src: Arc<dyn PageSource>, role: Role, name: uvm::VmaName) -> Result<Loaded, &'static str> {
    let (lo, hi) = img.span();
    if hi <= lo {
        return Err("image vide");
    }
    let size = hi - lo;
    let bias = match (img.etype, role) {
        (ET_EXEC, _) => {
            if lo < MMAP_MIN_ADDR || hi > USER_END.min(mm.task_size()) {
                return Err("adresses de l'executable hors de l'espace utilisateur");
            }
            0
        }
        (_, Role::Main) => {
            let align = img.max_align();
            let (base, bits) = if img.is32 { (ELF_ET_DYN_BASE32, PIE_RANDOM_BITS32) } else { (ELF_ET_DYN_BASE, PIE_RANDOM_BITS) };
            let base = (base + uvm::rand::random_pages(bits)) & !(align - 1);
            base.checked_sub(lo).ok_or("base PIE invalide")?
        }
        (_, Role::Interpreter) => {
            // Réservation de toute l'étendue (alignée), puis segments posés dedans.
            let align = img.max_align();
            let start = mm
                .mmap(0, size + align, Prot::NONE, Backing::Anon, false, "interp")
                .map_err(|_| "espace insuffisant pour l'interpreteur")?;
            let aligned = (start + align - 1) & !(align - 1);
            aligned - lo
        }
    };

    for ph in img.loads() {
        let va = bias + ph.p_vaddr as usize;
        let start = page_down(va);
        let end = page_up(va + ph.p_memsz as usize);
        let lead = va - start;
        let offset = (ph.p_offset as usize).checked_sub(lead).ok_or("segment mal aligne")?;
        mm.map_fixed(Vma {
            start,
            end,
            prot: prot_of(ph.p_flags),
            backing: Backing::File { src: src.clone(), offset, file_va: start, filesz: lead + ph.p_filesz as usize },
            name: name.clone(),
            may_write: true,
        })
        .map_err(|_| "projection d'un segment impossible")?;
    }

    // Table des segments en mémoire : `PT_PHDR`, sinon le segment qui contient `e_phoff`.
    let phdr = img
        .phdrs
        .iter()
        .find(|p| p.p_type == PT_PHDR)
        .map(|p| bias + p.p_vaddr as usize)
        .or_else(|| {
            img.loads()
                .find(|p| p.p_offset <= img.phoff && img.phoff < p.p_offset + p.p_filesz)
                .map(|p| bias + (p.p_vaddr + (img.phoff - p.p_offset)) as usize)
        })
        .unwrap_or(0);
    let exec_stack = img.phdrs.iter().find(|p| p.p_type == PT_GNU_STACK).map_or(false, |p| p.p_flags & PF_X != 0);

    Ok(Loaded {
        bias,
        entry: bias + img.entry as usize,
        phdr,
        phent: img.phentsize as usize,
        phnum: img.phnum as usize,
        brk_base: bias + hi,
        exec_stack,
    })
}
