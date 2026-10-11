//! The sound of a video in the loupe (M14.5): it plays through the host's loudspeaker, the
//! picture follows it, and without a working device the video plays as it did before.

use std::time::{Duration, Instant};

use lightcraft_video::testkit::{Clip, Sound};
use serde_json::{Value, json};

use crate::headless::Headless;
use crate::sound::testing::FakeOut;
use crate::tests_video_play::{position, status, this_clip_in_detail};

const SETTLE: Duration = Duration::from_secs(10);
/// A frame of the headless app.
const FRAME: f64 = 1.0 / 60.0;

/// A 3-second clip (12 frames a second) with sound: silence for a quarter of a second, then a tone.
fn talkie(sound: Sound) -> Clip {
    Clip { width: 160, height: 120, frames: 36, fps: 12, keyint: 12, sound: Some(sound), quicktime: sound == Sound::Pcm8, ..Clip::default() }
}

/// The clip in Detail, with a loudspeaker the test listens to.
fn with_speaker(tag: &str, clip: &Clip, out: &FakeOut) -> (Headless, std::path::PathBuf) {
    let (mut h, _, _, dir) = this_clip_in_detail(tag, "clip.mov", clip);
    h.app.services.audio = Some(Box::new(out.clone()));
    (h, dir)
}

/// Run frames, the loudspeaker playing along, until `done` holds.
fn run(h: &mut Headless, out: &FakeOut, done: impl Fn(&Headless) -> bool) -> bool {
    let t0 = Instant::now();
    while !done(h) {
        if t0.elapsed() > SETTLE {
            return false;
        }
        h.step();
        out.pump(FRAME);
        std::thread::sleep(Duration::from_millis(1));
    }
    true
}

fn audio(h: &Headless) -> Value {
    status(h)["audio"].clone()
}

/// What the loudspeaker was given after `from` samples: (how many, the loudest).
fn heard(out: &FakeOut, from: usize) -> (usize, f32) {
    let device = out.device.lock().unwrap();
    let new = &device.heard[from.min(device.heard.len())..];
    (new.len(), new.iter().fold(0.0f32, |peak, v| peak.max(v.abs())))
}

fn heard_len(out: &FakeOut) -> usize {
    out.device.lock().unwrap().heard.len()
}

/// A video played without its sound. Now the sound plays, in step: the picture follows the
/// samples the loudspeaker has taken, a pause is silent, a seek lands on the sound of that
/// moment, and the volume and mute do what they say.
#[test]
fn a_video_plays_its_sound_and_the_picture_follows_it() {
    for sound in [Sound::Aac, Sound::Pcm16, Sound::Pcm8] {
        let out = FakeOut::new(48_000, 2);
        let (mut h, dir) = with_speaker(&format!("sound-{sound:?}"), &talkie(sound), &out);
        assert!(!out.running(), "looking at the poster opens no device");
        h.app.run("video.play", json!({})).unwrap();
        assert!(run(&mut h, &out, |h| position(h) > 0.6), "{sound:?}: {}", status(&h));
        let a = audio(&h);
        assert_eq!((a["clock"].as_str(), a["playing"].as_bool(), a["silent"].clone()), (Some("audio"), Some(true), Value::Null), "{a}");
        let codec = if sound == Sound::Aac { "AAC" } else { "PCM" };
        assert_eq!(
            (a["codec"].as_str(), a["sampleRate"].as_u64(), a["channels"].as_u64()),
            (Some(codec), Some(u64::from(sound.rate())), Some(sound.channels() as u64))
        );
        // in step: the frame on screen is the one for where the sound is
        let ahead = a["position"].as_f64().unwrap() - position(&h);
        assert!((-0.05..0.14).contains(&ahead), "{sound:?}: the sound is {ahead} s ahead of the picture: {}", status(&h));
        // the tone is heard at its level, on both loudspeakers whatever the clip's own layout
        let (_, peak) = heard(&out, 0);
        assert!((peak - 0.5).abs() < 0.06, "{sound:?}: peak {peak}");
        {
            let device = out.device.lock().unwrap();
            let loud = device.heard.chunks(2).filter(|f| f[0].abs() > 0.2).count();
            let both = device.heard.chunks(2).filter(|f| f[0].abs() > 0.2 && f[1].abs() > 0.05).count();
            assert!(loud > 1000 && both * 10 > loud * 9, "{sound:?}: {both} of {loud} loud frames on both sides");
        }

        // pause: silence, and neither the sound nor the picture moves on
        h.app.run("video.pause", json!({})).unwrap();
        h.step();
        let (held, mark) = (position(&h), heard_len(&out));
        for _ in 0..30 {
            h.step();
            out.pump(FRAME);
        }
        assert_eq!((position(&h), heard(&out, mark).1), (held, 0.0), "{sound:?}: paused");
        assert_eq!(audio(&h)["playing"], false);

        // a seek into the silence at the start: what plays next is silent; then into the tone
        h.app.run("video.seek", json!({"seconds": 0.0})).unwrap();
        h.app.run("video.play", json!({})).unwrap();
        let mark = heard_len(&out);
        assert!(run(&mut h, &out, |h| audio(h)["position"].as_f64().is_some_and(|t| t > 0.15)), "{}", status(&h));
        h.app.run("video.pause", json!({})).unwrap();
        h.step();
        assert!(heard(&out, mark).1 < 0.03, "{sound:?}: the first moments are silent, heard {}", heard(&out, mark).1);
        h.app.run("video.seek", json!({"seconds": 1.5})).unwrap();
        h.app.run("video.play", json!({})).unwrap();
        let mark = heard_len(&out);
        assert!(run(&mut h, &out, |h| audio(h)["position"].as_f64().is_some_and(|t| t > 1.6)), "{}", status(&h));
        assert!((1.5..1.9).contains(&position(&h)), "{}", status(&h));
        assert!(heard(&out, mark).1 > 0.4, "{sound:?}: the tone, heard {}", heard(&out, mark).1);

        // half the volume, then muted: the picture plays on
        let r = h.app.run("video.volume", json!({"level": 0.5})).unwrap();
        assert_eq!(r, json!({"muted": false, "volume": 0.5}));
        h.step();
        out.pump(FRAME);
        let (mark, at) = (heard_len(&out), position(&h));
        assert!(run(&mut h, &out, |h| position(h) > at + 0.2), "{}", status(&h));
        let (_, peak) = heard(&out, mark);
        assert!((peak - 0.25).abs() < 0.04, "{sound:?}: at half volume the peak is {peak}");
        assert_eq!(h.app.run("video.mute", json!({})).unwrap()["muted"], true);
        h.step();
        out.pump(FRAME);
        let (mark, at) = (heard_len(&out), position(&h));
        assert!(run(&mut h, &out, |h| position(h) > at + 0.2), "muted, the picture still plays: {}", status(&h));
        assert_eq!(heard(&out, mark).1, 0.0, "{sound:?}: muted");
        assert_eq!((audio(&h)["muted"].clone(), audio(&h)["clock"].as_str()), (json!(true), Some("audio")));
        // turning the volume up unmutes; odd levels are clamped or refused, never a crash
        assert_eq!(h.app.run("video.volume", json!({"level": 7.0})).unwrap(), json!({"muted": false, "volume": 1.0}));
        assert_eq!(h.app.run("video.volume", json!({"level": -1.0})).unwrap()["volume"], 0.0);
        assert!(h.app.run("video.volume", json!({})).is_err());
        assert!(h.app.run("video.volume", json!({"level": "loud"})).is_err());
        assert_eq!(h.app.run("video.mute", json!({"on": false})).unwrap()["muted"], false);
        h.app.run("video.volume", json!({"level": 1.0})).unwrap();

        // to the end, and the device is given back when the video leaves the loupe
        assert!(run(&mut h, &out, |h| status(h)["ended"] == true), "{sound:?}: {}", status(&h));
        assert!((position(&h) - 35.0 / 12.0).abs() < 1e-6, "the last frame: {}", status(&h));
        assert!(out.running());
        h.app.run("video.stop", json!({})).unwrap();
        h.step();
        assert!(!out.running(), "{sound:?}: the device is stopped with the video");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// The volume controls are on the transport bar, and are remembered with the app's settings.
#[test]
fn the_volume_is_on_the_transport_bar_and_remembered() {
    let out = FakeOut::new(44_100, 2);
    let (mut h, dir) = with_speaker("volume-bar", &talkie(Sound::Pcm16), &out);
    for id in ["video:mute", "video:volume"] {
        let w = h.request("ui.widgets", json!({"filter": id}), SETTLE);
        assert_eq!(w["result"].as_array().map(Vec::len), Some(1), "{id}: {w}");
    }
    h.request("ui.clickWidget", json!({"id": "video:mute"}), SETTLE);
    assert!(h.app.ui.settings.video_muted, "a click on the loudspeaker mutes");
    h.request("ui.clickWidget", json!({"id": "video:mute"}), SETTLE);
    assert!(!h.app.ui.settings.video_muted);
    // the middle of the slider
    h.request("ui.clickWidget", json!({"id": "video:volume"}), SETTLE);
    assert!((h.app.ui.settings.video_volume - 0.5).abs() < 0.05, "{}", h.app.ui.settings.video_volume);
    // saved with the other app settings, and read back
    let saved = serde_json::to_value(&h.app.ui.settings).unwrap();
    assert!((saved["videoVolume"].as_f64().unwrap() - 0.5).abs() < 0.05 && saved["videoMuted"] == false, "{saved}");
    let old: crate::state::AppSettings = serde_json::from_value(json!({})).unwrap();
    assert_eq!((old.video_volume, old.video_muted), (1.0, false), "settings from before there was sound");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Without a loudspeaker, with one that can't be opened, with one that stops, and for a clip
/// without sound, the video plays by the wall clock as before, and says why it is silent.
#[test]
fn a_video_plays_on_without_a_working_loudspeaker() {
    // no device at all (tests, headless runs, the browser)
    let (mut h, _, _, dir) = this_clip_in_detail("sound-none", "clip.mov", &talkie(Sound::Aac));
    h.app.run("video.play", json!({})).unwrap();
    assert!(h.step_until(SETTLE, |h| position(h) > 0.5), "{}", status(&h));
    assert_eq!(audio(&h)["clock"], "wall");
    let _ = std::fs::remove_dir_all(&dir);

    // a device that can't be opened
    let mut out = FakeOut::new(48_000, 2);
    out.broken = Some("no output device".into());
    let (mut h, dir) = with_speaker("sound-broken", &talkie(Sound::Aac), &out);
    h.app.run("video.play", json!({})).unwrap();
    assert!(run(&mut h, &out, |h| position(h) > 0.5), "{}", status(&h));
    assert_eq!((audio(&h)["clock"].as_str(), audio(&h)["silent"].as_str()), (Some("wall"), Some("no output device")));
    assert_eq!(status(&h)["error"], Value::Null, "the picture is not at fault");
    let _ = std::fs::remove_dir_all(&dir);

    // a device that stops taking samples mid-way (unplugged)
    let out = FakeOut::new(48_000, 2);
    let (mut h, dir) = with_speaker("sound-stall", &talkie(Sound::Pcm16), &out);
    h.app.run("video.play", json!({})).unwrap();
    assert!(run(&mut h, &out, |h| position(h) > 0.4), "{}", status(&h));
    assert_eq!(audio(&h)["clock"], "audio");
    out.device.lock().unwrap().stalled = true;
    assert!(run(&mut h, &out, |h| audio(h)["clock"] == "wall"), "{}", status(&h));
    assert_eq!(audio(&h)["silent"], "the audio device stopped");
    let at = position(&h);
    assert!(run(&mut h, &out, |h| position(h) > at + 0.3), "it plays on by the wall clock: {}", status(&h));
    assert!(run(&mut h, &out, |h| status(h)["ended"] == true), "{}", status(&h));
    let _ = std::fs::remove_dir_all(&dir);

    // a clip without sound never opens the device; one whose sound can't be decoded says so
    let out = FakeOut::new(48_000, 2);
    let (mut h, dir) = with_speaker("sound-less", &Clip { sound: None, ..talkie(Sound::Aac) }, &out);
    h.app.run("video.play", json!({})).unwrap();
    assert!(run(&mut h, &out, |h| position(h) > 0.5), "{}", status(&h));
    assert_eq!((audio(&h)["clock"].as_str(), audio(&h)["silent"].as_str()), (Some("wall"), Some("the video has no sound")));
    assert_eq!(out.device.lock().unwrap().starts, 0);
    let _ = std::fs::remove_dir_all(&dir);
}
