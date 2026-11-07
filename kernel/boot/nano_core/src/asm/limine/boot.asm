; Entrée de MaiOS depuis Limine (protocole Limine natif, BIOS et UEFI).
;
; Limine nous donne la main en mode long, noyau projeté à ses adresses hautes, interruptions
; coupées, sur une pile à lui et avec sa GDT (révision 6 du protocole : CR0/CR4/EFER réduits au
; strict nécessaire). On passe sur la pile du noyau (section .stack, même disposition que le
; démarrage BIOS : page de garde, pile, pile de double faute), on charge notre GDT, on active
; ce dont le noyau a besoin (SSE, AVX si compilé pour, NX, syscall, pages globales), puis on
; appelle `limine_entry` (Rust, nano_core/src/limine.rs).

%include "defines.asm"

bits 64

section .init.text progbits alloc exec nowrite
global _start
extern limine_entry

_start:
	cli
	cld
	mov rsp, initial_bsp_stack_top

	lgdt [rel GDT.ptr]
	push GDT.code
	lea rax, [rel .reload_cs]
	push rax
	retfq
.reload_cs:
	mov ax, GDT.data
	mov ss, ax
	mov ds, ax
	mov es, ax
	mov fs, ax
	mov gs, ax

	; CR0 : coprocesseur surveillé (MP), pas émulé (EM), écriture protégée (WP).
	mov rax, cr0
	and rax, ~(1 << 2)
	or rax, (1 << 1) | (1 << 16)
	mov cr0, rax
	; CR4 : pages globales (PGE), FXSAVE/SSE (OSFXSR), exceptions SIMD (OSXMMEXCPT).
	mov rax, cr4
	or rax, (1 << 7) | (1 << 9) | (1 << 10)
	mov cr4, rax

%ifdef ENABLE_AVX
	mov eax, 1
	cpuid
	test ecx, 1 << 26          ; XSAVE ?
	jz .no_avx
	test ecx, 1 << 28          ; AVX ?
	jz .no_avx
	mov rax, cr4
	or rax, 1 << 18            ; OSXSAVE
	mov cr4, rax
	xor ecx, ecx
	xgetbv
	or eax, 0b111              ; x87, SSE, AVX
	xor ecx, ecx
	xsetbv
	jmp .avx_done
.no_avx:
	jmp .halt                  ; noyau compilé pour AVX sur un processeur sans AVX
.avx_done:
%endif ; ENABLE_AVX

	; EFER : pages non exécutables (NXE) et instructions syscall/sysret (SCE).
	mov ecx, 0xC0000080
	rdmsr
	or eax, (1 << 11) | (1 << 0)
	wrmsr

	; TSC_AUX invalide jusqu'à l'initialisation du cœur (rdtscp sert d'identifiant de cœur) ;
	; bases FS/GS nulles.
	mov ecx, 0xC0000103
	mov eax, 0xFFFFFFFF
	xor edx, edx
	wrmsr
	xor eax, eax
	mov ecx, 0xC0000100
	wrmsr
	mov ecx, 0xC0000101
	wrmsr
	mov ecx, 0xC0000102
	wrmsr

	mov rdi, initial_double_fault_stack_top
	mov rax, limine_entry
	call rax

.halt:
	cli
	hlt
	jmp .halt


; En lecture seule une fois nos tables de pages en place : bit « accessed » (40) déjà posé, pour
; que le processeur n'ait jamais à l'écrire en chargeant un segment.
section .rodata
GDT:
	dq 0 ; entrée nulle
.code equ $ - GDT
	dq (1<<40) | (1<<41) | (1<<43) | (1<<44) | (1<<47) | (1<<53) ; code 64 bits
.data equ $ - GDT
	dq (1<<40) | (1<<41) | (1<<44) | (1<<47) ; données
.end equ $
.ptr:
	dw .end - GDT - 1
	dq GDT


; Pile initiale du BSP, même disposition que le démarrage BIOS : page de garde (non projetée
; par `paging::init`), pile, puis une page de pile de double faute.
section .stack nobits alloc noexec write
align 4096
global initial_bsp_stack_guard_page
initial_bsp_stack_guard_page:
	resb 4096
global initial_bsp_stack_bottom
initial_bsp_stack_bottom:
	resb 4096 * INITIAL_STACK_SIZE
global initial_bsp_stack_top
initial_bsp_stack_top:
	resb 4096
global initial_double_fault_stack_top
initial_double_fault_stack_top:
