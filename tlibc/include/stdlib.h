#ifndef _STDLIB_H
#define _STDLIB_H

#include "stddef.h"

#define EXIT_SUCCESS 0
#define EXIT_FAILURE 1
#define RAND_MAX     0x7FFFFFFF

typedef struct { int quot; int rem; } div_t;

/* Memory allocation */
void *malloc(size_t size);
void *calloc(size_t nelem, size_t elsize);
void *realloc(void *ptr, size_t size);
void  free(void *ptr);
void *aligned_alloc(size_t alignment, size_t size);
int   posix_memalign(void **memptr, size_t alignment, size_t size);

/* Process control */
void   abort(void) __attribute__((noreturn));
void   exit(int status) __attribute__((noreturn));
void   _exit(int status) __attribute__((noreturn));
int    atexit(void (*func)(void));

/* String conversions */
int    atoi(const char *s);
long   atol(const char *s);
double atof(const char *s);
long   strtol(const char *nptr, char **endptr, int base);
unsigned long strtoul(const char *nptr, char **endptr, int base);
double strtod(const char *nptr, char **endptr);
float  strtof(const char *nptr, char **endptr);
long long strtoll(const char *nptr, char **endptr, int base);
unsigned long long strtoull(const char *nptr, char **endptr, int base);

/* Pseudo-random */
int  rand(void);
void srand(unsigned int seed);

/* Sorting & searching */
void  qsort(void *base, size_t nmemb, size_t size,
             int (*compar)(const void *, const void *));
void *bsearch(const void *key, const void *base, size_t nmemb, size_t size,
              int (*compar)(const void *, const void *));

/* Environment */
char *getenv(const char *name);
int   setenv(const char *name, const char *value, int overwrite);
int   unsetenv(const char *name);

/* Math helpers */
int  abs(int n);
long labs(long n);
div_t div(int numer, int denom);

#endif /* _STDLIB_H */
