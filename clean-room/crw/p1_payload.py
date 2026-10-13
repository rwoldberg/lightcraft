"""P1 — RawData payload partition statistics (no bitstream decoding).

Questions (plan 01-plan.md P1):
  1. Exact leading zero run of RawData per file.
  2. Does RawData split as [low-bits wh/4][compressed L][pad] (prefix hypothesis) or
     [compressed L][low-bits wh/4][pad] (suffix hypothesis) on EOS bodies? Tested by
     row-structure statistics of both candidate regions (2 bits/pixel, rawW/4 bytes/row):
     masked sensor rows should show as structured/near-constant rows, the entropy-coded
     stream should not align to 788-byte rows.
  3. The constant third DecoderTable u32 (514) and the 2-3 byte remainder.
  4. Byte-stuffing check in the compressed region (ff-digram statistics).

Prints statistics only; no sample values of image content beyond aggregate counts.

Usage: python -I p1_payload.py file.crw [more...]
"""

import math
import struct
import sys
from collections import Counter


def read_block_records(buf, start, length, endian):
    if length < 6:
        return []
    (tbl,) = struct.unpack_from(endian + "I", buf, start + length - 4)
    if tbl + 2 > length - 4:
        return []
    (n,) = struct.unpack_from(endian + "H", buf, start + tbl)
    recs = []
    pos = start + tbl + 2
    if tbl + 2 + n * 10 > length:
        return []
    for _ in range(n):
        tc, ln, off = struct.unpack_from(endian + "HII", buf, pos)
        pos += 10
        recs.append((tc, ln, off))
    return recs


def find_records(buf, start, length, endian, depth=0):
    """Yield (tag_id, abs_offset, length) for every in-heap record, recursively."""
    if depth > 8:
        return
    for tc, ln, off in read_block_records(buf, start, length, endian):
        stg = (tc >> 14) & 3
        dt = (tc >> 11) & 7
        tag = tc & 0x3FFF
        if stg != 0:
            yield (tag, None, (ln, off))  # in-entry value
            continue
        if off + ln > length:
            continue
        if dt in (5, 6):
            yield from find_records(buf, start + off, ln, endian, depth + 1)
        else:
            yield (tag, start + off, ln)


def entropy(data):
    if not data:
        return 0.0
    c = Counter(data)
    n = len(data)
    return -sum(v / n * math.log2(v / n) for v in c.values())


def zero_run(data):
    i = 0
    while i < len(data) and data[i] == 0:
        i += 1
    return i


def row_structure(region, row_bytes, crop_top, label, out):
    """Reshape region into rows of row_bytes; report zero/constant-row runs and
    per-band mean of 2-bit values (both bit orders give the same mean-of-byte/4 scale,
    so use byte means as the aggregate)."""
    nrows = len(region) // row_bytes
    if nrows == 0:
        out.append(f"      {label}: shorter than one row")
        return
    zrows = 0
    for r in range(nrows):
        row = region[r * row_bytes : (r + 1) * row_bytes]
        if any(row):
            break
        zrows += 1
    bands = []
    for name, lo, hi in (("rows 0..top", 0, min(crop_top, nrows)),
                         ("top..top+32", min(crop_top, nrows), min(crop_top + 32, nrows)),
                         ("middle 32", nrows // 2, min(nrows // 2 + 32, nrows)),
                         ("last 32", max(0, nrows - 32), nrows)):
        seg = region[lo * row_bytes : hi * row_bytes]
        if seg:
            bands.append(f"{name}: mean {sum(seg) / len(seg):7.2f} ent {entropy(seg):5.3f}")
    out.append(f"      {label}: {nrows} rows of {row_bytes} B, leading all-zero rows: {zrows}")
    for b in bands:
        out.append(f"        {b}")


def ff_digrams(data, out):
    n = len(data)
    c = Counter(data)
    ffs = c.get(0xFF, 0)
    ff00 = sum(1 for i in range(n - 1) if data[i] == 0xFF and data[i + 1] == 0)
    exp = ffs * c.get(0, 0) / max(1, n)  # expected if independent
    out.append(f"      stuffing check: ff count {ffs} ({ffs / max(1, n):.4%}), "
               f"ff00 digrams {ff00}, expected-if-independent {exp:.1f}")


def analyse(fname, show_bytes):
    out = [f"== {fname}"]
    buf = open(fname, "rb").read()
    endian = {b"II": "<", b"MM": ">"}.get(buf[0:2])
    (hlen,) = struct.unpack_from(endian + "I", buf, 2)
    recs = dict()
    raw = None
    for tag, off, ln in find_records(buf, hlen, len(buf) - hlen, endian):
        if tag == 0x2005:
            raw = (off, ln)
        recs.setdefault(tag, (off, ln))
    if raw is None or raw[0] is None:
        out.append("  !! no RawData record")
        return out
    roff, rlen = raw
    data = buf[roff : roff + rlen]
    # SensorInfo
    soff, sln = recs.get(0x1031, (None, 0))
    w = h = top = 0
    if soff:
        vals = struct.unpack_from(endian + "H" * (sln // 2), buf, soff)
        w, h, top = vals[1], vals[2], vals[6]
    # DecoderTable
    doff, dln = recs.get(0x1835, (None, 0))
    dt = struct.unpack_from(endian + "IIII", buf, doff) if doff and dln >= 16 else (None,) * 4
    n_tbl, d1, d2, comp_len = dt
    low = (w * h) // 4
    rem = rlen - (comp_len or 0)
    out.append(f"  raw {w}x{h}  S={rlen}  tbl={n_tbl}  f1={d1} f2={d2}  L={comp_len}"
               f"  S-L={rem}  wh/4={low}  S-L-wh/4={rem - low}")
    out.append(f"  leading zero run: {zero_run(data)} bytes"
               + (f"  (= {zero_run(data) / (w / 4):.2f} rows of 2-bit LSBs)" if w else ""))
    if show_bytes:
        out.append(f"  last 8 bytes: {data[-8:].hex()}")
    has_low = rem - low in (0, 1, 2, 3, 4) and low > 4 * (w or 1)
    if has_low and w:
        out.append("  prefix hypothesis  — region [0 : wh/4) as 2-bit rows:")
        row_structure(data[:low], w // 4, top, "prefix", out)
        out.append("  suffix hypothesis  — region [L : L+wh/4) as 2-bit rows:")
        row_structure(data[comp_len : comp_len + low], w // 4, top, "suffix", out)
        out.append("  compressed-candidate stats:")
        for name, seg in (("after-prefix [wh/4 : wh/4+L)", data[low : low + comp_len]),
                          ("from-0 [0 : L)", data[:comp_len])):
            out.append(f"      {name}: entropy {entropy(seg[:1 << 20]):.4f} (first MiB), "
                       f"{entropy(seg[-(1 << 20):]):.4f} (last MiB)")
        ff_digrams(data[low : low + comp_len], out)
    else:
        out.append("  no wh/4 region (PowerShot class): compressed assumed [0 : L)")
        out.append(f"      entropy first MiB {entropy(data[:1 << 20]):.4f}, "
                   f"last MiB {entropy(data[max(0, comp_len - (1 << 20)) : comp_len]):.4f}")
        ff_digrams(data[:comp_len], out)
        out.append(f"      tail region [L : S): {rem} bytes"
                   + (f", hex {data[comp_len:rlen].hex()}" if show_bytes and rem <= 8 else ""))
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
