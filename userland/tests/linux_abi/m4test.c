/* m4test — appels de MaiProc ajoutés pour les clients réels (GLib, Firefox, coreutils) :
 * inotify, minuteurs POSIX (timer_create), priorités (getpriority/setpriority, ioprio),
 * renameat2(RENAME_NOREPLACE), FIONREAD d'une socket UDP (taille du prochain datagramme),
 * mémoire partagée System V (shmget, shmat, shmdt, shmctl : MIT-SHM de X11).
 * Compilé deux fois : musl statique (m4test) et glibc dynamique (m4test_glibc).
 *
 * Usage : m4test      tous les contrôles ; code de sortie = nombre d'échecs
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/shm.h>
#include <sys/wait.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#ifndef RENAME_NOREPLACE
#define RENAME_NOREPLACE 1
#endif

static int fails;

#define CHECK(cond, ...)                                  \
    do {                                                  \
        int ok_ = (cond);                                 \
        printf(ok_ ? "[ok] " : "[ECHEC] ");               \
        printf(__VA_ARGS__);                              \
        printf("\n");                                     \
        fflush(stdout);                                   \
        if (!ok_) fails++;                                \
    } while (0)

/* --- inotify ---------------------------------------------------------------------------- */

struct ev {
    int wd;
    uint32_t mask, cookie;
    char name[64];
};

/* Lit les événements disponibles (sans attendre) dans `out` ; rend leur nombre. */
static int drain(int fd, struct ev *out, int max) {
    char buf[4096] __attribute__((aligned(8)));
    int n = 0;
    for (;;) {
        ssize_t r = read(fd, buf, sizeof buf);
        if (r <= 0) break;
        for (char *p = buf; p < buf + r && n < max;) {
            struct inotify_event *e = (struct inotify_event *)p;
            out[n].wd = e->wd;
            out[n].mask = e->mask;
            out[n].cookie = e->cookie;
            snprintf(out[n].name, sizeof out[n].name, "%s", e->len ? e->name : "");
            n++;
            p += sizeof *e + e->len;
        }
    }
    return n;
}

static int find(struct ev *e, int n, int wd, uint32_t mask, const char *name) {
    for (int i = 0; i < n; i++)
        if (e[i].wd == wd && (e[i].mask & mask) == mask && (!name || !strcmp(e[i].name, name))) return i;
    return -1;
}

static void test_inotify(void) {
    char dir[64], f1[96], f2[96], sub[96];
    snprintf(dir, sizeof dir, "/tmp/m4ino.%d", getpid());
    snprintf(f1, sizeof f1, "%s/a.txt", dir);
    snprintf(f2, sizeof f2, "%s/b.txt", dir);
    snprintf(sub, sizeof sub, "%s/sous", dir);
    mkdir(dir, 0755);
    int fd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    CHECK(fd >= 0, "inotify_init1 = %d", fd);
    if (fd < 0) return;
    CHECK(fcntl(fd, F_GETFD) == FD_CLOEXEC, "inotify : IN_CLOEXEC");
    char small[8];
    CHECK(read(fd, small, sizeof small) < 0 && errno == EAGAIN, "inotify : file vide -> EAGAIN");
    int wd = inotify_add_watch(fd, dir, IN_ALL_EVENTS);
    CHECK(wd > 0, "inotify_add_watch(dossier) = %d", wd);
    CHECK(inotify_add_watch(fd, dir, IN_ALL_EVENTS) == wd, "inotify : meme inode, meme wd");
    CHECK(inotify_add_watch(fd, dir, IN_ALL_EVENTS | IN_MASK_CREATE) < 0 && errno == EEXIST, "inotify : IN_MASK_CREATE -> EEXIST");
    CHECK(inotify_add_watch(fd, dir, 0) < 0 && errno == EINVAL, "inotify : masque vide -> EINVAL");
    CHECK(inotify_add_watch(fd, "/tmp/m4ino-absent", IN_ALL_EVENTS) < 0 && errno == ENOENT, "inotify : chemin absent -> ENOENT");

    int f = open(f1, O_CREAT | O_WRONLY, 0644);
    write(f, "bonjour", 7);
    close(f);
    chmod(f1, 0600);
    int fwd = inotify_add_watch(fd, f1, IN_ALL_EVENTS);
    CHECK(fwd > 0 && fwd != wd, "inotify_add_watch(fichier) = %d", fwd);
    CHECK(inotify_add_watch(fd, f1, IN_ALL_EVENTS | IN_ONLYDIR) < 0 && errno == ENOTDIR, "inotify : IN_ONLYDIR sur un fichier -> ENOTDIR");
    int avail = -1;
    ioctl(fd, FIONREAD, &avail);
    CHECK(avail > 0 && avail % 16 == 0, "inotify : FIONREAD = %d octets", avail);
    CHECK(read(fd, small, sizeof small) < 0 && errno == EINVAL, "inotify : tampon trop petit -> EINVAL");
    struct pollfd p = { fd, POLLIN, 0 };
    CHECK(poll(&p, 1, 0) == 1 && (p.revents & POLLIN), "inotify : poll lisible");

    struct ev e[64];
    int n = drain(fd, e, 64);
    CHECK(find(e, n, wd, IN_CREATE, "a.txt") >= 0, "inotify : IN_CREATE a.txt (%d evenements)", n);
    CHECK(find(e, n, wd, IN_OPEN, "a.txt") >= 0, "inotify : IN_OPEN a.txt");
    CHECK(find(e, n, wd, IN_MODIFY, "a.txt") >= 0, "inotify : IN_MODIFY a.txt");
    CHECK(find(e, n, wd, IN_CLOSE_WRITE, "a.txt") >= 0, "inotify : IN_CLOSE_WRITE a.txt");
    CHECK(find(e, n, wd, IN_ATTRIB, "a.txt") >= 0, "inotify : IN_ATTRIB a.txt (chmod)");
    int c = find(e, n, wd, IN_CREATE, "a.txt"), o = find(e, n, wd, IN_OPEN, "a.txt");
    CHECK(c >= 0 && o > c, "inotify : IN_CREATE avant IN_OPEN");

    f = open(f1, O_RDONLY);
    read(f, small, 4);
    close(f);
    n = drain(fd, e, 64);
    CHECK(find(e, n, fwd, IN_ACCESS, "") >= 0, "inotify : IN_ACCESS sur le fichier (sans nom)");
    CHECK(find(e, n, wd, IN_ACCESS, "a.txt") >= 0, "inotify : IN_ACCESS au dossier (avec nom)");
    CHECK(find(e, n, fwd, IN_CLOSE_NOWRITE, "") >= 0, "inotify : IN_CLOSE_NOWRITE");

    CHECK(rename(f1, f2) == 0, "rename a.txt -> b.txt");
    n = drain(fd, e, 64);
    int from = find(e, n, wd, IN_MOVED_FROM, "a.txt"), to = find(e, n, wd, IN_MOVED_TO, "b.txt");
    CHECK(from >= 0 && to >= 0, "inotify : IN_MOVED_FROM a.txt, IN_MOVED_TO b.txt");
    CHECK(from >= 0 && to >= 0 && e[from].cookie && e[from].cookie == e[to].cookie, "inotify : meme cookie (%u)", from >= 0 ? e[from].cookie : 0);
    CHECK(find(e, n, fwd, IN_MOVE_SELF, "") >= 0, "inotify : IN_MOVE_SELF");

    mkdir(sub, 0755);
    n = drain(fd, e, 64);
    CHECK(find(e, n, wd, IN_CREATE | IN_ISDIR, "sous") >= 0, "inotify : IN_CREATE|IN_ISDIR sous");
    rmdir(sub);
    unlink(f2);
    n = drain(fd, e, 64);
    CHECK(find(e, n, wd, IN_DELETE | IN_ISDIR, "sous") >= 0, "inotify : IN_DELETE|IN_ISDIR sous");
    CHECK(find(e, n, wd, IN_DELETE, "b.txt") >= 0, "inotify : IN_DELETE b.txt");
    CHECK(find(e, n, fwd, IN_DELETE_SELF, "") >= 0, "inotify : IN_DELETE_SELF du fichier");
    CHECK(find(e, n, fwd, IN_IGNORED, "") >= 0, "inotify : IN_IGNORED apres suppression");
    CHECK(inotify_rm_watch(fd, fwd) < 0 && errno == EINVAL, "inotify_rm_watch(wd eteint) -> EINVAL");

    int one = inotify_add_watch(fd, dir, IN_CREATE | IN_ONESHOT);
    close(open(f1, O_CREAT | O_WRONLY, 0644));
    close(open(f2, O_CREAT | O_WRONLY, 0644));
    n = drain(fd, e, 64);
    int creates = 0;
    for (int i = 0; i < n; i++) creates += e[i].wd == one && (e[i].mask & IN_CREATE);
    CHECK(creates == 1 && find(e, n, one, IN_IGNORED, "") >= 0, "inotify : IN_ONESHOT (1 evenement puis IN_IGNORED)");
    unlink(f1);
    unlink(f2);

    wd = inotify_add_watch(fd, dir, IN_ALL_EVENTS);
    CHECK(inotify_rm_watch(fd, wd) == 0, "inotify_rm_watch = 0");
    n = drain(fd, e, 64);
    CHECK(find(e, n, wd, IN_IGNORED, "") >= 0, "inotify : IN_IGNORED apres inotify_rm_watch");
    close(fd);
    rmdir(dir);
}

/* --- minuteurs POSIX ---------------------------------------------------------------------- */

static int wait_sig(int sig, siginfo_t *si, long ms) {
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, sig);
    struct timespec ts = { ms / 1000, (ms % 1000) * 1000000L };
    return sigtimedwait(&s, si, &ts);
}

static void test_timers(void) {
    int sig = SIGRTMIN + 1;
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, sig);
    sigaddset(&s, SIGALRM);
    sigprocmask(SIG_BLOCK, &s, NULL);

    timer_t t;
    struct sigevent sev;
    memset(&sev, 0, sizeof sev);
    sev.sigev_notify = SIGEV_SIGNAL;
    sev.sigev_signo = sig;
    sev.sigev_value.sival_int = 4242;
    CHECK(timer_create(CLOCK_MONOTONIC, &sev, &t) == 0, "timer_create(CLOCK_MONOTONIC)");
    struct itimerspec its = { { 0, 0 }, { 0, 20000000 } }, cur;
    CHECK(timer_settime(t, 0, &its, NULL) == 0, "timer_settime 20 ms");
    timer_gettime(t, &cur);
    CHECK(cur.it_value.tv_sec == 0 && cur.it_value.tv_nsec > 0 && cur.it_value.tv_nsec <= 20000000, "timer_gettime : reste %ld ns", cur.it_value.tv_nsec);
    siginfo_t si;
    int r = wait_sig(sig, &si, 1000);
    CHECK(r == sig && si.si_code == SI_TIMER && si.si_value.sival_int == 4242, "signal du minuteur (code %d, valeur %d)", si.si_code, si.si_value.sival_int);
    timer_gettime(t, &cur);
    CHECK(cur.it_value.tv_sec == 0 && cur.it_value.tv_nsec == 0, "minuteur a un coup desarme");

    /* Périodique, signal bloqué pendant 100 ms : un seul signal en file, le reste en dépassement. */
    struct itimerspec per = { { 0, 5000000 }, { 0, 5000000 } };
    timer_settime(t, 0, &per, NULL);
    struct timespec nap = { 0, 100000000 };
    nanosleep(&nap, NULL);
    r = wait_sig(sig, &si, 1000);
    int ov = timer_getoverrun(t);
    CHECK(r == sig && si.si_overrun >= 5, "depassement : si_overrun = %d", si.si_overrun);
    CHECK(ov == si.si_overrun, "timer_getoverrun = si_overrun (%d)", ov);
    struct itimerspec zero = { { 0, 0 }, { 0, 0 } };
    timer_settime(t, 0, &zero, NULL);
    while (wait_sig(sig, &si, 30) == sig) {}
    CHECK(timer_delete(t) == 0, "timer_delete");
    /* musl rend la valeur brute de l'appel (-EINVAL) sans poser errno ; la glibc rend -1. */
    errno = 0;
    r = timer_delete(t);
    CHECK(r == -EINVAL || (r == -1 && errno == EINVAL), "timer_delete(efface) -> EINVAL");

    /* Échéance absolue sur CLOCK_REALTIME, sans `sigevent` (SIGALRM, valeur = identifiant). */
    timer_t t2;
    CHECK(timer_create(CLOCK_REALTIME, NULL, &t2) == 0, "timer_create(CLOCK_REALTIME, NULL)");
    struct timespec now;
    clock_gettime(CLOCK_REALTIME, &now);
    now.tv_nsec += 30000000;
    if (now.tv_nsec >= 1000000000) { now.tv_sec++; now.tv_nsec -= 1000000000; }
    struct itimerspec abs = { { 0, 0 }, now };
    CHECK(timer_settime(t2, TIMER_ABSTIME, &abs, NULL) == 0, "timer_settime TIMER_ABSTIME");
    r = wait_sig(SIGALRM, &si, 1000);
    CHECK(r == SIGALRM && si.si_code == SI_TIMER, "SIGALRM du minuteur absolu");
    timer_delete(t2);

    /* Minuteur de temps processeur. */
    timer_t t3;
    sev.sigev_value.sival_int = 7;
    CHECK(timer_create(CLOCK_PROCESS_CPUTIME_ID, &sev, &t3) == 0, "timer_create(CLOCK_PROCESS_CPUTIME_ID)");
    struct itimerspec c50 = { { 0, 0 }, { 0, 50000000 } };
    timer_settime(t3, 0, &c50, NULL);
    struct timespec a, b;
    clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &a);
    volatile unsigned long spin = 0;
    r = -1;
    for (int k = 0; k < 400 && r != sig; k++) {
        for (int j = 0; j < 1000000; j++) spin += j;
        r = wait_sig(sig, &si, 0);
    }
    clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &b);
    long used = (b.tv_sec - a.tv_sec) * 1000 + (b.tv_nsec - a.tv_nsec) / 1000000;
    CHECK(r == sig && si.si_value.sival_int == 7 && used >= 45, "minuteur de temps processeur (%ld ms de calcul)", used);
    timer_delete(t3);

    CHECK(syscall(SYS_timer_create, CLOCK_MONOTONIC_RAW, NULL, &t) < 0 && errno == EOPNOTSUPP, "CLOCK_MONOTONIC_RAW -> EOPNOTSUPP");
    sev.sigev_signo = 0;
    CHECK(timer_create(CLOCK_MONOTONIC, &sev, &t) < 0 && errno == EINVAL, "signal 0 -> EINVAL");
    sigprocmask(SIG_UNBLOCK, &s, NULL);
}

/* --- priorités ---------------------------------------------------------------------------- */

#define IOPRIO_CLASS_SHIFT 13
#define IOPRIO_PRIO_VALUE(c, l) (((c) << IOPRIO_CLASS_SHIFT) | (l))

static void test_prio(void) {
    errno = 0;
    CHECK(getpriority(PRIO_PROCESS, 0) == 0 && errno == 0, "getpriority = 0");
    CHECK(syscall(SYS_getpriority, PRIO_PROCESS, 0) == 20, "getpriority brut = 20 - nice");
    CHECK(setpriority(PRIO_PROCESS, 0, 5) == 0 && getpriority(PRIO_PROCESS, 0) == 5, "setpriority 5");
    CHECK(setpriority(PRIO_PROCESS, 0, 50) == 0 && getpriority(PRIO_PROCESS, 0) == 19, "nice borne a 19");
    if (geteuid() == 0)
        CHECK(setpriority(PRIO_PROCESS, 0, 0) == 0, "setpriority 0 (root : baisser le nice)");
    else
        CHECK(setpriority(PRIO_PROCESS, 0, 0) < 0 && errno == EACCES, "setpriority 0 sans root -> EACCES");
    /* Sans priorité posée : 0 (IOPRIO_CLASS_NONE, Linux 6). */
    long dflt = 0;
    CHECK(getpriority(PRIO_PGRP, 0) >= 0, "getpriority(PRIO_PGRP)");
    CHECK(getpriority(9, 0) == -1 && errno == EINVAL, "which invalide -> EINVAL");
    CHECK(getpriority(PRIO_PROCESS, 999999) == -1 && errno == ESRCH, "pid absent -> ESRCH");

    CHECK(syscall(SYS_ioprio_get, 1, 0) == dflt, "ioprio_get par defaut = 0 (classe NONE)");
    CHECK(syscall(SYS_ioprio_set, 1, 0, IOPRIO_PRIO_VALUE(2, 7)) == 0 && syscall(SYS_ioprio_get, 1, 0) == IOPRIO_PRIO_VALUE(2, 7), "ioprio_set BE/7");
    CHECK(syscall(SYS_ioprio_set, 1, 0, IOPRIO_PRIO_VALUE(3, 0)) == 0 && syscall(SYS_ioprio_get, 1, 0) == IOPRIO_PRIO_VALUE(3, 0), "ioprio_set IDLE");
    CHECK(syscall(SYS_ioprio_set, 1, 0, IOPRIO_PRIO_VALUE(0, 3)) < 0 && errno == EINVAL, "classe NONE avec niveau -> EINVAL");
    CHECK(syscall(SYS_ioprio_set, 1, 0, IOPRIO_PRIO_VALUE(5, 0)) < 0 && errno == EINVAL, "classe inconnue -> EINVAL");
    CHECK(syscall(SYS_ioprio_set, 1, 0, 0) == 0 && syscall(SYS_ioprio_get, 1, 0) == dflt, "classe NONE : retour au defaut");
    CHECK(syscall(SYS_ioprio_get, 9, 0) < 0 && errno == EINVAL, "ioprio_get(which invalide) -> EINVAL");
}

/* --- divers -------------------------------------------------------------------------------- */

static void test_misc(void) {
    char a[64], b[64];
    snprintf(a, sizeof a, "/tmp/m4ren_a.%d", getpid());
    snprintf(b, sizeof b, "/tmp/m4ren_b.%d", getpid());
    close(open(a, O_CREAT | O_WRONLY, 0644));
    close(open(b, O_CREAT | O_WRONLY, 0644));
    CHECK(syscall(SYS_renameat2, AT_FDCWD, a, AT_FDCWD, b, RENAME_NOREPLACE) < 0 && errno == EEXIST, "renameat2(RENAME_NOREPLACE) -> EEXIST");
    unlink(b);
    CHECK(syscall(SYS_renameat2, AT_FDCWD, a, AT_FDCWD, b, RENAME_NOREPLACE) == 0, "renameat2(RENAME_NOREPLACE) vers un nom libre");
    unlink(b);

    /* FIONREAD sur UDP : taille du prochain datagramme (le résolveur de la glibc s'en sert). */
    int rx = socket(AF_INET, SOCK_DGRAM, 0), tx = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in sa = { .sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK) };
    socklen_t sl = sizeof sa;
    bind(rx, (struct sockaddr *)&sa, sizeof sa);
    getsockname(rx, (struct sockaddr *)&sa, &sl);
    sendto(tx, "0123456789", 10, 0, (struct sockaddr *)&sa, sizeof sa);
    sendto(tx, "abcdefghijklmnopqrst", 20, 0, (struct sockaddr *)&sa, sizeof sa);
    struct pollfd p = { rx, POLLIN, 0 };
    poll(&p, 1, 1000);
    int n = -1;
    ioctl(rx, FIONREAD, &n);
    CHECK(n == 10, "FIONREAD UDP = %d (premier datagramme)", n);
    char buf[64];
    recv(rx, buf, sizeof buf, 0);
    ioctl(rx, FIONREAD, &n);
    CHECK(n == 20, "FIONREAD UDP = %d (second datagramme)", n);
    close(rx);
    close(tx);
}

/* --- mémoire partagée System V ------------------------------------------------------------ */

static void test_shm(void) {
    struct shmid_ds ds;
    int id = shmget(IPC_PRIVATE, 10000, IPC_CREAT | 0600);
    CHECK(id >= 0, "shmget(IPC_PRIVATE, 10000) -> %d", id);
    if (id < 0) return;
    CHECK(shmctl(id, IPC_STAT, &ds) == 0 && ds.shm_segsz == 10000 && ds.shm_nattch == 0 &&
          ds.shm_cpid == getpid() && (ds.shm_perm.mode & 0777) == 0600 && ds.shm_perm.uid == geteuid(),
          "IPC_STAT : taille %zu, nattch %lu, cpid %d, mode %o", (size_t)ds.shm_segsz,
          (unsigned long)ds.shm_nattch, (int)ds.shm_cpid, (unsigned)ds.shm_perm.mode);

    char *p = shmat(id, NULL, 0);
    CHECK(p != (void *)-1 && ((uintptr_t)p & 4095) == 0, "shmat -> %p", (void *)p);
    if (p == (void *)-1) return;
    CHECK(p[0] == 0 && p[9999] == 0, "pages mises a zero");
    strcpy(p, "bonjour");
    p[9999] = 42;
    shmctl(id, IPC_STAT, &ds);
    CHECK(ds.shm_nattch == 1 && ds.shm_atime != 0 && ds.shm_lpid == getpid(), "nattch 1 apres shmat");

    /* Une seconde attache voit les mêmes pages. */
    char *q = shmat(id, NULL, SHM_RDONLY);
    CHECK(q != (void *)-1 && q != p && strcmp(q, "bonjour") == 0 && q[9999] == 42, "seconde attache (lecture seule) partagee");
    CHECK(mprotect(q, 4096, PROT_READ | PROT_WRITE) == -1 && errno == EACCES, "mprotect(PROT_WRITE) d'une attache SHM_RDONLY -> EACCES");
    shmctl(id, IPC_STAT, &ds);
    CHECK(ds.shm_nattch == 2, "nattch 2 (deux attaches) : %lu", (unsigned long)ds.shm_nattch);
    CHECK(shmdt(q) == 0, "shmdt de la seconde attache");
    CHECK(shmdt(q) == -1 && errno == EINVAL, "shmdt d'une adresse detachee -> EINVAL");

    /* fork : l'enfant hérite de l'attache (nattch 2) et écrit dans le segment. */
    int pipefd[2];
    pipe(pipefd);
    pid_t c = fork();
    if (c == 0) {
        char b;
        read(pipefd[0], &b, 1);
        strcpy(p, "enfant");
        _exit(0);
    }
    shmctl(id, IPC_STAT, &ds);
    CHECK(ds.shm_nattch == 2, "nattch 2 apres fork : %lu", (unsigned long)ds.shm_nattch);
    write(pipefd[1], "x", 1);
    int st;
    waitpid(c, &st, 0);
    close(pipefd[0]);
    close(pipefd[1]);
    CHECK(strcmp(p, "enfant") == 0, "ecriture de l'enfant visible");
    shmctl(id, IPC_STAT, &ds);
    CHECK(ds.shm_nattch == 1, "nattch 1 apres la sortie de l'enfant : %lu", (unsigned long)ds.shm_nattch);

    /* Clés : création exclusive, taille, droits. */
    key_t key = 0x4d414953; /* « MAIS » */
    int k = shmget(key, 8192, IPC_CREAT | IPC_EXCL | 0640);
    CHECK(k >= 0, "shmget(cle, IPC_CREAT|IPC_EXCL) -> %d", k);
    CHECK(shmget(key, 8192, IPC_CREAT | IPC_EXCL | 0640) == -1 && errno == EEXIST, "seconde creation exclusive -> EEXIST");
    CHECK(shmget(key, 4096, 0) == k, "shmget(cle) retrouve le segment");
    CHECK(shmget(key, 16384, 0) == -1 && errno == EINVAL, "taille plus grande que le segment -> EINVAL");
    CHECK(shmget(0x4d414954, 4096, 0) == -1 && errno == ENOENT, "cle inconnue sans IPC_CREAT -> ENOENT");
    CHECK(shmget(IPC_PRIVATE, 0, IPC_CREAT | 0600) == -1 && errno == EINVAL, "taille nulle -> EINVAL");
    CHECK(shmat(k, (void *)0x10001, 0) == (void *)-1 && errno == EINVAL, "adresse non alignee sans SHM_RND -> EINVAL");
    /* Adresse libre sous 4 Gio en i386, au-delà en 64 bits. */
    uintptr_t want = sizeof(void *) == 8 ? (uintptr_t)0x7e0000001234ull : (uintptr_t)0x7e001234u;
    char *r = shmat(k, (void *)want, SHM_RND);
    CHECK(r == (void *)(want & ~(uintptr_t)0xfff), "SHM_RND arrondit l'adresse : %p", (void *)r);
    if (r != (void *)-1) shmdt(r);
    CHECK(shmctl(k, IPC_RMID, NULL) == 0, "IPC_RMID (cle)");
    CHECK(shmget(key, 4096, 0) == -1 && errno == ENOENT, "cle oubliee apres IPC_RMID");

    /* IPC_SET : droits. */
    ds.shm_perm.mode = 0644;
    CHECK(shmctl(id, IPC_SET, &ds) == 0, "IPC_SET");
    shmctl(id, IPC_STAT, &ds);
    CHECK((ds.shm_perm.mode & 0777) == 0644, "mode 0644 apres IPC_SET");

    /* IPC_RMID d'un segment attaché : marqué détruit, toujours lisible, détruit au shmdt. */
    CHECK(shmctl(id, IPC_RMID, NULL) == 0, "IPC_RMID d'un segment attache");
    CHECK(shmctl(id, IPC_STAT, &ds) == 0 && (ds.shm_perm.mode & SHM_DEST) && ds.shm_perm.__key == IPC_PRIVATE,
          "SHM_DEST pose, cle IPC_PRIVATE");
    CHECK(strcmp(p, "enfant") == 0, "segment detruit toujours lisible");
    CHECK(shmdt(p) == 0, "dernier shmdt");
    CHECK(shmctl(id, IPC_STAT, &ds) == -1 && errno == EINVAL, "IPC_STAT apres destruction -> EINVAL");
    CHECK(shmat(id, NULL, 0) == (void *)-1 && errno == EINVAL, "shmat apres destruction -> EINVAL");

    /* IPC_INFO. */
    struct shminfo info;
    CHECK(shmctl(0, IPC_INFO, (struct shmid_ds *)&info) >= 0 && info.shmmni == 4096 && info.shmmin == 1, "IPC_INFO : shmmni %lu", (unsigned long)info.shmmni);
}

int main(void) {
    printf("m4test : inotify, minuteurs, priorites (pid %d)\n", getpid());
    fflush(stdout);
    test_inotify();
    test_timers();
    test_prio();
    test_misc();
    test_shm();
    printf("m4test : %s (%d echec%s)\n", fails ? "ECHEC" : "OK", fails, fails > 1 ? "s" : "");
    return fails;
}
