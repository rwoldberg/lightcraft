# CRW clean-room derivation — starting assumptions

Written 2026-10-12, **before** reading any file in this repository (other than the standing
CLAUDE.md instructions loaded into the session), any web page, any specification, or any sample
file. Everything below is recalled background knowledge of the author (Claude Fable 5) and is the
baseline the derivation starts from. **Every item is: unverified, no source consulted.**

This file is committed once and never edited. Corrections, confirmations and refutations go in
`01-plan.md` and the derivation documents, each citing the experiment or published document that
established them. A reviewer can diff this file against the final documentation to see which
facts were assumed and which were derived.

Confidence labels are the author's own recollection strength, not evidence: *firm* = would be
surprised if wrong; *hazy* = plausibly misremembered or confused with another format (CR2/CR3,
other makers' raws).

## A. Container

- **A1** (firm, unverified, no source consulted): `.CRW` files are in CIFF, the "Camera Image
  File Format", a Canon-proprietary heap-based container. It is not TIFF-based and not
  Exif/JPEG-based (unlike the later `.CR2`, which is TIFF).
- **A2** (firm, unverified, no source consulted): the file begins with a 2-byte byte-order mark,
  `II` for little-endian (as TIFF does); Canon cameras write little-endian. `MM`/big-endian is
  allowed by the format but not expected in camera files.
- **A3** (hazy, unverified, no source consulted): a 32-bit integer at offset 2 gives the length
  of the file header, i.e. the offset at which the heap data begins; the common value is 26
  (0x1A).
- **A4** (firm, unverified, no source consulted): the header contains the ASCII signature
  `HEAPCCDR` (at offset 6, immediately after the header-length field).
- **A5** (firm, unverified, no source consulted): the body of the file is a "heap". A heap's
  directory is located via the **last 4 bytes** of the heap region: a 32-bit offset, relative to
  the start of the heap, pointing at the directory. Data blocks come first, directory last.
- **A6** (firm, unverified, no source consulted): a directory is a 16-bit record count followed
  by that many fixed-size 10-byte records: tag (u16), size (u32), offset (u32), with offsets
  relative to the start of the containing heap. There may be trailing bytes after the records.
- **A7** (hazy, unverified, no source consulted): the tag value's high bits encode storage class
  and data type, the low bits the tag identity. In particular, one storage class means "value
  stored in the heap" (size/offset meaningful) and another means "value stored in the record
  itself" (the 8 bytes that would hold size+offset hold the data inline). Recalled masks:
  storage class ≈ `0xC000`, data type ≈ `0x3800`, id ≈ low 11 bits — exact masks uncertain.
- **A8** (firm, unverified, no source consulted): some records are themselves heaps
  (sub-directories), giving a tree: e.g. an image-properties heap, a camera-object heap, an
  Exif-information heap. Parsing must recurse; recursion depth must be bounded against malformed
  files.
- **A9** (hazy, unverified, no source consulted): recalled tag numbers — `0x2005` raw image
  data; `0x1835` "decoder table" selector for the raw compression; `0x1031` sensor/image
  dimensions (including the margins of masked pixels); `0x2007` embedded JPEG
  ("JpgFromRaw"-like); `0x080a` make and model as NUL-separated strings; something like
  `0x102a` holding shot info (ISO, etc.) and a white-balance record whose tag I do not recall.
  All tag numbers here are suspect.
- **A10** (firm, unverified, no source consulted): cameras writing CRW also write a sibling
  `.THM` file: a small Exif/JPEG thumbnail carrying the metadata. The CRW itself may embed a
  preview JPEG as well.
- **A11** (hazy, unverified, no source consulted): CRW was written by early Canon EOS DSLRs
  (D30, D60, 10D, 300D/Digital Rebel) and PowerShot models of the same era (G1–G6, S-series,
  Pro1). Later bodies (20D on) switched to CR2.

## B. Raw data storage and coding

- **B1** (firm, unverified, no source consulted): the raw payload is a single plane of Bayer
  CFA samples, pattern RGGB at the sensor origin, covering the full readout area including
  masked/optically-black border pixels, which is slightly larger than the visible image.
- **B2** (hazy, unverified, no source consulted): sample depth is 10 or 12 bits depending on
  the model; the EOS D60 is 12-bit. (10-bit is recalled for some PowerShots.)
- **B3** (firm, unverified, no source consulted): the raw data is losslessly compressed with a
  Canon-proprietary scheme. It is **not** a JPEG stream: no JFIF/JPEG markers, no
  entropy-coded-segment byte stuffing wrapper — a bare bitstream.
- **B4** (firm, unverified, no source consulted): the scheme is Huffman-coded DPCM in the style
  of JPEG's AC coefficient coding: the stream is organised in blocks of 64 difference values;
  each Huffman code decodes to a (run-of-zero-differences, magnitude-bit-count) pair; a
  magnitude of n bits follows as n raw bits with JPEG-style sign extension (a leading 0 bit
  means negative, value = read − (2^n − 1)).
- **B5** (hazy, unverified, no source consulted): there is a small fixed set of Huffman table
  pairs (two or three), built into the camera/decoder rather than stored in the file; the
  "decoder table" record (see A9) holds an index selecting which set. Within a set, one table
  codes the first value of each 64-block and a second table codes the remaining 63 — the
  first/rest split is the haziest part of this recollection.
- **B6** (hazy, unverified, no source consulted): a "low bits" mode exists: when enabled, the
  2 least-significant bits of every pixel are stored separately, uncompressed and packed 4 per
  byte, at the start of the raw data section, and the Huffman stream codes only the high bits;
  the final sample is (decoded << 2) | low_bits. Which cameras/flags enable it is unknown to
  me; possibly all 10-bit → 12-bit? Uncertain.
- **B7** (hazy, unverified, no source consulted): decoded differences accumulate into samples
  via two running predictors, one for even and one for odd columns (i.e. per CFA column
  parity). I do not recall where the predictors reset (per row? per row pair? never after
  initialisation to a mid-grey such as 512?) or how the first pixels of the image are formed.
- **B8** (hazy, unverified, no source consulted): bits are consumed MSB-first within each byte;
  there is no marker-escaping; I vaguely recall some per-camera quirk where a count of spare
  bits or a skipped prefix is involved, but cannot state it.
- **B9** (firm, unverified, no source consulted): the visible image is a crop of the decoded
  area; a sensor-info record gives the full decoded width/height and the left/top borders of
  the visible region.
- **B10** (hazy, unverified, no source consulted): black level is taken from the masked border
  pixels (or a stored value); white-balance coefficients are in a metadata record as per-channel
  levels or gains. Formats of those records not recalled.
- **B11** (hazy, unverified, no source consulted): EOS D60 visible size is 3072×2048 (6.3 MP);
  the decoded raw area is slightly wider/taller (numbers not recalled). File size around 7–8 MB,
  consistent with ~12 bits/pixel compressed losslessly below 2:1.
- **B12** (hazy, unverified, no source consulted): some PowerShot-era CRW variants may store
  the raw data uncompressed or in a different packing; unsure whether any in-scope body does.

## C. Process assumptions

- **C1** (firm, unverified, no source consulted): ExifTool publishes prose documentation of the
  CIFF/CRW structure and tag names (web pages, distinct from its Perl source, which is off
  limits under the clean-room rules).
- **C2** (hazy, unverified, no source consulted): Canon once published a CIFF specification
  document (circa 1997–98, "CIFF Specification on Image Data File", version ~1.0 r4) that
  circulates publicly as a PDF.
- **C3** (hazy, unverified, no source consulted): raw.pixls.us hosts CRW samples for several of
  the bodies in A11, some CC0; per-file licences vary, so each file must be checked.
