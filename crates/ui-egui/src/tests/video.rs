//! Videos in the grid and the panels (M14.2): a video thumbnail carries its length, and the
//! Edit panel says it can't be edited instead of offering sliders that fail.

use std::time::Duration;

use lightcraft_catalog::{MediaKind, PhotoId};
use lightcraft_engine::Session;
use lightcraft_video::testkit::{Clip, clip};
use serde_json::json;

use crate::headless::Headless;
use crate::state::{RightPanel, ViewMode};
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

#[test]
fn a_video_shows_its_length_in_the_grid_and_no_sliders_in_edit() {
    let dir = std::env::temp_dir().join(format!("lc-ui-video-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // 3 seconds of a 160 x 120 clip, and an ordinary picture next to it
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
    h.settle(SETTLE);
    let video: PhotoId = h.app.session.catalog.photos().find(|p| p.kind == MediaKind::Video).map(|p| p.id).unwrap();
    let photo: PhotoId = h.app.session.catalog.photos().find(|p| p.kind == MediaKind::Image).map(|p| p.id).unwrap();
    // the grid: only the video has the length badge, and its poster drew (no failed thumbnail)
    assert_eq!(widget_ids(&mut h, "badge:video:"), vec![format!("badge:video:{}", video.0)]);
    assert!(h.app.renderer.failure(crate::render::Slot::Thumb(video)).is_none(), "the poster rendered");
    shot(&mut h, "video-grid.png");

    // Edit: a notice, not sliders
    h.request("engine.execute", json!({"command": "library.select", "params": {"ids": [video.0]}}), SETTLE);
    h.request("ui.set", json!({"view": "detail"}), SETTLE);
    h.app.ui.right = RightPanel::Edit;
    h.settle(SETTLE);
    assert_eq!(widget_ids(&mut h, "notice:videoNoEdit"), vec!["notice:videoNoEdit".to_string()]);
    let sliders = widget_ids(&mut h, "slider:");
    assert!(sliders.is_empty(), "{sliders:?}");
    shot(&mut h, "video-detail.png");
    // an ordinary photo keeps its sliders and has no notice
    h.request("engine.execute", json!({"command": "library.select", "params": {"ids": [photo.0]}}), SETTLE);
    h.settle(SETTLE);
    assert!(widget_ids(&mut h, "notice:videoNoEdit").is_empty());
    assert!(!widget_ids(&mut h, "slider:").is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
