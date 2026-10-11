//! Video in the UI: the player behind the loupe, its transport bar, and what the grid says about
//! a video.
//!
//! A [`Player`] is made when a video is first played (looking at a poster costs nothing). It owns
//! one worker thread that opens the file and decodes frames a few ahead; the UI thread only picks
//! the frame that is due and uploads it to a texture, so a slow drive or a heavy clip never stalls
//! a frame of the app. The clip's sound plays through the host's loudspeaker when it has one
//! ([`crate::sound`]), and then the sound is the clock the picture follows.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use egui::{Align2, Color32, Rect, Sense, Stroke, pos2, vec2};
use lightcraft_catalog::PhotoId;
use lightcraft_video::Frame;
use serde_json::{Value, json};

use crate::LightcraftApp;
use crate::render::Slot;
use crate::sound::{AudioOut, Clock, Sound};
use crate::theme::Tokens;
use crate::widgets::register;

/// Seconds of decoded frames kept ahead of the one on screen. The decoders work on many frames
/// at once and hand them over in bursts (a 4K HEVC clip: about 20 frames every 450 ms), so the
/// next burst has to be asked for while this much of the last one is still waiting.
const AHEAD_S: f64 = 0.8;
/// The fewest and the most frames kept ahead, whatever the frame rate.
const AHEAD_MIN: usize = 6;
const AHEAD_MAX: usize = 36;
/// The long edge (pixels) frames are decoded at for playback, at most: a larger view shows them
/// enlarged. Converting and keeping a queue of full 4K frames costs more time and memory than
/// decoding in software leaves.
const PLAY_EDGE: u32 = 1920;

/// Frames to keep ahead at `rate` frames a second.
fn ahead(rate: f64) -> usize {
    let rate = if rate.is_finite() && rate > 0.0 { rate } else { 30.0 };
    ((rate * AHEAD_S).ceil() as usize).clamp(AHEAD_MIN, AHEAD_MAX)
}
/// The transport bar under a video.
const BAR_HEIGHT: f32 = 44.0;
/// Seconds the picture may lag the sound before the sound waits for it.
const BEHIND_S: f64 = 0.25;
/// The most times a player halves the size it decodes at because the picture could not keep up.
const MAX_COARSER: u32 = 2;

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
    /// The clip's sound (none in the browser build).
    sound: Option<Sound>,
    /// The picture fell behind the sound: the sound waits until frames are ready again.
    buffering: bool,
    /// The seek whose frame queue has been full once: playing starts then, not on the first
    /// frames of a burst with the next one still half a second away.
    primed: Option<u64>,
    /// Times the sound had to wait for the picture.
    waits: u64,
    /// Halvings of the size frames are decoded at, one for every second time the sound had to
    /// wait (once can be a busy moment): a softer picture that keeps up is better than a sharp one
    /// that stutters.
    coarser: u32,
    /// Volume and mute as last applied (for the status).
    volume: f32,
    muted: bool,
    /// What the host's output says about the hardware.
    note: Option<String>,
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

/// The decode thread: opens the file, then keeps [`AHEAD_S`] of frames ready from the last seek.
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
    let ahead = ahead(info.frame_rate);
    // the frames waiting to be shown (the queue, and as many again in the decoder's last burst)
    // count as working memory, so background decodes make room
    let pixels = (info.width as usize).saturating_mul(info.height as usize).min(PLAY_EDGE as usize * PLAY_EDGE as usize * 9 / 16);
    let _permit = lightcraft_engine::memory::work_gate().acquire_urgent(pixels.saturating_mul(4).saturating_mul(ahead * 2));
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
                if st.error.is_none() && !st.ended && st.frames.len() < ahead {
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
        let sound_path = path.clone();
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
        // without its sound thread the video plays silently
        let sound = Sound::open(sound_path, ctx);
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
            sound,
            buffering: false,
            primed: None,
            waits: 0,
            coarser: 0,
            volume: 1.0,
            muted: false,
            note: None,
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
            "waits": self.waits,
            "reduced": self.coarser,
            "audio": self.sound.as_ref().map(|sound| sound.status(self.muted, self.volume, self.note.clone())),
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
        self.buffering = false;
        if let Some(sound) = self.sound.as_mut() {
            sound.seek(to, self.generation);
        }
    }

    /// Why this video plays without sound, when it does.
    fn silent(&self) -> Option<String> {
        self.sound.as_ref().and_then(Sound::silent)
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

    /// Take the frame that is due at app time `now` and put it on screen. `out` is the host's
    /// loudspeaker, `volume` 0..1.
    fn update(&mut self, ctx: &egui::Context, now: f64, max_edge: u32, out: Option<&mut (dyn AudioOut + 'static)>, volume: f32, muted: bool) {
        (self.volume, self.muted) = (volume, muted);
        self.note = out.as_ref().and_then(|out| out.note());
        // the sound runs while the picture it belongs to is on screen and keeping up
        let primed = self.primed == Some(self.generation);
        let run = self.playing && self.shown == Some(self.generation) && primed && !self.buffering;
        let gain = if muted { 0.0 } else { volume };
        let clock = self.sound.as_mut().map_or(Clock::Wall, |sound| sound.update(out, now, run, gain));
        let mut due = None;
        let mut st = self.shared.lock();
        if !primed && st.generation == self.generation && st.seek.is_none() && (st.frames.len() >= ahead(self.frame_rate) || st.ended) {
            self.primed = Some(self.generation);
        }
        if self.buffering && (st.frames.len() >= ahead(self.frame_rate) / 2 || st.ended || !self.playing) {
            self.buffering = false;
        }
        st.max_edge = Some((max_edge.min(PLAY_EDGE) >> self.coarser).max(16));
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
        } else if self.playing && clock != Clock::Hold && self.primed == Some(self.generation) {
            let time = match clock {
                // the sound says where playback is; should it stop, the wall clock carries on from here
                Clock::Audio(time) => {
                    self.started = Some((now, time));
                    time
                }
                _ => {
                    let (at, from) = *self.started.get_or_insert((now, self.position));
                    from + (now - at)
                }
            };
            let half = if self.frame_rate > 0.0 { 0.5 / self.frame_rate } else { 0.0 };
            let target = time + half;
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
                } else if matches!(clock, Clock::Audio(_)) {
                    // decoding is behind the sound: a little is caught up by passing over late
                    // frames; more than that and the sound waits for the picture
                    if !self.buffering && time - self.position > BEHIND_S {
                        self.buffering = true;
                        self.waits += 1;
                        self.coarser = u32::try_from(self.waits / 2).unwrap_or(MAX_COARSER).min(MAX_COARSER);
                    }
                } else {
                    // decoding is behind: the clock waits for it instead of skipping ahead
                    self.started = Some((now, self.position));
                }
            }
        } else if self.playing {
            // waiting for the sound or the frame queue to be ready: time does not pass
            self.started = None;
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

/// End playback when the loupe no longer shows the player's video, and the sound with it.
pub fn tick(app: &mut LightcraftApp) {
    if app.video.as_ref().is_some_and(|player| current(app).is_none_or(|(id, ..)| id != player.photo)) {
        app.video = None;
    }
    if app.video.is_none()
        && let Some(out) = app.services.audio.as_mut()
    {
        out.stop();
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
/// - `video.mute {on?}` (without `on`: toggles), `video.volume {level}` (0..1): remembered in the
///   app's settings; these two return `{muted, volume}`
///
/// The others return the player's status (`null` when there is no player).
pub fn command(app: &mut LightcraftApp, id: &str, p: &Value) -> Option<Result<Value, String>> {
    if !id.starts_with("video.") {
        return None;
    }
    let run = |app: &mut LightcraftApp| -> Result<Value, String> {
        match id {
            "video.mute" => {
                let on = p.get("on").and_then(Value::as_bool).unwrap_or(!app.ui.settings.video_muted);
                app.ui.settings.video_muted = on;
                return Ok(json!({"muted": on, "volume": app.ui.settings.video_volume}));
            }
            "video.volume" => {
                let level = p.get("level").and_then(Value::as_f64).filter(|l| l.is_finite()).ok_or("video.volume: give `level` (0 to 1)")?;
                app.ui.settings.video_volume = level.clamp(0.0, 1.0) as f32;
                // turning the volume up is asking to hear it
                app.ui.settings.video_muted &= level <= 0.0;
                return Ok(json!({"muted": app.ui.settings.video_muted, "volume": app.ui.settings.video_volume}));
            }
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

/// A loudspeaker: its cone, and waves for the volume or a cross when muted.
fn speaker_glyph(p: &egui::Painter, c: egui::Pos2, size: f32, level: f32, muted: bool, color: Color32) {
    let (x, half) = (c.x - size * 0.5, size * 0.5);
    let cone = vec![
        pos2(x, c.y - half * 0.36),
        pos2(x + size * 0.24, c.y - half * 0.36),
        pos2(x + size * 0.52, c.y - half * 0.84),
        pos2(x + size * 0.52, c.y + half * 0.84),
        pos2(x + size * 0.24, c.y + half * 0.36),
        pos2(x, c.y + half * 0.36),
    ];
    p.add(egui::Shape::convex_polygon(cone, color, Stroke::NONE));
    let stroke = Stroke::new(1.4, color);
    let from = pos2(x + size * 0.56, c.y);
    if muted || level <= 0.0 {
        let (a, r) = (from + vec2(size * 0.28, 0.0), size * 0.15);
        p.line_segment([a + vec2(-r, -r), a + vec2(r, r)], stroke);
        p.line_segment([a + vec2(-r, r), a + vec2(r, -r)], stroke);
        return;
    }
    // one wave for a low volume, two from about half
    for wave in 0..if level < 0.5 { 1 } else { 2 } {
        let r = size * (0.16 + 0.2 * wave as f32);
        let arc: Vec<egui::Pos2> = (-4..=4).map(|i| i as f32 * 0.2).map(|a: f32| from + vec2(a.cos() * r, a.sin() * r)).collect();
        p.add(egui::Shape::line(arc, stroke));
    }
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
    let (volume, muted) = (app.ui.settings.video_volume, app.ui.settings.video_muted);
    if let Some(player) = app.video.as_mut().filter(|p| p.photo == id) {
        player.update(ui.ctx(), now, edge, app.services.audio.as_deref_mut(), volume, muted);
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
    // the volume: a loudspeaker that mutes, and a short slider
    let silent = app.video.as_ref().filter(|p| p.photo == id).and_then(Player::silent);
    let slider = Rect::from_min_max(pos2(length.left() - 14.0 - 56.0, mid - 10.0), pos2(length.left() - 14.0, mid + 10.0));
    let speaker = Rect::from_center_size(pos2(slider.left() - 20.0, mid), vec2(28.0, 28.0));
    let m = ui.interact(speaker, egui::Id::new("video-mute"), Sense::click());
    let m = match &silent {
        Some(why) => m.on_hover_text(format!("{}: {why}", crate::i18n::tr("No sound"))),
        None => m.on_hover_text(crate::i18n::tr(if muted { "Unmute" } else { "Mute" })),
    };
    register(ui.ctx(), "video:mute", speaker);
    if m.hovered() {
        p.rect_filled(speaker, 6.0, t.hover);
    }
    speaker_glyph(&p, speaker.center(), 16.0, volume, muted, if silent.is_some() { t.text_dim } else { t.text });
    if m.clicked() {
        act = Some(("video.mute", json!({})));
    }
    let v = ui.interact(slider, egui::Id::new("video-volume"), Sense::click_and_drag()).on_hover_text(crate::i18n::tr("Volume"));
    register(ui.ctx(), "video:volume", slider);
    let shown = if muted { 0.0 } else { volume.clamp(0.0, 1.0) };
    let groove = Rect::from_min_max(pos2(slider.left(), mid - 1.5), pos2(slider.right(), mid + 1.5));
    p.rect_filled(groove, 1.5, t.hover);
    let knob = slider.left() + slider.width() * shown;
    p.rect_filled(Rect::from_min_max(groove.min, pos2(knob, groove.bottom())), 1.5, t.text_label);
    p.circle_filled(pos2(knob, mid), if v.hovered() || v.dragged() { 5.5 } else { 4.0 }, t.text);
    if (v.clicked() || v.dragged())
        && let Some(pointer) = v.interact_pointer_pos()
    {
        act = Some(("video.volume", json!({"level": ((pointer.x - slider.left()) / slider.width()).clamp(0.0, 1.0)})));
    }
    let track = Rect::from_min_max(pos2(time.right() + 14.0, mid - 10.0), pos2((speaker.left() - 12.0).max(time.right() + 15.0), mid + 10.0));
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
    fn the_frame_queue_covers_a_decoders_burst() {
        // a 4K HEVC clip comes out about 20 frames at a time: more than that must be kept ahead
        assert_eq!(super::ahead(30.0), 24);
        assert_eq!(super::ahead(12.0), 10);
        assert_eq!((super::ahead(1.0), super::ahead(240.0)), (super::AHEAD_MIN, super::AHEAD_MAX));
        // a rate a damaged file gives is treated as 30
        for odd in [0.0, -5.0, f64::NAN, f64::INFINITY] {
            assert_eq!(super::ahead(odd), 24, "{odd}");
        }
    }

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
