//! Generated test clips (feature `testkit`, for tests only): LightCraft never commits media, so
//! tests here and in the crates above encode a few small frames and wrap them in an MP4.
//!
//! Every frame is plain grey, a little brighter than the one before ([`grey`]), with the stored
//! picture's top left quarter tinted red: a test can tell which frame it got and which way up.

use std::io::Cursor;

use filmcraft_h264enc::{Encoder, EncoderConfig, PacketFormat, Profile, RateControl, YuvFrame};
use filmcraft_isobmff::{AvcConfig, Brand, HevcConfig, Mp4Writer, SampleEntry, TrackConfig, WriteSample, WriterOptions};

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
}

impl Default for Clip {
    fn default() -> Self {
        Clip { width: 64, height: 48, frames: 12, fps: 24, keyint: 5, rotation: 0, cover: None, quicktime: false }
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

/// An H.264 video of `c.frames` frames in an MP4 (or QuickTime) file.
pub fn clip(c: &Clip) -> Result<Vec<u8>, String> {
    let (w, h) = (c.width as usize, c.height as usize);
    if w < 16 || h < 16 || w % 2 != 0 || h % 2 != 0 || c.fps == 0 || c.frames == 0 {
        return Err("a clip needs an even size of at least 16 x 16, a frame rate and a frame".into());
    }
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

    let brand = if c.quicktime { Brand::Mov } else { Brand::Mp4 };
    let mut writer = Mp4Writer::new(Cursor::new(Vec::new()), WriterOptions::new(brand)).map_err(|e| e.to_string())?;
    let avcc = AvcConfig::parse(&encoder.avcc()).map_err(|e| e.to_string())?;
    let entry = SampleEntry::avc(avcc, c.width as u16, c.height as u16);
    let track = writer.add_track(TrackConfig::new(entry, c.fps * TICKS)).map_err(|e| e.to_string())?;
    for p in &packets {
        let offset = i32::try_from((p.pts - p.dts) * i64::from(TICKS)).map_err(|_| "composition offset")?;
        let sample = WriteSample { data: &p.data, duration: TICKS, composition_offset: offset, is_sync: p.keyframe };
        writer.write_sample(track, sample).map_err(|e| e.to_string())?;
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
