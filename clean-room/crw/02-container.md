# CRW container (CIFF heap file) — derived parsing notes (P0)

Status: derived from published documentation and the 29-file corpus; every fact cites its
source. Sources: **[SPEC]** = Canon, *CIFF Specification on Image Data File* v1.0 r4 (see
`sources.md`, hash pinned); **[ET]** = ExifTool's prose pages `canon_raw.html` /
`TagNames/CanonRaw.html`; **[OBS]** = observation of the corpus via `ciff_dump.py`
(`logs/structure-public.txt`, `logs/private/structure-private.txt`). Assumption numbers refer
to `00-starting-assumptions.md`.

## 1 File header (26 bytes) [SPEC §4.4.1, ET; OBS: all 29 files]

| Offset | Size | Field | Observed |
|---|---|---|---|
| 0 | 2 | byteOrder: `II` little-endian, `MM` big-endian | `II` in all 29 |
| 2 | 4 | headerLength: file start → heap-space start | 26 in all 29 |
| 6 | 4 | type | `HEAP` in all 29 |
| 10 | 4 | subType | `CCDR` in all 29 (spec reserves `JPGM`, `TIFP`, `ARCH`) |
| 14 | 4 | version: hi16 major, lo16 minor | 0x00010002 in all 29 |
| 18 | 8 | reserved1, reserved2 | — |

Multi-byte integers throughout the file follow byteOrder [SPEC]. Parser rules: refuse unknown
byteOrder; honour headerLength rather than assuming 26; refuse type ≠ `HEAP`; refuse
subType ≠ `CCDR` as unsupported (a CHDK sensor dump or other container — A11/B12). `MM` files
exist nowhere in the corpus (§6 of the plan); the Rust parser will implement both orders from
[SPEC] and test `MM` synthetically.

## 2 Heap structure [SPEC §4.1–4.4; ET; OBS]

- The **root block** spans headerLength..EOF. A **block** = ValueData (heap space), then the
  offsetTbl, then optional other data, with a u32 `offsetTblOffset` stored in the final 4 bytes
  of the block, relative to the block start [SPEC §4.1.2; ET "DirStart at BlockSize−4"].
- **offsetTbl** = u16 `numRecords` + numRecords × 10-byte `DC_RECORD_ENTRY {typeCode u16,
  length u32, offset u32}`; offsets are relative to the containing block's start [SPEC §4.4.2;
  ET]. numRecords = 0 is legal [SPEC §4.1.4].
- **typeCode bit fields** [SPEC §4.4.2.1, masks quoted in §8.2]:
  - bits 15–14, mask 0xc000, storage: `00` in heap space (length/offset meaningful), `01` in
    record entry (the 8 bytes of length+offset hold the value, zero-padded; treated as length 8
    unless self-delimiting), `10`/`11` reserved → refuse as unsupported.
  - bits 13–11, mask 0x3800, dataType: 0 byte, 1 ASCII (NUL-terminated), 2 u16s, 3 u32s/f32s,
    4 structure (byte-aligned mix), 5/6 heap record (a complete sub-block), 7 reserved →
    refuse.
  - bits 10–0, mask 0x07ff, IDCode. ExifTool's "TagID" convention = typeCode & 0x3fff
    (dataType | IDCode); this document uses that convention for tag numbers.
- **Sub-blocks** (dataType 5/6): the entry's length/offset give the sub-block's extent inside
  the parent's ValueData; offsets inside it are relative to its own start [SPEC §4.2; ET;
  OBS: all nested directories resolve correctly this way on all 29 files].
- **Special records** [SPEC §4.4.4]: 0x0000 null (reserves an offsetTbl slot; length/offset
  zero), 0x0001 free (reserves heap space). Skip both.
- **Alignment**: record lengths are even; odd-length values are padded [SPEC §4.4.5].
- Records may appear in any order; data generally precedes its directory ("the natural way to
  write information" [ET]).

## 3 Observed file tree (CRW from cameras) [OBS: all 29 files]

```
root (block = headerLength..EOF)
├─ 0x2005 RawData                 struct, at heap offset 0, the bulk of the file
├─ 0x2007 JpgFromRaw              struct, a complete JPEG (starts FF D8)
├─ (G1/G5/G6, D30: 0x2008 ThumbnailImage — JPEG)
└─ 0x300a ImageProps              heap
   ├─ 0x1803 ImageFormat          dword, in-entry
   ├─ 0x1810 ImageInfo            dword[7] — the FULL-VIEW image: w, h, aspect f32,
   │                                rotation s32, componentBitDepth, colorBitDepth, colorBW
   │                                (per [SPEC §5.4.2] ImageSpec; NOT the raw bit depth)
   ├─ 0x180e TimeStamp            dword[3]: time_t, zone, zoneInfo [SPEC CapturedTime]
   ├─ 0x0816/0x0817 file names    ascii (e.g. CRW_0099.CRW / CRW_0099.THM)
   ├─ 0x0805 description          ascii
   ├─ 0x3003 MeasuredInfo         heap (0x1814 MeasuredEV …)
   ├─ 0x2804 ImageDescription     heap (0x0805 descr, 0x0815 CanonImageType, e.g.
   │                                "CRW:EOS D60 CMOS RAW IMAGE")
   ├─ 0x2807 CameraObject         heap
   │  ├─ 0x0810 OwnerName         ascii
   │  ├─ 0x080a CanonRawMakeModel ascii: "Canon\0<model>\0" [SPEC ModelName]
   │  └─ 0x3004 CameraSpecification heap (0x180b SerialNumber, 0x101c BaseISO,
   │       0x080b FirmwareVersion …)
   └─ 0x300b ExifInformation      heap
      ├─ 0x102a CanonShotInfo     u16 array (first u16 = byte length)
      ├─ 0x102d CanonCameraSettings u16 array
      ├─ 0x1031 SensorInfo        u16[17]: len(34), rawW, rawH, ?, ?, cropL, cropT,
      │                             cropR, cropB, then 4+ more u16 (black windows? TBD P3)
      ├─ 0x1835 DecoderTable      u32[4]: (tableNumber, 0, 514, compressedLength) — see
      │                             03-payload-layout.md
      ├─ 0x10a9 ColorBalance, 0x10a8, 0x10aa  u16 arrays (WB candidates, P3)
      ├─ 0x1030 WhiteSample       (10D/300D; BlackLevels candidate [ET], P3)
      ├─ 0x1834 CanonModelID      dword, in-entry
      └─ 0x1033 CustomFunctions, 0x10c0/0x10c1/0x10c2, 0x0036 … (not needed for decode)
```

Per-body constants observed (rawW×rawH; crop rect, inclusive):

| Body | Raw | Crop (L,T)–(R,B) | Visible |
|---|---|---|---|
| EOS D30 | 2224×1456 | (56,12)–(2215,1451) | 2160×1440 |
| EOS D60 / 10D / 300D | 3152×2068 | (72,16)–(3143,2063) | 3072×2048 |
| PowerShot G1 | 2144×1560 | (24,15)–(2071,1550) | 2048×1536 |
| PowerShot G5 | 2672×1968 | — (fields differ; P3) | 2592×1944 (ImageInfo) |
| PowerShot G6 | 3160×2344 | — (P3) | 3072×2304 (ImageInfo) |

(G-series SensorInfo tails differ in shape from the EOS ones — e.g. G1's fields 9–12 are
`2108, 15, 2111, 1550`, a second rectangle to the right of the crop: a plausible black/dark
measurement window. To be established in P3.)

## 4 Records the decoder will consume

0x2005 RawData (payload), 0x1835 DecoderTable, 0x1031 SensorInfo, 0x1810 ImageInfo (visible
size cross-check + rotation), 0x080a make/model + 0x1834 CanonModelID (dispatch), 0x10a9 /
0x10a8 / 0x10aa / 0x1030 (black & WB, P3), 0x180e TimeStamp, 0x2007 JpgFromRaw (preview +
geometry verification), 0x102a CanonShotInfo (ISO etc. for metadata display).

## 5 Hostile-input rules for the eventual Rust parser (never-crash)

All reads through checked `get`-style access; every offset/length validated against the
containing block before use; offsets/lengths are u32 → promote to u64/usize with checked math,
no wrapping; recursion over sub-blocks bounded (observed depth 4; cap at 16 with a clear
"malformed: heap nesting too deep" error); a record table whose 2 + 10·numRecords exceeds
offsetTblOffset→block-end space is malformed; in-entry values are exactly the 8 bytes of
length+offset; unknown storage class, dataType 7, unknown subType, or missing mandatory
records (0x2005, 0x1031, 0x1835) → typed unsupported/malformed errors, never a panic;
allocations sized by header fields capped against the actual file size.
