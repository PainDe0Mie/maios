#ifndef _TIME_H
#define _TIME_H

#include "stddef.h"

typedef long time_t;
typedef int  clockid_t;

#define CLOCK_REALTIME  0
#define CLOCK_MONOTONIC 1
#define CLOCKS_PER_SEC  1000000L

struct timespec {
    time_t tv_sec;
    long   tv_nsec;
};

struct timeval {
    time_t tv_sec;
    long   tv_usec;
};

struct tm {
    int tm_sec;
    int tm_min;
    int tm_hour;
    int tm_mday;
    int tm_mon;
    int tm_year;
    int tm_wday;
    int tm_yday;
    int tm_isdst;
};

int       clock_gettime(clockid_t clock_id, struct timespec *tp);
int       gettimeofday(struct timeval *tv, void *tz);
time_t    time(time_t *tloc);
int       nanosleep(const struct timespec *req, struct timespec *rem);
long      clock(void);
struct tm *localtime(const time_t *timer);
struct tm *gmtime(const time_t *timer);
double    difftime(time_t t1, time_t t0);
time_t    mktime(struct tm *t);

#endif /* _TIME_H */
