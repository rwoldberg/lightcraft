//! Playing a video in the loupe (M14.3): play, pause, seek, step, the end, and what happens when
//! the file can't be played.

use std::time::Duration;

use lightcraft_catalog::{MediaKind, PhotoId};
use lightcraft_engine::Session;
use lightcraft_video::testkit::{Clip, clip};
use serde_json::{Value, json};

use crate::headless::Headless;
use crate::state::ViewMode;
use crate::{LightcraftApp, Services};

const SETTLE: Duration = Duration::from_secs(10);

fn widget_ids(h: &mut Headless, filter: &str) -> Vec<String> {
    let w = h.request("ui.widgets", json!({"filter": filter}), SETTLE);
    w["result"].as_array().unwrap_or_else(|| panic!("{w}")).iter().filter_map(|x| x["id"].as_str().map(str::to_string)).collect()
}

/// Save what the window shows when `LC_VIDEO_SHOT` names a folder (to look at the result by hand).
fn shot(h: &mut Headless, name: &str) {
    let Some(dir) = std::env::var_os("LC_VIDEO_SHOT") else { return };
    let image = h.snapshot(SETTLE);
    let [w, h_] = image.size;
    let rgba = lightcraft_raster::Rgba8::from_fn(w, h_, |x, y| image.pixels[y * w + x].to_array());
    let o = lightcraft_engine::export::ExportOptions { format: lightcraft_engine::export::ExportFormat::Png, ..Default::default() };
    let _ = std::fs::write(std::path::Path::new(&dir).join(name), lightcraft_engine::export::encode_image(&rgba, &o).unwrap());
}

/// A session with one 3-second clip (160 x 120, 12 frames a second) and one still, in Detail on the clip.
fn clip_in_detail(tag: &str) -> (Headless, PhotoId, PhotoId, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("lc-ui-video-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("clip.mp4"), clip(&Clip { width: 160, height: 120, frames: 36, fps: 12, keyint: 12, ..Clip::default() }).unwrap())
        .unwrap();
    let still = lightcraft_raster::Rgba8::from_fn(120, 80, |x, y| [(x * 2) as u8, (y * 3) as u8, 90, 255]);
    let o = lightcraft_engine::export::ExportOptions { format: lightcraft_engine::export::ExportFormat::Png, ..Default::default() };
    std::fs::write(dir.join("still.png"), lightcraft_engine::export::encode_image(&still, &o).unwrap()).unwrap();
    let mut app = LightcraftApp::new(Session::new().with_fs(), Services { png: None, ..Default::default() });
    app.ui.view = ViewMode::PhotoGrid;
    let mut h = Headless::new(app, [1100.0, 760.0], 1.0);
    let r = h.request("engine.execute", json!({"command": "library.import", "params": {"paths": [dir.to_string_lossy()]}}), SETTLE);
    assert_eq!(r["result"]["imported"].as_array().map(Vec::len), Some(2), "{r}");
    let video = h.app.session.catalog.photos().find(|p| p.kind == MediaKind::Video).map(|p| p.id).unwrap();
    let photo = h.app.session.catalog.photos().find(|p| p.kind == MediaKind::Image).map(|p| p.id).unwrap();
    h.request("engine.execute", json!({"command": "library.select", "params": {"ids": [video.0]}}), SETTLE);
    h.request("ui.set", json!({"view": "detail"}), SETTLE);
    h.settle(SETTLE);
    (h, video, photo, dir)
}

fn status(h: &Headless) -> Value {
    h.app.video.as_ref().map_or(Value::Null, crate::video::Player::status)
}

fn position(h: &Headless) -> f64 {
    status(h)["position"].as_f64().unwrap_or(-1.0)
}

/// A video in the loupe could only be looked at as a still. Now it plays: Play runs through its
/// frames in time, Pause holds one, seeking and stepping land on the frame asked for, the end
/// stops it, and Play there starts over.
#[test]
fn a_video_plays_pauses_seeks_and_steps_in_the_loupe() {
    let (mut h, video, _, dir) = clip_in_detail("play");
    // before it is played: the poster, the transport bar, and no player (no decode thread yet)
    assert!(h.app.video.is_none());
    for id in ["video:play", "video:seek", "video:time", "video:picture"] {
        assert_eq!(widget_ids(&mut h, id), vec![id.to_string()]);
    }
    assert!(crate::menus::ui_enabled(&h.app, "video.playPause"));
    shot(&mut h, "video-poster.png");

    let r = h.app.run("video.playPause", json!({})).unwrap();
    assert_eq!((r["playing"].as_bool(), r["photo"].as_u64()), (Some(true), Some(video.0)), "{r}");
    assert!(h.step_until(SETTLE, |h| position(h) > 0.5), "it plays: {}", status(&h));
    assert_eq!(status(&h)["playing"], true);
    assert!((status(&h)["duration"].as_f64().unwrap() - 3.0).abs() < 1e-6 && (status(&h)["frameRate"].as_f64().unwrap() - 12.0).abs() < 1e-6);
    shot(&mut h, "video-playing.png");

    // pause: the frame stays
    h.app.run("video.pause", json!({})).unwrap();
    let held = position(&h);
    for _ in 0..30 {
        h.step();
    }
    assert_eq!(position(&h), held, "paused");

    // seek, by fraction and by time: the frame shown then
    h.app.run("video.seek", json!({"fraction": 0.5})).unwrap();
    assert!(h.step_until(SETTLE, |h| (position(h) - 1.5).abs() < 1e-6 && status(h)["framesShown"].as_u64() > Some(0)), "{}", status(&h));
    h.app.run("video.seek", json!({"seconds": 2.26})).unwrap();
    assert!(h.step_until(SETTLE, |h| (position(h) - 27.0 / 12.0).abs() < 1e-6), "frame 27 covers 2.26 s: {}", status(&h));
    // a frame on, then two back (not a key frame: decoded from the key frame before it)
    h.app.run("video.step", json!({})).unwrap();
    assert!(h.step_until(SETTLE, |h| (position(h) - 28.0 / 12.0).abs() < 1e-6), "{}", status(&h));
    h.app.run("video.step", json!({"frames": -2})).unwrap();
    assert!(h.step_until(SETTLE, |h| (position(h) - 26.0 / 12.0).abs() < 1e-6), "{}", status(&h));
    assert_eq!(status(&h)["playing"], false, "stepping pauses");

    // Space plays; at the end playback stops by itself on the last frame
    h.request("ui.key", json!({"key": "space"}), SETTLE);
    assert!(h.step_until(SETTLE, |h| status(h)["playing"] == true), "Space plays: {}", status(&h));
    assert!(h.step_until(SETTLE, |h| status(h)["ended"] == true), "{}", status(&h));
    assert_eq!(status(&h)["playing"], false);
    assert!((position(&h) - 35.0 / 12.0).abs() < 1e-6, "the last frame: {}", status(&h));
    // Play at the end starts over
    h.app.run("video.play", json!({})).unwrap();
    assert!(h.step_until(SETTLE, |h| status(h)["playing"] == true && position(h) > 0.0 && position(h) < 1.0), "{}", status(&h));
    // agents see the player
    let inspect = h.request("ui.inspect", json!({}), SETTLE);
    assert_eq!(inspect["result"]["video"]["photo"], video.0, "{inspect}");

    // odd requests are answered, not crashes
    for odd in [json!({"seconds": -5.0}), json!({"seconds": 1e12}), json!({"fraction": 7.0})] {
        assert!(h.app.run("video.seek", odd).is_ok());
    }
    assert!(h.app.run("video.seek", json!({})).is_err());
    assert!(h.app.run("video.step", json!({"frames": i64::MAX})).is_ok());
    assert!(h.app.run("video.nonsense", json!({})).is_err());
    h.settle(SETTLE);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The player belongs to the video on screen: it ends when the loupe shows something else, and
/// the commands say so when there is no video to play.
#[test]
fn the_player_ends_with_its_video_and_only_videos_play() {
    let (mut h, video, photo, dir) = clip_in_detail("ends");
    h.app.run("video.play", json!({})).unwrap();
    assert!(h.step_until(SETTLE, |h| position(h) > 0.2), "{}", status(&h));
    // another photo: the player is gone, and a still can't be played
    h.request("engine.execute", json!({"command": "library.select", "params": {"ids": [photo.0]}}), SETTLE);
    h.step();
    assert!(h.app.video.is_none());
    assert!(!crate::menus::ui_enabled(&h.app, "video.playPause"));
    let e = h.app.run("video.play", json!({})).unwrap_err();
    assert!(e.contains("open a video in Detail"), "{e}");
    // back on the video, then to the grid
    h.request("engine.execute", json!({"command": "library.select", "params": {"ids": [video.0]}}), SETTLE);
    h.app.run("video.play", json!({})).unwrap();
    h.step();
    assert!(h.app.video.is_some());
    h.request("ui.set", json!({"view": "photoGrid"}), SETTLE);
    h.step();
    assert!(h.app.video.is_none(), "leaving Detail ends playback");
    // stop and status never need a video
    assert_eq!(h.app.run("video.stop", json!({})).unwrap(), Value::Null);
    assert_eq!(h.app.run("video.status", json!({})).unwrap(), Value::Null);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A video that went missing or was damaged after it was imported says why it can't be played;
/// the app carries on.
#[test]
fn a_video_that_cannot_be_played_says_why() {
    for damage in ["missing", "truncated", "garbage"] {
        let (mut h, _, _, dir) = clip_in_detail(damage);
        let path = dir.join("clip.mp4");
        let bytes = std::fs::read(&path).unwrap();
        match damage {
            "missing" => std::fs::remove_file(&path).unwrap(),
            // the headers are at the end of this file: half of it has none
            "truncated" => std::fs::write(&path, &bytes[..bytes.len() / 2]).unwrap(),
            _ => {
                // intact headers, picture data overwritten
                let mdat = bytes.windows(4).position(|w| w == b"mdat").unwrap();
                let moov = bytes.windows(4).position(|w| w == b"moov").unwrap();
                let mut bad = bytes.clone();
                bad[mdat + 8..moov - 4].fill(0xff);
                std::fs::write(&path, bad).unwrap();
            }
        }
        h.app.run("video.play", json!({})).unwrap();
        assert!(h.step_until(SETTLE, |h| status(h)["error"].is_string()), "{damage}: {}", status(&h));
        h.step();
        assert_eq!(status(&h)["playing"], false, "{damage}");
        h.settle(SETTLE);
        assert_eq!(widget_ids(&mut h, "video:error"), vec!["video:error".to_string()], "{damage}");
        if damage == "garbage" {
            shot(&mut h, "video-error.png");
        }
        // play again: still an error, still no crash
        h.app.run("video.playPause", json!({})).unwrap();
        for _ in 0..20 {
            h.step();
        }
        assert_eq!(status(&h)["playing"], false, "{damage}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// A clip that an older import catalogued as an ordinary picture (a Lightroom catalog's "VIDEO"
/// row) showed in the loupe with no way to play it. It gets the player like any video.
#[test]
fn a_video_catalogued_as_a_picture_still_gets_the_player() {
    use lightcraft_catalog::{Op, Photo, Source};
    let (mut h, video, _, dir) = clip_in_detail("legacy");
    // the same file again, as that import left it
    let path = dir.join("clip.mp4").to_string_lossy().to_string();
    let id = h.app.session.catalog.alloc_photo_id();
    let old = Photo::new(id, Source::File { path }, "clip.mp4", "VIDEO", 160, 120, "2026-10-09T00:00:00");
    assert_eq!(old.kind, MediaKind::Image);
    h.app.session.commit("old import", Op::AddPhoto { photo: Box::new(old) }).unwrap();
    assert_ne!(id, video);
    h.request("engine.execute", json!({"command": "library.select", "params": {"ids": [id.0]}}), SETTLE);
    h.settle(SETTLE);
    for widget in ["video:play", "video:seek", "video:picture"] {
        assert_eq!(widget_ids(&mut h, widget), vec![widget.to_string()]);
    }
    assert_eq!(widget_ids(&mut h, "notice:videoNoEdit").len() + widget_ids(&mut h, "slider:").len(), widget_ids(&mut h, "notice:videoNoEdit").len());
    h.app.run("video.play", json!({})).unwrap();
    assert!(h.step_until(SETTLE, |h| position(h) > 0.3), "{}", status(&h));
    assert_eq!(status(&h)["photo"], id.0);
    let _ = std::fs::remove_dir_all(&dir);
}
