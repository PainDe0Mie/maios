# ─────────────────────────────────────────────────────────────────────────────
# MaiOS Doom port — build doomgeneric for MaiOS
# ─────────────────────────────────────────────────────────────────────────────
#
# Prerequisites:
#   1. Clone doomgeneric:  git clone https://github.com/ozkl/doomgeneric
#   2. Build tlibc:        cd ../../tlibc && make
#   3. Provide DOOM1.WAD in this directory (shareware version is free)
#
# Build:  make
# Result: doom.elf (load in MaiOS with: loadc /path/to/doom.elf)
# ─────────────────────────────────────────────────────────────────────────────

CC        = x86_64-elf-gcc
CFLAGS    = -nostdlib -nostartfiles -nostdinc -mno-red-zone -mcmodel=large \
            -ffunction-sections -fdata-sections -fno-pie -fno-stack-protector \
            -I../../tlibc/include \
            -Idoomgeneric/doomgeneric \
            -DNORMALUNIX -DLINUX \
            -O2 -Wall -Wno-unused-parameter -Wno-pointer-sign

LDFLAGS   = -nostdlib -static -Wl,--emit-relocs -Wl,--gc-sections

TLIBC     = ../../tlibc/target/x86_64-unknown-theseus/release/libtlibc.a

# doomgeneric source files (from the doomgeneric repo)
DOOM_DIR  = doomgeneric/doomgeneric
DOOM_SRCS = $(filter-out $(DOOM_DIR)/doomgeneric_sdl.c \
                         $(DOOM_DIR)/doomgeneric_xlib.c \
                         $(DOOM_DIR)/doomgeneric_win.c \
                         $(DOOM_DIR)/doomgeneric_sosb.c \
                         $(DOOM_DIR)/i_sound.c, \
              $(wildcard $(DOOM_DIR)/*.c))

# Our MaiOS platform layer
MAIOS_SRCS = doomgeneric_maios.c

ALL_SRCS  = $(DOOM_SRCS) $(MAIOS_SRCS)
ALL_OBJS  = $(ALL_SRCS:.c=.o)

.PHONY: all clean

all: doom.elf

doom.elf: $(ALL_OBJS) $(TLIBC)
	$(CC) $(LDFLAGS) -o $@ $(ALL_OBJS) $(TLIBC)
	@echo "=== doom.elf built successfully ==="
	@echo "Copy doom.elf + DOOM1.WAD into the MaiOS disk image,"
	@echo "then run: loadc /path/to/doom.elf"

%.o: %.c
	$(CC) $(CFLAGS) -c -o $@ $<

clean:
	rm -f $(ALL_OBJS) doom.elf
