//! `lightcraft-video`: MP4 / QuickTime (`.mp4`, `.mov`, `.m4v`) video — what a file holds, its
//! poster image, and its frames.
//!
//! A thin wrapper around FilmCraft's clean-room, pure-Rust demuxer (`filmcraft-isobmff`) and
//! decoders (`filmcraft-h264`, and `filmcraft-hevc` behind the `hevc` feature), with `zune-jpeg`
//! for Motion JPEG (what older digital cameras recorded), and `filmcraft-aac` for the sound
//! ([`AudioReader`]). Its API is plain data ([`Info`], [`Frame`], [`AudioBlock`], [`Error`]) so
//! the crate knows nothing about the rest of LightCraft.
//!
//! A video is never read whole: [`Video::open`] reads the file's headers through a
//! [`ByteSource`] (a `File`, or bytes in memory), and frames read their samples as they decode.
//!
//! Never panics: sizes and counts from the file are checked before they are used, and every call
//! into the demuxer and the decoders runs under `catch_unwind`, so a panic inside them becomes
//! [`Error::Malformed`].

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

pub use filmcraft_isobmff::ByteSource;
use filmcraft_isobmff::{CodecConfig, Mp4File, Track, TrackKind};

mod audio;
mod boxes;
mod convert;
#[cfg(feature = "testkit")]
pub mod testkit;

pub use audio::{AudioBlock, AudioCodec, AudioInfo, AudioReader};
use convert::{Samples, Yuv};

/// Neither side of a picture may exceed this, nor their product [`MAX_PIXELS`] (8K is 7680 x 4320).
const MAX_SIDE: u32 = 16_384;
const MAX_PIXELS: u64 = 1 << 26;
/// One compressed frame larger than this is not one.
const MAX_SAMPLE: u32 = 256 << 20;
/// Samples fed to a decoder without a picture coming out before giving up on the file.
const MAX_BARREN_SAMPLES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A valid file this build cannot show: no video track, a codec without a decoder here…
    Unsupported(String),
    /// Larger than LightCraft accepts.
    Limit(String),
    /// Not a readable MP4 / QuickTime file, or damaged.
    Malformed(String),
    /// The file could not be read.
    Io(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Unsupported(why) => write!(f, "unsupported video: {why}"),
            Error::Limit(why) => write!(f, "video too large: {why}"),
            Error::Malformed(why) => write!(f, "damaged video: {why}"),
            Error::Io(why) => write!(f, "cannot read video: {why}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

impl From<filmcraft_isobmff::Error> for Error {
    fn from(e: filmcraft_isobmff::Error) -> Self {
        use filmcraft_isobmff::Error as E;
        match e {
            E::Io(e) => Error::Io(e.to_string()),
            E::Unsupported(why) => Error::Unsupported(why),
            E::TooLarge(what) => Error::Limit(what.to_string()),
            other => Error::Malformed(other.to_string()),
        }
    }
}

/// Run `f`, turning a panic inside the demuxer or a decoder into an error.
fn guard<T>(what: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| Err(Error::Malformed(format!("{what} failed unexpectedly"))))
}

/// How the picture is compressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Codec {
    H264,
    /// H.265: decoded only in builds with the `hevc` feature.
    Hevc,
    /// Every frame is a JPEG picture (sample entry `jpeg`): older digital cameras.
    MotionJpeg,
    /// Anything else, by its sample-entry code (`apch`, `av01`, `mjpa`…): no decoder here.
    Other(String),
}

impl Codec {
    /// The name people know it by.
    pub fn name(&self) -> &str {
        match self {
            Codec::H264 => "H.264",
            Codec::Hevc => "HEVC",
            Codec::MotionJpeg => "Motion JPEG",
            Codec::Other(code) => code,
        }
    }
}

/// What a video file declares, read from its headers alone (nothing is decoded).
#[derive(Debug, Clone, PartialEq)]
pub struct Info {
    /// Size as shown: after the track's rotation.
    pub width: u32,
    pub height: u32,
    /// Quarter turns clockwise the stored picture is shown with (a phone held upright: 1).
    pub rotation: u8,
    /// Seconds.
    pub duration: f64,
    /// Frames per second, averaged over the track (0 when unknown).
    pub frame_rate: f64,
    pub frames: usize,
    pub codec: Codec,
    /// This build can decode the picture ([`Video::reader`] works).
    pub playable: bool,
    pub has_audio: bool,
    /// The sound track, when there is one.
    pub audio: Option<AudioInfo>,
    /// When it was recorded, `YYYY-MM-DDTHH:MM:SS`: local time when the camera wrote one,
    /// otherwise the container's creation time (UTC).
    pub created: Option<String>,
    pub make: Option<String>,
    pub model: Option<String>,
    /// The file carries a preview image ([`Video::embedded_preview`]).
    pub has_preview: bool,
}

/// One picture of a video: opaque RGBA, row-major, as shown (rotated).
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Seconds from the start of the video.
    pub time: f64,
}

type Source = Arc<dyn ByteSource + Send + Sync>;

struct Inner {
    source: Source,
    file: Mp4File,
    /// Index of the video track in `file.tracks`.
    track: usize,
    info: Info,
    preview: Option<(u64, u64)>,
    /// Index of the sound track in `file.tracks`, and what it holds.
    audio: Option<(usize, AudioInfo)>,
}

/// An opened video file. Cheap to clone; readers keep it alive.
#[derive(Clone)]
pub struct Video {
    inner: Arc<Inner>,
}

fn codec_of(track: &Track) -> Codec {
    match track.codec() {
        Some(CodecConfig::Avc(_)) => Codec::H264,
        Some(CodecConfig::Hevc(_)) => Codec::Hevc,
        // `mjpa` / `mjpb` are interlaced (two fields a sample, `mjpb` without JPEG markers): not read
        Some(CodecConfig::Jpeg { fourcc }) if &fourcc.0 == b"jpeg" => Codec::MotionJpeg,
        _ => Codec::Other(track.entries.first().map(|e| e.format.to_string_lossy()).unwrap_or_default().trim().to_string()),
    }
}

fn playable(codec: &Codec) -> bool {
    match codec {
        Codec::H264 | Codec::MotionJpeg => true,
        Codec::Hevc => cfg!(feature = "hevc"),
        Codec::Other(_) => false,
    }
}

/// `2024-05-01T10:11:12+0200`, `2024:05:01 10:11:12`, `2024-05-01`… → `2024-05-01T10:11:12`.
fn recorded(text: &str) -> Option<String> {
    let digits: Vec<u32> = text.chars().take(19).filter_map(|c| c.to_digit(10)).collect();
    let number = |from: usize, len: usize| digits.get(from..from + len).map(|d| d.iter().fold(0, |n, d| n * 10 + d));
    let (year, month, day) = (number(0, 4)?, number(4, 2)?, number(6, 2)?);
    if !(1900..=9999).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (hour, minute, second) = (number(8, 2).unwrap_or(0), number(10, 2).unwrap_or(0), number(12, 2).unwrap_or(0));
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    Some(format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}"))
}

impl Video {
    /// Read the headers of the video in `source`.
    pub fn open(source: impl ByteSource + Send + Sync + 'static) -> Result<Self> {
        let source: Source = Arc::new(source);
        // 8-bit sound under the code the demuxer mistakes for video: shown to it under its other name
        let renamed = boxes::raw_sound_entries(&*source);
        let source: Source = if renamed.is_empty() { source } else { Arc::new(boxes::Renamed { inner: source, at: renamed }) };
        guard("reading the video", || {
            let file = filmcraft_isobmff::open(&*source)?;
            let track = file
                .tracks
                .iter()
                .position(|t| t.kind == TrackKind::Video && t.enabled && !t.samples.is_empty())
                .or_else(|| file.tracks.iter().position(|t| t.kind == TrackKind::Video && !t.samples.is_empty()))
                .ok_or_else(|| Error::Unsupported("no video track".into()))?;
            let t = file.tracks.get(track).ok_or_else(|| Error::Malformed("no video track".into()))?;
            let (width, height) = match t.video() {
                Some(v) if v.width > 0 && v.height > 0 => (u32::from(v.width), u32::from(v.height)),
                _ => (t.width, t.height),
            };
            if width == 0 || height == 0 {
                return Err(Error::Malformed("the picture has no size".into()));
            }
            if width > MAX_SIDE || height > MAX_SIDE || u64::from(width) * u64::from(height) > MAX_PIXELS {
                return Err(Error::Limit(format!("{width} x {height} pixels")));
            }
            let rotation = t.display_rotation().unwrap_or(0) % 4;
            let (width, height) = if rotation % 2 == 1 { (height, width) } else { (width, height) };
            let seconds = |ticks: u64, scale: u32| if scale == 0 { 0.0 } else { ticks as f64 / f64::from(scale) };
            let track_seconds = seconds(t.duration, t.timescale);
            let duration = if track_seconds > 0.0 { track_seconds } else { seconds(file.duration, file.timescale) };
            let frames = t.samples.len();
            let meta = |keys: &[&str]| keys.iter().find_map(|k| file.metadata.get(k)).map(str::trim).filter(|v| !v.is_empty());
            let created =
                meta(&["com.apple.quicktime.creationdate", "©day"]).and_then(recorded).or_else(|| boxes::created(&*source).and_then(boxes::iso));
            let codec = codec_of(t);
            let preview = boxes::preview(&*source);
            let audio = audio::describe(&file);
            let info = Info {
                width,
                height,
                rotation,
                duration,
                frame_rate: if track_seconds > 0.0 { frames as f64 / track_seconds } else { 0.0 },
                frames,
                playable: playable(&codec),
                codec,
                has_audio: file.tracks.iter().any(|t| t.kind == TrackKind::Audio),
                audio: audio.as_ref().map(|(_, info)| info.clone()),
                created,
                make: meta(&["com.apple.quicktime.make", "©mak"]).map(str::to_string),
                model: meta(&["com.apple.quicktime.model", "©mod"]).map(str::to_string),
                has_preview: preview.is_some(),
            };
            Ok(Video { inner: Arc::new(Inner { source: source.clone(), file, track, info, preview, audio }) })
        })
    }

    /// [`Video::open`] for a file on disk (the file stays open while the video or a reader lives).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn open_path(path: impl AsRef<std::path::Path>) -> Result<Self> {
        Self::open(std::fs::File::open(path).map_err(|e| Error::Io(e.to_string()))?)
    }

    pub fn info(&self) -> &Info {
        &self.inner.info
    }

    /// The preview image the file carries (cover art, or a camera's thumbnail): the bytes of a
    /// JPEG or PNG, for an image decoder.
    pub fn embedded_preview(&self) -> Result<Option<Vec<u8>>> {
        let Some((offset, len)) = self.inner.preview else { return Ok(None) };
        let len = usize::try_from(len.min(boxes::MAX_PREVIEW)).map_err(|_| Error::Limit("preview image".into()))?;
        let mut bytes = vec![0u8; len];
        self.inner.source.read_at(offset, &mut bytes).map_err(|e| Error::Io(e.to_string()))?;
        Ok(Some(bytes))
    }

    /// The frames of the video, from its start.
    pub fn reader(&self) -> Result<Reader> {
        Reader::new(self.inner.clone())
    }

    /// The sound of the video, from its start. An error when there is none, or none this build
    /// can decode ([`AudioInfo::playable`]).
    pub fn audio_reader(&self) -> Result<AudioReader> {
        AudioReader::new(self.inner.clone())
    }

    /// The first picture of the video, its long edge reduced towards `max_edge` when given.
    pub fn poster_frame(&self, max_edge: Option<u32>) -> Result<Frame> {
        let mut reader = self.reader()?;
        reader.set_max_edge(max_edge);
        reader.next_frame()?.ok_or_else(|| Error::Malformed("the video has no pictures".into()))
    }
}

enum Decoder {
    H264(Box<filmcraft_h264::Decoder>),
    #[cfg(feature = "hevc")]
    Hevc(Box<filmcraft_hevc::Decoder>),
    /// Motion JPEG: every sample is a picture by itself, so there is nothing to keep between them.
    Jpeg,
}

fn decoder(track: &Track) -> Result<Decoder> {
    match track.codec() {
        Some(CodecConfig::Avc(config)) => {
            filmcraft_h264::Decoder::from_avcc(&config.to_bytes()).map(|d| Decoder::H264(Box::new(d))).map_err(|e| Error::Malformed(e.to_string()))
        }
        #[cfg(feature = "hevc")]
        Some(CodecConfig::Hevc(config)) => {
            filmcraft_hevc::Decoder::from_hvcc(&config.to_bytes()).map(|d| Decoder::Hevc(Box::new(d))).map_err(|e| Error::Malformed(e.to_string()))
        }
        #[cfg(not(feature = "hevc"))]
        Some(CodecConfig::Hevc(_)) => Err(Error::Unsupported("HEVC video isn't included in this build".into())),
        Some(CodecConfig::Jpeg { .. }) if codec_of(track) == Codec::MotionJpeg => Ok(Decoder::Jpeg),
        _ => Err(Error::Unsupported(format!("no decoder for {} video", codec_of(track).name()))),
    }
}

/// A decoder's picture with its presentation time (track timescale).
struct Picture {
    pts: i64,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

/// The largest whole reduction (1, 2, 4 or 8) that keeps the long edge at `max_edge` or more.
fn divisor(width: u32, height: u32, max_edge: Option<u32>) -> u32 {
    let Some(max_edge) = max_edge.filter(|e| *e > 0) else { return 1 };
    let long = width.max(height);
    [8, 4, 2].into_iter().find(|d| long / d >= max_edge).unwrap_or(1)
}

fn h264_error(e: filmcraft_h264::Error) -> Error {
    match e {
        filmcraft_h264::Error::Unsupported(why) => Error::Unsupported(why),
        other => Error::Malformed(other.to_string()),
    }
}

#[cfg(feature = "hevc")]
fn hevc_error(e: filmcraft_hevc::Error) -> Error {
    match e {
        filmcraft_hevc::Error::Unsupported(why) => Error::Unsupported(why),
        other => Error::Malformed(other.to_string()),
    }
}

fn checked(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE || u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(Error::Limit(format!("a {width} x {height} picture")));
    }
    Ok(())
}

/// One Motion JPEG sample → its picture.
fn jpeg_picture(data: &[u8], pts: i64, rotation: u8, max_edge: Option<u32>) -> Result<Picture> {
    use zune_core::colorspace::ColorSpace;
    use zune_core::options::DecoderOptions;
    let side = MAX_SIDE as usize;
    let options = DecoderOptions::default().set_max_width(side).set_max_height(side).set_strict_mode(false).jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(zune_core::bytestream::ZCursor::new(data), options);
    let malformed = |e: zune_jpeg::errors::DecodeErrors| Error::Malformed(format!("a frame of the video: {e}"));
    // the size is checked before the picture is allocated
    decoder.decode_headers().map_err(malformed)?;
    let info = decoder.info().ok_or_else(|| Error::Malformed("a frame of the video has no size".into()))?;
    let (width, height) = (u32::from(info.width), u32::from(info.height));
    checked(width, height)?;
    let pixels = decoder.decode().map_err(malformed)?;
    let channels = pixels.len() / (width as usize * height as usize);
    if !matches!(channels, 1 | 3 | 4) {
        return Err(Error::Malformed("a frame of the video is incomplete".into()));
    }
    let (width, height, rgba) = convert::packed_to_rgba(&pixels, channels, width, height, rotation, divisor(width, height, max_edge));
    Ok(Picture { pts, width, height, rgba })
}

impl Decoder {
    fn set_draft(&mut self, on: bool) {
        match self {
            Decoder::H264(d) => d.set_draft(on),
            #[cfg(feature = "hevc")]
            Decoder::Hevc(d) => d.set_draft(on),
            Decoder::Jpeg => {}
        }
    }

    /// Decode one sample (or, with `None`, what the decoder still holds) into display pictures.
    fn decode(&mut self, sample: Option<(&[u8], i64)>, rotation: u8, max_edge: Option<u32>) -> Result<Vec<Picture>> {
        match self {
            Decoder::H264(d) => {
                let pictures = match sample {
                    Some((data, pts)) => d.decode(data, pts).map_err(h264_error)?,
                    None => d.flush(),
                };
                pictures
                    .iter()
                    .map(|p| {
                        checked(p.width, p.height)?;
                        let yuv = Yuv {
                            width: p.width,
                            height: p.height,
                            chroma_width: p.chroma_width,
                            chroma_height: p.chroma_height,
                            y: Samples::U8(&p.y),
                            u: Samples::U8(&p.u),
                            v: Samples::U8(&p.v),
                            y_stride: p.y_stride,
                            uv_stride: p.uv_stride,
                            bit_depth: 8,
                            matrix: p.color.matrix,
                            full_range: p.color.full_range,
                        };
                        let (width, height, rgba) = convert::to_rgba(&yuv, rotation, divisor(p.width, p.height, max_edge));
                        Ok(Picture { pts: p.pts, width, height, rgba })
                    })
                    .collect()
            }
            #[cfg(feature = "hevc")]
            Decoder::Hevc(d) => {
                let pictures = match sample {
                    Some((data, pts)) => d.decode(data, pts).map_err(hevc_error)?,
                    None => d.flush(),
                };
                fn samples(plane: &filmcraft_hevc::Plane) -> Samples<'_> {
                    match plane {
                        filmcraft_hevc::Plane::U8(s) => Samples::U8(s),
                        filmcraft_hevc::Plane::U16(s) => Samples::U16(s),
                    }
                }
                pictures
                    .iter()
                    .map(|p| {
                        checked(p.width, p.height)?;
                        let yuv = Yuv {
                            width: p.width,
                            height: p.height,
                            chroma_width: p.chroma_width,
                            chroma_height: p.chroma_height,
                            y: samples(&p.y),
                            u: samples(&p.u),
                            v: samples(&p.v),
                            y_stride: p.y_stride,
                            uv_stride: p.uv_stride,
                            bit_depth: p.bit_depth,
                            matrix: p.color.matrix,
                            full_range: p.color.full_range,
                        };
                        let (width, height, rgba) = convert::to_rgba(&yuv, rotation, divisor(p.width, p.height, max_edge));
                        Ok(Picture { pts: p.pts, width, height, rgba })
                    })
                    .collect()
            }
            Decoder::Jpeg => match sample.map(|(data, pts)| jpeg_picture(data, pts, rotation, max_edge)) {
                Some(Ok(picture)) => Ok(vec![picture]),
                // frames don't depend on each other: a damaged one is passed over, the rest play
                Some(Err(Error::Malformed(_))) | None => Ok(Vec::new()),
                Some(Err(e)) => Err(e),
            },
        }
    }
}

/// Reads the frames of a [`Video`] in the order they are shown. Owns its decoder, so it can
/// live on a worker thread.
pub struct Reader {
    video: Arc<Inner>,
    /// `None` once the decoder failed: every later call reports `failed`.
    decoder: Option<Decoder>,
    failed: Option<Error>,
    /// The next sample to decode (file order).
    next: usize,
    ready: VecDeque<Frame>,
    /// After a seek: pictures shown before this time (track timescale) are decoded, not returned.
    from: Option<i64>,
    finished: bool,
    max_edge: Option<u32>,
    draft: bool,
}

impl Reader {
    fn new(video: Arc<Inner>) -> Result<Self> {
        let decoder = guard("starting the video decoder", || decoder(track(&video)?))?;
        Ok(Reader {
            video,
            decoder: Some(decoder),
            failed: None,
            next: 0,
            ready: VecDeque::new(),
            from: None,
            finished: false,
            max_edge: None,
            draft: false,
        })
    }

    /// Reduce frames by a whole factor so their long edge stays at `max_edge` or more (a 4K
    /// video for a small view): cheaper to convert and to draw. `None`: full size.
    pub fn set_max_edge(&mut self, max_edge: Option<u32>) {
        self.max_edge = max_edge;
    }

    /// Let the decoder skip its finishing filters on pictures nothing else depends on: faster,
    /// slightly approximate. For playback, never for a still.
    pub fn set_draft(&mut self, on: bool) {
        self.draft = on;
        if let Some(d) = self.decoder.as_mut() {
            d.set_draft(on);
        }
    }

    /// Continue from the frame shown at `seconds` (clamped to the video).
    pub fn seek(&mut self, seconds: f64) -> Result<()> {
        let video = self.video.clone();
        let t = track(&video)?;
        let seconds = if seconds.is_finite() { seconds.clamp(0.0, video.info.duration.max(0.0)) } else { 0.0 };
        let time = (seconds * f64::from(t.timescale)) as i64;
        let last = t.samples.len().saturating_sub(1);
        let target = t.sample_at_presentation_time(time).unwrap_or(last).min(last);
        let key = t.sync_sample_before(target).min(target);
        let mut fresh = guard("starting the video decoder", || decoder(t))?;
        fresh.set_draft(self.draft);
        self.decoder = Some(fresh);
        self.failed = None;
        self.next = key;
        self.ready.clear();
        self.finished = false;
        self.from = t.presentation_pts(target);
        Ok(())
    }

    /// The next frame, or `None` after the last one.
    pub fn next_frame(&mut self) -> Result<Option<Frame>> {
        let mut barren = 0usize;
        loop {
            if let Some(frame) = self.ready.pop_front() {
                return Ok(Some(frame));
            }
            if let Some(e) = &self.failed {
                return Err(e.clone());
            }
            if self.finished {
                return Ok(None);
            }
            if barren > MAX_BARREN_SAMPLES {
                return Err(self.fail(Error::Malformed("no picture could be decoded".into())));
            }
            barren += 1;
            if let Err(e) = self.step() {
                return Err(self.fail(e));
            }
        }
    }

    fn fail(&mut self, e: Error) -> Error {
        // a decoder that failed (or panicked) is in an unknown state: it is not used again
        self.decoder = None;
        self.failed = Some(e.clone());
        e
    }

    /// Decode the next sample, or drain the decoder after the last one.
    fn step(&mut self) -> Result<()> {
        let video = self.video.clone();
        let t = track(&video)?;
        let (rotation, max_edge, from) = (video.info.rotation, self.max_edge, self.from);
        let Some(decoder) = self.decoder.as_mut() else { return Err(Error::Malformed("the video decoder stopped".into())) };
        let pictures = match t.samples.get(self.next) {
            Some(sample) => {
                if sample.size > MAX_SAMPLE {
                    return Err(Error::Limit("a frame of the video".into()));
                }
                let pts = t.presentation_pts(self.next).unwrap_or(sample.pts);
                let index = self.next;
                self.next += 1;
                guard("decoding the video", || {
                    let data = video.file.read_sample(&*video.source, video.track, index)?;
                    decoder.decode(Some((&data, pts)), rotation, max_edge)
                })?
            }
            None => {
                self.finished = true;
                guard("decoding the video", || decoder.decode(None, rotation, max_edge))?
            }
        };
        let scale = f64::from(t.timescale.max(1));
        for p in pictures {
            if from.is_some_and(|from| p.pts < from) {
                continue;
            }
            self.ready.push_back(Frame { width: p.width, height: p.height, rgba: p.rgba, time: (p.pts as f64 / scale).max(0.0) });
        }
        Ok(())
    }
}

fn track(video: &Inner) -> Result<&Track> {
    video.file.tracks.get(video.track).ok_or_else(|| Error::Malformed("no video track".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{Clip, Sound, clip, grey, sound_at};

    fn small() -> Clip {
        Clip { width: 64, height: 48, frames: 12, fps: 24, keyint: 5, ..Clip::default() }
    }

    /// The centre of the bottom right quarter: plain grey in every frame.
    fn level(frame: &Frame) -> i32 {
        let (x, y) = (frame.width * 3 / 4, frame.height * 3 / 4);
        i32::from(frame.rgba[((y * frame.width + x) * 4 + 1) as usize])
    }

    /// Is the pixel at this fraction of the frame the red marker of the stored top left quarter?
    fn red(frame: &Frame, fx: f32, fy: f32) -> bool {
        let (x, y) = ((frame.width as f32 * fx) as u32, (frame.height as f32 * fy) as u32);
        let px = &frame.rgba[((y * frame.width + x) * 4) as usize..][..3];
        px[0] > px[1].saturating_add(40) && px[0] > px[2].saturating_add(40)
    }

    #[test]
    fn headers_describe_the_video() {
        let video = Video::open(clip(&small()).unwrap()).unwrap();
        let info = video.info();
        assert_eq!((info.width, info.height, info.rotation, info.frames), (64, 48, 0, 12));
        assert_eq!((info.codec.clone(), info.playable, info.has_audio, info.has_preview), (Codec::H264, true, false, false));
        assert!((info.duration - 0.5).abs() < 1e-6, "{}", info.duration);
        assert!((info.frame_rate - 24.0).abs() < 1e-6, "{}", info.frame_rate);
        assert_eq!(video.embedded_preview().unwrap(), None);
    }

    #[test]
    fn every_frame_comes_out_in_order_with_its_time_and_picture() {
        let video = Video::open(clip(&small()).unwrap()).unwrap();
        let mut reader = video.reader().unwrap();
        let mut frames = Vec::new();
        while let Some(frame) = reader.next_frame().unwrap() {
            frames.push(frame);
        }
        assert_eq!(frames.len(), 12);
        for (i, frame) in frames.iter().enumerate() {
            assert_eq!((frame.width, frame.height, frame.rgba.len()), (64, 48, 64 * 48 * 4));
            assert!((frame.time - i as f64 / 24.0).abs() < 1e-6, "frame {i} at {}", frame.time);
            assert!((level(frame) - grey(i as u32)).abs() <= 8, "frame {i}: {} for {}", level(frame), grey(i as u32));
            assert!(red(frame, 0.25, 0.25) && !red(frame, 0.75, 0.25) && !red(frame, 0.25, 0.75), "frame {i}");
        }
        assert_eq!(reader.next_frame().unwrap(), None, "and stays finished");
    }

    #[test]
    fn rotated_video_is_shown_upright() {
        for (rotation, at) in [(1u8, (0.75, 0.25)), (2, (0.75, 0.75)), (3, (0.25, 0.75))] {
            let video = Video::open(clip(&Clip { rotation, ..small() }).unwrap()).unwrap();
            let info = video.info();
            let turned = rotation % 2 == 1;
            assert_eq!((info.width, info.height, info.rotation), if turned { (48, 64, rotation) } else { (64, 48, rotation) });
            let frame = video.poster_frame(None).unwrap();
            assert_eq!((frame.width, frame.height), (info.width, info.height));
            assert!(red(&frame, at.0, at.1), "rotation {rotation}: the marker is at {at:?}");
            assert!(!red(&frame, 1.0 - at.0, 1.0 - at.1), "rotation {rotation}");
        }
    }

    #[test]
    fn seeking_lands_on_the_frame_shown_then_and_plays_on() {
        let video = Video::open(clip(&small()).unwrap()).unwrap();
        let mut reader = video.reader().unwrap();
        // frame 7 is two frames after a key frame (key frames at 0, 5, 10)
        for target in [7usize, 0, 11, 5] {
            reader.seek(target as f64 / 24.0 + 0.001).unwrap();
            let frame = reader.next_frame().unwrap().unwrap();
            assert!((frame.time - target as f64 / 24.0).abs() < 1e-6, "sought {target}, got {}", frame.time);
            assert!((level(&frame) - grey(target as u32)).abs() <= 8, "sought {target}");
            if target < 11 {
                let after = reader.next_frame().unwrap().unwrap();
                assert!((after.time - (target + 1) as f64 / 24.0).abs() < 1e-6);
            }
        }
        // out of range and not-a-number times are clamped, never an error
        for odd in [-5.0, 1e9, f64::NAN, f64::INFINITY] {
            reader.seek(odd).unwrap();
            assert!(reader.next_frame().unwrap().is_some());
        }
    }

    #[test]
    fn frames_can_be_reduced_for_a_small_view() {
        let video = Video::open(clip(&small()).unwrap()).unwrap();
        let frame = video.poster_frame(Some(16)).unwrap();
        assert_eq!((frame.width, frame.height), (16, 12), "a quarter: the long edge stays at 16 or more");
        assert_eq!(video.poster_frame(Some(60)).unwrap().width, 64, "never below what was asked for");
        assert!(red(&frame, 0.25, 0.25) && !red(&frame, 0.75, 0.75));
    }

    #[test]
    fn the_embedded_preview_is_found_and_returned() {
        let cover = [&[0xff, 0xd8, 0xff, 0xe0][..], &[7u8; 200]].concat();
        let video = Video::open(clip(&Clip { cover: Some(cover.clone()), ..small() }).unwrap()).unwrap();
        assert!(video.info().has_preview);
        assert_eq!(video.embedded_preview().unwrap(), Some(cover));
        // and the video still plays
        assert!(video.poster_frame(None).is_ok());
    }

    #[test]
    fn recorded_dates_are_normalised() {
        assert_eq!(recorded("2024-05-01T10:11:12+0200").as_deref(), Some("2024-05-01T10:11:12"));
        assert_eq!(recorded("2024:05:01 10:11:12").as_deref(), Some("2024-05-01T10:11:12"));
        assert_eq!(recorded("2024-05-01").as_deref(), Some("2024-05-01T00:00:00"));
        assert_eq!(recorded("2024"), None);
        assert_eq!(recorded("2024-13-01"), None);
        assert_eq!(recorded("not a date"), None);
        assert_eq!(recorded(""), None);
    }

    #[test]
    fn damaged_files_are_errors_never_panics() {
        assert!(matches!(Video::open(b"not a video at all".to_vec()), Err(Error::Malformed(_) | Error::Unsupported(_))));
        assert!(Video::open(Vec::new()).is_err());
        let good = clip(&small()).unwrap();
        let play = |bytes: Vec<u8>| {
            if let Ok(video) = Video::open(bytes) {
                let _ = video.embedded_preview();
                if let Ok(mut reader) = video.reader() {
                    let _ = reader.seek(0.2);
                    for _ in 0..16 {
                        if !matches!(reader.next_frame(), Ok(Some(_))) {
                            break;
                        }
                    }
                }
            }
        };
        // cut short anywhere
        for cut in (0..good.len()).step_by(good.len() / 97 + 1) {
            play(good[..cut].to_vec());
        }
        // and with bytes changed throughout: headers, tables and picture data
        let mut seed = 0x9e37_79b9u32;
        for round in 0..300 {
            let mut bad = good.clone();
            for _ in 0..=(round % 4) {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let at = seed as usize % bad.len();
                bad[at] = (seed >> 24) as u8;
            }
            play(bad);
        }
    }

    #[test]
    fn a_reader_reports_its_failure_again_instead_of_decoding_on() {
        let mut bytes = clip(&small()).unwrap();
        // wreck the picture data of every frame, leaving the headers intact
        let mdat = bytes.windows(4).position(|w| w == b"mdat").unwrap();
        let moov = bytes.windows(4).position(|w| w == b"moov").unwrap();
        for b in &mut bytes[mdat + 8..moov - 4] {
            *b = 0xff;
        }
        let video = Video::open(bytes).unwrap();
        let mut reader = video.reader().unwrap();
        let first = reader.next_frame();
        assert!(first.is_err(), "{first:?}");
        assert_eq!(reader.next_frame(), first);
    }

    fn motion_jpeg() -> Clip {
        Clip { motion_jpeg: true, quicktime: true, ..small() }
    }

    /// Motion JPEG (older digital cameras) had no decoder: such a file opened as "jpeg",
    /// unplayable and without a poster.
    #[test]
    fn motion_jpeg_plays_frame_by_frame() {
        let video = Video::open(clip(&motion_jpeg()).unwrap()).unwrap();
        let info = video.info();
        assert_eq!((info.codec.clone(), info.codec.name(), info.playable), (Codec::MotionJpeg, "Motion JPEG", true));
        assert_eq!((info.width, info.height, info.rotation, info.frames), (64, 48, 0, 12));
        assert!((info.duration - 0.5).abs() < 1e-6, "{}", info.duration);
        let mut reader = video.reader().unwrap();
        reader.set_draft(true);
        let mut frames = Vec::new();
        while let Some(frame) = reader.next_frame().unwrap() {
            frames.push(frame);
        }
        assert_eq!(frames.len(), 12);
        for (i, frame) in frames.iter().enumerate() {
            assert_eq!((frame.width, frame.height, frame.rgba.len()), (64, 48, 64 * 48 * 4));
            assert!((frame.time - i as f64 / 24.0).abs() < 1e-6, "frame {i} at {}", frame.time);
            assert!((level(frame) - grey(i as u32)).abs() <= 8, "frame {i}: {} for {}", level(frame), grey(i as u32));
            assert!(red(frame, 0.25, 0.25) && !red(frame, 0.75, 0.25) && !red(frame, 0.25, 0.75), "frame {i}");
        }
        assert_eq!(reader.next_frame().unwrap(), None, "and stays finished");
        // every frame is a key frame: a seek lands on any of them
        for target in [7usize, 0, 11, 4] {
            reader.seek(target as f64 / 24.0 + 0.001).unwrap();
            let frame = reader.next_frame().unwrap().unwrap();
            assert!((frame.time - target as f64 / 24.0).abs() < 1e-6, "sought {target}, got {}", frame.time);
            assert!((level(&frame) - grey(target as u32)).abs() <= 8, "sought {target}");
        }
    }

    #[test]
    fn motion_jpeg_is_turned_and_reduced_like_the_rest() {
        for (rotation, at) in [(1u8, (0.75, 0.25)), (2, (0.75, 0.75)), (3, (0.25, 0.75))] {
            let video = Video::open(clip(&Clip { rotation, ..motion_jpeg() }).unwrap()).unwrap();
            let frame = video.poster_frame(None).unwrap();
            assert_eq!((frame.width, frame.height), if rotation % 2 == 1 { (48, 64) } else { (64, 48) });
            assert!(red(&frame, at.0, at.1) && !red(&frame, 1.0 - at.0, 1.0 - at.1), "rotation {rotation}");
        }
        let video = Video::open(clip(&motion_jpeg()).unwrap()).unwrap();
        let frame = video.poster_frame(Some(16)).unwrap();
        assert_eq!((frame.width, frame.height), (16, 12));
        assert!(red(&frame, 0.25, 0.25) && !red(&frame, 0.75, 0.75));
    }

    /// The bytes of sample `index` of the file's video track, as (offset, length).
    fn sample_span(bytes: &[u8], index: usize) -> (usize, usize) {
        let file = filmcraft_isobmff::open(bytes.to_vec()).unwrap();
        let sample = &file.tracks[0].samples[index];
        (sample.offset as usize, sample.size as usize)
    }

    #[test]
    fn a_damaged_motion_jpeg_frame_is_passed_over() {
        let mut bytes = clip(&motion_jpeg()).unwrap();
        // frames 0 and 3 are not pictures any more
        for index in [0, 3] {
            let (at, len) = sample_span(&bytes, index);
            bytes[at..at + len].fill(0x55);
        }
        let video = Video::open(bytes.clone()).unwrap();
        let poster = video.poster_frame(None).unwrap();
        assert!((poster.time - 1.0 / 24.0).abs() < 1e-6, "the first picture that decodes: {}", poster.time);
        let mut reader = video.reader().unwrap();
        let mut times = Vec::new();
        while let Some(frame) = reader.next_frame().unwrap() {
            times.push((frame.time * 24.0).round() as u32);
        }
        assert_eq!(times, [1, 2, 4, 5, 6, 7, 8, 9, 10, 11]);
        // with no frame left the video has no pictures: an error, not a panic or an endless loop
        let (first, _) = sample_span(&bytes, 0);
        let (last, len) = sample_span(&bytes, 11);
        bytes[first..last + len].fill(0x55);
        let video = Video::open(bytes).unwrap();
        assert!(matches!(video.poster_frame(None), Err(Error::Malformed(_))));
        assert_eq!(video.reader().unwrap().next_frame(), Ok(None));
    }

    #[test]
    fn damaged_motion_jpeg_files_are_errors_never_panics() {
        let good = clip(&Clip { sound: Some(Sound::Pcm8), ..motion_jpeg() }).unwrap();
        let play = |bytes: Vec<u8>| {
            if let Ok(video) = Video::open(bytes)
                && let Ok(mut reader) = video.reader()
            {
                let _ = reader.seek(0.2);
                for _ in 0..16 {
                    if !matches!(reader.next_frame(), Ok(Some(_))) {
                        break;
                    }
                }
            }
        };
        for cut in (0..good.len()).step_by(good.len() / 97 + 1) {
            play(good[..cut].to_vec());
        }
        let mut seed = 0x1234_5678u32;
        for round in 0..300 {
            let mut bad = good.clone();
            for _ in 0..=(round % 4) {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let at = seed as usize % bad.len();
                bad[at] = (seed >> 24) as u8;
            }
            play(bad);
        }
    }

    /// A clip from an older camera: Motion JPEG with 8-bit sound under the QuickTime code `raw `.
    /// The demuxer took that sound for video and refused the file ("truncated data while reading
    /// sample entry"), so it could not even be imported.
    #[test]
    fn a_clip_with_raw_8_bit_sound_opens_and_plays() {
        for clip_ in [Clip { sound: Some(Sound::Pcm8), ..motion_jpeg() }, Clip { sound: Some(Sound::Pcm8), quicktime: true, ..small() }] {
            let bytes = clip(&clip_).unwrap();
            assert!(bytes.windows(4).any(|w| w == b"raw "), "the sound is stored as `raw `");
            assert!(filmcraft_isobmff::open(&bytes).is_err(), "the demuxer alone still refuses it: the workaround is needed");
            let video = Video::open(bytes).unwrap();
            let info = video.info();
            assert_eq!((info.width, info.height, info.frames, info.playable, info.has_audio), (64, 48, 12, true, true));
            let mut reader = video.reader().unwrap();
            let mut frames = 0;
            while let Some(frame) = reader.next_frame().unwrap() {
                assert!((level(&frame) - grey(frames)).abs() <= 8, "frame {frames}");
                frames += 1;
            }
            assert_eq!(frames, 12);
        }
        // the same sound in a file that never needed help is left alone
        let video = Video::open(clip(&Clip { sound: Some(Sound::Pcm8), ..small() }).unwrap()).unwrap();
        assert!(video.info().has_audio);
    }

    /// Every sample of a clip's sound, with the time of the first one.
    fn all_sound(video: &Video) -> (Vec<f32>, f64) {
        let mut reader = video.audio_reader().unwrap();
        let (rate, channels) = (f64::from(reader.sample_rate()), reader.channels());
        let (mut samples, mut start) = (Vec::new(), None);
        while let Some(block) = reader.next_block().unwrap() {
            assert_eq!(block.samples.len() % channels, 0);
            let expected = *start.get_or_insert(block.time) + (samples.len() / channels) as f64 / rate;
            assert!((block.time - expected).abs() < 0.5 / rate, "blocks follow each other: {} for {expected}", block.time);
            samples.extend(block.samples);
        }
        (samples, start.unwrap())
    }

    /// How loud `samples` are (root mean square) on one channel between two times.
    fn loudness(samples: &[f32], sound: Sound, channel: usize, from: f64, to: f64) -> f32 {
        let (rate, channels) = (f64::from(sound.rate()), sound.channels());
        let frames = &samples[(from * rate) as usize * channels..(to * rate) as usize * channels];
        let values: Vec<f32> = frames.chunks(channels).map(|f| f[channel]).collect();
        (values.iter().map(|v| v * v).sum::<f32>() / values.len() as f32).sqrt()
    }

    /// A video's sound was only reported as present. Its track is described, and its samples
    /// come out in order at the right times: silence, then the tone, in every format.
    #[test]
    fn the_sound_of_a_clip_is_described_and_read_in_order() {
        for sound in [Sound::Pcm8, Sound::Pcm16, Sound::Aac] {
            for quicktime in [false, true] {
                let video = Video::open(clip(&Clip { sound: Some(sound), quicktime, ..small() }).unwrap()).unwrap();
                let info = video.info().audio.clone().unwrap();
                let codec = if sound == Sound::Aac { AudioCodec::Aac } else { AudioCodec::Pcm };
                assert_eq!(
                    (info.codec.clone(), info.sample_rate, usize::from(info.channels), info.playable),
                    (codec, sound.rate(), sound.channels(), true)
                );
                assert!((info.duration - 0.5).abs() < 0.05, "{sound:?}: {}", info.duration);
                assert!(video.info().has_audio);
                let (samples, start) = all_sound(&video);
                let (rate, channels) = (f64::from(sound.rate()), sound.channels());
                assert!(start.abs() < 1.0 / rate, "{sound:?}: the sound starts with the picture, not {start}");
                let frames = samples.len() / channels;
                assert!((frames as f64 / rate - 0.5).abs() < 0.05, "{sound:?}: {frames} frames");
                if sound == Sound::Aac {
                    // lossy: the silence is quiet, the tone is there, the right channel at half its level
                    assert!(loudness(&samples, sound, 0, 0.02, 0.2) < 0.01, "{}", loudness(&samples, sound, 0, 0.02, 0.2));
                    let (left, right) = (loudness(&samples, sound, 0, 0.3, 0.45), loudness(&samples, sound, 1, 0.3, 0.45));
                    assert!((left - 0.3536).abs() < 0.03 && (right - 0.1768).abs() < 0.02, "left {left}, right {right}");
                } else {
                    let step = if sound == Sound::Pcm8 { 1.0 / 127.0 } else { 1.0 / 32_000.0 };
                    for (i, frame) in samples.chunks(channels).enumerate() {
                        let want = sound_at(i as f64 / rate);
                        assert!((frame[0] - want).abs() <= step, "{sound:?} frame {i}: {} for {want}", frame[0]);
                        if channels == 2 {
                            assert!((frame[1] - want * 0.5).abs() <= step, "{sound:?} frame {i}, right");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn seeking_the_sound_lands_on_the_sample_heard_then() {
        for sound in [Sound::Pcm8, Sound::Pcm16, Sound::Aac] {
            let video = Video::open(clip(&Clip { sound: Some(sound), ..small() }).unwrap()).unwrap();
            let mut reader = video.audio_reader().unwrap();
            let (rate, channels) = (f64::from(sound.rate()), sound.channels());
            for target in [0.3012, 0.1, 0.0, 0.26] {
                reader.seek(target).unwrap();
                let block = reader.next_block().unwrap().unwrap();
                assert!((block.time - target).abs() <= 1.0 / rate, "{sound:?}: sought {target}, got {}", block.time);
                // enough of it to judge: the rest of that tenth of a second
                let mut samples = block.samples;
                while samples.len() < (0.04 * rate) as usize * channels {
                    samples.extend(reader.next_block().unwrap().unwrap().samples);
                }
                if sound == Sound::Aac {
                    let heard =
                        (samples.chunks(channels).take((0.04 * rate) as usize).map(|f| f[0] * f[0]).sum::<f32>() / (0.04 * rate as f32)).sqrt();
                    assert_eq!(heard > 0.2, target > 0.25, "{sound:?} at {target}: {heard}");
                } else {
                    let step = if sound == Sound::Pcm8 { 1.0 / 127.0 } else { 1.0 / 32_000.0 };
                    for (i, frame) in samples.chunks(channels).take(200).enumerate() {
                        let want = sound_at(block.time + i as f64 / rate);
                        assert!((frame[0] - want).abs() <= step, "{sound:?} at {target}, frame {i}: {} for {want}", frame[0]);
                    }
                }
            }
            // past the end there is nothing; odd times are clamped, never an error
            reader.seek(1e9).unwrap();
            let rest: usize = std::iter::from_fn(|| reader.next_block().unwrap()).map(|b| b.samples.len()).sum();
            assert!(rest <= 2048 * channels, "{sound:?}: {rest}");
            for odd in [-5.0, f64::NAN, f64::INFINITY] {
                reader.seek(odd).unwrap();
                assert!(reader.next_block().unwrap().is_some());
            }
        }
    }

    #[test]
    fn a_clip_without_sound_or_with_sound_nothing_reads_says_so() {
        let video = Video::open(clip(&small()).unwrap()).unwrap();
        assert_eq!((video.info().audio.clone(), video.info().has_audio), (None, false));
        assert_eq!(video.audio_reader().err(), Some(Error::Unsupported("the video has no sound".into())));
        // a format without a decoder here: named, not playable, and the picture still plays
        let mut bytes = clip(&Clip { sound: Some(Sound::Pcm16), quicktime: true, ..small() }).unwrap();
        let moov = bytes.windows(4).position(|w| w == b"moov").unwrap();
        let entry = moov + bytes[moov..].windows(4).position(|w| w == b"twos").unwrap();
        bytes[entry..entry + 4].copy_from_slice(b"ulaw");
        let video = Video::open(bytes).unwrap();
        let info = video.info().audio.clone().unwrap();
        assert_eq!((info.codec, info.playable, info.sample_rate, info.channels), (AudioCodec::Other("ulaw".into()), false, 48_000, 2));
        assert_eq!(video.audio_reader().err(), Some(Error::Unsupported("no decoder for ulaw sound".into())));
        assert!(video.poster_frame(None).is_ok());
    }

    /// A damaged packet of sound is silence for its length; the rest plays, at the right times.
    #[test]
    fn a_damaged_packet_of_sound_is_silence_not_the_end() {
        let mut bytes = clip(&Clip { sound: Some(Sound::Aac), ..small() }).unwrap();
        let file = filmcraft_isobmff::open(bytes.clone()).unwrap();
        let track = file.tracks.iter().find(|t| t.kind == TrackKind::Audio).unwrap();
        // the packets of the tone from 0.32 s to 0.38 s
        for sample in &track.samples[16..19] {
            bytes[sample.offset as usize..][..sample.size as usize].fill(0xff);
        }
        let video = Video::open(bytes).unwrap();
        let (samples, _) = all_sound(&video);
        assert!((samples.len() as f64 / 2.0 / 48_000.0 - 0.5).abs() < 0.05, "nothing is lost: {}", samples.len());
        assert!(loudness(&samples, Sound::Aac, 0, 0.33, 0.36) < 0.01, "the damaged part is silent");
        assert!(loudness(&samples, Sound::Aac, 0, 0.42, 0.47) > 0.3, "and the tone comes back");
        assert!(samples.iter().all(|v| v.is_finite() && v.abs() <= 1.0));
    }

    #[test]
    fn damaged_sound_is_an_error_or_silence_never_a_panic() {
        for sound in [Sound::Pcm8, Sound::Pcm16, Sound::Aac] {
            let good = clip(&Clip { sound: Some(sound), quicktime: sound == Sound::Pcm8, ..small() }).unwrap();
            let listen = |bytes: Vec<u8>| {
                if let Ok(video) = Video::open(bytes)
                    && let Ok(mut reader) = video.audio_reader()
                {
                    let _ = reader.seek(0.2);
                    for _ in 0..64 {
                        match reader.next_block() {
                            Ok(Some(block)) => assert!(block.samples.iter().all(|v| v.is_finite() && v.abs() <= 1.0)),
                            _ => break,
                        }
                    }
                }
            };
            for cut in (0..good.len()).step_by(good.len() / 61 + 1) {
                listen(good[..cut].to_vec());
            }
            let mut seed = 0x0bad_5eedu32;
            for round in 0..200 {
                let mut bad = good.clone();
                for _ in 0..=(round % 4) {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let at = seed as usize % bad.len();
                    bad[at] = (seed >> 24) as u8;
                }
                listen(bad);
            }
        }
    }

    /// Interlaced Motion JPEG (`mjpa`, `mjpb`) is named, and refused with a reason.
    #[test]
    fn interlaced_motion_jpeg_is_a_clear_error() {
        let mut bytes = clip(&motion_jpeg()).unwrap();
        let moov = bytes.windows(4).position(|w| w == b"moov").unwrap();
        let entry = moov + bytes[moov..].windows(4).position(|w| w == b"jpeg").unwrap();
        bytes[entry..entry + 4].copy_from_slice(b"mjpa");
        let video = Video::open(bytes).unwrap();
        assert_eq!((video.info().codec.clone(), video.info().playable), (Codec::Other("mjpa".into()), false));
        assert_eq!(video.reader().err(), Some(Error::Unsupported("no decoder for mjpa video".into())));
    }

    /// Write clips to try in the app: `LC_VIDEO_CLIPS=<folder> cargo test -p lightcraft-video sample_clips`.
    #[test]
    fn sample_clips_are_written_on_request() {
        let Some(dir) = std::env::var_os("LC_VIDEO_CLIPS") else { return };
        let long = Clip { width: 320, height: 240, frames: 120, fps: 24, keyint: 24, ..Clip::default() };
        let clips = [
            ("h264-aac.mp4", Clip { sound: Some(Sound::Aac), ..long.clone() }),
            ("h264-pcm16.mov", Clip { sound: Some(Sound::Pcm16), quicktime: true, ..long.clone() }),
            ("mjpeg-pcm8.mov", Clip { sound: Some(Sound::Pcm8), quicktime: true, motion_jpeg: true, ..long.clone() }),
            ("h264-silent.mp4", long),
        ];
        for (name, c) in clips {
            std::fs::write(std::path::Path::new(&dir).join(name), clip(&c).unwrap()).unwrap();
        }
    }

    /// Open and decode a real file: `LC_VIDEO_FILE=<path> cargo test -p lightcraft-video real_file -- --nocapture`.
    /// Use `--release` for timings.
    #[test]
    fn real_file_opens_and_decodes() {
        let Ok(path) = std::env::var("LC_VIDEO_FILE") else { return };
        let video = Video::open_path(&path).unwrap();
        println!("{path}: {:?}", video.info());
        let mut reader = video.reader().unwrap();
        // as the player asks for them: `LC_VIDEO_EDGE=<pixels>` (the view's long edge), `LC_VIDEO_DRAFT=1`
        reader.set_max_edge(std::env::var("LC_VIDEO_EDGE").ok().and_then(|e| e.parse().ok()));
        reader.set_draft(std::env::var_os("LC_VIDEO_DRAFT").is_some());
        let (started, mut frames) = (std::time::Instant::now(), 0usize);
        // how long each frame took to come out: a decoder gives frames in bursts, and the
        // longest gaps are what a player's queue has to cover
        let (mut last, mut gaps) = (started, Vec::new());
        while let Some(frame) = reader.next_frame().unwrap() {
            assert_eq!(frame.rgba.len(), frame.width as usize * frame.height as usize * 4);
            frames += 1;
            gaps.push(last.elapsed().as_secs_f64() * 1e3);
            last = std::time::Instant::now();
        }
        println!("{frames} of {} frames, {:.1} ms a frame", video.info().frames, started.elapsed().as_secs_f64() * 1e3 / frames.max(1) as f64);
        let slow: Vec<String> = gaps.iter().enumerate().filter(|(_, ms)| **ms > 50.0).take(24).map(|(i, ms)| format!("{i}: {ms:.0}")).collect();
        gaps.sort_by(f64::total_cmp);
        let at = |q: f64| gaps.get(((gaps.len() as f64 - 1.0) * q) as usize).copied().unwrap_or(0.0);
        println!(
            "gaps between frames, ms: median {:.1}, 90 % {:.1}, 99 % {:.1}, longest {:.1}; over 50 ms (frame: ms): {slow:?}",
            at(0.5),
            at(0.9),
            at(0.99),
            at(1.0)
        );
        assert!(frames > 0);
        if video.info().audio.as_ref().is_some_and(|a| a.playable) {
            let mut reader = video.audio_reader().unwrap();
            let (started, mut samples, mut peak) = (std::time::Instant::now(), 0usize, 0.0f32);
            while let Some(block) = reader.next_block().unwrap() {
                samples += block.samples.len();
                peak = block.samples.iter().fold(peak, |p, v| p.max(v.abs()));
            }
            let seconds = samples as f64 / reader.channels() as f64 / f64::from(reader.sample_rate());
            println!("sound: {seconds:.2} s, peak {peak:.3}, decoded in {:.0} ms", started.elapsed().as_secs_f64() * 1e3);
        }
    }

    #[cfg(not(feature = "hevc"))]
    #[test]
    fn hevc_without_its_feature_is_a_clear_error() {
        let bytes = crate::testkit::hevc_headers_only(64, 48).unwrap();
        let video = Video::open(bytes).unwrap();
        assert_eq!((video.info().codec.clone(), video.info().playable), (Codec::Hevc, false));
        assert_eq!(video.reader().err(), Some(Error::Unsupported("HEVC video isn't included in this build".into())));
    }
}
