# Canon CRW (CIFF) raw decoding — clean-room derivation plan

Written 2026-10-12. **Status: awaiting the maintainer's approval. No decoder code and no table
data exist yet; nothing below §5 phase P1 starts until this plan is approved.**

Companion documents in this directory:

- [`00-starting-assumptions.md`](00-starting-assumptions.md) — what was assumed before any
  source was consulted (committed first, immutable). Items are cited below as A1–A11 (container),
  B1–B12 (raw coding), C1–C3 (process).
- [`sources.md`](sources.md) — every URL opened and search run, with what was taken.
- [`ciff_dump.py`](ciff_dump.py) + [`logs/`](logs/) — the planning-phase container survey.

Rules in force (unchanged from the task): no raw-decoder source is ever opened, fetched,
searched inside or requested (dcraw, LibRaw, rawspeed, rawloader/rawler, dnglab, darktable,
RawTherapee, libopenraw, ExifTool's Perl code — including licence headers, comments and
mirrors); the earlier `crw-decode` branch is not checked out, diffed, logged or read; public
samples are CC0 files from raw.pixls.us only; the maintainer's private files never enter the
repository and no document ever includes their image content (SHA-256 and size only).

---

## 1 What public documentation establishes (sources, kept separate from assumptions)

### 1.1 Canon, *CIFF Specification on Image Data File*, v1.0 r4, 1997-12-24

Archived PDF (SHA-256 `416d4e49ea3f6ad2ba5ccc76b48ca100605f4a6127816c145e6dd5d6c3b51cf4`,
URL in `sources.md`; local copy in gitignored `plan/crw/`). Canon's
own published specification. It establishes, with exact definitions:

- **Heap file header** (quoted struct): `byteOrder[2]` (`"II"` little-endian / `"MM"`
  big-endian), `headerLength` u32 ("distance from the start of the file to the beginning of the
  heap-space"), `type[4]` = `"HEAP"`, `subType[4]` (`"JPGM"` for JPEG-embedded; reserved values
  include `"CCDR"`, `"TIFP"`, `"ARCH"`), `version` u32 = 0x00010002, two reserved u32s.
- **Heap** = heap space followed by the `offsetTbl`; a u32 `offsetTblOffset` is stored "in the
  final position at the end of the heap", as an offset relative to the start of the heap.
- **offsetTbl** = `numRecords` u16 + that many `DC_RECORD_ENTRY {typeCode u16, length u32,
  offset u32}`; offset is "offset value from the beginning of the heap"; `numRecords` may be 0.
- **typeCode bit fields**: bits 15–14 storage (`00` kStg_InHeapSpace, `01` kStg_InRecordEntry —
  data held in the 8 bytes of the length+offset fields themselves — `10`/`11` reserved); bits
  13–11 dataType (`000` byte, `001` ASCII string, `010` 2-byte, `011` 4-byte, `100` structure,
  `101`/`110` heap record, `111` reserved); bits 10–0 IDCode. Masks (quoted):
  `kStgFormatMask 0xc000`, `kDataTypeMask 0x3800`, `kIDCodeMask 0x07ff`.
- Heap records (dataType 101/110) are themselves heaps → the tree structure.
- Null (`kTC_Null` 0x0000) and free (`kTC_Free` 0x0001) records; all records 2-byte aligned,
  odd lengths padded.
- Common metadata record structs: ImageSpec (imageWidth, imageHeight, pixelAspectRatio f32,
  rotationAngle s32, componentBitDepth, colorBitDepth, colorBW), ImageFormat, CapturedTime
  (time_t + zone), ModelName ("two null-terminated strings … manufacturer name … model name"),
  and the ImageProps/ShootingRecord/MeasuredInfo/Camera/CameraSpecification hierarchy.
- **The spec does not describe the raw image payload at all.** ImageFormat's fileFormat value 2
  is merely "Reserved (for CanonCRW format)". Everything about the raw coding must come from
  sample analysis (and an approved oracle), not from documentation.

### 1.2 ExifTool prose pages (documentation, not Perl source)

`exiftool.org/canon_raw.html` (format description) and `TagNames/CanonRaw.html` (tag tables):

- A `.CRW` file is a heap file: 26-byte header with signature `HEAPCCDR` (= type `HEAP` +
  subType `CCDR`), CRWVersion 0x00010002; the root block runs from `headerLength` to EOF; a
  subdirectory is a complete block inside its parent's ValueData whose block size is the
  entry's Size; offsets are relative to the containing block's ValueData start.
- Tag IDs (TagID = dataType | IDCode, i.e. typeCode & 0x3fff): **0x2005 RawData** ("the bulk
  of the CRW file"), **0x2007 JpgFromRaw** (embedded JPEG), 0x2008 ThumbnailImage, **0x1835
  DecoderTable** with sub-values named *DecoderTableNumber, CompressedDataOffset,
  CompressedDataLength*, **0x1031 SensorInfo**, 0x1810 ImageInfo, 0x1030 WhiteSample (with
  *BlackLevels*), 0x10a9 ColorBalance, 0x10ae ColorTemperature, 0x080a CanonRawMakeModel,
  0x102a CanonShotInfo, 0x1834 CanonModelID, 0x300a ImageProps, 0x2807 CameraObject, 0x300b
  ExifInformation, 0x0000 NullRecord, 0x0001 FreeBytes, and others (full list in `sources.md`).
- Neither page describes the raw compression, bit depth or decoder tables' contents.

### 1.3 Wikipedia, "Camera Image File Format"

Spec identity and dates; the CRW camera list: EOS D30, D60, 10D, 300D; PowerShot Pro1, G1–G6,
S30–S70. (Matches A11.)

## 2 Corpus

### 2.1 Public (CC0, raw.pixls.us; downloaded into gitignored `corpus/crw/`, hashes verified)

| File | Body | Size (bytes) | SHA-256 |
|---|---|---|---|
| `crw-canon-eos-d30.crw` | EOS D30 | 2 993 260 | `4ee482f2b6f9f1a8d370703ea04e9f2abaa49356973a7f56b229af93a78e8a2e` |
| `crw-canon-eos-d60.crw` | EOS D60 | 8 591 234 | `2b98659ecff170af1dfe44f89b493139e75e995b2979686f6ff0847cfb56b97f` |
| `crw-canon-eos-10d-a.crw` | EOS 10D | 7 787 330 | `b0730eb3a67cc2281a32e0b2ee104199246928d954a68b7e5b72366514c73d02` |
| `crw-canon-eos-10d-b.crw` | EOS 10D | 10 822 040 | `e879c94afdc64dabc88b01ce2d02628aa225259303ec71dd61f7fb0322bfea2d` |
| `crw-canon-eos-10d-c.crw` | EOS 10D | 10 583 702 | `58144118854657df5cd2185df40fecf6b665fc6754fef0416c2f222b68b025c6` |
| `crw-canon-eos-300d-a.crw` | EOS 300D | 9 056 348 | `47da16e1668af5500eea63cf42cf2992d9cb3d32180b8d786f17f1e2a57f39f4` |
| `crw-canon-eos-300d-b.crw` | EOS 300D | 8 389 842 | `9e7f62239fe728046af28d0d4a8a722d35d40146a79794a5bda1775306af4383` |
| `crw-canon-eos-digital-rebel.crw` | EOS 300D (US name) | 10 153 786 | `d6a3131bfac390fde01b96882ec378211f04570c3252708b58f0d05b33a9fdfc` |
| `crw-canon-powershot-g1.crw` | PowerShot G1 | 2 300 702 | `f39be5acf14057ce04527fd8d0574e57ec8ed5fed082d7bfaff763075a44ee8a` |
| `crw-canon-powershot-g5.crw` | PowerShot G5 | 4 894 440 | `bb084223d714871b99de4f549eaf00513e30a7080feeead700252c2008060196` |
| `crw-canon-powershot-g6.crw` | PowerShot G6 | 8 052 032 | `244378e1f5660b6cc6cefc93a75ed4928d0b28aa6304ff327b61c748538f4fef` |

All 11 are listed CC0 by raw.pixls.us (licence per file verified in the site's data on
2026-10-12; `sources.md`). Each download was checked against the site's published SHA-256.

### 2.2 Private (maintainer's EOS D60 files, `../RawImages/`, local only — never committed, no
image content in any document)

18 files, `CRW_0476.crw` … `CRW_2219.crw`, 5 500 958 – 8 628 810 bytes. SHA-256 and sizes
recorded in `sources.md`-adjacent planning logs; full hash list:

| File | Size | SHA-256 |
|---|---|---|
| CRW_0476.crw | 6 906 290 | `7966aeb5abf3abe80868b8cb1606d671112c42f44d877d3ff79305a52558cacc` |
| CRW_0478.crw | 5 708 018 | `6d07ef616f4b1c7c3025e7af315bb7fb4173ddefea45e2e52d5a4d28e9f2851f` |
| CRW_0486.crw | 8 055 488 | `bf0876f5ba55b2bd120f9588b91f9d4caad54ab73315e542114bf738459d8b3b` |
| CRW_0527.crw | 7 158 482 | `2fd42a69caa20e7c07fe022bd73dfe2626d60a3bacc4228073d9627a5dfa03c1` |
| CRW_0683.crw | 7 453 390 | `c4b320a158b1859fa0709050caddfe2bec0d0a566eafeb17df89cc6b64346a75` |
| CRW_0717.crw | 6 633 986 | `0d6b64935d928e2e0d378eb6efe2b46bed9026ed227111fd47b5fe8b911e22ad` |
| CRW_0765.crw | 5 636 168 | `e8da71d85e07d4157fb5753f850b331dfa6451d7382c4c0d4f9568144e6e8bc5` |
| CRW_0822.crw | 6 478 092 | `817acdc28dd1fe2cd0065b44c63722aaa1f800275e0b0b245ad3b3d964df7c9f` |
| CRW_0879.crw | 6 861 152 | `df66c000ac18b872a69b0fdc171245f89869bd0ad415e117c77e6c27ea93c0ca` |
| CRW_1011.crw | 6 822 000 | `9044f07f30064249cb5ea78bd6ba5d19ae3d2da278dbf7f10aa357872fbbd469` |
| CRW_1153.crw | 7 729 790 | `9cd504ed0991c4c6033d72a2676c778b84d198ddfad202437fcb7f014fa15a17` |
| CRW_1277.crw | 6 850 414 | `c4f0ce8c4ce85d3668b3726c46018d296c784b10a2f0f322acba8b1681fe86f4` |
| CRW_1514.crw | 6 531 360 | `9aaa960461f829b218eaa7f50cf49fa3cb80e92baf9f5aabc03038fddbefe06c` |
| CRW_1646.crw | 6 204 492 | `f9f3dbc2f9aed2cdc2ddccd9a9768a1e02501c337258a211ac9fb663ad78ff74` |
| CRW_1740.crw | 5 500 958 | `033f225fcc8bfdb07b013d971c594bda2adfa0a683c133acc22a171ddcdf95a6` |
| CRW_1776.crw | 6 746 924 | `b07921872c8cf7767841945818a6c8d59553415580d7ebe6a08115964ec6de44` |
| CRW_2035.crw | 6 595 632 | `0ffd9fec448a667b7850ea505b5b117f01c07a98aff5e0b7ec9da7fc5909b4e8` |
| CRW_2219.crw | 8 628 810 | `0460e9ba4884c7e0867570cf71be7a6d2d038fdcecf062fde5cc28422602dd07` |

## 3 Established from sample files during planning (container level only)

All from `ciff_dump.py` over the 29 files (logs: `logs/structure-public.txt`,
`logs/private/structure-private.txt`). No raw-data decoding was attempted.

- **All 29 files parse fully** with the documented container model: `II`, headerLength 26,
  `HEAP`+`CCDR`, version 0x00010002; every record resolves in bounds; heap tree ≤ 4 deep
  (root → ImageProps → CameraObject/ExifInformation/… → CameraSpecification). Confirms
  A1–A8 on real files (details per assumption in §4).
- **Root block layout** in every file: RawData (0x2005) first at heap offset 0, then
  JpgFromRaw (0x2007, starts with JPEG SOI), then ImageProps (0x300a).
- **SensorInfo (0x1031)**, 17 u16s beginning with its own length (34): D60/10D/300D all
  `(34, 3152, 2068, 1, 1, 72, 16, 3143, 2063, 24, 224, 40, 1856, 0, 0, 0, 0)`; D30
  `(34, 2224, 1456, 1, 0, 56, 12, 2215, 1451, 8, 220, 40, 1244, …)`; G1
  `(34, 2144, 1560, 1, 1, 24, 15, 2071, 1550, 2108, 15, 2111, 1550, …)`; G5 2672×1968; G6
  3160×2344. For the EOS bodies, fields 5–8 as (left, top, right, bottom) give exactly the
  visible size: 3143−72+1 = 3072, 2063−16+1 = 2048, matching ImageInfo (0x1810) = 3072×2048.
  (Bears on B1/B9/B11; the remaining fields are candidates for black-measurement windows — to
  be established.)
- **ImageInfo (0x1810)** u32s `(3072, 2048, 1.0f, rot, 8, 24, 257)` — the 8/24 bit depths
  describe the *full-view image* (per the spec's ImageSpec definition), **not** the raw depth;
  rot = 270 on the portrait 300D shots. Corrects the hope that raw bit depth is read from here.
- **DecoderTable (0x1835)** = 4 u32s `(N, 0, 514, L)`:
  - N ∈ {0, 1, 2} — all three values observed, varying per shot *on the same body* (private
    D60 set has 15× N=1, 2× N=0, 1× N=2; public files: D30 1, D60 0, 10D 0/2/2, 300D 0/2,
    Rebel 0, G1 1, G5 1, G6 0). Supports B5's "selector" reading; presumably exposure/ISO
    dependent — to be established.
  - Third u32 is 514 in all 29 files (meaning unknown; 512+2? — to be established).
  - **L vs RawData size S**: on every EOS file (D30, D60, 10D, 300D, 23 files),
    S − L = ⌈width×height/4⌉ + 2 or 3 bytes (e.g. D60: 6 694 146 − 5 064 559 = 1 629 587;
    3152×2068/4 = 1 629 584). On every PowerShot file (G1, G5, G6), S − L = 2 or 3 bytes.
    Strong container-level evidence for B6 (a 2-bits-per-pixel region, pixels packed 4 per
    byte, present on the EOS bodies and absent on these PowerShots), **but its placement,
    packing order and meaning are still to be derived** — note the tension with B6's
    recollection that low bits appear on PowerShots.
- **RawData byte statistics**: overall entropy 7.84–7.99 bits/byte (dense entropy coding); on
  the D60-class files the first 2048 bytes are all zero; on D30 the data is high-entropy from
  byte 0; on G1/G5 the first ~522–529 bytes are zero-dominated. No JPEG markers at the start;
  JPEG SOI appears only in the separate 0x2007 record.
- The three 10D files and the three 300D/Rebel files are internally consistent per body;
  the 18 private D60 files are uniform with the public D60 file.

## 4 Starting assumptions → experiments

Status: **spec** = established by §1 documents; **samples** = established by §3 observations;
**open** = still unverified — the listed experiment decides it during derivation.

| # | Status | Correction / experiment, and what counts as confirmed |
|---|---|---|
| A1 CIFF heap container | spec+samples | Confirmed (header `HEAP`/`CCDR`; all 29 parse). |
| A2 `II` first, LE | spec+samples | Confirmed; `MM` legal per spec — decoder must accept or cleanly refuse it; no `MM` sample exists (§6). |
| A3 u32 header length @2 = 26 | spec+samples | Confirmed (26 in all 29; field is authoritative, not the constant). |
| A4 `HEAPCCDR` @6 | spec+samples | Confirmed, with correction: it is two fields, type `HEAP` + subType `CCDR`; other subTypes exist (`JPGM`…) and must be refused as unsupported. |
| A5 directory via trailing u32 | spec+samples | Confirmed (`offsetTblOffset` in the heap's final position, relative to heap start). |
| A6 u16 count + 10-byte entries | spec+samples | Confirmed (typeCode u16, length u32, offset u32 — in that order). |
| A7 tag bit masks | spec+samples | Confirmed exactly: 0xc000 storage (0x0000 in-heap, 0x4000 in-entry, rest reserved), 0x3800 dataType, 0x07ff IDCode. |
| A8 nested heaps, bounded recursion | spec+samples | Confirmed (dataType 5/6 = heap record); observed depth ≤ 4; parser caps at 8. |
| A9 tag numbers | samples | Mostly right: 0x2005 RawData, 0x1835 DecoderTable, 0x1031 SensorInfo, 0x2007 JpgFromRaw, 0x080a make/model, 0x102a ShotInfo all confirmed; ColorBalance is 0x10a9. DecoderTable is 4×u32, not "16 bytes of table data". |
| A10 .THM sibling + embedded JPEG | part | Embedded JpgFromRaw confirmed (SOI seen); ThumbnailFileName records name a `.THM`, but no `.THM` files are present in `../RawImages` — treat the THM as optional; not needed for decoding. |
| A11 camera list | spec-adjacent | Matches Wikipedia + raw.pixls.us. Later `.CRW`-named CHDK dumps are a different format: detect via header and refuse as unsupported. |
| B1 Bayer CFA, RGGB at origin, full readout incl. masked borders | open | Decode must yield exactly SensorInfo's width×height samples; CFA phase from (i) colour of uniform patches vs the embedded JPEG, (ii) per-channel black/white statistics, (iii) oracle's CFA output. Confirmed when all three agree on all 29 files. |
| B2 10- or 12-bit samples | open | From decoded histograms (max code, gaps), clipped-highlight files, and the low-bits arithmetic (wh/4 = 2 bits/pixel implies the Huffman stream codes depth−2 bits on EOS bodies). Confirmed when the white clip point is stable per body and matches the oracle's scale. |
| B3 not JPEG, bare bitstream | samples (part) + open | No JPEG structure observed; fully confirmed when the derived bitstream model consumes the payload exactly on all files. |
| B4 Huffman run/size DPCM in 64-blocks | open | The central derivation (P2, §5): a candidate code model must decode every file such that (i) the stream is consumed to within the final byte, (ii) every sample lands in [0, 2^depth), (iii) rows have natural image statistics (smoothness, CFA correlation). Confirmed only when one model does this for all 29 files and matches the oracle exactly. |
| B5 small fixed set of table pairs selected by file | samples (part) + open | DecoderTableNumber ∈ {0,1,2} observed varying per shot. Open: the tables' content, whether they live in camera or file (the constant `514` field and the RawData prefix are candidates to rule in/out), first-vs-rest table split. Confirmed when one fixed table set per N decodes every file with that N, across bodies. |
| B6 2 LSBs stored separately, packed 4/byte | samples (part) + open | wh/4-byte region established on EOS bodies — but B6 guessed PowerShots; observed on EOS instead, and placement unknown (D60-class zero prefix suggests the region leads, since masked top rows plausibly have zero LSBs — but D30's leading bytes are high-entropy, so order may differ or D30's masked rows are noisy). Experiments: (i) statistical — LSB region should be near-uniform in lit areas and structured in masked areas consistently across files; (ii) decisive — oracle output codes' 2 LSBs must equal the extracted region bit-for-bit with one fixed packing rule. |
| B7 two column-parity predictors | open | Derived in P2 by fitting predictor variants in the Python model; confirmed when interior and first-row/first-column pixels match the oracle on all files, incl. the masked border. |
| B8 MSB-first bits, no byte-stuffing | open | Both orders tried in P2; the wrong one fails the exact-consumption test almost immediately. Confirmed with B4. |
| B9 visible crop from sensor record | samples (part) + open | Crop rectangle (72,16)–(3143,2063) = 3072×2048 established; confirmed as *the* crop when our visible output aligns with the embedded JPEG (geometry check as in arw6) and the oracle. |
| B10 black from masked pixels / record; WB in metadata | open | Candidates observed: 0x1030 WhiteSample (BlackLevels), 0x10a9 ColorBalance, 0x10a8/0x10aa (u16 arrays that look like per-channel levels). Experiments: correlate record values with decoded masked-area means per channel and with the embedded JPEG's white balance; confirmed when black/WB applied to our decode reproduces the JPEG's grey axis on neutral patches. |
| B11 D60 3072×2048 / slightly larger raw | samples | Confirmed: 3152×2068 raw, 3072×2048 visible. |
| B12 uncompressed CRW variants | open | For the in-scope bodies, none observed (all payloads are high-entropy). CHDK `.CRW` dumps are a different format → refused as unsupported, not decoded. Confirmed by header discrimination tests. |
| C1 ExifTool prose docs exist | confirmed | §1.2. |
| C2 Canon CIFF spec circulates | confirmed | §1.1 (34-page archived PDF). |
| C3 raw.pixls.us CRW samples, mixed licences | confirmed | §2.1 — CC0 for 7 bodies; several classic bodies NC-only (§6). |

## 5 Derivation plan (phases, freeze, verification)

Modelled on [`docs/arw6-compression.md`](../../docs/arw6-compression.md): published docs +
samples → Python reference model → **freeze** → black-box verification, no tuning after the
freeze. All phase outputs are documents/scripts in `clean-room/crw/`; each derived fact cites
the experiment (script + log) or document that established it.

- **P0 — container (mostly done during planning).** Finalize the CIFF parsing notes
  (`02-container.md`): header, heap walk, record classes, the records we consume (0x2005,
  0x1835, 0x1031, 0x1810, 0x080a, 0x1834, 0x10a9/0x10a8/0x10aa, 0x1030, 0x180e, 0x5029 …),
  endianness rules, malformed-file behaviour. Includes hostile-input rules for the eventual
  Rust parser (offsets/lengths checked with `get`, bounded recursion, no panics).
- **P1 — payload partition.** Statistics-only scripts (`p1_*.py`): establish the split of
  RawData into (candidate) low-bits region, entropy-coded region, and the 2–3 trailing bytes;
  the meaning of DecoderTable fields (incl. the constant 514) by cross-file regression;
  per-body low-bits presence. Output: `03-payload-layout.md` with per-file accounting tables.
  Still no bitstream decoding.
- **P2 — entropy code derivation.** The core. Hypothesis-driven, in a Python reference model
  (`model/`): bit order (B8), code alphabet and block structure (B4), table set per
  DecoderTableNumber (B5), predictor/DPCM rules (B7), low-bits recombination (B6), bit depth
  (B2). Method: propose a code model; the acceptance tests are *internal* — exact stream
  consumption on every file, samples within range, natural-image statistics, cross-file
  consistency (same tables for same N across bodies) — iterating over the hypothesis space.
  If the oracle (§5a) is approved early, black-box probing is allowed to speed this up
  (feeding CC0 files and byte-locally modified CC0 files to the oracle binary and observing
  output deltas, as arw6 probed with synthetic tiles); every probe is logged in `sources.md`
  style in `04-derivation-log.md`. **No decoder source is consulted regardless.**
- **P3 — geometry, black, white balance, colour.** Visible crop (B9) against the embedded
  JPEG at pixel alignment; black level from masked regions + candidate records (B10); white
  point per body (B2); WB coefficients from 0x10a9/0x10a8 against the JPEG's neutrals; CFA
  phase (B1). Output: `05-geometry-colour.md`.
- **P4 — freeze + verification.** Write `06-crw-format.md`: the complete derived format
  (container summary, payload layout, tables as data, algorithm in prose + the Python model's
  final hash). **Freeze point: that document's commit.** Only then run the approved oracle
  over all 11 public files (and, locally, the 18 private files) and compare decoded samples
  exactly; record match/mismatch per file in `07-verification.md` with zero tuning. A
  mismatch reopens derivation explicitly: the reopening, its cause and the new freeze commit
  are recorded — never silently adjusted.
- **P5 — Rust implementation (separate task(s), after the maintainer approves the derived
  format).** `crates/raw` CIFF container + `vendor/crw.rs` decoder; never-crash rules
  (hostile offsets, bounded recursion, no unwrap/expect/panic, capped allocations);
  unconditional synthetic tests (round-trip via a test-only encoder like `arw6_testenc`,
  truncated/hostile input) + corpus-gated tests with pinned sums; `cargo xtask corpus`
  entries for 2 pinned CC0 files; docs (`docs/crw-parity.md` or a section in
  `docs/raw-parity.md`), parity-checklist/gaps/ROADMAP rows per CLAUDE.md. Perf budget: at
  6 MP these files should decode far inside the 1 s/24 MP export budget; measured in P5.

### 5a Reference ("oracle") for what the decoded samples should be — **maintainer's decision**

| Candidate | What it gives | Licence / repo-rule implications |
|---|---|---|
| **(1) LibRaw binary, run as a black box** (as `docs/arw6-compression.md` did: built binary only, source never read) | Exact 16-bit pre-processing codes; the gold standard for B4–B8 | The task forbids fetching LibRaw *source*, so building from source is out for me. LibRaw.org publishes pre-built Windows binaries; I am also forbidden to install/run downloaded programs, so the **maintainer** would download and run it (or approve my running it). Output used as data only; nothing enters the repo but comparison sums. LibRaw is LGPL/CDDL — running a binary copies no code; the house precedent treats this as clean. |
| **(2) Adobe DNG Converter** (free Adobe tool, CRW → DNG; mosaic DNG holds the raw codes losslessly) | Exact codes + Adobe's reading of black/white/WB/crop as DNG tags | Adobe software: CLAUDE.md forbids copying Adobe assets and reading app bundles; running their converter on our CC0/own files and reading the output DNG (an open, documented format) copies no Adobe asset. Residual concerns: Adobe's EULA, and that its interpretation (crop, black) is Adobe's, not the camera's. Same install/run constraint: maintainer runs it. |
| **(3) The embedded JpgFromRaw** | Geometry, orientation, CFA phase, WB neutrals, gross tone sanity | None — it is inside our samples. Lossy and processed, so never a pixel-exact reference. |
| **(4) No-outside-reference checks** | Exact stream consumption, range bounds, masked-area statistics, low-bits cross-checks, Python↔Rust equality, round-trip with our own test encoder | None. Necessary but not sufficient: they validate self-consistency, not correctness of interpretation. |

**Recommendation:** (3) + (4) always; (1) as the primary oracle, obtained as an official
pre-built binary and run by the maintainer (or by me with explicit approval), used only after
the P4 freeze except for approved black-box probing in P2; (2) optionally as a second opinion
where (1) and our model disagree. **Flagged: the choice, and who runs what, is the
maintainer's.**

## 6 What cannot be established from the available samples

- **Bodies with no CC0 file** (raw.pixls.us has them only as CC-BY-NC-SA, which the rules
  exclude): PowerShot G2, G3, Pro1, Pro70, S30, S40, S45, S50, S60, S70 (and S2 IS/S3 IS-class
  CHDK-era bodies). They can at best be decoded speculatively: if their container metadata
  (DecoderTable, SensorInfo, payload accounting) matches a verified body's pattern, the decoder
  may accept them with an "unverified" note, as arw6 does for the ILCE-7M5; otherwise they
  return unsupported. No G2/G3/S-series verification is possible without new samples.
- **Big-endian (`MM`) files**: none exist here; `MM` handling will be implemented from the spec
  and tested only synthetically, or refused — decision in P5.
- **Reserved storage classes (0x8000/0xc000) and dataType 7**: unobserved; refused as
  unsupported with a regression test.
- **Per-body table-number coverage**: observed N per body — D30 {1}, D60 {0,1,2}, 10D {0,2},
  300D {0,2}, G1 {1}, G5 {1}, G6 {0}. Numbers a body never writes can't be verified for it;
  cross-body consistency of a given N is testable and part of P2's acceptance.
- **Controlled-exposure pairs** (same scene raw+JPEG at known settings, dark frames, clipped
  highlights): none; black/white derivation rests on masked areas, metadata records and the
  embedded JPEGs, which is weaker than arw6's lossless/compressed pairs. If the maintainer can
  shoot dark-frame and clipped D60 exposures, B2/B10 confirmation strengthens materially.
- **Colour calibration** (camera matrices/profiles): out of scope here, as for every body;
  LightCraft's own calibration work, never copied.
- **The remaining SensorInfo fields** (e.g. the `24, 224, 40, 1856` tail on EOS bodies) may
  resist interpretation without more bodies; unknowns will be documented as unknown.

## 7 Open questions for the maintainer — **answered 2026-10-12, plan approved**

Maintainer's decisions (recorded verbatim in intent, numbered as below):

1. **Oracle:** no Adobe DNG Converter. LibRaw binary as the arw6 work did; the maintainer will
   install it when it is needed.
2. **Black-box probing** before the freeze: allowed, arw6-style (every probe logged).
3. **libopenraw wiki page:** stays unread.
4. **Python reference model:** local-only, like arw6's Phase 0 model (not committed). The
   committed evidence set stays as in this commit; the private-file name+size+hash table in
   §2.2 is fine to keep.
5. **Scope order:** agreed — D60-class first, then D30, then G1/G5/G6.
6. **NC-licensed bodies:** accept "unverified, header-matched" decoding. The maintainer owns
   none of those cameras and has no files from them.
7. **Extra captures:** the maintainer still has the D60 (and a D20) and will provide controlled
   shots when the derivation needs them.

Also noted by the maintainer: the earlier `crw-decode` branch has been deleted and no longer
exists in the repository; the do-not-read rule about it is therefore moot but remains honoured
for any surviving copies.

### Original questions (for the record)

1. **Oracle** (§5a): approve candidate (1) (official pre-built LibRaw binary, black box), and
   who runs it — you, or me after your explicit approval of the specific download? Allow (2)
   Adobe DNG Converter as a secondary reference?
2. **Black-box probing before the freeze** (P2): may the oracle be used interactively on CC0
   and synthetic/modified-CC0 inputs during derivation (arw6-style), with every probe logged —
   or only after the freeze?
3. **Prose pages from decoder projects**: the libopenraw freedesktop wiki has a "Canon CRW"
   page (surfaced in a search, not opened). It is prose documentation, but authored by a GPL
   decoder project and may paraphrase its implementation. Default is to leave it unread;
   confirm, or allow it as a documentation source logged in `sources.md`?
4. **Commit policy for `clean-room/crw/`**: plan, sources log, scripts and public-sample logs
   are committed (this commit); `logs/private/` is gitignored (may hold personal metadata:
   owner name, serial, timestamps). The Python reference model (P2): committed, or local-only
   like arw6's Phase 0 model? Private-file hashes in §2.2: confirm you're happy with the
   file-name+size+hash table being committed.
5. **Scope order**: derive on the D60-class first (richest corpus: 23 files, one sensor
   geometry, all three table numbers), then D30, then G1/G5/G6 — agreed?
6. **NC-licensed bodies** (§6): accept "unverified, header-matched" decoding for G2/G3/Pro1/
   S-series, or return unsupported until samples exist? Do you own any of these cameras, or
   CRW files from other bodies (10D/300D/G-series), that could serve as further private
   samples?
7. **Extra captures**: if you still have the D60, controlled shots (lens cap dark frame;
   clipped sky; grey card at two ISOs, each at all quality settings so all DecoderTableNumbers
   appear) would directly confirm B2/B5/B10. Worth doing?

---

*Approved 2026-10-12 (§7). P0 documentation and P1 are under way.*
