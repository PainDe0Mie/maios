/* m3test — jalon M3 de MaiProc : vDSO, epoll, eventfd, timerfd, signalfd, pidfd, sockets Unix,
 * /proc. Compilé deux fois : musl statique (m3test) et glibc dynamique (m3test_glibc, chargé par
 * /lib64/ld-linux-x86-64.so.2).
 *
 * Usage : m3test      tous les contrôles ; code de sortie = nombre d'échecs
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <sched.h>
#include <stddef.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <dirent.h>
#include <sys/auxv.h>
#include <sys/epoll.h>
#include <sys/mman.h>
#include <sys/eventfd.h>
#include <sys/signalfd.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/timerfd.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef SYS_pidfd_open
#define SYS_pidfd_open 434
#endif
#ifndef SYS_pidfd_send_signal
#define SYS_pidfd_send_signal 424
#endif
#ifndef P_PIDFD
#define P_PIDFD 3
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

static long long ns_of(struct timespec t) { return t.tv_sec * 1000000000LL + t.tv_nsec; }

static long long mono_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return ns_of(t);
}

/* --- vDSO -------------------------------------------------------------------------------- */

static void test_vdso(void) {
    unsigned long ehdr = getauxval(AT_SYSINFO_EHDR);
    CHECK(ehdr != 0 && !memcmp((void *)ehdr, "\x7f" "ELF", 4), "AT_SYSINFO_EHDR -> image ELF du vDSO a %#lx", ehdr);
    struct timespec a, b;
    clock_gettime(CLOCK_REALTIME, &a);
    syscall(SYS_clock_gettime, CLOCK_REALTIME, &b);
    long long d = ns_of(b) - ns_of(a);
    CHECK(d >= 0 && d < 50000000LL, "CLOCK_REALTIME : vDSO et appel systeme d'accord (ecart %lld ns)", d);
    clock_gettime(CLOCK_MONOTONIC, &a);
    syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &b);
    d = ns_of(b) - ns_of(a);
    CHECK(d >= 0 && d < 50000000LL, "CLOCK_MONOTONIC : vDSO et appel systeme d'accord (ecart %lld ns)", d);
    struct timeval tv;
    gettimeofday(&tv, NULL);
    CHECK(tv.tv_sec > 1600000000 && tv.tv_usec >= 0 && tv.tv_usec < 1000000, "gettimeofday : %ld.%06ld", (long)tv.tv_sec, (long)tv.tv_usec);
    time_t tt = time(NULL);
    CHECK(tt >= tv.tv_sec && tt - tv.tv_sec < 3, "time() coherent avec gettimeofday");
    /* Monotonie et coût. */
    int mono_ok = 1;
    long long prev = mono_ns(), t0 = prev;
    const int N = 200000;
    for (int i = 0; i < N; i++) {
        long long n = mono_ns();
        if (n < prev) mono_ok = 0;
        prev = n;
    }
    long long per = (prev - t0) / N;
    long long s0 = mono_ns();
    for (int i = 0; i < 20000; i++) syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &a);
    long long per_sys = (mono_ns() - s0) / 20000;
    CHECK(mono_ok, "CLOCK_MONOTONIC croissante sur %d lectures", N);
    printf("     clock_gettime : %lld ns (vDSO) contre %lld ns (appel systeme)\n", per, per_sys);
    CHECK(per < per_sys, "vDSO plus rapide que l'appel systeme");
    int cpu = sched_getcpu();
    CHECK(cpu >= 0 && cpu < 1024, "sched_getcpu (vDSO getcpu) : coeur %d", cpu);
    /* Page de données scellée : mprotect en écriture refusé. */
    void *vvar = (void *)((ehdr & ~0xfffUL) - 4096);
    CHECK(mprotect(vvar, 4096, PROT_READ | PROT_WRITE) != 0, "mprotect(PROT_WRITE) sur [vvar] refuse (%s)", strerror(errno));
}

/* --- eventfd, timerfd, signalfd -------------------------------------------------------- */

static void test_eventfd(void) {
    int e = eventfd(3, EFD_NONBLOCK | EFD_CLOEXEC);
    uint64_t v = 0;
    CHECK(e >= 0 && read(e, &v, 8) == 8 && v == 3, "eventfd : valeur initiale 3 lue");
    CHECK(read(e, &v, 8) < 0 && errno == EAGAIN, "eventfd vide : EAGAIN");
    v = 5;
    write(e, &v, 8);
    v = 7;
    write(e, &v, 8);
    CHECK(read(e, &v, 8) == 8 && v == 12, "eventfd : ecritures cumulees (12)");
    close(e);
    e = eventfd(2, EFD_SEMAPHORE | EFD_NONBLOCK);
    uint64_t a = 0, b = 0;
    read(e, &a, 8);
    read(e, &b, 8);
    CHECK(a == 1 && b == 1 && read(e, &a, 8) < 0, "eventfd semaphore : 1 par lecture");
    close(e);
    /* Réveil d'un parent bloqué par un enfant. */
    e = eventfd(0, 0);
    pid_t c = fork();
    if (c == 0) {
        usleep(20000);
        uint64_t one = 42;
        write(e, &one, 8);
        _exit(0);
    }
    v = 0;
    ssize_t n = read(e, &v, 8);
    CHECK(n == 8 && v == 42, "eventfd bloquant reveille par un autre processus");
    waitpid(c, NULL, 0);
    close(e);
}

static void test_timerfd(void) {
    int t = timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC);
    struct itimerspec it = {{0, 10000000}, {0, 10000000}}; /* 10 ms, périodique */
    /* Origine prise à l'armement : l'affichage d'un CHECK peut prendre plusieurs ms. */
    long long t0 = mono_ns();
    int armed = t >= 0 && timerfd_settime(t, 0, &it, NULL) == 0;
    uint64_t ticks = 0, total = 0;
    char trace[256] = {0};
    int tl = 0;
    while (total < 5) {
        if (read(t, &ticks, 8) != 8) break;
        total += ticks;
        tl += snprintf(trace + tl, sizeof trace - tl, " %llu@%lldms", (unsigned long long)ticks, (mono_ns() - t0) / 1000000);
    }
    long long el = (mono_ns() - t0) / 1000000;
    CHECK(armed, "timerfd periodique 10 ms arme");
    CHECK(total >= 5 && el >= 45 && el < 1000, "timerfd : %llu echeances en %lld ms depuis l'armement (%s)", (unsigned long long)total, el, trace);
    struct itimerspec cur;
    timerfd_gettime(t, &cur);
    CHECK(cur.it_interval.tv_nsec == 10000000, "timerfd_gettime : intervalle conserve");
    /* Échéance absolue sur l'horloge réelle. */
    int r = timerfd_create(CLOCK_REALTIME, TFD_NONBLOCK);
    struct timespec now;
    clock_gettime(CLOCK_REALTIME, &now);
    struct itimerspec abs = {{0, 0}, {now.tv_sec, now.tv_nsec + 30000000}};
    if (abs.it_value.tv_nsec >= 1000000000) { abs.it_value.tv_sec++; abs.it_value.tv_nsec -= 1000000000; }
    timerfd_settime(r, TFD_TIMER_ABSTIME, &abs, NULL);
    CHECK(read(r, &ticks, 8) < 0 && errno == EAGAIN, "timerfd absolu pas encore echu : EAGAIN");
    struct pollfd p = {r, POLLIN, 0};
    int pr = poll(&p, 1, 1000);
    CHECK(pr == 1 && (p.revents & POLLIN) && read(r, &ticks, 8) == 8 && ticks == 1, "timerfd absolu : poll puis 1 echeance");
    close(r);
    close(t);
}

static void test_signalfd(void) {
    sigset_t m;
    sigemptyset(&m);
    sigaddset(&m, SIGUSR1);
    sigaddset(&m, SIGUSR2);
    sigprocmask(SIG_BLOCK, &m, NULL);
    int s = signalfd(-1, &m, SFD_NONBLOCK | SFD_CLOEXEC);
    struct signalfd_siginfo si;
    CHECK(s >= 0 && read(s, &si, sizeof si) < 0 && errno == EAGAIN, "signalfd vide : EAGAIN");
    kill(getpid(), SIGUSR1);
    memset(&si, 0, sizeof si);
    CHECK(read(s, &si, sizeof si) == sizeof si && si.ssi_signo == SIGUSR1 && si.ssi_pid == (uint32_t)getpid(),
          "signalfd : SIGUSR1 lu (pid emetteur %u)", si.ssi_pid);
    union sigval sv = {.sival_int = 777};
    sigqueue(getpid(), SIGUSR2, sv);
    CHECK(read(s, &si, sizeof si) == sizeof si && si.ssi_signo == SIGUSR2 && si.ssi_int == 777, "signalfd : sigqueue avec valeur 777");
    /* Lecture bloquante réveillée par le signal d'un enfant. */
    int sb = signalfd(-1, &m, 0);
    pid_t me = getpid();
    pid_t c = fork();
    if (c == 0) {
        usleep(20000);
        kill(me, SIGUSR1);
        _exit(0);
    }
    CHECK(read(sb, &si, sizeof si) == sizeof si && si.ssi_signo == SIGUSR1, "signalfd bloquant reveille par kill d'un enfant");
    waitpid(c, NULL, 0);
    close(sb);
    close(s);
    sigprocmask(SIG_UNBLOCK, &m, NULL);
}

/* --- epoll ------------------------------------------------------------------------------- */

static void test_epoll(void) {
    int ep = epoll_create1(EPOLL_CLOEXEC);
    int p[2];
    pipe(p);
    int e = eventfd(0, EFD_NONBLOCK);
    struct epoll_event ev = {.events = EPOLLIN, .data.u64 = 1};
    CHECK(ep >= 0 && epoll_ctl(ep, EPOLL_CTL_ADD, p[0], &ev) == 0, "epoll_ctl ADD tube");
    ev.data.u64 = 2;
    epoll_ctl(ep, EPOLL_CTL_ADD, e, &ev);
    CHECK(epoll_ctl(ep, EPOLL_CTL_ADD, e, &ev) < 0 && errno == EEXIST, "ADD en double : EEXIST");
    struct epoll_event out[8];
    CHECK(epoll_wait(ep, out, 8, 0) == 0, "rien de pret : 0 evenement");
    write(p[1], "x", 1);
    int n = epoll_wait(ep, out, 8, 1000);
    CHECK(n == 1 && out[0].data.u64 == 1 && (out[0].events & EPOLLIN), "tube lisible rapporte (data=1)");
    /* Niveau : toujours rapporté tant que non lu. */
    n = epoll_wait(ep, out, 8, 0);
    CHECK(n == 1, "declenchement par niveau : encore rapporte");
    char c;
    read(p[0], &c, 1);
    CHECK(epoll_wait(ep, out, 8, 0) == 0, "apres lecture : plus rien");
    /* Réveil d'un epoll_wait bloqué par un autre processus. */
    pid_t k = fork();
    if (k == 0) {
        usleep(20000);
        uint64_t one = 1;
        write(e, &one, 8);
        _exit(0);
    }
    long long t0 = mono_ns();
    n = epoll_wait(ep, out, 8, 2000);
    long long ms = (mono_ns() - t0) / 1000000;
    CHECK(n == 1 && out[0].data.u64 == 2 && ms >= 10 && ms < 1500, "epoll_wait bloque reveille par eventfd (%lld ms)", ms);
    waitpid(k, NULL, 0);
    uint64_t v;
    read(e, &v, 8);
    /* Par front. */
    ev.events = EPOLLIN | EPOLLET;
    ev.data.u64 = 3;
    epoll_ctl(ep, EPOLL_CTL_MOD, p[0], &ev);
    write(p[1], "ab", 2);
    n = epoll_wait(ep, out, 8, 100);
    int n2 = epoll_wait(ep, out, 8, 0);
    CHECK(n == 1 && n2 == 0, "EPOLLET : un seul rapport par front (%d puis %d)", n, n2);
    write(p[1], "c", 1);
    CHECK(epoll_wait(ep, out, 8, 100) == 1, "EPOLLET : nouveau front rapporte");
    read(p[0], (char[8]){0}, 8);
    /* Unique (ONESHOT) puis réarmement. */
    ev.events = EPOLLIN | EPOLLONESHOT;
    ev.data.u64 = 4;
    epoll_ctl(ep, EPOLL_CTL_MOD, p[0], &ev);
    write(p[1], "d", 1);
    n = epoll_wait(ep, out, 8, 100);
    n2 = epoll_wait(ep, out, 8, 0);
    CHECK(n == 1 && n2 == 0, "EPOLLONESHOT : desarme apres un rapport");
    epoll_ctl(ep, EPOLL_CTL_MOD, p[0], &ev);
    CHECK(epoll_wait(ep, out, 8, 100) == 1, "EPOLL_CTL_MOD rearme l'entree");
    read(p[0], &c, 1);
    /* Raccroché : fermeture de l'écrivain. */
    ev.events = EPOLLIN;
    ev.data.u64 = 5;
    epoll_ctl(ep, EPOLL_CTL_MOD, p[0], &ev);
    close(p[1]);
    n = epoll_wait(ep, out, 8, 100);
    CHECK(n == 1 && (out[0].events & EPOLLHUP), "EPOLLHUP quand l'ecrivain ferme");
    /* Imbrication et boucle. */
    int ep2 = epoll_create1(0);
    ev.events = EPOLLIN;
    ev.data.u64 = 9;
    CHECK(epoll_ctl(ep2, EPOLL_CTL_ADD, ep, &ev) == 0, "epoll dans un epoll");
    CHECK(epoll_wait(ep2, out, 8, 100) == 1 && out[0].data.u64 == 9, "epoll interne pret vu par l'externe");
    CHECK(epoll_ctl(ep, EPOLL_CTL_ADD, ep2, &ev) < 0 && errno == ELOOP, "boucle d'epoll refusee (ELOOP)");
    /* Fermeture : l'entrée disparaît avec la description. */
    epoll_ctl(ep, EPOLL_CTL_DEL, p[0], NULL);
    CHECK(epoll_ctl(ep, EPOLL_CTL_DEL, p[0], NULL) < 0 && errno == ENOENT, "DEL deux fois : ENOENT");
    close(p[0]);
    close(e);
    close(ep2);
    close(ep);
    /* 500 descripteurs surveillés, un seul prêt : epoll ne parcourt que les prêts. */
    int big = epoll_create1(0);
    int fds[500];
    for (int i = 0; i < 500; i++) {
        fds[i] = eventfd(0, EFD_NONBLOCK);
        struct epoll_event x = {.events = EPOLLIN, .data.u32 = i};
        epoll_ctl(big, EPOLL_CTL_ADD, fds[i], &x);
    }
    uint64_t one = 1;
    write(fds[377], &one, 8);
    n = epoll_wait(big, out, 8, 100);
    CHECK(n == 1 && out[0].data.u32 == 377, "500 surveilles, le seul pret trouve (%u)", out[0].data.u32);
    for (int i = 0; i < 500; i++) close(fds[i]);
    close(big);
}

/* --- pidfd ------------------------------------------------------------------------------- */

static void test_pidfd(void) {
    pid_t c = fork();
    if (c == 0) {
        pause();
        _exit(0);
    }
    int pfd = syscall(SYS_pidfd_open, c, 0);
    CHECK(pfd >= 0, "pidfd_open(%d)", c);
    struct pollfd p = {pfd, POLLIN, 0};
    CHECK(poll(&p, 1, 0) == 0, "processus vivant : pidfd non lisible");
    CHECK(syscall(SYS_pidfd_send_signal, pfd, SIGTERM, NULL, 0) == 0, "pidfd_send_signal(SIGTERM)");
    CHECK(poll(&p, 1, 2000) == 1 && (p.revents & POLLIN), "fin du processus : pidfd lisible");
    siginfo_t si;
    memset(&si, 0, sizeof si);
    CHECK(waitid(P_PIDFD, pfd, &si, WEXITED) == 0 && si.si_pid == c && si.si_status == SIGTERM, "waitid(P_PIDFD) : tue par SIGTERM");
    close(pfd);
}

/* --- Sockets Unix ------------------------------------------------------------------------ */

static void test_unix(void) {
    int sv[2];
    CHECK(socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, sv) == 0, "socketpair(AF_UNIX, SOCK_STREAM)");
    write(sv[0], "bonjour", 7);
    char b[64] = {0};
    CHECK(read(sv[1], b, sizeof b) == 7 && !memcmp(b, "bonjour", 7), "flux : donnees transmises");
    /* SCM_RIGHTS : passage d'un tube. */
    int p[2];
    pipe(p);
    char cbuf[CMSG_SPACE(sizeof(int))];
    memset(cbuf, 0, sizeof cbuf);
    struct iovec io = {.iov_base = "F", .iov_len = 1};
    struct msghdr mh = {.msg_iov = &io, .msg_iovlen = 1, .msg_control = cbuf, .msg_controllen = sizeof cbuf};
    struct cmsghdr *cm = CMSG_FIRSTHDR(&mh);
    cm->cmsg_level = SOL_SOCKET;
    cm->cmsg_type = SCM_RIGHTS;
    cm->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(cm), &p[1], sizeof(int));
    CHECK(sendmsg(sv[0], &mh, 0) == 1, "sendmsg avec SCM_RIGHTS");
    char rb[4];
    char rc[CMSG_SPACE(sizeof(int))];
    struct iovec rio = {.iov_base = rb, .iov_len = sizeof rb};
    struct msghdr rm = {.msg_iov = &rio, .msg_iovlen = 1, .msg_control = rc, .msg_controllen = sizeof rc};
    ssize_t n = recvmsg(sv[1], &rm, MSG_CMSG_CLOEXEC);
    struct cmsghdr *rcm = CMSG_FIRSTHDR(&rm);
    int got = -1;
    if (rcm && rcm->cmsg_type == SCM_RIGHTS) memcpy(&got, CMSG_DATA(rcm), sizeof(int));
    CHECK(n == 1 && got >= 0 && got != p[1], "recvmsg : descripteur recu (%d)", got);
    write(got, "via-fd", 6);
    close(got);
    memset(b, 0, sizeof b);
    CHECK(read(p[0], b, sizeof b) == 6 && !strcmp(b, "via-fd"), "ecriture par le descripteur recu arrivee dans le tube");
    close(p[0]);
    close(p[1]);
    /* Identité du pair, MSG_PEEK, fermeture → fin de fichier. */
    struct ucred uc;
    socklen_t ul = sizeof uc;
    CHECK(getsockopt(sv[0], SOL_SOCKET, SO_PEERCRED, &uc, &ul) == 0 && uc.pid == getpid(), "SO_PEERCRED : pid %d", uc.pid);
    write(sv[0], "abc", 3);
    CHECK(recv(sv[1], b, 3, MSG_PEEK) == 3 && recv(sv[1], b, 3, 0) == 3, "MSG_PEEK puis lecture");
    shutdown(sv[0], SHUT_WR);
    CHECK(read(sv[1], b, sizeof b) == 0, "SHUT_WR : fin de fichier chez le pair");
    close(sv[0]);
    signal(SIGPIPE, SIG_IGN);
    CHECK(write(sv[1], "x", 1) < 0 && errno == EPIPE, "ecriture vers un pair ferme : EPIPE");
    close(sv[1]);
    /* SOCK_SEQPACKET : frontières conservées. */
    socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sv);
    send(sv[0], "un", 2, 0);
    send(sv[0], "deux", 4, 0);
    int l1 = recv(sv[1], b, sizeof b, 0);
    int l2 = recv(sv[1], b, sizeof b, 0);
    CHECK(l1 == 2 && l2 == 4, "SOCK_SEQPACKET : messages separes (%d, %d)", l1, l2);
    close(sv[0]);
    close(sv[1]);
    /* Serveur de flux par chemin : bind, listen, connect (fils), accept, écho. */
    const char *path = "/m3test.sock";
    unlink(path);
    int srv = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un sa = {.sun_family = AF_UNIX};
    strcpy(sa.sun_path, path);
    CHECK(bind(srv, (struct sockaddr *)&sa, sizeof sa) == 0 && listen(srv, 4) == 0, "bind(%s) + listen", path);
    struct stat st;
    CHECK(stat(path, &st) == 0 && S_ISSOCK(st.st_mode), "stat : S_IFSOCK");
    int ep = epoll_create1(0);
    struct epoll_event ev = {.events = EPOLLIN, .data.fd = srv};
    epoll_ctl(ep, EPOLL_CTL_ADD, srv, &ev);
    pid_t c = fork();
    if (c == 0) {
        int s = socket(AF_UNIX, SOCK_STREAM, 0);
        if (connect(s, (struct sockaddr *)&sa, sizeof sa) != 0) _exit(1);
        write(s, "ping", 4);
        char r[8] = {0};
        int k = read(s, r, sizeof r);
        _exit(k == 4 && !memcmp(r, "pong", 4) ? 0 : 2);
    }
    struct epoll_event out;
    CHECK(epoll_wait(ep, &out, 1, 2000) == 1, "epoll : connexion en attente sur la socket d'ecoute");
    int cl = accept4(srv, NULL, NULL, SOCK_CLOEXEC);
    memset(b, 0, sizeof b);
    CHECK(cl >= 0 && read(cl, b, sizeof b) == 4 && !strcmp(b, "ping"), "accept puis lecture du client");
    write(cl, "pong", 4);
    int stc = 0;
    waitpid(c, &stc, 0);
    CHECK(WIFEXITED(stc) && WEXITSTATUS(stc) == 0, "client (autre processus) a recu la reponse");
    close(cl);
    close(ep);
    close(srv);
    unlink(path);
    /* Datagrammes, espace abstrait. */
    int d1 = socket(AF_UNIX, SOCK_DGRAM, 0), d2 = socket(AF_UNIX, SOCK_DGRAM, 0);
    struct sockaddr_un da = {.sun_family = AF_UNIX};
    memcpy(da.sun_path, "\0m3dgram", 8);
    socklen_t dl = offsetof(struct sockaddr_un, sun_path) + 8;
    CHECK(bind(d1, (struct sockaddr *)&da, dl) == 0, "bind abstrait (\\0m3dgram)");
    struct sockaddr_un da2 = {.sun_family = AF_UNIX};
    memcpy(da2.sun_path, "\0m3src", 6);
    bind(d2, (struct sockaddr *)&da2, offsetof(struct sockaddr_un, sun_path) + 6);
    sendto(d2, "dg1", 3, 0, (struct sockaddr *)&da, dl);
    sendto(d2, "dg-deux", 7, 0, (struct sockaddr *)&da, dl);
    struct sockaddr_un from;
    socklen_t fl = sizeof from;
    int r1 = recvfrom(d1, b, sizeof b, 0, (struct sockaddr *)&from, &fl);
    CHECK(r1 == 3 && fl == offsetof(struct sockaddr_un, sun_path) + 6 && !memcmp(from.sun_path, "\0m3src", 6), "datagramme recu avec l'adresse de l'emetteur");
    int r2 = recv(d1, b, 2, 0);
    CHECK(r2 == 2, "datagramme tronque a la taille du tampon");
    CHECK(recv(d1, b, sizeof b, MSG_DONTWAIT) < 0 && errno == EAGAIN, "reste du datagramme tronque perdu, file vide");
    close(d1);
    close(d2);
}

/* --- /proc ------------------------------------------------------------------------------- */

static int read_file(const char *p, char *buf, size_t n) {
    int fd = open(p, O_RDONLY);
    if (fd < 0) return -1;
    int k = read(fd, buf, n - 1);
    close(fd);
    if (k >= 0) buf[k] = 0;
    return k;
}

static void test_proc(int argc, char **argv) {
    char b[4096];
    int pid = 0;
    char state = 0;
    CHECK(read_file("/proc/self/stat", b, sizeof b) > 0 && sscanf(b, "%d (%*[^)]) %c", &pid, &state) == 2 && pid == getpid(),
          "/proc/self/stat : pid %d etat %c", pid, state);
    CHECK(read_file("/proc/self/status", b, sizeof b) > 0 && strstr(b, "Name:\t") && strstr(b, "VmRSS:"), "/proc/self/status : Name, VmRSS");
    int n = read_file("/proc/self/cmdline", b, sizeof b);
    int args_ok = n > 0 && !strcmp(b, argv[0]);
    CHECK(args_ok, "/proc/self/cmdline : argv[0] = %s (%d octets, argc %d)", argv[0], n, argc);
    char link[256] = {0};
    ssize_t l = readlink("/proc/self/exe", link, sizeof link - 1);
    CHECK(l > 0 && strstr(link, "m3test"), "readlink /proc/self/exe : %s", link);
    char me[32];
    snprintf(me, sizeof me, "%d", getpid());
    l = readlink("/proc/self", link, sizeof link - 1);
    link[l > 0 ? l : 0] = 0;
    CHECK(!strcmp(link, me), "readlink /proc/self : %s", link);
    DIR *d = opendir("/proc");
    int found = 0, pids = 0;
    struct dirent *de;
    while (d && (de = readdir(d))) {
        if (de->d_name[0] >= '0' && de->d_name[0] <= '9') pids++;
        if (!strcmp(de->d_name, me)) found = 1;
    }
    if (d) closedir(d);
    CHECK(found && pids >= 1, "readdir /proc : %d processus, dont soi-meme", pids);
    int fd = open("/proc/self/maps", O_RDONLY);
    CHECK(fd >= 0, "/proc/self/maps ouvert");
    n = read(fd, b, sizeof b - 1);
    b[n > 0 ? n : 0] = 0;
    CHECK(strstr(b, "[vdso]") && strstr(b, "[stack]"), "/proc/self/maps : [vdso], [stack]");
    close(fd);
    d = opendir("/proc/self/fd");
    int nfd = 0;
    while (d && (de = readdir(d))) nfd += de->d_name[0] != '.';
    if (d) closedir(d);
    CHECK(nfd >= 3, "/proc/self/fd : %d descripteurs", nfd);
    int e = eventfd(0, 0);
    char fdp[64];
    snprintf(fdp, sizeof fdp, "/proc/self/fd/%d", e);
    l = readlink(fdp, link, sizeof link - 1);
    link[l > 0 ? l : 0] = 0;
    CHECK(!strcmp(link, "anon_inode:[eventfd]"), "lien d'un eventfd : %s", link);
    close(e);
    double la1, la5, la15;
    CHECK(read_file("/proc/loadavg", b, sizeof b) > 0 && sscanf(b, "%lf %lf %lf", &la1, &la5, &la15) == 3, "/proc/loadavg : %s", strtok(b, "\n"));
    CHECK(read_file("/proc/stat", b, sizeof b) > 0 && !strncmp(b, "cpu ", 4) && strstr(b, "btime"), "/proc/stat : lignes cpu et btime");
    CHECK(read_file("/sys/devices/system/cpu/online", b, sizeof b) > 0 && b[0] == '0', "/sys/devices/system/cpu/online : %s", strtok(b, "\n"));
    long ncpu = sysconf(_SC_NPROCESSORS_ONLN);
    CHECK(ncpu >= 1, "sysconf(_SC_NPROCESSORS_ONLN) = %ld", ncpu);
    CHECK(read_file("/proc/sys/kernel/osrelease", b, sizeof b) > 0 && strstr(b, "maios"), "/proc/sys/kernel/osrelease : %s", strtok(b, "\n"));
}

/* MaiSS : projections MAP_SHARED de fichiers (pages communes, ecriture differee, coherence avec
 * read/write, heritage par fork). */
static void test_mmap_shared(void) {
    const char *path = "/maiss_mmap.bin";
    int fd = open(path, O_CREAT | O_RDWR | O_TRUNC, 0644);
    CHECK(fd >= 0, "open(O_CREAT) %s", path);
    if (fd < 0)
        return;
    static char buf[8192];
    memset(buf, 'a', sizeof buf);
    CHECK(write(fd, buf, sizeof buf) == (ssize_t)sizeof buf, "fichier de 8 Kio cree");
    char *p = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    CHECK(p != MAP_FAILED, "mmap(MAP_SHARED, PROT_WRITE) d'un fichier");
    if (p == MAP_FAILED)
        return;
    p[0] = 'X';
    p[5000] = 'Y';
    char c = 0;
    CHECK(pread(fd, &c, 1, 0) == 1 && c == 'X', "read() voit l'ecriture de la projection avant msync (%c)", c);
    CHECK(msync(p, 8192, MS_SYNC) == 0, "msync(MS_SYNC)");
    CHECK(pwrite(fd, "Z", 1, 100) == 1 && p[100] == 'Z', "la projection voit write() (%c)", p[100]);
    pid_t pid = fork();
    if (pid == 0) {
        p[200] = 'C';
        _exit(0);
    }
    int st = 0;
    waitpid(pid, &st, 0);
    CHECK(p[200] == 'C', "MAP_SHARED de fichier herite par fork : ecriture de l'enfant visible");
    char *q = mmap(NULL, 8192, PROT_READ, MAP_SHARED, fd, 0);
    CHECK(q != MAP_FAILED && q[5000] == 'Y' && q[200] == 'C', "seconde projection : memes pages");
    char *r = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    if (r != MAP_FAILED)
        r[1] = 'P';
    CHECK(r != MAP_FAILED && p[1] == 'a', "MAP_PRIVATE reste prive (copie a l'ecriture)");
    munmap(r, 4096);
    munmap(q, 8192);
    munmap(p, 8192);
    close(fd);
    static char back[8192];
    fd = open(path, O_RDONLY);
    ssize_t n = fd >= 0 ? read(fd, back, sizeof back) : -1;
    CHECK(n == 8192 && back[0] == 'X' && back[1] == 'a' && back[100] == 'Z' && back[200] == 'C' && back[5000] == 'Y',
          "contenu reecrit dans le fichier apres munmap (%zd octets)", n);
    if (fd >= 0)
        close(fd);
    unlink(path);
}

int main(int argc, char **argv) {
    printf("m3test : jalon M3 (pid %d)\n", getpid());
    fflush(stdout);
    test_vdso();
    test_eventfd();
    test_timerfd();
    test_signalfd();
    test_epoll();
    test_pidfd();
    test_unix();
    test_mmap_shared();
    test_proc(argc, argv);
    printf("m3test : %s (%d echec%s)\n", fails ? "ECHEC" : "OK", fails, fails > 1 ? "s" : "");
    return fails;
}
