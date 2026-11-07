/* maitest — vérifie l'ABI Linux de MaiProc depuis un vrai processus en anneau 3.
 *
 * Usage : maitest [args...]   tous les contrôles ; code de sortie = nombre d'échecs
 *         maitest segv        accès à l'adresse 0 : le processus doit être tué (signal 11)
 *         maitest exit N      sortie immédiate avec le code N
 */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/random.h>
#include <sys/stat.h>
#include <sys/utsname.h>
#include <time.h>
#include <unistd.h>

static int fails;
static __thread int tls_counter = 7;

#define CHECK(cond, ...)                                  \
    do {                                                  \
        int ok_ = (cond);                                 \
        printf(ok_ ? "[ok] " : "[ECHEC] ");               \
        printf(__VA_ARGS__);                              \
        printf("\n");                                     \
        if (!ok_) fails++;                                \
    } while (0)

static long ms_between(struct timespec a, struct timespec b) {
    return (b.tv_sec - a.tv_sec) * 1000 + (b.tv_nsec - a.tv_nsec) / 1000000;
}

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "segv")) {
        printf("maitest : acces a l'adresse 0...\n");
        fflush(stdout);
        *(volatile int *)0 = 1;
        printf("[ECHEC] toujours vivant\n");
        return 99;
    }
    if (argc > 2 && !strcmp(argv[1], "exit"))
        return atoi(argv[2]);

    printf("maitest : processus Linux sur MaiOS (pid %d, tid %d)\n", getpid(), (int)gettid());
    CHECK(argc >= 1 && argv[0] != NULL, "argc=%d argv[0]=%s", argc, argv[0]);
    for (int i = 1; i < argc; i++)
        printf("     argv[%d]=%s\n", i, argv[i]);
    CHECK(getenv("PATH") != NULL, "PATH=%s", getenv("PATH") ? getenv("PATH") : "(absent)");

    /* Vecteur auxiliaire. */
    CHECK(getauxval(AT_PAGESZ) == 4096, "AT_PAGESZ=%lu", getauxval(AT_PAGESZ));
    CHECK(getauxval(AT_RANDOM) != 0, "AT_RANDOM=%#lx", getauxval(AT_RANDOM));
    CHECK(getauxval(AT_PHDR) != 0, "AT_PHDR=%#lx AT_ENTRY=%#lx", getauxval(AT_PHDR), getauxval(AT_ENTRY));
    printf("     main=%p pile=%p\n", (void *)main, (void *)&argc);

    struct utsname u;
    CHECK(uname(&u) == 0, "uname : %s %s %s", u.sysname, u.release, u.machine);

    /* TLS (FS) et errno. */
    tls_counter++;
    CHECK(tls_counter == 8, "TLS (__thread) = %d", tls_counter);
    errno = 0;
    CHECK(close(12345) == -1 && errno == EBADF, "close(12345) -> EBADF (errno=%d)", errno);

    /* Tas : brk et mmap (musl mallocng), mremap via realloc. */
    char *p = malloc(100);
    CHECK(p != NULL, "malloc(100) -> %p", (void *)p);
    strcpy(p, "tas");
    size_t big = 64u << 20;
    char *q = malloc(big);
    CHECK(q != NULL, "malloc(64 Mio) -> %p", (void *)q);
    if (q) {
        memset(q, 0x5a, big);
        CHECK(q[0] == 0x5a && q[big - 1] == 0x5a, "64 Mio ecrits (fautes a la demande)");
        q = realloc(q, 2 * big);
        CHECK(q != NULL && q[big - 1] == 0x5a, "realloc(128 Mio) -> %p, contenu garde", (void *)q);
        free(q);
    }
    CHECK(!strcmp(p, "tas"), "petit bloc intact");
    free(p);

    /* mmap, mprotect, munmap. */
    int *m = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(m != MAP_FAILED && m[100] == 0, "mmap anonyme (pages zero) -> %p", (void *)m);
    if (m != MAP_FAILED) {
        m[0] = 42;
        CHECK(mprotect(m, 8192, PROT_READ) == 0 && m[0] == 42, "mprotect(PROT_READ)");
        CHECK(madvise(m, 8192, MADV_DONTNEED) == 0 && m[0] == 0, "madvise(DONTNEED) -> page zero");
        CHECK(munmap(m, 8192) == 0, "munmap");
    }

    /* Fichiers du VFS. */
    char cwd[256];
    CHECK(getcwd(cwd, sizeof cwd) != NULL, "getcwd = %s", cwd);
    const char msg[] = "Bonjour depuis l'anneau 3\n";
    int fd = open("/maitest.txt", O_CREAT | O_RDWR | O_TRUNC, 0644);
    CHECK(fd >= 0, "open(O_CREAT) -> fd %d", fd);
    if (fd >= 0) {
        CHECK(write(fd, msg, sizeof msg - 1) == (ssize_t)(sizeof msg - 1), "write");
        CHECK(lseek(fd, 0, SEEK_SET) == 0, "lseek(0)");
        char buf[64] = {0};
        CHECK(read(fd, buf, sizeof buf) == (ssize_t)(sizeof msg - 1) && !strcmp(buf, msg), "read : %.25s", buf);
        struct stat st;
        CHECK(fstat(fd, &st) == 0 && st.st_size == (off_t)(sizeof msg - 1) && S_ISREG(st.st_mode), "fstat taille %ld", (long)st.st_size);
        close(fd);
        CHECK(unlink("/maitest.txt") == 0 && access("/maitest.txt", F_OK) != 0, "unlink");
    }

    FILE *f = fopen("/proc/self/maps", "r");
    CHECK(f != NULL, "fopen(/proc/self/maps)");
    if (f) {
        char line[160];
        int n = 0;
        while (fgets(line, sizeof line, f) && n < 4) {
            printf("     %s", line);
            n++;
        }
        fclose(f);
    }

    DIR *d = opendir("/");
    int entries = 0;
    if (d) {
        struct dirent *e;
        printf("     / :");
        while ((e = readdir(d)) != NULL) {
            if (entries < 8)
                printf(" %s", e->d_name);
            entries++;
        }
        printf("\n");
        closedir(d);
    }
    CHECK(entries > 2, "readdir(/) : %d entrees", entries);

    /* Temps. */
    struct timespec t0, t1;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    struct timespec nap = {0, 50 * 1000 * 1000};
    nanosleep(&nap, NULL);
    clock_gettime(CLOCK_MONOTONIC, &t1);
    long ms = ms_between(t0, t1);
    CHECK(ms >= 45 && ms < 1000, "nanosleep(50 ms) -> %ld ms", ms);
    time_t now = time(NULL);
    char date[64];
    strftime(date, sizeof date, "%Y-%m-%d %H:%M:%S UTC", gmtime(&now));
    CHECK(now > 1600000000, "date : %s", date);

    /* Aléa. */
    unsigned long r1 = 0, r2 = 0;
    CHECK(getrandom(&r1, sizeof r1, 0) == sizeof r1 && getrandom(&r2, sizeof r2, 0) == sizeof r2 && r1 != r2,
          "getrandom : %016lx", r1);

    /* Virgule flottante (registres SSE du processus, sauvés à chaque changement). */
    volatile double x = 1.5;
    double acc = 0;
    for (int i = 0; i < 1000; i++)
        acc += x * i;
    CHECK(acc == 749250.0, "flottants : %.1f", acc);

    printf("maitest : %s (%d echec%s)\n", fails ? "ECHEC" : "OK", fails, fails > 1 ? "s" : "");
    return fails;
}
