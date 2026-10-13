# CRW RawData payload layout (P1) — established by statistics, no decoding

Status: derived 2026-10-12 from the 29-file corpus by `p1_payload.py`, `p1b_markers.py`,
`p1c_lowbits_bounds.py` (logs: `logs/p1-*.txt`, `logs/p1b-*.txt`, `logs/p1c-*.txt`, private
counterparts under `logs/private/`). **The entropy-coded bitstream itself has not been decoded;**
everything here comes from sizes, offsets, byte histograms and ff-digram statistics.
Assumption IDs refer to `00-starting-assumptions.md`; corrections are stated explicitly.

## 1 The three layouts

With S = RawData record size, (N, 0, 514, L) = the DecoderTable u32s, w×h = SensorInfo raw
dimensions, Q = ⌈w·h/4⌉ (one byte per four pixels — the "low-bits" region candidate):

| Class (bodies) | RawData layout | Evidence |
|---|---|---|
| EOS: D30, D60, 10D, 300D (23 files) | `[low bits, Q][514 zero bytes][stuffed entropy, L−514−t][tail t]` | S−L = Q+2 or Q+3 on all 23; the section after Q starts with exactly 514 zero bytes in all 23; the entropy part contains **zero** non-stuffed ff (every 0xff is followed by 0x00) in all 23 |
| PowerShot 12-bit: G5, G6 | `[514 zero bytes][low bits, Q][stuffed entropy][ff d9 (+00)]` | S−L = 2; first 514 bytes zero; **all** non-stuffed ff (5002 of 5002 on G5, 7928 of 7928 on G6) fall inside [514, 514+Q); the rest of the stream is perfectly stuffed; payload ends `ff d9` |
| PowerShot 10-bit: G1 | `[514 zero bytes][stuffed entropy][ff d9 + 00]` | S−L = 3; first 514 bytes zero; zero non-stuffed ff anywhere; payload ends `ff d9 00` |

Private corpus: all 18 D60 files match the EOS row exactly (`logs/private/p1c-bounds-private.txt`).

## 2 DecoderTable (0x1835) semantics

`(N, 0, 514, L)` where:

- **N = decoder table selector**, 0–2, varies per shot on the same body (B5 supported; all
  three values exercised by the corpus). Presumably chosen by the camera per image; the
  mapping (ISO? content?) is not yet established and is not needed for decoding.
- Second u32: 0 in all 29 files.
- **514 = offset to the entropy data within the compressed section** (ExifTool names the field
  *CompressedDataOffset*). Behaviourally: the compressed section begins with exactly 514 zero
  bytes in all 29 files, EOS and PowerShot alike — a reserved/padding block the decoder skips.
  (On G5/G6 the low-bits region sits between this zero block and the entropy data.)
- **L = CompressedDataLength**: the byte length of the compressed section — from the end of
  the low-bits prefix on EOS, from 0 on PowerShots — excluding the small tail.

## 3 Tails

- EOS: 2 bytes `00 00` (D30, and files where S−L−Q = 2) or 3 bytes `ff 00 00` (D60/10D/Rebel
  files with S−L−Q = 3). The `ff 00` there may be a final stuffed 0xff of the entropy stream
  sitting just outside L — to be settled in P2 when the stream end is decodable.
- PowerShots: `ff d9` (the JPEG EOI marker used as a terminator) plus one `00` pad when needed
  to keep the record length even (G1: S−L = 3 = `ff d9 00`; G5/G6: S−L = 2 = `ff d9`).

## 4 The low-bits regions (still a hypothesis as to *content*, region *extent* established)

- Extent Q = w·h/4 exactly, in all 25 files that have the region (B6's arithmetic holds).
- D60-class prefix: reshaped at w/4 = 788 bytes/row, the first **7 rows are exactly zero**
  (5516 = 7×788 bytes) in the public D60 file, and the leading zero run is row-aligned —
  strong evidence the region is row-major at 2 bits/pixel (`logs/p1-payload-public.txt`).
  Below the crop-top band the bytes are near-uniform (entropy ≈ 7.99), as LSB noise should be.
- D30's region is high-entropy from byte 0 and ends in a run of `ff` bytes (saturated LSBs?);
  no zero rows — the D30's masked rows evidently carry noise. Placement for D30 is supported
  by the size arithmetic and by its entropy part being perfectly stuffed (0 non-stuffed ff).
- **Not yet established:** the bit packing order within each byte, and that the bits really
  are the two LSBs of the final samples (vs. e.g. MSBs). Decisive test planned (P2, with the
  oracle): the oracle's output codes' 2 LSBs must equal this region bit-for-bit under one
  fixed packing rule, for every file.

## 5 Corrections to the starting assumptions

- **B3 (bare bitstream, "no JPEG wrapper")** — half right: there are no JPEG markers or
  segment structure, but the entropy data **is byte-stuffed exactly like JPEG entropy data**
  (0xff is always followed by 0x00; 2 576–28 275 stuffed ff bytes per file, zero exceptions
  across 29 files), and the PowerShot payloads even end with a JPEG EOI. **B8's "no
  byte-stuffing" is refuted.** The decoder must unstuff `ff 00 → ff`.
- **B6 (low bits)** — the region exists, but the recollection had its distribution backwards:
  it is present on all EOS bodies (even the 10-bit-era D30 — see open question below) and on
  the later 12-bit PowerShots (G5/G6), absent on the 10-bit G1; and its position differs by
  class (prefix on EOS, after the 514-byte zero block on PowerShots).
- **B2 (bit depth)** — if the low-bits hypothesis holds, G1 is the only "no extra bits" body
  here; whether EOS bodies are 10+2 or the region means something else is a P2 question. (The
  D30 having the region argues the split is not simply "12-bit bodies".)
- **A9 DecoderTable** — layout now pinned: (selector, 0, entropy-offset 514, compressed length).

## 6 Per-file accounting (public corpus)

| File | w×h | S | N | L | S−L | Q | S−L−Q | zero run @comp |
|---|---|---|---|---|---|---|---|---|
| d30 | 2224×1456 | 2 824 648 | 1 | 2 015 110 | 809 538 | 809 536 | 2 | 514 |
| d60 | 3152×2068 | 6 694 146 | 0 | 5 064 559 | 1 629 587 | 1 629 584 | 3 | 514 |
| 10d-a | 3152×2068 | 7 134 454 | 0 | 5 504 868 | 1 629 586 | 1 629 584 | 2 | 514 |
| 10d-b | 3152×2068 | 7 244 858 | 2 | 5 615 272 | 1 629 586 | 1 629 584 | 2 | 514 |
| 10d-c | 3152×2068 | 7 455 524 | 2 | 5 825 938 | 1 629 586 | 1 629 584 | 2 | 514 |
| 300d-a | 3152×2068 | 6 929 638 | 0 | 5 300 052 | 1 629 586 | 1 629 584 | 2 | 514 |
| 300d-b | 3152×2068 | 7 206 250 | 2 | 5 576 664 | 1 629 586 | 1 629 584 | 2 | 514 |
| rebel | 3152×2068 | 6 406 164 | 0 | 4 776 577 | 1 629 587 | 1 629 584 | 3 | 514 |
| g1 | 2144×1560 | 2 257 354 | 1 | 2 257 351 | 3 | — | — | 514 |
| g5 | 2672×1968 | 4 861 806 | 1 | 4 861 804 | 2 | 1 314 624 (inside L) | — | 514 |
| g6 | 3160×2344 | 7 313 156 | 0 | 7 313 154 | 2 | 1 851 760 (inside L) | — | 514 |

(S here is the RawData record size, not the file size. Private D60 table:
`logs/private/p1-payload-private.txt`.)

## 7 Open for P2 (entropy derivation)

1. The entropy code itself (B4): alphabet, tables per selector N, block structure, predictors
   (B7), bit order within bytes (rest of B8), bit depth per body (B2).
2. Low-bits packing order and recombination (B6) — decisive oracle test in §4.
3. Why 514 (= 512 + 2?) — cosmetic; decoder just skips to the offset given by the record.
4. The D60-class "7 zero rows" and D30's trailing `ff` run in the low-bits region — expect an
   explanation to fall out of the geometry work (P3).
5. EOS tail `ff 00 00` vs `00 00` — settle when the stream end is decodable.

P2 requires the approved oracle (LibRaw pre-built binary, run black-box; plan §5a decision 1–2)
for probing and for the low-bits test. **Requested from the maintainer before P2 starts.**
