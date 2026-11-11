#!/usr/bin/env python3
"""Vecteurs de test de `mai_icons` : flux zlib (toutes stratégies) et PNG de tous types,
profondeurs, filtres et entrelacements, avec leurs pixels attendus (RVBA prémultipliés).

    python3 kernel/gfx/mai_icons/tests/mkvectors.py /tmp/iconvec
    MAI_ICONS_VECTORS=/tmp/iconvec cargo test     # dans une copie hors du workspace

Encodeur indépendant du décodeur testé : filtres choisis au hasard par ligne, Adam7 écrit
d'après la spécification."""
import os
import random
import struct
import sys
import zlib

out = sys.argv[1] if len(sys.argv) > 1 else "/tmp/iconvec"
os.makedirs(out, exist_ok=True)
rng = random.Random(1234)

# --- zlib -----------------------------------------------------------------------------------
def payloads():
    yield b""
    yield b"a"
    yield bytes(rng.randrange(256) for _ in range(5000))
    yield b"abcabcabcabc" * 3000
    yield bytes(rng.choice(b"ab ") for _ in range(70000))
    yield bytes(range(256)) * 300

n = 0
for data in payloads():
    for level in (0, 1, 6, 9):
        for strat in (zlib.Z_DEFAULT_STRATEGY, zlib.Z_FIXED, zlib.Z_HUFFMAN_ONLY, zlib.Z_RLE, zlib.Z_FILTERED):
            c = zlib.compressobj(level, zlib.DEFLATED, 15, 9, strat)
            z = c.compress(data) + c.flush()
            open(f"{out}/z{n:03}.zlib", "wb").write(z)
            open(f"{out}/z{n:03}.raw", "wb").write(data)
            n += 1

# --- PNG ------------------------------------------------------------------------------------
def chunk(kind, body):
    return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body) & 0xFFFFFFFF)

def paeth(a, b, c):
    p = a + b - c
    pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
    if pa <= pb and pa <= pc:
        return a
    return b if pb <= pc else c

def filt(rows, bpp):
    """Filtre chaque ligne (octets bruts) avec un filtre au hasard."""
    res = b""
    prev = bytes(len(rows[0])) if rows else b""
    for row in rows:
        f = rng.randrange(5)
        o = bytearray([f])
        for i, x in enumerate(row):
            a = row[i - bpp] if i >= bpp else 0
            b = prev[i]
            c = prev[i - bpp] if i >= bpp else 0
            pred = [0, a, b, (a + b) // 2, paeth(a, b, c)][f]
            o.append((x - pred) & 255)
        res += bytes(o)
        prev = row
    return res

def pack_row(samples, depth):
    if depth == 8:
        return bytes(samples)
    if depth == 16:
        return b"".join(struct.pack(">H", s) for s in samples)
    bits = ""
    for s in samples:
        bits += format(s, f"0{depth}b")
    bits += "0" * (-len(bits) % 8)
    return bytes(int(bits[i:i + 8], 2) for i in range(0, len(bits), 8))

CH = {0: 1, 2: 3, 3: 1, 4: 2, 6: 4}
ADAM7 = [(0, 0, 8, 8), (4, 0, 8, 8), (0, 4, 4, 8), (2, 0, 4, 4), (0, 2, 2, 4), (1, 0, 2, 2), (0, 1, 1, 2)]

def to8(v, d):
    return v >> 8 if d == 16 else (v if d == 8 else v * 255 // ((1 << d) - 1))

def premul(p):
    a = p[3]
    return bytes([(p[0] * a + 127) // 255, (p[1] * a + 127) // 255, (p[2] * a + 127) // 255, a])

k = 0
for color, depths in ((0, (1, 2, 4, 8, 16)), (2, (8, 16)), (3, (1, 2, 4, 8)), (4, (8, 16)), (6, (8, 16))):
    for depth in depths:
        for interlace in (0, 1):
            for (w, h) in ((1, 1), (3, 5), (13, 9), (33, 17)):
                ch = CH[color]
                maxv = (1 << depth) - 1
                pal = []
                trns = None
                if color == 3:
                    npal = min(1 << depth, rng.randrange(1, 257))
                    pal = [(rng.randrange(256), rng.randrange(256), rng.randrange(256)) for _ in range(npal)]
                    alphas = [rng.randrange(256) for _ in range(rng.randrange(0, npal + 1))]
                    img = [[rng.randrange(npal)] for _ in range(w * h)]
                else:
                    img = [[rng.randrange(maxv + 1) for _ in range(ch)] for _ in range(w * h)]
                key = None
                if color in (0, 2) and rng.random() < 0.6:
                    key = list(img[rng.randrange(w * h)])
                # Pixels attendus.
                exp = b""
                for px in img:
                    if color == 0:
                        g = to8(px[0], depth)
                        a = 0 if key is not None and px == key else 255
                        exp += premul((g, g, g, a))
                    elif color == 2:
                        a = 0 if key is not None and px == key else 255
                        exp += premul((to8(px[0], depth), to8(px[1], depth), to8(px[2], depth), a))
                    elif color == 3:
                        r, g, b = pal[px[0]]
                        a = alphas[px[0]] if px[0] < len(alphas) else 255
                        exp += premul((r, g, b, a))
                    elif color == 4:
                        g = to8(px[0], depth)
                        exp += premul((g, g, g, to8(px[1], depth)))
                    else:
                        exp += premul(tuple(to8(v, depth) for v in px))
                bpp = max(1, ch * depth // 8)
                raw = b""
                passes = ADAM7 if interlace else [(0, 0, 1, 1)]
                for (x0, y0, dx, dy) in passes:
                    rows = []
                    for y in range(y0, h, dy):
                        samples = []
                        for x in range(x0, w, dx):
                            samples += img[y * w + x]
                        if samples:
                            rows.append(pack_row(samples, depth))
                    if rows and rows[0]:
                        raw += filt(rows, bpp)
                png = b"\x89PNG\r\n\x1a\n"
                png += chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, depth, color, 0, 0, interlace))
                png += chunk(b"tEXt", b"Comment\x00test")
                if color == 3:
                    png += chunk(b"PLTE", b"".join(bytes(p) for p in pal))
                    if alphas:
                        png += chunk(b"tRNS", bytes(alphas))
                if key is not None:
                    png += chunk(b"tRNS", b"".join(struct.pack(">H", v) for v in key))
                z = zlib.compress(raw, rng.choice((0, 1, 6, 9)))
                # IDAT coupé en plusieurs segments.
                cut = rng.randrange(len(z) + 1)
                png += chunk(b"IDAT", z[:cut]) + chunk(b"IDAT", z[cut:])
                png += chunk(b"IEND", b"")
                open(f"{out}/p{k:03}.png", "wb").write(png)
                open(f"{out}/p{k:03}.rgba", "wb").write(struct.pack("<II", w, h) + exp)
                k += 1
print(f"{n} flux zlib, {k} PNG dans {out}")
