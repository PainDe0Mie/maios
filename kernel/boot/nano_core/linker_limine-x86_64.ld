/*
 * Image du noyau MaiOS pour Limine (protocole Limine natif).
 *
 * Tout est dans la moitié haute (Limine l'exige d'un noyau non relogeable) : contrairement au
 * démarrage Multiboot2, il n'y a plus de code 32 bits ni d'en-tête à 1 Mio physique. On garde
 * la même disposition qu'avant, décalée de KERNEL_OFFSET, pour que le reste du noyau (sections,
 * pile initiale, projection des sections par `paging::init`) ne change pas.
 */
ENTRY(_start)
OUTPUT_FORMAT(elf64-x86-64)

KERNEL_OFFSET = 0xFFFFFFFF80000000;

SECTIONS {
	/* Chaque section commence sur une page : MaiOS projette chacune dans ses propres pages. */
	. = KERNEL_OFFSET + 1M;

	.init ALIGN(4K) : AT(ADDR(.init) - KERNEL_OFFSET)
	{
		*(.init .init.*)
	}

	.text ALIGN(4K) : AT(ADDR(.text) - KERNEL_OFFSET)
	{
		KEEP(*(.pre_init_array))
		*(.text .text.*)
	}

	.rodata ALIGN(4K) : AT(ADDR(.rodata) - KERNEL_OFFSET)
	{
		*(.rodata .rodata.*)
	}

	.eh_frame ALIGN(4K) : AT(ADDR(.eh_frame) - KERNEL_OFFSET)
	{
		*(.eh_frame)
	}

	.gcc_except_table ALIGN(4K) : AT(ADDR(.gcc_except_table) - KERNEL_OFFSET)
	{
		*(.gcc_except_table .gcc_except_table.*)
	}

	.cls ALIGN(4K) : AT(ADDR(.cls) - KERNEL_OFFSET)
	{
		*(.cls)
	}

	.tdata ALIGN(4K) : AT(ADDR(.tdata) - KERNEL_OFFSET)
	{
		*(.tdata .tdata.*)
	}

	.tbss : AT(ADDR(.tbss) - KERNEL_OFFSET)
	{
		*(.tbss .tbss.*)
	}

	.data ALIGN(4K) : AT(ADDR(.data) - KERNEL_OFFSET)
	{
		/* Requêtes Limine, entre leurs marqueurs (honorés depuis la révision 2). */
		KEEP(*(.limine_requests_start))
		KEEP(*(.limine_requests))
		KEEP(*(.limine_requests_end))
		*(.padata)
		*(.data .data.* )
	}

	.bss ALIGN(4K) : AT(ADDR(.bss) - KERNEL_OFFSET)
	{
		*(.bss .bss.*)
	}

	/* Copie des informations de démarrage, seule dans ses pages : projetée puis rendue à part. */
	.boot_info ALIGN(4K) : AT(ADDR(.boot_info) - KERNEL_OFFSET)
	{
		KEEP(*(.boot_info))
		. = ALIGN(4K);
	}

	/* Page de garde de 2 Mio avant la pile, comme au démarrage BIOS. */
	. = ALIGN(2M);
	. = . + 2M;
	.stack ALIGN(4K) : AT(ADDR(.stack) - KERNEL_OFFSET)
	{
		*(.stack)
	}

	/* .got (Global Offset Table) section and its relocation types are currently unsupported */
	.got (NOLOAD) :
	{
		KEEP(*(.got .got.*))
	}

	.data.rel.ro ALIGN(4K) : AT(ADDR(.data.rel.ro) - KERNEL_OFFSET)
	{
		*(.data.rel.ro.local*) *(.data.rel.ro .data.rel.ro.*)
	}
}

__THESEUS_CLS_SIZE = SIZEOF(.cls);
__THESEUS_TLS_SIZE = SIZEOF(.tdata) + SIZEOF(.tbss);
