/* proctest — jalon M2 de MaiProc : processus, signaux, tubes, fils, depuis l'anneau 3.
 *
 * Usage : proctest            tous les contrôles ; code de sortie = nombre d'échecs
 *         proctest exit N     sortie immédiate avec le code N (cible d'execve)
 *         proctest echo       recopie l'entrée standard sur la sortie standard
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <setjmp.h>
#include <signal.h>
#include <spawn.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

extern char **environ;
static int fails;
static char self[512];

#define CHECK(cond, ...)                                  \
    do {                                                  \
        int ok_ = (cond);                                 \
        printf(ok_ ? "[ok] " : "[ECHEC] ");               \
        printf(__VA_ARGS__);                              \
        printf("\n");                                     \
        fflush(stdout);                                   \
        if (!ok_) fails++;                                \
    } while (0)

static long long now_us(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1000000LL + t.tv_nsec / 1000;
}

/* --- fork, copie à l'écriture, wait4 ------------------------------------------------------ */

static void test_fork(void) {
    volatile int shared_var = 41;
    pid_t me = getpid();
    pid_t c = fork();
    if (c == 0) {
        shared_var = 1234; /* copie à l'écriture : le parent ne doit pas le voir */
        _exit(getppid() == me && shared_var == 1234 ? 7 : 1);
    }
    CHECK(c > 0, "fork -> pid %d", c);
    int st = 0;
    pid_t w = waitpid(c, &st, 0);
    CHECK(w == c && WIFEXITED(st) && WEXITSTATUS(st) == 7, "wait4 : enfant %d code %d (attendu 7)", w, WEXITSTATUS(st));
    CHECK(shared_var == 41, "copie a l'ecriture : variable du parent intacte (%d)", shared_var);
    CHECK(waitpid(-1, &st, WNOHANG) == -1 && errno == ECHILD, "plus d'enfant : ECHILD");

    /* Fork d'un gros processus : tables partagées, rien de copié. */
    size_t big = 64u << 20;
    char *m = mmap(NULL, big, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    for (size_t i = 0; i < big; i += 4096) m[i] = (char)(i >> 12);
    long long t0 = now_us();
    c = fork();
    if (c == 0) {
        int ok = 1;
        for (size_t i = 0; i < big; i += 1 << 20) ok &= m[i] == (char)(i >> 12);
        m[0] = 99; /* une seule page copiée */
        _exit(ok ? 0 : 1);
    }
    long long t1 = now_us();
    waitpid(c, &st, 0);
    long long t2 = now_us();
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0 && m[0] == 0, "fork 64 Mio : enfant lit les pages du parent, ecriture privee");
    printf("     fork 64 Mio : %lld us (appel), %lld us (enfant complet)\n", t1 - t0, t2 - t0);
    munmap(m, big);
}

/* --- Mémoire partagée héritée --------------------------------------------------------------- */

static void test_shared(void) {
    volatile int *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    *p = 1;
    volatile int *q = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    pid_t c = fork();
    if (c == 0) {
        *p = 2;
        q[1024] = 77; /* page jamais touchée avant le fork */
        _exit(0);
    }
    int st;
    waitpid(c, &st, 0);
    CHECK(*p == 2 && q[1024] == 77, "MAP_SHARED herite : ecritures de l'enfant visibles (%d, %d)", *p, q[1024]);
}

/* --- Tubes ------------------------------------------------------------------------------- */

static void test_pipe(void) {
    int fd[2];
    CHECK(pipe2(fd, O_CLOEXEC) == 0, "pipe2");
    pid_t c = fork();
    if (c == 0) {
        close(fd[0]);
        const char *msg = "bonjour par le tube";
        write(fd[1], msg, strlen(msg));
        _exit(0);
    }
    close(fd[1]);
    char buf[64] = {0};
    ssize_t n, tot = 0;
    while ((n = read(fd[0], buf + tot, sizeof buf - 1 - tot)) > 0) tot += n;
    CHECK(n == 0 && !strcmp(buf, "bonjour par le tube"), "lecture du tube puis fin de fichier : \"%s\"", buf);
    close(fd[0]);
    int st;
    waitpid(c, &st, 0);

    /* Lecteur fermé : EPIPE (SIGPIPE ignoré). */
    signal(SIGPIPE, SIG_IGN);
    pipe(fd);
    close(fd[0]);
    CHECK(write(fd[1], "x", 1) == -1 && errno == EPIPE, "ecriture sans lecteur : EPIPE");
    close(fd[1]);
    signal(SIGPIPE, SIG_DFL);

    /* Tube plein en non bloquant : EAGAIN apres la capacite. */
    pipe2(fd, O_NONBLOCK);
    char blk[4096];
    memset(blk, 'a', sizeof blk);
    long total = 0;
    while (write(fd[1], blk, sizeof blk) > 0) total += sizeof blk;
    CHECK(errno == EAGAIN && total >= 65536, "tube plein : EAGAIN apres %ld octets", total);
    close(fd[0]);
    close(fd[1]);

    /* Redirection + exec : l'enfant "echo" recopie stdin sur stdout. */
    int in[2], out[2];
    pipe(in);
    pipe(out);
    c = fork();
    if (c == 0) {
        dup2(in[0], 0);
        dup2(out[1], 1);
        close(in[0]); close(in[1]); close(out[0]); close(out[1]);
        execl(self, "proctest", "echo", (char *)NULL);
        _exit(127);
    }
    close(in[0]);
    close(out[1]);
    write(in[1], "ligne relayee\n", 14);
    close(in[1]);
    memset(buf, 0, sizeof buf);
    tot = 0;
    while ((n = read(out[0], buf + tot, sizeof buf - 1 - tot)) > 0) tot += n;
    close(out[0]);
    waitpid(c, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0 && !strcmp(buf, "ligne relayee\n"), "dup2 + execve + tubes : \"%.13s\"", buf);
}

/* --- execve, vfork, posix_spawn ---------------------------------------------------------------- */

static void test_exec(void) {
    pid_t c = fork();
    if (c == 0) {
        char *av[] = {"proctest", "exit", "42", NULL};
        execve(self, av, environ);
        _exit(1);
    }
    int st;
    waitpid(c, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 42, "fork + execve : code %d (attendu 42)", WEXITSTATUS(st));

    CHECK(execl("/nexiste/pas", "x", (char *)NULL) == -1 && errno == ENOENT, "execve d'un fichier absent : ENOENT, processus intact");

    volatile int seen = 0;
    c = vfork();
    if (c == 0) {
        seen = 1; /* espace partagé jusqu'à l'exec */
        execl(self, "proctest", "exit", "3", (char *)NULL);
        _exit(1);
    }
    waitpid(c, &st, 0);
    CHECK(seen == 1 && WIFEXITED(st) && WEXITSTATUS(st) == 3, "vfork + execve : memoire partagee, code %d", WEXITSTATUS(st));

    pid_t sp;
    char *av[] = {"proctest", "exit", "5", NULL};
    int r = posix_spawn(&sp, self, NULL, NULL, av, environ);
    waitpid(sp, &st, 0);
    CHECK(r == 0 && WIFEXITED(st) && WEXITSTATUS(st) == 5, "posix_spawn : code %d", WEXITSTATUS(st));
}

/* --- Signaux --------------------------------------------------------------------------------- */

static volatile sig_atomic_t got_usr1, got_alrm, got_chld, info_pid, on_alt;
static sigjmp_buf segv_jump;
static char altstack[65536];

static void on_usr1(int s, siginfo_t *si, void *uc) {
    (void)uc;
    got_usr1 = s;
    info_pid = si->si_pid;
}
static void on_alrm(int s) { (void)s; got_alrm++; }
static void on_chld(int s) { (void)s; got_chld++; }
static void on_segv(int s) {
    char here;
    on_alt = (&here >= altstack && &here < altstack + sizeof altstack);
    siglongjmp(segv_jump, s);
}

static void test_signals(void) {
    struct sigaction sa = {0};
    sa.sa_sigaction = on_usr1;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGUSR1, &sa, NULL);
    raise(SIGUSR1);
    CHECK(got_usr1 == SIGUSR1 && info_pid == getpid(), "gestionnaire SA_SIGINFO : signal %d, si_pid %d", (int)got_usr1, (int)info_pid);

    /* Masque, attente, sigsuspend. */
    sigset_t blk, old;
    sigemptyset(&blk);
    sigaddset(&blk, SIGUSR1);
    sigprocmask(SIG_BLOCK, &blk, &old);
    got_usr1 = 0;
    kill(getpid(), SIGUSR1);
    sigset_t pend;
    sigpending(&pend);
    CHECK(got_usr1 == 0 && sigismember(&pend, SIGUSR1), "signal bloque : en attente, non livre");
    sigset_t wait = old;
    sigdelset(&wait, SIGUSR1);
    int r = sigsuspend(&wait);
    CHECK(r == -1 && errno == EINTR && got_usr1 == SIGUSR1, "sigsuspend : livre puis EINTR");
    sigprocmask(SIG_SETMASK, &old, NULL);

    /* Faute récupérée sur la pile alternative. */
    stack_t ss = {.ss_sp = altstack, .ss_size = sizeof altstack, .ss_flags = 0};
    sigaltstack(&ss, NULL);
    struct sigaction sg = {0};
    sg.sa_handler = on_segv;
    sg.sa_flags = SA_ONSTACK | SA_NODEFER;
    sigaction(SIGSEGV, &sg, NULL);
    int got = sigsetjmp(segv_jump, 1);
    if (got == 0) {
        *(volatile int *)16 = 1;
        CHECK(0, "faute non signalee");
    }
    CHECK(got == SIGSEGV && on_alt, "SIGSEGV recupere par siglongjmp, sur la pile alternative (%d)", (int)on_alt);
    signal(SIGSEGV, SIG_DFL);

    /* alarm + pause. */
    signal(SIGALRM, on_alrm);
    long long t0 = now_us();
    alarm(1);
    pause();
    long long dt = now_us() - t0;
    CHECK(got_alrm == 1 && dt > 800000 && dt < 2500000, "alarm(1) + pause : SIGALRM apres %lld ms", dt / 1000);

    /* nanosleep interrompu : EINTR et temps restant. */
    struct itimerval it = {.it_value = {0, 100000}};
    setitimer(ITIMER_REAL, &it, NULL);
    struct timespec req = {2, 0}, rem = {0, 0};
    r = nanosleep(&req, &rem);
    CHECK(r == -1 && errno == EINTR && rem.tv_sec >= 1, "nanosleep interrompu : EINTR, reste %ld s", (long)rem.tv_sec);

    /* SA_RESTART : lecture de tube relancée après le gestionnaire. */
    int fd[2];
    pipe(fd);
    struct sigaction sr = {0};
    sr.sa_handler = on_alrm;
    sr.sa_flags = SA_RESTART;
    sigaction(SIGALRM, &sr, NULL);
    pid_t c = fork();
    if (c == 0) {
        usleep(300000);
        write(fd[1], "z", 1);
        _exit(0);
    }
    got_alrm = 0;
    struct itimerval it2 = {.it_value = {0, 100000}};
    setitimer(ITIMER_REAL, &it2, NULL);
    char ch = 0;
    ssize_t n = read(fd[0], &ch, 1);
    CHECK(n == 1 && ch == 'z' && got_alrm == 1, "SA_RESTART : read relance apres SIGALRM (%zd)", n);
    int st;
    waitpid(c, &st, 0);
    close(fd[0]);
    close(fd[1]);

    /* SIGCHLD, mort par signal. */
    signal(SIGCHLD, on_chld);
    c = fork();
    if (c == 0) {
        for (;;) pause();
    }
    kill(c, SIGTERM);
    waitpid(c, &st, 0);
    CHECK(WIFSIGNALED(st) && WTERMSIG(st) == SIGTERM && got_chld >= 1, "kill SIGTERM : enfant tue (%d), SIGCHLD recu", WTERMSIG(st));

    /* Arrêt, reprise, WNOHANG. */
    c = fork();
    if (c == 0) {
        for (;;) usleep(10000);
    }
    CHECK(waitpid(c, &st, WNOHANG) == 0, "WNOHANG : enfant vivant -> 0");
    kill(c, SIGSTOP);
    CHECK(waitpid(c, &st, WUNTRACED) == c && WIFSTOPPED(st) && WSTOPSIG(st) == SIGSTOP, "SIGSTOP : WIFSTOPPED");
    kill(c, SIGCONT);
    CHECK(waitpid(c, &st, WCONTINUED) == c && WIFCONTINUED(st), "SIGCONT : WIFCONTINUED");
    kill(c, SIGKILL);
    CHECK(waitpid(c, &st, 0) == c && WIFSIGNALED(st) && WTERMSIG(st) == SIGKILL, "SIGKILL : enfant tue");
    signal(SIGCHLD, SIG_DFL);

    /* Groupes de processus. */
    c = fork();
    if (c == 0) {
        setpgid(0, 0);
        for (;;) pause();
    }
    setpgid(c, c);
    usleep(20000);
    CHECK(getpgid(c) == c, "setpgid : groupe %d", getpgid(c));
    kill(-c, SIGKILL);
    waitpid(c, &st, 0);
    CHECK(WIFSIGNALED(st), "kill(-pgid) : groupe tue");
}

/* --- Fils, futex ---------------------------------------------------------------------------- */

static pthread_mutex_t mtx = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cnd = PTHREAD_COND_INITIALIZER;
static long counter;
static atomic_int ready;
static __thread int tls_id;

static void *worker(void *arg) {
    tls_id = (int)(intptr_t)arg;
    for (int i = 0; i < 20000; i++) {
        pthread_mutex_lock(&mtx);
        counter++;
        pthread_mutex_unlock(&mtx);
        if ((i & 1023) == 0) sched_yield();
    }
    atomic_fetch_add(&ready, 1);
    pthread_mutex_lock(&mtx);
    pthread_cond_broadcast(&cnd);
    pthread_mutex_unlock(&mtx);
    return (void *)(intptr_t)(tls_id * 10);
}

static void test_threads(void) {
    enum { N = 4 };
    pthread_t th[N];
    long long t0 = now_us();
    for (int i = 0; i < N; i++) pthread_create(&th[i], NULL, worker, (void *)(intptr_t)(i + 1));
    pthread_mutex_lock(&mtx);
    while (atomic_load(&ready) < N) pthread_cond_wait(&cnd, &mtx);
    pthread_mutex_unlock(&mtx);
    int ok = 1;
    for (int i = 0; i < N; i++) {
        void *ret;
        pthread_join(th[i], &ret);
        ok &= (intptr_t)ret == (i + 1) * 10;
    }
    long long dt = now_us() - t0;
    CHECK(counter == N * 20000 && ok, "4 fils, mutex contendu : compteur %ld (attendu %d), join OK, %lld ms", counter, N * 20000, dt / 1000);

    struct timespec dl;
    clock_gettime(CLOCK_REALTIME, &dl);
    dl.tv_nsec += 50 * 1000000;
    if (dl.tv_nsec >= 1000000000) { dl.tv_sec++; dl.tv_nsec -= 1000000000; }
    pthread_mutex_lock(&mtx);
    t0 = now_us();
    int r = pthread_cond_timedwait(&cnd, &mtx, &dl);
    dt = now_us() - t0;
    pthread_mutex_unlock(&mtx);
    CHECK(r == ETIMEDOUT && dt >= 30000 && dt < 1000000, "pthread_cond_timedwait : ETIMEDOUT apres %lld ms", dt / 1000);

    /* Un fil qui fait exit_group termine tout le processus (dans un enfant). */
    pid_t c = fork();
    if (c == 0) {
        pthread_t t;
        pthread_create(&t, NULL, (void *(*)(void *))exit, (void *)9);
        for (;;) pause();
    }
    int st;
    waitpid(c, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 9, "exit() depuis un fil : processus termine, code %d", WEXITSTATUS(st));
}

int main(int argc, char **argv) {
    if (argc > 2 && !strcmp(argv[1], "exit")) return atoi(argv[2]);
    if (argc > 1 && !strcmp(argv[1], "echo")) {
        char b[256];
        ssize_t n;
        while ((n = read(0, b, sizeof b)) > 0) write(1, b, n);
        return 0;
    }
    ssize_t n = readlink("/proc/self/exe", self, sizeof self - 1);
    if (n <= 0) strcpy(self, argv[0]);
    printf("proctest : jalon M2 (pid %d, %s)\n", getpid(), self);
    fflush(stdout);
    test_fork();
    test_shared();
    test_pipe();
    test_exec();
    test_signals();
    test_threads();
    printf("proctest : %s (%d echec%s)\n", fails ? "ECHEC" : "OK", fails, fails > 1 ? "s" : "");
    return fails;
}
