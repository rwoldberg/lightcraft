"""P1b — ff-successor and JPEG-marker statistics of the RawData payload regions.

Follow-up to p1_payload.py: on EOS bodies every 0xff in the compressed region was followed
by 0x00 (JPEG-style stuffing); on the G5 the payload starts with exactly 514 zero bytes
(= the constant DecoderTable field) and ends in ff d9 (JPEG EOI). This script measures,
per file: the histogram of bytes following 0xff in the compressed region, any JPEG markers
(ff xx, xx not 00), and — for public files — hex of small structural windows (region
starts/ends, bytes around offset 514). Still no entropy decoding.

Usage: python -I p1b_markers.py [--no-bytes] file.crw ...
"""

import struct
import sys
from collections import Counter


def find_records(buf, start, length, endian, depth=0):
    """Yield (tag_id, abs_offset, length) for in-heap records, recursively
    (same container model as ciff_dump.py; duplicated so the script runs under python -I)."""
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
        stg = (tc >> 14) & 3
        dt = (tc >> 11) & 7
        tag = tc & 0x3FFF
        if stg != 0:
            yield (tag, None, (ln, off))
            continue
        if off + ln > length:
            continue
        if dt in (5, 6):
            yield from find_records(buf, start + off, ln, endian, depth + 1)
        else:
            yield (tag, start + off, ln)


def analyse(fname, show):
    out = [f"== {fname}"]
    buf = open(fname, "rb").read()
    endian = {b"II": "<", b"MM": ">"}.get(buf[0:2])
    (hlen,) = struct.unpack_from(endian + "I", buf, 2)
    recs = {}
    for tag, off, ln in find_records(buf, hlen, len(buf) - hlen, endian):
        if off is not None:
            recs.setdefault(tag, (off, ln))
    roff, rlen = recs[0x2005]
    data = buf[roff : roff + rlen]
    soff, sln = recs.get(0x1031, (None, 0))
    vals = struct.unpack_from(endian + "H" * (sln // 2), buf, soff) if soff else ()
    w, h = (vals[1], vals[2]) if len(vals) > 2 else (0, 0)
    doff, _ = recs.get(0x1835, (None, 0))
    n_tbl, _, f2, comp_len = struct.unpack_from(endian + "IIII", buf, doff)
    low = w * h // 4
    has_low = 0 <= rlen - comp_len - low <= 4
    comp = data[low : low + comp_len] if has_low else data[:comp_len]
    out.append(f"  raw {w}x{h} tbl={n_tbl} f2={f2} L={comp_len} lowbits={'yes' if has_low else 'no'}")

    succ = Counter()
    markers = Counter()
    first_markers = []
    i = 0
    while True:
        i = comp.find(0xFF, i)
        if i < 0 or i + 1 >= len(comp):
            break
        nxt = comp[i + 1]
        succ[nxt] += 1
        if nxt != 0:
            markers[nxt] += 1
            if len(first_markers) < 12:
                first_markers.append((i, nxt))
            i += 2
        else:
            i += 2
    top = ", ".join(f"{b:02x}:{c}" for b, c in succ.most_common(8))
    out.append(f"  ff followed by: {top}")
    if markers:
        mk = ", ".join(f"ff{b:02x}:{c}" for b, c in sorted(markers.items()))
        out.append(f"  markers: {mk}")
        out.append(f"  first markers at: {[(hex(p), hex(m)) for p, m in first_markers]}")
        rst = [p for p, m in first_markers if 0xD0 <= m <= 0xD7]
        if len(rst) > 2:
            out.append(f"  first restart gaps: {[rst[k + 1] - rst[k] for k in range(len(rst) - 1)]}")
    else:
        out.append("  markers: none (pure stuffed entropy data)")
    if show:
        out.append(f"  comp[0:24]   : {comp[:24].hex()}")
        out.append(f"  comp[-8:]    : {comp[-8:].hex()}")
        out.append(f"  raw[504:544] : {data[504:544].hex()}")
        out.append(f"  raw[-8:]     : {data[-8:].hex()}")
        if has_low:
            out.append(f"  low[-8:]     : {data[low - 8 : low].hex()} (end of low-bits region)")
    return out


def main():
    args = sys.argv[1:]
    show = True
    if args and args[0] == "--no-bytes":
        show = False
        args = args[1:]
    for fname in args:
        try:
            print("\n".join(analyse(fname, show)))
        except Exception as e:
            print(f"== {fname}\n  !! exception: {e!r}")
        print()


if __name__ == "__main__":
    main()
