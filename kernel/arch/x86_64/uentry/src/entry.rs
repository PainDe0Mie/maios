//! Assembleur des entrées et sorties.
//!
//! Tout le code d'entrée vit dans **une seule** fonction nue (une section, un symbole), ce qui
//! convient au chargeur de modules de MaiOS ; les points d'entrée y sont à des déplacements fixes :
//!
//! | Déplacement | Contenu |
//! |---|---|
//! | `0 .. 8192` | 256 talons d'IDT de 32 octets |
//! | `8192` | entrée `syscall` (IA32_LSTAR) |
//! | `8448` | passage initial en mode utilisateur (`rdi` = cadre) |
//! | `8512` | entrée `syscall` du mode compatible (IA32_CSTAR, processeurs AMD) |
//! | ensuite | chemin commun, retour, entrée paranoïaque |

use super::{
    uentry_paranoid, uentry_prepare_return, uentry_trap, BLOCKS, BLOCK_SHIFT, HANDLERS, MAX_CPUS,
    SMAP_ON, KERNEL_CS, KERNEL_SS, SYSCALL_VECTOR, SYSCALL32_VECTOR, USER_CS, USER32_CS, USER_SS,
};

const STUB_SIZE: usize = 32;
const SYSCALL_OFFSET: usize = 8192;
const ENTER_OFFSET: usize = 8448;
const CSTAR_OFFSET: usize = 8512;

pub(crate) fn stub_address(vector: usize) -> usize {
    let base = uentry_area as usize;
    debug_assert!(base % STUB_SIZE == 0);
    base + vector * STUB_SIZE
}

pub(crate) fn syscall_entry_address() -> usize {
    uentry_area as usize + SYSCALL_OFFSET
}

pub(crate) fn cstar_entry_address() -> usize {
    uentry_area as usize + CSTAR_OFFSET
}

/// Saute dans le chemin de retour vers l'utilisateur avec `rsp = frame`.
pub(crate) unsafe fn jump_to_return_path(frame: usize) -> ! {
    let target = uentry_area as usize + ENTER_OFFSET;
    core::arch::asm!("jmp {t}", t = in(reg) target, in("rdi") frame, options(noreturn));
}

/// Passage en mode utilisateur avec point de retour (voir [`crate::run_user`]).
///
/// Sauve les registres préservés et `rsp` dans `*jmp_slot`, réserve sous ce point une zone
/// d'information de 16 octets (`[kstack_top, kstack_top + 16)`) puis la place du cadre, appelle
/// `prepare(arg, kstack_top)` (qui renvoie l'adresse du cadre) et part vers l'utilisateur.
#[naked]
pub(crate) unsafe extern "C" fn run_user_trampoline(prepare: usize, arg: usize, jmp_slot: *mut usize) -> u64 {
    core::arch::asm!(
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "cli",
        "mov qword ptr [rdx], rsp",
        "mov rax, rdi",
        "mov rdi, rsi",
        "lea rsi, [rsp - 16]",
        "and rsi, -16",
        "lea rsp, [rsi - 256]",
        "call rax",
        "mov rdi, rax",
        "lea r11, [rip + {area}]",
        "add r11, {enter_off}",
        "jmp r11",
        area = sym uentry_area,
        enter_off = const ENTER_OFFSET,
        options(noreturn)
    );
}

/// Retour au point sauvé par [`run_user_trampoline`] : `run_user` renvoie `code`.
#[naked]
pub(crate) unsafe extern "C" fn exit_user_trampoline(jmp: usize, code: u64) -> ! {
    core::arch::asm!(
        "mov rsp, rdi",
        "mov rax, rsi",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "sti",
        "ret",
        options(noreturn)
    );
}

/// Talons, entrées et retours. Ne jamais appeler directement.
#[naked]
unsafe extern "C" fn uentry_area() {
    core::arch::asm!(
        ".p2align 5",
        // ---- 256 talons ------------------------------------------------------------------
        ".Luentry_v = 0",
        ".rept 256",
        ".p2align 5, 0xcc",
        ".if (.Luentry_v == 2) || (.Luentry_v == 18)",
        // NMI, #MC : toujours l'entrée paranoïaque.
        "    push 0",
        "    push .Luentry_v",
        "    jmp 7f",
        ".else",
        ".if (.Luentry_v == 8) || (.Luentry_v == 10) || (.Luentry_v == 11) || (.Luentry_v == 12) || (.Luentry_v == 13) || (.Luentry_v == 14) || (.Luentry_v == 17) || (.Luentry_v == 21) || (.Luentry_v == 29) || (.Luentry_v == 30)",
        "    test byte ptr [rsp + 16], 3",
        "    jnz 1f",
        "    jmp qword ptr [rip + {handlers} + .Luentry_v * 8]",
        "1:",
        ".else",
        "    test byte ptr [rsp + 8], 3",
        "    jnz 1f",
        "    jmp qword ptr [rip + {handlers} + .Luentry_v * 8]",
        "1:",
        "    push 0",
        ".endif",
        "    push .Luentry_v",
        "    jmp 8f",
        ".endif",
        ".Luentry_v = .Luentry_v + 1",
        ".endr",

        // ---- syscall (déplacement 8192) -------------------------------------------------
        ".org {sys_off}, 0xcc",
        "swapgs",
        "mov qword ptr gs:[0], rsp",
        "mov rsp, qword ptr gs:[8]",
        "push {user_ss}",
        "push qword ptr gs:[0]",
        "swapgs",
        "push r11",
        "push {user_cs}",
        "push rcx",
        "push 0",
        "push {sysvec}",
        "jmp 8f",
        ".org {enter_off}, 0xcc",

        // ---- premier passage en mode utilisateur (déplacement 8448) ----------------------
        "mov rsp, rdi",
        "jmp 9f",

        // ---- syscall du mode compatible (déplacement 8512) --------------------------------
        // Comme l'entrée 64 bits (FMASK coupe IF et TF) ; le cadre porte le CS 32 bits et le
        // vecteur `SYSCALL32_VECTOR` (table i386, retour par `sysretl`).
        ".org {cstar_off}, 0xcc",
        "swapgs",
        "mov qword ptr gs:[0], rsp",
        "mov rsp, qword ptr gs:[8]",
        "push {user_ss}",
        "push qword ptr gs:[0]",
        "swapgs",
        "push r11",
        "push {user32_cs}",
        "push rcx",
        "push 0",
        "push {sys32vec}",
        "jmp 8f",

        // ---- chemin commun depuis l'utilisateur -------------------------------------------
        "8:",
        "push rax",
        "push rbx",
        "push rcx",
        "push rdx",
        "push rsi",
        "push rdi",
        "push rbp",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "rdgsbase rax",
        "push rax",
        "rdfsbase rax",
        "push rax",
        "swapgs",
        "mov rax, qword ptr gs:[16]",
        "mov rbx, qword ptr gs:[24]",
        "swapgs",
        "wrgsbase rax",
        "wrfsbase rbx",
        "test byte ptr [rip + {smap_on}], 1",
        "jz 2f",
        "clac",
        "2:",
        "cld",
        "mov rdi, rsp",
        "call {trap}",

        // ---- retour vers l'utilisateur (rsp = cadre) --------------------------------------
        "9:",
        "cli",
        "mov rdi, rsp",
        "call {prepare}",
        "pop rax",
        "wrfsbase rax",
        "pop rax",
        "wrgsbase rax",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rbp",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rbx",
        "pop rax",
        "cmp qword ptr [rsp], {sysvec}",
        "jne 6f",
        "mov rcx, qword ptr [rsp + 16]",
        "mov r11, qword ptr [rsp + 32]",
        "mov rsp, qword ptr [rsp + 40]",
        "sysretq",
        // `sysretl` : retour en mode compatible (CS = STAR[63:48] | 3, SS = + 8), permis aussi
        // sur Intel ; `uentry_prepare_return` n'y laisse que des cadres sûrs.
        "6:",
        "cmp qword ptr [rsp], {sys32vec}",
        "jne 3f",
        "mov ecx, dword ptr [rsp + 16]",
        "mov r11, qword ptr [rsp + 32]",
        "mov esp, dword ptr [rsp + 40]",
        ".byte 0x0f, 0x07",
        "3:",
        "add rsp, 16",
        "iretq",

        // ---- entrée paranoïaque (NMI, #MC) -------------------------------------------------
        "7:",
        "push rax",
        "push rbx",
        "push rcx",
        "push rdx",
        "push rsi",
        "push rdi",
        "push rbp",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "rdgsbase rax",
        "push rax",
        "rdfsbase rax",
        "push rax",
        "rdtscp",
        "and ecx, {cpu_mask}",
        "shl rcx, {block_shift}",
        "lea rbx, [rip + {blocks}]",
        "add rbx, rcx",
        "mov rax, qword ptr [rbx + 16]",
        "test rax, rax",
        "jz 4f",
        "wrgsbase rax",
        "mov rax, qword ptr [rbx + 24]",
        "wrfsbase rax",
        "4:",
        "test byte ptr [rip + {smap_on}], 1",
        "jz 5f",
        "clac",
        "5:",
        "cld",
        "mov rdi, rsp",
        "call {paranoid}",
        "pop rax",
        "wrfsbase rax",
        "pop rax",
        "wrgsbase rax",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rbp",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rbx",
        "pop rax",
        "add rsp, 16",
        "iretq",

        handlers = sym HANDLERS,
        blocks = sym BLOCKS,
        smap_on = sym SMAP_ON,
        trap = sym uentry_trap,
        prepare = sym uentry_prepare_return,
        paranoid = sym uentry_paranoid,
        sys_off = const SYSCALL_OFFSET,
        enter_off = const ENTER_OFFSET,
        cstar_off = const CSTAR_OFFSET,
        user_ss = const USER_SS,
        user_cs = const USER_CS,
        user32_cs = const USER32_CS,
        sysvec = const SYSCALL_VECTOR,
        sys32vec = const SYSCALL32_VECTOR,
        cpu_mask = const MAX_CPUS - 1,
        block_shift = const BLOCK_SHIFT,
        options(noreturn),
    );
}

/// Appelle un gestionnaire `x86-interrupt` (sans code d'erreur) comme s'il était déclenché par
/// le matériel depuis le niveau 0 : faux cadre `iretq` qui revient ici.
#[naked]
pub(crate) unsafe extern "C" fn invoke_legacy(handler: u64) {
    core::arch::asm!(
        "push rbp",
        "mov rbp, rsp",
        "and rsp, -16",
        "push {kss}",
        "push rbp",
        "pushfq",
        "push {kcs}",
        "lea rax, [rip + 2f]",
        "push rax",
        "jmp rdi",
        "2:",
        "pop rbp",
        "ret",
        kss = const KERNEL_SS,
        kcs = const KERNEL_CS,
        options(noreturn),
    );
}
