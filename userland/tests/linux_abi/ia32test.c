/* Banc S2 du plan 07 (Steam) : ABI i386 de MaiProc (processus 32 bits en mode compatible).
 *
 * Programme 32 bits autonome (sans libc : chaque appel passe par `int $0x80`), compilé par
 * `gcc -m32 -ffreestanding -nostdlib -static`. Il éprouve directement ce dont une libc i386 a
 * besoin : chargeur ELF32 et pile initiale, limite des 4 Gio, TLS par `set_thread_area` et
 * `%gs`, fils (`clone` avec `user_desc`), signaux (cadres `sigframe`/`rt_sigframe` ia32,
 * état x87/SSE, pile alternative, fautes), `fork`/`execve` (32 → 32 et 32 → 64 bits), fichiers
 * (`stat64`, `_llseek`, `pread64`, `ftruncate64`, verrous `flock64`), `iovec`, `msghdr` et
 * `SCM_RIGHTS` au format 32 bits, temps 32 et 64 bits, liste de verrous robustes 32 bits.
 *
 *   run -s /extra_files/linux/ia32test
 */

typedef unsigned int u32;
typedef int i32;
typedef unsigned short u16;
typedef unsigned char u8;
typedef unsigned long long u64;
typedef long long i64;

/* ---- Appels système --------------------------------------------------------------------- */

long sys6(long n, long a, long b, long c, long d, long e, long f);
long clone_thread(long flags, void *stack, void *ptid, void *tls, void *ctid, int (*fn)(void));
void restore_rt(void);
void restore_old(void);
void do_fault(void);
extern char fault_insn[], fault_next[];
void _start(void);

__asm__(
    ".text\n"
    ".globl _start\n"
    "_start:\n"
    "  xor %ebp, %ebp\n"
    "  mov %esp, %eax\n"
    "  and $-16, %esp\n"
    "  sub $12, %esp\n"
    "  push %eax\n"
    "  call cmain\n"
    "  mov %eax, %ebx\n"
    "  mov $252, %eax\n"
    "  int $0x80\n"
    "  hlt\n"
    ".globl sys6\n"
    "sys6:\n"
    "  push %ebp\n  push %ebx\n  push %esi\n  push %edi\n"
    "  mov 20(%esp), %eax\n  mov 24(%esp), %ebx\n  mov 28(%esp), %ecx\n"
    "  mov 32(%esp), %edx\n  mov 36(%esp), %esi\n  mov 40(%esp), %edi\n  mov 44(%esp), %ebp\n"
    "  int $0x80\n"
    "  pop %edi\n  pop %esi\n  pop %ebx\n  pop %ebp\n  ret\n"
    ".globl clone_thread\n"
    "clone_thread:\n"
    "  push %ebp\n  push %ebx\n  push %esi\n  push %edi\n"
    "  mov 20(%esp), %ebx\n  mov 24(%esp), %ecx\n  mov 28(%esp), %edx\n"
    "  mov 32(%esp), %esi\n  mov 36(%esp), %edi\n  mov 40(%esp), %ebp\n"
    "  mov $120, %eax\n  int $0x80\n"
    "  test %eax, %eax\n  jnz 1f\n"
    "  call *%ebp\n"
    "  mov %eax, %ebx\n  mov $1, %eax\n  int $0x80\n  hlt\n"
    "1:\n  pop %edi\n  pop %esi\n  pop %ebx\n  pop %ebp\n  ret\n"
    /* vsys6(fn, nr, a1..a6) : appel par __kernel_vsyscall (registres comme int 0x80). */
    ".globl vsys6\n"
    "vsys6:\n"
    "  push %ebp\n  push %ebx\n  push %esi\n  push %edi\n"
    "  mov 24(%esp), %eax\n  mov 28(%esp), %ebx\n  mov 32(%esp), %ecx\n"
    "  mov 36(%esp), %edx\n  mov 40(%esp), %esi\n  mov 44(%esp), %edi\n  mov 48(%esp), %ebp\n"
    "  call *20(%esp)\n"
    "  pop %edi\n  pop %esi\n  pop %ebx\n  pop %ebp\n  ret\n"
    ".globl restore_rt\n"
    "restore_rt:\n  mov $173, %eax\n  int $0x80\n  hlt\n"
    ".globl restore_old\n"
    "restore_old:\n  pop %eax\n  mov $119, %eax\n  int $0x80\n  hlt\n"
    ".globl do_fault, fault_insn, fault_next\n"
    "do_fault:\n"
    "fault_insn:\n  movl 0, %eax\n"
    "fault_next:\n  ret\n");

#define sc0(n) sys6(n, 0, 0, 0, 0, 0, 0)
#define sc1(n, a) sys6(n, (long)(a), 0, 0, 0, 0, 0)
#define sc2(n, a, b) sys6(n, (long)(a), (long)(b), 0, 0, 0, 0)
#define sc3(n, a, b, c) sys6(n, (long)(a), (long)(b), (long)(c), 0, 0, 0)
#define sc4(n, a, b, c, d) sys6(n, (long)(a), (long)(b), (long)(c), (long)(d), 0, 0)
#define sc5(n, a, b, c, d, e) sys6(n, (long)(a), (long)(b), (long)(c), (long)(d), (long)(e), 0)

enum {
    NR_exit = 1, NR_fork = 2, NR_read = 3, NR_write = 4, NR_open = 5, NR_close = 6,
    NR_waitpid = 7, NR_unlink = 10, NR_execve = 11, NR_time = 13, NR_getpid = 20,
    NR_kill = 37, NR_pipe = 42, NR_brk = 45, NR_ioctl = 54, NR_gettimeofday = 78,
    NR_mmap = 90, NR_munmap = 91, NR_socketcall = 102, NR_wait4 = 114, NR_uname = 122,
    NR_sigprocmask = 126, NR_llseek = 140, NR_readv = 145, NR_writev = 146,
    NR_sched_yield = 158, NR_nanosleep = 162, NR_rt_sigaction = 174,
    NR_rt_sigprocmask = 175, NR_rt_sigpending = 176, NR_pread64 = 180, NR_sigaltstack = 186,
    NR_ugetrlimit = 191, NR_mmap2 = 192, NR_ftruncate64 = 194, NR_stat64 = 195,
    NR_fstat64 = 197, NR_fcntl64 = 221, NR_gettid = 224, NR_futex = 240,
    NR_set_thread_area = 243, NR_get_thread_area = 244, NR_exit_group = 252,
    NR_clock_gettime = 265, NR_tgkill = 270, NR_set_robust_list = 311,
    NR_prlimit64 = 340, NR_statx = 383, NR_clock_gettime64 = 403,
    NR_clock_nanosleep_time64 = 407, NR_fstat = 108,
};

/* ---- Sortie ------------------------------------------------------------------------------ */

static u32 slen(const char *s) { u32 n = 0; while (s[n]) n++; return n; }
static int seq(const char *a, const char *b) { while (*a && *a == *b) { a++; b++; } return *a == *b; }
static void out(const char *s) { sc3(NR_write, 1, s, slen(s)); }
static void out_u(u32 v) {
    char b[12]; int i = 11; b[i] = 0;
    do { b[--i] = '0' + v % 10; v /= 10; } while (v);
    out(b + i);
}
static void out_x(u32 v) {
    char b[11]; b[0] = '0'; b[1] = 'x'; b[10] = 0;
    for (int i = 0; i < 8; i++) b[9 - i] = "0123456789abcdef"[(v >> (4 * i)) & 15];
    out(b);
}
static void *mset(void *d, int c, u32 n) { u8 *p = d; while (n--) *p++ = (u8)c; return d; }
static void mcpy(void *d, const void *s, u32 n) { u8 *a = d; const u8 *b = s; while (n--) *a++ = *b++; }
static int mcmp(const void *a, const void *b, u32 n) {
    const u8 *x = a, *y = b;
    for (u32 i = 0; i < n; i++) if (x[i] != y[i]) return 1;
    return 0;
}
/* Requis par gcc pour les copies de structures. */
void *memset(void *d, int c, u32 n) { return mset(d, c, n); }
void *memcpy(void *d, const void *s, u32 n) { mcpy(d, s, n); return d; }

static int ok_n, ko_n;
static void check(const char *what, int cond, long detail) {
    if (cond) { out("[ok] "); ok_n++; }
    else { out("[ECHEC] "); ko_n++; }
    out(what);
    if (!cond) { out(" ("); out_x((u32)detail); out(")"); }
    out("\n");
}
static int is_err(long r) { return (unsigned long)r >= 0xfffff001ul; }

/* ---- Structures i386 ---------------------------------------------------------------------- */

struct user_desc {
    u32 entry_number, base_addr, limit;
    u32 flags; /* seg_32bit:1 contents:2 read_exec_only:1 limit_in_pages:1 seg_not_present:1 useable:1 */
};
struct ksigaction { u32 handler, flags, restorer; u32 mask[2]; };
struct ts32 { i32 sec, nsec; };
struct ts64 { i64 sec, nsec; };
struct iov32 { u32 base, len; };
struct msghdr32 { u32 name, namelen, iov, iovlen, control, controllen, flags; };
struct flock64_32 { short type, whence; i64 start __attribute__((packed)); i64 len __attribute__((packed)); i32 pid; } __attribute__((packed));
struct robust_head { u32 next; i32 off; u32 pending; };
struct robust_entry { u32 next; u32 word; };

#define SA_SIGINFO 4
#define SA_ONSTACK 0x08000000
#define SA_RESTORER 0x04000000
#define SIGUSR1 10
#define SIGUSR2 12
#define SIGSEGV 11

/* ---- Données partagées ------------------------------------------------------------------- */

static u32 tls_main[16] __attribute__((aligned(16)));
static u32 tls_thread[16] __attribute__((aligned(16)));
static u8 thread_stack[16384] __attribute__((aligned(16)));
static volatile u32 child_tid_word, parent_tid_word;
static volatile u32 thread_gs0, thread_tid;
static struct robust_head rhead;
static struct robust_entry rentry;

static volatile int h_sig, h_signo, h_code, h_pid, h_gs, h_gs0, h_count, h_onstack;
static volatile u32 h_trapno, h_cr2, h_addr, h_eip;
static u32 alt_lo, alt_hi;

static u32 gs_read0(void) { u32 v; __asm__ volatile("movl %%gs:0, %0" : "=r"(v)); return v; }
static u16 gs_sel(void) { u16 v; __asm__ volatile("mov %%gs, %0" : "=r"(v)); return v; }

/* ---- Tests -------------------------------------------------------------------------------- */

long vsys6(u32 fn, long nr, long a, long b, long c, long d, long e, long f);
static u32 vdso_entry, vdso_ehdr;

static void test_auxv(u32 *sp) {
    u32 argc = sp[0];
    u32 *p = sp + 1 + argc + 1;
    while (*p) p++;
    p++;
    u32 pagesz = 0, entry = 0, random = 0, hwcap = 0, sysinfo = 0;
    const char *plat = "";
    for (; p[0]; p += 2) {
        switch (p[0]) {
        case 6: pagesz = p[1]; break;
        case 9: entry = p[1]; break;
        case 15: plat = (const char *)p[1]; break;
        case 16: hwcap = p[1]; break;
        case 25: random = p[1]; break;
        case 32: sysinfo = p[1]; break;
        case 33: vdso_ehdr = p[1]; break;
        }
    }
    check("auxv : AT_PAGESZ 4096", pagesz == 4096, pagesz);
    check("auxv : AT_ENTRY = _start", entry == (u32)_start, entry);
    check("auxv : AT_PLATFORM i686", seq(plat, "i686"), 0);
    check("auxv : AT_HWCAP et AT_RANDOM", hwcap != 0 && random != 0, random);
    vdso_entry = sysinfo;
    check("pile initiale sous 4 Gio, alignee", ((u32)sp & 15) == 0 && (u32)sp < 0xffffe000u, (long)sp);
}

static void test_memory(void) {
    u32 cur = sc1(NR_brk, 0);
    u32 grown = sc1(NR_brk, cur + 65536);
    check("brk : agrandissement", grown == cur + 65536, grown);
    ((volatile u8 *)cur)[65535] = 7;
    check("brk : memoire utilisable", ((volatile u8 *)cur)[65535] == 7, 0);
    sc1(NR_brk, cur);

    long p = sys6(NR_mmap2, 0, 1 << 20, 3, 0x22, -1, 0);
    check("mmap2 anonyme", !is_err(p) && (u32)p + (1 << 20) <= 0xffffe000u, p);
    if (!is_err(p)) {
        ((volatile u32 *)p)[1000] = 0x12345678;
        check("mmap2 : lecture/ecriture", ((volatile u32 *)p)[1000] == 0x12345678, 0);
        check("munmap", sc2(NR_munmap, p, 1 << 20) == 0, 0);
    }
    u32 args[6] = {0, 8192, 3, 0x22, (u32)-1, 0};
    long q = sc1(NR_mmap, args);
    check("old_mmap (structure)", !is_err(q), q);
    long f = sys6(NR_mmap2, 0xffffd000u, 0x2000, 3, 0x32, -1, 0);
    check("mmap fixe au-dela de TASK_SIZE refuse", is_err(f), f);
    /* 2,5 Gio puis 2 Gio : le second ne tient plus sous 4 Gio. */
    long big = sys6(NR_mmap2, 0, 0xa0000000u, 0, 0x22, -1, 0);
    long big2 = sys6(NR_mmap2, 0, 0x80000000u, 0, 0x22, -1, 0);
    check("mmap 2,5 Gio sous 4 Gio", !is_err(big), big);
    check("mmap au-dela de l'espace 32 bits : ENOMEM", big2 == -12, big2);
    if (!is_err(big)) sc2(NR_munmap, big, 0xa0000000u);
    if (!is_err(big2)) sc2(NR_munmap, big2, 0x80000000u);
}

static void test_tls(void) {
    struct user_desc d = {(u32)-1, (u32)tls_main, 0xfffff, 0x51};
    tls_main[0] = (u32)tls_main;
    long r = sc1(NR_set_thread_area, &d);
    check("set_thread_area (entree -1 -> 12)", r == 0 && d.entry_number == 12, r ? r : (long)d.entry_number);
    u32 sel = (d.entry_number << 3) | 3;
    __asm__ volatile("mov %0, %%gs" ::"r"(sel));
    check("%gs:0 = bloc TLS", gs_read0() == (u32)tls_main, gs_read0());
    struct user_desc g = {12, 0, 0, 0};
    r = sc1(NR_get_thread_area, &g);
    check("get_thread_area", r == 0 && g.base_addr == (u32)tls_main && g.limit == 0xfffff && (g.flags & 0x7f) == 0x51, g.flags);
    for (int i = 0; i < 200; i++) sc0(NR_sched_yield);
    struct ts32 ts = {0, 20000000};
    sc2(NR_nanosleep, &ts, 0);
    check("TLS et %gs gardes apres commutations", gs_read0() == (u32)tls_main && gs_sel() == sel, gs_sel());
}

static int thread_fn(void) {
    thread_gs0 = gs_read0();
    thread_tid = sc0(NR_gettid);
    /* Verrou robuste tenu à la fin du fil : le noyau doit le marquer FUTEX_OWNER_DIED. */
    rentry.word = thread_tid;
    rentry.next = (u32)&rhead;
    rhead.next = (u32)&rentry;
    rhead.off = 4;
    rhead.pending = 0;
    sc2(NR_set_robust_list, &rhead, 12);
    return 0;
}

static void test_thread(void) {
    check("set_robust_list : taille 24 refusee", sc2(NR_set_robust_list, &rhead, 24) == -22, 0);
    struct user_desc d = {12, (u32)tls_thread, 0xfffff, 0x51};
    tls_thread[0] = (u32)tls_thread;
    child_tid_word = 1;
    long flags = 0x100 | 0x200 | 0x400 | 0x800 | 0x10000 | 0x40000 | 0x80000 | 0x100000 | 0x200000;
    long tid = clone_thread(flags, thread_stack + sizeof thread_stack, (void *)&parent_tid_word, &d, (void *)&child_tid_word, thread_fn);
    check("clone (fil)", tid > 0, tid);
    if (tid <= 0) return;
    for (int i = 0; i < 1000 && child_tid_word; i++) sc4(NR_futex, &child_tid_word, 0, child_tid_word, 0);
    check("CLONE_CHILD_CLEARTID + futex", child_tid_word == 0, child_tid_word);
    check("CLONE_PARENT_SETTID", parent_tid_word == (u32)tid, parent_tid_word);
    check("fil : TLS du user_desc", thread_gs0 == (u32)tls_thread, thread_gs0);
    check("fil : tid propre", thread_tid == (u32)tid, thread_tid);
    check("TLS du fil principal intact", gs_read0() == (u32)tls_main, gs_read0());
    check("liste robuste 32 bits : FUTEX_OWNER_DIED", rentry.word == 0x40000000u, rentry.word);
}

static void h_rt(int sig, u32 *info, u8 *uc) {
    h_sig = sig;
    h_signo = info[0];
    h_code = info[2];
    h_pid = info[3];
    h_gs = *(u32 *)(uc + 20);
    h_gs0 = gs_read0();
    h_count++;
    /* Écrase xmm0 et la pile x87 : le retour doit les rétablir. */
    __asm__ volatile("pxor %%xmm0, %%xmm0\n fldz\n fstp %%st(0)" ::: "xmm0");
}

static void h_old(int sig) { h_sig = sig; h_count++; }

static void h_segv(int sig, u32 *info, u8 *uc) {
    u8 *sc = uc + 20;
    h_sig = sig;
    h_addr = info[3];
    h_trapno = *(u32 *)(sc + 48);
    h_cr2 = *(u32 *)(sc + 84);
    h_eip = *(u32 *)(sc + 56);
    *(u32 *)(sc + 56) = (u32)fault_next;
}

static void h_alt(int sig, u32 *info, u8 *uc) {
    u32 sp = (u32)&sig;
    u32 ss[3];
    (void)info; (void)uc;
    sc2(NR_sigaltstack, 0, ss);
    h_onstack = sp >= alt_lo && sp < alt_hi && ss[1] == 1;
}

static void test_signals(void) {
    struct ksigaction a = {(u32)h_rt, SA_SIGINFO | SA_RESTORER, (u32)restore_rt, {0, 0}}, old;
    check("rt_sigaction i386", sc4(NR_rt_sigaction, SIGUSR1, &a, &old, 8) == 0, 0);
    struct ksigaction back;
    sc4(NR_rt_sigaction, SIGUSR1, 0, &back, 8);
    check("rt_sigaction : relecture (sans drapeau interne)", back.handler == (u32)h_rt && back.flags == (SA_SIGINFO | SA_RESTORER) && back.restorer == (u32)restore_rt, back.flags);
    static const u32 pat[4] = {0x11111111, 0x22222222, 0x33333333, 0x44444444};
    u32 after[4];
    __asm__ volatile("movdqu %0, %%xmm0" ::"m"(pat) : "xmm0");
    long r = sc2(NR_kill, sc0(NR_getpid), SIGUSR1);
    __asm__ volatile("movdqu %%xmm0, %0" : "=m"(after));
    check("kill + rt_sigreturn : eax", r == 0, r);
    check("cadre rt : sig, si_signo, SI_USER, si_pid", h_sig == SIGUSR1 && h_signo == SIGUSR1 && h_code == 0 && h_pid == sc0(NR_getpid), h_code);
    check("cadre rt : gs du sigcontext, TLS dans le gestionnaire", (h_gs & 0xffff) == 0x63 && h_gs0 == (u32)tls_main, h_gs);
    check("etat SSE retabli apres le gestionnaire", mcmp(after, pat, 16) == 0, after[0]);

    struct ksigaction o = {(u32)h_old, SA_RESTORER, (u32)restore_old, {0, 0}};
    sc4(NR_rt_sigaction, SIGUSR2, &o, 0, 8);
    h_sig = 0;
    sc3(NR_tgkill, sc0(NR_getpid), sc0(NR_gettid), SIGUSR2);
    check("cadre sigframe (sans SA_SIGINFO) + sigreturn", h_sig == SIGUSR2, h_sig);

    /* sigprocmask 32 bits : bloqué → en attente → débloqué → livré. */
    u32 m = 1u << (SIGUSR2 - 1), oldm = 0;
    h_sig = 0;
    sc3(NR_sigprocmask, 0, &m, &oldm);
    sc2(NR_kill, sc0(NR_getpid), SIGUSR2);
    u32 pend[2] = {0, 0};
    sc2(NR_rt_sigpending, pend, 8);
    check("sigprocmask : signal en attente", h_sig == 0 && (pend[0] & m), pend[0]);
    sc3(NR_sigprocmask, 1, &m, 0);
    check("sigprocmask : livre au deblocage", h_sig == SIGUSR2, h_sig);

    struct ksigaction s = {(u32)h_segv, SA_SIGINFO | SA_RESTORER, (u32)restore_rt, {0, 0}};
    sc4(NR_rt_sigaction, SIGSEGV, &s, 0, 8);
    do_fault();
    check("SIGSEGV : si_addr, trapno 14, cr2, eip", h_sig == SIGSEGV && h_addr == 0 && h_trapno == 14 && h_cr2 == 0 && h_eip == (u32)fault_insn, h_eip);

    long st = sys6(NR_mmap2, 0, 65536, 3, 0x22, -1, 0);
    u32 ss[3] = {(u32)st, 0, 65536};
    alt_lo = (u32)st;
    alt_hi = (u32)st + 65536;
    check("sigaltstack i386", sc2(NR_sigaltstack, ss, 0) == 0, 0);
    struct ksigaction al = {(u32)h_alt, SA_SIGINFO | SA_RESTORER | SA_ONSTACK, (u32)restore_rt, {0, 0}};
    sc4(NR_rt_sigaction, SIGUSR1, &al, 0, 8);
    sc2(NR_kill, sc0(NR_getpid), SIGUSR1);
    check("gestionnaire sur la pile alternative (SS_ONSTACK)", h_onstack, 0);
}

static void test_process(char *self) {
    long pid = sc0(NR_fork);
    if (pid == 0) sc1(NR_exit_group, 42);
    u32 ru[20];
    mset(ru, 0xee, sizeof ru);
    int st = 0;
    long w = sc4(NR_wait4, pid, &st, 0, ru);
    check("fork + wait4 : statut 42", w == pid && (st >> 8) == 42, st);
    check("wait4 : rusage 32 bits (72 octets)", ru[18] == 0xeeeeeeeeu && ru[17] != 0xeeeeeeeeu, ru[18]);

    pid = sc0(NR_fork);
    if (pid == 0) {
        char *argv[] = {self, "exec", 0};
        char *envp[] = {"IA32=1", 0};
        sc3(NR_execve, self, argv, envp);
        sc1(NR_exit_group, 99);
    }
    st = 0;
    sc3(NR_waitpid, pid, &st, 0);
    check("execve 32 -> 32 bits (argv, envp)", (st >> 8) == 5, st);

    pid = sc0(NR_fork);
    if (pid == 0) {
        char *argv[] = {"dash", "-c", "exit 3", 0};
        char *envp[] = {0};
        sc3(NR_execve, "/extra_files/linux/glibc/dash", argv, envp);
        sc1(NR_exit_group, 98);
    }
    st = 0;
    sc3(NR_waitpid, pid, &st, 0);
    check("execve 32 -> 64 bits (dash de la glibc)", (st >> 8) == 3, st);
}

static void test_time(void) {
    struct ts32 a = {0, 0};
    struct ts64 b = {0, 0};
    check("clock_gettime (time32)", sc2(NR_clock_gettime, 1, &a) == 0 && a.nsec >= 0 && a.nsec < 1000000000, a.nsec);
    check("clock_gettime64", sc2(NR_clock_gettime64, 1, &b) == 0 && b.sec >= a.sec && b.sec - a.sec < 2, (long)b.sec);
    i32 tv[2];
    sc2(NR_gettimeofday, tv, 0);
    long t = sc1(NR_time, 0);
    check("gettimeofday et time (32 bits)", tv[1] >= 0 && tv[1] < 1000000 && t > 1700000000 && tv[0] - t < 2, tv[1]);
    struct ts64 s = {0, 30000000}, c, d;
    sc2(NR_clock_gettime64, 1, &c);
    sc4(NR_clock_nanosleep_time64, 1, 0, &s, 0);
    sc2(NR_clock_gettime64, 1, &d);
    i64 ns = (d.sec - c.sec) * 1000000000LL + (d.nsec - c.nsec);
    check("clock_nanosleep_time64 30 ms", ns >= 30000000LL && ns < 2000000000LL, (long)(ns >> 10));
}

static void test_files(void) {
    const char *path = "/tmp/ia32test.dat";
    long fd = sc3(NR_open, path, 0x42 | 0x200, 0644);
    check("open O_CREAT", fd >= 0, fd);
    if (fd < 0) return;
    char data[100];
    for (int i = 0; i < 100; i++) data[i] = (char)('A' + i % 26);
    check("write 100", sc3(NR_write, fd, data, 100) == 100, 0);
    u32 res[2] = {0, 0};
    check("_llseek", sc5(NR_llseek, fd, 0, 10, res, 0) == 0 && res[0] == 10 && res[1] == 0, res[0]);
    char buf[8];
    sc3(NR_read, fd, buf, 5);
    check("read apres _llseek", mcmp(buf, data + 10, 5) == 0, 0);
    check("pread64 (decalage en deux registres)", sc5(NR_pread64, fd, buf, 4, 20, 0) == 4 && mcmp(buf, data + 20, 4) == 0, 0);
    u8 st[96 + 8];
    mset(st, 0xee, sizeof st);
    sc2(NR_fstat64, fd, st);
    check("fstat64 : taille et type", *(i64 *)(st + 44) == 100 && (*(u32 *)(st + 16) & 0170000) == 0100000 && st[96] == 0xee, *(u32 *)(st + 44));
    check("fstat64 : st_ino = __st_ino", *(u32 *)(st + 12) == *(u32 *)(st + 88), 0);
    check("ftruncate64", sc3(NR_ftruncate64, fd, 50, 0) == 0, 0);
    sc2(NR_stat64, path, st);
    check("stat64 apres ftruncate64", *(i64 *)(st + 44) == 50, *(u32 *)(st + 44));
    u8 old[64 + 8];
    mset(old, 0xee, sizeof old);
    sc2(NR_fstat, fd, old);
    check("fstat (ancien format 32 bits)", *(u32 *)(old + 20) == 50 && old[64] == 0xee, *(u32 *)(old + 20));
    u8 sx[256];
    check("statx", sc5(NR_statx, -100, path, 0, 0x7ff, sx) == 0 && *(u64 *)(sx + 40) == 50, 0);

    struct flock64_32 fl = {1 /* F_WRLCK */, 0, 0, 10, 0};
    check("fcntl64 F_SETLK64", sc3(NR_fcntl64, fd, 13, &fl) == 0, 0);
    long me = sc0(NR_getpid);
    long pid = sc0(NR_fork);
    if (pid == 0) {
        struct flock64_32 q = {1, 0, 5, 1, 0};
        long r = sc3(NR_fcntl64, fd, 12, &q);
        sc1(NR_exit_group, r == 0 && q.type == 1 && q.start == 0 && q.len == 10 && q.pid == me ? 0 : 1);
    }
    int ws = 0;
    sc3(NR_waitpid, pid, &ws, 0);
    check("fcntl64 F_GETLK64 (flock64 groupee) vu d'un enfant", ws == 0, ws);
    sc1(NR_close, fd);
    check("unlink", sc1(NR_unlink, path) == 0, 0);
}

static void test_iov_sockets(void) {
    int p[2];
    sc1(NR_pipe, p);
    struct iov32 v[3] = {{(u32)"abc", 3}, {(u32)"defg", 4}, {(u32)"hi", 2}};
    check("writev iovec 32 bits", sc3(NR_writev, p[1], v, 3) == 9, 0);
    char a[4], b[5] = {0};
    struct iov32 r[2] = {{(u32)a, 4}, {(u32)b, 5}};
    check("readv iovec 32 bits", sc3(NR_readv, p[0], r, 2) == 9 && mcmp(a, "abcd", 4) == 0 && mcmp(b, "efghi", 5) == 0, 0);

    int sv[2];
    u32 args[4] = {1 /* AF_UNIX */, 1 /* SOCK_STREAM */, 0, (u32)sv};
    check("socketcall(SOCKETPAIR)", sc2(NR_socketcall, 8, args) == 0, 0);
    u32 sargs[4] = {(u32)sv[0], (u32)"ping", 4, 0};
    check("socketcall(SEND)", sc2(NR_socketcall, 9, sargs) == 4, 0);
    char rb[8];
    u32 rargs[4] = {(u32)sv[1], (u32)rb, 8, 0};
    check("socketcall(RECV)", sc2(NR_socketcall, 10, rargs) == 4 && mcmp(rb, "ping", 4) == 0, 0);

    /* SCM_RIGHTS : en-tête cmsghdr de 12 octets, alignement de 4. */
    u32 ctl[4] = {16, 1 /* SOL_SOCKET */, 1 /* SCM_RIGHTS */, (u32)p[1]};
    struct iov32 one = {(u32)"x", 1};
    struct msghdr32 m = {0, 0, (u32)&one, 1, (u32)ctl, 16, 0};
    u32 margs[3] = {(u32)sv[0], (u32)&m, 0};
    check("sendmsg msghdr 32 bits + SCM_RIGHTS", sc2(NR_socketcall, 16, margs) == 1, 0);
    u32 rctl[8];
    mset(rctl, 0, sizeof rctl);
    char c;
    struct iov32 ro = {(u32)&c, 1};
    struct msghdr32 rm = {0, 0, (u32)&ro, 1, (u32)rctl, sizeof rctl, 0};
    u32 rmargs[3] = {(u32)sv[1], (u32)&rm, 0};
    long n = sc2(NR_socketcall, 17, rmargs);
    check("recvmsg : cmsghdr 32 bits", n == 1 && rctl[0] == 16 && rctl[1] == 1 && rctl[2] == 1 && rm.controllen == 16, rctl[0]);
    long nfd = (long)rctl[3];
    sc3(NR_write, nfd, "Z", 1);
    char z = 0;
    sc3(NR_read, p[0], &z, 1);
    check("descripteur recu utilisable", z == 'Z', z);
}

static void test_misc(void) {
    char u[6 * 65];
    sc1(NR_uname, u);
    check("uname : sysname Linux, machine x86_64", seq(u, "Linux") && seq(u + 4 * 65, "x86_64"), 0);
    u32 lim[2] = {0, 0};
    check("ugetrlimit(RLIMIT_STACK) 32 bits", sc2(NR_ugetrlimit, 3, lim) == 0 && lim[0] == 8u << 20 && lim[1] == 0xffffffffu, lim[1]);
    u64 l64[2];
    check("prlimit64", sc4(NR_prlimit64, 0, 7, 0, l64) == 0 && l64[0] > 0, 0);
    check("appel inconnu : ENOSYS", sc0(999) == -38, 0);
}

/* ---- vDSO i386 : image ELF32, __kernel_vsyscall, horloges ---------------------------------- */

static u32 vdso_sym(const char *name) {
    u8 *b = (u8 *)vdso_ehdr;
    u32 phoff = *(u32 *)(b + 28), dyn = 0;
    u16 phnum = *(u16 *)(b + 44);
    for (u32 i = 0; i < phnum; i++) {
        u32 *ph = (u32 *)(b + phoff + i * 32);
        if (ph[0] == 2) dyn = ph[2];
    }
    if (!dyn) return 0;
    u32 sym = 0, str = 0, hash = 0;
    for (u32 *d = (u32 *)(b + dyn); d[0]; d += 2) {
        if (d[0] == 4) hash = d[1];
        if (d[0] == 5) str = d[1];
        if (d[0] == 6) sym = d[1];
    }
    u32 n = ((u32 *)(b + hash))[1];
    for (u32 i = 1; i < n; i++) {
        u32 *s = (u32 *)(b + sym + i * 16);
        if (seq((const char *)(b + str + s[0]), name)) return vdso_ehdr + s[1];
    }
    return 0;
}

typedef int (*gt64_fn)(int, struct ts64 *);
typedef int (*gt32_fn)(int, struct ts32 *);
typedef int (*gtod_fn)(i32 *, void *);
typedef i32 (*time_fn)(i32 *);

static void test_vdso(void) {
    u8 *e = (u8 *)vdso_ehdr;
    check("vDSO : AT_SYSINFO_EHDR -> ELF32 i386", e && e[0] == 0x7f && e[1] == 'E' && e[4] == 1 && *(u16 *)(e + 18) == 3, (long)vdso_ehdr);
    if (!e) return;
    u32 vs = vdso_sym("__kernel_vsyscall");
    check("vDSO : AT_SYSINFO = __kernel_vsyscall", vs != 0 && vs == vdso_entry, (long)vdso_entry);
    if (!vdso_entry) return;
    check("__kernel_vsyscall : getpid", vsys6(vdso_entry, NR_getpid, 0, 0, 0, 0, 0, 0) == sc0(NR_getpid), 0);
    /* 6 arguments : mmap2 d'une page de fichier (2e argument = longueur, 6e = décalage en pages). */
    const char *path = "/tmp/ia32vs.dat";
    static char pg[8192];
    long fd = sc3(NR_open, path, 0x42 | 0x200, 0644);
    mset(pg, 'a', 4096);
    mset(pg + 4096, 'Q', 4096);
    sc3(NR_write, fd, pg, 8192);
    long m = vsys6(vdso_entry, NR_mmap2, 0, 4096, 1 /* PROT_READ */, 2 /* MAP_PRIVATE */, fd, 1);
    check("__kernel_vsyscall : mmap2 a 6 arguments", !is_err(m) && *(volatile char *)m == 'Q', m);
    if (!is_err(m)) sc2(NR_munmap, m, 4096);
    sc1(NR_close, fd);
    sc1(NR_unlink, path);

    gt64_fn g64 = (gt64_fn)vdso_sym("__vdso_clock_gettime64");
    gt32_fn g32 = (gt32_fn)vdso_sym("__vdso_clock_gettime");
    gtod_fn gtod = (gtod_fn)vdso_sym("__vdso_gettimeofday");
    time_fn tm = (time_fn)vdso_sym("__vdso_time");
    check("vDSO : symboles d'horloge", g64 && g32 && gtod && tm, 0);
    if (!(g64 && g32 && gtod && tm)) return;
    struct ts64 a, c, rs, rv;
    struct ts32 b;
    sc2(NR_clock_gettime64, 1, &a);
    int r = g64(1, &c);
    check("__vdso_clock_gettime64 MONOTONIC suit l'appel systeme", r == 0 && (c.sec > a.sec || (c.sec == a.sec && c.nsec >= a.nsec)) && c.sec - a.sec < 2 && c.nsec < 1000000000, (long)c.nsec);
    sc2(NR_clock_gettime64, 0, &rs);
    g64(0, &rv);
    i64 dn = (rv.sec - rs.sec) * 1000000000LL + (rv.nsec - rs.nsec);
    check("__vdso_clock_gettime64 REALTIME : ecart < 10 ms", dn >= 0 && dn < 10000000LL, (long)(dn >> 10));
    check("__vdso_clock_gettime (time32)", g32(0, &b) == 0 && b.sec >= (i32)rs.sec && b.nsec >= 0 && b.nsec < 1000000000, b.nsec);
    i32 tv[2], tt = 0;
    check("__vdso_gettimeofday et __vdso_time", gtod(tv, 0) == 0 && tv[1] >= 0 && tv[1] < 1000000 && tm(&tt) >= tv[0] && tt - tv[0] < 2, tv[1]);
    check("__vdso_clock_gettime64 : horloge CPU par l'appel systeme", g64(2 /* PROCESS_CPUTIME */, &c) == 0 && c.nsec < 1000000000, 0);
}

/* Structures et appels propres à i386 que la glibc n'emploie plus (ou plus directement). */
static volatile u32 tm_sig, tm_code, tm_val;
static void h_timer(int sig, u32 *info, u8 *uc) {
    (void)uc;
    tm_sig = (u32)sig;
    tm_code = info[2];
    tm_val = info[5]; /* compat_siginfo : _timer { tid, overrun, sigval } à partir de 12 */
}

static void test_compat_structs(void) {
    /* select historique (82) : sel_arg_struct ; fd_set de mots de 32 bits, rien au-delà. */
    int p[2];
    sc1(NR_pipe, p);
    sc3(NR_write, p[1], "s", 1);
    u32 set[2] = {1u << p[0], 0xdeadbeef};
    u32 sel[5] = {(u32)p[0] + 1, (u32)set, 0, 0, 0};
    long r = sc1(82, sel);
    check("select (82) : fd_set 32 bits, mot suivant intact", r == 1 && set[0] == 1u << p[0] && set[1] == 0xdeadbeef, r);
    char c;
    sc3(NR_read, p[0], &c, 1);
    i32 tv[3] = {0, 20000, 0x5a5a5a5a};
    set[0] = 1u << p[0];
    r = sys6(142, p[0] + 1, (long)set, 0, 0, (long)tv, 0);
    check("_newselect (142) : timeval 32 bits, delai epuise", r == 0 && set[0] == 0 && tv[0] == 0 && tv[1] == 0 && tv[2] == 0x5a5a5a5a, r);

    /* getdents (141) : compat_linux_dirent, d_type dans le dernier octet. */
    long d = sc3(NR_open, "/tmp", 0x10000 /* O_DIRECTORY */, 0);
    u8 de[512];
    r = sc3(141, d, de, sizeof de);
    u16 reclen = *(u16 *)(de + 8);
    check("getdents (141) : '.' puis DT_DIR en fin d'enregistrement", r > 0 && (reclen & 3) == 0 && de[10] == '.' && de[11] == 0 && de[reclen - 1] == 4, r);
    sc1(NR_close, d);

    /* statfs (99, 64 octets) et statfs64 (268, taille 84). */
    u8 sf[84 + 4];
    mset(sf, 0xee, sizeof sf);
    check("statfs (99) : compat_statfs", sc2(99, "/", sf) == 0 && *(u32 *)(sf + 4) == 4096 && sf[64] == 0xee, *(u32 *)(sf + 4));
    mset(sf, 0xee, sizeof sf);
    check("statfs64 (268) : 84 octets groupes", sc3(268, "/", 84, sf) == 0 && *(u32 *)(sf + 4) == 4096 && *(u64 *)(sf + 8) > 0 && *(u32 *)(sf + 60) == 4096 && sf[84] == 0xee, *(u32 *)(sf + 60));
    check("statfs64 : mauvaise taille -> EINVAL", sc3(268, "/", 88, sf) == -22, 0);

    /* sysinfo (116) : compat_sysinfo de 64 octets ; times (43) ; getrusage (77). */
    u8 si[64 + 4];
    mset(si, 0xee, sizeof si);
    check("sysinfo : compat_sysinfo", sc1(116, si) == 0 && *(u32 *)(si + 16) > 0 && *(u32 *)(si + 52) >= 1 && *(u16 *)(si + 40) >= 1 && si[64] == 0xee, *(u32 *)(si + 52));
    u32 tms[5] = {0, 0, 0, 0, 0xabcdabcd};
    check("times : compat_tms", sc1(43, tms) > 0 && tms[4] == 0xabcdabcd, tms[4]);
    u32 ru[19];
    ru[18] = 0x12345678;
    check("getrusage : compat_rusage", sc2(77, 0, ru) == 0 && ru[1] < 1000000 && ru[18] == 0x12345678, ru[18]);

    /* setitimer/getitimer (104/105) : itimerval 32 bits. */
    i32 it[4] = {0, 0, 5, 0}, cur[5];
    cur[4] = 0x77777777;
    sc3(104, 0, it, 0);
    sc2(105, 0, cur);
    check("setitimer/getitimer : itimerval 32 bits", cur[0] == 0 && cur[2] >= 4 && cur[2] <= 5 && cur[3] >= 0 && cur[3] < 1000000 && cur[4] == 0x77777777, cur[2]);
    i32 off[4] = {0, 0, 0, 0};
    sc3(104, 0, off, 0);

    /* timer_create (259) : compat_sigevent ; timer_settime (260) : itimerspec 32 bits. */
    struct ksigaction sa = {(u32)h_timer, SA_SIGINFO | SA_RESTORER, (u32)restore_rt, {0, 0}};
    sc4(NR_rt_sigaction, SIGUSR2, &sa, 0, 8);
    u32 sev[16] = {0x1234, SIGUSR2, 0 /* SIGEV_SIGNAL */};
    i32 tid = -1;
    check("timer_create (compat_sigevent)", sc3(259, 1, sev, &tid) == 0 && tid >= 0, tid);
    i32 its[4] = {0, 0, 0, 20000000};
    check("timer_settime (itimerspec 32 bits)", sc4(260, tid, 0, its, 0) == 0, 0);
    struct ts32 w = {0, 150000000};
    sc2(NR_nanosleep, &w, 0);
    check("signal du minuteur : SI_TIMER et sigval", tm_sig == SIGUSR2 && tm_code == (u32)-2 && tm_val == 0x1234, tm_val);
    i32 long_its[4] = {1, 0, 30, 0}, g[5];
    g[4] = 0x31313131;
    sc4(260, tid, 0, long_its, 0);
    check("timer_gettime (32 bits)", sc2(261, tid, g) == 0 && g[0] == 1 && g[2] >= 29 && g[2] <= 30 && g[4] == 0x31313131, g[2]);
    sc1(263, tid);

    /* timerfd_settime (325) : itimerspec 32 bits. */
    long tfd = sc2(322, 1, 0);
    i32 tits[4] = {0, 0, 0, 10000000};
    u64 exp = 0;
    check("timerfd_settime (32 bits) puis read", sc4(325, tfd, 0, tits, 0) == 0 && sc3(NR_read, tfd, &exp, 8) == 8 && exp == 1, (long)exp);
    sc1(NR_close, tfd);

    /* sendfile (187) : compat_off_t. */
    long f = sc3(NR_open, "/tmp/ia32sf.dat", 0x42 | 0x200, 0644);
    sc3(NR_write, f, "0123456789abcdefghij", 20);
    i32 sfo[2] = {10, 0x6b6b6b6b};
    char sb[8] = {0};
    check("sendfile (187) : decalage 32 bits", sc4(187, p[1], f, sfo, 5) == 5 && sfo[0] == 15 && sfo[1] == 0x6b6b6b6b && sc3(NR_read, p[0], sb, 8) == 5 && mcmp(sb, "abcde", 5) == 0, sfo[0]);
    sc1(NR_close, f);
    sc1(NR_unlink, "/tmp/ia32sf.dat");

    /* waitid (284) : compat_siginfo de l'enfant. */
    long pid = sc0(NR_fork);
    if (pid == 0) sc1(NR_exit_group, 7);
    u32 info[32 + 1];
    info[32] = 0x99999999;
    r = sc5(284, 1 /* P_PID */, pid, info, 4 /* WEXITED */, 0);
    check("waitid : compat_siginfo (CLD_EXITED, pid, statut)", r == 0 && info[0] == 17 && info[2] == 1 && info[3] == (u32)pid && info[5] == 7 && info[32] == 0x99999999, info[5]);

    /* rt_sigtimedwait (177) : compat_siginfo, timespec 32 bits. */
    u32 m[2] = {1u << (SIGUSR1 - 1), 0}, om[2];
    sc4(NR_rt_sigprocmask, 0 /* SIG_BLOCK */, m, om, 8);
    sc3(NR_tgkill, sc0(NR_getpid), sc0(NR_gettid), SIGUSR1);
    u32 wi[33];
    wi[32] = 0x42424242;
    struct ts32 to = {1, 0};
    r = sc4(177, m, wi, &to, 8);
    check("rt_sigtimedwait (32 bits) : compat_siginfo", r == SIGUSR1 && wi[0] == SIGUSR1 && wi[2] == (u32)-6 /* SI_TKILL */ && wi[32] == 0x42424242, r);
    sc4(NR_rt_sigprocmask, 2 /* SIG_SETMASK */, om, 0, 8);

    /* nice (34) : monte le nice de 1. */
    long before = sys6(96, 0, 0, 0, 0, 0, 0); /* getpriority : 20 - nice */
    check("nice(1)", sc1(34, 1) == 0 && sys6(96, 0, 0, 0, 0, 0, 0) == before - 1, before);
    sc1(NR_close, p[0]);
    sc1(NR_close, p[1]);
}

int cmain(u32 *sp) {
    char **argv = (char **)(sp + 1);
    if (sp[0] >= 2 && seq(argv[1], "exec")) {
        char **envp = argv + sp[0] + 1;
        int good = envp[0] && seq(envp[0], "IA32=1");
        out("ia32test : execve recu\n");
        return good ? 5 : 6;
    }
    out("ia32test : debut (pid ");
    out_u(sc0(NR_getpid));
    out(")\n");
    test_auxv(sp);
    test_vdso();
    test_memory();
    test_tls();
    test_thread();
    test_signals();
    test_process(argv[0]);
    test_time();
    test_files();
    test_iov_sockets();
    test_misc();
    test_compat_structs();
    out("ia32test : FIN ");
    out_u(ok_n);
    out(" ok, ");
    out_u(ko_n);
    out(" echec\n");
    return ko_n ? 1 : 0;
}
