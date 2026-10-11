//! The loudspeaker: cpal audio output for the sound of videos (`lightcraft_ui_egui::sound`).
//!
//! The same approach as FilmCraft's desktop app: the default output device at its own format,
//! with the samples the device has taken as the playback clock. When the default device cannot
//! be opened another one is used and a note says so; without any device videos play silently.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use lightcraft_ui_egui::sound::AudioOut;

type Fill = Box<dyn FnMut(&mut [f32], usize) + Send>;

/// A driver buffer larger than this (samples) is not converted: it gets silence.
const MAX_BUFFER: usize = 1 << 20;

pub struct CpalOut {
    stream: Option<cpal::Stream>,
    played: Arc<AtomicU64>,
    failed: Arc<AtomicBool>,
    channels: u16,
    note: Option<Note>,
    /// The fallback picked when the default device could not be opened, as (host, device)
    /// names, so later starts reuse it instead of probing every device again.
    fallback: Mutex<Option<(String, String)>>,
}

/// Something about the hardware the user should know.
#[derive(Clone, Debug, PartialEq)]
enum Note {
    /// The default output device cannot be opened; this named device is used instead.
    Fallback(String),
    /// No output device at all: videos play without sound.
    NoDevice,
}

impl Note {
    fn text(&self) -> String {
        match self {
            Note::Fallback(device) => format!("The default audio output device cannot be opened; using '{device}'"),
            Note::NoDevice => "No audio output device found: videos play without sound".to_string(),
        }
    }
}

/// The output device to use, with a note when it is a fallback.
struct DeviceChoice {
    device: cpal::Device,
    fallback: Option<Note>,
}

/// Fallback order when the default device cannot be opened: the sound-server bridges first
/// (`pipewire`, `pulse`: they share the card with every other app), then other named devices,
/// and raw `hw:` / `plughw:` devices last (opening one takes the card for itself and silences
/// the rest of the desktop).
fn fallback_rank(name: &str) -> u8 {
    let n = name.to_ascii_lowercase();
    if n == "pipewire" || n.starts_with("pipewire:") {
        0
    } else if n == "pulse" || n.starts_with("pulse:") {
        1
    } else if n.starts_with("hw:") || n.starts_with("plughw:") {
        3
    } else {
        2
    }
}

/// The host's default output device, or, when that cannot be opened (a broken ALSA `default`),
/// one that can. The other devices are probed only after the default fails, in
/// [`fallback_rank`] order, and the pick is remembered in `cache`.
fn output_device(host: &cpal::Host, cache: &Mutex<Option<(String, String)>>) -> Option<DeviceChoice> {
    if let Some(device) = host.default_output_device()
        && device.default_output_config().is_ok()
    {
        return Some(DeviceChoice { device, fallback: None });
    }
    let host_name = host.id().name().to_string();
    let mut cache = cache.lock().unwrap_or_else(PoisonError::into_inner);
    let mut devices: Vec<(String, cpal::Device)> = host.output_devices().ok()?.filter_map(|d| d.name().ok().map(|n| (n, d))).collect();
    if let Some((cached_host, cached)) = cache.as_ref()
        && *cached_host == host_name
        && let Some(i) = devices.iter().position(|(n, _)| n == cached)
    {
        let (found, device) = devices.swap_remove(i);
        return Some(DeviceChoice { device, fallback: Some(Note::Fallback(found)) });
    }
    devices.sort_by_key(|(n, _)| fallback_rank(n));
    let (found, device) = devices.into_iter().find(|(_, d)| d.default_output_config().is_ok())?;
    log::warn!("the default audio output device cannot be opened; falling back to '{found}'");
    *cache = Some((host_name, found.clone()));
    Some(DeviceChoice { device, fallback: Some(Note::Fallback(found)) })
}

fn stream_error(failed: &AtomicBool, e: &cpal::StreamError) {
    if !failed.swap(true, Ordering::SeqCst) {
        log::warn!("audio stream error: {e}");
    }
}

fn convert_samples<T: cpal::Sample + cpal::FromSample<f32>>(input: &[f32], output: &mut [T]) {
    // Integer formats hold -1 up to just under 1: a full-scale +1 must not wrap.
    let upper = f32::from_bits(1.0_f32.to_bits() - 1);
    for (sample, converted) in input.iter().zip(output) {
        *converted = T::from_sample(if sample.is_finite() { sample.clamp(-1.0, upper) } else { 0.0 });
    }
}

impl CpalOut {
    /// Nothing is opened here: the device is looked for when a video first plays.
    pub fn new() -> Self {
        CpalOut {
            stream: None,
            played: Arc::new(AtomicU64::new(0)),
            failed: Arc::new(AtomicBool::new(false)),
            channels: 2,
            note: None,
            fallback: Mutex::new(None),
        }
    }

    /// A stream for a device whose samples are not `f32`: filled as `f32`, then converted.
    fn converted_stream<T>(&self, device: &cpal::Device, config: &cpal::StreamConfig, mut fill: Fill) -> Result<cpal::Stream, cpal::BuildStreamError>
    where
        T: cpal::SizedSample + cpal::FromSample<f32>,
    {
        let channels = usize::from(config.channels).max(1);
        let (played, failed) = (self.played.clone(), self.failed.clone());
        let error_flag = failed.clone();
        let mut scratch: Vec<f32> = Vec::new();
        device.build_output_stream(
            config,
            move |buffer: &mut [T], _: &cpal::OutputCallbackInfo| {
                // an absurd driver buffer must not take the audio thread down
                if buffer.len() > MAX_BUFFER || scratch.try_reserve(buffer.len().saturating_sub(scratch.len())).is_err() {
                    buffer.fill(T::EQUILIBRIUM);
                    if !failed.swap(true, Ordering::SeqCst) {
                        log::warn!("the audio conversion buffer could not be allocated");
                    }
                    return;
                }
                scratch.resize(buffer.len(), 0.0);
                fill(&mut scratch, channels);
                convert_samples(&scratch, buffer);
                played.fetch_add((buffer.len() / channels) as u64, Ordering::SeqCst);
            },
            move |e| stream_error(&error_flag, &e),
            None,
        )
    }
}

impl AudioOut for CpalOut {
    fn start(&mut self, mut fill: Fill) -> Result<u32, String> {
        self.stop();
        let host = cpal::default_host();
        let Some(DeviceChoice { device, fallback }) = output_device(&host, &self.fallback) else {
            self.note = Some(Note::NoDevice);
            return Err("no audio output device".into());
        };
        self.note = fallback;
        let supported = device.default_output_config().map_err(|e| e.to_string())?;
        let (channels, rate) = (usize::from(supported.channels()), supported.sample_rate().0);
        if channels == 0 || rate == 0 {
            return Err("the output device returned an invalid channel count or sample rate".into());
        }
        let config: cpal::StreamConfig = supported.clone().into();
        self.channels = supported.channels();
        self.played.store(0, Ordering::SeqCst);
        self.failed.store(false, Ordering::SeqCst);
        let (played, failed) = (self.played.clone(), self.failed.clone());
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => device.build_output_stream(
                &config,
                move |buffer: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    fill(buffer, channels);
                    played.fetch_add((buffer.len() / channels) as u64, Ordering::SeqCst);
                },
                move |e| stream_error(&failed, &e),
                None,
            ),
            cpal::SampleFormat::I8 => self.converted_stream::<i8>(&device, &config, fill),
            cpal::SampleFormat::I16 => self.converted_stream::<i16>(&device, &config, fill),
            cpal::SampleFormat::I24 => self.converted_stream::<cpal::I24>(&device, &config, fill),
            cpal::SampleFormat::I32 => self.converted_stream::<i32>(&device, &config, fill),
            cpal::SampleFormat::I64 => self.converted_stream::<i64>(&device, &config, fill),
            cpal::SampleFormat::U8 => self.converted_stream::<u8>(&device, &config, fill),
            cpal::SampleFormat::U16 => self.converted_stream::<u16>(&device, &config, fill),
            cpal::SampleFormat::U32 => self.converted_stream::<u32>(&device, &config, fill),
            cpal::SampleFormat::U64 => self.converted_stream::<u64>(&device, &config, fill),
            cpal::SampleFormat::F64 => self.converted_stream::<f64>(&device, &config, fill),
            other => return Err(format!("unsupported sample format {other:?}")),
        }
        .map_err(|e| e.to_string())?;
        stream.play().map_err(|e| e.to_string())?;
        self.stream = Some(stream);
        Ok(rate)
    }

    fn stop(&mut self) {
        self.stream = None;
    }

    fn channels(&self) -> usize {
        usize::from(self.channels)
    }

    fn played_frames(&self) -> Option<u64> {
        self.stream.as_ref().filter(|_| !self.failed.load(Ordering::SeqCst)).map(|_| self.played.load(Ordering::SeqCst))
    }

    fn note(&self) -> Option<String> {
        self.note.as_ref().map(Note::text)
    }
}

#[cfg(test)]
mod tests {
    use cpal::Sample;

    use super::*;

    #[test]
    fn fallback_prefers_sound_servers_over_raw_hardware() {
        let mut names = vec!["plughw:CARD=PCH,DEV=0", "hw:CARD=PCH,DEV=0", "sysdefault:CARD=PCH", "pulse", "pipewire"];
        names.sort_by_key(|n| fallback_rank(n));
        assert_eq!(names, ["pipewire", "pulse", "sysdefault:CARD=PCH", "plughw:CARD=PCH,DEV=0", "hw:CARD=PCH,DEV=0"]);
    }

    #[test]
    fn notes_name_the_fallback_device() {
        assert!(Note::Fallback("pipewire".into()).text().contains("'pipewire'"));
        assert!(!Note::NoDevice.text().is_empty());
    }

    #[test]
    fn integer_conversion_clips_and_keeps_the_channels_in_order() {
        let mut signed = [0_i16; 8];
        convert_samples(&[-2.0, 2.0, -0.5, 0.5, 0.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY], &mut signed);
        assert_eq!(signed, [i16::MIN, i16::MAX, -16_384, 16_384, 0, 0, 0, 0]);
        let mut unsigned = [0_u16; 3];
        convert_samples(&[-1.0, 0.0, 1.0], &mut unsigned);
        assert_eq!(unsigned, [0, 32_768, u16::MAX]);
        let mut deep = [cpal::I24::EQUILIBRIUM; 3];
        convert_samples(&[-1.0, 0.0, 1.0], &mut deep);
        assert_eq!(deep.map(|s| s.inner()), [-8_388_608, 0, 8_388_607]);
    }

    /// A stream that failed, or was never started, is not a clock: playback then runs on the
    /// wall clock. No device is opened by making the output.
    #[test]
    fn an_output_without_a_running_stream_is_not_a_clock() {
        let failed = AtomicBool::new(false);
        stream_error(&failed, &cpal::StreamError::DeviceNotAvailable);
        assert!(failed.load(Ordering::SeqCst));
        let mut output = CpalOut::new();
        assert_eq!((output.played_frames(), output.note(), output.channels()), (None, None, 2));
        output.stop();
    }
}
