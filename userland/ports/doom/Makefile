# ─────────────────────────────────────────────────────────────────────────────
# MaiOS Doom port — build doomgeneric for MaiOS
# ─────────────────────────────────────────────────────────────────────────────
#
# Prerequisites:
#   1. Clone doomgeneric:  git clone https://github.com/ozkl/doomgeneric
#   2. Build tlibc:        cd ../../libc && make
#   3. Provide a valid IWAD/PWAD through WAD_SOURCE (defaults to the repository root)
#
# Build:  make
# Result: doom.elf (load in MaiOS with: loadc /path/to/doom.elf)
# ─────────────────────────────────────────────────────────────────────────────

# Use x86_64-elf-gcc if available, otherwise fall back to system gcc
CC        = $(shell which x86_64-elf-gcc 2>/dev/null || echo gcc)
CFLAGS    = -nostdlib -nostartfiles -nostdinc -mno-red-zone -mcmodel=large \
            -ffunction-sections -fdata-sections -fno-pie -fno-stack-protector \
            -I../../libc/include \
            -Idoomgeneric/doomgeneric \
            -include stdio.h \
            -DNORMALUNIX -DLINUX \
            -O2 -Wall -Wno-unused-parameter -Wno-pointer-sign

# MaiProc : la moitié basse de l'espace virtuel appartient aux processus utilisateur. Doom, chargé
# dans l'espace du noyau par `loadc`, est donc lié dans la moitié haute (entrée P4 287, en haut
# de la zone des allocations dynamiques du noyau, partagée par tous les espaces d'adresses).
LINK_BASE = 0xffff8f8000000000
LDFLAGS   = -nostdlib -static -Wl,--emit-relocs -Wl,--gc-sections \
            -Wl,-Ttext-segment=$(LINK_BASE) \
            -Wl,--defsym=__THESEUS_CLS_SIZE=0 \
            -Wl,--defsym=__THESEUS_TLS_SIZE=0

TLIBC     = ../../libc/target/x86_64-unknown-theseus/release/libtlibc.a
WAD_SOURCE ?= ../../../assets/DOOM1.WAD

# doomgeneric source files (from the doomgeneric repo)
DOOM_DIR  = doomgeneric/doomgeneric
# i_sound.c and i_joystick.c are kept: without FEATURE_SOUND / ORIGCODE they
# compile to SDL-free stubs that the engine (s_sound.c, m_config.c...) needs.
DOOM_SRCS = $(filter-out $(DOOM_DIR)/doomgeneric_%.c \
                         $(DOOM_DIR)/i_allegro%.c \
                         $(DOOM_DIR)/i_sdl%.c \
                         $(DOOM_DIR)/i_cdmus.c, \
              $(wildcard $(DOOM_DIR)/*.c))

# Our MaiOS platform layer
MAIOS_SRCS = doomgeneric_maios.c

ALL_SRCS  = $(DOOM_SRCS) $(MAIOS_SRCS)
ALL_OBJS  = $(ALL_SRCS:.c=.o)

.PHONY: all clean wad-check

all: wad-check doom.elf

wad-check: ; @test -f "$(WAD_SOURCE)" || { echo "Error: WAD introuvable: $(WAD_SOURCE)"; exit 1; }; magic=$$(head -c 4 "$(WAD_SOURCE)"); test "$$magic" = "IWAD" -o "$$magic" = "PWAD" || { echo "Error: WAD invalide (signature IWAD/PWAD absente): $(WAD_SOURCE)"; exit 1; }; echo "=== WAD valide : $(WAD_SOURCE) ==="

doom.elf: $(ALL_OBJS) $(TLIBC)
	$(CC) $(LDFLAGS) -o $@ $(ALL_OBJS) $(TLIBC)
	@echo "=== doom.elf built successfully ==="
	@echo "Copy doom.elf + DOOM1.WAD into the MaiOS disk image,"
	@echo "then run: loadc /path/to/doom.elf"

$(TLIBC): ; (cd ../../libc && sh build.sh)

%.o: %.c
	$(CC) $(CFLAGS) -c -o $@ $<

clean:
	rm -f $(ALL_OBJS) doom.elf
