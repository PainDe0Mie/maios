#ifndef _SIGNAL_H
#define _SIGNAL_H

typedef void (*sighandler_t)(int);
typedef unsigned long long sigset_t;

#define SIG_DFL ((sighandler_t)0)
#define SIG_IGN ((sighandler_t)1)

#define SIGABRT  6
#define SIGFPE   8
#define SIGILL   4
#define SIGINT   2
#define SIGSEGV  11
#define SIGTERM  15
#define SIGPIPE  13
#define SIGCHLD  17
#define SIGUSR1  10
#define SIGUSR2  12

struct sigaction {
    sighandler_t sa_handler;
    int          sa_flags;
    sigset_t     sa_mask;
};

sighandler_t signal(int sig, sighandler_t handler);
int raise(int sig);
int sigaction(int sig, const struct sigaction *act, struct sigaction *oldact);
int sigprocmask(int how, const sigset_t *set, sigset_t *oldset);
int sigemptyset(sigset_t *set);
int sigfillset(sigset_t *set);
int sigaddset(sigset_t *set, int signo);
int sigdelset(sigset_t *set, int signo);

#endif /* _SIGNAL_H */
