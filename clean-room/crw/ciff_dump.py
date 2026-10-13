"""CIFF/CRW container structure dump (planning phase — no raw-data decoding).

Prints the heap tree, record entries (tag, storage class, data type, size, offset),
a few interpreted metadata records (make/model, ImageSpec, SensorInfo-ish u16 arrays,
DecoderTable u32s), and byte statistics of the RawData record. It never decodes the
compressed raw bitstream.

Container layout implemented from published documentation only:
- Canon, "CIFF Specification on Image Data File" v1.0 r4, 1997-12-24 (archived PDF):
  heap file header {byteOrder[2], headerLength u32, type[4], subType[4], version u32,
  reserved1/2 u32}; heap = heap space + offsetTbl; u32 offsetTblOffset stored in the
  final position of the heap; offsetTbl = numRecords u16 + entries {typeCode u16,
  length u32, offset u32}; typeCode bits 15-14 storage (00 in heap space, 01 in record
  entry), bits 13-11 dataType (000 byte, 001 ASCII, 010 word, 011 dword, 100 struct,
  101/110 heap record), bits 10-0 IDCode.
- ExifTool "canon_raw.html" (prose page): offsets are relative to the start of the
  containing block's ValueData; a subdirectory is a complete block inside ValueData
  whose length is the entry's Size; the root block runs from headerLength to EOF;
  tag IDs incl. 0x2005 RawData, 0x1835 DecoderTable, 0x1031 SensorInfo, 0x080a
  make/model, 0x1810 ImageInfo.

Usage: python -I ciff_dump.py [--no-strings] file.crw [more.crw ...]
--no-strings suppresses record contents (for logs of private files: structure,
sizes, offsets and statistics only; ASCII make/model still shown, pixel data never).
"""

import math
import struct
import sys
from collections import Counter

# Tag names from ExifTool's published tag table (documentation page, not source).
TAG_NAMES = {
    0x0000: "NullRecord", 0x0001: "FreeBytes",
    0x0805: "CanonFileDescription", 0x080A: "CanonRawMakeModel",
    0x080B: "CanonFirmwareVersion", 0x0810: "OwnerName",
    0x0815: "CanonImageType", 0x0816: "OriginalFileName",
    0x0817: "ThumbnailFileName",
    0x100A: "TargetImageType", 0x1010: "ShutterReleaseMethod",
    0x1011: "ShutterReleaseTiming", 0x1016: "ReleaseSetting",
    0x101C: "BaseISO", 0x1028: "CanonFlashInfo",
    0x1029: "CanonFocalLength", 0x102A: "CanonShotInfo",
    0x102C: "CanonColorInfo2", 0x102D: "CanonCameraSettings",
    0x1030: "WhiteSample", 0x1031: "SensorInfo",
    0x1033: "CustomFunctions", 0x1038: "CanonAFInfo",
    0x10A9: "ColorBalance", 0x10AE: "ColorTemperature",
    0x10B4: "ColorSpace", 0x10B5: "RawJpgInfo",
    0x1803: "ImageFormat", 0x1804: "RecordID",
    0x1806: "SelfTimerTime", 0x1807: "TargetDistanceSetting",
    0x180B: "SerialNumber", 0x180E: "TimeStamp",
    0x1810: "ImageInfo", 0x1813: "FlashInfo",
    0x1814: "MeasuredEV", 0x1817: "FileNumber",
    0x1818: "ExposureInfo", 0x1834: "CanonModelID",
    0x1835: "DecoderTable", 0x183B: "SerialNumberFormat",
    0x2005: "RawData", 0x2007: "JpgFromRaw", 0x2008: "ThumbnailImage",
    0x2804: "ImageDescription", 0x2807: "CameraObject",
    0x3002: "ShootingRecord", 0x3003: "MeasuredInfo",
    0x3004: "CameraSpecification", 0x300A: "ImageProps",
    0x300B: "ExifInformation",
}

DATATYPES = {0: "byte", 1: "ascii", 2: "word", 3: "dword",
             4: "struct", 5: "heap1", 6: "heap2", 7: "rsvd7"}

MAX_DEPTH = 8


def entropy(data):
    if not data:
        return 0.0
    counts = Counter(data)
    n = len(data)
    return -sum(c / n * math.log2(c / n) for c in counts.values())


def byte_stats(data, label, out):
    n = len(data)
    out.append(f"      {label}: {n} bytes, entropy {entropy(data):.4f} bits/byte")
    if n >= 4096:
        for part, name in ((data[:2048], "first 2 KiB"), (data[n // 2:n // 2 + 2048], "middle 2 KiB"),
                           (data[-2048:], "last 2 KiB")):
            c = Counter(part)
            top = ", ".join(f"{b:02x}:{cnt}" for b, cnt in c.most_common(4))
            out.append(f"        {name}: entropy {entropy(part):.3f}, top bytes {top}, "
                       f"zeros {c.get(0, 0)}, ff {c.get(255, 0)}")


def dump_heap(f, blk_start, blk_len, depth, endian, out, show, path="root"):
    """blk_start/blk_len: the block (ValueData + offsetTbl + trailing u32)."""
    pre = "  " * depth
    if depth > MAX_DEPTH:
        out.append(f"{pre}!! depth limit at {path}")
        return
    if blk_len < 6:
        out.append(f"{pre}!! block too small ({blk_len}) at {path}")
        return
    f.seek(blk_start + blk_len - 4)
    (tbl_off,) = struct.unpack(endian + "I", f.read(4))
    if tbl_off + 2 > blk_len - 4:
        out.append(f"{pre}!! offsetTblOffset {tbl_off:#x} out of range (block {blk_len:#x}) at {path}")
        return
    f.seek(blk_start + tbl_off)
    (nrec,) = struct.unpack(endian + "H", f.read(2))
    out.append(f"{pre}heap {path}: block @{blk_start:#x} len {blk_len:#x}, offsetTbl @+{tbl_off:#x}, {nrec} records")
    if 2 + nrec * 10 > blk_len - tbl_off:
        out.append(f"{pre}!! record table overruns block at {path}")
        return
    entries = []
    for i in range(nrec):
        tc, length, offset = struct.unpack(endian + "HII", f.read(10))
        entries.append((tc, length, offset))
    for i, (tc, length, offset) in enumerate(entries):
        stg = (tc >> 14) & 3
        dt = (tc >> 11) & 7
        tag_id = tc & 0x3FFF        # dataType | IDCode, ExifTool's "TagID"
        name = TAG_NAMES.get(tag_id, "?")
        loc = {0: "heap", 1: "inrec", 2: "stg2?", 3: "stg3?"}[stg]
        out.append(f"{pre}  [{i}] tag {tc:#06x} id {tag_id:#06x} {name:<22} {DATATYPES[dt]:<6} "
                   f"{loc:<5} len {length:#x} off {offset:#x}")
        if stg == 1:
            continue  # value lives in the length/offset fields themselves
        if offset + length > blk_len:
            out.append(f"{pre}      !! record overruns block")
            continue
        abs_off = blk_start + offset
        if dt in (5, 6):
            dump_heap(f, abs_off, length, depth + 1, endian, out, show, f"{path}/{name or hex(tag_id)}")
        elif tag_id == 0x2005:
            f.seek(abs_off)
            data = f.read(length)
            byte_stats(data, "RawData", out)
        elif tag_id in (0x2007, 0x2008):
            f.seek(abs_off)
            head = f.read(4)
            out.append(f"      starts with {'JPEG SOI' if head[:2] == bytes([0xFF, 0xD8]) else head.hex()}")
        elif show and dt == 1 and length < 256:
            f.seek(abs_off)
            s = f.read(length).rstrip(b"\x00")
            out.append(f"{pre}      = {s!r}")
        elif show and dt == 2 and length <= 64:
            f.seek(abs_off)
            vals = struct.unpack(endian + "H" * (length // 2), f.read(length & ~1))
            out.append(f"{pre}      = u16 {vals}")
        elif show and dt == 3 and length <= 64:
            f.seek(abs_off)
            vals = struct.unpack(endian + "I" * (length // 4), f.read(length & ~3))
            out.append(f"{pre}      = u32 {vals}")
        elif show and dt == 4 and tag_id == 0x1835 and length <= 64:
            f.seek(abs_off)
            vals = struct.unpack(endian + "I" * (length // 4), f.read(length & ~3))
            out.append(f"{pre}      = DecoderTable u32 {vals}")


def dump(fname, show):
    out = [f"== {fname}"]
    with open(fname, "rb") as f:
        f.seek(0, 2)
        fsize = f.tell()
        f.seek(0)
        hdr = f.read(26)
        if len(hdr) < 26:
            out.append("!! file shorter than header")
            return out
        bo = hdr[0:2]
        endian = {b"II": "<", b"MM": ">"}.get(bo)
        if endian is None:
            out.append(f"!! unknown byte order {bo!r} — not CIFF? first 16: {hdr[:16].hex()}")
            return out
        (hdr_len,) = struct.unpack(endian + "I", hdr[2:6])
        sig_type, sig_sub = hdr[6:10], hdr[10:14]
        (version,) = struct.unpack(endian + "I", hdr[14:18])
        out.append(f"  size {fsize}, byteOrder {bo.decode()}, headerLength {hdr_len}, "
                   f"type {sig_type!r} subType {sig_sub!r}, version {version:#010x}")
        if sig_type != b"HEAP":
            out.append("!! not a HEAP file — stopping (CHDK dump or other format?)")
            return out
        dump_heap(f, hdr_len, fsize - hdr_len, 0, endian, out, show)
    return out


def main():
    args = sys.argv[1:]
    show = True
    if args and args[0] == "--no-strings":
        show = False
        args = args[1:]
    for fname in args:
        try:
            lines = dump(fname, show)
        except Exception as e:  # planning tool: report, don't die
            lines = [f"== {fname}", f"!! exception: {e!r}"]
        print("\n".join(lines))
        print()


if __name__ == "__main__":
    main()
