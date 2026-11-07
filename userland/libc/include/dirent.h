#ifndef _DIRENT_H
#define _DIRENT_H

#define DT_UNKNOWN 0
#define DT_REG     8
#define DT_DIR     4

struct dirent {
    unsigned long long d_ino;
    long long          d_off;
    unsigned short     d_reclen;
    unsigned char      d_type;
    char               d_name[256];
};

typedef struct DIR DIR;

DIR           *opendir(const char *name);
struct dirent *readdir(DIR *dirp);
int            closedir(DIR *dirp);
void           rewinddir(DIR *dirp);

#endif /* _DIRENT_H */
