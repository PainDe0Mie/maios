#ifndef _UNISTD_H
#define _UNISTD_H

#include "stddef.h"
#include "sys/types.h"

typedef long ssize_t;
typedef long off_t;
typedef int  pid_t;

ssize_t read(int fd, void *buf, size_t count);
ssize_t write(int fd, const void *buf, size_t count);
int     close(int fd);
off_t   lseek(int fd, off_t offset, int whence);

pid_t   getpid(void);
pid_t   getppid(void);
unsigned int getuid(void);
unsigned int geteuid(void);
unsigned int getgid(void);
unsigned int getegid(void);

char   *getcwd(char *buf, size_t size);
int     chdir(const char *path);
int     isatty(int fd);
long    sysconf(int name);

unsigned int sleep(unsigned int seconds);
int     usleep(unsigned int usec);
void    _exit(int status) __attribute__((noreturn));

#define _SC_PAGESIZE         30
#define _SC_CLK_TCK          2
#define _SC_NPROCESSORS_ONLN 84

#define STDIN_FILENO  0
#define STDOUT_FILENO 1
#define STDERR_FILENO 2

#endif /* _UNISTD_H */
