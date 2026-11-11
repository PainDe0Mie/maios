/* m3net — jalon M3 de MaiProc, suite : sockets TCP/UDP (IPv4, IPv6, double pile) et io_uring.
 * Compilé deux fois : musl statique (m3net) et glibc dynamique (m3net_glibc).
 *
 * Usage : m3net              contrôles locaux ; code de sortie = nombre d'échecs
 *         m3net ext PORT     en plus : écho TCP et UDP par 10.0.2.2:PORT (hôte de QEMU)
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <net/if.h>
#include <netdb.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

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

/* `struct __kernel_timespec` d'io_uring : 2 × 64 bits, aussi en i386 (où `timespec` fait 8 octets). */
struct kts {
    long long tv_sec, tv_nsec;
};

static long long mono_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1000000000LL + t.tv_nsec;
}

static void msleep(int ms) {
    struct timespec t = {ms / 1000, (ms % 1000) * 1000000L};
    nanosleep(&t, NULL);
}

/* --- TCP/UDP --------------------------------------------------------------------------- */

static int listener4(const char *ip, int port, int reuse, int reuseport, int backlog) {
    int s = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1;
    if (reuse) setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    if (reuseport) setsockopt(s, SOL_SOCKET, SO_REUSEPORT, &one, sizeof one);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(port)};
    inet_pton(AF_INET, ip, &a.sin_addr);
    if (bind(s, (struct sockaddr *)&a, sizeof a) < 0 || listen(s, backlog) < 0) {
        int e = errno;
        close(s);
        errno = e;
        return -1;
    }
    return s;
}

static int port_of(int s) {
    struct sockaddr_storage a;
    socklen_t l = sizeof a;
    getsockname(s, (struct sockaddr *)&a, &l);
    return a.ss_family == AF_INET ? ntohs(((struct sockaddr_in *)&a)->sin_port) : ntohs(((struct sockaddr_in6 *)&a)->sin6_port);
}

static int connect4(const char *ip, int port) {
    int c = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(port)};
    inet_pton(AF_INET, ip, &a.sin_addr);
    if (connect(c, (struct sockaddr *)&a, sizeof a) < 0) {
        int e = errno;
        close(c);
        errno = e;
        return -1;
    }
    return c;
}

static ssize_t read_full(int fd, void *buf, size_t n) {
    size_t got = 0;
    while (got < n) {
        ssize_t r = read(fd, (char *)buf + got, n - got);
        if (r <= 0) return got ? (ssize_t)got : r;
        got += r;
    }
    return got;
}

static void test_tcp_basic(void) {
    int l = listener4("127.0.0.1", 0, 0, 0, 16);
    int port = l >= 0 ? port_of(l) : 0;
    CHECK(l >= 0 && port > 0, "TCP : ecoute sur 127.0.0.1:%d", port);
    pid_t pid = fork();
    if (pid == 0) {
        int c = connect4("127.0.0.1", port);
        if (c < 0) _exit(1);
        struct sockaddr_in me, peer;
        socklen_t l1 = sizeof me, l2 = sizeof peer;
        getsockname(c, (struct sockaddr *)&me, &l1);
        getpeername(c, (struct sockaddr *)&peer, &l2);
        if (ntohs(peer.sin_port) != port || ntohs(me.sin_port) < 32768 || ntohs(me.sin_port) > 60999) _exit(2);
        if (write(c, "bonjour", 7) != 7) _exit(3);
        char b[8] = {0};
        if (read_full(c, b, 5) != 5 || memcmp(b, "monde", 5)) _exit(4);
        /* 4 Mio dans un sens, somme de contrôle dans l'autre. */
        static char big[65536];
        unsigned sum = 0;
        for (int i = 0; i < 64; i++) {
            for (int j = 0; j < 65536; j++) big[j] = (char)(i * 7 + j), sum += (unsigned char)big[j];
            if (write(c, big, sizeof big) != sizeof big) _exit(5);
        }
        shutdown(c, SHUT_WR);
        unsigned back = 0;
        if (read_full(c, &back, 4) != 4 || back != sum) _exit(6);
        close(c);
        _exit(0);
    }
    struct sockaddr_in peer;
    socklen_t pl = sizeof peer;
    int a = accept(l, (struct sockaddr *)&peer, &pl);
    char ip[32] = "";
    inet_ntop(AF_INET, &peer.sin_addr, ip, sizeof ip);
    CHECK(a >= 0 && !strcmp(ip, "127.0.0.1"), "TCP : accept, pair %s:%d", ip, ntohs(peer.sin_port));
    char b[8] = {0};
    CHECK(read_full(a, b, 7) == 7 && !memcmp(b, "bonjour", 7), "TCP : recu \"%s\"", b);
    write(a, "monde", 5);
    long long t0 = mono_ns();
    static char big[65536];
    unsigned sum = 0;
    size_t total = 0;
    ssize_t r;
    while ((r = read(a, big, sizeof big)) > 0) {
        for (ssize_t j = 0; j < r; j++) sum += (unsigned char)big[j];
        total += r;
    }
    long long dt = mono_ns() - t0;
    CHECK(total == 64 * 65536, "TCP : 4 Mio par le chemin rapide local (%zu octets, %.0f Mo/s), fin de flux apres SHUT_WR", total,
          dt > 0 ? total * 1000.0 / dt : 0.0);
    write(a, &sum, 4);
    int st = 0;
    waitpid(pid, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0, "TCP : client (getsockname/getpeername, port ephemere, somme) : code %d", WEXITSTATUS(st));
    int v = 0;
    socklen_t vl = sizeof v;
    getsockopt(l, SOL_SOCKET, SO_ACCEPTCONN, &v, &vl);
    CHECK(v == 1, "TCP : SO_ACCEPTCONN = 1 sur l'ecouteur");
    close(a);
    close(l);
    int c = connect4("127.0.0.1", port);
    CHECK(c < 0 && errno == ECONNREFUSED, "TCP : port ferme -> ECONNREFUSED (%s)", strerror(errno));
}

static void test_tcp_states(void) {
    int l = listener4("127.0.0.1", 0, 0, 0, 4);
    int port = port_of(l);
    fcntl(l, F_SETFL, O_NONBLOCK);
    int a = accept(l, NULL, NULL);
    CHECK(a < 0 && errno == EAGAIN, "TCP : accept non bloquant sans connexion -> EAGAIN");
    int c = connect4("127.0.0.1", port);
    struct pollfd p = {l, POLLIN, 0};
    CHECK(poll(&p, 1, 1000) == 1 && (p.revents & POLLIN), "TCP : poll de l'ecouteur -> POLLIN");
    a = accept(l, NULL, NULL);
    int one = 1, v = 0;
    socklen_t vl = sizeof v;
    setsockopt(c, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
    getsockopt(c, IPPROTO_TCP, TCP_NODELAY, &v, &vl);
    struct tcp_info ti;
    socklen_t tl = sizeof ti;
    memset(&ti, 0, sizeof ti);
    getsockopt(c, IPPROTO_TCP, TCP_INFO, &ti, &tl);
    CHECK(v == 1 && ti.tcpi_state == 1, "TCP : TCP_NODELAY = %d, TCP_INFO etat %d (ESTABLISHED)", v, ti.tcpi_state);
    /* epoll : données puis demi-fermeture du pair. */
    int ep = epoll_create1(0);
    struct epoll_event e = {.events = EPOLLIN | EPOLLRDHUP, .data.fd = a}, out;
    epoll_ctl(ep, EPOLL_CTL_ADD, a, &e);
    write(c, "x", 1);
    int n = epoll_wait(ep, &out, 1, 1000);
    CHECK(n == 1 && (out.events & EPOLLIN) && !(out.events & EPOLLRDHUP), "TCP : epoll -> EPOLLIN sur donnees");
    char ch;
    read(a, &ch, 1);
    shutdown(c, SHUT_WR);
    n = epoll_wait(ep, &out, 1, 1000);
    CHECK(n == 1 && (out.events & EPOLLRDHUP), "TCP : epoll -> EPOLLRDHUP apres SHUT_WR du pair (%#x)", n == 1 ? out.events : 0);
    CHECK(read(a, &ch, 1) == 0, "TCP : lecture -> fin de flux");
    close(c);
    close(a);
    /* Fermeture avec des données non lues : RST, le pair voit ECONNRESET. */
    fcntl(l, F_SETFL, 0);
    c = connect4("127.0.0.1", port);
    a = accept(l, NULL, NULL);
    write(a, "abc", 3);
    msleep(5);
    close(c);
    msleep(5);
    char b[4];
    ssize_t r = read(a, b, sizeof b);
    CHECK(r < 0 && errno == ECONNRESET, "TCP : fermeture avec donnees non lues -> ECONNRESET chez le pair (%zd, %s)", r, strerror(errno));
    close(a);
    close(ep);
    close(l);
}

static void test_tcp_reuse(void) {
    int l1 = listener4("127.0.0.1", 0, 0, 0, 4);
    int port = port_of(l1);
    int l2 = listener4("127.0.0.1", port, 0, 0, 4);
    CHECK(l2 < 0 && errno == EADDRINUSE, "TCP : deuxieme ecouteur sur le meme port -> EADDRINUSE");
    close(l1);
    /* SO_REUSEADDR : relier le port tant qu'une connexion acceptée vit encore. */
    int l = listener4("127.0.0.1", 0, 1, 0, 4);
    port = port_of(l);
    int c = connect4("127.0.0.1", port);
    int a = accept(l, NULL, NULL);
    close(l);
    int x = listener4("127.0.0.1", port, 0, 0, 4);
    int xe = errno;
    int y = listener4("127.0.0.1", port, 1, 0, 4);
    CHECK(x < 0 && xe == EADDRINUSE && y >= 0, "TCP : relance du serveur : sans SO_REUSEADDR EADDRINUSE, avec : ok");
    close(y);
    close(a);
    close(c);
    /* SO_REUSEPORT : connexions réparties entre deux écouteurs (hachage des flux). */
    int r1 = listener4("127.0.0.1", 0, 0, 1, 64);
    port = port_of(r1);
    int r2 = listener4("127.0.0.1", port, 0, 1, 64);
    int conns[32], n1 = 0, n2 = 0;
    for (int i = 0; i < 32; i++) conns[i] = connect4("127.0.0.1", port);
    fcntl(r1, F_SETFL, O_NONBLOCK);
    fcntl(r2, F_SETFL, O_NONBLOCK);
    int s;
    while ((s = accept(r1, NULL, NULL)) >= 0) n1++, close(s);
    while ((s = accept(r2, NULL, NULL)) >= 0) n2++, close(s);
    CHECK(r2 >= 0 && n1 + n2 == 32 && n1 > 0 && n2 > 0, "TCP : SO_REUSEPORT, 32 connexions reparties %d / %d", n1, n2);
    for (int i = 0; i < 32; i++) close(conns[i]);
    close(r1);
    close(r2);
}

static void test_ipv6(void) {
    int l = socket(AF_INET6, SOCK_STREAM, 0);
    struct sockaddr_in6 a6 = {.sin6_family = AF_INET6, .sin6_addr = in6addr_any};
    int ok = bind(l, (struct sockaddr *)&a6, sizeof a6) == 0 && listen(l, 8) == 0;
    int port = port_of(l);
    CHECK(ok && port > 0, "IPv6 : ecoute sur [::]:%d", port);
    int c4 = connect4("127.0.0.1", port);
    struct sockaddr_in6 peer;
    socklen_t pl = sizeof peer;
    int a = accept(l, (struct sockaddr *)&peer, &pl);
    char ip[64] = "";
    inet_ntop(AF_INET6, &peer.sin6_addr, ip, sizeof ip);
    CHECK(c4 >= 0 && a >= 0 && !strcmp(ip, "::ffff:127.0.0.1"), "IPv6 : double pile, client IPv4 vu comme %s", ip);
    close(a);
    close(c4);
    int c6 = socket(AF_INET6, SOCK_STREAM, 0);
    struct sockaddr_in6 d = {.sin6_family = AF_INET6, .sin6_port = htons(port), .sin6_addr = in6addr_loopback};
    int r = connect(c6, (struct sockaddr *)&d, sizeof d);
    a = accept(l, (struct sockaddr *)&peer, &pl);
    inet_ntop(AF_INET6, &peer.sin6_addr, ip, sizeof ip);
    write(c6, "v6", 2);
    char b[2] = {0};
    CHECK(r == 0 && a >= 0 && !strcmp(ip, "::1") && read_full(a, b, 2) == 2 && !memcmp(b, "v6", 2), "IPv6 : connexion par [::1], pair %s", ip);
    close(a);
    close(c6);
    close(l);
    l = socket(AF_INET6, SOCK_STREAM, 0);
    int one = 1;
    setsockopt(l, IPPROTO_IPV6, IPV6_V6ONLY, &one, sizeof one);
    bind(l, (struct sockaddr *)&a6, sizeof a6);
    listen(l, 4);
    c4 = connect4("127.0.0.1", port_of(l));
    CHECK(c4 < 0 && errno == ECONNREFUSED, "IPv6 : IPV6_V6ONLY refuse l'IPv4 (%s)", strerror(errno));
    close(l);
}

static void test_udp(void) {
    int a = socket(AF_INET, SOCK_DGRAM, 0), b = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in ba = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    bind(b, (struct sockaddr *)&ba, sizeof ba);
    int port = port_of(b);
    ba.sin_port = htons(port);
    sendto(a, "un", 2, 0, (struct sockaddr *)&ba, sizeof ba);
    sendto(a, "deux", 4, 0, (struct sockaddr *)&ba, sizeof ba);
    char buf[16] = {0};
    struct sockaddr_in from;
    socklen_t fl = sizeof from;
    ssize_t n = recvfrom(b, buf, sizeof buf, 0, (struct sockaddr *)&from, &fl);
    CHECK(n == 2 && !memcmp(buf, "un", 2) && ntohs(from.sin_port) == port_of(a) && from.sin_addr.s_addr == htonl(INADDR_LOOPBACK),
          "UDP : datagramme recu de 127.0.0.1:%d (liaison automatique de l'emetteur)", ntohs(from.sin_port));
    n = recv(b, buf, 2, MSG_TRUNC);
    CHECK(n == 4, "UDP : frontieres des messages, MSG_TRUNC rend la taille reelle (%zd)", n);
    /* UDP connectée vers un port fermé : « port inaccessible » rapporté. */
    int c = socket(AF_INET, SOCK_DGRAM, 0);
    close(b);
    connect(c, (struct sockaddr *)&ba, sizeof ba);
    send(c, "x", 1, 0);
    ssize_t r = recv(c, buf, sizeof buf, MSG_DONTWAIT);
    CHECK(r < 0 && errno == ECONNREFUSED, "UDP : connectee vers un port ferme -> ECONNREFUSED (%s)", strerror(errno));
    close(a);
    close(c);
    /* Double pile en UDP. */
    int s6 = socket(AF_INET6, SOCK_DGRAM, 0);
    struct sockaddr_in6 a6 = {.sin6_family = AF_INET6, .sin6_addr = in6addr_any};
    bind(s6, (struct sockaddr *)&a6, sizeof a6);
    int p6 = port_of(s6);
    int s4 = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in d4 = {.sin_family = AF_INET, .sin_port = htons(p6), .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    sendto(s4, "46", 2, 0, (struct sockaddr *)&d4, sizeof d4);
    struct sockaddr_in6 f6;
    socklen_t f6l = sizeof f6;
    n = recvfrom(s6, buf, sizeof buf, 0, (struct sockaddr *)&f6, &f6l);
    char ip[64] = "";
    inet_ntop(AF_INET6, &f6.sin6_addr, ip, sizeof ip);
    CHECK(n == 2 && !strcmp(ip, "::ffff:127.0.0.1"), "UDP : IPv4 vers une socket [::], source %s", ip);
    close(s4);
    close(s6);
}

static void test_net_misc(void) {
    struct addrinfo hints = {.ai_family = AF_INET, .ai_socktype = SOCK_STREAM}, *res = NULL;
    int r = getaddrinfo("localhost", "80", &hints, &res);
    char ip[32] = "";
    if (!r) inet_ntop(AF_INET, &((struct sockaddr_in *)res->ai_addr)->sin_addr, ip, sizeof ip);
    CHECK(r == 0 && !strcmp(ip, "127.0.0.1"), "getaddrinfo(\"localhost\") -> %s (%s)", ip, r ? gai_strerror(r) : "ok");
    if (res) freeaddrinfo(res);
    int l = listener4("127.0.0.1", 0, 0, 0, 4);
    char needle[32];
    snprintf(needle, sizeof needle, "0100007F:%04X 00000000:0000 0A", port_of(l));
    FILE *f = fopen("/proc/net/tcp", "r");
    char line[256];
    int found = 0;
    while (f && fgets(line, sizeof line, f))
        if (strstr(line, needle)) found = 1;
    if (f) fclose(f);
    CHECK(found, "/proc/net/tcp : ecouteur %s", needle);
    close(l);
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    char buf[1024];
    struct ifconf ifc = {.ifc_len = sizeof buf, .ifc_buf = buf};
    int lo = 0;
    if (ioctl(s, SIOCGIFCONF, &ifc) == 0)
        for (int i = 0; i < ifc.ifc_len / (int)sizeof(struct ifreq); i++)
            if (!strcmp(ifc.ifc_req[i].ifr_name, "lo")) lo = 1;
    struct ifreq ifr = {0};
    strcpy(ifr.ifr_name, "lo");
    int fl = ioctl(s, SIOCGIFFLAGS, &ifr) == 0 ? ifr.ifr_flags : 0;
    CHECK(lo && (fl & IFF_LOOPBACK) && (fl & IFF_UP), "SIOCGIFCONF/SIOCGIFFLAGS : lo present, flags %#x", fl);
    close(s);
}

static void test_external(int port) {
    int c = connect4("10.0.2.2", port);
    char b[16] = {0};
    int ok = c >= 0 && write(c, "maios-tcp", 9) == 9 && read_full(c, b, 9) == 9 && !memcmp(b, "maios-tcp", 9);
    CHECK(ok, "reseau : echo TCP par 10.0.2.2:%d (smoltcp, e1000) (%s)", port, c < 0 ? strerror(errno) : "connecte");
    if (c >= 0) close(c);
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    struct timeval tv = {2, 0};
    setsockopt(u, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    struct sockaddr_in d = {.sin_family = AF_INET, .sin_port = htons(port)};
    inet_pton(AF_INET, "10.0.2.2", &d.sin_addr);
    ssize_t n = -1;
    for (int i = 0; i < 3 && n != 9; i++) {
        sendto(u, "maios-udp", 9, 0, (struct sockaddr *)&d, sizeof d);
        n = recv(u, b, sizeof b, 0);
    }
    CHECK(n == 9 && !memcmp(b, "maios-udp", 9), "reseau : echo UDP par 10.0.2.2:%d (%zd)", port, n);
    close(u);
}

/* --- io_uring (appels bruts, ABI de Linux) ----------------------------------------------- */

struct sqe {
    uint8_t opcode, flags;
    uint16_t ioprio;
    int32_t fd;
    uint64_t off, addr;
    uint32_t len, op_flags;
    uint64_t user_data;
    uint16_t buf_index, personality;
    int32_t file_index;
    uint64_t addr3, pad;
};
struct cqe {
    uint64_t user_data;
    int32_t res;
    uint32_t flags;
};
struct uparams {
    uint32_t sq_entries, cq_entries, flags, sq_thread_cpu, sq_thread_idle, features, wq_fd, resv[3];
    struct {
        uint32_t head, tail, ring_mask, ring_entries, flags, dropped, array, resv1;
        uint64_t user_addr;
    } sq_off;
    struct {
        uint32_t head, tail, ring_mask, ring_entries, overflow, cqes, flags, resv1;
        uint64_t user_addr;
    } cq_off;
};
struct uring {
    int fd;
    unsigned *sq_head, *sq_tail, *sq_mask, *sq_flags, *sq_array, *cq_head, *cq_tail, *cq_mask;
    struct sqe *sqes;
    struct cqe *cqes;
    unsigned tail, features, entries;
};

enum { OP_NOP = 0, OP_READV = 1, OP_WRITEV = 2, OP_POLL_ADD = 6, OP_POLL_REMOVE = 7, OP_TIMEOUT = 11, OP_TIMEOUT_REMOVE = 12,
       OP_ACCEPT = 13, OP_ASYNC_CANCEL = 14, OP_LINK_TIMEOUT = 15, OP_OPENAT = 18, OP_CLOSE = 19, OP_STATX = 21, OP_READ = 22,
       OP_WRITE = 23, OP_SEND = 26, OP_RECV = 27, OP_PROVIDE_BUFFERS = 31, OP_MSG_RING = 40 };
#define F_FIXED_FILE 1
#define F_LINK 4
#define F_BUFFER_SELECT 32
#define F_CQE_SKIP 64
#define ENTER_GETEVENTS 1
#define ENTER_SQ_WAKEUP 2
#define ENTER_EXT_ARG 8
#define SETUP_SQPOLL 2
#define SETUP_CQSIZE 8
#define CQE_F_BUFFER 1
#define CQE_F_MORE 2

static int ur_init(struct uring *r, unsigned entries, struct uparams *p) {
    memset(r, 0, sizeof *r);
    r->fd = syscall(425, entries, p);
    if (r->fd < 0) return -errno;
    size_t sqsz = p->sq_off.array + p->sq_entries * 4, cqsz = p->cq_off.cqes + p->cq_entries * sizeof(struct cqe);
    size_t sz = sqsz > cqsz ? sqsz : cqsz;
    char *ring = mmap(NULL, sz, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_POPULATE, r->fd, 0);
    void *sqes = mmap(NULL, p->sq_entries * sizeof(struct sqe), PROT_READ | PROT_WRITE, MAP_SHARED | MAP_POPULATE, r->fd, 0x10000000);
    if (ring == MAP_FAILED || sqes == MAP_FAILED) return -ENOMEM;
    r->sq_head = (unsigned *)(ring + p->sq_off.head);
    r->sq_tail = (unsigned *)(ring + p->sq_off.tail);
    r->sq_mask = (unsigned *)(ring + p->sq_off.ring_mask);
    r->sq_flags = (unsigned *)(ring + p->sq_off.flags);
    r->sq_array = (unsigned *)(ring + p->sq_off.array);
    r->cq_head = (unsigned *)(ring + p->cq_off.head);
    r->cq_tail = (unsigned *)(ring + p->cq_off.tail);
    r->cq_mask = (unsigned *)(ring + p->cq_off.ring_mask);
    r->cqes = (struct cqe *)(ring + p->cq_off.cqes);
    r->sqes = sqes;
    r->tail = *r->sq_tail;
    r->features = p->features;
    r->entries = p->sq_entries;
    return 0;
}

static struct sqe *ur_get(struct uring *r, uint8_t op, int fd, uint64_t ud) {
    unsigned idx = r->tail & *r->sq_mask;
    r->sq_array[idx] = idx;
    struct sqe *s = &r->sqes[idx];
    memset(s, 0, sizeof *s);
    s->opcode = op;
    s->fd = fd;
    s->user_data = ud;
    r->tail++;
    return s;
}

/* Publie les SQE et soumet ; `wait` : attend ce nombre de complétions. */
static int ur_submit(struct uring *r, unsigned wait) {
    unsigned n = r->tail - *r->sq_tail;
    __atomic_store_n(r->sq_tail, r->tail, __ATOMIC_RELEASE);
    int ret = syscall(426, r->fd, n, wait, wait ? ENTER_GETEVENTS : 0, NULL, 0);
    return ret < 0 ? -errno : ret;
}

static unsigned ur_ready(struct uring *r) {
    return __atomic_load_n(r->cq_tail, __ATOMIC_ACQUIRE) - *r->cq_head;
}

/* Prochaine complétion (attente bornée à 2 s). */
static int ur_wait(struct uring *r, struct cqe *out) {
    long long end = mono_ns() + 2000000000LL;
    while (!ur_ready(r)) {
        struct { uint64_t sigmask; uint32_t sz, pad; uint64_t ts; } arg = {0, 8, 0, 0};
        struct kts ts = {0, 100000000};
        arg.ts = (uint64_t)(uintptr_t)&ts;
        syscall(426, r->fd, 0, 1, ENTER_GETEVENTS | ENTER_EXT_ARG, &arg, sizeof arg);
        if (mono_ns() > end) return -ETIME;
    }
    unsigned h = *r->cq_head;
    *out = r->cqes[h & *r->cq_mask];
    __atomic_store_n(r->cq_head, h + 1, __ATOMIC_RELEASE);
    return 0;
}

static void ur_close(struct uring *r) { close(r->fd); }

static void test_uring_basic(void) {
    struct uparams p = {0};
    struct uring r;
    int e = ur_init(&r, 8, &p);
    CHECK(e == 0 && p.sq_entries == 8 && p.cq_entries == 16 && (p.features & 0x2b), "io_uring : anneau 8/16, fonctions %#x (%d)", p.features, e);
    for (int i = 0; i < 3; i++) ur_get(&r, OP_NOP, -1, 100 + i);
    int sub = ur_submit(&r, 3);
    struct cqe c[3];
    int ok = sub == 3;
    for (int i = 0; i < 3; i++) ok &= ur_wait(&r, &c[i]) == 0 && c[i].user_data == 100u + i && c[i].res == 0;
    CHECK(ok, "io_uring : 3 NOP soumis et completes dans l'ordre");

    /* Lecture d'un tube vide : en attente sans fil (sondage rapide), finie par l'écriture. */
    int pp[2];
    pipe(pp);
    char buf[32] = {0};
    struct sqe *s = ur_get(&r, OP_READ, pp[0], 7);
    s->addr = (uintptr_t)buf;
    s->len = sizeof buf;
    s->off = (uint64_t)-1;
    ur_submit(&r, 0);
    msleep(20);
    int pending = ur_ready(&r) == 0;
    write(pp[1], "anneau", 6);
    struct cqe cq;
    ok = ur_wait(&r, &cq) == 0 && cq.user_data == 7 && cq.res == 6 && !memcmp(buf, "anneau", 6);
    CHECK(pending && ok, "io_uring : READ d'un tube vide en attente puis complete par l'ecriture (%d)", cq.res);

    /* Écriture puis lecture liées (IOSQE_IO_LINK). */
    s = ur_get(&r, OP_WRITE, pp[1], 1);
    s->addr = (uintptr_t)"lien";
    s->len = 4;
    s->flags = F_LINK;
    memset(buf, 0, sizeof buf);
    s = ur_get(&r, OP_READ, pp[0], 2);
    s->addr = (uintptr_t)buf;
    s->len = 4;
    ur_submit(&r, 2);
    struct cqe c1, c2;
    ok = ur_wait(&r, &c1) == 0 && ur_wait(&r, &c2) == 0 && c1.user_data == 1 && c1.res == 4 && c2.user_data == 2 && c2.res == 4 && !memcmp(buf, "lien", 4);
    CHECK(ok, "io_uring : chaine WRITE -> READ (IOSQE_IO_LINK)");
    /* Échec dans une chaîne : la suite est annulée. */
    s = ur_get(&r, OP_READ, -1, 3);
    s->addr = (uintptr_t)buf;
    s->len = 1;
    s->flags = F_LINK;
    ur_get(&r, OP_NOP, -1, 4);
    ur_submit(&r, 2);
    ur_wait(&r, &c1);
    ur_wait(&r, &c2);
    CHECK(c1.res == -EBADF && c2.user_data == 4 && c2.res == -ECANCELED, "io_uring : echec dans une chaine -> suite annulee (%d, %d)", c1.res, c2.res);

    /* READV/WRITEV sur un fichier. */
    int fd = open("/tmp/m3net_uring", O_CREAT | O_RDWR | O_TRUNC, 0644);
    struct iovec wv[2] = {{"vecteur ", 8}, {"io_uring", 8}};
    s = ur_get(&r, OP_WRITEV, fd, 5);
    s->addr = (uintptr_t)wv;
    s->len = 2;
    s->off = 0;
    ur_submit(&r, 1);
    ur_wait(&r, &c1);
    char rb1[8] = {0}, rb2[9] = {0};
    struct iovec rv[2] = {{rb1, 8}, {rb2, 8}};
    s = ur_get(&r, OP_READV, fd, 6);
    s->addr = (uintptr_t)rv;
    s->len = 2;
    s->off = 0;
    ur_submit(&r, 1);
    ur_wait(&r, &c2);
    CHECK(c1.res == 16 && c2.res == 16 && !memcmp(rb1, "vecteur ", 8) && !memcmp(rb2, "io_uring", 8), "io_uring : WRITEV/READV d'un fichier (%d, %d)", c1.res, c2.res);
    close(fd);

    /* Délai relatif. */
    struct kts ts = {0, 20000000};
    long long t0 = mono_ns();
    s = ur_get(&r, OP_TIMEOUT, -1, 8);
    s->addr = (uintptr_t)&ts;
    s->len = 1;
    ur_submit(&r, 1);
    ur_wait(&r, &c1);
    long long el = (mono_ns() - t0) / 1000000;
    CHECK(c1.user_data == 8 && c1.res == -ETIME && el >= 15 && el < 1000, "io_uring : TIMEOUT de 20 ms -> -ETIME en %lld ms", el);
    /* Délai par nombre de complétions. */
    struct kts lng = {10, 0};
    s = ur_get(&r, OP_TIMEOUT, -1, 9);
    s->addr = (uintptr_t)&lng;
    s->len = 1;
    s->off = 2;
    ur_get(&r, OP_NOP, -1, 10);
    ur_get(&r, OP_NOP, -1, 11);
    ur_submit(&r, 3);
    int got9 = 0, res9 = -1;
    for (int i = 0; i < 3; i++)
        if (ur_wait(&r, &c1) == 0 && c1.user_data == 9) got9 = 1, res9 = c1.res;
    CHECK(got9 && res9 == 0, "io_uring : TIMEOUT par nombre (2 completions) -> 0 (%d)", res9);
    /* Retrait d'un délai. */
    s = ur_get(&r, OP_TIMEOUT, -1, 12);
    s->addr = (uintptr_t)&lng;
    s->len = 1;
    ur_submit(&r, 0);
    s = ur_get(&r, OP_TIMEOUT_REMOVE, -1, 13);
    s->addr = 12;
    ur_submit(&r, 2);
    ur_wait(&r, &c1);
    ur_wait(&r, &c2);
    int rm = (c1.user_data == 13 ? c1.res : c2.res), tm = (c1.user_data == 12 ? c1.res : c2.res);
    CHECK(rm == 0 && tm == -ECANCELED, "io_uring : TIMEOUT_REMOVE -> %d, delai -> %d", rm, tm);

    /* Annulation d'une lecture en attente. */
    s = ur_get(&r, OP_READ, pp[0], 14);
    s->addr = (uintptr_t)buf;
    s->len = 4;
    ur_submit(&r, 0);
    s = ur_get(&r, OP_ASYNC_CANCEL, -1, 15);
    s->addr = 14;
    ur_submit(&r, 2);
    ur_wait(&r, &c1);
    ur_wait(&r, &c2);
    int rd = (c1.user_data == 14 ? c1.res : c2.res), cn = (c1.user_data == 15 ? c1.res : c2.res);
    CHECK(rd == -ECANCELED && cn == 0, "io_uring : ASYNC_CANCEL d'une lecture en attente (%d, %d)", rd, cn);

    /* LINK_TIMEOUT : la lecture est bornée. */
    s = ur_get(&r, OP_READ, pp[0], 16);
    s->addr = (uintptr_t)buf;
    s->len = 4;
    s->flags = F_LINK;
    struct kts lt = {0, 20000000};
    s = ur_get(&r, OP_LINK_TIMEOUT, -1, 17);
    s->addr = (uintptr_t)&lt;
    s->len = 1;
    ur_submit(&r, 2);
    ur_wait(&r, &c1);
    ur_wait(&r, &c2);
    rd = (c1.user_data == 16 ? c1.res : c2.res);
    int lr = (c1.user_data == 17 ? c1.res : c2.res);
    CHECK(rd == -ECANCELED && lr == -ETIME, "io_uring : LINK_TIMEOUT borne une lecture (%d, %d)", rd, lr);

    /* POLL_ADD à plusieurs coups sur un eventfd. */
    int efd = eventfd(0, EFD_NONBLOCK);
    s = ur_get(&r, OP_POLL_ADD, efd, 18);
    s->op_flags = POLLIN;
    s->len = 1; /* IORING_POLL_ADD_MULTI */
    ur_submit(&r, 0);
    uint64_t one = 1, v;
    write(efd, &one, 8);
    ur_wait(&r, &c1);
    read(efd, &v, 8);
    write(efd, &one, 8);
    ur_wait(&r, &c2);
    read(efd, &v, 8);
    CHECK((c1.res & POLLIN) && (c1.flags & CQE_F_MORE) && (c2.res & POLLIN) && (c2.flags & CQE_F_MORE), "io_uring : POLL_ADD multiple, 2 coups (F_MORE)");
    s = ur_get(&r, OP_POLL_REMOVE, -1, 19);
    s->addr = 18;
    ur_submit(&r, 2);
    ur_wait(&r, &c1);
    ur_wait(&r, &c2);
    CHECK((c1.user_data == 19 ? c1.res : c2.res) == 0, "io_uring : POLL_REMOVE");

    /* CQE_SKIP_SUCCESS. */
    s = ur_get(&r, OP_NOP, -1, 20);
    s->flags = F_CQE_SKIP;
    ur_get(&r, OP_NOP, -1, 21);
    ur_submit(&r, 1);
    ur_wait(&r, &c1);
    msleep(5);
    CHECK(c1.user_data == 21 && ur_ready(&r) == 0, "io_uring : IOSQE_CQE_SKIP_SUCCESS (une seule completion)");

    /* eventfd de notification. */
    int nfd = eventfd(0, EFD_NONBLOCK);
    int rr = syscall(427, r.fd, 4, &nfd, 1);
    ur_get(&r, OP_NOP, -1, 22);
    ur_submit(&r, 1);
    ur_wait(&r, &c1);
    v = 0;
    read(nfd, &v, 8);
    CHECK(rr == 0 && v >= 1, "io_uring : REGISTER_EVENTFD, eventfd signale (%llu)", (unsigned long long)v);
    syscall(427, r.fd, 5, NULL, 0);

    /* Sondage des opérations. */
    struct { uint8_t last, len; uint16_t resv; uint32_t resv2[3]; struct { uint8_t op, resv; uint16_t flags; uint32_t resv2; } ops[64]; } probe;
    memset(&probe, 0, sizeof probe);
    rr = syscall(427, r.fd, 8, &probe, 64);
    CHECK(rr == 0 && (probe.ops[OP_READ].flags & 1) && (probe.ops[OP_ACCEPT].flags & 1), "io_uring : PROBE (derniere operation %d)", probe.last);

    /* Fichiers enregistrés. */
    int fds[1] = {pp[1]};
    rr = syscall(427, r.fd, 2, fds, 1);
    s = ur_get(&r, OP_WRITE, 0, 23);
    s->flags = F_FIXED_FILE;
    s->addr = (uintptr_t)"fixe";
    s->len = 4;
    ur_submit(&r, 1);
    ur_wait(&r, &c1);
    memset(buf, 0, sizeof buf);
    read(pp[0], buf, 4);
    CHECK(rr == 0 && c1.res == 4 && !memcmp(buf, "fixe", 4), "io_uring : REGISTER_FILES, ecriture par fichier enregistre");

    /* OPENAT, STATX, CLOSE par l'anneau. */
    s = ur_get(&r, OP_OPENAT, AT_FDCWD, 24);
    s->addr = (uintptr_t)"/tmp/m3net_uring";
    s->op_flags = O_RDONLY;
    ur_submit(&r, 1);
    ur_wait(&r, &c1);
    /* `struct statx` brute (absente du musl de Debian) : stx_size à l'offset 40. */
    uint64_t stx[32];
    memset(stx, 0, sizeof stx);
    s = ur_get(&r, OP_STATX, AT_FDCWD, 25);
    s->addr = (uintptr_t)"/tmp/m3net_uring";
    s->len = 0xfff;
    s->off = (uintptr_t)stx;
    ur_submit(&r, 1);
    ur_wait(&r, &c2);
    s = ur_get(&r, OP_CLOSE, c1.res, 26);
    ur_submit(&r, 1);
    struct cqe c3;
    ur_wait(&r, &c3);
    CHECK(c1.res >= 0 && c2.res == 0 && stx[5] == 16 && c3.res == 0, "io_uring : OPENAT (fd %d), STATX (%llu octets), CLOSE", c1.res,
          (unsigned long long)stx[5]);

    /* Délai d'attente d'io_uring_enter (EXT_ARG). */
    struct { uint64_t sigmask; uint32_t sz, pad; uint64_t ts; } arg = {0, 8, 0, 0};
    struct kts w = {0, 10000000};
    arg.ts = (uint64_t)(uintptr_t)&w;
    rr = syscall(426, r.fd, 0, 1, ENTER_GETEVENTS | ENTER_EXT_ARG, &arg, sizeof arg);
    CHECK(rr < 0 && errno == ETIME, "io_uring : attente bornee (EXT_ARG) -> ETIME");
    close(pp[0]);
    close(pp[1]);
    close(efd);
    close(nfd);
    ur_close(&r);
}

static void test_uring_net(void) {
    struct uparams p = {0};
    struct uring r;
    ur_init(&r, 16, &p);
    int l = listener4("127.0.0.1", 0, 0, 0, 8);
    int port = port_of(l);
    struct sqe *s = ur_get(&r, OP_ACCEPT, l, 30);
    ur_submit(&r, 0);
    int c = connect4("127.0.0.1", port);
    struct cqe c1, c2;
    ur_wait(&r, &c1);
    int a = c1.res;
    CHECK(c1.user_data == 30 && a >= 0, "io_uring : ACCEPT en attente puis complete par connect (fd %d)", a);
    /* Tampons fournis (BUFFER_SELECT) pour RECV. */
    static char pool[4][64];
    s = ur_get(&r, OP_PROVIDE_BUFFERS, 4, 31);
    s->addr = (uintptr_t)pool;
    s->len = 64;
    s->off = 100;
    s->buf_index = 7;
    ur_submit(&r, 1);
    ur_wait(&r, &c1);
    s = ur_get(&r, OP_RECV, a, 32);
    s->flags = F_BUFFER_SELECT;
    s->buf_index = 7;
    ur_submit(&r, 0);
    s = ur_get(&r, OP_SEND, c, 33);
    s->addr = (uintptr_t)"selection";
    s->len = 9;
    ur_submit(&r, 2);
    ur_wait(&r, &c1);
    ur_wait(&r, &c2);
    struct cqe *rcv = c1.user_data == 32 ? &c1 : &c2, *snd = c1.user_data == 33 ? &c1 : &c2;
    int bid = rcv->flags >> 16;
    CHECK(snd->res == 9 && rcv->res == 9 && (rcv->flags & CQE_F_BUFFER) && bid >= 100 && bid < 104 && !memcmp(pool[bid - 100], "selection", 9),
          "io_uring : SEND/RECV TCP, tampon fourni %d", bid);
    close(a);
    close(c);
    close(l);
    /* MSG_RING : une complétion déposée dans un autre anneau. */
    struct uparams p2 = {0};
    struct uring r2;
    ur_init(&r2, 4, &p2);
    s = ur_get(&r, OP_MSG_RING, r2.fd, 34);
    s->len = 1234;
    s->off = 0xabcd;
    ur_submit(&r, 1);
    ur_wait(&r, &c1);
    ur_wait(&r2, &c2);
    CHECK(c1.res == 0 && c2.user_data == 0xabcd && c2.res == 1234, "io_uring : MSG_RING vers un autre anneau");
    ur_close(&r2);
    ur_close(&r);
    /* Débordement sans perte : 24 NOP sans vider un anneau de complétion de 8. */
    struct uparams p3 = {.flags = SETUP_CQSIZE, .cq_entries = 8};
    ur_init(&r, 8, &p3);
    for (int b = 0; b < 3; b++) {
        for (int i = 0; i < 8; i++) ur_get(&r, OP_NOP, -1, b * 8 + i);
        ur_submit(&r, 0);
    }
    int n = 0, order = 1;
    struct cqe cq;
    while (ur_wait(&r, &cq) == 0 && n < 24) order &= cq.user_data == (uint64_t)n, n++;
    CHECK(n == 24 && order, "io_uring : debordement sans perte (FEAT_NODROP), %d completions dans l'ordre", n);
    ur_close(&r);
    /* SQPOLL : soumission sans appel système. */
    struct uparams p4 = {.flags = SETUP_SQPOLL, .sq_thread_idle = 50};
    int e = ur_init(&r, 8, &p4);
    ur_get(&r, OP_NOP, -1, 40);
    __atomic_store_n(r.sq_tail, r.tail, __ATOMIC_RELEASE);
    long long end = mono_ns() + 1000000000LL;
    while (!ur_ready(&r) && mono_ns() < end) sched_yield();
    int direct = ur_ready(&r) > 0;
    if (direct) ur_wait(&r, &cq);
    msleep(150);
    int need = (__atomic_load_n(r.sq_flags, __ATOMIC_ACQUIRE) & 1) != 0;
    ur_get(&r, OP_NOP, -1, 41);
    __atomic_store_n(r.sq_tail, r.tail, __ATOMIC_RELEASE);
    syscall(426, r.fd, 1, 0, ENTER_SQ_WAKEUP, NULL, 0);
    int woke = ur_wait(&r, &cq) == 0 && cq.user_data == 41;
    CHECK(e == 0 && direct && need && woke, "io_uring : SQPOLL (soumission sans appel %d, NEED_WAKEUP %d, reveil %d)", direct, need, woke);
    ur_close(&r);
}

int main(int argc, char **argv) {
    printf("m3net : jalon M3 reseau et io_uring (pid %d)\n", getpid());
    fflush(stdout);
    signal(SIGPIPE, SIG_IGN);
    test_tcp_basic();
    test_tcp_states();
    test_tcp_reuse();
    test_ipv6();
    test_udp();
    test_net_misc();
    test_uring_basic();
    test_uring_net();
    if (argc >= 3 && !strcmp(argv[1], "ext")) test_external(atoi(argv[2]));
    printf("m3net : %s (%d echec%s)\n", fails ? "ECHEC" : "OK", fails, fails > 1 ? "s" : "");
    return fails;
}
