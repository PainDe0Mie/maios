/* Banc S3 du plan 07 (Steam) : conteneurs — capacités, espaces de noms utilisateur, montage,
 * UTS, IPC et pid (init, orphelins, /proc par espace), subreaper, PDEATHSIG, pidfd,
 * `mount`/`umount2`/`pivot_root`/`chroot`, `openat2` (`RESOLVE_*`), instances de devpts,
 * `execve` sous `no_new_privs`, comme les emploient bubblewrap (pressure-vessel de Steam) et le
 * bac à sable de Chromium (`steamwebhelper`).
 *
 * Lancé en root ; les vérifications « sans privilège » se font dans un enfant qui passe en
 * uid/gid 1000 (aucune capacité), puis crée son espace utilisateur comme `unshare -r`.
 *
 *   run -s /extra_files/linux/nstest
 */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <stddef.h>
#include <arpa/inet.h>
#include <fcntl.h>
#include <net/if.h>
#include <netinet/in.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/ipc.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/shm.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/un.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef CLONE_NEWCGROUP
#define CLONE_NEWCGROUP 0x02000000
#endif
#ifndef MS_SLAVE
#define MS_SLAVE (1 << 19)
#endif

static int fails, oks;

#define CHECK(cond, ...)                                                   \
    do {                                                                   \
        if (cond) {                                                        \
            oks++;                                                         \
        } else {                                                           \
            fails++;                                                       \
            printf("nstest : [ECHEC] ");                                   \
            printf(__VA_ARGS__);                                           \
            printf(" (errno %d)\n", errno);                                \
        }                                                                  \
    } while (0)

struct cap_hdr {
    unsigned version;
    int pid;
};
struct cap_data {
    unsigned eff, prm, inh;
};

static unsigned long long cap_eff(void) {
    struct cap_hdr h = {0x20080522, 0};
    struct cap_data d[2];
    if (syscall(SYS_capget, &h, d) != 0)
        return ~0ULL;
    return d[0].eff | ((unsigned long long)d[1].eff << 32);
}

static int write_file(const char *path, const char *text) {
    int fd = open(path, O_WRONLY);
    if (fd < 0)
        return -1;
    int n = write(fd, text, strlen(text));
    int e = errno;
    close(fd);
    errno = e;
    return n == (int)strlen(text) ? 0 : -1;
}

static int read_file(const char *path, char *buf, int size) {
    int fd = open(path, O_RDONLY);
    if (fd < 0)
        return -1;
    int n = 0, r;
    while (n < size - 1 && (r = read(fd, buf + n, size - 1 - n)) > 0)
        n += r;
    buf[n] = 0;
    close(fd);
    return n;
}

static int exists(const char *p) {
    struct stat st;
    return stat(p, &st) == 0;
}

/* Enfant : exécute `fn` et rend son nombre d'échecs comme code de sortie. */
static int in_child(int (*fn)(void)) {
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        fails = 0;
        oks = 0;
        int r = fn();
        printf("nstest : (enfant %d : %d ok, %d echec)\n", getpid(), oks, fails + r);
        fflush(stdout);
        _exit((fails + r) > 100 ? 100 : fails + r);
    }
    int st = 0;
    waitpid(pid, &st, 0);
    int f = WIFEXITED(st) ? WEXITSTATUS(st) : 1;
    if (!WIFEXITED(st))
        printf("nstest : [ECHEC] enfant tue (statut %#x)\n", st);
    fails += f;
    return f;
}

/* --- 1. Capacités de root ------------------------------------------------------------------ */

static int prctl_read_bnd(void);
static int rtnl_lo_up(int with_addr);

static void test_root_caps(void) {
    CHECK(cap_eff() == ((1ULL << 41) - 1), "root : toutes les capacites (%llx)", cap_eff());
    CHECK(prctl_read_bnd(), "PR_CAPBSET_READ");
}

static int prctl_read_bnd(void) {
    return syscall(SYS_prctl, 23 /* PR_CAPBSET_READ */, 21, 0, 0, 0) == 1 && syscall(SYS_prctl, 23, 99, 0, 0, 0) < 0 && errno == EINVAL;
}

/* Relance ce programme avec « capeff » : code 0 si le nouveau programme n'a aucune capacité
 * effective, 1 sinon. */
static int exec_capeff(void) {
    char self[256];
    ssize_t n = readlink("/proc/self/exe", self, sizeof self - 1);
    if (n <= 0)
        return -1;
    self[n] = 0;
    pid_t c = fork();
    if (c == 0) {
        execl(self, "nstest", "capeff", (char *)NULL);
        _exit(127);
    }
    int st;
    if (waitpid(c, &st, 0) != c || !WIFEXITED(st))
        return -1;
    return WEXITSTATUS(st);
}

/* Root sans no_new_privs : un execve rend le permis par l'ensemble limitant, même après un
 * capset qui a tout vidé (cap_bprm_creds_from_file, handle_privileged_root). */
static int capexec_child(void) {
    struct cap_hdr h = {0x20080522, 0};
    struct cap_data z[2];
    memset(z, 0, sizeof z);
    CHECK(syscall(SYS_capset, &h, z) == 0 && cap_eff() == 0, "capset : tout vider");
    CHECK(exec_capeff() == 1, "execve en root : capacites rendues par l'ensemble limitant");
    return 0;
}

static int nnp_child(void) {
    CHECK(syscall(SYS_prctl, 39, 0, 0, 0, 0) == 0, "no_new_privs absent au depart");
    CHECK(syscall(SYS_prctl, 38, 1, 0, 0, 0) == 0, "PR_SET_NO_NEW_PRIVS");
    CHECK(syscall(SYS_prctl, 39, 0, 0, 0, 0) == 1, "PR_GET_NO_NEW_PRIVS");
    char buf[4096];
    read_file("/proc/self/status", buf, sizeof buf);
    CHECK(strstr(buf, "NoNewPrivs:\t1") != NULL, "/proc/self/status NoNewPrivs");
    /* Ambiant : levée impossible sans héritable. */
    CHECK(syscall(SYS_prctl, 47, 2 /* RAISE */, 12, 0, 0) < 0 && errno == EPERM, "PR_CAP_AMBIENT_RAISE sans heritable");
    /* Sous no_new_privs, l'execve ne gagne rien (LSM_UNSAFE_NO_NEW_PRIVS : permis borné à
     * l'ancien) : c'est ce qui fait marcher `bwrap --cap-drop ALL` en root. */
    struct cap_hdr h = {0x20080522, 0};
    struct cap_data z[2];
    memset(z, 0, sizeof z);
    CHECK(syscall(SYS_capset, &h, z) == 0, "capset : tout vider");
    CHECK(exec_capeff() == 0, "execve sous no_new_privs : aucune capacite regagnee");
    return 0;
}

/* --- 2. Utilisateur sans privilège ----------------------------------------------------------- */

static int drop_to_1000(void) {
    if (setgroups(0, NULL) || setresgid(1000, 1000, 1000) || setresuid(1000, 1000, 1000))
        return -1;
    return 0;
}

static int unpriv_child(void) {
    CHECK(drop_to_1000() == 0, "passage en uid 1000");
    CHECK(cap_eff() == 0, "uid 1000 : aucune capacite (%llx)", cap_eff());
    char buf[4096];
    read_file("/proc/self/status", buf, sizeof buf);
    CHECK(strstr(buf, "CapEff:\t0000000000000000") != NULL, "/proc/self/status CapEff nul");
    errno = 0;
    CHECK(mount("none", "/tmp", "tmpfs", 0, NULL) < 0 && errno == EPERM, "mount sans privilege -> EPERM");
    CHECK(unshare(CLONE_NEWNS) < 0 && errno == EPERM, "unshare(NEWNS) sans privilege -> EPERM");
    CHECK(chroot("/tmp") < 0 && errno == EPERM, "chroot sans privilege -> EPERM");
    CHECK(sethostname("x", 1) < 0 && errno == EPERM, "sethostname sans privilege -> EPERM");
    CHECK(kill(getppid(), 0) < 0 && errno == EPERM, "kill d'un processus de root sans privilege -> EPERM");
    int s80 = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a80 = {.sin_family = AF_INET, .sin_port = htons(80)};
    CHECK(bind(s80, (struct sockaddr *)&a80, sizeof a80) < 0 && errno == EACCES, "bind du port 80 sans CAP_NET_BIND_SERVICE -> EACCES");
    close(s80);
    CHECK(rtnl_lo_up(0) == -EPERM, "RTM_NEWLINK sans CAP_NET_ADMIN -> EPERM");
    CHECK(unshare(CLONE_NEWNET) < 0 && errno == EPERM, "unshare(NEWNET) sans privilege -> EPERM");
    return 0;
}

/* --- 3. Espace utilisateur + montage, comme `unshare -r --mount` -------------------------------- */

static int map_root(void) {
    char m[64];
    if (write_file("/proc/self/setgroups", "deny") < 0)
        return -1;
    snprintf(m, sizeof m, "0 %d 1", 1000);
    if (write_file("/proc/self/uid_map", m) < 0)
        return -2;
    if (write_file("/proc/self/gid_map", m) < 0)
        return -3;
    return 0;
}

static int userns_child(void) {
    CHECK(drop_to_1000() == 0, "passage en uid 1000");
    int oldmnt = open("/proc/self/ns/mnt", O_RDONLY);
    CHECK(oldmnt >= 0, "ouverture de /proc/self/ns/mnt");
    char l1[64] = {0}, l2[64] = {0};
    readlink("/proc/self/ns/user", l1, sizeof l1 - 1);
    CHECK(unshare(CLONE_NEWUSER | CLONE_NEWNS) == 0, "unshare(NEWUSER|NEWNS)");
    readlink("/proc/self/ns/user", l2, sizeof l2 - 1);
    CHECK(strncmp(l1, "user:[", 6) == 0 && strcmp(l1, l2) != 0, "nouvel espace utilisateur (%s -> %s)", l1, l2);
    CHECK(getuid() == 65534, "uid sans carte = 65534 (%d)", getuid());
    CHECK(cap_eff() == ((1ULL << 41) - 1), "toutes les capacites dans le nouvel espace");
    /* Carte non privilégiée : sans `setgroups=deny`, gid_map refusée. */
    CHECK(write_file("/proc/self/gid_map", "0 1000 1") < 0 && errno == EPERM, "gid_map avant setgroups=deny -> EPERM");
    CHECK(map_root() == 0, "setgroups=deny, uid_map et gid_map");
    CHECK(write_file("/proc/self/uid_map", "0 1000 1") < 0 && errno == EPERM, "deuxieme ecriture de uid_map -> EPERM");
    CHECK(getuid() == 0 && geteuid() == 0 && getgid() == 0, "root dans l'espace (%d/%d)", getuid(), getgid());
    char buf[8192];
    read_file("/proc/self/uid_map", buf, sizeof buf);
    CHECK(strstr(buf, "         0       1000          1") != NULL, "uid_map relue : %s", buf);
    CHECK(setgroups(0, NULL) < 0 && errno == EPERM, "setgroups refuse (deny)");
    struct stat st;
    stat("/", &st);
    CHECK(st.st_uid == 65534, "proprietaire de / (root global) = 65534 (%d)", (int)st.st_uid);

    /* tmpfs privé */
    mkdir("/tmp/nst", 0755);
    CHECK(mount("tmpfs", "/tmp/nst", "tmpfs", MS_NOSUID | MS_NODEV, "mode=755,size=1m") == 0, "mount tmpfs");
    CHECK(write_file("/tmp/nst/x", "") < 0, "fichier absent (creation par open)");
    int fd = open("/tmp/nst/x", O_CREAT | O_WRONLY, 0644);
    CHECK(fd >= 0 && write(fd, "abc", 3) == 3, "ecriture dans le tmpfs");
    close(fd);
    stat("/tmp/nst/x", &st);
    CHECK(st.st_uid == 0, "fichier cree : proprietaire 0 dans l'espace (%d)", (int)st.st_uid);
    CHECK(chown("/tmp/nst/x", 5, 5) < 0 && errno == EINVAL, "chown vers une identite sans carte -> EINVAL");
    read_file("/proc/self/mountinfo", buf, sizeof buf);
    CHECK(strstr(buf, " /tmp/nst rw,nosuid,nodev,relatime - tmpfs tmpfs rw,mode=755,size=1m") != NULL, "mountinfo : %s", buf);

    /* Liaisons, lecture seule par montage */
    mkdir("/tmp/nst/a", 0755);
    mkdir("/tmp/nst/b", 0755);
    write_file("/tmp/nst/a/f", "");
    close(open("/tmp/nst/a/f", O_CREAT | O_WRONLY, 0644));
    CHECK(mount("/tmp/nst/a", "/tmp/nst/b", NULL, MS_BIND | MS_REC, NULL) == 0, "mount --rbind a b");
    CHECK(exists("/tmp/nst/b/f"), "b/f visible par la liaison");
    CHECK(mount("none", "/tmp/nst/b", NULL, MS_BIND | MS_REMOUNT | MS_RDONLY | MS_NOSUID | MS_NODEV, NULL) == 0, "remount,bind,ro de b");
    CHECK(open("/tmp/nst/b/g", O_CREAT | O_WRONLY, 0644) < 0 && errno == EROFS, "creation dans b -> EROFS");
    CHECK(open("/tmp/nst/b/f", O_WRONLY) < 0 && errno == EROFS, "ecriture dans b -> EROFS");
    fd = open("/tmp/nst/a/g", O_CREAT | O_WRONLY, 0644);
    CHECK(fd >= 0, "a reste inscriptible");
    close(fd);
    CHECK(exists("/tmp/nst/b/g"), "b voit le fichier cree par a");

    /* Montages hérités verrouillés */
    CHECK(umount2("/proc", 0) < 0 && errno == EINVAL, "umount d'un montage verrouille -> EINVAL");
    CHECK(mount("none", "/proc", NULL, MS_BIND | MS_REMOUNT, NULL) < 0 && errno == EPERM, "retirer nosuid d'un montage verrouille -> EPERM");
    CHECK(mount("proc", "/tmp/nst/b", "proc", 0, NULL) < 0 && errno == EPERM, "monter proc sans espace pid -> EPERM");

    /* Socket Unix jointe par un autre chemin (liaison par inode) */
    int srv = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un sa = {.sun_family = AF_UNIX};
    strcpy(sa.sun_path, "/tmp/nst/a/sock");
    CHECK(bind(srv, (struct sockaddr *)&sa, sizeof sa) == 0 && listen(srv, 1) == 0, "bind de a/sock");
    int cli = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un sb = {.sun_family = AF_UNIX};
    strcpy(sb.sun_path, "/tmp/nst/b/sock");
    CHECK(connect(cli, (struct sockaddr *)&sb, sizeof sb) == 0, "connect par b/sock (meme inode)");
    stat("/tmp/nst/a/sock", &st);
    CHECK(S_ISSOCK(st.st_mode), "a/sock est une socket");
    close(cli);
    close(srv);

    /* setns vers l'espace initial : refusé (pas CAP_SYS_ADMIN sur son propriétaire) */
    CHECK(setns(oldmnt, CLONE_NEWNS) < 0 && errno == EPERM, "setns vers l'espace de montage initial -> EPERM");

    /* UTS, IPC, cgroup */
    CHECK(unshare(CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWCGROUP) == 0, "unshare(NEWUTS|NEWIPC|NEWCGROUP)");
    CHECK(sethostname("nstest", 6) == 0, "sethostname dans l'espace UTS");
    struct utsname u;
    uname(&u);
    CHECK(strcmp(u.nodename, "nstest") == 0, "uname : %s", u.nodename);
    int id = shmget(0x5a5a, 4096, IPC_CREAT | 0600);
    CHECK(id >= 0, "shmget dans l'espace IPC");
    CHECK(unshare(0x80 /* CLONE_NEWTIME */) < 0 && errno == EINVAL, "NEWTIME pas encore pris en charge -> EINVAL");
    return 0;
}

/* --- 5. Espace pid, comme bwrap --unshare-pid --proc /proc ---------------------------------- */

static int zp[2], sp[2];

static int count_proc_pids(void) {
    DIR *d = opendir("/proc");
    int n = 0;
    struct dirent *e;
    while (d && (e = readdir(d)))
        if (e->d_name[0] >= '1' && e->d_name[0] <= '9')
            n++;
    if (d)
        closedir(d);
    return n;
}

/* Init (pid 1) de l'espace : /proc, enfants, orphelins, signaux ; code de sortie = échecs. */
static int pidns_init(void) {
    fails = 0;
    oks = 0;
    char c;
    read(sp[0], &c, 1); /* le parent a lu notre NSpid */
    CHECK(getpid() == 1, "init : getpid = 1 (%d)", getpid());
    CHECK(getppid() == 0, "init : parent hors de l'espace -> getppid = 0 (%d)", getppid());
    CHECK(unshare(CLONE_NEWNS) == 0, "init : espace de montage propre");
    CHECK(mount("proc", "/proc", "proc", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL) == 0, "mount proc dans l'espace pid");
    char l[64] = {0};
    readlink("/proc/self", l, sizeof l - 1);
    CHECK(strcmp(l, "1") == 0, "/proc/self -> 1 (%s)", l);
    CHECK(count_proc_pids() == 1, "/proc ne montre que l'espace (%d)", count_proc_pids());
    char buf[4096];
    read_file("/proc/1/status", buf, sizeof buf);
    CHECK(strstr(buf, "\nPid:\t1\n") && strstr(buf, "\nPPid:\t0\n") && strstr(buf, "\nNSpid:\t1\n"), "status de l'init :\n%s", buf);
    /* Ses voisins ne le tuent pas sans gestionnaire (SIGNAL_UNKILLABLE) */
    CHECK(kill(1, SIGTERM) == 0 && kill(1, SIGKILL) == 0, "kill(1) dans l'espace : accepte");
    CHECK(getpid() == 1, "init toujours vivant apres SIGTERM et SIGKILL");
    pid_t ch = fork();
    if (ch == 0) {
        int ok = getpid() == 2 && getppid() == 1;
        kill(1, SIGKILL);
        _exit(ok ? 0 : 1);
    }
    CHECK(ch == 2, "premier enfant : pid 2 (%d)", ch);
    int st = 0;
    CHECK(waitpid(ch, &st, 0) == ch && WIFEXITED(st) && WEXITSTATUS(st) == 0, "enfant : getpid 2, getppid 1 (%#x)", st);
    /* Orphelin : recueilli par l'init */
    pid_t a = fork();
    if (a == 0) {
        if (fork() == 0) {
            for (int i = 0; i < 400 && getppid() != 1; i++)
                usleep(5000);
            _exit(getppid() == 1 ? 42 : 43);
        }
        _exit(0);
    }
    waitpid(a, &st, 0);
    pid_t got = waitpid(-1, &st, 0);
    CHECK(got > a && WIFEXITED(st) && WEXITSTATUS(st) == 42, "orphelin recueilli par l'init (pid %d, statut %#x)", got, st);
    /* Un dormeur garde le tube : il doit mourir avec l'init */
    pid_t z = fork();
    if (z == 0) {
        for (;;)
            pause();
    }
    CHECK(count_proc_pids() == 2, "/proc : init et dormeur (%d)", count_proc_pids());
    snprintf(l, sizeof l, "/proc/%d/stat", z);
    char want[64];
    snprintf(want, sizeof want, "%d (nstest) S 1 ", z);
    /* Le dormeur peut ne pas avoir encore atteint pause() : attendre l'état S (2 s au plus). */
    for (int i = 0; i < 400; i++) {
        read_file(l, buf, sizeof buf);
        if (strncmp(buf, want, strlen(want)) == 0)
            break;
        usleep(5000);
    }
    CHECK(strncmp(buf, want, strlen(want)) == 0, "stat du dormeur : %s", buf);
    close(zp[1]);
    return fails;
}

static int pidns_child(void) {
    CHECK(drop_to_1000() == 0, "passage en uid 1000");
    pid_t me = getpid();
    CHECK(unshare(CLONE_NEWUSER | CLONE_NEWNS) == 0 && map_root() == 0, "espace utilisateur + montage");
    CHECK(unshare(CLONE_NEWPID) == 0, "unshare(NEWPID)");
    CHECK(getpid() == me, "l'appelant garde son pid (%d)", getpid());
    char l1[64] = {0}, l2[64] = {0};
    readlink("/proc/self/ns/pid", l1, sizeof l1 - 1);
    readlink("/proc/self/ns/pid_for_children", l2, sizeof l2 - 1);
    CHECK(strncmp(l2, "pid:[", 5) == 0 && strcmp(l1, l2) != 0, "pid_for_children distinct (%s / %s)", l1, l2);
    CHECK(unshare(CLONE_NEWPID) < 0 && errno == EINVAL, "second unshare(NEWPID) -> EINVAL");
    pipe(zp);
    pipe(sp);
    alarm(30);
    pid_t init = fork();
    if (init == 0) {
        close(zp[0]);
        int f = pidns_init();
        printf("nstest : (init de l'espace pid : %d ok, %d echec)\n", oks, f);
        _exit(f > 100 ? 100 : f);
    }
    close(zp[1]);
    CHECK(init > 1, "pid de l'init vu de l'exterieur (%d)", init);
    char path[64], buf[4096], want[64];
    snprintf(path, sizeof path, "/proc/%d/status", init);
    read_file(path, buf, sizeof buf);
    snprintf(want, sizeof want, "\nNSpid:\t%d\t1\n", init);
    CHECK(strstr(buf, want) != NULL, "NSpid vu de l'exterieur :\n%s", buf);
    write(sp[1], "x", 1);
    int st = 0;
    CHECK(waitpid(init, &st, 0) == init, "waitpid(init)");
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0, "init : %d echec(s) (statut %#x)", WEXITSTATUS(st), st);
    char c;
    CHECK(read(zp[0], &c, 1) == 0, "fin de l'init : le reste de l'espace est tue");
    errno = 0;
    pid_t f = fork();
    if (f == 0)
        _exit(0);
    CHECK(f < 0 && errno == ENOMEM, "fork dans l'espace dont l'init est mort -> ENOMEM (%d)", f);
    alarm(0);
    return 0;
}

/* clone(CLONE_NEWUSER | CLONE_NEWPID | CLONE_NEWNS) en une fois, comme bwrap */
static int clone_ns_child(void) {
    CHECK(drop_to_1000() == 0, "passage en uid 1000");
    long r = syscall(SYS_clone, CLONE_NEWUSER | CLONE_NEWPID | CLONE_NEWNS | SIGCHLD, 0, 0, 0, 0);
    if (r == 0)
        _exit(getpid() == 1 && getuid() == 65534 ? 0 : 1);
    int st = 0;
    CHECK(r > 0 && waitpid(r, &st, 0) == r && WIFEXITED(st) && WEXITSTATUS(st) == 0, "clone(NEWUSER|NEWPID|NEWNS) : pid 1, uid 65534 (%ld, %#x)", r, st);
    return 0;
}

/* --- 6. Subreaper, PDEATHSIG, pidfd ----------------------------------------------------------- */

static int reaper_child(void) {
    int v = -1;
    CHECK(prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) == 0 && prctl(PR_GET_CHILD_SUBREAPER, &v, 0, 0, 0) == 0 && v == 1, "PR_SET_CHILD_SUBREAPER");
    pid_t me = getpid();
    int rp[2];
    pipe(rp);
    pid_t a = fork();
    if (a == 0) {
        if (fork() == 0) {
            int s = -1;
            prctl(PR_SET_PDEATHSIG, SIGUSR1, 0, 0, 0);
            prctl(PR_GET_PDEATHSIG, &s, 0, 0, 0);
            if (s != SIGUSR1)
                _exit(3);
            write(rp[1], "x", 1);
            for (int i = 0; i < 400; i++)
                usleep(5000);
            _exit(4);
        }
        char c;
        read(rp[0], &c, 1);
        _exit(0);
    }
    int st = 0;
    waitpid(a, &st, 0);
    pid_t b = waitpid(-1, &st, 0);
    CHECK(b > 0 && WIFSIGNALED(st) && WTERMSIG(st) == SIGUSR1, "PDEATHSIG : tue par SIGUSR1 et recueilli par le subreaper (%d, %#x)", b, st);
    a = fork();
    if (a == 0) {
        if (fork() == 0) {
            for (int i = 0; i < 400 && getppid() != me; i++)
                usleep(5000);
            _exit(getppid() == me ? 42 : 43);
        }
        _exit(0);
    }
    waitpid(a, &st, 0);
    b = waitpid(-1, &st, 0);
    CHECK(b > 0 && WIFEXITED(st) && WEXITSTATUS(st) == 42, "orphelin recueilli par le subreaper (%d, %#x)", b, st);
    return 0;
}

struct clone_args_ {
    unsigned long long flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls;
};

static void test_pidfd(void) {
    int pidfd = -1;
    struct clone_args_ ca = {0};
    ca.flags = 0x1000; /* CLONE_PIDFD */
    ca.pidfd = (unsigned long long)(unsigned long)&pidfd;
    ca.exit_signal = SIGCHLD;
    long r = syscall(435 /* clone3 */, &ca, sizeof ca);
    if (r == 0)
        _exit(7);
    CHECK(r > 0 && pidfd >= 0, "clone3(CLONE_PIDFD) : %ld, pidfd %d", r, pidfd);
    siginfo_t si;
    memset(&si, 0, sizeof si);
    CHECK(waitid(3 /* P_PIDFD */, pidfd, &si, WEXITED) == 0 && si.si_pid == r && si.si_status == 7, "waitid(P_PIDFD) : pid %d, statut %d", si.si_pid, si.si_status);
    close(pidfd);
}

/* --- 7. Espace réseau, comme bwrap --unshare-net ------------------------------------------- */

/* Une requête rtnetlink acquittée ; rend l'erreur de l'accusé (0 ou -errno), -1000 si la
   réponse n'est pas pour nous (bwrap vérifie nlmsg_seq et nlmsg_pid = getpid()). */
static int rtnl_do(unsigned short type, unsigned short flags, const void *body, int blen) {
    int fd = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, 0 /* NETLINK_ROUTE */);
    if (fd < 0)
        return -2000;
    struct {
        unsigned short family, pad;
        unsigned pid, groups;
    } sa = {16, 0, (unsigned)getpid(), 0}, da = {16, 0, 0, 0};
    if (bind(fd, (struct sockaddr *)&sa, sizeof sa) < 0) {
        close(fd);
        return -3000;
    }
    unsigned char buf[256] = {0};
    unsigned len = 16 + blen;
    memcpy(buf, &len, 4);
    memcpy(buf + 4, &type, 2);
    flags |= 1 | 4; /* NLM_F_REQUEST | NLM_F_ACK */
    memcpy(buf + 6, &flags, 2);
    unsigned seq = 77;
    memcpy(buf + 8, &seq, 4);
    memcpy(buf + 16, body, blen);
    sendto(fd, buf, len, 0, (struct sockaddr *)&da, sizeof da);
    unsigned char r[512];
    int n = recv(fd, r, sizeof r, 0);
    close(fd);
    unsigned short rt;
    unsigned rseq, rpid;
    int err;
    memcpy(&rt, r + 4, 2);
    memcpy(&rseq, r + 8, 4);
    memcpy(&rpid, r + 12, 4);
    memcpy(&err, r + 16, 4);
    if (n < 20 || rt != 2 /* NLMSG_ERROR */ || rseq != seq || rpid != (unsigned)getpid())
        return -1000;
    return err;
}

/* `loopback_setup` de bubblewrap : RTM_NEWADDR 127.0.0.1/8 (si demandé), puis RTM_NEWLINK
   IFF_UP. Rend la première erreur. */
static int rtnl_lo_up(int with_addr) {
    if (with_addr) {
        unsigned char b[24] = {AF_INET, 8, 0x80 /* IFA_F_PERMANENT */, 254 /* RT_SCOPE_HOST */};
        unsigned idx = 1;
        memcpy(b + 4, &idx, 4);
        unsigned char at[8] = {8, 0, 2 /* IFA_LOCAL */, 0, 127, 0, 0, 1};
        memcpy(b + 8, at, 8);
        at[2] = 1; /* IFA_ADDRESS */
        memcpy(b + 16, at, 8);
        int e = rtnl_do(20 /* RTM_NEWADDR */, 0x400 | 0x200 /* CREATE | EXCL */, b, sizeof b);
        if (e)
            return e;
    }
    unsigned char l[16] = {0};
    int idx = 1;
    unsigned up = IFF_UP;
    memcpy(l + 4, &idx, 4);
    memcpy(l + 8, &up, 4);
    memcpy(l + 12, &up, 4);
    return rtnl_do(16 /* RTM_NEWLINK */, 0, l, sizeof l);
}

static unsigned lo_flags(void) {
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    struct ifreq r;
    memset(&r, 0, sizeof r);
    strcpy(r.ifr_name, "lo");
    int ok = ioctl(s, SIOCGIFFLAGS, &r);
    close(s);
    return ok < 0 ? 0xffffffff : (unsigned short)r.ifr_flags;
}

static struct sockaddr_in lo4(int port) {
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(port)};
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    return a;
}

static int tcp_listen(int port) {
    int s = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = lo4(port);
    if (bind(s, (struct sockaddr *)&a, sizeof a) < 0 || listen(s, 4) < 0) {
        int e = errno;
        close(s);
        errno = e;
        return -1;
    }
    return s;
}

static int tcp_connect(int s, int port) {
    struct sockaddr_in a = lo4(port);
    return connect(s, (struct sockaddr *)&a, sizeof a);
}

static socklen_t abs_addr(struct sockaddr_un *a, const char *name) {
    memset(a, 0, sizeof *a);
    a->sun_family = AF_UNIX;
    strcpy(a->sun_path + 1, name);
    return offsetof(struct sockaddr_un, sun_path) + 1 + strlen(name);
}

static int netns_child(void) {
    /* Avant : objets dans l'espace initial (écouteur TCP, nom abstrait, socket par chemin). */
    int old_lst = tcp_listen(47002);
    CHECK(old_lst >= 0, "ecouteur 127.0.0.1:47002 dans l'espace initial");
    int old_cli = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_un ua;
    socklen_t ul = abs_addr(&ua, "nstest-abs");
    int old_abs = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(bind(old_abs, (struct sockaddr *)&ua, ul) == 0 && listen(old_abs, 1) == 0, "nom abstrait dans l'espace initial");
    unlink("/tmp/nsnet.sock");
    struct sockaddr_un pa = {.sun_family = AF_UNIX};
    strcpy(pa.sun_path, "/tmp/nsnet.sock");
    int old_path = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(bind(old_path, (struct sockaddr *)&pa, sizeof pa) == 0 && listen(old_path, 1) == 0, "socket Unix par chemin dans l'espace initial");
    chmod("/tmp/nsnet.sock", 0777);
    char l1[64] = {0}, l2[64] = {0};
    readlink("/proc/self/ns/net", l1, sizeof l1 - 1);
    int oldnet = open("/proc/self/ns/net", O_RDONLY);

    CHECK(drop_to_1000() == 0, "passage en uid 1000");
    CHECK(unshare(CLONE_NEWUSER | CLONE_NEWNET) == 0 && map_root() == 0, "unshare(NEWUSER|NEWNET)");
    readlink("/proc/self/ns/net", l2, sizeof l2 - 1);
    CHECK(strncmp(l2, "net:[", 5) == 0 && strcmp(l1, l2) != 0, "nouvel espace reseau (%s -> %s)", l1, l2);

    /* lo seul, éteint et sans adresse */
    unsigned f = lo_flags();
    CHECK(f != 0xffffffff && (f & IFF_LOOPBACK) && !(f & IFF_UP), "lo eteint au depart (%#x)", f);
    CHECK(if_nametoindex("lo") == 1, "if_nametoindex(lo) = 1");
    CHECK(if_nametoindex("eth0") == 0, "pas d'eth0 dans l'espace");
    char buf[4096];
    read_file("/proc/net/dev", buf, sizeof buf);
    CHECK(strstr(buf, "lo:") && !strstr(buf, "eth"), "/proc/net/dev : lo seul\n%s", buf);
    int s = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = lo4(0);
    CHECK(bind(s, (struct sockaddr *)&a, sizeof a) < 0 && errno == EADDRNOTAVAIL, "bind(127.0.0.1) lo eteint -> EADDRNOTAVAIL");
    CHECK(tcp_connect(s, 47002) < 0 && errno == ENETUNREACH, "connect(127.0.0.1) lo eteint -> ENETUNREACH");
    close(s);
    s = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in ext = {.sin_family = AF_INET, .sin_port = htons(7)};
    ext.sin_addr.s_addr = inet_addr("10.0.2.2");
    CHECK(connect(s, (struct sockaddr *)&ext, sizeof ext) < 0 && errno == ENETUNREACH, "connect(10.0.2.2) -> ENETUNREACH");
    close(s);
    s = socket(AF_INET, SOCK_DGRAM, 0);
    CHECK(connect(s, (struct sockaddr *)&ext, sizeof ext) < 0 && errno == ENETUNREACH, "UDP connect(10.0.2.2) -> ENETUNREACH");
    CHECK(sendto(s, "x", 1, 0, (struct sockaddr *)&ext, sizeof ext) < 0 && errno == ENETUNREACH, "UDP sendto(10.0.2.2) -> ENETUNREACH");
    close(s);

    /* La séquence de bwrap */
    CHECK(rtnl_lo_up(1) == 0, "loopback_setup de bwrap (RTM_NEWADDR + RTM_NEWLINK)");
    f = lo_flags();
    CHECK((f & IFF_UP) && (f & IFF_RUNNING), "lo allume (%#x)", f);
    unsigned char b[24] = {AF_INET, 8, 0x80, 254, 1, 0, 0, 0, 8, 0, 2, 0, 127, 0, 0, 1, 8, 0, 1, 0, 127, 0, 0, 1};
    CHECK(rtnl_do(20, 0x600, b, sizeof b) == -EEXIST, "RTM_NEWADDR deja present -> EEXIST");
    read_file("/proc/net/if_inet6", buf, sizeof buf);
    CHECK(strstr(buf, "00000000000000000000000000000001") != NULL, "::1 ajoutee a l'allumage");

    /* Isolation : le port 47002 de l'espace initial n'existe pas ici, et il est libre. */
    s = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(tcp_connect(s, 47002) < 0 && errno == ECONNREFUSED, "ecouteur de l'espace initial invisible -> ECONNREFUSED");
    close(s);
    int lst = tcp_listen(47002);
    CHECK(lst >= 0, "meme port libre dans le nouvel espace");
    s = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(tcp_connect(s, 47002) == 0, "connexion locale dans l'espace");
    int c = accept(lst, NULL, NULL);
    char m[8] = {0};
    CHECK(c >= 0 && write(s, "ping", 4) == 4 && read(c, m, 4) == 4 && !strcmp(m, "ping"), "echange TCP par lo");
    close(c);
    close(s);
    close(lst);
    /* Une socket créée avant l'unshare reste dans l'espace initial (sock_net). */
    CHECK(tcp_connect(old_cli, 47002) == 0, "socket d'avant l'unshare : connexion dans l'espace initial");
    c = accept(old_lst, NULL, NULL);
    CHECK(c >= 0, "acceptee par l'ecouteur de l'espace initial");
    close(c);
    s = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in p80 = lo4(80);
    CHECK(bind(s, (struct sockaddr *)&p80, sizeof p80) == 0, "port 80 : root de l'espace a CAP_NET_BIND_SERVICE");
    close(s);

    /* Noms abstraits par espace ; chemins partagés */
    s = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(connect(s, (struct sockaddr *)&ua, ul) < 0 && errno == ECONNREFUSED, "nom abstrait de l'espace initial invisible");
    close(s);
    s = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(bind(s, (struct sockaddr *)&ua, ul) == 0, "meme nom abstrait libre dans l'espace");
    close(s);
    s = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(connect(s, (struct sockaddr *)&pa, sizeof pa) == 0, "socket Unix par chemin joignable d'un autre espace (X11, Wayland)");
    close(s);

    CHECK(setns(oldnet, CLONE_NEWNET) < 0 && errno == EPERM, "setns vers l'espace reseau initial -> EPERM");
    close(old_lst);
    close(old_cli);
    close(old_abs);
    close(old_path);
    return 0;
}

/* root : unshare(NEWNET) puis retour par setns */
static int netns_root_child(void) {
    int oldnet = open("/proc/self/ns/net", O_RDONLY);
    CHECK(unshare(CLONE_NEWNET) == 0, "root : unshare(NEWNET)");
    CHECK(!(lo_flags() & IFF_UP), "root : lo eteint dans le nouvel espace");
    CHECK(rtnl_lo_up(0) == 0 && (lo_flags() & IFF_UP), "root : RTM_NEWLINK IFF_UP seul (127.0.0.1 ajoutee)");
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in a = lo4(0);
    CHECK(bind(s, (struct sockaddr *)&a, sizeof a) == 0, "root : bind(127.0.0.1) apres allumage");
    close(s);
    CHECK(setns(oldnet, CLONE_NEWNET) == 0, "root : setns vers l'espace initial");
    char buf[2048];
    read_file("/proc/net/route", buf, sizeof buf);
    CHECK(lo_flags() & IFF_UP, "root : lo de l'espace initial");
    return 0;
}

/* --- 4. pivot_root, comme bubblewrap --------------------------------------------------------- */

static int pivot_child(void) {
    /* Dossier système lié en lecture seule (bwrap --ro-bind /usr /usr) : /usr de la racine ext4
       s'il y en a une, sinon /lib de la racine système. */
    const char *sd = access("/usr/bin", F_OK) == 0 ? "usr" : "lib";
    char p_old[64], p_new[64], p_abs[64], p_probe[64], p_ro[64], p_mi[64];
    snprintf(p_old, sizeof p_old, "/oldroot/%s", sd);
    snprintf(p_new, sizeof p_new, "/newroot/%s", sd);
    snprintf(p_abs, sizeof p_abs, "/%s", sd);
    snprintf(p_probe, sizeof p_probe, "/%s/%s", sd, sd[0] == 'u' ? "bin" : "x86_64-linux-gnu");
    snprintf(p_ro, sizeof p_ro, "/%s/nst", sd);
    snprintf(p_mi, sizeof p_mi, " /%s ro,nosuid,nodev", sd);
    CHECK(drop_to_1000() == 0, "passage en uid 1000");
    CHECK(unshare(CLONE_NEWUSER | CLONE_NEWNS) == 0 && map_root() == 0, "espace utilisateur + montage");
    CHECK(mount(NULL, "/", NULL, MS_SLAVE | MS_REC, NULL) == 0, "mount --make-rslave /");
    mkdir("/tmp/nsp", 0755);
    CHECK(mount("tmpfs", "/tmp/nsp", "tmpfs", MS_NODEV | MS_NOSUID, NULL) == 0, "tmpfs de base");
    CHECK(chdir("/tmp/nsp") == 0, "chdir base");
    mkdir("newroot", 0755);
    CHECK(mount("newroot", "newroot", NULL, MS_BIND | MS_REC, NULL) == 0, "newroot lie sur lui-meme");
    mkdir("oldroot", 0755);
    CHECK(syscall(SYS_pivot_root, "/tmp/nsp", "oldroot") == 0, "pivot_root(base, oldroot)");
    CHECK(chdir("/") == 0 && exists("/oldroot/tmp") && exists("/newroot"), "racine = base, ancienne en /oldroot");
    /* Contenu du conteneur, lié depuis l'ancienne racine */
    mkdir(p_new, 0755);
    mkdir("/newroot/proc", 0755);
    mkdir("/newroot/dev", 0755);
    CHECK(mount(p_old, p_new, NULL, MS_BIND | MS_REC, NULL) == 0, "--ro-bind %s : liaison", p_abs);
    CHECK(mount("none", p_new, NULL, MS_BIND | MS_REMOUNT | MS_RDONLY | MS_NOSUID | MS_NODEV, NULL) == 0, "--ro-bind %s : lecture seule", p_abs);
    CHECK(mount("/oldroot/proc", "/newroot/proc", NULL, MS_BIND | MS_REC, NULL) == 0, "liaison de /proc");
    CHECK(mount("tmpfs", "/newroot/dev", "tmpfs", MS_NOSUID, "mode=0755") == 0, "--dev : tmpfs");
    close(open("/newroot/dev/null", O_CREAT | O_WRONLY, 0666));
    CHECK(mount("/oldroot/dev/null", "/newroot/dev/null", NULL, MS_BIND, NULL) == 0, "--dev : liaison de /dev/null");
    CHECK(umount2("/oldroot", MNT_DETACH) == 0, "umount2(oldroot, MNT_DETACH)");
    CHECK(chdir("/newroot") == 0, "chdir newroot");
    CHECK(syscall(SYS_pivot_root, ".", ".") == 0, "pivot_root(\".\", \".\")");
    CHECK(umount2(".", MNT_DETACH) == 0, "umount2(\".\", MNT_DETACH)");
    CHECK(chdir("/") == 0, "chdir /");
    CHECK(exists(p_probe) && !exists("/oldroot") && !exists("/tmp/nsp"), "racine du conteneur");
    int fd = open("/dev/null", O_WRONLY);
    CHECK(fd >= 0 && write(fd, "x", 1) == 1, "/dev/null du conteneur");
    close(fd);
    CHECK(open(p_ro, O_CREAT | O_WRONLY, 0644) < 0 && errno == EROFS, "%s en lecture seule", p_abs);
    char cwd[256];
    CHECK(getcwd(cwd, sizeof cwd) && strcmp(cwd, "/") == 0, "getcwd = / (%s)", cwd);
    char buf[8192];
    CHECK(read_file("/proc/self/mountinfo", buf, sizeof buf) > 0 && strstr(buf, " / ") && strstr(buf, p_mi), "mountinfo du conteneur :\n%s", buf);
    CHECK(strstr(buf, "/oldroot") == NULL, "ancienne racine invisible");
    /* chroot dans le conteneur */
    mkdir("/dev/cj", 0755);
    CHECK(chroot("/dev") == 0 && chdir("/") == 0 && exists("/cj") && exists("/null"), "chroot /dev");
    /* Plus d'espace utilisateur depuis une racine enfermée */
    CHECK(unshare(CLONE_NEWUSER) < 0 && errno == EPERM, "unshare(NEWUSER) apres chroot -> EPERM");
    return 0;
}

/* openat2 (Linux 5.6) : bubblewrap >= 0.11 ouvre les sources de ses montages par
 * `RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS`, sans repli. */
struct o2_how {
    unsigned long long flags, mode, resolve;
};
#define O2_NO_XDEV 0x01
#define O2_NO_MAGICLINKS 0x02
#define O2_NO_SYMLINKS 0x04
#define O2_BENEATH 0x08
#define O2_IN_ROOT 0x10
#define O2_CACHED 0x20

static int o2(int dfd, const char *p, unsigned long long flags, unsigned long long mode, unsigned long long resolve) {
    struct o2_how h = {flags, mode, resolve};
    return syscall(437, dfd, p, &h, sizeof h);
}

/* Vrai si `fd` désigne le même inode que `path`. */
static int o2_same(int fd, const char *path) {
    struct stat a, b;
    int r = fd >= 0 && fstat(fd, &a) == 0 && stat(path, &b) == 0 && a.st_ino == b.st_ino && a.st_dev == b.st_dev;
    if (fd >= 0)
        close(fd);
    return r;
}

/* Vrai si l'appel a échoué avec `e` (et ferme un descripteur rendu par erreur). */
static int o2_err(int fd, int e) {
    if (fd >= 0) {
        close(fd);
        return 0;
    }
    return errno == e;
}

static void test_openat2(void) {
    system("rm -rf /tmp/o2");
    mkdir("/tmp/o2", 0755);
    mkdir("/tmp/o2/a", 0755);
    mkdir("/tmp/o2/a/b", 0755);
    close(open("/tmp/o2/a/b/f", O_CREAT | O_WRONLY, 0644));
    symlink("a/b", "/tmp/o2/rel");
    symlink("/a/b", "/tmp/o2/abs");
    symlink("../..", "/tmp/o2/up");
    int d = open("/tmp/o2", O_PATH | O_DIRECTORY | O_CLOEXEC);
    CHECK(d >= 0, "open(/tmp/o2, O_PATH)");

    /* Validation de `struct open_how` (build_open_flags). */
    struct {
        struct o2_how h;
        unsigned long long extra;
    } big = {{O_RDONLY, 0, 0}, 0};
    int fd = syscall(437, d, "a", &big, sizeof big);
    CHECK(fd >= 0, "openat2 : open_how de 32 octets, fin nulle");
    if (fd >= 0)
        close(fd);
    big.extra = 1;
    CHECK(o2_err(syscall(437, d, "a", &big, sizeof big), E2BIG), "openat2 : octets en plus non nuls -> E2BIG");
    CHECK(o2_err(syscall(437, d, "a", &big, 16), EINVAL), "openat2 : taille 16 -> EINVAL");
    CHECK(o2_err(o2(d, "a", 0x4, 0, 0), EINVAL), "openat2 : drapeau inconnu -> EINVAL");
    CHECK(o2_err(o2(d, "a", O_RDONLY, 0644, 0), EINVAL), "openat2 : mode sans O_CREAT -> EINVAL");
    CHECK(o2_err(o2(d, "a", O_PATH | O_RDWR, 0, 0), EINVAL), "openat2 : O_PATH | O_RDWR -> EINVAL");
    CHECK(o2_err(o2(d, "a", O_RDONLY, 0, 0x40), EINVAL), "openat2 : resolve inconnu -> EINVAL");
    CHECK(o2_err(o2(d, "a", O_RDONLY, 0, O2_BENEATH | O2_IN_ROOT), EINVAL), "openat2 : BENEATH | IN_ROOT -> EINVAL");
    CHECK(o2_err(o2(d, "a/n", O_CREAT | O_WRONLY, 0600, O2_CACHED), EAGAIN), "openat2 : CACHED | O_CREAT -> EAGAIN");
    CHECK(o2_err(o2(d, "", O_RDONLY, 0, 0), ENOENT), "openat2 : chemin vide -> ENOENT");

    /* RESOLVE_BENEATH : jamais hors du dossier de départ. */
    CHECK(o2_same(o2(d, "a/b/f", O_RDONLY, 0, O2_BENEATH), "/tmp/o2/a/b/f"), "BENEATH : a/b/f");
    CHECK(o2_same(o2(d, "rel/f", O_RDONLY, 0, O2_BENEATH), "/tmp/o2/a/b/f"), "BENEATH : lien relatif interne");
    CHECK(o2_same(o2(d, "a/b/../../a", O_PATH, 0, O2_BENEATH), "/tmp/o2/a"), "BENEATH : .. interne");
    CHECK(o2_err(o2(d, "../o2/a", O_PATH, 0, O2_BENEATH), EXDEV), "BENEATH : .. au depart -> EXDEV");
    CHECK(o2_err(o2(d, "/tmp/o2/a", O_PATH, 0, O2_BENEATH), EXDEV), "BENEATH : chemin absolu -> EXDEV");
    CHECK(o2_err(o2(d, "abs/f", O_PATH, 0, O2_BENEATH), EXDEV), "BENEATH : lien absolu -> EXDEV");
    CHECK(o2_err(o2(d, "up", O_PATH, 0, O2_BENEATH), EXDEV), "BENEATH : lien qui remonte -> EXDEV");

    /* RESOLVE_IN_ROOT : le dossier de départ tient lieu de racine. */
    CHECK(o2_same(o2(d, "/a/b/f", O_RDONLY, 0, O2_IN_ROOT), "/tmp/o2/a/b/f"), "IN_ROOT : /a/b/f");
    CHECK(o2_same(o2(d, "../../../a/b/f", O_RDONLY, 0, O2_IN_ROOT), "/tmp/o2/a/b/f"), "IN_ROOT : .. borne a la racine");
    CHECK(o2_same(o2(d, "abs/f", O_RDONLY, 0, O2_IN_ROOT), "/tmp/o2/a/b/f"), "IN_ROOT : lien absolu dans la racine");
    CHECK(o2_same(o2(d, "up/a", O_PATH, 0, O2_IN_ROOT), "/tmp/o2/a"), "IN_ROOT : lien qui remonte borne");
    CHECK(o2_err(o2(d, "abs/f", O_RDONLY, 0, 0), ENOENT), "sans portee : lien absolu depuis /");
    fd = o2(d, "/a/neuf", O_CREAT | O_WRONLY | O_EXCL, 0600, O2_IN_ROOT);
    CHECK(fd >= 0 && access("/tmp/o2/a/neuf", F_OK) == 0, "IN_ROOT : O_CREAT dans la racine");
    if (fd >= 0)
        close(fd);
    CHECK(o2_err(o2(d, "/a/neuf", O_CREAT | O_WRONLY | O_EXCL, 0600, O2_IN_ROOT), EEXIST), "IN_ROOT : O_EXCL -> EEXIST");

    /* RESOLVE_NO_SYMLINKS. */
    CHECK(o2_err(o2(d, "rel/f", O_RDONLY, 0, O2_NO_SYMLINKS), ELOOP), "NO_SYMLINKS : lien traverse -> ELOOP");
    CHECK(o2_err(o2(d, "rel", O_PATH, 0, O2_NO_SYMLINKS), ELOOP), "NO_SYMLINKS : lien final suivi -> ELOOP");
    fd = o2(d, "rel", O_PATH | O_NOFOLLOW, 0, O2_NO_SYMLINKS);
    CHECK(fd >= 0, "NO_SYMLINKS : lien final non suivi (O_PATH | O_NOFOLLOW)");
    if (fd >= 0)
        close(fd);
    CHECK(o2_err(o2(AT_FDCWD, "/proc/self/status", O_RDONLY, 0, O2_NO_SYMLINKS), ELOOP), "NO_SYMLINKS : /proc/self -> ELOOP");

    /* Liens magiques de /proc. */
    char p[64];
    snprintf(p, sizeof p, "/proc/self/fd/%d", d);
    CHECK(o2_err(o2(AT_FDCWD, p, O_PATH, 0, O2_NO_MAGICLINKS), ELOOP), "NO_MAGICLINKS : /proc/self/fd/N -> ELOOP");
    CHECK(o2_err(o2(AT_FDCWD, "/proc/self/cwd", O_PATH, 0, O2_NO_MAGICLINKS), ELOOP), "NO_MAGICLINKS : /proc/self/cwd -> ELOOP");
    fd = o2(AT_FDCWD, "/proc/self/status", O_RDONLY, 0, O2_NO_MAGICLINKS);
    CHECK(fd >= 0, "NO_MAGICLINKS : /proc/self/status permis");
    if (fd >= 0)
        close(fd);
    fd = o2(AT_FDCWD, p, O_PATH | O_NOFOLLOW, 0, O2_NO_MAGICLINKS);
    CHECK(fd >= 0, "NO_MAGICLINKS : lien magique final non suivi");
    if (fd >= 0)
        close(fd);
    CHECK(o2_same(o2(AT_FDCWD, p, O_PATH, 0, 0), "/tmp/o2"), "sans restriction : /proc/self/fd/N suivi");
    int pr = open("/proc", O_PATH | O_DIRECTORY);
    CHECK(o2_err(o2(pr, "self/cwd", O_PATH, 0, O2_IN_ROOT), EXDEV), "IN_ROOT : lien magique -> EXDEV");
    CHECK(o2_err(o2(pr, "self/root", O_PATH, 0, O2_BENEATH), EXDEV), "BENEATH : lien magique -> EXDEV");
    close(pr);

    /* RESOLVE_NO_XDEV. */
    CHECK(o2_same(o2(d, "a/b/f", O_RDONLY, 0, O2_NO_XDEV), "/tmp/o2/a/b/f"), "NO_XDEV : meme montage");
    CHECK(o2_err(o2(AT_FDCWD, "/proc/self/status", O_RDONLY, 0, O2_NO_XDEV), EXDEV), "NO_XDEV : / -> /proc -> EXDEV");
    pr = open("/proc/self", O_PATH | O_DIRECTORY);
    CHECK(o2_err(o2(pr, "../..", O_PATH, 0, O2_NO_XDEV), EXDEV), "NO_XDEV : .. hors du montage -> EXDEV");
    close(pr);

    close(d);
    system("rm -rf /tmp/o2");
}

/* devpts : chaque montage est une instance (Linux >= 4.7) — numéros, ptmx et options propres.
 * bubblewrap `--dev` en monte une (`newinstance,ptmxmode=0666,mode=620`). Lancé dans un espace
 * de montage privé. */
static int pts_child(void) {
    CHECK(unshare(CLONE_NEWNS) == 0 && mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL) == 0, "espace de montage prive");
    mkdir("/tmp/npt", 0755);
    CHECK(mount("tmpfs", "/tmp/npt", "tmpfs", 0, NULL) == 0, "tmpfs /tmp/npt");
    mkdir("/tmp/npt/pts", 0755);
    /* Un terminal de l'instance de l'hôte, ouvert avant. */
    int host = open("/dev/ptmx", O_RDWR | O_NOCTTY);
    CHECK(host >= 0, "ptmx de l'hote");
    CHECK(mount("devpts", "/tmp/npt/pts", "devpts", MS_NOSUID | MS_NOEXEC, "bogus=1") < 0 && errno == EINVAL, "devpts : option inconnue -> EINVAL");
    CHECK(mount("devpts", "/tmp/npt/pts", "devpts", MS_NOSUID | MS_NOEXEC, "newinstance,ptmxmode=0666,mode=620") == 0, "devpts : nouvelle instance");
    struct stat st;
    CHECK(stat("/tmp/npt/pts/ptmx", &st) == 0 && S_ISCHR(st.st_mode) && (st.st_mode & 0777) == 0666 && major(st.st_rdev) == 5 && minor(st.st_rdev) == 2,
          "instance : ptmx 5:2 en 0666 (%o)", st.st_mode);
    char mi[8192];
    read_file("/proc/self/mountinfo", mi, sizeof mi);
    CHECK(strstr(mi, "/tmp/npt/pts rw,nosuid,noexec") != NULL && strstr(mi, "devpts devpts rw,mode=620,ptmxmode=666") != NULL, "mountinfo : options de l'instance");
    /* O_PATH désigne sans ouvrir : aucun terminal créé ; /proc/self/fd/N garde le chemin. */
    int op = open("/tmp/npt/pts/ptmx", O_PATH | O_CLOEXEC);
    CHECK(op >= 0 && fstat(op, &st) == 0 && S_ISCHR(st.st_mode), "O_PATH de ptmx : designe sans ouvrir");
    char lk[64], fdp[32];
    snprintf(fdp, sizeof fdp, "/proc/self/fd/%d", op);
    ssize_t ln = readlink(fdp, lk, sizeof lk - 1);
    CHECK(ln == 17 && memcmp(lk, "/tmp/npt/pts/ptmx", 17) == 0, "O_PATH : /proc/self/fd/N -> chemin d'ouverture");
    close(op);
    /* Le ptmx de l'instance numérote à partir de 0, indépendamment de l'hôte. */
    symlink("pts/ptmx", "/tmp/npt/ptmx");
    ln = readlink("/tmp/npt/ptmx", lk, sizeof lk - 1);
    CHECK(ln == 8 && memcmp(lk, "pts/ptmx", 8) == 0, "readlink d'un lien vers devpts");
    symlink("/proc/self/fd", "/tmp/npt/fd");
    ln = readlink("/tmp/npt/fd", lk, sizeof lk - 1);
    CHECK(ln == 13 && memcmp(lk, "/proc/self/fd", 13) == 0, "readlink d'un lien vers /proc");
    int m = open("/tmp/npt/ptmx", O_RDWR | O_NOCTTY);
    CHECK(m >= 0, "ptmx de l'instance (lien pts/ptmx)");
    int n = -1, u = 0;
    CHECK(ioctl(m, TIOCGPTN, &n) == 0 && n == 0, "instance : premier terminal numero 0 (%d)", n);
    CHECK(ioctl(m, TIOCSPTLCK, &u) == 0, "unlockpt");
    DIR *d = opendir("/tmp/npt/pts");
    int seen0 = 0, seenptmx = 0, other = 0;
    struct dirent *e;
    while (d && (e = readdir(d))) {
        if (!strcmp(e->d_name, "0"))
            seen0 = 1;
        else if (!strcmp(e->d_name, "ptmx"))
            seenptmx = 1;
        else if (e->d_name[0] != '.')
            other = 1;
    }
    if (d)
        closedir(d);
    CHECK(seen0 && seenptmx && !other, "instance : /pts liste 0 et ptmx seulement");
    int s = open("/tmp/npt/pts/0", O_RDWR | O_NOCTTY);
    CHECK(s >= 0, "esclave 0 de l'instance");
    CHECK(fstat(s, &st) == 0 && (st.st_mode & 0777) == 0620, "esclave : mode=620 de l'instance (%o)", st.st_mode & 0777);
    /* L'esclave 0 de l'instance parle à son maître, pas à celui de l'hôte. */
    char c = 0;
    CHECK(write(s, "z", 1) == 1 && read(m, &c, 1) == 1 && c == 'z', "esclave -> maitre de l'instance");
    int hn = -1;
    ioctl(host, TIOCGPTN, &hn);
    char hp[32];
    snprintf(hp, sizeof hp, "/tmp/npt/pts/%d", hn);
    CHECK(hn == 0 || (stat(hp, &st) < 0 && errno == ENOENT), "terminal %d de l'hote absent de l'instance", hn);
    /* /dev/ptmx (5:2) : l'instance montée sur `pts` dans son dossier (devpts_acquire).
     * mknod demande CAP_MKNOD dans l'espace initial. */
    if (mknod("/tmp/npt/x", S_IFCHR | 0666, makedev(5, 2)) == 0) {
        int m2 = open("/tmp/npt/x", O_RDWR | O_NOCTTY);
        n = -1;
        CHECK(m2 >= 0 && ioctl(m2, TIOCGPTN, &n) == 0 && n == 1, "5:2 a cote de pts : instance montee la (%d)", n);
        if (m2 >= 0)
            close(m2);
    } else {
        printf("nstest : mknod 5:2 refuse (errno %d), verifications sautees\n", errno);
    }
    close(s);
    close(m);
    close(host);
    umount2("/tmp/npt/pts", MNT_DETACH);
    umount2("/tmp/npt", MNT_DETACH);
    rmdir("/tmp/npt");
    return 0;
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "capeff") == 0)
        return cap_eff() != 0;
    setvbuf(stdout, NULL, _IONBF, 0);
    printf("nstest : debut (pid %d)\n", getpid());
    test_root_caps();
    in_child(capexec_child);
    in_child(nnp_child);
    in_child(unpriv_child);
    in_child(userns_child);
    /* Les montages de l'enfant n'ont pas touché l'espace initial. */
    CHECK(!exists("/tmp/nst/x"), "tmpfs de l'enfant invisible du parent");
    struct utsname u;
    uname(&u);
    CHECK(strcmp(u.nodename, "nstest") != 0, "nom d'hote du parent inchange (%s)", u.nodename);
    CHECK(shmget(0x5a5a, 0, 0) < 0 && errno == ENOENT, "segment IPC de l'enfant invisible");
    in_child(pivot_child);
    in_child(pidns_child);
    in_child(clone_ns_child);
    in_child(reaper_child);
    test_pidfd();
    test_openat2();
    in_child(netns_child);
    in_child(netns_root_child);
    in_child(pts_child);
    unlink("/tmp/nsnet.sock");
    char buf[8192];
    read_file("/proc/self/mountinfo", buf, sizeof buf);
    CHECK(strstr(buf, " /proc rw,nosuid,nodev,noexec,relatime - proc proc rw") != NULL, "mountinfo initial : %s", buf);
    CHECK(strstr(buf, " /dev/pts ") != NULL && strstr(buf, " /dev ") != NULL, "/dev et /dev/pts montes");
    struct stat st;
    CHECK(stat("/dev/null", &st) == 0 && S_ISCHR(st.st_mode) && major(st.st_rdev) == 1 && minor(st.st_rdev) == 3, "/dev/null : peripherique 1:3");
    rmdir("/tmp/nst/a");
    rmdir("/tmp/nst/b");
    rmdir("/tmp/nst");
    rmdir("/tmp/nsp");
    printf("nstest : FIN %d ok, %d echec\n", oks, fails);
    return fails != 0;
}
