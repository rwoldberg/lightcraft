//! Video files in the library (MP4 / QuickTime, through `lightcraft-video`).
//!
//! A video is catalogued like a photo ([`MediaKind::Video`], with its duration) and shown by its
//! poster: the preview image the file carries when it has one, else its first picture. A video
//! is never read whole: only its headers, and the frames that are shown.

use std::path::Path;

use lightcraft_catalog::MediaKind;
use lightcraft_pipeline::SourceInfo;
use lightcraft_raster::resample::{Filter, fit};
use lightcraft_raster::{Rgb32f, Rgba8};

use crate::media::ProbeInfo;

/// File extensions imported as video (lower case).
pub const EXTENSIONS: &[&str] = &["mov", "mp4", "m4v"];

/// Does `path` name a video file (by its extension)?
pub fn is_video(path: &str) -> bool {
    Path::new(path).extension().is_some_and(|e| EXTENSIONS.contains(&e.to_string_lossy().to_lowercase().as_str()))
}

/// Bytes hashed at each end of a video for [`ProbeInfo::content_hash`].
const HASH_SPAN: u64 = 1 << 20;

/// What identifies a video's content without reading all of it (a clip can be many gigabytes):
/// its size and its first and last megabyte. Two different recordings never share these in
/// practice; unlike a photo's hash it is not a hash of the whole file, so it is spelled
/// differently (`video-…`) and an import's copy is verified against the source instead.
fn content_hash(source: &dyn lightcraft_video::ByteSource) -> Option<String> {
    let len = source.len();
    let read = |at: u64, n: u64| {
        let mut buf = vec![0u8; usize::try_from(n).ok()?];
        source.read_at(at, &mut buf).ok()?;
        Some(buf)
    };
    let head = read(0, len.min(HASH_SPAN))?;
    let tail = read(len.saturating_sub(HASH_SPAN).max(head.len() as u64), len.saturating_sub(head.len() as u64).min(HASH_SPAN))?;
    let mut hasher = lightcraft_preview::Hasher128::new();
    hasher.u64(len).update(&head).update(&tail);
    Some(format!("video-{len:x}-{}", hasher.finish()))
}

fn format_of(path: &str) -> String {
    Path::new(path).extension().map(|e| e.to_string_lossy().to_uppercase()).unwrap_or_else(|| "MP4".into())
}

/// Describe a video from its headers, as an import's probe.
pub fn probe_source(path: &str, source: impl lightcraft_video::ByteSource + Send + Sync + 'static) -> Result<ProbeInfo, String> {
    let content_hash = content_hash(&source);
    let file_size = source.len();
    let video = lightcraft_video::Video::open(source).map_err(|e| e.to_string())?;
    let info = video.info();
    let camera = [info.make.as_deref(), info.model.as_deref()].into_iter().flatten().collect::<Vec<_>>().join(" ");
    Ok(ProbeInfo {
        width: info.width,
        height: info.height,
        format: format_of(path),
        kind: MediaKind::Video,
        file_size,
        captured: info.created.clone(),
        meta: lightcraft_catalog::Meta { camera, ..Default::default() },
        content_hash,
        duration: Some(info.duration).filter(|d| d.is_finite() && *d > 0.0),
        ..Default::default()
    })
}

/// A video's poster in the develop working space, no larger than `max_edge`: its embedded
/// preview when it has one, else its first picture.
pub fn poster_source(source: impl lightcraft_video::ByteSource + Send + Sync + 'static, max_edge: usize) -> Result<(Rgb32f, SourceInfo), String> {
    let video = lightcraft_video::Video::open(source).map_err(|e| e.to_string())?;
    if let Ok(Some(image)) = video.embedded_preview()
        && let Ok(shown) = crate::files::load_bytes(&image, max_edge)
    {
        return Ok(shown);
    }
    let frame = first_picture(&video, max_edge)?;
    // display sRGB → linear Rec.2020, the working space
    let to_working =
        lightcraft_codecs::SourceSpace::named(lightcraft_codecs::NamedSpace::Srgb, lightcraft_codecs::SpaceOrigin::Untagged).to_working().to_f32();
    let working = frame.to_linear().map(|p| {
        let row = |r: [f32; 3]| r[0] * p[0] + r[1] * p[1] + r[2] * p[2];
        [row(to_working[0]), row(to_working[1]), row(to_working[2])]
    });
    Ok((fit(&working, max_edge, max_edge, Filter::Box), SourceInfo::default()))
}

/// A video's poster for display (sRGB, no larger than `max_edge`): [`poster_source`] without the
/// working-space conversion, for the quick preview shown first.
pub fn poster_srgb_source(source: impl lightcraft_video::ByteSource + Send + Sync + 'static, max_edge: usize) -> Option<Rgba8> {
    let video = lightcraft_video::Video::open(source).ok()?;
    let edge = u32::try_from(max_edge).unwrap_or(u32::MAX);
    if let Ok(Some(image)) = video.embedded_preview()
        && let Ok(mut d) = lightcraft_codecs::decode(&image, lightcraft_codecs::DecodeOptions::fit(edge, edge))
    {
        if d.image.width.max(d.image.height) > max_edge {
            d.image = fit(&d.image, max_edge, max_edge, Filter::Box);
            d.alpha = None;
        }
        return Some(d.to_srgb8().oriented(lightcraft_geom::Orientation::from_exif(d.orientation)));
    }
    let frame = first_picture(&video, max_edge).ok()?;
    Some(if frame.width.max(frame.height) > max_edge { fit(&frame.to_linear(), max_edge, max_edge, Filter::Box).to_srgb8() } else { frame })
}

/// The first picture of `video`, decoded at about `max_edge` (a whole-factor reduction: it may
/// be somewhat larger).
fn first_picture(video: &lightcraft_video::Video, max_edge: usize) -> Result<Rgba8, String> {
    let frame = video.poster_frame(u32::try_from(max_edge).ok()).map_err(|e| e.to_string())?;
    Rgba8::from_bytes(frame.width as usize, frame.height as usize, &frame.rgba).ok_or_else(|| "the video's first picture is incomplete".to_string())
}

/// [`probe_source`] for a file on disk.
#[cfg(not(target_arch = "wasm32"))]
pub fn probe(path: &str) -> Result<ProbeInfo, String> {
    probe_source(path, std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?)
}

/// [`poster_source`] for a file on disk.
#[cfg(not(target_arch = "wasm32"))]
pub fn poster(path: &str, max_edge: usize) -> Result<(Rgb32f, SourceInfo), String> {
    poster_source(std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?, max_edge).map_err(|e| format!("{path}: {e}"))
}

/// [`poster_srgb_source`] for a file on disk.
#[cfg(not(target_arch = "wasm32"))]
pub fn poster_srgb(path: &str, max_edge: usize) -> Option<Rgba8> {
    poster_srgb_source(std::fs::File::open(path).ok()?, max_edge)
}

/// The browser build has no file system: its videos are not opened yet.
#[cfg(target_arch = "wasm32")]
pub fn probe(path: &str) -> Result<ProbeInfo, String> {
    Err(format!("{path}: video isn't supported in the browser build yet"))
}

#[cfg(target_arch = "wasm32")]
pub fn poster(path: &str, _max_edge: usize) -> Result<(Rgb32f, SourceInfo), String> {
    Err(format!("{path}: video isn't supported in the browser build yet"))
}

#[cfg(target_arch = "wasm32")]
pub fn poster_srgb(_path: &str, _max_edge: usize) -> Option<Rgba8> {
    None
}
