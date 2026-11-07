#ifndef _BITS_STDIO_H
#define _BITS_STDIO_H

#include <stddef.h>
#include <stdarg.h>

// XXX: this is only here because cbindgen can't handle string constants
#define P_tmpdir "/tmp"

typedef struct FILE FILE;

#define SEEK_SET 0
#define SEEK_CUR 1
#define SEEK_END 2

// A typedef doesn't suffice, because libgmp uses this definition to check if
// STDIO was loaded.
#define FILE FILE

#ifdef __cplusplus
extern "C" {
#endif

int asprintf(char **strp, const char * fmt, ...);
int fprintf(FILE * stream, const char * fmt, ...);
int vfprintf(FILE *stream, const char *fmt, va_list args);
int vsnprintf(char *buffer, size_t size, const char *format, va_list args);
extern FILE *stderr;
extern FILE *stdout;
FILE *fopen(const char *filename, const char *mode);
int fclose(FILE *stream);
size_t fread(void *ptr, size_t size, size_t count, FILE *stream);
size_t fwrite(const void *ptr, size_t size, size_t count, FILE *stream);
int fseek(FILE *stream, long long offset, int whence);
long long ftell(FILE *stream);
void rewind(FILE *stream);
int fgetc(FILE *stream);
int fputc(int c, FILE *stream);
char *fgets(char *buf, int size, FILE *stream);
int fputs(const char *str, FILE *stream);
int fflush(FILE *stream);
int puts(const char *str);
int putchar(int c);
int sscanf(const char *input, const char *format, ...);
int remove(const char *filename);
int rename(const char *old_name, const char *new_name);
int printf(const char * fmt, ...);
int snprintf(char *s, size_t n, const char * fmt, ...);
int sprintf(char *s, const char * fmt, ...);
// int fscanf(FILE * stream, const char * fmt, ...);
// int scanf(const char * fmt, ...);
// int sscanf(const char * input, const char * fmt, ...);

#ifdef __cplusplus
} // extern "C"
#endif

#endif /* _BITS_STDIO_H */
