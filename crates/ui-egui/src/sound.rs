//! The sound of the video in the loupe.
//!
//! The host app provides the loudspeaker as an [`AudioOut`] (cpal on the desktop; none in tests,
//! headless runs and the browser). A [`Sound`] owns one worker thread that decodes the clip's
//! sound a little ahead, converts it to the device's rate and channels, and keeps it in a small
//! buffer; the device's callback only copies from that buffer.
//!
//! **The sound is the clock.** While it plays, the number of samples the device has taken says
//! where playback is, and the picture follows ([`Clock::Audio`]): that is what keeps lips in step.
//! Without a device, without sound in the clip, or when the device stops taking samples, playback
//! runs on the wall clock as it did before there was sound.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};

/// Audio output provided by the host app (cpal on the desktop).
pub trait AudioOut {
    /// Start output; `fill(buffer, channels)` is called on the audio thread and must fill the
    /// whole buffer with interleaved samples in -1..1. Starting again replaces the stream.
    /// → the device's sample rate.
    fn start(&mut self, fill: Box<dyn FnMut(&mut [f32], usize) + Send>) -> Result<u32, String>;
    /// Stop output (cheap when nothing plays).
    fn stop(&mut self);
    /// Device output channels: what `fill` is called with.
    fn channels(&self) -> usize {
        2
    }
    /// Frames the device has taken since `start`; `None` when the stream failed or is not running.
    fn played_frames(&self) -> Option<u64>;
    /// Something the user should know about the hardware (the default device could not be opened
    /// and another one is used; there is no output device), or `None`.
    fn note(&self) -> Option<String> {
        None
    }
}

/// Seconds of converted sound kept ready for the device.
const AHEAD_S: f64 = 0.5;
/// Seconds of sound that must be ready before playback starts or resumes after a seek.
const PRIMED_S: f64 = 0.1;
/// How long (s) the device may take no samples while playing before the wall clock takes over.
pub const STALL_S: f64 = 1.0;
/// How long (s) playback waits to learn whether the clip has sound before starting without it.
const DECIDE_S: f64 = 2.0;
/// The longest silence (s) put in front of sound that starts later than where playback is.
const MAX_LEAD_S: f64 = 5.0;

/// One frame of `input` channels as one frame of `out` channels. Input is front left, front
/// right, centre, low frequency, then the surrounds (what `lightcraft-video` gives).
fn map_frame(input: &[f32], out: &mut [f32]) {
    let at = |i: usize| input.get(i).copied().unwrap_or(0.0);
    let (n, m) = (input.len(), out.len());
    out.fill(0.0);
    let (left, right) = match n {
        0 => (0.0, 0.0),
        1 => (at(0), at(0)),
        2 => (at(0), at(1)),
        _ => {
            // more than two: centre and surrounds are folded into the front pair
            let (ls, rs) = match n {
                3 => (0.0, 0.0),
                4 => (at(3) * 0.5, at(3) * 0.5),
                5 => (at(3), at(4)),
                6 | 7 => (at(4), at(5)),
                _ => (at(4) + at(6), at(5) + at(7)),
            };
            if m >= n {
                // the device has all of them: nothing to fold
                for (o, v) in out.iter_mut().zip(input) {
                    *o = *v;
                }
                return;
            }
            ((at(0) + 0.707 * at(2) + 0.707 * ls) * 0.6, (at(1) + 0.707 * at(2) + 0.707 * rs) * 0.6)
        }
    };
    match out {
        [] => {}
        [only] => *only = (left + right) * 0.5,
        [l, r, ..] => (*l, *r) = (left, right),
    }
}

/// Decoded sound → what the device plays: its channels, at its rate (linear interpolation).
struct Converter {
    /// Input frames per output frame.
    step: f64,
    channels_in: usize,
    channels_out: usize,
    /// The last frame of the block before (as device channels), once there has been one.
    previous: Option<Vec<f32>>,
    /// Where the next output frame lies, in input frames from the start of the next block
    /// (-1..0: between the previous block's last frame and this one's first).
    position: f64,
}

impl Converter {
    fn new(rate_in: u32, rate_out: u32, channels_in: usize, channels_out: usize) -> Self {
        let step = if rate_in == 0 || rate_out == 0 { 1.0 } else { f64::from(rate_in) / f64::from(rate_out) };
        Converter { step, channels_in: channels_in.max(1), channels_out: channels_out.max(1), previous: None, position: 0.0 }
    }

    /// Start over (after a seek the next block does not follow the last one).
    fn reset(&mut self) {
        self.previous = None;
        self.position = 0.0;
    }

    /// Add `input` (interleaved, the clip's channels and rate) to `out` (the device's).
    fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let (cin, cout) = (self.channels_in, self.channels_out);
        let frames = input.len() / cin;
        if frames == 0 {
            return;
        }
        let mut mapped = vec![0.0f32; frames * cout];
        for (frame, to) in input.chunks_exact(cin).zip(mapped.chunks_exact_mut(cout)) {
            map_frame(frame, to);
        }
        if (self.step - 1.0).abs() < 1e-9 {
            out.extend_from_slice(&mapped);
            return;
        }
        let silence = vec![0.0f32; cout];
        let frame = |i: i64| -> &[f32] {
            match usize::try_from(i) {
                Ok(i) => mapped.get(i * cout..(i + 1) * cout).unwrap_or(&silence),
                // before this block: the last frame of the one before (or this block's first)
                Err(_) => self.previous.as_deref().unwrap_or_else(|| mapped.get(..cout).unwrap_or(&silence)),
            }
        };
        let last = (frames - 1) as f64;
        out.reserve(((frames as f64 / self.step) as usize + 2) * cout);
        let mut position = self.position;
        while position <= last {
            let whole = position.floor();
            let (a, b, t) = (frame(whole as i64), frame(whole as i64 + 1), (position - whole) as f32);
            // the frame after the block's last is not known yet: `position <= last` only reaches
            // it with t == 0
            out.extend(a.iter().zip(if t > 0.0 { b } else { a }).map(|(a, b)| a + (b - a) * t));
            position += self.step;
        }
        self.position = position - frames as f64;
        self.previous = mapped.get((frames - 1) * cout..).map(<[f32]>::to_vec);
    }
}

/// What the UI thread, the decode thread and the device's callback share.
#[derive(Default)]
struct State {
    /// Converted sound waiting for the device (interleaved, the device's channels).
    samples: VecDeque<f32>,
    /// A position to continue from, not yet taken by the worker.
    seek: Option<f64>,
    /// Counts seeks (the player's): sound of an older one is discarded.
    generation: u64,
    stop: bool,
    /// The last of the sound has been decoded.
    ended: bool,
    /// Enough sound for the current seek is ready (or there is no more).
    primed: bool,
    /// The device may take samples (playing, and the picture is ready).
    running: bool,
    /// Volume, 0 when muted.
    gain: f32,
    /// Frames the device has taken since the seek.
    consumed: u64,
    /// Seconds: where the sound in `samples` started (the seek).
    base: f64,
    /// Times the device asked for sound and there was not enough.
    underruns: u64,
    /// The device's (sample rate, channels), once the stream runs.
    device: Option<(u32, usize)>,
    /// The clip's sound (codec, sample rate, channels), once the file is open.
    track: Option<(String, u32, usize)>,
    /// The worker knows whether the clip has sound it can play.
    decided: bool,
    /// Why there is no sound.
    silent: Option<String>,
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

/// The device's callback: copy what is ready, silence for the rest.
fn fill(shared: &Shared, buffer: &mut [f32], channels: usize) {
    let mut st = shared.lock();
    let usable = st.running && channels > 0 && st.device.is_some_and(|(_, c)| c == channels);
    let n = if usable { buffer.len().min(st.samples.len()) / channels * channels } else { 0 };
    let gain = if st.gain.is_finite() { st.gain.clamp(0.0, 1.0) } else { 0.0 };
    for (out, sample) in buffer.iter_mut().zip(st.samples.drain(..n)) {
        *out = sample * gain;
    }
    if let Some(rest) = buffer.get_mut(n..) {
        rest.fill(0.0);
    }
    st.consumed += n.checked_div(channels).unwrap_or(0) as u64;
    if usable && n < buffer.len() && !st.ended {
        st.underruns += 1;
    }
    drop(st);
    shared.wake.notify_all();
}

/// The decode thread: opens the file, waits for the device, then keeps [`AHEAD_S`] of sound ready.
#[cfg(not(target_arch = "wasm32"))]
fn work(shared: &Shared, path: &str, repaint: &egui::Context) {
    let silent = |why: String| {
        let mut st = shared.lock();
        st.silent = Some(why);
        st.decided = true;
        st.primed = true;
        st.ended = true;
        drop(st);
        repaint.request_repaint();
    };
    let opened = lightcraft_video::Video::open_path(path).map_err(|e| e.to_string()).and_then(|video| {
        let info = video.info().audio.clone().ok_or("the video has no sound")?;
        let reader = video.audio_reader().map_err(|e| e.to_string())?;
        Ok((reader, info))
    });
    let (mut reader, info) = match opened {
        Ok(r) => r,
        Err(e) => return silent(e),
    };
    {
        let mut st = shared.lock();
        st.track = Some((info.codec.name().to_string(), reader.sample_rate(), reader.channels()));
        st.decided = true;
    }
    repaint.request_repaint();
    let (rate, channels) = {
        let mut st = shared.lock();
        loop {
            if st.stop || st.silent.is_some() {
                return;
            }
            if let Some(device) = st.device {
                break device;
            }
            st = shared.wake.wait_timeout(st, Duration::from_millis(100)).unwrap_or_else(PoisonError::into_inner).0;
        }
    };
    let mut converter = Converter::new(reader.sample_rate(), rate, reader.channels(), channels);
    let per_second = rate as usize * channels;
    let (high, primed) = ((per_second as f64 * AHEAD_S) as usize, (per_second as f64 * PRIMED_S) as usize);
    let mut generation = 0u64;
    // after a seek: where the sound should start, until its first block is in
    let mut lead: Option<f64> = None;
    loop {
        {
            let mut st = shared.lock();
            loop {
                if st.stop {
                    return;
                }
                if let Some(to) = st.seek.take() {
                    generation = st.generation;
                    st.samples.clear();
                    st.consumed = 0;
                    st.base = to;
                    st.ended = false;
                    st.primed = false;
                    drop(st);
                    converter.reset();
                    lead = Some(to);
                    if let Err(e) = reader.seek(to) {
                        return silent(e.to_string());
                    }
                    break;
                }
                if !st.ended && st.samples.len() < high {
                    break;
                }
                st = shared.wake.wait_timeout(st, Duration::from_millis(100)).unwrap_or_else(PoisonError::into_inner).0;
            }
        }
        let next = reader.next_block();
        let mut converted = Vec::new();
        if let Ok(Some(block)) = &next {
            // sound that starts later than where playback is: silence up to it
            if let Some(from) = lead.take() {
                let gap = (block.time - from).clamp(0.0, MAX_LEAD_S);
                converted.resize((gap * f64::from(rate)).round() as usize * channels, 0.0);
            }
            converter.push(&block.samples, &mut converted);
        }
        let mut st = shared.lock();
        if st.generation != generation || st.seek.is_some() {
            continue;
        }
        let was = st.primed;
        match next {
            Ok(Some(_)) => {
                st.samples.extend(converted);
                st.primed |= st.samples.len() >= primed;
            }
            Ok(None) => (st.ended, st.primed) = (true, true),
            Err(e) => {
                // what is ready still plays; then the picture carries on without sound
                (st.ended, st.primed) = (true, true);
                st.silent = Some(e.to_string());
            }
        }
        let now = st.primed;
        drop(st);
        if now != was {
            repaint.request_repaint();
        }
    }
}

/// What playback follows this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Clock {
    /// No sound drives playback: time passes by the wall clock.
    Wall,
    /// The sound is not ready yet: playback waits.
    Hold,
    /// Seconds: where the sound is.
    Audio(f64),
}

/// The sound of one video player.
pub(crate) struct Sound {
    shared: Arc<Shared>,
    /// The output stream was started for this sound.
    started: bool,
    /// The device failed or stalled: the wall clock is used from then on.
    failed: Option<String>,
    /// App time this sound was made (to stop waiting for a file that does not open).
    since: Option<f64>,
    /// The device's frame counter and the app time it last moved.
    seen: (Option<u64>, f64),
    clock: Clock,
}

impl Drop for Sound {
    fn drop(&mut self) {
        // the worker ends by itself; the stream is stopped by the player's owner
        let mut st = self.shared.lock();
        st.stop = true;
        st.running = false;
        st.samples.clear();
        drop(st);
        self.shared.wake.notify_all();
    }
}

impl Sound {
    /// The sound of the video at `path`: the file is opened on a worker thread.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn open(path: String, ctx: &egui::Context) -> Option<Self> {
        let shared = Arc::new(Shared::default());
        shared.lock().seek = Some(0.0);
        let (for_worker, repaint) = (shared.clone(), ctx.clone());
        std::thread::Builder::new()
            .name("lc-audio-decode".into())
            .spawn(move || {
                // a panic in the decoder ends the sound, not playback and not the app
                if let Err(e) = lightcraft_engine::guard::catch("video sound", || work(&for_worker, &path, &repaint)) {
                    let mut st = for_worker.lock();
                    st.silent = Some(e);
                    (st.decided, st.primed, st.ended) = (true, true, true);
                    drop(st);
                    repaint.request_repaint();
                }
            })
            .ok()?;
        Some(Sound { shared, started: false, failed: None, since: None, seen: (None, 0.0), clock: Clock::Wall })
    }

    /// Continue from `seconds`; `generation` is the player's count of seeks.
    pub(crate) fn seek(&mut self, seconds: f64, generation: u64) {
        let mut st = self.shared.lock();
        st.generation = generation;
        st.seek = Some(seconds);
        st.samples.clear();
        st.consumed = 0;
        st.base = seconds;
        st.primed = false;
        st.ended = false;
        st.running = false;
        drop(st);
        self.shared.wake.notify_all();
    }

    /// Once a frame: start the device when the clip has sound, let it run (or not), and say
    /// what playback follows. `out` is the host's output, when it has one.
    pub(crate) fn update(&mut self, out: Option<&mut (dyn AudioOut + 'static)>, now: f64, run: bool, gain: f32) -> Clock {
        self.clock = self.advance(out, now, run, gain);
        self.clock
    }

    fn advance(&mut self, out: Option<&mut (dyn AudioOut + 'static)>, now: f64, run: bool, gain: f32) -> Clock {
        let since = *self.since.get_or_insert(now);
        let Some(out) = out else { return Clock::Wall };
        if self.failed.is_some() {
            return Clock::Wall;
        }
        let mut st = self.shared.lock();
        st.gain = gain;
        if st.silent.is_some() && st.samples.is_empty() {
            st.running = false;
            return Clock::Wall;
        }
        if !st.decided {
            // the file is being opened: wait for it, but not for ever
            return if now - since < DECIDE_S { Clock::Hold } else { Clock::Wall };
        }
        if !self.started {
            drop(st);
            let shared = self.shared.clone();
            let rate = out.start(Box::new(move |buffer, channels| fill(&shared, buffer, channels)));
            let channels = out.channels();
            let mut st = self.shared.lock();
            match rate {
                Ok(rate) if rate > 0 && channels > 0 => {
                    st.device = Some((rate, channels));
                    self.started = true;
                    self.seen = (None, now);
                }
                Ok(_) => self.failed = Some("the output device has no channels or no sample rate".into()),
                Err(e) => self.failed = Some(e),
            }
            drop(st);
            self.shared.wake.notify_all();
            return if self.failed.is_some() { Clock::Wall } else { Clock::Hold };
        }
        let Some((rate, _)) = st.device else { return Clock::Wall };
        if !st.primed || st.seek.is_some() {
            st.running = false;
            return Clock::Hold;
        }
        if st.ended && st.samples.is_empty() {
            // the sound is over before the picture: the rest plays by the wall clock
            st.running = false;
            return Clock::Wall;
        }
        st.running = run;
        let time = st.base + st.consumed as f64 / f64::from(rate.max(1));
        drop(st);
        // a device that stops taking samples hands the clock back
        let played = out.played_frames();
        if !run || played != self.seen.0 {
            self.seen = (played, now);
        } else if now - self.seen.1 > STALL_S {
            self.failed = Some("the audio device stopped".into());
            out.stop();
            self.shared.lock().running = false;
            return Clock::Wall;
        }
        Clock::Audio(time)
    }

    /// For `video.status` / `ui.inspect`.
    pub(crate) fn status(&self, muted: bool, volume: f32, note: Option<String>) -> Value {
        let st = self.shared.lock();
        let (codec, rate, channels) = match &st.track {
            Some((codec, rate, channels)) => (json!(codec), json!(rate), json!(channels)),
            None => (Value::Null, Value::Null, Value::Null),
        };
        json!({
            "codec": codec,
            "sampleRate": rate,
            "channels": channels,
            "playing": st.running,
            "muted": muted,
            "volume": volume,
            "underruns": st.underruns,
            "position": match self.clock { Clock::Audio(time) => json!(time), _ => Value::Null },
            "clock": match self.clock { Clock::Audio(_) => "audio", Clock::Hold => "waiting", Clock::Wall => "wall" },
            "silent": self.failed.clone().or_else(|| st.silent.clone()),
            "note": note,
        })
    }

    /// Why this video plays without sound, when it does.
    pub(crate) fn silent(&self) -> Option<String> {
        self.failed.clone().or_else(|| self.shared.lock().silent.clone())
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A loudspeaker for tests: it takes samples when the test says time has passed.

    use std::sync::{Arc, Mutex, PoisonError};

    use super::AudioOut;

    type Fill = Box<dyn FnMut(&mut [f32], usize) + Send>;

    #[derive(Default)]
    pub(crate) struct Device {
        fill: Option<Fill>,
        played: u64,
        /// Everything the device was given (interleaved).
        pub heard: Vec<f32>,
        pub starts: u32,
        /// The device takes no more samples (as an unplugged one).
        pub stalled: bool,
    }

    /// A [`AudioOut`] whose device is shared with the test.
    #[derive(Clone)]
    pub(crate) struct FakeOut {
        pub device: Arc<Mutex<Device>>,
        pub rate: u32,
        pub channels: usize,
        /// `start` fails with this.
        pub broken: Option<String>,
    }

    impl FakeOut {
        pub(crate) fn new(rate: u32, channels: usize) -> Self {
            FakeOut { device: Arc::default(), rate, channels, broken: None }
        }

        /// The device plays for `seconds`.
        pub(crate) fn pump(&self, seconds: f64) {
            let mut device = self.device.lock().unwrap_or_else(PoisonError::into_inner);
            let frames = (seconds * f64::from(self.rate)).round() as usize;
            if device.stalled || frames == 0 {
                return;
            }
            let mut buffer = vec![0.0f32; frames * self.channels];
            if let Some(fill) = device.fill.as_mut() {
                fill(&mut buffer, self.channels);
                device.played += frames as u64;
                device.heard.extend(buffer);
            }
        }

        pub(crate) fn running(&self) -> bool {
            self.device.lock().unwrap_or_else(PoisonError::into_inner).fill.is_some()
        }
    }

    impl AudioOut for FakeOut {
        fn start(&mut self, fill: Fill) -> Result<u32, String> {
            if let Some(why) = &self.broken {
                return Err(why.clone());
            }
            let mut device = self.device.lock().unwrap_or_else(PoisonError::into_inner);
            device.fill = Some(fill);
            device.played = 0;
            device.starts += 1;
            Ok(self.rate)
        }

        fn stop(&mut self) {
            self.device.lock().unwrap_or_else(PoisonError::into_inner).fill = None;
        }

        fn channels(&self) -> usize {
            self.channels
        }

        fn played_frames(&self) -> Option<u64> {
            let device = self.device.lock().unwrap_or_else(PoisonError::into_inner);
            device.fill.as_ref().map(|_| device.played)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_are_mapped_to_what_the_device_has() {
        let map = |input: &[f32], channels: usize| {
            let mut out = vec![9.0; channels];
            map_frame(input, &mut out);
            out
        };
        assert_eq!(map(&[0.5], 2), [0.5, 0.5], "one channel is heard on both sides");
        assert_eq!(map(&[0.5, -0.25], 2), [0.5, -0.25]);
        assert_eq!(map(&[0.5, 0.25], 1), [0.375]);
        assert_eq!(map(&[0.5], 6), [0.5, 0.5, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(map(&[0.1, 0.2, 0.3, 0.4, 0.5, 0.6], 6), [0.1, 0.2, 0.3, 0.4, 0.5, 0.6]);
        // 5.1 on two loudspeakers: centre and surrounds fold in, the low frequency channel does not
        let folded = map(&[0.0, 0.0, 1.0, 1.0, 0.0, 0.0], 2);
        assert!((folded[0] - 0.4242).abs() < 1e-3 && folded[0] == folded[1], "{folded:?}");
        let surround = map(&[0.0, 0.0, 0.0, 0.0, 1.0, 0.0], 2);
        assert!(surround[0] > 0.4 && surround[1] == 0.0, "{surround:?}");
        assert_eq!(map(&[], 2), [0.0, 0.0]);
        assert!(map(&[1.0, 1.0], 0).is_empty());
    }

    /// A slow ramp keeps its slope and its values across block borders at any rate.
    #[test]
    fn sound_is_converted_to_the_devices_rate_without_gaps() {
        for (rate_in, rate_out) in [(8000u32, 48_000u32), (44_100, 48_000), (48_000, 44_100), (48_000, 48_000), (96_000, 48_000)] {
            let mut converter = Converter::new(rate_in, rate_out, 1, 2);
            let input: Vec<f32> = (0..rate_in / 10).map(|i| i as f32 / rate_in as f32).collect();
            let mut out = Vec::new();
            // uneven blocks, as a decoder gives them
            for block in input.chunks(333) {
                converter.push(block, &mut out);
            }
            let frames = out.len() / 2;
            let expected = (rate_out / 10) as usize;
            assert!(frames.abs_diff(expected) <= 6, "{rate_in} → {rate_out}: {frames} frames for {expected}");
            for (i, frame) in out.chunks(2).enumerate() {
                let want = i as f32 / rate_out as f32;
                assert!((frame[0] - want).abs() < 2e-4 && frame[0] == frame[1], "{rate_in} → {rate_out}, frame {i}: {} for {want}", frame[0]);
            }
        }
        // nothing in, nothing out; odd rates and channel counts are answered, not a panic
        let mut converter = Converter::new(0, 0, 0, 0);
        let mut out = Vec::new();
        converter.push(&[], &mut out);
        converter.push(&[0.5, 0.25], &mut out);
        assert_eq!(out, [0.5, 0.25]);
        converter.reset();
    }

    /// (samples waiting, frames taken, underruns), under one lock.
    fn counts(shared: &Shared) -> (usize, u64, u64) {
        let st = shared.lock();
        (st.samples.len(), st.consumed, st.underruns)
    }

    #[test]
    fn the_device_gets_silence_unless_the_sound_runs() {
        let shared = Shared::default();
        {
            let mut st = shared.lock();
            st.samples.extend([0.5f32; 8]);
            st.device = Some((48_000, 2));
            st.gain = 0.5;
        }
        let mut buffer = [9.0f32; 6];
        fill(&shared, &mut buffer, 2);
        assert_eq!(buffer, [0.0; 6], "paused: silence, and nothing is used up");
        assert_eq!(counts(&shared), (8, 0, 0));
        shared.lock().running = true;
        fill(&shared, &mut buffer, 2);
        assert_eq!(buffer, [0.25; 6], "the volume is applied");
        assert_eq!(counts(&shared), (2, 3, 0));
        // not enough left: what there is, then silence, counted as an underrun
        let mut buffer = [9.0f32; 6];
        fill(&shared, &mut buffer, 2);
        assert_eq!(buffer, [0.25, 0.25, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(counts(&shared), (0, 4, 1));
        // a callback with other channels than the stream was started with, or none, is silence
        shared.lock().samples.extend([0.5f32; 8]);
        let mut odd = [9.0f32; 3];
        fill(&shared, &mut odd, 3);
        fill(&shared, &mut odd, 0);
        assert_eq!(odd, [0.0; 3]);
        // a volume that is not a number mutes
        shared.lock().gain = f32::NAN;
        let mut buffer = [9.0f32; 2];
        fill(&shared, &mut buffer, 2);
        assert_eq!(buffer, [0.0; 2]);
    }
}
