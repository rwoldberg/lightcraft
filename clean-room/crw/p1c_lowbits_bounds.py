"""P1c — verify the low-bits region bounds via non-stuffed ff statistics.

On EOS bodies the entropy region contains no ff followed by non-00 at all (perfect
JPEG-style stuffing); on the 12-bit PowerShots (G5/G6) a region of unstuffed uniform
bytes exists. This script shows that every non-stuffed ff falls inside
[514, 514 + wh/4) of the RawData payload, establishing the layout
[514 zeros][low bits wh/4][stuffed entropy][terminator] for those bodies, and
reports the zero run + stuffing purity for every file class. No entropy decoding.

Usage: python -I p1c_lowbits_bounds.py file.crw ...
"""

import struct
import sys


def records(buf, start, length, endian, depth=0):
    if depth > 8 or length < 6:
        return
    (tbl,) = struct.unpack_from(endian + "I", buf, start + length - 4)
    if tbl + 2 > length - 4:
        return
    (n,) = struct.unpack_from(endian + "H", buf, start + tbl)
    if tbl + 2 + n * 10 > length:
        return
    for k in range(n):
        tc, ln, off = struct.unpack_from(endian + "HII", buf, start + tbl + 2 + k * 10)
        if (tc >> 14) & 3:
            continue
        if off + ln > length:
            continue
        if ((tc >> 11) & 7) in (5, 6):
            yield from records(buf, start + off, ln, endian, depth + 1)
        else:
            yield (tc & 0x3FFF, start + off, ln)


def analyse(fname):
    buf = open(fname, "rb").read()
    endian = {b"II": "<", b"MM": ">"}.get(buf[0:2])
    (hl,) = struct.unpack_from(endian + "I", buf, 2)
    r = {}
    for t, o, l in records(buf, hl, len(buf) - hl, endian):
        r.setdefault(t, (o, l))
    ro, rl = r[0x2005]
    so, sl = r[0x1031]
    v = struct.unpack_from(endian + "H" * (sl // 2), buf, so)
    w, h = v[1], v[2]
    do, _ = r[0x1835]
    n_tbl, _, f2, comp_len = struct.unpack_from(endian + "IIII", buf, do)
    data = buf[ro : ro + rl]
    low = w * h // 4
    eos = 0 <= rl - comp_len - low <= 4

    # positions of non-stuffed ff (ff followed by non-00), excluding the terminator
    pos = []
    i = 0
    while True:
        i = data.find(0xFF, i)
        if i < 0 or i + 1 >= len(data):
            break
        if data[i + 1] != 0:
            pos.append(i)
        i += 2
    pos = [p for p in pos if p < rl - 4]

    comp_start = low if eos else 0
    zr = 0
    while comp_start + zr < rl and data[comp_start + zr] == 0:
        zr += 1
    name = fname.replace("\\", "/").rsplit("/", 1)[-1]
    if eos:
        inside = sum(1 for p in pos if p < low)
        print(f"{name:36} EOS layout: lowbits [0:{low}) + zeros({zr}) + entropy; "
              f"non-stuffed ff: {len(pos)} total, {inside} in lowbits, "
              f"{len(pos) - inside} in entropy (expect 0)")
    else:
        lo, hi = 514, 514 + low
        inside = sum(1 for p in pos if lo <= p < hi)
        if pos and inside == len(pos):
            print(f"{name:36} PS 12-bit layout: zeros({zr}) + lowbits [{lo}:{hi}) + entropy; "
                  f"all {len(pos)} non-stuffed ff inside lowbits; "
                  f"first {pos[0]}, last {pos[-1]}")
        elif not pos:
            print(f"{name:36} PS 10-bit layout: zeros({zr}) + entropy; "
                  f"no non-stuffed ff (pure stuffed stream)")
        else:
            print(f"{name:36} PS ?: {len(pos)} non-stuffed ff, {inside} inside "
                  f"[{lo}:{hi}); first {pos[0]}, last {pos[-1]} — hypothesis fails")


for fn in sys.argv[1:]:
    try:
        analyse(fn)
    except Exception as e:
        print(f"{fn}: !! {e!r}")
