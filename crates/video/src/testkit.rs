//! Generated test clips (feature `testkit`, for tests only): LightCraft never commits media, so
//! tests here and in the crates above encode a few small frames (H.264, or JPEG pictures for a
//! Motion JPEG clip) and wrap them in an MP4.
//!
//! Every frame is plain grey, a little brighter than the one before ([`grey`]), with the stored
//! picture's top left quarter tinted red: a test can tell which frame it got and which way up.

use std::io::Cursor;

use filmcraft_h264enc::{Encoder, EncoderConfig, PacketFormat, Profile, RateControl, YuvFrame};
use filmcraft_isobmff::{AvcConfig, Brand, HevcConfig, Mp4Writer, PcmConfig, SampleEntry, TrackConfig, WriteSample, WriterOptions};

/// What [`clip`] makes.
#[derive(Debug, Clone)]
pub struct Clip {
    /// Even, and at least 16.
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    pub fps: u32,
    /// Frames from one key frame to the next.
    pub keyint: u32,
    /// Quarter turns clockwise the track is shown with (as a phone held upright records).
    pub rotation: u8,
    /// A cover image to embed (`moov/udta/meta/ilst/covr`): the bytes of a JPEG or PNG.
    pub cover: Option<Vec<u8>>,
    /// A QuickTime `.mov` instead of an `.mp4`.
    pub quicktime: bool,
    /// Motion JPEG (every frame a JPEG picture, all of them key frames) instead of H.264.
    pub motion_jpeg: bool,
    /// A second track of sound, as long as the picture ([`sound_at`] says what is heard when).
    pub sound: Option<Sound>,
}

/// The sound track of a [`Clip`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    /// 8-bit PCM, 8 kHz, one channel. In a QuickTime file it is stored under the code `raw `, as
    /// older cameras did.
    Pcm8,
    /// 16-bit PCM, 48 kHz, two channels (the right one at half the level of the left).
    Pcm16,
    /// AAC, 48 kHz, two channels (the right one at half the level of the left).
    Aac,
}

impl Sound {
    /// Samples a second.
    pub fn rate(self) -> u32 {
        if self == Sound::Pcm8 { 8000 } else { 48_000 }
    }

    pub fn channels(self) -> usize {
        if self == Sound::Pcm8 { 1 } else { 2 }
    }
}

/// What a clip's sound holds at `seconds` (left channel, -1..1): nothing for the first quarter
/// of a second, then a steady 440 Hz tone. A test can tell where in the clip a sample is from.
pub fn sound_at(seconds: f64) -> f32 {
    if seconds < 0.25 { 0.0 } else { (0.5 * (seconds * 440.0 * std::f64::consts::TAU).sin()) as f32 }
}

impl Default for Clip {
    fn default() -> Self {
        Clip { width: 64, height: 48, frames: 12, fps: 24, keyint: 5, rotation: 0, cover: None, quicktime: false, motion_jpeg: false, sound: None }
    }
}

/// Media ticks per frame: the track's timescale is `fps * TICKS`.
const TICKS: u32 = 100;

/// The grey a decoded frame `index` shows outside the red quarter (0..=255, within a few levels).
pub fn grey(index: u32) -> i32 {
    (luma(index) - 16) * 255 / 219
}

fn luma(index: u32) -> i32 {
    (40 + 12 * index as i32).min(220)
}

fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    out
}

/// Byte offset of the first box of type `kind` at the top level of `bytes[from..to]`.
fn top_level(bytes: &[u8], from: usize, to: usize, kind: &[u8; 4]) -> Option<(usize, usize)> {
    let mut at = from;
    while at + 8 <= to {
        let size = u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?) as usize;
        if size < 8 || at + size > to {
            return None;
        }
        if bytes.get(at + 4..at + 8)? == kind {
            return Some((at, size));
        }
        at += size;
    }
    None
}

/// Add `child` at the end of the file's `moov` box (written last by [`Mp4Writer::finish`]).
fn append_to_moov(bytes: &mut Vec<u8>, child: &[u8]) -> Result<(), String> {
    let (at, size) = top_level(bytes, 0, bytes.len(), b"moov").ok_or("the clip has no moov box")?;
    if at + size != bytes.len() {
        return Err("moov is not the last box".into());
    }
    let grown = u32::try_from(size + child.len()).map_err(|_| "moov too large")?;
    bytes.get_mut(at..at + 4).ok_or("moov header")?.copy_from_slice(&grown.to_be_bytes());
    bytes.extend_from_slice(child);
    Ok(())
}

/// Give the (only) track the display matrix of `rotation` quarter turns clockwise.
fn rotate_track(bytes: &mut [u8], rotation: u8) -> Result<(), String> {
    let (moov, moov_size) = top_level(bytes, 0, bytes.len(), b"moov").ok_or("the clip has no moov box")?;
    let (trak, trak_size) = top_level(bytes, moov + 8, moov + moov_size, b"trak").ok_or("the clip has no track")?;
    let (tkhd, _) = top_level(bytes, trak + 8, trak + trak_size, b"tkhd").ok_or("the track has no header")?;
    let version = *bytes.get(tkhd + 8).ok_or("track header")?;
    // after the box and version: times, id and duration (wider in version 1), then layer,
    // alternate group, volume and reserved words
    let matrix = tkhd + 8 + 4 + if version == 1 { 32 } else { 20 } + 8 + 8;
    let one = 0x0001_0000i32;
    let [a, b, c, d] = match rotation % 4 {
        0 => [one, 0, 0, one],
        1 => [0, one, -one, 0],
        2 => [-one, 0, 0, -one],
        _ => [0, -one, one, 0],
    };
    let values = [a, b, 0, c, d, 0, 0, 0, 0x4000_0000];
    for (i, v) in values.iter().enumerate() {
        bytes.get_mut(matrix + i * 4..matrix + i * 4 + 4).ok_or("track matrix")?.copy_from_slice(&v.to_be_bytes());
    }
    Ok(())
}

type Writer = Mp4Writer<Cursor<Vec<u8>>>;

/// The clip's frames as an H.264 track.
fn h264_track(c: &Clip, writer: &mut Writer) -> Result<(), String> {
    let (w, h) = (c.width as usize, c.height as usize);
    let mut config = EncoderConfig::new(c.width, c.height, c.fps, 1);
    config.profile = Profile::Main;
    config.rate = RateControl::Qp(18);
    config.keyint = c.keyint.max(1);
    config.bframes = 0;
    config.scenecut = false;
    config.threads = 1;
    config.format = PacketFormat::LengthPrefixed;
    let mut encoder = Encoder::new(config).map_err(|e| e.to_string())?;
    let (cw, ch) = (w / 2, h / 2);
    let u = vec![128u8; cw * ch];
    let mut v = vec![128u8; cw * ch];
    for row in 0..ch / 2 {
        v.get_mut(row * cw..row * cw + cw / 2).ok_or("chroma plane")?.fill(220);
    }
    let mut packets = Vec::new();
    for i in 0..c.frames {
        let y = vec![luma(i) as u8; w * h];
        let frame = YuvFrame { y: &y, u: &u, v: &v, y_stride: w, uv_stride: cw };
        packets.extend(encoder.encode(&frame, i64::from(i)).map_err(|e| e.to_string())?);
    }
    packets.extend(encoder.flush());

    let avcc = AvcConfig::parse(&encoder.avcc()).map_err(|e| e.to_string())?;
    let entry = SampleEntry::avc(avcc, c.width as u16, c.height as u16);
    let track = writer.add_track(TrackConfig::new(entry, c.fps * TICKS)).map_err(|e| e.to_string())?;
    for p in &packets {
        let offset = i32::try_from((p.pts - p.dts) * i64::from(TICKS)).map_err(|_| "composition offset")?;
        let sample = WriteSample { data: &p.data, duration: TICKS, composition_offset: offset, is_sync: p.keyframe };
        writer.write_sample(track, sample).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The clip's frames as a Motion JPEG track: one JPEG picture a sample.
fn jpeg_track(c: &Clip, writer: &mut Writer) -> Result<(), String> {
    let (w, h) = (c.width as usize, c.height as usize);
    let (width, height) = (u16::try_from(c.width).map_err(|_| "clip width")?, u16::try_from(c.height).map_err(|_| "clip height")?);
    let track = writer.add_track(TrackConfig::new(SampleEntry::jpeg(width, height), c.fps * TICKS)).map_err(|e| e.to_string())?;
    for i in 0..c.frames {
        let level = grey(i).clamp(0, 255) as u8;
        let mut rgb = vec![level; w * h * 3];
        for row in 0..h / 2 {
            for px in rgb.get_mut(row * w * 3..(row * w + w / 2) * 3).ok_or("picture")?.as_chunks_mut::<3>().0 {
                *px = [230, 30, 30];
            }
        }
        let mut data = Vec::new();
        jpeg_encoder::Encoder::new(&mut data, 92).encode(&rgb, width, height, jpeg_encoder::ColorType::Rgb).map_err(|e| e.to_string())?;
        let sample = WriteSample { data: &data, duration: TICKS, composition_offset: 0, is_sync: true };
        writer.write_sample(track, sample).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The clip's sound: as long as its picture.
fn sound_track(c: &Clip, sound: Sound, writer: &mut Writer) -> Result<(), String> {
    let (rate, channels) = (sound.rate(), sound.channels());
    let frames = (u64::from(c.frames) * u64::from(rate) / u64::from(c.fps)) as usize;
    let left: Vec<f32> = (0..frames).map(|i| sound_at(i as f64 / f64::from(rate))).collect();
    if sound == Sound::Aac {
        let right: Vec<f32> = left.iter().map(|v| v * 0.5).collect();
        let mut encoder = filmcraft_aac::Encoder::new(filmcraft_aac::EncoderConfig::cbr(rate, channels, 128_000)).map_err(|e| e.to_string())?;
        let mut packets = encoder.encode(&[&left, &right]);
        packets.extend(encoder.flush());
        let entry = SampleEntry::aac(encoder.audio_specific_config(), channels as u32, rate);
        // the encoder's lead-in is not part of the clip: the track starts after it
        let config = TrackConfig { media_start: Some(i64::from(encoder.priming_samples())), ..TrackConfig::new(entry, rate) };
        let track = writer.add_track(config).map_err(|e| e.to_string())?;
        for data in &packets {
            let sample = WriteSample { data, duration: filmcraft_aac::FRAME_LEN as u32, composition_offset: 0, is_sync: true };
            writer.write_sample(track, sample).map_err(|e| e.to_string())?;
        }
        return Ok(());
    }
    let bits = if sound == Sound::Pcm8 { 8 } else { 16 };
    let config = PcmConfig {
        bits,
        float: false,
        big_endian: true,
        signed: bits != 8 || !c.quicktime,
        channels: channels as u32,
        sample_rate: f64::from(rate),
    };
    let track = writer.add_track(TrackConfig::new(SampleEntry::pcm(config), rate)).map_err(|e| e.to_string())?;
    let mut data = Vec::new();
    for v in &left {
        if bits == 8 {
            // 8-bit sound is unsigned, around 128, in a QuickTime file and signed in an MP4
            let level = (v * 127.0).round() as i8;
            data.push(if c.quicktime { (i16::from(level) + 128) as u8 } else { level as u8 });
        } else {
            data.extend_from_slice(&((v * 32_767.0).round() as i16).to_be_bytes());
            data.extend_from_slice(&((v * 0.5 * 32_767.0).round() as i16).to_be_bytes());
        }
    }
    let frame = channels * usize::from(bits / 8);
    for block in data.chunks(1024 * frame) {
        let sample = WriteSample { data: block, duration: (block.len() / frame) as u32, composition_offset: 0, is_sync: true };
        writer.write_sample(track, sample).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// A video of `c.frames` frames (H.264, or Motion JPEG) in an MP4 (or QuickTime) file.
pub fn clip(c: &Clip) -> Result<Vec<u8>, String> {
    let (w, h) = (c.width as usize, c.height as usize);
    if w < 16 || h < 16 || w % 2 != 0 || h % 2 != 0 || c.fps == 0 || c.frames == 0 {
        return Err("a clip needs an even size of at least 16 x 16, a frame rate and a frame".into());
    }
    let brand = if c.quicktime { Brand::Mov } else { Brand::Mp4 };
    let mut writer = Mp4Writer::new(Cursor::new(Vec::new()), WriterOptions::new(brand)).map_err(|e| e.to_string())?;
    if c.motion_jpeg {
        jpeg_track(c, &mut writer)?;
    } else {
        h264_track(c, &mut writer)?;
    }
    if let Some(sound) = c.sound {
        sound_track(c, sound, &mut writer)?;
    }
    let mut bytes = writer.finish().map_err(|e| e.to_string())?.into_inner();
    if !c.rotation.is_multiple_of(4) {
        rotate_track(&mut bytes, c.rotation)?;
    }
    if let Some(cover) = &c.cover {
        // ISO style: `meta` is a full box (version and flags), its handler names the item list
        let handler = bx(b"hdlr", &[&[0u8; 8][..], b"mdirappl", &[0u8; 9]].concat());
        let data = bx(b"data", &[&[0, 0, 0, 13, 0, 0, 0, 0][..], cover].concat());
        let meta = bx(b"meta", &[&[0u8; 4][..], &handler, &bx(b"ilst", &bx(b"covr", &data))].concat());
        append_to_moov(&mut bytes, &bx(b"udta", &meta))?;
    }
    Ok(bytes)
}

/// A file whose one video track is declared HEVC, with headers only (its single sample is not a
/// picture): for what a build without the HEVC decoder says about such a file.
pub fn hevc_headers_only(width: u16, height: u16) -> Result<Vec<u8>, String> {
    let mut writer = Mp4Writer::new(Cursor::new(Vec::new()), WriterOptions::new(Brand::Mov)).map_err(|e| e.to_string())?;
    let config =
        HevcConfig { general_profile_idc: 1, chroma_format_idc: 1, bit_depth_luma: 8, bit_depth_chroma: 8, length_size: 4, ..HevcConfig::default() };
    let track = writer.add_track(TrackConfig::new(SampleEntry::hevc(config, width, height), 2400)).map_err(|e| e.to_string())?;
    let sample = WriteSample { data: &[0, 0, 0, 1, 0], duration: 100, composition_offset: 0, is_sync: true };
    writer.write_sample(track, sample).map_err(|e| e.to_string())?;
    Ok(writer.finish().map_err(|e| e.to_string())?.into_inner())
}
