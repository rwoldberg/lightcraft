//! Video in the UI: the player behind the loupe, its transport bar, and what the grid says about
//! a video.
//!
//! A [`Player`] is made when a video is first played (looking at a poster costs nothing). It owns
//! one worker thread that opens the file and decodes frames a few ahead; the UI thread only picks
//! the frame that is due and uploads it to a texture, so a slow drive or a heavy clip never stalls
//! a frame of the app. Playing has no sound yet.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use egui::{Align2, Color32, Rect, Sense, Stroke, pos2, vec2};
use lightcraft_catalog::PhotoId;
use lightcraft_video::Frame;
use serde_json::{Value, json};

use crate::LightcraftApp;
use crate::render::Slot;
use crate::theme::Tokens;
use crate::widgets::register;

/// Decoded frames kept ahead of the one on screen.
const AHEAD: usize = 6;
/// The transport bar under a video.
const BAR_HEIGHT: f32 = 44.0;

/// A length in seconds as a clock: `0:07`, `12:34`, `1:02:03`.
pub fn clock(seconds: f64) -> String {
    let total = if seconds.is_finite() && seconds > 0.0 { seconds.round().min(359_999.0) as u64 } else { 0 };
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// What the UI thread and the decode thread share.
#[derive(Default)]
struct State {
    /// Decoded frames in the order they are shown, each with the seek it belongs to.
    frames: VecDeque<(u64, Frame)>,
    /// A position to continue from, not yet taken by the worker.
    seek: Option<f64>,
    /// Counts seeks: frames of an older one are discarded.
    generation: u64,
    /// The long edge frames are wanted at (pixels on screen).
    max_edge: Option<u32>,
    stop: bool,
    /// The last frame of the video has been decoded.
    ended: bool,
    error: Option<String>,
    /// (seconds, frames per second), once the file is open.
    info: Option<(f64, f64)>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The video playing (or paused) in the loupe.
pub struct Player {
    pub photo: PhotoId,
    shared: Arc<Shared>,
    texture: Option<egui::TextureHandle>,
    playing: bool,
    /// Seconds: the time of the frame on screen (or where a seek is going).
    position: f64,
    duration: f64,
    frame_rate: f64,
    /// While playing: (app time, video time) the playback clock started from.
    started: Option<(f64, f64)>,
    generation: u64,
    /// The seek the frame on screen belongs to.
    shown: Option<u64>,
    ended: bool,
    error: Option<String>,
    frames_shown: u64,
    frames_dropped: u64,
}

impl Drop for Player {
    fn drop(&mut self) {
        // the worker ends by itself: never wait here for a decode or a slow drive
        self.shared.lock().stop = true;
        self.shared.wake.notify_all();
    }
}

fn fail(shared: &Shared, generation: u64, error: String) {
    let mut st = shared.lock();
    if st.generation == generation {
        st.error = Some(error);
    }
}

/// The decode thread: opens the file, then keeps [`AHEAD`] frames ready from the last seek.
#[cfg(not(target_arch = "wasm32"))]
fn work(shared: &Shared, path: &str, repaint: &egui::Context) {
    let opened = lightcraft_video::Video::open_path(path).and_then(|video| {
        let info = video.info().clone();
        video.reader().map(|reader| (reader, info))
    });
    let (mut reader, info) = match opened {
        Ok(r) => r,
        Err(e) => {
            // nothing will ever take the pending seek: the failure is the answer to it
            let mut st = shared.lock();
            st.seek = None;
            st.error = Some(e.to_string());
            drop(st);
            repaint.request_repaint();
            return;
        }
    };
    shared.lock().info = Some((info.duration, info.frame_rate));
    // the frames waiting to be shown count as working memory, so background decodes make room
    let bytes = (info.width as usize).saturating_mul(info.height as usize).saturating_mul(4).saturating_mul(AHEAD);
    let _permit = lightcraft_engine::memory::work_gate().acquire_urgent(bytes);
    let mut generation = 0u64;
    loop {
        {
            let mut st = shared.lock();
            loop {
                if st.stop {
                    return;
                }
                reader.set_max_edge(st.max_edge);
                if let Some(to) = st.seek.take() {
                    generation = st.generation;
                    st.frames.clear();
                    st.ended = false;
                    st.error = None;
                    drop(st);
                    if let Err(e) = reader.seek(to) {
                        fail(shared, generation, e.to_string());
                    }
                    break;
                }
                if st.error.is_none() && !st.ended && st.frames.len() < AHEAD {
                    break;
                }
                st = shared.wake.wait_timeout(st, Duration::from_millis(100)).unwrap_or_else(PoisonError::into_inner).0;
            }
        }
        let next = reader.next_frame();
        let mut st = shared.lock();
        if st.generation == generation && st.seek.is_none() {
            match next {
                Ok(Some(frame)) => st.frames.push_back((generation, frame)),
                Ok(None) => st.ended = true,
                Err(e) => st.error = Some(e.to_string()),
            }
        }
        drop(st);
        repaint.request_repaint();
    }
}

impl Player {
    /// The browser build has neither files nor threads to decode on yet.
    #[cfg(target_arch = "wasm32")]
    fn open(_photo: PhotoId, _path: String, _duration: f64, _ctx: &egui::Context) -> Result<Self, String> {
        Err("video playback isn't available in the browser build yet".into())
    }

    /// Start a player for `photo`, the video at `path`. The file is opened on the worker.
    #[cfg(not(target_arch = "wasm32"))]
    fn open(photo: PhotoId, path: String, duration: f64, ctx: &egui::Context) -> Result<Self, String> {
        let shared = Arc::new(Shared::default());
        shared.lock().seek = Some(0.0);
        let (for_worker, repaint) = (shared.clone(), ctx.clone());
        std::thread::Builder::new()
            .name("lc-video-decode".into())
            .spawn(move || {
                // a panic in the decoder ends playback with an error, not the app
                if let Err(e) = lightcraft_engine::guard::catch("video playback", || work(&for_worker, &path, &repaint)) {
                    for_worker.lock().error = Some(e);
                    repaint.request_repaint();
                }
            })
            .map_err(|e| format!("could not start the video player: {e}"))?;
        Ok(Player {
            photo,
            shared,
            texture: None,
            playing: false,
            position: 0.0,
            duration,
            frame_rate: 0.0,
            started: None,
            generation: 0,
            shown: None,
            ended: false,
            error: None,
            frames_shown: 0,
            frames_dropped: 0,
        })
    }

    pub fn status(&self) -> Value {
        json!({
            "photo": self.photo.0,
            "playing": self.playing,
            "position": self.position,
            "duration": self.duration,
            "frameRate": self.frame_rate,
            "ended": self.ended,
            "error": self.error,
            "framesShown": self.frames_shown,
            "framesDropped": self.frames_dropped,
        })
    }

    fn seek(&mut self, seconds: f64) {
        let to = if seconds.is_finite() { seconds.clamp(0.0, self.duration.max(0.0)) } else { 0.0 };
        self.generation += 1;
        self.position = to;
        self.ended = false;
        self.error = None;
        self.started = None;
        let mut st = self.shared.lock();
        st.generation = self.generation;
        st.seek = Some(to);
        st.frames.clear();
        st.ended = false;
        st.error = None;
        drop(st);
        self.shared.wake.notify_all();
    }

    fn set_playing(&mut self, on: bool) {
        if on && self.ended {
            // Play at the end starts over
            self.seek(0.0);
        }
        self.playing = on && self.error.is_none();
        self.started = None;
    }

    /// Move `frames` frames (negative: back) and stay there.
    fn step(&mut self, frames: i64) {
        self.playing = false;
        let rate = if self.frame_rate > 0.0 { self.frame_rate } else { 30.0 };
        // a quarter frame past the frame's start: the time it is certainly on screen
        self.seek(self.position + (frames as f64 + 0.25) / rate);
    }

    /// Take the frame that is due at app time `now` and put it on screen.
    fn update(&mut self, ctx: &egui::Context, now: f64, max_edge: u32) {
        let mut due = None;
        let mut st = self.shared.lock();
        st.max_edge = Some(max_edge.max(16));
        if let Some((duration, rate)) = st.info {
            if duration > 0.0 {
                self.duration = duration;
            }
            self.frame_rate = rate;
        }
        while st.frames.front().is_some_and(|(g, _)| *g != self.generation) {
            st.frames.pop_front();
        }
        if st.generation == self.generation && st.seek.is_none() {
            self.error.clone_from(&st.error);
        }
        if self.shown != Some(self.generation) {
            // the first frame after opening or a seek is shown at once, playing or not
            due = st.frames.pop_front();
            if due.is_some() {
                self.started = None;
            }
        } else if self.playing {
            let (at, from) = *self.started.get_or_insert((now, self.position));
            let half = if self.frame_rate > 0.0 { 0.5 / self.frame_rate } else { 0.0 };
            let target = from + (now - at) + half;
            while st.frames.front().is_some_and(|(_, f)| f.time <= target) {
                if due.is_some() {
                    // late: a newer frame is due too
                    self.frames_dropped += 1;
                }
                due = st.frames.pop_front();
            }
            if due.is_none() && st.frames.is_empty() {
                if st.ended {
                    self.playing = false;
                    self.ended = true;
                } else {
                    // decoding is behind: the clock waits for it instead of skipping ahead
                    self.started = Some((now, self.position));
                }
            }
        }
        if st.ended && st.frames.is_empty() && self.shown == Some(self.generation) && !self.playing && self.position + 1e-6 >= self.duration {
            self.ended = true;
        }
        drop(st);
        if self.error.is_some() {
            self.playing = false;
        }
        if let Some((generation, frame)) = due {
            self.shared.wake.notify_all();
            self.position = frame.time;
            self.shown = Some(generation);
            self.frames_shown += 1;
            let size = [frame.width as usize, frame.height as usize];
            if size[0] > 0 && size[1] > 0 && frame.rgba.len() == size[0] * size[1] * 4 {
                let image = egui::ColorImage::from_rgba_unmultiplied(size, &frame.rgba);
                match &mut self.texture {
                    Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
                    None => self.texture = Some(ctx.load_texture("video", image, egui::TextureOptions::LINEAR)),
                }
            }
        }
        if self.playing {
            ctx.request_repaint();
        } else if self.shown != Some(self.generation) && self.error.is_none() {
            ctx.request_repaint_after(Duration::from_millis(30));
        }
    }
}

/// The video the loupe shows: the active photo, when it is a video and Detail is open.
fn current(app: &LightcraftApp) -> Option<(PhotoId, String, f64)> {
    if app.ui.view != crate::state::ViewMode::Detail {
        return None;
    }
    let id = app.session.active()?;
    let photo = app.session.catalog.photo(id).filter(|p| lightcraft_engine::video::is_video_photo(p))?;
    let lightcraft_catalog::Source::File { path } = &photo.source else { return None };
    Some((id, path.clone(), photo.duration.unwrap_or(0.0)))
}

/// Is there a video to play (the `video.*` commands are enabled)?
pub fn available(app: &LightcraftApp) -> bool {
    current(app).is_some()
}

/// End playback when the loupe no longer shows the player's video.
pub fn tick(app: &mut LightcraftApp) {
    if app.video.as_ref().is_some_and(|player| current(app).is_none_or(|(id, ..)| id != player.photo)) {
        app.video = None;
    }
}

/// The player for the loupe's video, made on first use.
fn player(app: &mut LightcraftApp) -> Result<&mut Player, String> {
    let (id, path, duration) = current(app).ok_or("open a video in Detail to play it")?;
    if app.video.as_ref().is_none_or(|p| p.photo != id) {
        let ctx = app.tasks.repaint.clone().unwrap_or_default();
        app.video = Some(Player::open(id, path, duration, &ctx)?);
    }
    app.video.as_mut().ok_or_else(|| "the video player is not available".to_string())
}

/// The `video.*` commands; `None`: not one of them.
///
/// - `video.playPause`, `video.play`, `video.pause`
/// - `video.seek {seconds}` or `{fraction}` (0..1 of the length)
/// - `video.step {frames}` (default 1; negative steps back): pauses
/// - `video.stop`: ends playback and shows the poster again
/// - `video.status`
///
/// Each returns the player's status (`null` when there is no player).
pub fn command(app: &mut LightcraftApp, id: &str, p: &Value) -> Option<Result<Value, String>> {
    if !id.starts_with("video.") {
        return None;
    }
    let run = |app: &mut LightcraftApp| -> Result<Value, String> {
        match id {
            "video.status" => {}
            "video.stop" => app.video = None,
            "video.playPause" => {
                let player = player(app)?;
                player.set_playing(!player.playing);
            }
            "video.play" => player(app)?.set_playing(true),
            "video.pause" => player(app)?.set_playing(false),
            "video.seek" => {
                let player = player(app)?;
                let seconds = match (p.get("seconds").and_then(Value::as_f64), p.get("fraction").and_then(Value::as_f64)) {
                    (Some(s), _) => s,
                    (None, Some(f)) => f.clamp(0.0, 1.0) * player.duration,
                    (None, None) => return Err("video.seek: give `seconds` or `fraction`".into()),
                };
                player.seek(seconds);
            }
            "video.step" => {
                let frames = p.get("frames").and_then(Value::as_i64).unwrap_or(1).clamp(-10_000, 10_000);
                player(app)?.step(frames);
            }
            other => return Err(format!("unknown command `{other}`")),
        }
        Ok(app.video.as_ref().map_or(Value::Null, Player::status))
    };
    Some(run(app))
}

fn play_glyph(p: &egui::Painter, c: egui::Pos2, size: f32, playing: bool, color: Color32) {
    if playing {
        for dx in [-0.3, 0.3] {
            p.rect_filled(Rect::from_center_size(c + vec2(dx * size, 0.0), vec2(size * 0.26, size)), 1.0, color);
        }
    } else {
        let points = vec![c + vec2(-size * 0.36, -size * 0.5), c + vec2(size * 0.52, 0.0), c + vec2(-size * 0.36, size * 0.5)];
        p.add(egui::Shape::convex_polygon(points, color, Stroke::NONE));
    }
}

/// The loupe for a video: its poster until it is played, then the frames, and the transport bar.
pub fn show_loupe(app: &mut LightcraftApp, ui: &mut egui::Ui, canvas: Rect, photo: &lightcraft_catalog::Photo) {
    let t = Tokens::get(ui.ctx());
    let id = photo.id;
    let fullscreen = app.ui.fullscreen;
    let bar = Rect::from_min_max(pos2(canvas.left(), canvas.bottom() - BAR_HEIGHT), canvas.max);
    let area = Rect::from_min_max(canvas.min, pos2(canvas.right(), bar.top())).shrink(if fullscreen { 0.0 } else { 24.0 });
    let size = [photo.width.max(1) as usize, photo.height.max(1) as usize];
    let img = crate::panels::detail::fit_texture_rect(area, size);
    app.image_rect = Some(img);
    let ppp = ui.ctx().pixels_per_point();
    let edge = (img.width().max(img.height()) * ppp).ceil().max(16.0) as u32;
    let now = ui.input(|i| i.time);
    if let Some(player) = app.video.as_mut().filter(|p| p.photo == id) {
        player.update(ui.ctx(), now, edge);
    }
    let p = ui.painter_at(canvas);
    let frame = app.video.as_ref().filter(|p| p.photo == id).and_then(|p| p.texture.as_ref().map(egui::TextureHandle::id));
    let whole = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
    match frame {
        Some(texture) => {
            p.image(texture, img, whole, Color32::WHITE);
        }
        None => {
            // the poster: the photo's own render, or what stands in for it until that is ready
            let (w, h) = ((img.width() * ppp) as usize, (img.height() * ppp) as usize);
            if let Some(job) = app.session.loupe_job(id, w.max(8), h.max(8), true) {
                app.renderer.request(Slot::Main, job, 100);
            }
            if app.renderer.textures.get(&Slot::Main).is_none_or(|x| x.photo != id)
                && let Some(q) = app.session.quick_view_job(id, w.max(h).max(8), true)
            {
                app.renderer.request_quick(Slot::Preview, q, 110);
            }
            let mine = |s: Slot| app.renderer.textures.get(&s).filter(|x| x.photo == id);
            if let Some(tex) = mine(Slot::Main).or_else(|| mine(Slot::Preview)).or_else(|| app.renderer.thumb(id)) {
                p.image(tex.tex.id(), crate::panels::detail::fit_texture_rect(img, tex.size), whole, Color32::WHITE);
            }
        }
    }
    let (playing, position, duration, error) = match app.video.as_ref().filter(|p| p.photo == id) {
        Some(player) => (player.playing, player.position, player.duration, player.error.clone()),
        None => (false, 0.0, photo.duration.unwrap_or(0.0), None),
    };
    // a click on the picture plays / pauses; a large play mark says it can be played
    let picture = ui.interact(img, egui::Id::new(("video-loupe", id.0)), Sense::click());
    register(ui.ctx(), "video:picture", img);
    let mut act: Option<(&str, Value)> = None;
    if picture.clicked() {
        act = Some(("video.playPause", json!({})));
    }
    if !playing && error.is_none() {
        let c = img.center();
        let r = (img.width().min(img.height()) * 0.11).clamp(18.0, 44.0);
        p.circle_filled(c, r, Color32::from_black_alpha(if picture.hovered() { 190 } else { 140 }));
        play_glyph(&p, c, r * 0.8, false, Color32::WHITE);
    }
    if let Some(e) = &error {
        let text = format!("{}: {e}", crate::i18n::tr("This video can't be played"));
        let g = p.layout(text, t.font(12.5), Color32::WHITE, (img.width() - 32.0).max(120.0));
        let r = Rect::from_center_size(img.center(), g.size() + vec2(24.0, 16.0));
        p.rect_filled(r, 6.0, Color32::from_black_alpha(200));
        p.galley(r.min + vec2(12.0, 8.0), g, Color32::WHITE);
        register(ui.ctx(), "video:error", r);
    }

    // the transport bar: play / pause, the time, the scrubber, the length
    p.rect_filled(bar, 0.0, t.canvas);
    let mid = bar.center().y;
    let button = Rect::from_center_size(pos2(bar.left() + 30.0, mid), vec2(30.0, 30.0));
    let b = ui.interact(button, egui::Id::new("video-play"), Sense::click()).on_hover_text(crate::i18n::tr("Play / Pause (Space)"));
    register(ui.ctx(), "video:play", button);
    if b.hovered() {
        p.rect_filled(button, 6.0, t.hover);
    }
    play_glyph(&p, button.center(), 13.0, playing, t.text);
    if b.clicked() {
        act = Some(("video.playPause", json!({})));
    }
    let time = p.text(pos2(button.right() + 12.0, mid), Align2::LEFT_CENTER, clock(position), t.font(12.0), t.text_label);
    let length = p.text(pos2(bar.right() - 20.0, mid), Align2::RIGHT_CENTER, clock(duration), t.font(12.0), t.text_dim);
    register(ui.ctx(), "video:time", time);
    let track = Rect::from_min_max(pos2(time.right() + 14.0, mid - 10.0), pos2((length.left() - 14.0).max(time.right() + 15.0), mid + 10.0));
    let s = ui.interact(track, egui::Id::new("video-seek"), Sense::click_and_drag());
    register(ui.ctx(), "video:seek", track);
    let fraction = if duration > 0.0 { (position / duration).clamp(0.0, 1.0) as f32 } else { 0.0 };
    let line = Rect::from_min_max(pos2(track.left(), mid - 1.5), pos2(track.right(), mid + 1.5));
    p.rect_filled(line, 1.5, t.hover);
    let at = track.left() + track.width() * fraction;
    p.rect_filled(Rect::from_min_max(line.min, pos2(at, line.bottom())), 1.5, t.accent);
    p.circle_filled(pos2(at, mid), if s.hovered() || s.dragged() { 6.5 } else { 5.0 }, t.text);
    if (s.clicked() || s.dragged())
        && let Some(pointer) = s.interact_pointer_pos()
        && track.width() > 0.0
    {
        act = Some(("video.seek", json!({"fraction": ((pointer.x - track.left()) / track.width()).clamp(0.0, 1.0)})));
    }
    if let Some((command, params)) = act {
        let _ = app.run(command, params);
    }
}

#[cfg(test)]
mod tests {
    use super::clock;

    #[test]
    fn lengths_read_as_clocks() {
        assert_eq!(clock(0.0), "0:00");
        assert_eq!(clock(0.4), "0:00");
        assert_eq!(clock(6.6), "0:07");
        assert_eq!(clock(754.0), "12:34");
        assert_eq!(clock(3723.0), "1:02:03");
        // nonsense from a damaged file is a zero, never a panic or a wall of digits
        for odd in [-3.0, f64::NAN, f64::INFINITY, 1e300] {
            assert!(clock(odd).len() <= 8, "{odd}: {}", clock(odd));
        }
        assert_eq!(clock(f64::NAN), "0:00");
    }
}
