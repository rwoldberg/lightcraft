# lightcraft-video

MP4 and QuickTime video (`.mp4`, `.mov`, `.m4v`) for LightCraft: what a file holds, its poster
image, and its frames. A thin, panic-guarded wrapper around FilmCraft's clean-room, pure-Rust
(no `unsafe`, MIT OR Apache-2.0) demuxer and decoders: `filmcraft-isobmff`, `filmcraft-h264`, and
`filmcraft-hevc` behind the `hevc` feature.

```rust
let video = lightcraft_video::Video::open_path("clip.mov")?;   // headers only
let info = video.info();        // size as shown, rotation, duration, frame rate, codec, playable,
                                // has_audio, created, make / model, has_preview
if let Some(image) = video.embedded_preview()? { /* a JPEG or PNG: decode with lightcraft-codecs */ }
let poster = video.poster_frame(Some(512))?;                   // the first picture, reduced
let mut reader = video.reader()?;                              // owns its decoder: Send
reader.set_max_edge(Some(1280));                               // whole-factor reduction for a small view
reader.seek(12.5)?;
while let Some(frame) = reader.next_frame()? { /* frame.rgba, frame.width, frame.height, frame.time */ }
```

- **Never read whole.** `Video::open` reads the file's headers through a `ByteSource` (a `File`,
  or bytes in memory); a reader fetches each compressed frame as it decodes it.
- **As shown.** Frames come back opaque RGBA with the track's rotation applied (a phone held
  upright records sideways), converted with the stream's own matrix and range (BT.601 / 709 /
  2020, video or full range). 10-bit pictures are reduced to 8 bits.
- **Embedded preview.** Cover art (`meta/ilst/covr`) or a camera's thumbnail in `moov/udta`, when
  the file has one. `Video::poster_frame` is the fallback: the first decoded picture.
- **Errors, never panics.** `Error::Unsupported` (no video track, a codec without a decoder
  here), `Error::Limit`, `Error::Malformed`, `Error::Io`. Sizes and counts from the file are
  checked before use, and every call into the demuxer and decoders runs under `catch_unwind`.
  A reader that failed keeps reporting that failure instead of decoding on.

## Not yet

- Sound (the audio track is only reported: `Info::has_audio`).
- HDR video is shown without tone mapping (HLG / PQ pictures look flat).
- Codecs other than H.264 and HEVC (ProRes, AV1, Motion JPEG…): the file opens, `playable` is
  false and `reader()` is `Error::Unsupported`. HEVC 4:2:2 and 12-bit are unsupported upstream.
- Hardware decoding: pictures are decoded in software on the CPU.

## Features

- `hevc`: H.265 clips (most iPhone video). Optional for the reason `lightcraft-heif` is: HEVC is
  patent-encumbered in some jurisdictions, so including a decoder is a distributor's choice.
  Without it an HEVC file still opens and gives its embedded preview; `reader()` is a clear
  "HEVC video isn't included in this build" error.
- `testkit`: generated test clips (`lightcraft_video::testkit::clip`), which needs an H.264
  encoder. For tests only; no product build enables it. LightCraft never commits media.

## The pin

The FilmCraft crates are a git dependency pinned to one commit in the workspace `Cargo.toml`
(same team, same licence, shared so the sibling apps have one decoder). Moving the pin is a
reviewed change: re-run `cargo test -p lightcraft-video --features hevc` and play real clips.

The crate depends on no other LightCraft crate.
