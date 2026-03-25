#ifndef _SETJMP_H
#define _SETJMP_H

typedef struct {
    unsigned long long regs[8];
} __attribute__((aligned(16))) jmp_buf[1];

int  setjmp(jmp_buf env);
void longjmp(jmp_buf env, int val) __attribute__((noreturn));

#endif /* _SETJMP_H */
