//! vDSO : `clock_gettime`, `gettimeofday`, `time` et `getcpu` exécutés en mode utilisateur,
//! sans entrer dans le noyau (environ 20 ns au lieu d'un aller-retour `syscall`).
//!
//! Deux pages projetées dans chaque processus, comme Linux :
//! - `[vvar]` (lecture seule) : données de l'horloge, protégées par un compteur de séquence
//!   (*seqlock*) pour de futures mises à jour (`settimeofday`, NTP) ;
//! - `[vdso]` (lecture et exécution) : un objet ELF partagé minimal (`ET_DYN`, table `DT_HASH`,
//!   symboles `__vdso_*` et leurs alias), que la glibc et musl trouvent par `AT_SYSINFO_EHDR`.
//!
//! Le code est écrit en assembleur indépendant de la position et copié tel quel : il lit `[vvar]`
//! par adressage relatif à `rip` (la page de données est juste avant celle du code). Le calcul
//! reproduit exactement celui du noyau (`time::Instant`) : ns = TSC × période (fs) / 10⁶, en
//! 128 bits, si bien qu'appel système et vDSO donnent la même heure. Si l'horloge monotone du
//! noyau n'est pas le TSC (HPET), `mode = 0` et chaque fonction se rabat sur l'appel système.
//!
//! Les deux pages sont des objets partagés **scellés** ([`uvm::ShmObject::sealed_from_frames`]) :
//! communes à tous les processus, jamais inscriptibles (`mprotect` refuse `PROT_WRITE`).

use alloc::{sync::Arc, vec::Vec};
use kernel_config::memory::PAGE_SIZE;
use log::{error, info};
use memory::paging::kernel_half::phys_to_virt;
use uvm::{Backing, Mm, Prot, ShmObject};

/// Décalage du code dans la page `[vdso]` (en-têtes ELF et tables avant).
const TEXT_OFF: usize = 0x800;

/// Disposition de `[vvar]` (doit suivre les décalages de l'assembleur : base − 0x1800).
mod vvar {
    pub const SEQ: usize = 0;
    pub const MODE: usize = 4;
    pub const PERIOD_FS: usize = 8;
    pub const EPOCH_SEC: usize = 16;
    pub const EPOCH_NSEC: usize = 24;
}

/// Code du vDSO. En tête, une table de décalages (u32) : `clock_gettime`, `gettimeofday`,
/// `time`, `getcpu`, `clock_getres`, fin. Les accès à `[vvar]` sont relatifs à `rip` :
/// `2b − 0x1800` est le début de `[vvar]` une fois le bloc copié en `[vdso] + TEXT_OFF`.
#[naked]
unsafe extern "C" fn vdso_blob() {
    core::arch::asm!(
        "2:",
        ".long 3f - 2b",
        ".long 4f - 2b",
        ".long 5f - 2b",
        ".long 6f - 2b",
        ".long 7f - 2b",
        ".long 8f - 2b",
        // ---- clock_gettime(clockid = edi, ts = rsi) --------------------------------------
        ".p2align 4, 0xcc",
        "3:",
        "cmp edi, 11",
        "ja 39f",
        "mov eax, 1",
        "mov ecx, edi",
        "shl eax, cl",
        // REALTIME 0, MONOTONIC 1, MONOTONIC_RAW 4, *_COARSE 5 6, BOOTTIME 7, TAI 11.
        "test eax, 0x8f3",
        "jz 39f",
        "call 9f",
        "cmp rax, -1",
        "je 39f",
        "xor edx, edx",
        "mov rcx, 1000000000",
        "div rcx",
        "cmp edi, 1",
        "je 34f",
        "cmp edi, 4",
        "je 34f",
        "cmp edi, 6",
        "je 34f",
        "cmp edi, 7",
        "je 34f",
        "add rax, r8",
        "add rdx, r9",
        "cmp rdx, rcx",
        "jb 34f",
        "sub rdx, rcx",
        "inc rax",
        "34:",
        "mov qword ptr [rsi], rax",
        "mov qword ptr [rsi + 8], rdx",
        "xor eax, eax",
        "ret",
        "39:",
        "mov eax, 228",
        "syscall",
        "ret",
        // ---- gettimeofday(tv = rdi, tz = rsi) ---------------------------------------------
        ".p2align 4, 0xcc",
        "4:",
        "call 9f",
        "cmp rax, -1",
        "je 49f",
        "test rdi, rdi",
        "jz 43f",
        "xor edx, edx",
        "mov rcx, 1000000000",
        "div rcx",
        "add rax, r8",
        "add rdx, r9",
        "cmp rdx, rcx",
        "jb 44f",
        "sub rdx, rcx",
        "inc rax",
        "44:",
        "mov qword ptr [rdi], rax",
        "mov rax, rdx",
        "xor edx, edx",
        "mov ecx, 1000",
        "div rcx",
        "mov qword ptr [rdi + 8], rax",
        "43:",
        "test rsi, rsi",
        "jz 45f",
        "mov qword ptr [rsi], 0",
        "45:",
        "xor eax, eax",
        "ret",
        "49:",
        "mov eax, 96",
        "syscall",
        "ret",
        // ---- time(tloc = rdi) -------------------------------------------------------------
        ".p2align 4, 0xcc",
        "5:",
        "call 9f",
        "cmp rax, -1",
        "je 59f",
        "xor edx, edx",
        "mov rcx, 1000000000",
        "div rcx",
        "add rax, r8",
        "add rdx, r9",
        "cmp rdx, rcx",
        "jb 54f",
        "inc rax",
        "54:",
        "test rdi, rdi",
        "jz 55f",
        "mov qword ptr [rdi], rax",
        "55:",
        "ret",
        "59:",
        "mov eax, 201",
        "syscall",
        "ret",
        // ---- getcpu(cpu = rdi, node = rsi, cache = rdx) : TSC_AUX = identifiant du cœur ----
        ".p2align 4, 0xcc",
        "6:",
        "rdtscp",
        "test rdi, rdi",
        "jz 62f",
        "mov dword ptr [rdi], ecx",
        "62:",
        "test rsi, rsi",
        "jz 63f",
        "mov dword ptr [rsi], 0",
        "63:",
        "xor eax, eax",
        "ret",
        // ---- clock_getres : appel système ---------------------------------------------------
        ".p2align 4, 0xcc",
        "7:",
        "mov eax, 229",
        "syscall",
        "ret",
        // ---- lecture de l'horloge : rax = ns monotones (-1 : repli), r8/r9 = époque --------
        ".p2align 4, 0xcc",
        "9:",
        "22:",
        "mov r10d, dword ptr [rip + 2b - 6144]",
        "test r10d, 1",
        "jnz 23f",
        "mov eax, dword ptr [rip + 2b - 6140]",
        "test eax, eax",
        "jz 24f",
        "lfence",
        "rdtsc",
        "shl rdx, 32",
        "or rax, rdx",
        "mul qword ptr [rip + 2b - 6136]",
        "mov rcx, 1000000",
        "div rcx",
        "mov r8, qword ptr [rip + 2b - 6128]",
        "mov r9, qword ptr [rip + 2b - 6120]",
        "cmp r10d, dword ptr [rip + 2b - 6144]",
        "jne 22b",
        "ret",
        "23:",
        "pause",
        "jmp 22b",
        "24:",
        "mov rax, -1",
        "ret",
        "8:",
        "int3",
        options(noreturn)
    );
}

/// Code du vDSO et décalages de ses fonctions.
fn blob() -> (&'static [u8], [u32; 5]) {
    let start = vdso_blob as usize as *const u8;
    // SAFETY : en-tête de 6 mots écrit par l'assembleur ci-dessus, dans le texte du noyau.
    let hdr = unsafe { core::slice::from_raw_parts(start as *const u32, 6) };
    let len = hdr[5] as usize;
    // SAFETY : `len` octets de code à partir du début du bloc.
    let code = unsafe { core::slice::from_raw_parts(start, len) };
    (code, [hdr[0], hdr[1], hdr[2], hdr[3], hdr[4]])
}

struct Vdso {
    vvar: Arc<ShmObject>,
    text: Arc<ShmObject>,
    /// Page `[vdso]` des processus i386 et adresse de `__kernel_vsyscall` dans la page.
    text32: Option<(Arc<ShmObject>, usize)>,
}

static VDSO: spin::Once<Option<Vdso>> = spin::Once::new();

fn put(b: &mut [u8], off: usize, v: &[u8]) {
    b[off..off + v.len()].copy_from_slice(v);
}

/// Image ELF de la page `[vdso]`.
fn build_image(code: &[u8], offs: [u32; 5]) -> Vec<u8> {
    const NAMES: [&str; 5] = ["clock_gettime", "gettimeofday", "time", "getcpu", "clock_getres"];
    let mut img = alloc::vec![0u8; PAGE_SIZE];
    // --- Table des chaînes ---
    let mut strtab: Vec<u8> = alloc::vec![0];
    let mut add_str = |s: &str| {
        let off = strtab.len() as u32;
        strtab.extend_from_slice(s.as_bytes());
        strtab.push(0);
        off
    };
    let soname = add_str("linux-vdso.so.1");
    let mut syms: Vec<(u32, u8, u64)> = Vec::new();
    for (i, n) in NAMES.iter().enumerate() {
        let value = (TEXT_OFF + offs[i] as usize) as u64;
        syms.push((add_str(&alloc::format!("__vdso_{}", n)), 0x12, value)); // GLOBAL FUNC
        syms.push((add_str(n), 0x22, value)); // WEAK FUNC
    }
    let nsym = syms.len() + 1;
    // --- Disposition ---
    let phoff = 64;
    let phnum = 2;
    let symoff = phoff + phnum * 56;
    let stroff = symoff + nsym * 24;
    let hashoff = (stroff + strtab.len() + 7) & !7;
    let hashsz = 4 * (2 + 1 + nsym);
    let dynoff = (hashoff + hashsz + 7) & !7;
    let dyn_entries: [(u64, u64); 7] = [
        (4, hashoff as u64),          // DT_HASH
        (5, stroff as u64),           // DT_STRTAB
        (6, symoff as u64),           // DT_SYMTAB
        (10, strtab.len() as u64),    // DT_STRSZ
        (11, 24),                     // DT_SYMENT
        (14, soname as u64),          // DT_SONAME
        (0, 0),                       // DT_NULL
    ];
    let dynsz = dyn_entries.len() * 16;
    assert!(dynoff + dynsz <= TEXT_OFF && TEXT_OFF + code.len() <= PAGE_SIZE, "vdso : image trop grande");
    // --- En-tête ELF ---
    put(&mut img, 0, &[0x7f, b'E', b'L', b'F', 2, 1, 1, 0]);
    put(&mut img, 16, &3u16.to_le_bytes()); // ET_DYN
    put(&mut img, 18, &62u16.to_le_bytes()); // EM_X86_64
    put(&mut img, 20, &1u32.to_le_bytes());
    put(&mut img, 32, &(phoff as u64).to_le_bytes());
    put(&mut img, 52, &64u16.to_le_bytes()); // e_ehsize
    put(&mut img, 54, &56u16.to_le_bytes()); // e_phentsize
    put(&mut img, 56, &(phnum as u16).to_le_bytes());
    put(&mut img, 58, &64u16.to_le_bytes()); // e_shentsize
    // --- Segments ---
    let ph = |img: &mut Vec<u8>, i: usize, ty: u32, flags: u32, off: u64, sz: u64, align: u64| {
        let b = phoff + i * 56;
        put(img, b, &ty.to_le_bytes());
        put(img, b + 4, &flags.to_le_bytes());
        put(img, b + 8, &off.to_le_bytes());
        put(img, b + 16, &off.to_le_bytes()); // p_vaddr
        put(img, b + 24, &off.to_le_bytes()); // p_paddr
        put(img, b + 32, &sz.to_le_bytes());
        put(img, b + 40, &sz.to_le_bytes());
        put(img, b + 48, &align.to_le_bytes());
    };
    ph(&mut img, 0, 1, 5, 0, PAGE_SIZE as u64, PAGE_SIZE as u64); // PT_LOAD R+X
    ph(&mut img, 1, 2, 4, dynoff as u64, dynsz as u64, 8); // PT_DYNAMIC
    // --- Symboles (le premier est nul) ---
    for (i, (name, info, value)) in syms.iter().enumerate() {
        let b = symoff + (i + 1) * 24;
        put(&mut img, b, &name.to_le_bytes());
        img[b + 4] = *info;
        put(&mut img, b + 6, &1u16.to_le_bytes()); // st_shndx (défini)
        put(&mut img, b + 8, &value.to_le_bytes());
        put(&mut img, b + 16, &16u64.to_le_bytes());
    }
    put(&mut img, stroff, &strtab);
    // --- DT_HASH : un seau, chaîne linéaire (11 symboles) ---
    let mut hash: Vec<u32> = alloc::vec![1, nsym as u32, 1];
    hash.push(0);
    for i in 1..nsym {
        hash.push(if i + 1 < nsym { i as u32 + 1 } else { 0 });
    }
    for (i, w) in hash.iter().enumerate() {
        put(&mut img, hashoff + i * 4, &w.to_le_bytes());
    }
    for (i, (tag, val)) in dyn_entries.iter().enumerate() {
        put(&mut img, dynoff + i * 16, &tag.to_le_bytes());
        put(&mut img, dynoff + i * 16 + 8, &val.to_le_bytes());
    }
    put(&mut img, TEXT_OFF, code);
    img
}

// --- vDSO i386 -------------------------------------------------------------------------------

/// Nombre de mots de l'en-tête du code i386 (décalages des fonctions, puis fin).
const HDR32: usize = 12;

/// Code du vDSO i386 (mode compatible, `.code32`). En tête, des décalages (u32) depuis le début
/// du bloc : `clock_gettime`, `gettimeofday`, `time`, `getcpu`, `clock_getres`,
/// `clock_gettime64`, `clock_getres_time64`, `__kernel_vsyscall`, son point de choix
/// `syscall`/`int 0x80`, `__kernel_sigreturn`, `__kernel_rt_sigreturn`, fin.
///
/// Sans adressage relatif à `eip`, `[vvar]` se trouve par `call`/`pop` (`2b − 0x1800` comme en
/// 64 bits). L'heure est calculée exactement comme le noyau : produit 64 × 64 → 128 bits du TSC
/// par la période (quatre `mul`), puis division longue par 10⁶ (quatre `div` de 32 bits) ; la
/// conversion en secondes divise le résultat de 64 bits par 10⁹ en deux `div`. Les fonctions
/// suivent l'ABI cdecl d'i386 (arguments sur la pile ; `ebx`, `esi`, `edi`, `ebp` préservés) et se
/// rabattent sur l'appel système time32 ou time64 correspondant.
///
/// `__kernel_vsyscall` (arguments dans `ebx, ecx, edx, esi, edi, ebp`, comme `int 0x80`) suit
/// Linux : `ecx`, `edx`, `ebp` empilés, puis `syscall` du mode compatible avec le 2e argument
/// dans `ebp` et le 6e en `[esp]` (convention de l'entrée IA32_CSTAR), ou `int 0x80` quand le
/// processeur n'a pas `syscall` en mode compatible (Intel) : [`build32`] remplace alors les deux
/// premiers octets du point de choix par un saut vers `int 0x80` (les « alternatives » de Linux).
/// Un appel interrompu puis relancé rejoue le `syscall` (`rip − 2`, `ebp` et `[esp]` intacts).
#[naked]
unsafe extern "C" fn vdso32_blob() {
    core::arch::asm!(
        "2:",
        ".long 3f - 2b",
        ".long 4f - 2b",
        ".long 5f - 2b",
        ".long 6f - 2b",
        ".long 7f - 2b",
        ".long 13f - 2b",
        ".long 14f - 2b",
        ".long 15f - 2b",
        ".long 16f - 2b",
        ".long 17f - 2b",
        ".long 18f - 2b",
        ".long 8f - 2b",
        ".code32",
        // ---- lecture de l'horloge : edi = 20 octets de travail -----------------------------
        // Sortie : eax = 0 et [edi] = ns monotones (64 bits), [edi + 8] = époque (s, 64 bits),
        // [edi + 16] = époque (ns) ; eax = −1 : repli sur l'appel système. ecx, edx détruits.
        ".p2align 4, 0xcc",
        "9:",
        "push ebx",
        "push esi",
        "push ebp",
        "call 92f",
        "92:",
        "pop ebp",
        // sub ebp, (92b − 2b + 0x1800) → début de `[vvar]` (deux étiquettes : encodé à la main).
        ".byte 0x81, 0xed",
        ".long 92b - 2b + 6144",
        "93:",
        "mov esi, dword ptr [ebp]",
        "test esi, 1",
        "jnz 94f",
        "mov eax, dword ptr [ebp + 4]",
        "test eax, eax",
        "jz 95f",
        "lfence",
        "rdtsc",
        "mov ebx, edx",
        "mov ecx, eax",
        "mul dword ptr [ebp + 8]",
        "mov dword ptr [edi], eax",
        "mov dword ptr [edi + 4], edx",
        "mov dword ptr [edi + 8], 0",
        "mov dword ptr [edi + 12], 0",
        "mov eax, ecx",
        "mul dword ptr [ebp + 12]",
        "add dword ptr [edi + 4], eax",
        "adc dword ptr [edi + 8], edx",
        "adc dword ptr [edi + 12], 0",
        "mov eax, ebx",
        "mul dword ptr [ebp + 8]",
        "add dword ptr [edi + 4], eax",
        "adc dword ptr [edi + 8], edx",
        "adc dword ptr [edi + 12], 0",
        "mov eax, ebx",
        "mul dword ptr [ebp + 12]",
        "add dword ptr [edi + 8], eax",
        "adc dword ptr [edi + 12], edx",
        "mov ecx, 1000000",
        "xor edx, edx",
        "mov eax, dword ptr [edi + 12]",
        "div ecx",
        "mov eax, dword ptr [edi + 8]",
        "div ecx",
        "mov eax, dword ptr [edi + 4]",
        "div ecx",
        "mov ebx, eax",
        "mov eax, dword ptr [edi]",
        "div ecx",
        "mov dword ptr [edi], eax",
        "mov dword ptr [edi + 4], ebx",
        "mov eax, dword ptr [ebp + 16]",
        "mov dword ptr [edi + 8], eax",
        "mov eax, dword ptr [ebp + 20]",
        "mov dword ptr [edi + 12], eax",
        "mov eax, dword ptr [ebp + 24]",
        "mov dword ptr [edi + 16], eax",
        "cmp esi, dword ptr [ebp]",
        "jne 93b",
        "xor eax, eax",
        "96:",
        "pop ebp",
        "pop esi",
        "pop ebx",
        "ret",
        "94:",
        "pause",
        "jmp 93b",
        "95:",
        "mov eax, -1",
        "jmp 96b",
        // ---- ns → secondes : ebx:eax = s, edx = ns (ecx détruit) ---------------------------
        ".p2align 4, 0xcc",
        "19:",
        "mov ecx, 1000000000",
        "xor edx, edx",
        "mov eax, dword ptr [edi + 4]",
        "div ecx",
        "mov ebx, eax",
        "mov eax, dword ptr [edi]",
        "div ecx",
        "ret",
        // ---- + époque (horloges murales) ---------------------------------------------------
        ".p2align 4, 0xcc",
        "29:",
        "add eax, dword ptr [edi + 8]",
        "adc ebx, dword ptr [edi + 12]",
        "add edx, dword ptr [edi + 16]",
        "cmp edx, 1000000000",
        "jb 28f",
        "sub edx, 1000000000",
        "add eax, 1",
        "adc ebx, 0",
        "28:",
        "ret",
        // ---- clock_gettime commun : esi = horloge ; ecx = 0 et ebx:eax s, edx ns, ou −1 ---
        // REALTIME 0, MONOTONIC 1, MONOTONIC_RAW 4, *_COARSE 5 6, BOOTTIME 7, TAI 11.
        ".p2align 4, 0xcc",
        "37:",
        "cmp esi, 11",
        "ja 38f",
        "mov eax, 1",
        "mov ecx, esi",
        "shl eax, cl",
        "test eax, 0x8f3",
        "jz 38f",
        "call 9b",
        "cmp eax, -1",
        "je 38f",
        "call 19b",
        "cmp esi, 1",
        "je 36f",
        "cmp esi, 4",
        "je 36f",
        "cmp esi, 6",
        "je 36f",
        "cmp esi, 7",
        "je 36f",
        "call 29b",
        "36:",
        "xor ecx, ecx",
        "ret",
        "38:",
        "mov ecx, -1",
        "ret",
        // ---- clock_gettime(clk, struct old_timespec32 *ts) ---------------------------------
        ".p2align 4, 0xcc",
        "3:",
        "push ebx",
        "push esi",
        "push edi",
        "push ebp",
        "sub esp, 20",
        "mov edi, esp",
        "mov esi, dword ptr [esp + 40]",
        "call 37b",
        "test ecx, ecx",
        "jnz 32f",
        "mov ecx, dword ptr [esp + 44]",
        "mov dword ptr [ecx], eax",
        "mov dword ptr [ecx + 4], edx",
        "xor eax, eax",
        "jmp 33f",
        "32:",
        "mov eax, 265",
        "mov ebx, dword ptr [esp + 40]",
        "mov ecx, dword ptr [esp + 44]",
        "int 0x80",
        "33:",
        "add esp, 20",
        "pop ebp",
        "pop edi",
        "pop esi",
        "pop ebx",
        "ret",
        // ---- clock_gettime64(clk, struct __kernel_timespec *ts) ----------------------------
        ".p2align 4, 0xcc",
        "13:",
        "push ebx",
        "push esi",
        "push edi",
        "push ebp",
        "sub esp, 20",
        "mov edi, esp",
        "mov esi, dword ptr [esp + 40]",
        "call 37b",
        "test ecx, ecx",
        "jnz 132f",
        "mov ecx, dword ptr [esp + 44]",
        "mov dword ptr [ecx], eax",
        "mov dword ptr [ecx + 4], ebx",
        "mov dword ptr [ecx + 8], edx",
        "mov dword ptr [ecx + 12], 0",
        "xor eax, eax",
        "jmp 133f",
        "132:",
        "mov eax, 403",
        "mov ebx, dword ptr [esp + 40]",
        "mov ecx, dword ptr [esp + 44]",
        "int 0x80",
        "133:",
        "add esp, 20",
        "pop ebp",
        "pop edi",
        "pop esi",
        "pop ebx",
        "ret",
        // ---- gettimeofday(struct old_timeval32 *tv, struct timezone *tz) -------------------
        ".p2align 4, 0xcc",
        "4:",
        "push ebx",
        "push esi",
        "push edi",
        "push ebp",
        "sub esp, 20",
        "mov edi, esp",
        "call 9b",
        "cmp eax, -1",
        "je 45f",
        "cmp dword ptr [esp + 40], 0",
        "je 43f",
        "call 19b",
        "call 29b",
        "mov esi, eax",
        "mov eax, edx",
        "xor edx, edx",
        "mov ecx, 1000",
        "div ecx",
        "mov ecx, dword ptr [esp + 40]",
        "mov dword ptr [ecx], esi",
        "mov dword ptr [ecx + 4], eax",
        "43:",
        "mov ecx, dword ptr [esp + 44]",
        "test ecx, ecx",
        "jz 44f",
        "mov dword ptr [ecx], 0",
        "mov dword ptr [ecx + 4], 0",
        "44:",
        "xor eax, eax",
        "jmp 46f",
        "45:",
        "mov eax, 78",
        "mov ebx, dword ptr [esp + 40]",
        "mov ecx, dword ptr [esp + 44]",
        "int 0x80",
        "46:",
        "add esp, 20",
        "pop ebp",
        "pop edi",
        "pop esi",
        "pop ebx",
        "ret",
        // ---- time(old_time32_t *tloc) ------------------------------------------------------
        ".p2align 4, 0xcc",
        "5:",
        "push ebx",
        "push esi",
        "push edi",
        "push ebp",
        "sub esp, 20",
        "mov edi, esp",
        "call 9b",
        "cmp eax, -1",
        "je 55f",
        "call 19b",
        "call 29b",
        "mov ecx, dword ptr [esp + 40]",
        "test ecx, ecx",
        "jz 56f",
        "mov dword ptr [ecx], eax",
        "jmp 56f",
        "55:",
        "mov eax, 13",
        "mov ebx, dword ptr [esp + 40]",
        "int 0x80",
        "56:",
        "add esp, 20",
        "pop ebp",
        "pop edi",
        "pop esi",
        "pop ebx",
        "ret",
        // ---- getcpu(cpu, node, cache) : TSC_AUX = identifiant du cœur ----------------------
        ".p2align 4, 0xcc",
        "6:",
        "rdtscp",
        "mov eax, dword ptr [esp + 4]",
        "test eax, eax",
        "jz 62f",
        "mov dword ptr [eax], ecx",
        "62:",
        "mov eax, dword ptr [esp + 8]",
        "test eax, eax",
        "jz 63f",
        "mov dword ptr [eax], 0",
        "63:",
        "xor eax, eax",
        "ret",
        // ---- clock_getres (time32) et clock_getres_time64 : appel système ------------------
        ".p2align 4, 0xcc",
        "7:",
        "push ebx",
        "mov eax, 266",
        "mov ebx, dword ptr [esp + 8]",
        "mov ecx, dword ptr [esp + 12]",
        "int 0x80",
        "pop ebx",
        "ret",
        ".p2align 4, 0xcc",
        "14:",
        "push ebx",
        "mov eax, 406",
        "mov ebx, dword ptr [esp + 8]",
        "mov ecx, dword ptr [esp + 12]",
        "int 0x80",
        "pop ebx",
        "ret",
        // ---- __kernel_vsyscall : 15 octets, décrits par l'.eh_frame de `build_image32` -----
        ".p2align 4, 0xcc",
        "15:",
        "push ecx",
        "push edx",
        "push ebp",
        "16:",
        ".byte 0x89, 0xcd", // mov ebp, ecx (2e argument)
        ".byte 0x0f, 0x05", // syscall
        ".byte 0xeb, 0x02", // jmp → pop ebp
        ".byte 0xcd, 0x80", // int 0x80
        "pop ebp",
        "pop edx",
        "pop ecx",
        "ret",
        // ---- __kernel_sigreturn, __kernel_rt_sigreturn -------------------------------------
        ".p2align 4, 0xcc",
        "17:",
        "pop eax",
        "mov eax, 119",
        "int 0x80",
        ".p2align 4, 0xcc",
        "18:",
        "mov eax, 173",
        "int 0x80",
        "8:",
        "int3",
        ".code64",
        options(noreturn)
    );
}

/// Code du vDSO i386 et décalages de son en-tête.
fn blob32() -> (&'static [u8], [u32; HDR32 - 1]) {
    let start = vdso32_blob as usize as *const u8;
    // SAFETY : en-tête de `HDR32` mots écrit par l'assembleur ci-dessus, dans le texte du noyau.
    let hdr = unsafe { core::slice::from_raw_parts(start as *const u32, HDR32) };
    // SAFETY : `hdr[HDR32 - 1]` octets de code à partir du début du bloc.
    let code = unsafe { core::slice::from_raw_parts(start, hdr[HDR32 - 1] as usize) };
    let mut offs = [0u32; HDR32 - 1];
    offs.copy_from_slice(&hdr[..HDR32 - 1]);
    (code, offs)
}

/// Rangs de l'en-tête i386.
mod off32 {
    pub const VSYSCALL: usize = 7;
    pub const VSYSCALL_PICK: usize = 8;
}

/// Longueur de `__kernel_vsyscall` (taille de son symbole et étendue de son FDE).
const VSYSCALL_LEN: u32 = 15;

/// `syscall` utilisable en mode compatible (AMD, Hygon) : Intel lève #UD.
fn compat_syscall() -> bool {
    // SAFETY : CPUID 0 existe sur tout processeur x86_64.
    let r = unsafe { core::arch::x86_64::__cpuid(0) };
    let mut v = [0u8; 12];
    v[0..4].copy_from_slice(&r.ebx.to_le_bytes());
    v[4..8].copy_from_slice(&r.edx.to_le_bytes());
    v[8..12].copy_from_slice(&r.ecx.to_le_bytes());
    &v == b"AuthenticAMD" || &v == b"HygonGenuine"
}

/// Image ELF32 (`EM_386`) de la page `[vdso]` i386 : symboles de Linux (`__vdso_*`,
/// `__kernel_*`), `DT_HASH`, et `PT_GNU_EH_FRAME` avec la CFI de `__kernel_vsyscall` (un
/// dérouleur — libgcc, gdb — traverse ainsi un appel système fait par la glibc).
fn build_image32(code: &[u8], offs: &[u32; HDR32 - 1]) -> Vec<u8> {
    let mut img = alloc::vec![0u8; PAGE_SIZE];
    // Taille d'une fonction : jusqu'au début de la suivante dans le bloc (ou sa fin).
    let size_of = |i: usize| -> u32 {
        let next = offs.iter().copied().filter(|&o| o > offs[i]).min().unwrap_or(code.len() as u32);
        next - offs[i]
    };
    let mut strtab: Vec<u8> = alloc::vec![0];
    let mut add_str = |s: &str| {
        let off = strtab.len() as u32;
        strtab.extend_from_slice(s.as_bytes());
        strtab.push(0);
        off
    };
    let soname = add_str("linux-gate.so.1");
    let funcs: [(&str, usize); 10] = [
        ("__vdso_clock_gettime", 0),
        ("__vdso_gettimeofday", 1),
        ("__vdso_time", 2),
        ("__vdso_getcpu", 3),
        ("__vdso_clock_getres", 4),
        ("__vdso_clock_gettime64", 5),
        ("__vdso_clock_getres_time64", 6),
        ("__kernel_vsyscall", off32::VSYSCALL),
        ("__kernel_sigreturn", 9),
        ("__kernel_rt_sigreturn", 10),
    ];
    let mut syms: Vec<(u32, u32, u32)> = Vec::new();
    for (name, i) in funcs {
        let sz = if i == off32::VSYSCALL { VSYSCALL_LEN } else { size_of(i) };
        syms.push((add_str(name), TEXT_OFF as u32 + offs[i], sz));
    }
    let nsym = syms.len() + 1;
    // --- Disposition ---
    let phoff = 52;
    let phnum = 3;
    let symoff = phoff + phnum * 32;
    let stroff = symoff + nsym * 16;
    let hashoff = (stroff + strtab.len() + 3) & !3;
    let hashsz = 4 * (2 + 1 + nsym);
    let dynoff = (hashoff + hashsz + 3) & !3;
    let dyn_entries: [(u32, u32); 7] = [
        (4, hashoff as u32),       // DT_HASH
        (5, stroff as u32),        // DT_STRTAB
        (6, symoff as u32),        // DT_SYMTAB
        (10, strtab.len() as u32), // DT_STRSZ
        (11, 16),                  // DT_SYMENT
        (14, soname),              // DT_SONAME
        (0, 0),                    // DT_NULL
    ];
    let dynsz = dyn_entries.len() * 8;
    let hdroff = (dynoff + dynsz + 3) & !3;
    let ehoff = hdroff + 20;
    let vsyscall = TEXT_OFF as u32 + offs[off32::VSYSCALL];
    let eh = eh_frame32(ehoff as u32, vsyscall);
    assert!(ehoff + eh.len() <= TEXT_OFF && TEXT_OFF + code.len() <= PAGE_SIZE, "vdso32 : image trop grande");
    // --- En-tête ELF32 ---
    put(&mut img, 0, &[0x7f, b'E', b'L', b'F', 1, 1, 1, 0]);
    put(&mut img, 16, &3u16.to_le_bytes()); // ET_DYN
    put(&mut img, 18, &3u16.to_le_bytes()); // EM_386
    put(&mut img, 20, &1u32.to_le_bytes());
    put(&mut img, 28, &(phoff as u32).to_le_bytes());
    put(&mut img, 40, &52u16.to_le_bytes()); // e_ehsize
    put(&mut img, 42, &32u16.to_le_bytes()); // e_phentsize
    put(&mut img, 44, &(phnum as u16).to_le_bytes());
    put(&mut img, 46, &40u16.to_le_bytes()); // e_shentsize
    // --- Segments (Elf32_Phdr : type, offset, vaddr, paddr, filesz, memsz, flags, align) ---
    let ph = |img: &mut Vec<u8>, i: usize, ty: u32, flags: u32, off: u32, sz: u32, align: u32| {
        for (k, v) in [ty, off, off, off, sz, sz, flags, align].iter().enumerate() {
            put(img, phoff + i * 32 + k * 4, &v.to_le_bytes());
        }
    };
    ph(&mut img, 0, 1, 5, 0, PAGE_SIZE as u32, PAGE_SIZE as u32); // PT_LOAD R+X
    ph(&mut img, 1, 2, 4, dynoff as u32, dynsz as u32, 4); // PT_DYNAMIC
    ph(&mut img, 2, 0x6474_e550, 4, hdroff as u32, 20, 4); // PT_GNU_EH_FRAME
    // --- Symboles (Elf32_Sym : nom, valeur, taille, info, autre, section ; le premier nul) ---
    for (i, (name, value, size)) in syms.iter().enumerate() {
        let b = symoff + (i + 1) * 16;
        put(&mut img, b, &name.to_le_bytes());
        put(&mut img, b + 4, &value.to_le_bytes());
        put(&mut img, b + 8, &size.to_le_bytes());
        img[b + 12] = 0x12; // GLOBAL FUNC
        put(&mut img, b + 14, &1u16.to_le_bytes());
    }
    put(&mut img, stroff, &strtab);
    // --- DT_HASH : un seau, chaîne linéaire ---
    let mut hash: Vec<u32> = alloc::vec![1, nsym as u32, 1, 0];
    for i in 1..nsym {
        hash.push(if i + 1 < nsym { i as u32 + 1 } else { 0 });
    }
    for (i, w) in hash.iter().enumerate() {
        put(&mut img, hashoff + i * 4, &w.to_le_bytes());
    }
    for (i, (tag, val)) in dyn_entries.iter().enumerate() {
        put(&mut img, dynoff + i * 8, &tag.to_le_bytes());
        put(&mut img, dynoff + i * 8 + 4, &val.to_le_bytes());
    }
    // --- .eh_frame_hdr : version 1, eh_frame_ptr pcrel|sdata4, compte udata4, table
    // datarel|sdata4 d'une entrée (__kernel_vsyscall → son FDE, 24 octets après le CIE) ---
    put(&mut img, hdroff, &[1, 0x1b, 0x03, 0x3b]);
    put(&mut img, hdroff + 4, &(ehoff as i32 - (hdroff as i32 + 4)).to_le_bytes());
    put(&mut img, hdroff + 8, &1u32.to_le_bytes());
    put(&mut img, hdroff + 12, &(vsyscall as i32 - hdroff as i32).to_le_bytes());
    put(&mut img, hdroff + 16, &((ehoff + 24) as i32 - hdroff as i32).to_le_bytes());
    put(&mut img, ehoff, &eh);
    put(&mut img, TEXT_OFF, code);
    img
}

/// `.eh_frame` placée en `at` dans l'image : un CIE (`zR`, FDE en pcrel|sdata4, CFA = esp + 4,
/// eip en CFA − 4) et le FDE de `__kernel_vsyscall` (en `func`) : trois `push` (ecx, edx, ebp),
/// le corps de 8 octets, leurs `pop`, comme la CFI de `entry/vdso/vdso32/system_call.S`.
fn eh_frame32(at: u32, func: u32) -> Vec<u8> {
    let mut e: Vec<u8> = Vec::new();
    // CIE : 20 octets après le mot de longueur, identifiant 0.
    e.extend_from_slice(&20u32.to_le_bytes());
    e.extend_from_slice(&0u32.to_le_bytes());
    e.extend_from_slice(&[1, b'z', b'R', 0, 1, 0x7c, 8, 1, 0x1b]);
    e.extend_from_slice(&[0x0c, 4, 4, 0x88, 1]); // def_cfa esp+4 ; eip en cfa − 4
    e.extend_from_slice(&[0, 0]); // DW_CFA_nop
    // FDE : 40 octets après le mot de longueur.
    let fde = e.len() as u32;
    e.extend_from_slice(&40u32.to_le_bytes());
    e.extend_from_slice(&(fde + 4).to_le_bytes()); // distance jusqu'au CIE
    e.extend_from_slice(&func.wrapping_sub(at + fde + 8).to_le_bytes()); // pc_begin (pcrel)
    e.extend_from_slice(&VSYSCALL_LEN.to_le_bytes());
    e.push(0); // pas de données d'augmentation
    e.extend_from_slice(&[
        0x41, 0x0e, 8, 0x81, 2, // push ecx : cfa = esp + 8, ecx en cfa − 8
        0x41, 0x0e, 12, 0x82, 3, // push edx
        0x41, 0x0e, 16, 0x85, 4, // push ebp
        0x49, 0x0e, 12, 0xc5, // corps de 8 octets puis pop ebp
        0x41, 0x0e, 8, 0xc2, // pop edx
        0x41, 0x0e, 4, 0xc1, // pop ecx
    ]);
    debug_assert_eq!(e.len(), 24 + 44);
    e.extend_from_slice(&0u32.to_le_bytes()); // fin de .eh_frame
    e
}

/// Mode de l'horloge : 1 si l'horloge monotone du noyau est le TSC (le vDSO lit alors le même
/// compteur), 0 sinon (repli sur l'appel système).
fn clock_mode() -> u32 {
    // SAFETY : lecture du TSC.
    let a = unsafe { core::arch::x86_64::_rdtsc() };
    let i = time::Instant::now().ticks();
    // SAFETY : idem.
    let b = unsafe { core::arch::x86_64::_rdtsc() };
    (a <= i && i <= b && time::monotonic_period_fs() != 0 && time::monotonic_period_fs() != u64::MAX) as u32
}

fn new_frame() -> Option<usize> {
    let pfn = frame_allocator::alloc_raw_frames(1)?;
    // SAFETY : frame neuve, accessible par la physmap.
    unsafe { core::ptr::write_bytes(phys_to_virt(pfn * PAGE_SIZE) as *mut u8, 0, PAGE_SIZE) };
    Some(pfn)
}

/// Frame de `[vvar]`, pour les mises à jour de l'heure (`set_epoch`).
static VVAR_PFN: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Nouvel instant du démarrage (`clock_settime`) : publié dans `[vvar]` sous le *seqlock*
/// (compteur impair pendant l'écriture ; le lecteur du vDSO recommence s'il l'a vu changer).
pub fn set_epoch(sec: i64, nsec: i64) {
    use core::sync::atomic::{fence, AtomicU32, Ordering};
    let pfn = VVAR_PFN.load(Ordering::Acquire);
    if pfn == 0 {
        return;
    }
    // SAFETY : frame de `[vvar]`, vivante tant que le vDSO existe (toujours), projetée par la
    // physmap ; seul ce code l'écrit, sous `SETTIME`.
    unsafe {
        let v = phys_to_virt(pfn * PAGE_SIZE) as *mut u8;
        let seq = &*(v.add(vvar::SEQ) as *const AtomicU32);
        let _guard = SETTIME.lock();
        seq.fetch_add(1, Ordering::Relaxed);
        fence(Ordering::Release);
        core::ptr::write_volatile(v.add(vvar::EPOCH_SEC) as *mut i64, sec);
        core::ptr::write_volatile(v.add(vvar::EPOCH_NSEC) as *mut i64, nsec);
        fence(Ordering::Release);
        seq.fetch_add(1, Ordering::Release);
    }
}

static SETTIME: spin::Mutex<()> = spin::Mutex::new(());

fn build() -> Option<Vdso> {
    let (code, offs) = blob();
    if TEXT_OFF + code.len() > PAGE_SIZE {
        error!("vdso : code trop long ({} octets)", code.len());
        return None;
    }
    let image = build_image(code, offs);
    let (vvar_pfn, text_pfn) = (new_frame()?, new_frame()?);
    let mode = clock_mode();
    let epoch = crate::linux::sys::boot_epoch();
    // SAFETY : frames neuves, accessibles par la physmap.
    unsafe {
        let v = phys_to_virt(vvar_pfn * PAGE_SIZE) as *mut u8;
        (v.add(vvar::SEQ) as *mut u32).write(0);
        (v.add(vvar::MODE) as *mut u32).write(mode);
        (v.add(vvar::PERIOD_FS) as *mut u64).write(time::monotonic_period_fs());
        (v.add(vvar::EPOCH_SEC) as *mut i64).write(epoch);
        (v.add(vvar::EPOCH_NSEC) as *mut i64).write(0);
        core::ptr::copy_nonoverlapping(image.as_ptr(), phys_to_virt(text_pfn * PAGE_SIZE) as *mut u8, PAGE_SIZE);
    }
    VVAR_PFN.store(vvar_pfn, core::sync::atomic::Ordering::Release);
    let (sec, nsec) = crate::linux::sys::epoch_parts();
    if (sec, nsec) != (epoch, 0) {
        set_epoch(sec, nsec);
    }
    info!("vdso : {} octets de code, horloge {}", code.len(), if mode == 1 { "TSC" } else { "par appel systeme" });
    Some(Vdso { vvar: ShmObject::sealed_from_frames(&[vvar_pfn]), text: ShmObject::sealed_from_frames(&[text_pfn]), text32: build32() })
}

/// Page `[vdso]` i386 (même `[vvar]` que la page 64 bits).
fn build32() -> Option<(Arc<ShmObject>, usize)> {
    let (code, offs) = blob32();
    if TEXT_OFF + code.len() > PAGE_SIZE {
        error!("vdso32 : code trop long ({} octets)", code.len());
        return None;
    }
    let mut image = build_image32(code, &offs);
    let pick = TEXT_OFF + offs[off32::VSYSCALL_PICK] as usize;
    let fast = compat_syscall();
    if !fast {
        // `jmp +4` par-dessus `mov ebp, ecx ; syscall ; jmp` : droit sur `int 0x80`.
        image[pick..pick + 2].copy_from_slice(&[0xeb, 0x04]);
    }
    let pfn = new_frame()?;
    // SAFETY : frame neuve, accessible par la physmap.
    unsafe { core::ptr::copy_nonoverlapping(image.as_ptr(), phys_to_virt(pfn * PAGE_SIZE) as *mut u8, PAGE_SIZE) };
    info!("vdso32 : {} octets de code, __kernel_vsyscall par {}", code.len(), if fast { "syscall" } else { "int 0x80" });
    Some((ShmObject::sealed_from_frames(&[pfn]), TEXT_OFF + offs[off32::VSYSCALL] as usize))
}

/// vDSO des processus 32 bits : projette `[vvar]` et `[vdso]` (sous 4 Gio, la limite de
/// l'espace est déjà posée) ; renvoie `(AT_SYSINFO_EHDR, AT_SYSINFO)`.
pub fn map32(mm: &Mm) -> Option<(usize, usize)> {
    let v = VDSO.call_once(build).as_ref()?;
    let (text, vsyscall) = v.text32.as_ref()?;
    let base = mm.mmap(0, 2 * PAGE_SIZE, Prot::NONE, Backing::Anon, false, "[vvar]").ok()?;
    mm.mmap(base, PAGE_SIZE, Prot(Prot::READ), Backing::Shared { obj: v.vvar.clone(), pgoff: 0 }, true, "[vvar]").ok()?;
    mm.mmap(base + PAGE_SIZE, PAGE_SIZE, Prot::RX, Backing::Shared { obj: text.clone(), pgoff: 0 }, true, "[vdso]").ok()?;
    Some((base + PAGE_SIZE, base + PAGE_SIZE + vsyscall))
}

/// Projette `[vvar]` et `[vdso]` dans `mm` ; renvoie l'adresse de l'image ELF
/// (`AT_SYSINFO_EHDR`).
pub fn map(mm: &Mm) -> Option<usize> {
    let v = VDSO.call_once(build).as_ref()?;
    let base = mm.mmap(0, 2 * PAGE_SIZE, Prot::NONE, Backing::Anon, false, "[vvar]").ok()?;
    mm.mmap(base, PAGE_SIZE, Prot(Prot::READ), Backing::Shared { obj: v.vvar.clone(), pgoff: 0 }, true, "[vvar]").ok()?;
    mm.mmap(base + PAGE_SIZE, PAGE_SIZE, Prot::RX, Backing::Shared { obj: v.text.clone(), pgoff: 0 }, true, "[vdso]").ok()?;
    Some(base + PAGE_SIZE)
}
