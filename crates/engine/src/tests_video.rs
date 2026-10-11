//! Video files in the library: import, poster, and what they refuse (M14.2).

use lightcraft_catalog::{MediaKind, PhotoId};
use lightcraft_video::testkit::{Clip, clip};
use serde_json::json;

use crate::Session;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("lc-video-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn small() -> Clip {
    Clip { width: 64, height: 48, frames: 12, fps: 24, keyint: 5, ..Clip::default() }
}

fn imported(r: &serde_json::Value) -> Vec<PhotoId> {
    r["imported"].as_array().map(|a| a.iter().filter_map(|v| v.as_u64()).map(PhotoId).collect()).unwrap_or_default()
}

/// The mean colour of a rendered photo (display values).
fn mean(s: &mut Session, id: PhotoId) -> [f32; 3] {
    let r = s.render_job(id, 64, 64, false, true).unwrap().run().rendered.unwrap();
    let n = r.image.data.len().max(1) as f32;
    let sum = r.image.data.iter().fold([0.0f32; 3], |a, p| [a[0] + f32::from(p[0]), a[1] + f32::from(p[1]), a[2] + f32::from(p[2])]);
    [sum[0] / n, sum[1] / n, sum[2] / n]
}

/// `.mov` / `.mp4` files were not importable at all: now a folder's videos join the library as
/// videos, with their length, described from their headers and shown by their first picture.
#[test]
fn videos_import_with_kind_duration_and_a_poster() {
    let src = temp_dir("import");
    std::fs::write(src.join("clip.mp4"), clip(&small()).unwrap()).unwrap();
    std::fs::write(src.join("Upright.MOV"), clip(&Clip { rotation: 1, quicktime: true, frames: 6, ..small() }).unwrap()).unwrap();
    let mut s = Session::new().with_fs();
    // the import review says what they are
    let scan = s.execute("library.importPreview", &json!({"paths": [src.to_string_lossy()]})).unwrap();
    assert_eq!(scan["candidates"].as_array().map(Vec::len), Some(2), "{scan}");
    let r = s.execute("library.import", &json!({"paths": [src.to_string_lossy()]})).unwrap();
    let ids = imported(&r);
    assert_eq!(ids.len(), 2, "{r}");
    let by_name = |s: &Session, name: &str| s.catalog.photos().find(|p| p.file_name == name).map(|p| (**p).clone()).unwrap();
    let mp4 = by_name(&s, "clip.mp4");
    assert_eq!((mp4.kind, mp4.format.as_str(), mp4.width, mp4.height), (MediaKind::Video, "MP4", 64, 48));
    assert!((mp4.duration.unwrap() - 0.5).abs() < 1e-6, "{:?}", mp4.duration);
    assert!(mp4.content_hash.as_deref().is_some_and(|h| h.starts_with("video-")), "{:?}", mp4.content_hash);
    assert!(!mp4.develops_raw() && mp4.preview_only.is_none());
    // a clip recorded upright is catalogued upright
    let mov = by_name(&s, "Upright.MOV");
    assert_eq!((mov.kind, mov.format.as_str(), mov.width, mov.height), (MediaKind::Video, "MOV", 48, 64));
    assert!((mov.duration.unwrap() - 0.25).abs() < 1e-6);
    // the poster renders like any photo: the first frame is dark grey with a red quarter
    let [r_, g, b] = mean(&mut s, mp4.id);
    assert!(r_ > g + 5.0 && (g - b).abs() < 12.0 && g > 10.0 && g < 120.0, "poster mean {:?}", [r_, g, b]);
    assert!(s.thumb_job(mov.id, 96).unwrap().run().rendered.is_ok());
    // agents see the kind and the length
    let q = s.execute("catalog.query", &json!({"filter": {"kind": "video"}})).unwrap();
    assert_eq!(q["photos"].as_array().map(Vec::len), Some(2), "{q}");
    // a second import finds both already there
    let again = s.execute("library.import", &json!({"paths": [src.to_string_lossy()]})).unwrap();
    assert_eq!(imported(&again).len(), 0, "{again}");
    let _ = std::fs::remove_dir_all(&src);
}

/// A file that carries a preview image shows that, not its first picture.
#[test]
fn an_embedded_preview_is_the_poster() {
    use lightcraft_codecs::{ChromaSubsampling, EncodeImage, EncodeMeta, Samples, encode_jpeg};
    let src = temp_dir("cover");
    let green: Vec<u8> = (0..32 * 24).flat_map(|_| [20u8, 200, 30]).collect();
    let cover = encode_jpeg(&EncodeImage::new(32, 24, 3, Samples::U8(&green)), 92, ChromaSubsampling::S444, &EncodeMeta::default()).unwrap();
    std::fs::write(src.join("with-cover.mp4"), clip(&Clip { cover: Some(cover), ..small() }).unwrap()).unwrap();
    let mut s = Session::new().with_fs();
    let r = s.execute("library.import", &json!({"paths": [src.to_string_lossy()]})).unwrap();
    let id = imported(&r)[0];
    let [r_, g, b] = mean(&mut s, id);
    assert!(g > r_ + 80.0 && g > b + 80.0, "the green cover, not the grey and red frame: {:?}", [r_, g, b]);
    // the quick preview shown first is the same picture
    let quick = crate::video::poster_srgb(&src.join("with-cover.mp4").to_string_lossy(), 16).unwrap();
    assert!(quick.width.max(quick.height) <= 16);
    assert!(quick.data.iter().all(|p| p[1] > p[0].saturating_add(80)), "{:?}", quick.data.first());
    let _ = std::fs::remove_dir_all(&src);
}

/// Damaged and misnamed files are import failures with a reason, never a panic, and the rest of
/// the folder still imports.
#[test]
fn damaged_videos_fail_to_import_and_nothing_else_does() {
    let src = temp_dir("damaged");
    let good = clip(&small()).unwrap();
    std::fs::write(src.join("good.mp4"), &good).unwrap();
    std::fs::write(src.join("cut.mp4"), &good[..good.len() / 2]).unwrap();
    std::fs::write(src.join("text.mov"), b"this is not a video").unwrap();
    std::fs::write(src.join("empty.m4v"), b"").unwrap();
    let mut s = Session::new().with_fs();
    let r = s.execute("library.import", &json!({"paths": [src.to_string_lossy()]})).unwrap();
    assert_eq!(imported(&r).len(), 1, "{r}");
    assert_eq!(r["failed"].as_array().map(Vec::len), Some(3), "{r}");
    assert!(r["failed"].to_string().contains("video"), "the reason says what was wrong: {r}");
    assert_eq!(s.catalog.len(), 1);
    let _ = std::fs::remove_dir_all(&src);
}

/// A video is identified without reading all of it: a clip much larger than what is hashed
/// imports from its headers, and a copy into the library is still checked against the source.
#[test]
fn a_large_video_is_not_read_whole_and_copies_verified() {
    let src = temp_dir("large");
    let lib = temp_dir("large-lib");
    // 12 MiB of trailing data after the movie: the probe reads the headers and two megabytes
    let mut bytes = clip(&small()).unwrap();
    let free = 12usize << 20;
    bytes.extend_from_slice(&((free + 8) as u32).to_be_bytes());
    bytes.extend_from_slice(b"free");
    bytes.resize(bytes.len() + free, 0);
    let path = src.join("long.mp4");
    std::fs::write(&path, &bytes).unwrap();
    let info = crate::video::probe(&path.to_string_lossy()).unwrap();
    assert_eq!((info.kind, info.file_size), (MediaKind::Video, bytes.len() as u64));
    // the same recording with a different middle is a different file; the same bytes are the same
    let mut other = bytes.clone();
    other[bytes.len() / 2] ^= 1;
    std::fs::write(src.join("other.mp4"), &other).unwrap();
    assert_eq!(crate::video::probe(&src.join("other.mp4").to_string_lossy()).unwrap().content_hash, info.content_hash, "only the ends are hashed");
    std::fs::remove_file(src.join("other.mp4")).unwrap();
    let mut shorter = bytes.clone();
    shorter.truncate(bytes.len() - 1);
    std::fs::write(src.join("shorter.mp4"), &shorter).unwrap();
    assert_ne!(crate::video::probe(&src.join("shorter.mp4").to_string_lossy()).unwrap().content_hash, info.content_hash);
    std::fs::remove_file(src.join("shorter.mp4")).unwrap();

    let mut s = Session::new().with_fs();
    s.open_library(&lib, false).unwrap();
    let r = s.execute("library.import", &json!({"paths": [src.to_string_lossy()], "mode": "copy"})).unwrap();
    let id = imported(&r)[0];
    let p = s.catalog.photo(id).unwrap().clone();
    let lightcraft_catalog::Source::File { path: stored } = &p.source else { panic!("a file") };
    assert!(std::path::Path::new(stored).starts_with(&lib), "copied into the library: {stored}");
    assert_eq!(std::fs::read(stored).unwrap(), bytes, "byte for byte");
    assert!(path.exists(), "the source stays");
    drop(s);
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&lib);
}

/// A video can't be developed or rendered to an image format yet: both say so instead of
/// quietly editing or exporting its poster. Batches pass over it.
#[test]
fn videos_refuse_edits_and_rendered_exports() {
    let src = temp_dir("refuse");
    std::fs::write(src.join("clip.mp4"), clip(&small()).unwrap()).unwrap();
    let mut s = Session::new().with_fs();
    let r = s.execute("library.import", &json!({"paths": [src.to_string_lossy()]})).unwrap();
    let id = imported(&r)[0];
    let before = s.catalog.photo(id).unwrap().develop.clone();
    let mut d = lightcraft_develop::DevelopSettings::default();
    d.light.exposure = 1.0;
    let e = s.set_develop(id, d.clone(), "Exposure").unwrap_err().to_string();
    assert!(e.contains("videos can't be edited"), "{e}");
    assert!(s.develop_op(id, d, "Paste").is_none());
    assert_eq!(s.catalog.photo(id).unwrap().develop, before);
    // export: the file itself, not a picture of it
    let jpeg = crate::export::ExportOptions { format: crate::export::ExportFormat::Jpeg, ..Default::default() };
    let e = crate::export::prepare_export(&mut s, id, &jpeg, 1).err().unwrap();
    assert!(e.contains("is a video"), "{e}");
    let original = crate::export::ExportOptions { format: crate::export::ExportFormat::Original, ..Default::default() };
    let out = crate::export::prepare_export(&mut s, id, &original, 1).unwrap().run().unwrap();
    assert_eq!(out.bytes, std::fs::read(src.join("clip.mp4")).unwrap());
    let _ = std::fs::remove_dir_all(&src);
}

/// Every video extension is importable, whatever its case.
#[test]
fn video_extensions_are_importable() {
    assert!(crate::import::EXTENSIONS.iter().all(|e| *e == e.to_lowercase()));
    for ext in crate::video::EXTENSIONS {
        assert!(crate::import::EXTENSIONS.contains(ext), "{ext} is importable");
        assert!(crate::video::is_video(&format!("C:\\clips\\A.{}", ext.to_uppercase())));
    }
    assert!(!crate::video::is_video("photo.jpg") && !crate::video::is_video("mov") && !crate::video::is_video(""));
}

/// A clip named by a Lightroom catalog imported before videos were read sat in the library as an
/// ordinary picture ("VIDEO", 0 B): the loupe showed no player for it. Such an entry counts as a
/// video by its file, and Reload from Disk makes the catalog say so (undoably).
#[test]
fn a_video_catalogued_as_a_picture_is_still_a_video_and_reload_corrects_it() {
    use lightcraft_catalog::{Op, Photo, Source};
    let src = temp_dir("legacy");
    let path = src.join("GOPR0001.MP4");
    std::fs::write(&path, clip(&small()).unwrap()).unwrap();
    let mut s = Session::new().with_fs();
    let id = PhotoId(1);
    let old = Photo::new(id, Source::File { path: path.to_string_lossy().into() }, "GOPR0001.MP4", "VIDEO", 3840, 2160, "2026-10-09T00:00:00");
    assert_eq!((old.kind, old.duration, old.file_size), (MediaKind::Image, None, 0));
    s.commit("old import", Op::AddPhoto { photo: Box::new(old) }).unwrap();
    let photo = |s: &Session| s.catalog.photo(id).map(|p| (**p).clone()).unwrap();
    // already treated as a video: playable, not editable
    assert!(crate::video::is_video_photo(&photo(&s)));
    assert!(s.set_develop(id, lightcraft_develop::DevelopSettings::default(), "Exposure").is_err());
    // Reload: the catalog now says what the file is
    s.selection = crate::Selection::single(id);
    let r = s.execute("photo.reload", &json!({"ids": [id.0]})).unwrap();
    assert_eq!(r["reloaded"], json!([id.0]), "{r}");
    let now = photo(&s);
    assert_eq!((now.kind, now.width, now.height), (MediaKind::Video, 64, 48));
    assert!((now.duration.unwrap() - 0.5).abs() < 1e-6 && now.file_size > 0);
    let q = s.execute("catalog.query", &json!({"filter": {"kind": "video"}})).unwrap();
    assert_eq!(q["photos"].as_array().map(Vec::len), Some(1), "{q}");
    // nothing left to correct; and it can be undone
    assert_eq!(s.execute("photo.reload", &json!({"ids": [id.0]})).unwrap()["reloaded"], json!([]));
    s.execute("edit.undo", &json!({})).unwrap();
    let back = photo(&s);
    assert_eq!((back.kind, back.duration, back.width, back.file_size), (MediaKind::Image, None, 3840, 0));
    // an ordinary picture is not a video, whatever it is called inside
    let still = Photo::new(PhotoId(9), Source::File { path: "C:/pictures/mov.jpg".into() }, "mov.jpg", "JPEG", 4, 3, "now");
    assert!(!crate::video::is_video_photo(&still));
    let _ = std::fs::remove_dir_all(&src);
}
